use std::sync::Arc;
use std::time::Duration;

use rumqttc::{AsyncClient, Event, EventLoop, MqttOptions, Packet, QoS};
use tokio::sync::mpsc;

use crate::config::Config;
use crate::journal::Journal;
use crate::models::{ControlDecision, CycleCounts};
use crate::source::MeterObservation;
use crate::source::shelly::{self, SolarPhase};
use crate::units::{KiloWattHours, Percent, Soc, Watts};

#[derive(Debug, Clone)]
pub enum MqttEvent {
    /// Already normalized, not the meter's own JSON. The undecoded payload is
    /// captured verbatim by the raw log one line before it is parsed, so
    /// pushing the DTO down the channel as well would buy nothing — and would
    /// cost the coordinator loop its ignorance of what a Shelly is.
    Meter(MeterObservation),
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
    shelly_topic: String,
    solar_phase: SolarPhase,
    ha_prefix: String,
    tx: mpsc::Sender<MqttEvent>,
    journal: Arc<Journal>,
) {
    loop {
        match eventloop.poll().await {
            Ok(Event::Incoming(Packet::ConnAck(_))) => {
                tracing::info!("MQTT connected, subscribing to {shelly_topic}");
                if let Err(e) = client.subscribe(&shelly_topic, QoS::AtMostOnce).await {
                    tracing::error!("Failed to subscribe to {shelly_topic}: {e}");
                }
                publish_ha_discovery(&client, &ha_prefix).await;
            }
            Ok(Event::Incoming(Packet::Publish(publish))) => {
                if publish.topic == shelly_topic {
                    // Capture before parsing: a reading we fail to decode is
                    // exactly the one worth having on record.
                    journal.raw("shelly", &String::from_utf8_lossy(&publish.payload));
                    match shelly::parse(&publish.payload, solar_phase) {
                        Ok(obs) => {
                            // Logged here rather than in the coordinator loop,
                            // because this is where the reading now exists.
                            // The line is unchanged, but it is emitted from the
                            // subscriber task, so it can interleave with the
                            // `Decision:` line a few microseconds differently
                            // than it used to.
                            tracing::info!(
                                "Shelly: total={:.0}W (A={:.0} B={:.0} C={:.0}), solar={:.0}W",
                                obs.grid.total,
                                obs.grid.phases[0],
                                obs.grid.phases[1],
                                obs.grid.phases[2],
                                obs.solar,
                            );
                            let _ = tx.send(MqttEvent::Meter(obs)).await;
                        }
                        Err(e) => tracing::warn!("Failed to parse Shelly reading: {e}"),
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
        (
            "battery_charge_power",
            "Battery Actual Charge Power",
            "W",
            Some("power"),
        ),
        (
            "battery_discharge_power",
            "Battery Actual Discharge Power",
            "W",
            Some("power"),
        ),
        ("controller_status", "Controller Status", "", None),
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
    rte_percent: Option<Percent>,
    usable: KiloWattHours,
    total_capacity: KiloWattHours,
) {
    let values: &[(&str, String)] = &[
        (
            "rte_percent",
            rte_percent.map_or("unknown".to_string(), |v| format!("{v:.1}")),
        ),
        ("rte_usable_kwh", format!("{usable:.2}")),
        ("rte_total_capacity_kwh", format!("{total_capacity:.2}")),
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

pub async fn publish_battery_power(
    client: &AsyncClient,
    prefix: &str,
    charge: Watts,
    discharge: Watts,
) {
    let values: &[(&str, String)] = &[
        ("battery_charge_power", charge.to_string()),
        ("battery_discharge_power", discharge.to_string()),
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

pub async fn publish_status(client: &AsyncClient, prefix: &str, status: &str) {
    let topic = format!("{prefix}/controller_status");
    if let Err(e) = client
        .publish(&topic, QoS::AtMostOnce, false, status.as_bytes())
        .await
    {
        tracing::warn!("Failed to publish {topic}: {e}");
    }
}

pub async fn publish_battery_soc(client: &AsyncClient, prefix: &str, soc: Soc) {
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
