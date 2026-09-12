mod allocate;
mod battery;
mod cli;
mod clock;
mod command;
mod commands;
mod config;
mod controller;
mod device;
mod engine;
mod event;
#[cfg(test)]
mod fixtures;
mod journal;
mod models;
mod mqtt;
mod replay;
mod rte;
mod source;
mod units;
mod world;
mod zendure;

use allocate::Directive;
use clock::Clock;
use config::Config;
use device::{Applied, ControlPath, Outcome};
use engine::Engine;
use event::Event;
use journal::Journal;
use models::StorageMode;
use mqtt::MqttEvent;
use rumqttc::AsyncClient;
use tokio::sync::mpsc;
use units::{Soc, WattHours, Watts};
use world::{Measurement, World};
use zendure::ZendureClient;

/// Actuate a step's directives and report the fleet's health once.
///
/// Both arms of the loop — a decision and the failsafe idle — do exactly this,
/// differing only in which path is actuating and what the two status strings
/// are. Inlined twice it was the same twelve lines written out, and the third
/// caller (step 9's charger) would have made it three; extracted, the ordering
/// that matters is stated in one place.
///
/// The outcomes come back rather than being journalled here, because that write
/// has to happen *after* this returns, so the recorded outcome reflects whether
/// the device write actually landed. The `ControlPath` carries everything the
/// two arms used to differ by — the log wording, both status strings, and the
/// journal's `kind`.
async fn apply_and_publish(
    client: &ZendureClient,
    publisher: &AsyncClient,
    prefix: &str,
    directives: &[Directive],
    path: ControlPath,
) -> Vec<Outcome> {
    // The whole list, not its first element: a step that means "stop one box,
    // start another" has to reach both devices.
    let outcomes = device::actuate(client, directives, path).await;
    let failed = outcomes.iter().any(|o| o.applied == Applied::Error);

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
        mqtt::publish_status(publisher, prefix, status).await;
    }

    outcomes
}

/// Everything the poll produces that the decision never reads.
///
/// Round-trip efficiency, pack temperatures, SOC and battery power are all
/// derived from `ZendureReport` fields the objective does not consult, and they
/// are published for graphing rather than fed to the engine. Seventy-odd lines
/// of that sat inline in a `select!` branch, four levels of indentation deep,
/// between parsing the response and folding it into the world — so the arm's one
/// job was the hardest thing in it to see.
///
/// Takes `&mut` for the three pieces of state a poll advances: the rolling RTE
/// window, the pack capacities and the device's own minimum SOC, each of which
/// only ever changes here.
#[allow(clippy::too_many_arguments)]
async fn publish_poll_telemetry(
    publisher: &AsyncClient,
    prefix: &str,
    report: &models::ZendureReport,
    state: &battery::BatteryState,
    rte_tracker: &mut rte::RteTracker,
    pack_capacities: &mut Vec<WattHours>,
    min_soc_percent: &mut Soc,
) {
    let charge = Watts::from_device(report.properties.output_pack_power.unwrap_or(0));
    let discharge = Watts::from_device(report.properties.pack_input_power.unwrap_or(0));
    rte_tracker.record(charge, discharge);

    if report.pack_data.is_some() {
        *pack_capacities = rte::pack_capacities(&report.pack_data);
    }
    if let Some(ms) = report.properties.min_soc {
        *min_soc_percent = Soc::from_tenths(ms);
    }

    let total_capacity_kwh = pack_capacities.iter().copied().sum::<WattHours>().to_kwh();
    let usable_kwh = rte_tracker.usable_kwh(state.soc, *min_soc_percent, pack_capacities);
    mqtt::publish_rte(
        publisher,
        prefix,
        rte_tracker.rte_percent(),
        usable_kwh,
        total_capacity_kwh,
    )
    .await;

    let pack_temps: Vec<(usize, u32)> = report
        .pack_data
        .as_ref()
        .map(|packs| {
            packs
                .iter()
                .enumerate()
                .filter_map(|(i, p)| p.max_temp.map(|t| (i, t)))
                .collect()
        })
        .unwrap_or_default();
    mqtt::publish_temperatures(publisher, prefix, report.properties.hyper_tmp, &pack_temps).await;

    mqtt::publish_soc_calibrating(publisher, prefix, state.soc_calibrating).await;
    mqtt::publish_battery_soc(publisher, prefix, state.soc).await;
    mqtt::publish_battery_power(publisher, prefix, charge, discharge).await;

    // Persisted every poll, so the rolling 24h window survives a restart.
    rte_tracker.save();
}

/// End a subcommand, printing any failure as a message rather than as a
/// `Debug`-formatted error struct.
fn finish(
    result: Result<(), Box<dyn std::error::Error>>,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Err(e) = result {
        eprintln!("{e}");
        std::process::exit(1);
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // `args_os` rather than `args`, which panics on argv that is not UTF-8 —
    // the parser has a clean error for an argument it does not understand, and
    // a panic is not it.
    let args = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned());

    match cli::parse(args) {
        // Falls through to the controller below. No arguments has always meant
        // "run", and that is how the service invokes it.
        Ok(cli::Invocation::Daemon) => {}
        Ok(cli::Invocation::Help) => {
            print!("{}", cli::HELP);
            return Ok(());
        }
        Ok(cli::Invocation::Export { from, to, db, out }) => {
            return finish(commands::export(&db, from, to, out.as_deref()));
        }
        Ok(cli::Invocation::Replay {
            fixture,
            verify,
            overrides,
        }) => return finish(commands::replay_fixture(&fixture, verify, &overrides)),
        Err(message) => {
            // Printed and exited rather than returned. `main` renders an `Err`
            // with `Debug`, which turns a multi-line usage message into one
            // quoted line full of `\n`.
            eprintln!("{message}");
            std::process::exit(2);
        }
    }

    // `RUST_LOG` wins outright when it is set. It used to be merged with a
    // hard-coded `zendure=info`, and `add_directive` *replaces* a directive with
    // the same target rather than merging — so `RUST_LOG=zendure=debug` was
    // silently overwritten back to `info` and the module's debug lines were
    // unreachable by the one incantation an operator would try.
    let filter = match std::env::var("RUST_LOG") {
        Ok(spec) if !spec.trim().is_empty() => tracing_subscriber::EnvFilter::new(spec),
        _ => tracing_subscriber::EnvFilter::new("zendure=info"),
    };
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let config = Config::from_env()?;
    tracing::info!("Starting Zendure controller for {}", config.zendure_sn);

    let zendure_client = zendure::ZendureClient::new(&config.zendure_ip, config.zendure_sn.clone());

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

    let mut pack_capacities = rte::pack_capacities(&initial_report.pack_data);
    let mut min_soc_percent: Soc = initial_report
        .properties
        .min_soc
        .map(Soc::from_tenths)
        .unwrap_or(Soc::ZERO);
    tracing::info!(
        "Battery: SOC={}%, max_discharge={}W, max_charge={}W, current_power={}W, packs={}",
        battery_state.soc,
        battery_state.max_discharge_power,
        battery_state.max_charge_power,
        battery_state.current_power,
        pack_capacities.len(),
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
    let publisher_client = mqtt_client.clone();

    let (tx, mut rx) = mpsc::channel::<MqttEvent>(64);

    let shelly_topic = config.shelly_topic.clone();
    // The one place the configured phase is read: from here it belongs to the
    // adapter that knows what a phase is.
    let solar_phase = config.solar_phase;
    let ha_prefix = config.ha_publish_prefix.clone();
    let subscriber_prefix = config.ha_publish_prefix.clone();
    let subscriber_journal = journal.clone();
    let subscriber = tokio::spawn(async move {
        mqtt::run_subscriber(
            mqtt_client,
            eventloop,
            shelly_topic,
            solar_phase,
            subscriber_prefix,
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

    let rte_state_path = std::path::PathBuf::from(
        std::env::var("RTE_STATE_PATH")
            .unwrap_or_else(|_| "/var/lib/zendure/rte_state.json".to_string()),
    );
    let mut rte_tracker = rte::RteTracker::new(rte_state_path);

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

    // systemd stops this process with SIGTERM. Without an arm for it the process
    // simply died, and everything still queued for the journal's writer died
    // with it — a durability regression against the NDJSON capture, which wrote
    // synchronously on the calling thread and so survived any kill. The row most
    // worth having is the last decision before a restart.
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

    tracing::info!("Coordinator running, waiting for MQTT data...");

    loop {
        tokio::select! {
            _ = sigterm.recv() => {
                tracing::info!("SIGTERM — draining the journal and stopping");
                break;
            }
            _ = sigint.recv() => {
                tracing::info!("SIGINT — draining the journal and stopping");
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
                            mqtt::publish_status(&publisher_client, &ha_prefix, status).await;
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

                            let outcomes = apply_and_publish(
                                &zendure_client,
                                &publisher_client,
                                &ha_prefix,
                                &step.directives,
                                ControlPath::Objective,
                            )
                            .await;

                            // Recorded after actuation, so each outcome reflects whether
                            // the write to that device actually landed — which is what you
                            // want when reconstructing an incident. The engine's state goes
                            // in beside it so the row carries the inputs and the history the
                            // decision came from, not just its conclusion.
                                journal.decision(
                                    clock.now,
                                    ControlPath::Objective,
                                    &decision,
                                    &engine.state(),
                                    &outcomes,
                                );

                            mqtt::publish_decision(&publisher_client, &ha_prefix, &decision).await;
                            mqtt::publish_cycle_counts(
                                &publisher_client,
                                &ha_prefix,
                                &engine.cycle_counts(),
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
                    let outcomes = apply_and_publish(
                        &zendure_client,
                        &publisher_client,
                        &ha_prefix,
                        &step.directives,
                        ControlPath::Failsafe,
                    )
                    .await;

                        journal.decision(
                            clock.now,
                            ControlPath::Failsafe,
                            &decision,
                            &engine.state(),
                            &outcomes,
                        );

                    mqtt::publish_decision(&publisher_client, &ha_prefix, &decision).await;
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

                        publish_poll_telemetry(
                            &publisher_client,
                            &ha_prefix,
                            &report,
                            &state,
                            &mut rte_tracker,
                            &mut pack_capacities,
                            &mut min_soc_percent,
                        )
                        .await;

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

    // Order matters. The writer stops when every sender is gone, and the
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
