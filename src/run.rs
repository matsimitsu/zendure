//! The coordinator loop: the process's whole runtime, lifted out of `main`.
//!
//! `main` parses arguments, initialises logging, reads configuration and calls
//! [`run`]. The split exists so tests can provide custom device registries and
//! stop conditions without building real hardware.
//!
//! The loop has tests that drive it end to end with a virtual battery, a null
//! publisher (no broker), and a synthetic meter (no hardware). See [`run`]'s own
//! doc comment for details on how the seams work.
//!
//! [`shut_down`]'s ordering — the subtlest thing in the file — is still
//! covered separately, by its own unit test below.

use std::future::Future;
use std::time::Duration;

use crate::allocate::Directive;
use crate::announce::Announcer;
use crate::clock::Clock;
use crate::config::{Config, MeterConfig};
use crate::device::{Applied, BatteryMonitor, BatteryReading, ControlPath};
use crate::engine::Engine;
use crate::event::Event;
use crate::journal::Journal;
use crate::journal::Writer;
use crate::models::ControlDecision;
use crate::mqtt::{self, MqttEvent, MqttPublisher, PublisherTask};
use crate::publish::{NullPublisher, Publisher};
use crate::registry::{self, Battery, Devices};
use crate::source;
use crate::source::shelly::SolarPhase;
use crate::units::{Soc, Timestamp, WattHours};
use crate::web;
use crate::world::{Measurement, World};
use crate::{controller, rte};
use tokio::sync::mpsc;

/// How long each half of a shutdown waits before giving up and saying so.
///
/// Bounded rather than unbounded, and used for both the MQTT drain and the
/// journal's: the only other thing that would end an unbounded wait is
/// systemd's `TimeoutStopSec`, and that ends it with SIGKILL, which loses the
/// rows the wait was protecting. A deadline at least gets to name what it left
/// behind. Two seconds is long enough for a healthy broker to take a full queue
/// and short enough to be invisible in a `systemctl restart`.
const DRAIN_DEADLINE: Duration = Duration::from_secs(2);

/// The MQTT half of shutdown: the publisher `shut_down` drains, and the task
/// draining it. `None` in brokerless mode — see `run`'s own construction of
/// this and `shut_down`'s doc comment for why that half is then skipped
/// rather than faked.
type MqttDrain = (std::sync::Arc<MqttPublisher>, PublisherTask);

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

/// The stop condition production runs with.
///
/// Registers both handlers eagerly and returns a future that resolves on the
/// first of them, so a registration failure is reported at startup rather than
/// leaving a process that cannot be asked to stop.
///
/// The future is not polled until the loop begins, and startup first makes four
/// HTTP round trips with a 5s timeout each plus a 5s sleep in
/// `ensure_ram_mode`, so for up to ~25s a `systemctl stop` appears to do
/// nothing. The signal is *latched*, not lost: tokio installs its handler at
/// registration and the first loop iteration takes it — a delay well inside
/// systemd's default `TimeoutStopSec`, not a hang.
///
/// Without a SIGTERM arm the process simply dies, taking everything still
/// queued for the journal's writer with it. The row most worth having is the
/// last decision before a restart.
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

/// Actuate a decision, record what happened, and report it once.
///
/// Both arms of the loop — a decision and the failsafe idle — do exactly this,
/// differing only in the `ControlPath`, which carries the log wording, both
/// status strings, and the journal's `kind`.
///
/// The journal write happens **after** `actuate`, so each recorded outcome
/// reflects whether the write to that device actually landed — which is what
/// you want when reconstructing an incident.
///
/// `engine` is borrowed rather than its state passed in, because the state has
/// to be read after actuation too.
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

/// Everything a poll advances and publishes that no decision ever reads.
///
/// Round-trip efficiency, pack temperatures, SOC and battery power are all
/// derived from a [`BatteryReading`]'s telemetry, which the objective does not
/// consult, and they are published for graphing rather than fed to the
/// engine. A struct groups these together, moving only once through the
/// telemetry rather than scattering updates across the loop.
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

    /// Fold one poll in, then publish what it produced.
    ///
    /// Not `async`: every publish is a synchronous hand-off to the publisher's
    /// queue, and `save` is a plain file write.
    ///
    /// Takes the whole [`BatteryReading`] rather than a report and a state
    /// separately, so both halves come from the one adapter call that produced
    /// them.
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

/// Start the journal, and whichever combination of a broker connection and a
/// meter source `config` asks for, then fold events until `stop` resolves or
/// every feeder task goes away.
///
/// `devices` is built by the caller (`main.rs`'s `registry::from_config`) and
/// handed in, rather than built here, for the same reason `stop` already was:
/// both are the process's real edges — signals and hardware — and building
/// them inside `run` would leave no seam for a test to hold a handle on
/// either one. `main` is the only production call site and it always passes
/// the registry it just built; the seam exists for
/// `tests::run_drives_real_decisions_against_a_virtual_battery`, which
/// constructs its own `Devices`, keeps an `Arc<VirtualBattery>` clone the
/// registry also holds, and asserts on the battery directly once the loop
/// exits — no journal, no SQLite, in that assertion's path at all.
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

    // The battery's identity in the world, fixed for the life of the process,
    // and taken from the adapter that will answer for it — the world's key and
    // the address on a directive have to be the same string the adapter matches
    // against, so there is one source for it. Stable across restarts in a way
    // an index into a list would not be.
    let device_id = primary.id().clone();

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
    let journal = std::sync::Arc::new(journal);

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

    // What Home Assistant has already been told about. Shared with the
    // subscriber, which resets it on every ConnAck. Built unconditionally:
    // even a brokerless run announces into a sink that just discards it, and
    // `NullPublisher`'s own doc comment is why that is safe.
    let announcer = std::sync::Arc::new(Announcer::new());
    let ha_prefix = config.ha_publish_prefix.clone();

    let (tx, mut rx) = mpsc::channel::<MqttEvent>(64);
    // Every task that can produce an `MqttEvent` — the real MQTT subscriber,
    // the synthetic meter, both, or (impossible per `Config::from_toml_str`'s
    // coherence checks) neither. `rx.recv()` returning `None` below means
    // every sender is gone, which is only true once every feeder here has
    // ended — the same "closed channel means stop" reading a single
    // subscriber used to carry alone.
    let mut feeders: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    // The publisher: a real, queued sink over MQTT if `[mqtt]` is configured,
    // or `NullPublisher` if not. `mqtt_drain` is `Some` only in the first
    // case — it is what `shut_down` drains, and its absence is how a
    // brokerless run skips that half of shutdown entirely.
    let (publisher, mut mqtt_drain): (std::sync::Arc<dyn Publisher>, Option<MqttDrain>) =
        match &config.mqtt {
            Some(mqtt_cfg) => {
                let (mqtt_client, eventloop) = mqtt::create_mqtt_client(mqtt_cfg);
                // The sink the decision path publishes through. Its task owns
                // the only `await` against the broker; nothing below this line
                // can block on one.
                let (mqtt_publisher, publisher_task) = MqttPublisher::open(mqtt_client.clone());
                let publisher: std::sync::Arc<dyn Publisher> = mqtt_publisher.clone();

                // A synthetic meter has no Shelly topic to subscribe to, but this
                // task still has to run — it is what drives the broker's
                // eventloop, without which nothing this publisher queues ever
                // reaches the socket. An empty topic never matches a real
                // publish, so the subscribe-and-parse half of the loop below
                // simply never fires.
                let shelly_topic = config
                    .shelly
                    .as_ref()
                    .map(|s| s.topic.clone())
                    .unwrap_or_default();
                let solar_phase = config
                    .shelly
                    .as_ref()
                    .map(|s| s.solar_phase)
                    .unwrap_or(SolarPhase::A);
                let subscriber_journal = journal.clone();
                let subscriber_prefix = ha_prefix.clone();
                let subscriber_publisher = publisher.clone();
                let subscriber_announcer = announcer.clone();
                let feed_tx = tx.clone();
                feeders.push(tokio::spawn(async move {
                    mqtt::run_subscriber(
                        mqtt_client,
                        eventloop,
                        shelly_topic,
                        solar_phase,
                        subscriber_prefix,
                        subscriber_publisher,
                        subscriber_announcer,
                        feed_tx,
                        subscriber_journal,
                    )
                    .await;
                }));

                (publisher, Some((mqtt_publisher, publisher_task)))
            }
            None => {
                tracing::info!("No [mqtt] configured — publishing through a null sink");
                (std::sync::Arc::new(NullPublisher), None)
            }
        };

    // The meter: a synthetic house feeding the same channel, when
    // configured. `Config::from_toml_str` guarantees the primary device is
    // `Battery::Virtual` whenever the meter is synthetic — see its coherence
    // checks — so the match below never reaches its `unreachable!`.
    if let MeterConfig::Synthetic {
        base_load,
        solar_peak,
    } = &config.meter
    {
        let virtual_battery = match primary {
            Battery::Virtual(battery) => battery.clone(),
            _ => unreachable!(
                "Config::from_toml_str requires a virtual device when the meter is synthetic"
            ),
        };
        let profile = source::synthetic::HouseProfile::new(*base_load, *solar_peak);
        let feed_tx = tx.clone();
        let timezone = config.timezone;
        feeders.push(tokio::spawn(async move {
            source::synthetic::run_synthetic_meter(profile, virtual_battery, timezone, feed_tx)
                .await;
        }));
    }

    // Only the clones handed to the feeders above keep the channel open now.
    drop(tx);

    let mqtt_timeout = config.mqtt_timeout;
    let mut engine = Engine::new(
        controller::Controller::from_config(&config, &Clock::now(config.timezone)),
        World::new(),
        mqtt_timeout,
    );

    // Seed the world from the startup poll, so the first meter reading already
    // has a battery to decide about. A failure to read it fails startup above,
    // which is why `decide`'s `None` branch is unreachable in production.
    //
    // As an event through the fold, and journalled like any other, rather than
    // written into the world directly. `step` on a `DeviceUpdate` only folds
    // it in and returns an empty `Step`, so nothing else about startup changes.
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

    // The dashboard's live-state feed: a `watch` cell `run()` updates after
    // every event it folds, independent of the MQTT publisher and the
    // journal — see `web`'s module doc comment. `None` without `[web]`, so a
    // controller with no dashboard neither reads the journal at startup nor
    // clones a snapshot every tick into a channel nobody is listening to.
    //
    // Spawned separately from `feeders`: that list means "producer of
    // `MqttEvent`s whose death should stop the loop", which an HTTP listener
    // is not. A bind failure warns and runs without a dashboard, the same
    // way a missing `[mqtt]` runs brokerless.
    let mut dashboard_tx: Option<web::DashboardStateSender> = None;
    let mut web_task = None;
    if let Some(web_cfg) = &config.web {
        let history = web::seed_decision_log(&config.journal_path);
        let seed =
            web::DashboardState::seed(&engine.state(), history, Clock::now(config.timezone).now)
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

    let poll_interval = config.device.poll_interval();
    let mut poll_timer = tokio::time::interval(poll_interval);
    // Don't fire immediately — we just polled above
    poll_timer.tick().await;

    // A deadline, not a record of when MQTT was last heard from: both the
    // reading arm and the timeout arm re-arm it. Derived from a "last update"
    // stamp only the reading arm advanced, it sits permanently in the past once
    // a timeout fires and the loop spins for the whole outage.
    let mut mqtt_deadline = tokio::time::Instant::now() + mqtt_timeout;

    // Pinned once, polled by reference in every iteration: the arm has to
    // resume the same future each time round rather than build a fresh one, or
    // a signal that arrived mid-iteration would be dropped.
    let mut stop = std::pin::pin!(stop);

    tracing::info!("Coordinator running, waiting for MQTT data...");

    loop {
        tokio::select! {
            reason = &mut stop => {
                tracing::info!("{reason} — draining the journal and stopping");
                break;
            }
            event = rx.recv() => {
                // A closed channel means the subscriber task is gone, which is
                // not something this loop can recover from: stop.
                let Some(event) = event else { break };

                // A real match, not a `let else`: this ensures every variant is
                // handled, rather than falling through to an `else` arm.
                match event {
                    MqttEvent::Meter(obs) => {
                        mqtt_deadline = tokio::time::Instant::now() + mqtt_timeout;

                        // Already normalized by the source adapter: whichever meter
                        // sent this, the loop sees a signed total, three phases and a
                        // production figure, and nothing about the wire format.
                        let net_grid_power = obs.grid.total;

                        let clock = Clock::now(config.timezone);
                        // Bound, not passed inline, so it can be recorded
                        // *before* it is folded in: a crash mid-decision still
                        // leaves the input that caused it on record.
                        let event = Event::Meter {
                            at: clock,
                            grid: obs.grid,
                            solar: obs.solar,
                        };
                        journal.event(&event);
                        let step = engine.step(&event);

                        if let Some(status) = step.status {
                            tracing::info!("MQTT updates resumed");
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
                                clock.now,
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
                            let decision = step.decision.as_ref().map(|d| (d, clock.now));
                            tx.send_modify(|state| {
                                state.meter_tick(&snapshot, decision, clock.now)
                            });
                        }
                    }
                }
            }
            _ = tokio::time::sleep_until(mqtt_deadline) => {
                mqtt_deadline = tokio::time::Instant::now() + mqtt_timeout;

                let clock = Clock::now(config.timezone);
                let event = Event::MqttTimeout { at: clock };
                journal.event(&event);
                let step = engine.step(&event);

                if step.status.is_some() {
                    tracing::warn!(
                        "No MQTT updates for {}s — forcing idle as safety failsafe",
                        mqtt_timeout.as_secs(),
                    );
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
                        clock.now,
                        decision,
                        &step.directives,
                    )
                    .await;
                }

                if let Some(tx) = &dashboard_tx {
                    let snapshot = engine.state();
                    let decision = step.decision.as_ref().map(|d| (d, clock.now));
                    tx.send_modify(|state| state.failsafe_tick(&snapshot, decision, clock.now));
                }
            }
            _ = poll_timer.tick() => {
                // The raw capture happens inside `poll` itself, before
                // parsing — the adapter keeps that ordering, and hands the
                // bytes back rather than journalling them itself, so this is
                // still the one place that writes to the journal.
                match primary.poll().await {
                    Ok(reading) => {
                        if let Some(raw) = &reading.raw {
                            journal.raw(raw.kind, &raw.body);
                        }

                        tracing::debug!(
                            "Battery poll: SOC={}%, current_power={}W",
                            reading.state.soc,
                            reading.state.current_power,
                        );

                        let figures = telemetry.record_and_publish(
                            &*publisher,
                            &announcer,
                            &ha_prefix,
                            &reading,
                        );

                        let clock = Clock::now(config.timezone);
                        let event = Event::DeviceUpdate {
                            at: clock,
                            id: device_id.clone(),
                            measurement: Measurement::Battery(reading.state),
                        };
                        journal.event(&event);
                        engine.step(&event);

                        if let Some(tx) = &dashboard_tx {
                            let snapshot = engine.state();
                            tx.send_modify(|state| {
                                state.poll_tick(&snapshot, figures, clock.now)
                            });
                        }
                    }
                    Err(e) => {
                        if let Some(raw) = &e.raw {
                            journal.raw(raw.kind, &raw.body);
                        }
                        tracing::warn!("Failed to poll battery state: {}", e.error);
                    }
                }
            }
        }
    }

    // Fired for whichever reason broke the loop, not only a signal. Dropping
    // the sender is the other half: an SSE body is a `WatchStream` that runs
    // until the channel closes, so a browser tab left open would otherwise
    // hold `with_graceful_shutdown` to the deadline below.
    let _ = web_stop_tx.send(());
    drop(dashboard_tx);

    shut_down(
        mqtt_drain
            .as_mut()
            .map(|(publisher, task)| (&**publisher, task)),
        feeders,
        journal,
        journal_writer,
    )
    .await;

    // Holds no journal-writer sender and only ever reads the journal, so it
    // has no bearing on `shut_down`'s ordering — awaited after, once its own
    // shutdown signal has fired. Bounded like the MQTT drain above: an SSE
    // response body is a stream that runs for as long as the dashboard
    // channel does, so `with_graceful_shutdown` alone can wait forever for a
    // connection that will never end on its own — the deadline is what
    // actually ends it.
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

    Ok(())
}

/// Stop everything in the one order that does not lose rows or stall. The
/// order is not the one it looks like it should be.
///
/// The publisher drains **first**, while the feeders still run, because the
/// MQTT feeder owns the eventloop that is the only thing moving bytes to the
/// broker; aborting it first leaves the publisher awaiting a channel nobody
/// drains, stalling every shutdown for the full deadline. This half is skipped
/// in brokerless mode.
///
/// Then the journal. Its writer stops when every sender is gone and every
/// feeder holds one, so the feeders must finish first — and `abort` only
/// schedules cancellation, so awaiting it is what guarantees the captured clone
/// is gone. **This half is unconditional:** the journal is what this function
/// exists to protect.
///
/// Both drains are bounded, because an unbounded wait ends at systemd's
/// `TimeoutStopSec` — with SIGKILL, losing the rows the wait was protecting.
///
/// The MQTT half guarantees **hand-off, not delivery**: `publish` returns once
/// the request is in rumqttc's channel, so a tail can still be lost when the
/// eventloop is dropped. The journal, not the broker, is the record that has to
/// be right.
async fn shut_down(
    mqtt: Option<(&MqttPublisher, &mut PublisherTask)>,
    feeders: Vec<tokio::task::JoinHandle<()>>,
    journal: std::sync::Arc<Journal>,
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

    for feeder in feeders {
        feeder.abort();
        let _ = feeder.await;
    }
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
mod tests {
    use super::*;
    use crate::config::{DeviceConfig, SessionConfig, WebConfig};
    use crate::fixtures;
    use crate::journal;
    use crate::simulation::VirtualBattery;
    use crate::units::{Efficiency, GridPower, PowerMargin, RetentionDays, SolarPower, Watts};

    /// The two signal arms became one, so the wording an operator greps for is
    /// now produced by `Display`. Only the name is pinned here: retyping the
    /// whole log line would assert against a copy this test made, which keeps
    /// passing when the real one changes.
    #[test]
    fn a_stop_reason_renders_the_signal_name() {
        assert_eq!(StopReason::Sigterm.to_string(), "SIGTERM");
        assert_eq!(StopReason::Sigint.to_string(), "SIGINT");
    }

    /// `shut_down` must finish, and must not take the journal down with it.
    ///
    /// A broker that is gone parks the delivery task on rumqttc's channel, so
    /// the MQTT drain cannot complete and has to hit its deadline. Everything
    /// after it in the sequence depends on that being bounded: an unbounded
    /// await there hangs the process until systemd's `TimeoutStopSec` turns
    /// into a SIGKILL, which loses exactly the rows the shutdown exists to
    /// protect. Mutation-checked — replacing the timeout with a bare `await`
    /// makes this fail, and the whole call is wrapped so it fails rather than
    /// hangs.
    ///
    /// The subscriber holds an `Arc<Journal>` clone, as the real one does, so
    /// this also covers the ordering that matters most: the writer ends only
    /// when every sender is gone, and a task still holding one would stall the
    /// journal drain.
    ///
    /// Honest limit: the journal drain's own deadline is belt-and-braces and
    /// this test does not distinguish it — with the subscriber awaited, nothing
    /// holds a sender and the writer ends either way.
    #[tokio::test]
    async fn a_shutdown_finishes_when_the_broker_never_drains() {
        let dir = tempfile::TempDir::new().unwrap();
        let (journal, writer, path) = journal::testing::open(&dir, &SessionConfig::test_default());
        let journal = std::sync::Arc::new(journal);

        let events = fixtures::journey::events();
        for event in &events {
            journal.event(event);
        }

        // A publisher whose broker is not there, filled until its task parks.
        let opts = rumqttc::MqttOptions::new("zendure-shutdown-test", "127.0.0.1", 1);
        let (client, _eventloop) = rumqttc::AsyncClient::new(opts, 50);
        let (publisher, mut publisher_task) = MqttPublisher::open(client);
        for i in 0..300 {
            publisher.publish(crate::publish::Message::telemetry(
                "zendure/decision_power".to_string(),
                i.to_string(),
            ));
            tokio::task::yield_now().await;
        }
        assert!(publisher.queued() > 0, "the delivery task is parked");

        // Holds a journal sender and never returns, like the real subscriber.
        let subscriber_journal = journal.clone();
        let subscriber = tokio::spawn(async move {
            let _held = subscriber_journal;
            std::future::pending::<()>().await;
        });

        // Generously past both deadlines: this asserts that it is bounded at
        // all, not what the bound is.
        tokio::time::timeout(
            DRAIN_DEADLINE * 4,
            shut_down(
                Some((&publisher, &mut publisher_task)),
                vec![subscriber],
                journal,
                Some(writer),
            ),
        )
        .await
        .expect("a parked delivery task must not stall the shutdown");

        let conn = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(
            journal::testing::count(&conn, "SELECT COUNT(*) FROM events"),
            events.len() as i64,
            "every row handed to the journal survived a drain that could not finish",
        );
    }

    /// Builds the `Config` the wiring tests below share: a virtual battery, a
    /// null publisher (no `[mqtt]`) and a synthetic meter — no network, no
    /// broker, no hardware. `capacity` is a parameter because the two callers
    /// want different things from it: the wiring test wants it small enough
    /// to see the SOC move, the round-trip test does not care and uses
    /// whatever is convenient.
    fn virtual_config(dir: &tempfile::TempDir, capacity: WattHours) -> Config {
        Config {
            mqtt: None,
            device: DeviceConfig::Virtual {
                id: "sim".to_string(),
                packs: vec![capacity],
                soc: Soc::new(50),
                charge_efficiency: Efficiency::new(95.0),
                discharge_efficiency: Efficiency::new(95.0),
            },
            shelly: None,
            // A constant 2 kW load and no solar: the grid reading always
            // imports solidly, so the objective has something unambiguous to
            // discharge against instead of hovering near a threshold.
            meter: MeterConfig::Synthetic {
                base_load: Watts(2000),
                solar_peak: Watts(0),
            },
            web: None,
            ha_publish_prefix: "test".to_string(),
            charge_margin: PowerMargin::new(50),
            discharge_margin: PowerMargin::new(5),
            charge_start_threshold: GridPower(-100.0),
            discharge_start_threshold: GridPower(0.0),
            // No cooldowns: the tuning knobs a real deployment leans on to
            // avoid chattering, turned down here so the test does not have
            // to wait out a cooldown window to see a second decision within
            // its short run.
            min_mode_duration: Duration::from_secs(0),
            min_decision_interval: Duration::from_secs(0),
            idle_timeout: Duration::from_secs(300),
            cycle_warn_threshold: 200,
            min_soc: Soc::new(10),
            max_soc: Soc::new(100),
            balance_weekday: None,
            solar_discharge_block_threshold: SolarPower::ZERO,
            min_idle_before_discharge: Duration::from_secs(0),
            timezone: chrono_tz::Tz::UTC,
            mqtt_timeout: Duration::from_secs(60),
            journal_path: dir.path().join("journal.db"),
            journal_retention_days: RetentionDays::new(90).unwrap(),
            rte_state_path: dir.path().join("rte_state.json"),
            log_filter: "zendure=off".to_string(),
        }
    }

    /// Builds the registry `virtual_config`'s device describes, and hands
    /// back the same `Arc<VirtualBattery>` the registry holds — the seam
    /// `run`'s own doc comment describes, exercised directly rather than
    /// through `main`.
    fn virtual_devices(config: &Config) -> (Devices, std::sync::Arc<VirtualBattery>) {
        let devices = registry::from_config(config);
        let battery = match devices.primary() {
            Some((_, Battery::Virtual(battery))) => battery.clone(),
            _ => panic!("virtual_config must always build a DeviceConfig::Virtual"),
        };
        (devices, battery)
    }

    /// The end-to-end wiring test: the real `run()`, with a virtual battery, a
    /// null publisher and a synthetic meter — no network, no broker, no
    /// hardware. Asserted at the device, via this test's own
    /// `Arc<VirtualBattery>` clone, rather than through the journal.
    ///
    /// `journal_path` points somewhere `Journal::open` cannot create, which
    /// takes the disabled path deliberately — under `start_paused`, a single
    /// outstanding `spawn_blocking` task is by itself enough to stop
    /// `tokio::time::sleep` ever resolving, and the journal's writer is one.
    /// `a_run_against_the_simulator_replays_byte_identically` keeps a real one.
    ///
    /// The expected capacity: discharge is capped at 800 W and a 2 kW load
    /// keeps the objective pinned there, so at 95% efficiency the pack gives up
    /// 842.1 Wh to deliver 800 Wh. Over 300 simulated seconds a 1,000 Wh pack
    /// loses about 70.2 Wh — roughly 7 points of SOC from a 50% start.
    #[tokio::test(start_paused = true)]
    async fn run_drives_real_decisions_against_a_virtual_battery() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut config = virtual_config(&dir, WattHours(1_000.0));
        // Deliberately unopenable: `blocker` is a plain file, so
        // `Journal::open`'s `create_dir_all(parent)` fails and it falls back
        // to its documented disabled state — no channel, no writer thread, no
        // SQLite. See this test's own doc comment for why that is the right
        // choice here, not a workaround.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"unwritable as a directory").unwrap();
        config.journal_path = blocker.join("journal.db");
        let (devices, battery) = virtual_devices(&config);

        let soc_before = battery.reading().soc;

        let stop = async {
            tokio::time::sleep(Duration::from_secs(300)).await;
            StopReason::Sigterm
        };

        run(config, devices, stop)
            .await
            .expect("run must exit cleanly");

        assert!(
            battery.flow().discharging() > Watts::ZERO,
            "a constant importing load with no solar must leave the battery discharging"
        );

        let soc_after = battery.reading().soc;
        assert!(
            soc_after < soc_before,
            "SOC must have moved downward over 300 simulated seconds of discharge: \
             before={soc_before}%, after={soc_after}%",
        );
    }

    /// Asks the OS for a free port and hands the number back: `WebConfig`
    /// takes a port, not a listener, so the bind itself has to happen inside
    /// `run`.
    fn a_free_port() -> u16 {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.local_addr().unwrap().port()
    }

    async fn connect_to_dashboard(port: u16) -> tokio::net::TcpStream {
        for _ in 0..200 {
            if let Ok(stream) =
                tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).await
            {
                return stream;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the dashboard never came up on port {port}");
    }

    /// A browser tab left open must not hold shutdown to the drain deadline.
    ///
    /// Two independent faults show up as the same two seconds: a dashboard
    /// waiting on a signal handler of its own never hears a stop that is not
    /// a signal — like this test's own injected one — and a live
    /// `WatchStream` keeps every SSE body, and with it
    /// `with_graceful_shutdown`, open until the sender drops. Timed rather
    /// than inspected, because the deadline is the only observable either
    /// one has. Real time, not paused: the assertion is about a wall-clock
    /// deadline that auto-advance would skip past for free.
    #[tokio::test]
    async fn an_open_dashboard_stream_does_not_hold_shutdown_to_the_deadline() {
        const RUN_FOR: Duration = Duration::from_millis(300);

        let dir = tempfile::TempDir::new().unwrap();
        let mut config = virtual_config(&dir, WattHours(1_000.0));
        let port = a_free_port();
        config.web = Some(WebConfig {
            bind_address: std::net::Ipv4Addr::LOCALHOST.into(),
            port,
        });
        let (devices, _battery) = virtual_devices(&config);

        // The tab: `WatchStream` yields the current value on subscribe, so
        // this is a live body from the moment it connects.
        let tab = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut stream = connect_to_dashboard(port).await;
            stream
                .write_all(b"GET /events HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .await
                .unwrap();
            let mut body = Vec::new();
            stream.read_to_end(&mut body).await.unwrap();
            body
        });

        let stop = async {
            tokio::time::sleep(RUN_FOR).await;
            StopReason::Sigterm
        };

        let started = std::time::Instant::now();
        run(config, devices, stop)
            .await
            .expect("run must exit cleanly");
        let elapsed = started.elapsed();

        let body = tokio::time::timeout(DRAIN_DEADLINE, tab)
            .await
            .expect("the SSE body must end once the dashboard channel closes")
            .unwrap();
        assert!(
            String::from_utf8_lossy(&body).contains("event: battery-panel"),
            "the tab was never served a live fragment"
        );

        assert!(
            elapsed < RUN_FOR + DRAIN_DEADLINE / 2,
            "shutdown burned the dashboard's drain deadline: {elapsed:?}",
        );
    }

    /// A real `run()` loop populates the journal in a shape `from_recording`
    /// can consume, and replaying it produces the same commands — meter to
    /// channel to engine to journal to `read_range` to `replay::verify`, end to
    /// end. `replay_tests.rs` uses a canned event vector, which proves the fold
    /// is deterministic but takes on faith that a real loop's journal reads back.
    ///
    /// This one's subject *is* the journal, so it keeps a real one — which rules
    /// out `start_paused`, since `Journal::open` spawns a `spawn_blocking` task
    /// and any outstanding one stalls a paused clock's auto-advance. It runs on
    /// real time, kept to two seconds: enough for a couple of decisions at the
    /// synthetic meter's 1 Hz, short enough not to dominate the suite.
    #[tokio::test]
    async fn a_run_against_the_simulator_replays_byte_identically() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = virtual_config(&dir, WattHours(1_000.0));
        let journal_path = config.journal_path.clone();
        let (devices, _battery) = virtual_devices(&config);

        // A few real ticks of the 1s synthetic meter is enough for more than
        // one recorded decision; this test is not about how much history
        // replays, only that what was recorded replays identically.
        let stop = async {
            tokio::time::sleep(Duration::from_secs(2)).await;
            StopReason::Sigterm
        };
        // From the epoch: this test only wants everything the session ever
        // wrote, not a particular anchor, and `read_range` handles a `from`
        // that opens before the first decision by falling back to the range's
        // own start with a warning — which this test allows for below rather
        // than asserting away.
        let started_at = Timestamp::from_millis(0);

        run(config, devices, stop)
            .await
            .expect("run must exit cleanly");

        let recording = crate::journal::read::read_range(
            &journal_path,
            started_at,
            Timestamp::from_millis(i64::MAX),
        )
        .expect("a run that recorded decisions must produce a readable range");
        let (fixture, _warnings) = crate::replay::from_recording(recording)
            .expect("a populated journal must yield a fixture");

        let frames =
            crate::replay::run(&fixture, &[]).expect("the fixture's own format must replay");
        crate::replay::verify(&fixture, &frames)
            .expect("replaying a real run's own journal must match what it recorded");

        // Not trivially true: the run does command something, so `expected`
        // is not a list of nothing-but-dashes that `verify` would agree with
        // for free.
        assert!(
            fixture
                .expected
                .iter()
                .any(|line| !line.ends_with(crate::replay::NOTHING)),
            "{:?}",
            fixture.expected
        );
    }
}
