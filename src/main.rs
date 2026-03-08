mod battery;
mod config;
mod controller;
mod grid_power;
mod models;
mod mqtt;
mod zendure;

use battery::Battery;
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
    let mut battery_state = zendure_client
        .get_state()
        .await
        .map_err(|e| e.to_string())?;
    tracing::info!(
        "Battery: SOC={}%, max_discharge={}W, max_charge={}W, current_power={}W",
        battery_state.soc,
        battery_state.max_discharge_power,
        battery_state.max_charge_power,
        battery_state.current_power,
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
                }
            }
            _ = poll_timer.tick() => {
                match zendure_client.get_state().await {
                    Ok(state) => {
                        tracing::debug!(
                            "Battery poll: SOC={}%, current_power={}W",
                            state.soc,
                            state.current_power,
                        );
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
