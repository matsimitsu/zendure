mod battery;
mod config;
mod controller;
mod grid_power;
mod models;
mod mqtt;
mod rte;
mod zendure;

use config::Config;
use grid_power::{GridPowerEstimator, KwhDeltaEstimator};
use mqtt::MqttEvent;
use tokio::sync::mpsc;

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
    let mut battery_state = battery::BatteryState::from_properties(&initial_report.properties);
    let mut pack_capacities = rte::pack_capacities(&initial_report.pack_data);
    let mut min_soc_percent: u32 = initial_report
        .properties
        .min_soc
        .map(|v| v / 10)
        .unwrap_or(0);
    tracing::info!(
        "Battery: SOC={}%, max_discharge={}W, max_charge={}W, current_power={}W, packs={}",
        battery_state.soc,
        battery_state.max_discharge_power,
        battery_state.max_charge_power,
        battery_state.current_power,
        pack_capacities.len(),
    );

    let (mqtt_client, eventloop) = mqtt::create_mqtt_client(&config);
    let publisher_client = mqtt_client.clone();

    let (tx, mut rx) = mpsc::channel::<MqttEvent>(64);

    let meter_topic = config.meter_topic.clone();
    let solar_topic = config.solar_topic.clone();
    let ha_prefix = config.ha_publish_prefix.clone();
    let subscriber_prefix = config.ha_publish_prefix.clone();
    tokio::spawn(async move {
        mqtt::run_subscriber(
            mqtt_client,
            eventloop,
            meter_topic,
            solar_topic,
            subscriber_prefix,
            tx,
        )
        .await;
    });

    let mut latest_meter: Option<models::MeterReading> = None;
    let mut latest_solar_power: f64 = 0.0;
    let mut grid_estimator = KwhDeltaEstimator::new();
    let mut ctrl = controller::Controller::from_config(&config);

    let rte_state_path = std::path::PathBuf::from(
        std::env::var("RTE_STATE_PATH")
            .unwrap_or_else(|_| "/tmp/zendure_rte_state.json".to_string()),
    );
    let mut rte_tracker = rte::RteTracker::new(rte_state_path);

    let poll_interval = std::time::Duration::from_secs(config.zendure_poll_interval_secs);
    let mut poll_timer = tokio::time::interval(poll_interval);
    // Don't fire immediately — we just polled above
    poll_timer.tick().await;

    tracing::info!("Coordinator running, waiting for MQTT data...");

    loop {
        tokio::select! {
            event = rx.recv() => {
                let Some(event) = event else { break };

                let is_meter_update = matches!(&event, MqttEvent::MeterReading(_));

                match event {
                    MqttEvent::MeterReading(reading) => {
                        tracing::info!(
                            "Meter: total={:.0}W (P1={:.0} P2={:.0} P3={:.0})",
                            reading.total_power,
                            reading.phase1_power,
                            reading.phase2_power,
                            reading.phase3_power,
                        );
                        latest_meter = Some(reading);
                    }
                    MqttEvent::SolarReading(watts) => {
                        latest_solar_power = watts;
                        tracing::info!("Solar: {:.0}W", watts);
                    }
                }

                // Only make decisions on meter updates — solar just stores the
                // latest value for the next meter-triggered decision.
                if !is_meter_update {
                    continue;
                }

                let Some(meter) = &latest_meter else {
                    continue;
                };

                let net_grid_power = grid_estimator.update(meter, latest_solar_power);

                if let Some(decision) = ctrl.decide(net_grid_power, latest_solar_power, &battery_state) {
                    tracing::info!(
                        "Decision: {} at {}W — {} (net_grid={:.0}W)",
                        decision.mode,
                        decision.power_watts,
                        decision.reason,
                        net_grid_power,
                    );

                    mqtt::publish_decision(&publisher_client, &ha_prefix, &decision).await;
                    mqtt::publish_cycle_counts(
                        &publisher_client,
                        &ha_prefix,
                        &ctrl.cycle_counts(),
                    )
                    .await;
                }
            }
            _ = poll_timer.tick() => {
                match zendure_client.get_properties().await {
                    Ok(report) => {
                        let state = battery::BatteryState::from_properties(&report.properties);
                        tracing::debug!(
                            "Battery poll: SOC={}%, current_power={}W",
                            state.soc,
                            state.current_power,
                        );

                        // Feed RTE tracker with charge/discharge power
                        let charge_w = report.properties.output_pack_power.unwrap_or(0) as f64;
                        let discharge_w = report.properties.pack_input_power.unwrap_or(0) as f64;
                        rte_tracker.record(charge_w, discharge_w);

                        // Update pack data and SOC limits if available
                        if report.pack_data.is_some() {
                            pack_capacities = rte::pack_capacities(&report.pack_data);
                        }
                        if let Some(ms) = report.properties.min_soc {
                            min_soc_percent = ms / 10;
                        }

                        // Publish RTE sensors
                        let total_capacity_kwh: f64 =
                            pack_capacities.iter().sum::<f64>() / 1000.0;
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

                        // Persist RTE state periodically (every poll)
                        rte_tracker.save();

                        battery_state = state;
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
