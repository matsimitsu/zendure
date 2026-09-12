//! The coordinator loop: the process's whole runtime, lifted out of `main`.
//!
//! `main` parses arguments, initialises logging, reads configuration and calls
//! [`run`] — nothing else. The split exists because the loop below was, for a
//! long time, the one part of this crate no test had ever driven, and both
//! defects that reached production hid in exactly there: the MQTT-deadline
//! spin (`c131b2f`) was found by measuring CPU on the hardware rather than by
//! the suite, and a failing device write silently disarming the failsafe
//! (`bfcf5ab`) was found by reading.
//!
//! **The loop now has a test.** [`run`] always took its stop condition as a
//! parameter — one of the two seams a test needs — and the other, the device,
//! closed once [`registry::from_config`] could build a
//! [`crate::simulation::VirtualBattery`] from `[[device]] kind = "virtual"`
//! instead of `run` reaching for `Battery::zendure` itself. Paired with a
//! synthetic meter (`[meter] kind = "synthetic"`, `source::synthetic`) feeding
//! the same `MqttEvent` channel a real Shelly subscriber would, and a
//! [`crate::publish::NullPublisher`] standing in for a broker, `run` now runs
//! entirely off configuration with nothing real on the other end of any of its
//! three external seams — see `tests::run_drives_real_decisions_against_a_virtual_battery`.
//! That test also carries its own honest limit: it uses real time, not
//! paused, and says why.
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
/// derived from a [`BatteryReading`]'s telemetry, which the objective does not
/// consult, and they are published for graphing rather than fed to the
/// engine. Seventy-odd lines of that once sat inline in a `select!` branch,
/// four levels deep, between parsing the device's response and folding it
/// into the world — so the arm's one job was the hardest thing in it to see.
/// Reaching into a vendor-shaped report was the same problem one layer
/// further down: the adapter now hands back a `BatteryReading` with the
/// telemetry already extracted, so this struct works from one device-neutral
/// shape instead of the device's own wire type.
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
    ///
    /// Takes the whole [`BatteryReading`] rather than a report and a state
    /// separately — the last two parameters this function had that named a
    /// vendor type. Both halves come from the one adapter call that produced
    /// them, and asking for them as one value is what let `run` stop
    /// reaching into the device's own wire type itself.
    fn record_and_publish(
        &mut self,
        publisher: &dyn Publisher,
        announcer: &Announcer,
        prefix: &str,
        reading: &BatteryReading,
    ) {
        let telemetry = &reading.telemetry;
        self.rte.record(telemetry.charge, telemetry.discharge);

        if let Some(pack_capacities) = &telemetry.pack_capacities {
            self.pack_capacities = pack_capacities.clone();
        }
        if let Some(min_soc) = telemetry.min_soc {
            self.min_soc = min_soc;
        }

        let total_capacity_kwh = self
            .pack_capacities
            .iter()
            .copied()
            .sum::<WattHours>()
            .to_kwh();
        let usable_kwh =
            self.rte
                .usable_kwh(reading.state.soc, self.min_soc, &self.pack_capacities);
        mqtt::publish_rte(
            publisher,
            prefix,
            self.rte.rte_percent(),
            usable_kwh,
            total_capacity_kwh,
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
    }
}

/// Start the devices, the journal, and whichever combination of a broker
/// connection and a meter source `config` asks for, then fold events until
/// `stop` resolves or every feeder task goes away.
pub async fn run(
    config: Config,
    stop: impl Future<Output = StopReason>,
) -> Result<(), Box<dyn std::error::Error>> {
    tracing::info!(
        "Starting Zendure controller for {}",
        config.device.identity()
    );

    // Built from configuration rather than a fixed `Battery::zendure` call —
    // `registry::from_config` is the one place a `DeviceConfig` becomes a
    // live adapter. Every call site below reaches this device only through
    // `Devices` and the `BatteryController`/`BatteryMonitor` traits it
    // implements.
    let devices = registry::from_config(&config);

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
    // (`zendure.rs`) for the ordering and failure policy this now runs, which
    // used to live inline here.
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

    let poll_interval = config.device.poll_interval();
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

                        telemetry.record_and_publish(
                            &*publisher,
                            &announcer,
                            &ha_prefix,
                            &reading,
                        );

                        let event = Event::DeviceUpdate {
                            at: Clock::now(config.timezone),
                            id: device_id.clone(),
                            measurement: Measurement::Battery(reading.state),
                        };
                        journal.event(&event);
                        engine.step(&event);
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

    shut_down(
        mqtt_drain
            .as_mut()
            .map(|(publisher, task)| (&**publisher, task)),
        feeders,
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
/// The publisher drains **first**, while the feeders are still running,
/// because the MQTT feeder owns the broker's eventloop and the eventloop is
/// the only thing that actually moves bytes to the broker. Aborting it first
/// would leave the publisher task awaiting a channel nobody drains, so every
/// shutdown would stall for the full deadline and deliver nothing. `mqtt` is
/// `None` in brokerless mode (no `[mqtt]` configured) — this whole half is
/// then skipped, there being no broker connection to drain.
///
/// Then the journal. Its writer stops when every sender is gone, and every
/// feeder holds one, so every feeder has to be finished before the last
/// `Arc<Journal>` can drop. `abort` alone only schedules cancellation —
/// awaiting it is what guarantees the task and its captured clone are gone.
/// **This half is unconditional**, whether or not `mqtt` was `Some`: the
/// journal is the one thing this function exists to protect, brokerless or
/// not.
///
/// Both drains are bounded. The journal's was not, which made the deadline
/// above argue for something the code did not do: an unbounded wait ends at
/// systemd's `TimeoutStopSec`, and that ends with SIGKILL, which loses the rows
/// the wait was protecting. A deadline at least gets to say what was lost.
///
/// What the MQTT half guarantees, precisely: **hand-off, not delivery.** The
/// delivery task's `publish` returns once the request is in rumqttc's channel,
/// so the task can finish with up to fifty messages still in front of the
/// socket, and aborting the MQTT feeder drops the eventloop that would have
/// written them. In practice that feeder is live throughout the window and
/// flushes most of it, which is why the ordering is what it is — but a tail can
/// be lost, and the journal, not the broker, is the record that has to be
/// right.
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
    use crate::config::{DeviceConfig, SessionConfig};
    use crate::fixtures;
    use crate::journal;
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

    /// The end-to-end test: the real `run()`, with a virtual battery, a null
    /// publisher and a synthetic meter — no network, no broker, no hardware.
    /// This module's own doc comment used to say plainly that nothing drove
    /// `run` end to end; this is what closes that gap.
    ///
    /// **Real time, bounded to a few ticks — not paused time.** Paused time
    /// was tried first, and rejected for a specific, confirmed reason rather
    /// than a vague "it didn't work": the journal's writer
    /// (`journal::writer`) is a `spawn_blocking` task whose `blocking_recv`
    /// loop only ends when every `Journal` sender is dropped, so for the
    /// whole life of this test one such task is permanently alive. With
    /// `#[tokio::test(start_paused = true)]`, tokio only auto-advances its
    /// clock when the runtime is fully idle, and an outstanding
    /// `spawn_blocking` task apparently keeps it from ever reaching that
    /// state — confirmed by bisection: pointing `journal_path` at an
    /// unwritable location (so `Journal::open` disables itself and spawns no
    /// writer at all) let the very same test complete instantly under paused
    /// time, and restoring a real journal reproduced an unconditional hang,
    /// with no decisions logged past the first tick. Since the whole point of
    /// this test is asserting against a *real* journal, disabling it to make
    /// paused time work would have thrown away the thing being tested. Real
    /// time it is instead: `run_synthetic_meter`'s ticks are 1s regardless, so
    /// three real seconds is enough for several of them, and short enough not
    /// to make the suite noticeably slower.
    ///
    /// Asserted against the **journal**, per the brief: not "it didn't
    /// crash," but that a real decision was recorded, with the mode a
    /// constant importing load and no solar can only produce (discharge) and
    /// a non-zero commanded power.
    #[tokio::test]
    async fn run_drives_real_decisions_against_a_virtual_battery() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = Config {
            mqtt: None,
            device: DeviceConfig::Virtual {
                id: "sim".to_string(),
                packs: vec![WattHours(10_000.0)],
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
            ha_publish_prefix: "test".to_string(),
            charge_margin: PowerMargin::new(50),
            discharge_margin: PowerMargin::new(5),
            charge_start_threshold: GridPower(-100.0),
            discharge_start_threshold: GridPower(0.0),
            // No cooldowns: the tuning knobs a real deployment leans on to
            // avoid chattering, turned down here so the test does not have
            // to wait out a cooldown window to see a second decision within
            // its few real seconds.
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
        };
        let journal_path = config.journal_path.clone();

        // A few real ticks of the 1s synthetic meter, and nowhere near the
        // virtual device's 10s poll interval — this test's decisions all come
        // from meter events, not from a poll.
        let stop = async {
            tokio::time::sleep(Duration::from_secs(3)).await;
            StopReason::Sigterm
        };

        run(config, stop).await.expect("run must exit cleanly");

        let conn = rusqlite::Connection::open(&journal_path).unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT command FROM decisions \
                 WHERE kind = 'decision' AND command IS NOT NULL",
            )
            .unwrap();
        let commands: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();

        assert!(
            !commands.is_empty(),
            "the objective must have recorded at least one real decision"
        );
        assert!(
            commands
                .iter()
                .all(|c| c.starts_with("set_discharge(") || c.starts_with("set_idle")),
            "a constant importing load with no solar must never charge, got {commands:?}",
        );
        assert!(
            commands
                .iter()
                .any(|c| c.starts_with("set_discharge(") && c != "set_discharge(0W)"),
            "at least one discharge decision must carry non-zero power, got {commands:?}",
        );
    }
}
