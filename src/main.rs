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
    tracing::info!(
        "Starting Zendure controller (simulation mode) for {}",
        config.zendure_sn
    );

    // Poll battery on startup to get actual device state
    let zendure_client = zendure::ZendureClient::new(&config.zendure_ip, config.zendure_sn.clone());
    let battery_state = zendure_client
        .get_state()
        .await
        .map_err(|e| e.to_string())?;
    tracing::info!(
        "Battery: SOC={}%, max_discharge={}W, max_charge={}W",
        battery_state.soc,
        battery_state.max_discharge_power,
        battery_state.max_charge_power,
    );

    let (mqtt_client, eventloop) = mqtt::create_mqtt_client(&config);
    let publisher_client = mqtt_client.clone();

    // Channel for MQTT events from subscriber to coordinator
    let (tx, mut rx) = mpsc::channel::<MqttEvent>(64);

    // Spawn MQTT event loop + subscriber (single connection handles both pub and sub)
    let meter_topic = config.meter_topic;
    let solar_topic = config.solar_topic;
    let ha_prefix = config.ha_publish_prefix.clone();
    let subscriber_prefix = config.ha_publish_prefix;
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
    let mut ctrl = controller::Controller::new();

    tracing::info!("Coordinator running, waiting for MQTT data...");

    while let Some(event) = rx.recv().await {
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

        let Some(meter) = &latest_meter else {
            continue;
        };

        let net_grid_power = grid_estimator.update(meter, latest_solar_power);

        let decision = ctrl.decide(net_grid_power, latest_solar_power, &battery_state);
        tracing::info!(
            "Decision: {} at {}W — {} (net_grid={:.0}W)",
            decision.mode,
            decision.power_watts,
            decision.reason,
            net_grid_power,
        );

        mqtt::publish_decision(&publisher_client, &ha_prefix, &decision).await;
    }

    Ok(())
}
