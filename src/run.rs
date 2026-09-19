//! The coordinator loop: the process's whole runtime, lifted out of `main`.
//!
//! `main` parses arguments, initialises logging, reads configuration and calls
//! [`run`]. The split lets tests supply their own device registry and stop
//! condition, so the loop runs end to end with no hardware and no broker.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use crate::allocate::Directive;
use crate::announce::Announcer;
use crate::clock::Clock;
use crate::config::Config;
use crate::device::{Applied, BatteryMonitor, BatteryReading, ControlPath};
use crate::engine::Engine;
use crate::event::Event;
use crate::journal::Journal;
use crate::journal::Writer;
use crate::models::ControlDecision;
use crate::mqtt::{self, MqttPublisher, PublisherTask};
use crate::prediction;
use crate::publish::{NullPublisher, Publisher};
use crate::registry::{self, Devices};
use crate::scan::{self, Delivery, FailureTolerance, Inbox, Sampler, TickSeq};
use crate::source;
use crate::units::{Soc, Timestamp, WattHours};
use crate::web;
use crate::world::{Measurement, World};
use crate::{controller, rte};
use tokio::task::JoinSet;
use tokio::time::MissedTickBehavior;

/// How long each half of a shutdown waits before giving up and saying so.
/// Bounded: an unbounded wait would only end via systemd's `TimeoutStopSec`
/// and SIGKILL, losing the rows it was protecting. Two seconds drains a full
/// queue from a healthy broker and is invisible in a `systemctl restart`.
const DRAIN_DEADLINE: Duration = Duration::from_secs(2);

/// The MQTT half of shutdown: the publisher `shut_down` drains, and the task
/// draining it. `None` in brokerless mode — see `run`'s own construction of
/// this and `shut_down`'s doc comment for why that half is then skipped
/// rather than faked.
type MqttDrain = (Arc<MqttPublisher>, PublisherTask);

/// Why the loop stopped.
///
/// Carries which signal fired so the log line stays the one operators grep for,
/// rather than collapsing two arms into one message that names neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    Sigterm,
    Sigint,
}

impl std::fmt::Display for StopReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StopReason::Sigterm => write!(f, "SIGTERM"),
            StopReason::Sigint => write!(f, "SIGINT"),
        }
    }
}

/// The stop condition production runs with. Registers both handlers eagerly, so a
/// registration
/// failure surfaces at startup rather than leaving the process unable to be asked to
/// stop.
/// Startup's four HTTP round trips plus `ensure_ram_mode`'s 5s sleep make `systemctl
/// stop` look
/// ignored for ~25s, but the signal latches at registration, well inside systemd's
/// `TimeoutStopSec` — without this arm, SIGTERM kills the process outright, taking
/// queued journal writes with it.
pub fn shutdown_signal() -> Result<impl Future<Output = StopReason>, std::io::Error> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    Ok(async move {
        tokio::select! {
            _ = sigterm.recv() => StopReason::Sigterm,
            _ = sigint.recv() => StopReason::Sigint,
        }
    })
}

/// Actuate a decision, record what happened, and report it once. The two callers differ
/// only in
/// `ControlPath` (log wording, both status strings, the journal's `kind`). The journal
/// write
/// happens after `actuate`, so the recorded outcome reflects whether the write to the
/// device
/// actually landed. `engine` is borrowed, not its state passed in, because the state
/// must be read again after actuation.
#[allow(clippy::too_many_arguments)]
async fn apply_decision(
    devices: &Devices,
    publisher: &dyn Publisher,
    journal: &Journal,
    engine: &Engine,
    prefix: &str,
    path: ControlPath,
    at: Timestamp,
    decision: &ControlDecision,
    directives: &[Directive],
) {
    // The whole list, not its first element: a step that means "stop one box,
    // start another" has to reach both devices.
    let outcomes = registry::actuate(devices, directives, path).await;
    let failed = outcomes.iter().any(|o| o.applied == Applied::Error);

    // The engine's state goes in beside the decision so the row carries the
    // inputs and the history it came from, not just its conclusion.
    journal.decision(at, path, decision, &engine.state(), &outcomes);

    // Once after the loop rather than once per command. With one device that is
    // the same single publish as before; with two it stops the HA status
    // flapping twice per decision. A failure anywhere takes precedence: the
    // fleet is degraded even if some of it was commanded successfully.
    if !directives.is_empty() {
        let status = if failed {
            path.err_status()
        } else {
            path.ok_status()
        };
        mqtt::publish_status(publisher, prefix, status);
    }

    mqtt::publish_decision(publisher, prefix, decision);
    // Published on both paths. The counts do not change on a failsafe idle, but
    // a consumer that only sees them after an objective decision cannot tell a
    // quiet hour from a stalled one.
    mqtt::publish_cycle_counts(publisher, prefix, &engine.cycle_counts());
}

/// Everything a poll advances and publishes that no decision ever reads: RTE, pack
/// temperatures,
/// SOC and battery power come from a [`BatteryReading`]'s telemetry, published for
/// graphing and
/// never consulted by the objective.
struct PollTelemetry {
    rte: rte::RteTracker,
    /// Sticky: a report that carries no pack data leaves the last known set in
    /// place rather than publishing a capacity of zero.
    pack_capacities: Vec<WattHours>,
    /// The device's own floor, which it reports in tenths of a percent.
    min_soc: Soc,
}

impl PollTelemetry {
    fn new(state_path: std::path::PathBuf, pack_capacities: Vec<WattHours>, min_soc: Soc) -> Self {
        PollTelemetry {
            rte: rte::RteTracker::new(state_path),
            pack_capacities,
            min_soc,
        }
    }

    fn pack_count(&self) -> usize {
        self.pack_capacities.len()
    }

    /// RTE%, usable energy and pack capacity at `soc`. The one computation
    /// behind both the MQTT publish below and the dashboard panel.
    fn figures(&self, soc: Soc) -> web::DashboardTelemetry {
        web::DashboardTelemetry {
            rte: self.rte.rte_percent(),
            usable: self
                .rte
                .usable_kwh(soc, self.min_soc, &self.pack_capacities),
            capacity: self
                .pack_capacities
                .iter()
                .copied()
                .sum::<WattHours>()
                .to_kwh(),
        }
    }

    /// Fold one poll in, then publish what it produced. Not `async`: every publish is a
    /// synchronous hand-off to the publisher's queue and `save` is a plain file write.
    /// Takes the
    /// whole [`BatteryReading`] so both halves come from one adapter call.
    fn record_and_publish(
        &mut self,
        publisher: &dyn Publisher,
        announcer: &Announcer,
        prefix: &str,
        reading: &BatteryReading,
    ) -> web::DashboardTelemetry {
        let telemetry = &reading.telemetry;
        self.rte.record(telemetry.charge, telemetry.discharge);

        if let Some(pack_capacities) = &telemetry.pack_capacities {
            self.pack_capacities = pack_capacities.clone();
        }
        if let Some(min_soc) = telemetry.min_soc {
            self.min_soc = min_soc;
        }

        let figures = self.figures(reading.state.soc);
        mqtt::publish_rte(
            publisher,
            prefix,
            figures.rte,
            figures.usable,
            figures.capacity,
        );

        mqtt::publish_temperatures(
            publisher,
            announcer,
            prefix,
            telemetry.enclosure_temp,
            &telemetry.pack_temps,
        );

        mqtt::publish_soc_calibrating(publisher, prefix, reading.state.soc_calibrating);
        mqtt::publish_battery_soc(publisher, prefix, reading.state.soc);
        mqtt::publish_battery_power(publisher, prefix, telemetry.charge, telemetry.discharge);

        // Persisted every poll, so the rolling 24h window survives a restart.
        self.rte.save();

        figures
    }
}

/// Seeds the dashboard's actual-solar history from the journal's `meter`
/// events since local midnight, so a restart mid-day doesn't blank today's
/// line. Degrades to an empty history on a read failure — a logging concern,
/// never a startup failure, the same rule `web::seed_decision_log` follows.
fn seed_actual_solar(
    journal_path: &std::path::Path,
    timezone: chrono_tz::Tz,
) -> web::ActualSolarHistory {
    use chrono::{Datelike, TimeZone};

    let midnight = crate::clock::local_midnight(Clock::now(timezone).now, timezone);

    let mut history = web::ActualSolarHistory::default();
    match crate::journal::read::read_meter_solar_since(journal_path, midnight.as_millis()) {
        Ok(rows) => {
            for (ts_ms, solar) in rows {
                if let Some(local) = timezone.timestamp_millis_opt(ts_ms).single() {
                    history.record(
                        Timestamp::from_millis(ts_ms),
                        timezone,
                        local.ordinal(),
                        solar,
                    );
                }
            }
        }
        Err(e) => tracing::warn!("Dashboard: cannot seed actual-solar history from journal: {e}"),
    }
    history
}

/// Starts the journal and whichever combination of broker and meter source `config`
/// asks for,
/// then folds events until `stop` resolves or every feeder task goes away. `devices` is
/// handed in
/// rather than built here — like `stop`, it's a real process edge, and building it
/// inside `run`
/// would leave tests no handle on it (the wiring test keeps an `Arc<VirtualBattery>`
/// clone from it).
pub async fn run(
    config: Config,
    devices: Devices,
    stop: impl Future<Output = StopReason>,
) -> Result<(), Box<dyn std::error::Error>> {
    tracing::info!(
        "Starting Zendure controller for {}",
        config.device.identity()
    );

    let Some((_, primary)) = devices.primary() else {
        unreachable!("devices was just constructed with exactly the one battery above")
    };

    // The battery's identity in the world, fixed for the life of the process and taken
    // from the
    // adapter that answers for it, so the world's key and a directive's address are
    // always the
    // same string the adapter matches against. Stable across restarts, unlike an index
    // into a list.
    let device_id = BatteryMonitor::id(primary).clone();

    // On by default: by the time you think to enable logging, the bug you
    // wanted it for has already happened. Any failure here disables the journal
    // and leaves control untouched. Opened before the startup handshake below
    // so its raw capture has somewhere to go.
    let (journal, journal_writer) = Journal::open(
        &config.journal_path,
        config.journal_retention_days,
        env!("CARGO_PKG_VERSION"),
        &config.session(),
    );
    let journal = Arc::new(journal);

    // The startup handshake, then the first reading — see `BatteryMonitor::prepare`
    // (`zendure.rs`) for the ordering and failure policy.
    let reading = primary.prepare().await.map_err(|e| {
        if let Some(raw) = &e.raw {
            journal.raw(raw.kind, &raw.body);
        }
        e.error
    })?;
    if let Some(raw) = &reading.raw {
        journal.raw(raw.kind, &raw.body);
    }

    let BatteryReading {
        state: battery_state,
        telemetry: initial_telemetry,
        ..
    } = reading;

    let mut telemetry = PollTelemetry::new(
        config.rte_state_path.clone(),
        initial_telemetry.pack_capacities.unwrap_or_default(),
        initial_telemetry.min_soc.unwrap_or(Soc::ZERO),
    );
    let startup_soc = battery_state.soc;
    tracing::info!(
        "Battery: SOC={}%, max_discharge={}W, max_charge={}W, current_power={}W, packs={}",
        battery_state.soc,
        battery_state.max_discharge_power,
        battery_state.max_charge_power,
        battery_state.current_power,
        telemetry.pack_count(),
    );

    // What Home Assistant has already been told about. Shared with the broker
    // connection, which resets it on every ConnAck. Built unconditionally:
    // even a brokerless run announces into a sink that just discards it, and
    // `NullPublisher`'s own doc comment is why that is safe.
    let announcer = Arc::new(Announcer::new());
    let ha_prefix = config.ha_publish_prefix.clone();

    // Every task the loop's life depends on: one sampler per source, and the
    // broker connection when there is one. Any of them ending is fatal to the
    // loop — a sampler that returned would leave its slot frozen.
    let mut sources: JoinSet<()> = JoinSet::new();

    // The publisher: a real, queued sink over MQTT if `[mqtt]` is configured,
    // or `NullPublisher` if not. `mqtt_drain` is `Some` only in the first
    // case — it is what `shut_down` drains, and its absence is how a
    // brokerless run skips that half of shutdown entirely.
    let (publisher, mut mqtt_drain): (Arc<dyn Publisher>, Option<MqttDrain>) = match &config.mqtt {
        Some(mqtt_cfg) => {
            let (mqtt_client, eventloop) = mqtt::create_mqtt_client(mqtt_cfg);
            // The sink the decision path publishes through. Its task owns
            // the only `await` against the broker; nothing below this line
            // can block on one.
            let (mqtt_publisher, publisher_task) = MqttPublisher::open(mqtt_client);
            let publisher: Arc<dyn Publisher> = mqtt_publisher.clone();

            // Nothing incoming is subscribed to, but this task must still
            // run: it drives the broker's eventloop, without which nothing
            // queued ever reaches the socket.
            let connection_prefix = ha_prefix.clone();
            let connection_publisher = publisher.clone();
            let connection_announcer = announcer.clone();
            sources.spawn(async move {
                mqtt::run_connection(
                    eventloop,
                    connection_prefix,
                    connection_publisher,
                    connection_announcer,
                )
                .await;
            });

            (publisher, Some((mqtt_publisher, publisher_task)))
        }
        None => {
            tracing::info!("No [mqtt] configured — publishing through a null sink");
            (Arc::new(NullPublisher), None)
        }
    };

    let mqtt_timeout = config.mqtt_timeout;
    let mut engine = Engine::new(
        controller::Controller::from_config(&config, &Clock::now(config.timezone)),
        World::new(),
        mqtt_timeout,
    );

    // Seeds the world from the startup poll (as an `Event::DeviceUpdate` through the
    // fold and
    // journal, not written into `World` directly) so a replay has a battery from the
    // first tick —
    // skip the journal here and the opening minutes of a session become unreplayable. A
    // failure to
    // read it fails startup above, so `decide`'s `None` branch is unreachable in
    // production.
    let startup = Event::DeviceUpdate {
        at: Clock::now(config.timezone),
        id: device_id.clone(),
        measurement: Measurement::Battery(battery_state),
    };
    journal.event(&startup);
    engine.step(&startup);

    // The loop below breaks for reasons that are not signals — the injected
    // `stop`, or every feeder going away — and the dashboard has to see all
    // of them, so it waits on this rather than on a second signal handler.
    let (web_stop_tx, web_stop_rx) = tokio::sync::oneshot::channel::<()>();

    // The dashboard's live-state feed: a `watch` cell updated after every folded event,
    // independent of the MQTT publisher and journal (see `web`'s module doc). `None`
    // without
    // `[web]` skips reading the journal at startup and cloning snapshots nobody listens
    // to.
    // Spawned apart from `feeders` (whose death stops the loop, unlike an HTTP
    // listener); a bind failure just warns and runs without a dashboard, the same way a
    // missing `[mqtt]` runs brokerless.
    let mut dashboard_tx: Option<web::DashboardStateSender> = None;
    let mut web_task = None;
    if let Some(web_cfg) = &config.web {
        let history = web::seed_decision_log(&config.journal_path);
        let actual_solar = seed_actual_solar(&config.journal_path, config.timezone);
        let seed = web::DashboardState::seed(
            &engine.state(),
            history,
            actual_solar,
            Clock::now(config.timezone).now,
        )
        .with_telemetry(telemetry.figures(startup_soc));
        let (tx, rx) = tokio::sync::watch::channel(seed);
        web_task = web::spawn(web_cfg, rx, config.timezone, async move {
            let _ = web_stop_rx.await;
        })
        .await;
        if web_task.is_some() {
            dashboard_tx = Some(tx);
        }
    }

    // The forecast poller: independent of `Event`/`Engine::step` and the
    // `feeders` vec — the dashboard is its only consumer, so it reaches the
    // `watch` channel directly rather than through the main select loop.
    // Spawned only when both a dashboard and a `[prediction]` backend are
    // configured; absent either, no poller runs and no HTTP call is ever made.
    let (forecast_stop_tx, forecast_stop_rx) = tokio::sync::oneshot::channel::<()>();
    let mut forecast_task: Option<tokio::task::JoinHandle<()>> = None;
    if let (Some(tx), Some(prediction_cfg)) = (&dashboard_tx, &config.prediction) {
        let forecaster = prediction::from_config(prediction_cfg);
        let state_path = prediction_cfg.state_path().clone();
        let poll_times = prediction_cfg.poll_times().to_vec();
        let tx = tx.clone();
        let timezone = config.timezone;
        let forecast_journal = journal.clone();
        forecast_task = Some(tokio::spawn(async move {
            prediction::run_forecast_poller(
                forecaster,
                timezone,
                state_path,
                poll_times,
                tx,
                forecast_journal,
                forecast_stop_rx,
            )
            .await;
        }));
    }

    let tick = config.device.poll_interval();

    // Derived once, here at the boundary: as many rounds as fit in the window
    // the loop is allowed to go blind for, so the stand-down window stays the
    // one `[tuning] mqtt_timeout_secs` already names.
    let tolerance = FailureTolerance::over(mqtt_timeout, tick);

    let meter = Arc::new(source::from_config(&config, &devices));
    let battery = devices
        .handle(&device_id)
        .expect("the primary device was taken from this registry above");

    // Opened before the samplers are asked for anything, so a source that
    // never answers at all ages out from here.
    let inbox = Inbox::new(
        meter.tolerance(tolerance),
        [(device_id.clone(), battery.tolerance(tolerance))],
        Clock::now(config.timezone).now,
        mqtt_timeout,
    );

    // A `watch`, so rounds a busy source slept through coalesce into one
    // request instead of queueing behind it.
    let (requests, _) = tokio::sync::watch::channel(TickSeq::FIRST);
    sources.spawn(scan::sample_loop(
        meter,
        inbox.meter_slot(),
        requests.subscribe(),
        journal.clone(),
        config.timezone,
    ));
    sources.spawn(scan::sample_loop(
        battery,
        inbox
            .device_slot(&device_id)
            .expect("the inbox was just given this device"),
        requests.subscribe(),
        journal.clone(),
        config.timezone,
    ));

    let mut ticker = tokio::time::interval(tick);
    // `Delay`, not the default `Burst`: the tick body awaits a write to the
    // device, and a period it overran must not be followed by a catch-up
    // storm of decisions.
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // Don't fire immediately — nothing has been sampled yet.
    ticker.tick().await;

    // Pinned once, polled by reference in every iteration: the arm has to
    // resume the same future each time round rather than build a fresh one, or
    // a signal that arrived mid-iteration would be dropped.
    let mut stop = std::pin::pin!(stop);

    tracing::info!(
        "Coordinator running on a {}s tick, standing down after {} failed rounds",
        tick.as_secs(),
        tolerance.get(),
    );

    // The round every slot is currently answering. A tick reads only the
    // samples stamped with it, so one that arrived too late for the round
    // before is not served beside one taken for this one.
    let mut round = TickSeq::FIRST;

    // The first round, so the first tick finds filled slots.
    round = round.next();
    requests.send_modify(|seq| *seq = round);

    loop {
        tokio::select! {
            reason = &mut stop => {
                tracing::info!("{reason} — draining the journal and stopping");
                break;
            }
            Some(ended) = sources.join_next() => {
                // One sampler gone is one permanently frozen slot, and for the
                // battery nothing downstream would ever say so.
                match ended {
                    Ok(()) => tracing::error!("A source task ended — stopping"),
                    Err(e) => tracing::error!("A source task ended ({e}) — stopping"),
                }
                break;
            }
            _ = ticker.tick() => {
                // One call, shared by the meter event, the decision row, the
                // failsafe and the dashboard — all of which must agree about
                // when this tick happened.
                let at = Clock::now(config.timezone);

                let device_deliveries = inbox.take_devices(round, at.now);
                let meter_delivery = inbox.take_meter(round, at.now);

                // Folded whatever the rest of the fleet is doing: a sample is
                // taken once, and dropping it because another source went
                // quiet would leave `RteTracker::record` integrating one
                // trapezoid across the whole outage.
                let mut every_device_fresh = true;
                let mut down_devices = Vec::new();

                for (id, delivery) in device_deliveries {
                    let (sampled_at, reading) = match delivery {
                        Delivery::Fresh(sampled_at, reading) => (sampled_at, reading),
                        Delivery::Down(failures) => {
                            every_device_fresh = false;
                            down_devices.push((id, failures));
                            continue;
                        }
                        Delivery::Quiet | Delivery::Cold => {
                            every_device_fresh = false;
                            continue;
                        }
                    };

                    let figures = telemetry.record_and_publish(
                        &*publisher,
                        &announcer,
                        &ha_prefix,
                        &reading,
                    );

                    // The sample's own clock, not the tick's, so
                    // `events.ts_ms` says when the reading was taken.
                    let event = Event::DeviceUpdate {
                        at: sampled_at,
                        id,
                        measurement: Measurement::Battery(reading.state),
                    };
                    journal.event(&event);
                    engine.step(&event);

                    if let Some(tx) = &dashboard_tx {
                        let snapshot = engine.state();
                        tx.send_modify(|state| state.poll_tick(&snapshot, figures, at.now));
                    }
                }

                if meter_delivery.is_down() || !down_devices.is_empty() {
                    let event = Event::MqttTimeout { at };
                    journal.event(&event);
                    let step = engine.step(&event);

                    if step.status.is_some() {
                        tracing::warn!(
                            "No usable meter reading for {}s — forcing idle as safety failsafe",
                            mqtt_timeout.as_secs(),
                        );
                        // Which source actually stopped answering, since the
                        // event above can only say "mqtt_timeout".
                        if let Delivery::Down(failures) = &meter_delivery {
                            tracing::warn!(
                                "The meter is not answering ({} consecutive failures)",
                                failures.get(),
                            );
                        }
                        for (id, failures) in &down_devices {
                            tracing::warn!(
                                "{id} is not answering ({} consecutive failures)",
                                failures.get(),
                            );
                        }
                    }

                    if let Some(decision) = &step.decision {
                        // Every battery stands down, and one unreachable box does
                        // not leave the others running through the outage.
                        apply_decision(
                            &devices,
                            &*publisher,
                            &journal,
                            &engine,
                            &ha_prefix,
                            ControlPath::Failsafe,
                            at.now,
                            decision,
                            &step.directives,
                        )
                        .await;
                    }

                    if let Some(tx) = &dashboard_tx {
                        let snapshot = engine.state();
                        let decision = step.decision.as_ref().map(|d| (d, at.now));
                        tx.send_modify(|state| state.failsafe_tick(&snapshot, decision, at.now));
                    }
                } else {
                    // A setpoint inverts `battery.current_power + (export −
                    // margin)` exactly, which holds only while both terms
                    // describe the same instant — so a round missing any
                    // source's sample decides nothing and holds the last
                    // command.
                    match meter_delivery {
                        Delivery::Fresh(_, sample) if every_device_fresh => {
                            // Already normalized by the source adapter: whichever
                            // meter sent this, the loop sees a signed total, three
                            // phases and a production figure, and nothing about the
                            // wire format.
                            let net_grid_power = sample.observation.grid.total;

                            // Bound, not passed inline, so it can be recorded
                            // *before* it is folded in: a crash mid-decision still
                            // leaves the input that caused it on record.
                            let event = Event::Meter {
                                at,
                                grid: sample.observation.grid,
                                solar: sample.observation.solar,
                            };
                            journal.event(&event);
                            let step = engine.step(&event);

                            if let Some(status) = step.status {
                                tracing::info!("Meter readings resumed");
                                mqtt::publish_status(&*publisher, &ha_prefix, status);
                            }

                            if let Some(decision) = &step.decision {
                                if let Some(battery) = engine.battery() {
                                    tracing::info!(
                                        "Decision: {} at {}W — {} (net_grid={:.0}W, battery: SOC={}%, max_charge={}W, max_discharge={}W, current={}W, soc_limit={})",
                                        decision.mode,
                                        decision.power_watts,
                                        decision.reason,
                                        net_grid_power,
                                        battery.soc,
                                        battery.max_charge_power,
                                        battery.max_discharge_power,
                                        battery.current_power,
                                        battery.soc_limit_reached,
                                    );
                                }

                                apply_decision(
                                    &devices,
                                    &*publisher,
                                    &journal,
                                    &engine,
                                    &ha_prefix,
                                    ControlPath::Objective,
                                    at.now,
                                    decision,
                                    &step.directives,
                                )
                                .await;
                            }

                            // The meter is the only tick that extends the
                            // sparklines, so their window stays the meter's own
                            // cadence rather than every arm's.
                            if let Some(tx) = &dashboard_tx {
                                let snapshot = engine.state();
                                let decision = step.decision.as_ref().map(|d| (d, at.now));
                                tx.send_modify(|state| {
                                    state.meter_tick(&snapshot, decision, &at, config.timezone)
                                });
                            }
                        }
                        _ => tracing::debug!("Skipped a round: not every source had a fresh sample"),
                    }
                }

                // Asked for after this round's writes have landed, so every
                // sample the next tick reads is one round old.
                round = round.next();
                requests.send_modify(|seq| *seq = round);
            }
        }
    }

    // Ends every sampler on its own terms, rather than aborting one in the
    // middle of a request: `changed()` returns `Err` once the sender is gone.
    drop(requests);

    // Fired for whichever reason broke the loop, not only a signal. Dropping
    // the sender is the other half: an SSE body is a `WatchStream` that runs
    // until the channel closes, so a browser tab left open would otherwise
    // hold `with_graceful_shutdown` to the deadline below.
    let _ = web_stop_tx.send(());
    let _ = forecast_stop_tx.send(());
    drop(dashboard_tx);

    shut_down(
        mqtt_drain
            .as_mut()
            .map(|(publisher, task)| (&**publisher, task)),
        sources,
        journal,
        journal_writer,
    )
    .await;

    // Holds no journal-writer sender, so it has no bearing on `shut_down`'s ordering —
    // awaited
    // after, once its own shutdown signal fires. Bounded like the MQTT drain: an SSE
    // body is a
    // stream that runs as long as the dashboard channel does, so
    // `with_graceful_shutdown` alone
    // would wait forever for a connection that never ends on its own.
    if let Some(mut handle) = web_task
        && tokio::time::timeout(DRAIN_DEADLINE, &mut handle)
            .await
            .is_err()
    {
        handle.abort();
        tracing::warn!(
            "Dashboard did not stop within {}s — aborted",
            DRAIN_DEADLINE.as_secs(),
        );
    }

    // No careful drain sequence needed, unlike the journal/MQTT halves above:
    // `ForecastTracker` saves synchronously after every fetch, so there is
    // nothing queued for this task to lose by being aborted.
    if let Some(mut handle) = forecast_task
        && tokio::time::timeout(DRAIN_DEADLINE, &mut handle)
            .await
            .is_err()
    {
        handle.abort();
        tracing::warn!(
            "Forecast poller did not stop within {}s — aborted",
            DRAIN_DEADLINE.as_secs(),
        );
    }

    Ok(())
}

/// Stops everything in the order that avoids losing rows or stalling — not the order it
/// looks like it should be.
/// The publisher drains first, while the source tasks still run, because the broker
/// connection owns the eventloop moving bytes to the broker; stopping it first would
/// stall the drain for the full deadline. Skipped in brokerless mode.
/// Then the journal, unconditionally: its writer stops only once every sender is gone,
/// each sampler holds one so the source tasks must finish first, and since `shutdown`
/// only schedules cancellation, awaiting it is what guarantees a captured clone is
/// actually gone. Both drains are bounded — an unbounded wait would otherwise run to
/// systemd's `TimeoutStopSec` and SIGKILL, losing the rows being protected.
/// The MQTT half is hand-off, not delivery: `publish` returns once queued in rumqttc's
/// channel, so a tail can still be lost when the eventloop drops — the journal, not the
/// broker, is the record that has to be right.
async fn shut_down(
    mqtt: Option<(&MqttPublisher, &mut PublisherTask)>,
    mut sources: JoinSet<()>,
    journal: Arc<Journal>,
    journal_writer: Option<Writer>,
) {
    if let Some((publisher, publisher_task)) = mqtt {
        let queued = publisher.queued();
        publisher.close();

        // Matched rather than `.is_err()`, which sees only the timeout: a task
        // that ended early resolves instantly to `Ok(Err(JoinError))`, and
        // treating that as success reported a clean drain for a task that had
        // been dead for hours.
        match tokio::time::timeout(DRAIN_DEADLINE, &mut *publisher_task).await {
            // Drained. The task logs its own closing summary.
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!(
                "MQTT delivery task ended early ({e}) — {} messages dropped this session",
                publisher.dropped(),
            ),
            Err(_) => {
                publisher_task.abort();
                // The task prints this summary itself when it ends normally;
                // aborting it is the one path where nobody would. `queued` is
                // what was waiting when the drain began — some of it will
                // have gone out since.
                tracing::warn!(
                    "MQTT drain did not finish in {}s — {queued} messages were queued when it \
                     began, {} dropped and {} failed this session",
                    DRAIN_DEADLINE.as_secs(),
                    publisher.dropped(),
                    publisher.failed(),
                );
            }
        }
    }

    sources.shutdown().await;
    drop(journal);

    if let Some(writer) = journal_writer
        && tokio::time::timeout(DRAIN_DEADLINE, writer).await.is_err()
    {
        tracing::warn!(
            "Journal drain did not finish in {}s — the newest rows may be missing",
            DRAIN_DEADLINE.as_secs(),
        );
    }
}

#[cfg(test)]
#[path = "run_tests.rs"]
mod tests;
