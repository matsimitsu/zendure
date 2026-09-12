//! The coordinator loop: the process's whole runtime, lifted out of `main`.
//!
//! `main` parses arguments, initialises logging, reads configuration and calls
//! [`run`] — nothing else. The split exists because the loop below is the one
//! part of this crate no test has ever driven, and both defects that reached
//! production hid in exactly there: the MQTT-deadline spin (`c131b2f`) was found
//! by measuring CPU on the hardware rather than by the suite, and a failing
//! device write silently disarming the failsafe (`bfcf5ab`) was found by
//! reading.
//!
//! **The loop still has no test, and this module does not yet make one
//! possible.** [`run`] takes its stop condition as a parameter, which is one of
//! the two seams a test needs; the other is the device, and `run` still reaches
//! a real Zendure over HTTP in its first eighty lines. Until the read path is
//! behind a trait — `registry::actuate` already routes through [`Devices`], so
//! only the poll is missing — driving `run` from a test means owning a
//! battery. Saying otherwise would be worse than saying nothing: the next
//! person looking for a regression test for a `select!` defect would believe
//! the seam is here and stop looking.
//!
//! What is testable and is tested: [`shut_down`], whose ordering is the
//! subtlest thing in the file.

use std::future::Future;
use std::time::Duration;

use crate::allocate::Directive;
use crate::announce::Announcer;
use crate::clock::Clock;
use crate::config::Config;
use crate::device::{Applied, ControlPath};
use crate::engine::Engine;
use crate::event::Event;
use crate::journal::Journal;
use crate::journal::Writer;
use crate::models::ControlDecision;
use crate::models::StorageMode;
use crate::mqtt::{self, MqttEvent, MqttPublisher, PublisherTask};
use crate::publish::Publisher;
use crate::registry::{self, Battery, Devices};
use crate::units::{DeciKelvin, Soc, Timestamp, WattHours, Watts};
use crate::world::{Measurement, World};
use crate::zendure::ZendureClient;
use crate::{battery, controller, models, rte};
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
/// The cost of registering this early, stated because it is not free: the
/// future is not polled until the loop begins, and startup first makes four
/// HTTP round trips to the device with a 5s timeout each plus a deliberate 5s
/// sleep in `ensure_ram_mode` — so for up to ~25s a `systemctl stop` or a
/// Ctrl-C appears to do nothing. The signal is *latched*, not lost: tokio's
/// handler is installed at registration and the first loop iteration takes it.
/// So this is a delay well inside systemd's default `TimeoutStopSec`, not a
/// hang, and the alternative — registering just before the loop, as this did
/// when it lived in `main` — trades it for a window where the default
/// disposition would kill the process mid-handshake instead.
///
/// systemd stops this process with SIGTERM. Without an arm for it the process
/// simply died, and everything still queued for the journal's writer died with
/// it — a durability regression against the NDJSON capture, which wrote
/// synchronously on the calling thread and so survived any kill. The row most
/// worth having is the last decision before a restart.
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
/// differing only in the `ControlPath`, which already carries everything they
/// used to differ by: the log wording, both status strings, and the journal's
/// `kind`. Written out twice it was the same twenty lines, and step 9's charger
/// would have made it three.
///
/// The ordering that matters is stated once, here. The journal write happens
/// **after** `actuate`, so each recorded outcome reflects whether the write to
/// that device actually landed — which is what you want when reconstructing an
/// incident. An earlier version returned the outcomes and left the journalling
/// to the caller, arguing that the write "has to happen after this returns";
/// that confused "after `actuate`" with "after this function returns" and cost
/// the extraction its last twenty lines.
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
/// derived from `ZendureReport` fields the objective does not consult, and they
/// are published for graphing rather than fed to the engine. Seventy-odd lines
/// of that once sat inline in a `select!` branch, four levels deep, between
/// parsing the response and folding it into the world — so the arm's one job
/// was the hardest thing in it to see.
///
/// A struct rather than three `&mut` out-parameters behind an
/// `#[allow(clippy::too_many_arguments)]`. The three move together, only ever
/// change here, and the lint was telling the truth. It also fixes a name that
/// lied: `publish_poll_telemetry` did not only publish — the first thing it did
/// was advance the rolling RTE window and rewrite the pack capacities and the
/// device's own minimum SOC, which is not where a reader looks for them.
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

    /// Fold one poll in, then publish what it produced.
    ///
    /// Not `async`: every publish is a synchronous hand-off to the publisher's
    /// queue, and `save` is a plain file write. It was `async` with no `.await`
    /// in it for one commit, which is worse than useless — it puts a suspension
    /// point in the reader's head that the code does not have.
    fn record_and_publish(
        &mut self,
        publisher: &dyn Publisher,
        announcer: &Announcer,
        prefix: &str,
        report: &models::ZendureReport,
        state: &battery::BatteryState,
    ) {
        let charge = Watts::from_device(report.properties.output_pack_power.unwrap_or(0));
        let discharge = Watts::from_device(report.properties.pack_input_power.unwrap_or(0));
        self.rte.record(charge, discharge);

        if report.pack_data.is_some() {
            self.pack_capacities = rte::pack_capacities(&report.pack_data);
        }
        if let Some(ms) = report.properties.min_soc {
            self.min_soc = Soc::from_tenths(ms);
        }

        let total_capacity_kwh = self
            .pack_capacities
            .iter()
            .copied()
            .sum::<WattHours>()
            .to_kwh();
        let usable_kwh = self
            .rte
            .usable_kwh(state.soc, self.min_soc, &self.pack_capacities);
        mqtt::publish_rte(
            publisher,
            prefix,
            self.rte.rte_percent(),
            usable_kwh,
            total_capacity_kwh,
        );

        let pack_temps: Vec<mqtt::PackTemperature> = report
            .pack_data
            .as_ref()
            .map(|packs| {
                packs
                    .iter()
                    .enumerate()
                    .filter_map(|(index, p)| {
                        p.max_temp.map(|t| mqtt::PackTemperature {
                            index,
                            temp: DeciKelvin(t),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        mqtt::publish_temperatures(
            publisher,
            announcer,
            prefix,
            report.properties.hyper_tmp.map(DeciKelvin),
            &pack_temps,
        );

        mqtt::publish_soc_calibrating(publisher, prefix, state.soc_calibrating);
        mqtt::publish_battery_soc(publisher, prefix, state.soc);
        mqtt::publish_battery_power(publisher, prefix, charge, discharge);

        // Persisted every poll, so the rolling 24h window survives a restart.
        self.rte.save();
    }
}

/// Start the devices, the journal and the MQTT client, then fold events until
/// `stop` resolves or the subscriber goes away.
pub async fn run(
    config: Config,
    stop: impl Future<Output = StopReason>,
) -> Result<(), Box<dyn std::error::Error>> {
    tracing::info!("Starting Zendure controller for {}", config.zendure_sn);

    let zendure_client = ZendureClient::new(&config.zendure_ip, config.zendure_sn.clone());

    // The battery's identity in the world, fixed for the life of the process,
    // and taken from the adapter that will answer for it — the world's key and
    // the address on a directive have to be the same string the adapter matches
    // against, so there is one source for it. The serial is what a journal
    // reader would recognise it by, and it is stable across restarts in a way
    // an index into a list would not be.
    let device_id = zendure_client.id().clone();
    let initial_report = zendure_client
        .get_properties()
        .await
        .map_err(|e| e.to_string())?;

    // Sync tracked storage mode with the device's actual state. The client
    // defaults to RAM, but the device may have been left in Flash/standby
    // (e.g. after an idle-timeout standby before a restart). Without this,
    // ensure_ram_mode() short-circuits and never wakes the device, so it keeps
    // reporting chargeMaxLimit=0 / inverseMaxPower=0 and every command clamps to 0W.
    let initial_storage_mode = if initial_report.properties.smart_mode == Some(1) {
        StorageMode::Ram
    } else {
        StorageMode::Flash
    };
    zendure_client.set_storage_mode(initial_storage_mode);
    tracing::info!("Device storage mode at startup: {initial_storage_mode:?}");

    // Write the charge/discharge power caps once, here at startup. The device
    // stores these as setpoints it can reset to 0; we deliberately only write
    // them at startup (never mid-run) so a device-initiated 0 stops power flow
    // until a human restarts the process, rather than being silently overwritten.
    if let Err(e) = zendure_client.write_power_caps().await {
        tracing::warn!("Failed to write power caps at startup: {e}");
    }

    // Re-read so battery_state reflects the caps we just wrote, otherwise the
    // first decisions would use the pre-write (possibly 0) limits.
    let battery_report = match zendure_client.get_properties().await {
        Ok(report) => report,
        Err(e) => {
            tracing::warn!("Failed to re-read properties after writing caps: {e}");
            initial_report.clone()
        }
    };
    // The rated limits come from the adapter, which knows which box it is
    // talking to. Naming a model here instead put a hardware fact in the
    // coordinator, twice, where a second battery of another model would have
    // been clamped to this one's rating.
    let battery_state =
        battery::BatteryState::from_properties(&battery_report.properties, zendure_client.spec());

    let mut telemetry = PollTelemetry::new(
        config.rte_state_path.clone(),
        rte::pack_capacities(&initial_report.pack_data),
        initial_report
            .properties
            .min_soc
            .map(Soc::from_tenths)
            .unwrap_or(Soc::ZERO),
    );
    tracing::info!(
        "Battery: SOC={}%, max_discharge={}W, max_charge={}W, current_power={}W, packs={}",
        battery_state.soc,
        battery_state.max_discharge_power,
        battery_state.max_charge_power,
        battery_state.current_power,
        telemetry.pack_count(),
    );

    // One battery, held in the registry rather than as a bare local — the
    // next device this process drives is a second entry here, not a second
    // local variable threaded through every call site below.
    let devices = Devices::new([Battery::Zendure(zendure_client)]);

    // `Devices`/`BatteryController` has no read capability yet — `apply` is
    // the only thing behind the enum so far — so the startup handshake above
    // reached `zendure_client`'s inherent methods directly, and the poll arm
    // below still needs to. Rather than keep a second owned `ZendureClient`
    // alongside the registry (two names for the one battery, free to drift),
    // this reaches back into the registry for the concrete adapter it holds.
    // Temporary: the next commit adds a read trait and this match goes away
    // in favour of calling it on whichever adapter is being polled.
    let Some((_, Battery::Zendure(zendure_client))) = devices.primary() else {
        unreachable!("devices was just constructed with exactly the one Zendure battery above")
    };

    // On by default: by the time you think to enable logging, the bug you
    // wanted it for has already happened. Any failure here disables the journal
    // and leaves control untouched.
    let (journal, journal_writer) = Journal::open(
        &config.journal_path,
        config.journal_retention_days,
        env!("CARGO_PKG_VERSION"),
        &config.session(),
    );
    let journal = std::sync::Arc::new(journal);

    let (mqtt_client, eventloop) = mqtt::create_mqtt_client(&config);
    // The sink the decision path publishes through. Its task owns the only
    // `await` against the broker; nothing below this line can block on one.
    let (publisher, mut publisher_task) = MqttPublisher::open(mqtt_client.clone());
    // What Home Assistant has already been told about. Shared with the
    // subscriber, which resets it on every ConnAck.
    let announcer = std::sync::Arc::new(Announcer::new());

    let (tx, mut rx) = mpsc::channel::<MqttEvent>(64);

    let shelly_topic = config.shelly_topic.clone();
    // The one place the configured phase is read: from here it belongs to the
    // adapter that knows what a phase is.
    let solar_phase = config.solar_phase;
    let ha_prefix = config.ha_publish_prefix.clone();
    let subscriber_journal = journal.clone();
    let subscriber_prefix = ha_prefix.clone();
    let subscriber_publisher: std::sync::Arc<dyn Publisher> = publisher.clone();
    let subscriber_announcer = announcer.clone();
    let subscriber = tokio::spawn(async move {
        mqtt::run_subscriber(
            mqtt_client,
            eventloop,
            shelly_topic,
            solar_phase,
            subscriber_prefix,
            subscriber_publisher,
            subscriber_announcer,
            tx,
            subscriber_journal,
        )
        .await;
    });

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
    // written into the world directly. Reaching past the engine was the one
    // place the world came from something the journal had no record of — which
    // made the opening minutes of every session unreplayable, since a replay
    // rebuilding the world from events would find no battery and decide
    // nothing. `step` on a `DeviceUpdate` only folds it in and returns an empty
    // `Step`, so nothing else about startup changes.
    let startup = Event::DeviceUpdate {
        at: Clock::now(config.timezone),
        id: device_id.clone(),
        measurement: Measurement::Battery(battery_state),
    };
    journal.event(&startup);
    engine.step(&startup);

    let poll_interval = config.zendure_poll_interval;
    let mut poll_timer = tokio::time::interval(poll_interval);
    // Don't fire immediately — we just polled above
    poll_timer.tick().await;

    // A deadline, not a record of when MQTT was last heard from: both the
    // reading arm and the timeout arm re-arm it. Deriving it from a "last
    // update" timestamp that only the reading arm advanced left the deadline
    // permanently in the past once a timeout fired, so `sleep_until` was always
    // ready and the loop spun for the whole outage.
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

                // A real match, not a `let else`: the second variant step 9 adds
                // would otherwise take the `else` arm, break this loop and exit
                // the process cleanly and silently. Here it is a compile error.
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

                        if let Some(decision) = step.decision {
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
                                &decision,
                                &step.directives,
                            )
                            .await;
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

                if let Some(decision) = step.decision {
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
                        &decision,
                        &step.directives,
                    )
                    .await;
                }
            }
            _ = poll_timer.tick() => {
                // Capture the response verbatim before parsing, so undocumented
                // device fields survive even though our types drop them.
                let fetched = match zendure_client.get_properties_raw().await {
                    Ok(body) => {
                        journal.raw("zendure_poll", &body);
                        serde_json::from_str::<models::ZendureReport>(&body)
                            .map_err(|e| format!("parse error: {e}"))
                    }
                    Err(e) => Err(format!("request failed: {e}")),
                };
                match fetched {
                    Ok(report) => {
                        let state = battery::BatteryState::from_properties(&report.properties, zendure_client.spec());
                        tracing::debug!(
                            "Battery poll: SOC={}%, current_power={}W",
                            state.soc,
                            state.current_power,
                        );

                        telemetry.record_and_publish(
                            &*publisher,
                            &announcer,
                            &ha_prefix,
                            &report,
                            &state,
                        );

                        let event = Event::DeviceUpdate {
                            at: Clock::now(config.timezone),
                            id: device_id.clone(),
                            measurement: Measurement::Battery(state),
                        };
                        journal.event(&event);
                        engine.step(&event);
                    }
                    Err(e) => {
                        tracing::warn!("Failed to poll battery state: {e}");
                    }
                }
            }
        }
    }

    shut_down(
        &publisher,
        &mut publisher_task,
        subscriber,
        journal,
        journal_writer,
    )
    .await;

    Ok(())
}

/// Stop everything in the one order that does not lose rows or stall.
///
/// Order matters, and it is not the order it looks like it should be.
///
/// The publisher drains **first**, while the subscriber is still running,
/// because the subscriber owns the MQTT eventloop and the eventloop is the only
/// thing that actually moves bytes to the broker. Aborting it first would leave
/// the publisher task awaiting a channel nobody drains, so every shutdown would
/// stall for the full deadline and deliver nothing.
///
/// Then the journal. Its writer stops when every sender is gone, and the
/// subscriber task holds one, so the subscriber has to be finished before the
/// last `Arc<Journal>` can drop. `abort` alone only schedules cancellation —
/// awaiting it is what guarantees the task and its captured clone are gone.
///
/// Both drains are bounded. The journal's was not, which made the deadline
/// above argue for something the code did not do: an unbounded wait ends at
/// systemd's `TimeoutStopSec`, and that ends with SIGKILL, which loses the rows
/// the wait was protecting. A deadline at least gets to say what was lost.
///
/// What the MQTT half guarantees, precisely: **hand-off, not delivery.** The
/// delivery task's `publish` returns once the request is in rumqttc's channel,
/// so the task can finish with up to fifty messages still in front of the
/// socket, and aborting the subscriber drops the eventloop that would have
/// written them. In practice the subscriber is live throughout the window and
/// flushes most of it, which is why the ordering is what it is — but a tail can
/// be lost, and the journal, not the broker, is the record that has to be
/// right.
async fn shut_down(
    publisher: &MqttPublisher,
    publisher_task: &mut PublisherTask,
    subscriber: tokio::task::JoinHandle<()>,
    journal: std::sync::Arc<Journal>,
    journal_writer: Option<Writer>,
) {
    let queued = publisher.queued();
    publisher.close();

    // Matched rather than `.is_err()`, which sees only the timeout: a task that
    // ended early resolves instantly to `Ok(Err(JoinError))`, and treating that
    // as success reported a clean drain for a task that had been dead for
    // hours.
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
            // aborting it is the one path where nobody would. `queued` is what
            // was waiting when the drain began — some of it will have gone out
            // since.
            tracing::warn!(
                "MQTT drain did not finish in {}s — {queued} messages were queued when it \
                 began, {} dropped and {} failed this session",
                DRAIN_DEADLINE.as_secs(),
                publisher.dropped(),
                publisher.failed(),
            );
        }
    }

    subscriber.abort();
    let _ = subscriber.await;
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
    use crate::config::SessionConfig;
    use crate::fixtures;
    use crate::journal;

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
                &publisher,
                &mut publisher_task,
                subscriber,
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
}
