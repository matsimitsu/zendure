mod battery;
mod clock;
mod command;
mod config;
mod controller;
mod engine;
mod event;
mod models;
mod mqtt;
mod rawlog;
mod rte;
mod units;
mod zendure;

use clock::Clock;
use config::{Config, SolarPhase};
use engine::Engine;
use event::Event;
use models::StorageMode;
use mqtt::MqttEvent;
use rawlog::RawLog;
use tokio::sync::mpsc;
use units::{GridPower, Soc, SolarPower, WattHours, Watts};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("zendure=info".parse().unwrap()),
        )
        .init();

    let config = Config::from_env()?;
    tracing::info!("Starting Zendure controller for {}", config.zendure_sn);

    let zendure_client = zendure::ZendureClient::new(&config.zendure_ip, config.zendure_sn.clone());
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
    let battery_state = battery::BatteryState::from_properties(&battery_report.properties);

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

    // Raw capture: append-only NDJSON of everything in and out, on by default.
    // A bridge until the structured journal lands, but recorded data cannot be
    // backfilled, so it starts now. Any failure here disables the log and
    // leaves control untouched.
    let raw_log = RawLog::new(
        std::path::PathBuf::from(
            std::env::var("JOURNAL_RAW_PATH")
                .unwrap_or_else(|_| "/var/lib/zendure/raw".to_string()),
        ),
        std::env::var("JOURNAL_RETENTION_DAYS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(90),
    )
    .map(std::sync::Arc::new);

    let (mqtt_client, eventloop) = mqtt::create_mqtt_client(&config);
    let publisher_client = mqtt_client.clone();

    let (tx, mut rx) = mpsc::channel::<MqttEvent>(64);

    let shelly_topic = config.shelly_topic.clone();
    let ha_prefix = config.ha_publish_prefix.clone();
    let subscriber_prefix = config.ha_publish_prefix.clone();
    let subscriber_log = raw_log.clone();
    tokio::spawn(async move {
        mqtt::run_subscriber(
            mqtt_client,
            eventloop,
            shelly_topic,
            subscriber_prefix,
            tx,
            subscriber_log,
        )
        .await;
    });

    let mqtt_timeout = config.mqtt_timeout;
    let mut engine = Engine::new(
        controller::Controller::from_config(&config, &Clock::now(config.timezone)),
        battery_state,
        mqtt_timeout,
    );

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

    tracing::info!("Coordinator running, waiting for MQTT data...");

    loop {
        tokio::select! {
            event = rx.recv() => {
                let Some(MqttEvent::GridPowerReading(reading)) = event else { break };
                mqtt_deadline = tokio::time::Instant::now() + mqtt_timeout;

                let net_grid_power = GridPower(reading.total_act_power);
                // Solar production = export (negative power) on the phase the
                // inverter feeds into. The meter total nets this against loads
                // on other phases, so read the single phase directly.
                let solar_phase_power = match config.solar_phase {
                    SolarPhase::A => GridPower(reading.a_act_power),
                    SolarPhase::B => GridPower(reading.b_act_power),
                    SolarPhase::C => GridPower(reading.c_act_power),
                };
                let solar_power = SolarPower::from_phase_export(solar_phase_power);
                tracing::info!(
                    "Shelly: total={:.0}W (A={:.0} B={:.0} C={:.0}), solar={:.0}W",
                    reading.total_act_power,
                    reading.a_act_power,
                    reading.b_act_power,
                    reading.c_act_power,
                    solar_power,
                );

                let clock = Clock::now(config.timezone);
                let step = engine.step(&Event::GridPower {
                    at: clock,
                    total: net_grid_power,
                    solar: solar_power,
                });

                if let Some(status) = step.status {
                    tracing::info!("MQTT updates resumed");
                    mqtt::publish_status(&publisher_client, &ha_prefix, status).await;
                }

                if let Some(decision) = step.decision {
                    tracing::info!(
                        "Decision: {} at {}W — {} (net_grid={:.0}W, battery: SOC={}%, max_charge={}W, max_discharge={}W, current={}W, soc_limit={})",
                        decision.mode,
                        decision.power_watts,
                        decision.reason,
                        net_grid_power,
                        engine.battery().soc,
                        engine.battery().max_charge_power,
                        engine.battery().max_discharge_power,
                        engine.battery().current_power,
                        engine.battery().soc_limit_reached,
                    );

                    let mut outcome = "no_command";
                    let mut error = None;
                    if let Some(command) = step.commands.first() {
                        if let Err(e) = zendure_client.apply_command(command).await {
                            tracing::error!("Failed to apply decision to battery: {e}");
                            outcome = "error";
                            error = Some(e.to_string());
                            mqtt::publish_status(&publisher_client, &ha_prefix, "zendure_api_error").await;
                        } else {
                            outcome = "ok";
                            mqtt::publish_status(&publisher_client, &ha_prefix, "operational").await;
                        }
                    }

                    // Recorded after actuation, so `outcome` reflects whether the
                    // write to the device actually landed — which is what you want
                    // when reconstructing an incident.
                    if let Some(log) = &raw_log {
                        log.value("decision", &serde_json::json!({
                            "decision": &decision,
                            "command": step.commands.first().map(|c| c.to_string()),
                            "outcome": outcome,
                            "error": error,
                        }));
                    }

                    mqtt::publish_decision(&publisher_client, &ha_prefix, &decision).await;
                    mqtt::publish_cycle_counts(
                        &publisher_client,
                        &ha_prefix,
                        &engine.cycle_counts(),
                    )
                    .await;
                }
            }
            _ = tokio::time::sleep_until(mqtt_deadline) => {
                mqtt_deadline = tokio::time::Instant::now() + mqtt_timeout;

                let clock = Clock::now(config.timezone);
                let step = engine.step(&Event::MqttTimeout { at: clock });

                if step.status.is_some() {
                    tracing::warn!(
                        "No MQTT updates for {}s — forcing idle as safety failsafe",
                        mqtt_timeout.as_secs(),
                    );
                }

                if let Some(decision) = step.decision {
                    let mut outcome = "no_command";
                    let mut error = None;
                    if let Some(command) = step.commands.first() {
                        if let Err(e) = zendure_client.apply_command(command).await {
                            tracing::error!("Failed to apply failsafe idle to battery: {e}");
                            outcome = "error";
                            error = Some(e.to_string());
                            mqtt::publish_status(&publisher_client, &ha_prefix, "mqtt_timeout_api_error").await;
                        } else {
                            outcome = "ok";
                            mqtt::publish_status(&publisher_client, &ha_prefix, "mqtt_timeout").await;
                        }
                    }

                    if let Some(log) = &raw_log {
                        log.value("failsafe", &serde_json::json!({
                            "decision": &decision,
                            "command": step.commands.first().map(|c| c.to_string()),
                            "outcome": outcome,
                            "error": error,
                        }));
                    }

                    mqtt::publish_decision(&publisher_client, &ha_prefix, &decision).await;
                }
            }
            _ = poll_timer.tick() => {
                // Capture the response verbatim before parsing, so undocumented
                // device fields survive even though our types drop them.
                let fetched = match zendure_client.get_properties_raw().await {
                    Ok(body) => {
                        if let Some(log) = &raw_log {
                            log.raw("zendure_poll", &body);
                        }
                        serde_json::from_str::<models::ZendureReport>(&body)
                            .map_err(|e| format!("parse error: {e}"))
                    }
                    Err(e) => Err(format!("request failed: {e}")),
                };
                match fetched {
                    Ok(report) => {
                        let state = battery::BatteryState::from_properties(&report.properties);
                        tracing::debug!(
                            "Battery poll: SOC={}%, current_power={}W",
                            state.soc,
                            state.current_power,
                        );

                        // Feed RTE tracker with charge/discharge power
                        rte_tracker.record(
                            Watts::from_device(report.properties.output_pack_power.unwrap_or(0)),
                            Watts::from_device(report.properties.pack_input_power.unwrap_or(0)),
                        );

                        // Update pack data and SOC limits if available
                        if report.pack_data.is_some() {
                            pack_capacities = rte::pack_capacities(&report.pack_data);
                        }
                        if let Some(ms) = report.properties.min_soc {
                            min_soc_percent = Soc::from_tenths(ms);
                        }

                        // Publish RTE sensors
                        let total_capacity_kwh =
                            pack_capacities.iter().copied().sum::<WattHours>().to_kwh();
                        let usable_kwh =
                            rte_tracker.usable_kwh(state.soc, min_soc_percent, &pack_capacities);
                        mqtt::publish_rte(
                            &publisher_client,
                            &ha_prefix,
                            rte_tracker.rte_percent(),
                            usable_kwh,
                            total_capacity_kwh,
                        )
                        .await;

                        // Publish temperature sensors
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
                        mqtt::publish_temperatures(
                            &publisher_client,
                            &ha_prefix,
                            report.properties.hyper_tmp,
                            &pack_temps,
                        )
                        .await;

                        // Publish SOC calibration state
                        mqtt::publish_soc_calibrating(
                            &publisher_client,
                            &ha_prefix,
                            state.soc_calibrating,
                        )
                        .await;

                        mqtt::publish_battery_soc(
                            &publisher_client,
                            &ha_prefix,
                            state.soc,
                        )
                        .await;

                        // Publish actual battery power
                        mqtt::publish_battery_power(
                            &publisher_client,
                            &ha_prefix,
                            Watts::from_device(report.properties.output_pack_power.unwrap_or(0)),
                            Watts::from_device(report.properties.pack_input_power.unwrap_or(0)),
                        )
                        .await;

                        // Persist RTE state periodically (every poll)
                        rte_tracker.save();

                        engine.step(&Event::BatteryUpdate {
                            at: Clock::now(config.timezone),
                            state,
                        });
                    }
                    Err(e) => {
                        tracing::warn!("Failed to poll battery state: {e}");
                    }
                }
            }
        }
    }

    Ok(())
}
