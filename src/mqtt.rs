use std::time::Duration;

use rumqttc::{AsyncClient, Event, EventLoop, MqttOptions, Packet, QoS};
use tokio::sync::mpsc;

use crate::config::Config;
use crate::models::{ControlDecision, MeterReading};

#[derive(Debug, Clone)]
pub enum MqttEvent {
    MeterReading(MeterReading),
    SolarReading(f64),
}

pub fn create_mqtt_client(config: &Config) -> (AsyncClient, EventLoop) {
    let mut opts = MqttOptions::new(&config.mqtt_client_id, &config.mqtt_host, config.mqtt_port);
    opts.set_keep_alive(Duration::from_secs(30));
    if let (Some(user), Some(pass)) = (&config.mqtt_username, &config.mqtt_password) {
        opts.set_credentials(user, pass);
    }
    AsyncClient::new(opts, 50)
}

pub async fn run_subscriber(
    client: AsyncClient,
    mut eventloop: EventLoop,
    meter_topic: String,
    solar_topic: String,
    ha_prefix: String,
    tx: mpsc::Sender<MqttEvent>,
) {
    loop {
        match eventloop.poll().await {
            Ok(Event::Incoming(Packet::ConnAck(_))) => {
                tracing::info!("MQTT connected, subscribing to topics");
                subscribe_topics(&client, &meter_topic, &solar_topic).await;
                publish_ha_discovery(&client, &ha_prefix).await;
            }
            Ok(Event::Incoming(Packet::Publish(publish))) => {
                let topic = &publish.topic;
                let payload = &publish.payload;

                if topic == &meter_topic {
                    match serde_json::from_slice::<MeterReading>(payload) {
                        Ok(reading) => {
                            let _ = tx.send(MqttEvent::MeterReading(reading)).await;
                        }
                        Err(e) => tracing::warn!("Failed to parse meter reading: {e}"),
                    }
                } else if topic == &solar_topic {
                    match std::str::from_utf8(payload)
                        .ok()
                        .and_then(|s| s.trim().parse::<f64>().ok())
                    {
                        Some(watts) => {
                            let _ = tx.send(MqttEvent::SolarReading(watts)).await;
                        }
                        None => tracing::warn!(
                            "Failed to parse solar reading: {:?}",
                            std::str::from_utf8(payload)
                        ),
                    }
                }
            }
            Ok(_) => {}
            Err(e) => {
                tracing::error!("MQTT error: {e}");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

async fn subscribe_topics(client: &AsyncClient, meter_topic: &str, solar_topic: &str) {
    if let Err(e) = client.subscribe(meter_topic, QoS::AtMostOnce).await {
        tracing::error!("Failed to subscribe to {meter_topic}: {e}");
    }
    if let Err(e) = client.subscribe(solar_topic, QoS::AtMostOnce).await {
        tracing::error!("Failed to subscribe to {solar_topic}: {e}");
    }
}

pub async fn publish_ha_discovery(client: &AsyncClient, prefix: &str) {
    let sensors = [
        ("decision_mode", "Battery Decision Mode", "", None),
        (
            "decision_power",
            "Battery Decision Power",
            "W",
            Some("power"),
        ),
        ("decision_reason", "Battery Decision Reason", "", None),
        (
            "decision_grid_power",
            "Grid Power (at decision)",
            "W",
            Some("power"),
        ),
        (
            "decision_solar_power",
            "Solar Power (at decision)",
            "W",
            Some("power"),
        ),
    ];

    for (id, name, unit, device_class) in &sensors {
        let mut config = serde_json::json!({
            "name": name,
            "state_topic": format!("{prefix}/{id}"),
            "unique_id": format!("zendure_{id}"),
            "device": {
                "identifiers": ["zendure_controller"],
                "name": "Zendure Controller",
                "manufacturer": "Zendure",
                "model": "AC 2400+"
            }
        });

        if !unit.is_empty() {
            config["unit_of_measurement"] = serde_json::json!(unit);
        }
        if let Some(dc) = device_class {
            config["device_class"] = serde_json::json!(dc);
            config["state_class"] = serde_json::json!("measurement");
        }

        let config_topic = format!("homeassistant/sensor/zendure_{id}/config");
        if let Err(e) = client
            .publish(
                &config_topic,
                QoS::AtLeastOnce,
                true,
                config.to_string().as_bytes(),
            )
            .await
        {
            tracing::error!("Failed to publish HA discovery for {id}: {e}");
        }
    }

    tracing::info!("Published HomeAssistant MQTT discovery config");
}

pub async fn publish_decision(client: &AsyncClient, prefix: &str, decision: &ControlDecision) {
    let values: &[(&str, String)] = &[
        ("decision_mode", decision.mode.to_string()),
        ("decision_power", decision.power_watts.to_string()),
        ("decision_reason", decision.reason.clone()),
        ("decision_grid_power", format!("{:.0}", decision.grid_power)),
        (
            "decision_solar_power",
            format!("{:.0}", decision.solar_power),
        ),
    ];

    for (id, value) in values {
        let topic = format!("{prefix}/{id}");
        if let Err(e) = client
            .publish(&topic, QoS::AtMostOnce, false, value.as_bytes())
            .await
        {
            tracing::warn!("Failed to publish {topic}: {e}");
        }
    }
}
