use std::time::Duration;

use rumqttc::{AsyncClient, Event, EventLoop, MqttOptions, Packet, QoS};
use tokio::sync::mpsc;

use crate::config::Config;
use crate::models::{ControlDecision, CycleCounts, MeterReading};

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
        ("rte_percent", "Battery Round-Trip Efficiency", "%", None),
        (
            "rte_usable_kwh",
            "Battery Usable Energy",
            "kWh",
            Some("energy"),
        ),
        (
            "rte_total_capacity_kwh",
            "Battery Total Capacity",
            "kWh",
            Some("energy"),
        ),
        (
            "enclosure_temp",
            "Battery Enclosure Temperature",
            "°C",
            Some("temperature"),
        ),
        (
            "battery_soc",
            "Battery State of Charge",
            "%",
            Some("battery"),
        ),
        ("daily_cycles", "Battery Daily Mode Transitions", "", None),
        (
            "daily_cooldown_suppressions",
            "Battery Daily Cooldown Suppressions",
            "",
            None,
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

    // Binary sensors
    let binary_config = serde_json::json!({
        "name": "Battery SOC Calibrating",
        "state_topic": format!("{prefix}/soc_calibrating"),
        "unique_id": "zendure_soc_calibrating",
        "payload_on": "ON",
        "payload_off": "OFF",
        "device": {
            "identifiers": ["zendure_controller"],
            "name": "Zendure Controller",
            "manufacturer": "Zendure",
            "model": "AC 2400+"
        }
    });

    let config_topic = "homeassistant/binary_sensor/zendure_soc_calibrating/config";
    if let Err(e) = client
        .publish(
            config_topic,
            QoS::AtLeastOnce,
            true,
            binary_config.to_string().as_bytes(),
        )
        .await
    {
        tracing::error!("Failed to publish HA discovery for soc_calibrating: {e}");
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

pub async fn publish_cycle_counts(client: &AsyncClient, prefix: &str, counts: &CycleCounts) {
    let values: &[(&str, String)] = &[
        ("daily_cycles", counts.daily_transitions.to_string()),
        (
            "daily_cooldown_suppressions",
            counts.daily_cooldown_suppressions.to_string(),
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

pub async fn publish_rte(
    client: &AsyncClient,
    prefix: &str,
    rte_percent: Option<f64>,
    usable_kwh: f64,
    total_capacity_kwh: f64,
) {
    let values: &[(&str, String)] = &[
        (
            "rte_percent",
            rte_percent.map_or("unknown".to_string(), |v| format!("{v:.1}")),
        ),
        ("rte_usable_kwh", format!("{usable_kwh:.2}")),
        ("rte_total_capacity_kwh", format!("{total_capacity_kwh:.2}")),
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

pub async fn publish_soc_calibrating(client: &AsyncClient, prefix: &str, calibrating: bool) {
    let topic = format!("{prefix}/soc_calibrating");
    let value = if calibrating { "ON" } else { "OFF" };
    if let Err(e) = client
        .publish(&topic, QoS::AtMostOnce, false, value.as_bytes())
        .await
    {
        tracing::warn!("Failed to publish {topic}: {e}");
    }
}

pub async fn publish_battery_soc(client: &AsyncClient, prefix: &str, soc: u32) {
    let topic = format!("{prefix}/battery_soc");
    if let Err(e) = client
        .publish(&topic, QoS::AtMostOnce, false, soc.to_string().as_bytes())
        .await
    {
        tracing::warn!("Failed to publish {topic}: {e}");
    }
}

/// Convert a Zendure temperature (tenths of Kelvin) to degrees Celsius.
fn tenths_kelvin_to_celsius(value: u32) -> f64 {
    (value as f64 / 10.0) - 273.15
}

pub async fn publish_temperatures(
    client: &AsyncClient,
    prefix: &str,
    enclosure_temp: Option<u32>,
    pack_temps: &[(usize, u32)],
) {
    // Publish per-pack discovery + state (dynamic number of packs)
    for &(idx, raw_temp) in pack_temps {
        let id = format!("pack{idx}_temp");
        let name = format!("Battery Pack {idx} Temperature");
        publish_sensor_discovery(client, prefix, &id, &name, "°C", Some("temperature")).await;

        let celsius = tenths_kelvin_to_celsius(raw_temp);
        let topic = format!("{prefix}/{id}");
        if let Err(e) = client
            .publish(
                &topic,
                QoS::AtMostOnce,
                false,
                format!("{celsius:.1}").as_bytes(),
            )
            .await
        {
            tracing::warn!("Failed to publish {topic}: {e}");
        }
    }

    // Publish enclosure temperature state
    if let Some(raw_temp) = enclosure_temp {
        let celsius = tenths_kelvin_to_celsius(raw_temp);
        let topic = format!("{prefix}/enclosure_temp");
        if let Err(e) = client
            .publish(
                &topic,
                QoS::AtMostOnce,
                false,
                format!("{celsius:.1}").as_bytes(),
            )
            .await
        {
            tracing::warn!("Failed to publish {topic}: {e}");
        }
    }
}

async fn publish_sensor_discovery(
    client: &AsyncClient,
    prefix: &str,
    id: &str,
    name: &str,
    unit: &str,
    device_class: Option<&str>,
) {
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
