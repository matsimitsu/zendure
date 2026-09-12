//! The coordinator loop: the process's whole runtime, lifted out of `main`.
//!
//! `main` now parses arguments, initialises logging, reads configuration and
//! calls [`run`] — nothing else. That split exists for one reason: the loop
//! below could not be driven by a test while it was inlined in `main`, and the
//! two defects that reached production hid in exactly the place no test could
//! reach. The MQTT-deadline spin (`c131b2f`) was found by measuring CPU on the
//! hardware, not by the suite; a failing device write silently disarming the
//! failsafe (`bfcf5ab`) was found by reading. Both lived in this `select!`.
//!
//! [`run`] takes its stop condition as a parameter rather than registering
//! signal handlers itself, so a test can end the loop deterministically where
//! production ends it on SIGTERM.

use std::future::Future;
use std::time::Duration;

/// How long a shutdown waits for queued messages to reach the broker.
///
/// Bounded rather than unbounded: systemd's `TimeoutStopSec` is the only
/// other thing that would end the wait, and it ends it with SIGKILL, which
/// takes the journal's drain down with it. Two seconds is long enough for a
/// healthy broker to take a full queue and short enough to be invisible.
const DRAIN_DEADLINE: Duration = Duration::from_secs(2);

use crate::allocate::Directive;
use crate::announce::Announcer;
use crate::clock::Clock;
use crate::config::Config;
use crate::device::{self, Applied, ControlPath};
use crate::engine::Engine;
use crate::event::Event;
use crate::journal::Journal;
use crate::models::ControlDecision;
use crate::models::StorageMode;
use crate::mqtt::{self, MqttEvent, MqttPublisher};
use crate::publish::Publisher;
use crate::units::{DeciKelvin, Soc, Timestamp, WattHours, Watts};
use crate::world::{Measurement, World};
use crate::zendure::ZendureClient;
use crate::{battery, controller, models, rte};
use tokio::sync::mpsc;

/// Why the loop stopped.
///
/// Carries which signal fired so the log line stays the one operators grep for,
/// rather than collapsing two arms into one message that names neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stopped {
    Sigterm,
    Sigint,
}

impl std::fmt::Display for Stopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Stopped::Sigterm => write!(f, "SIGTERM"),
            Stopped::Sigint => write!(f, "SIGINT"),
        }
    }
}

/// The stop condition production runs with.
///
/// Registers both handlers eagerly and returns a future that resolves on the
/// first of them, so a registration failure is reported at startup rather than
/// leaving a process that cannot be asked to stop.
///
/// systemd stops this process with SIGTERM. Without an arm for it the process
/// simply died, and everything still queued for the journal's writer died with
/// it — a durability regression against the NDJSON capture, which wrote
/// synchronously on the calling thread and so survived any kill. The row most
/// worth having is the last decision before a restart.
pub fn shutdown_signal() -> Result<impl Future<Output = Stopped>, std::io::Error> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    Ok(async move {
        tokio::select! {
            _ = sigterm.recv() => Stopped::Sigterm,
            _ = sigint.recv() => Stopped::Sigint,
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
    client: &ZendureClient,
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
    let outcomes = device::actuate(client, directives, path).await;
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
    stop: impl Future<Output = Stopped>,
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
    let subscriber_prefix = config.ha_publish_prefix.clone();
    let subscriber_journal = journal.clone();
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
                                &zendure_client,
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
                        &zendure_client,
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

    // Order matters, and it is not the order it looks like it should be.
    //
    // The publisher drains *first*, while the subscriber is still running,
    // because the subscriber owns the MQTT eventloop and the eventloop is the
    // only thing that actually moves bytes to the broker. Aborting it first
    // would leave the publisher task awaiting a channel nobody drains, so every
    // shutdown would stall for the full deadline and deliver nothing.
    let queued = publisher.close();
    // Matched rather than `.is_err()`, which sees only the timeout: a task that
    // ended early resolves instantly to `Ok(Err(JoinError))`, and treating that
    // as success reported a clean drain for a task that had been dead for
    // hours. The three outcomes are genuinely different and only one is fine.
    match tokio::time::timeout(DRAIN_DEADLINE, &mut publisher_task).await {
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
            // was waiting when the drain *began* — some of it will have gone
            // out since.
            tracing::warn!(
                "MQTT drain did not finish in {}s — {queued} messages were queued when it \
                 began, {} dropped and {} failed this session",
                DRAIN_DEADLINE.as_secs(),
                publisher.dropped(),
                publisher.failed(),
            );
        }
    }

    // Then the journal. The writer stops when every sender is gone, and the
    // subscriber task holds one, so it has to be finished before the last
    // `Arc<Journal>` can drop. `abort` alone only schedules cancellation —
    // awaiting it is what guarantees the task and its captured clone are gone.
    subscriber.abort();
    let _ = subscriber.await;
    drop(journal);
    if let Some(writer) = journal_writer {
        let _ = writer.await;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two signal arms became one, so the wording an operator greps for is
    /// now produced by `Display` rather than written out twice. This is the
    /// only thing the extraction changed the shape of; pin it.
    #[test]
    fn stopping_renders_the_signal_name_the_log_line_always_used() {
        assert_eq!(
            format!("{} — draining the journal and stopping", Stopped::Sigterm),
            "SIGTERM — draining the journal and stopping",
        );
        assert_eq!(
            format!("{} — draining the journal and stopping", Stopped::Sigint),
            "SIGINT — draining the journal and stopping",
        );
    }
}
