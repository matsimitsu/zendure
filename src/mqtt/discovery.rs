//! What Home Assistant is told, and the bytes it reads.

use crate::announce::Announcer;
use crate::models::{ControlDecision, CycleCounts};
use crate::publish::{Message, Publisher};
use crate::units::{DeciKelvin, KiloWattHours, Percent, Soc, Watts};

/// The Home Assistant device every sensor is attached to.
fn ha_device() -> serde_json::Value {
    serde_json::json!({
        "identifiers": ["zendure_controller"],
        "name": "Zendure Controller",
        "manufacturer": "Zendure",
        "model": "AC 2400+"
    })
}

/// One sensor, and the only place its id is spelled.
///
/// The id is needed twice by nature: once in the discovery document that tells
/// Home Assistant which topic to watch, and once in the publish that puts a
/// value on that topic. Spelling it twice made a typo silent in both
/// directions — an entity that never receives a value, or a value nothing
/// subscribes to — and the announce-once map keys on the same string, so a
/// mismatch would also quietly defeat that. With one constant per sensor a
/// mismatch does not compile.
struct Sensor {
    id: &'static str,
    name: &'static str,
    unit: &'static str,
    device_class: Option<&'static str>,
}

impl Sensor {
    const fn new(
        id: &'static str,
        name: &'static str,
        unit: &'static str,
        device_class: Option<&'static str>,
    ) -> Self {
        Sensor {
            id,
            name,
            unit,
            device_class,
        }
    }

    /// This sensor's discovery document, as the bytes Home Assistant expects.
    fn discovery(&self, prefix: &str) -> Message {
        sensor_discovery(prefix, self.id, self.name, self.unit, self.device_class)
    }
}

/// The document itself, for the sensors whose id is only known at runtime.
///
/// Pack temperatures cannot be table entries — the pack count comes from a poll
/// — so the table's rows delegate here rather than the dynamic case
/// duplicating the shape.
fn sensor_discovery(
    prefix: &str,
    id: &str,
    name: &str,
    unit: &str,
    device_class: Option<&str>,
) -> Message {
    let mut config = serde_json::json!({
        "name": name,
        "state_topic": format!("{prefix}/{id}"),
        "unique_id": format!("zendure_{id}"),
        "device": ha_device(),
    });

    if !unit.is_empty() {
        config["unit_of_measurement"] = serde_json::json!(unit);
    }
    if let Some(dc) = device_class {
        config["device_class"] = serde_json::json!(dc);
        config["state_class"] = serde_json::json!("measurement");
    }

    Message::discovery(
        format!("homeassistant/sensor/zendure_{id}/config"),
        config.to_string(),
    )
}

const POWER: Option<&str> = Some("power");
const ENERGY: Option<&str> = Some("energy");

const DECISION_MODE: Sensor = Sensor::new("decision_mode", "Battery Decision Mode", "", None);
const DECISION_POWER: Sensor = Sensor::new("decision_power", "Battery Decision Power", "W", POWER);
const DECISION_REASON: Sensor = Sensor::new("decision_reason", "Battery Decision Reason", "", None);
const DECISION_GRID_POWER: Sensor = Sensor::new(
    "decision_grid_power",
    "Grid Power (at decision)",
    "W",
    POWER,
);
const RTE_PERCENT: Sensor = Sensor::new("rte_percent", "Battery Round-Trip Efficiency", "%", None);
const RTE_USABLE_KWH: Sensor =
    Sensor::new("rte_usable_kwh", "Battery Usable Energy", "kWh", ENERGY);
const RTE_TOTAL_CAPACITY_KWH: Sensor = Sensor::new(
    "rte_total_capacity_kwh",
    "Battery Total Capacity",
    "kWh",
    ENERGY,
);
const ENCLOSURE_TEMP: Sensor = Sensor::new(
    "enclosure_temp",
    "Battery Enclosure Temperature",
    "°C",
    Some("temperature"),
);
const BATTERY_SOC: Sensor = Sensor::new(
    "battery_soc",
    "Battery State of Charge",
    "%",
    Some("battery"),
);
const BATTERY_CHARGE_POWER: Sensor = Sensor::new(
    "battery_charge_power",
    "Battery Actual Charge Power",
    "W",
    POWER,
);
const BATTERY_DISCHARGE_POWER: Sensor = Sensor::new(
    "battery_discharge_power",
    "Battery Actual Discharge Power",
    "W",
    POWER,
);
const CONTROLLER_STATUS: Sensor = Sensor::new("controller_status", "Controller Status", "", None);
const DAILY_CYCLES: Sensor =
    Sensor::new("daily_cycles", "Battery Daily Mode Transitions", "", None);
const DAILY_COOLDOWN_SUPPRESSIONS: Sensor = Sensor::new(
    "daily_cooldown_suppressions",
    "Battery Daily Cooldown Suppressions",
    "",
    None,
);

/// Everything announced on connect. Pack temperatures are not here: the pack
/// count is only known from a poll, so they are announced from the telemetry
/// path instead.
const SENSORS: &[Sensor] = &[
    DECISION_MODE,
    DECISION_POWER,
    DECISION_REASON,
    DECISION_GRID_POWER,
    RTE_PERCENT,
    RTE_USABLE_KWH,
    RTE_TOTAL_CAPACITY_KWH,
    ENCLOSURE_TEMP,
    BATTERY_SOC,
    BATTERY_CHARGE_POWER,
    BATTERY_DISCHARGE_POWER,
    CONTROLLER_STATUS,
    DAILY_CYCLES,
    DAILY_COOLDOWN_SUPPRESSIONS,
];

/// The one binary sensor. Its own type of discovery document, hence its own
/// constant rather than a row in the table above.
const SOC_CALIBRATING: &str = "soc_calibrating";

pub fn publish_ha_discovery(publisher: &dyn Publisher, announcer: &Announcer, prefix: &str) {
    for sensor in SENSORS {
        announcer.announce(publisher, sensor.id, || sensor.discovery(prefix));
    }

    announcer.announce(publisher, SOC_CALIBRATING, || {
        let config = serde_json::json!({
            "name": "Battery SOC Calibrating",
            "state_topic": format!("{prefix}/{SOC_CALIBRATING}"),
            "unique_id": format!("zendure_{SOC_CALIBRATING}"),
            "payload_on": "ON",
            "payload_off": "OFF",
            "device": ha_device(),
        });
        Message::discovery(
            format!("homeassistant/binary_sensor/zendure_{SOC_CALIBRATING}/config"),
            config.to_string(),
        )
    });
}

/// Publish one value per id, all under the same prefix.
fn publish_values(publisher: &dyn Publisher, prefix: &str, values: Vec<(&str, String)>) {
    for (id, value) in values {
        // The payload is moved, not cloned again: these run on the decision
        // path, up to a dozen times a poll.
        publisher.publish(Message::telemetry(format!("{prefix}/{id}"), value));
    }
}

pub fn publish_decision(publisher: &dyn Publisher, prefix: &str, decision: &ControlDecision) {
    publish_values(
        publisher,
        prefix,
        vec![
            (DECISION_MODE.id, decision.mode.to_string()),
            (DECISION_POWER.id, decision.power_watts.to_string()),
            (DECISION_REASON.id, decision.reason.clone()),
            (
                DECISION_GRID_POWER.id,
                format!("{:.0}", decision.grid_power),
            ),
        ],
    );
}

pub fn publish_cycle_counts(publisher: &dyn Publisher, prefix: &str, counts: &CycleCounts) {
    publish_values(
        publisher,
        prefix,
        vec![
            (DAILY_CYCLES.id, counts.daily_transitions.to_string()),
            (
                DAILY_COOLDOWN_SUPPRESSIONS.id,
                counts.daily_cooldown_suppressions.to_string(),
            ),
        ],
    );
}

pub fn publish_rte(
    publisher: &dyn Publisher,
    prefix: &str,
    rte_percent: Option<Percent>,
    usable: KiloWattHours,
    total_capacity: KiloWattHours,
) {
    publish_values(
        publisher,
        prefix,
        vec![
            (
                RTE_PERCENT.id,
                rte_percent.map_or("unknown".to_string(), |v| format!("{v:.1}")),
            ),
            (RTE_USABLE_KWH.id, format!("{usable:.2}")),
            (RTE_TOTAL_CAPACITY_KWH.id, format!("{total_capacity:.2}")),
        ],
    );
}

pub fn publish_soc_calibrating(publisher: &dyn Publisher, prefix: &str, calibrating: bool) {
    let value = if calibrating { "ON" } else { "OFF" };
    publish_values(
        publisher,
        prefix,
        vec![(SOC_CALIBRATING, value.to_string())],
    );
}

pub fn publish_battery_power(
    publisher: &dyn Publisher,
    prefix: &str,
    charge: Watts,
    discharge: Watts,
) {
    publish_values(
        publisher,
        prefix,
        vec![
            (BATTERY_CHARGE_POWER.id, charge.to_string()),
            (BATTERY_DISCHARGE_POWER.id, discharge.to_string()),
        ],
    );
}

pub fn publish_status(publisher: &dyn Publisher, prefix: &str, status: &str) {
    publish_values(
        publisher,
        prefix,
        vec![(CONTROLLER_STATUS.id, status.to_string())],
    );
}

pub fn publish_battery_soc(publisher: &dyn Publisher, prefix: &str, soc: Soc) {
    publish_values(publisher, prefix, vec![(BATTERY_SOC.id, soc.to_string())]);
}

/// One pack's temperature reading. A named pair rather than `(usize, u32)`,
/// which said neither what the index was counting nor what unit the number was
/// in.
pub struct PackTemperature {
    pub index: usize,
    pub temp: DeciKelvin,
}

pub fn publish_temperatures(
    publisher: &dyn Publisher,
    announcer: &Announcer,
    prefix: &str,
    enclosure_temp: Option<DeciKelvin>,
    pack_temps: &[PackTemperature],
) {
    // Per-pack sensors are announced from here rather than with the static list
    // because the pack count is only known from a poll. This is the one caller
    // that needs the announcer for a reason other than retrying.
    for pack in pack_temps {
        let idx = pack.index;
        let id = format!("pack{idx}_temp");
        announcer.announce(publisher, &id, || {
            let name = format!("Battery Pack {idx} Temperature");
            sensor_discovery(prefix, &id, &name, "°C", Some("temperature"))
        });

        let celsius = pack.temp.to_celsius();
        publish_values(
            publisher,
            prefix,
            vec![(id.as_str(), format!("{celsius:.1}"))],
        );
    }

    // Publish enclosure temperature state
    if let Some(raw_temp) = enclosure_temp {
        let celsius = raw_temp.to_celsius();
        publish_values(
            publisher,
            prefix,
            vec![(ENCLOSURE_TEMP.id, format!("{celsius:.1}"))],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ControlMode;
    use crate::publish::{Delivery, RecordingPublisher};
    use crate::units::Setpoint;
    use rumqttc::QoS;
    use std::collections::BTreeSet;

    fn telemetry(publisher: &RecordingPublisher) -> Vec<(String, String)> {
        publisher
            .sent()
            .into_iter()
            .filter(|m| m.delivery == Delivery::Telemetry)
            .map(|m| (m.topic, m.payload))
            .collect()
    }

    // --- what goes on the wire ------------------------------------------
    //
    // `mqtt.rs` had no tests at all before the publisher trait, because every
    // helper needed a live `AsyncClient` to say anything about. These pin the
    // bytes Home Assistant reads.

    #[test]
    fn a_decision_publishes_four_values() {
        let p = RecordingPublisher::new();
        let decision = ControlDecision {
            mode: ControlMode::Charge,
            power_watts: Setpoint::new(1200),
            ..ControlDecision::test_sample()
        };
        publish_decision(&p, "zendure", &decision);

        assert_eq!(
            telemetry(&p),
            vec![
                ("zendure/decision_mode".into(), "charge".into()),
                ("zendure/decision_power".into(), "1200".into()),
                ("zendure/decision_reason".into(), "Grid demand".into()),
                // Rounded to whole watts, and the meter reports fractions.
                ("zendure/decision_grid_power".into(), "150".into()),
            ],
        );
    }

    #[test]
    fn rte_says_unknown_rather_than_a_number_it_does_not_have() {
        let p = RecordingPublisher::new();
        publish_rte(
            &p,
            "zendure",
            None,
            KiloWattHours(1.234),
            KiloWattHours(3.84),
        );

        assert_eq!(p.payload("zendure/rte_percent").unwrap(), "unknown");
        // Two decimals for energy, one for the efficiency — the precision the
        // `Display` forwarding exists to preserve.
        assert_eq!(p.payload("zendure/rte_usable_kwh").unwrap(), "1.23");
        assert_eq!(p.payload("zendure/rte_total_capacity_kwh").unwrap(), "3.84");
    }

    #[test]
    fn rte_publishes_one_decimal_when_it_has_a_figure() {
        let p = RecordingPublisher::new();
        publish_rte(
            &p,
            "zendure",
            Some(Percent(85.23456789)),
            KiloWattHours(0.0),
            KiloWattHours(0.0),
        );

        assert_eq!(p.payload("zendure/rte_percent").unwrap(), "85.2");
    }

    #[test]
    fn status_publishes_the_string_it_was_given() {
        let p = RecordingPublisher::new();
        publish_status(&p, "zendure", "mqtt_timeout");
        assert_eq!(
            p.payload("zendure/controller_status").unwrap(),
            "mqtt_timeout"
        );
    }

    #[test]
    fn soc_publishes_whole_percent() {
        let p = RecordingPublisher::new();
        publish_battery_soc(&p, "zendure", Soc::new(81));
        assert_eq!(p.payload("zendure/battery_soc").unwrap(), "81");
    }

    /// A binary sensor, so the payload is HA's `ON`/`OFF`, not `true`/`false`.
    #[test]
    fn soc_calibrating_publishes_on_and_off() {
        let p = RecordingPublisher::new();
        publish_soc_calibrating(&p, "zendure", true);
        assert_eq!(p.payload("zendure/soc_calibrating").unwrap(), "ON");

        publish_soc_calibrating(&p, "zendure", false);
        assert_eq!(p.payload("zendure/soc_calibrating").unwrap(), "OFF");
    }

    #[test]
    fn battery_power_publishes_both_directions_separately() {
        let p = RecordingPublisher::new();
        publish_battery_power(&p, "zendure", Watts::from_device(1200), Watts::ZERO);
        assert_eq!(p.payload("zendure/battery_charge_power").unwrap(), "1200");
        assert_eq!(p.payload("zendure/battery_discharge_power").unwrap(), "0");
    }

    #[test]
    fn cycle_counts_publish_both_counters() {
        let p = RecordingPublisher::new();
        publish_cycle_counts(
            &p,
            "zendure",
            &CycleCounts {
                daily_transitions: 7,
                daily_cooldown_suppressions: 2,
            },
        );
        assert_eq!(p.payload("zendure/daily_cycles").unwrap(), "7");
        assert_eq!(
            p.payload("zendure/daily_cooldown_suppressions").unwrap(),
            "2",
        );
    }

    #[test]
    fn temperatures_convert_tenths_of_kelvin_to_one_decimal_of_celsius() {
        let p = RecordingPublisher::new();
        publish_temperatures(
            &p,
            &Announcer::new(),
            "zendure",
            Some(DeciKelvin(3001)),
            &[
                PackTemperature {
                    index: 0,
                    temp: DeciKelvin(2981),
                },
                PackTemperature {
                    index: 1,
                    temp: DeciKelvin(2995),
                },
            ],
        );

        assert_eq!(p.payload("zendure/enclosure_temp").unwrap(), "27.0");
        assert_eq!(p.payload("zendure/pack0_temp").unwrap(), "25.0");
        assert_eq!(p.payload("zendure/pack1_temp").unwrap(), "26.4");
    }

    #[test]
    fn discovery_documents_are_retained_and_telemetry_is_not() {
        let p = RecordingPublisher::new();
        publish_ha_discovery(&p, &Announcer::new(), "zendure");

        let sent = p.sent();
        // The flag itself, not just the variant: an announcement that stops
        // being retained vanishes from Home Assistant on the next broker
        // restart, which is the whole reason this is QoS 1 and retained.
        assert!(sent.iter().all(|m| m.delivery.retain()));
        assert!(sent.iter().all(|m| m.delivery.qos() == QoS::AtLeastOnce));
        assert!(
            sent.iter().all(|m| m.topic.starts_with("homeassistant/")),
            "discovery lives under the homeassistant prefix, not the device's",
        );

        let soc = sent
            .iter()
            .find(|m| m.topic == "homeassistant/sensor/zendure_battery_soc/config")
            .expect("the SOC sensor is announced");
        let doc: serde_json::Value = serde_json::from_str(&soc.payload).unwrap();
        assert_eq!(doc["state_topic"], "zendure/battery_soc");
        assert_eq!(doc["unique_id"], "zendure_battery_soc");
        assert_eq!(doc["device_class"], "battery");
        assert_eq!(doc["state_class"], "measurement");
    }

    /// Every announced sensor gets values, and every value has a sensor.
    ///
    /// The ids are constants now, so a typo will not compile — but a sensor
    /// added to the table and never published, or published and never
    /// announced, still compiles fine. Both are silent in production: an entity
    /// that sits at "unknown" for ever, or a topic nothing subscribes to.
    #[test]
    fn every_announced_sensor_is_published_and_the_reverse() {
        let announced = RecordingPublisher::new();
        publish_ha_discovery(&announced, &Announcer::new(), "zendure");

        let announced: BTreeSet<String> = announced
            .sent()
            .iter()
            .map(|m| {
                let doc: serde_json::Value = serde_json::from_str(&m.payload).unwrap();
                doc["state_topic"].as_str().unwrap().to_string()
            })
            .collect();

        // Every helper, with values chosen only to make them all fire.
        let p = RecordingPublisher::new();
        publish_decision(&p, "zendure", &ControlDecision::test_sample());
        publish_cycle_counts(
            &p,
            "zendure",
            &CycleCounts {
                daily_transitions: 0,
                daily_cooldown_suppressions: 0,
            },
        );
        publish_rte(&p, "zendure", None, KiloWattHours(0.0), KiloWattHours(0.0));
        publish_soc_calibrating(&p, "zendure", false);
        publish_battery_power(&p, "zendure", Watts::ZERO, Watts::ZERO);
        publish_status(&p, "zendure", "operational");
        publish_battery_soc(&p, "zendure", Soc::ZERO);
        // Enclosure only: pack sensors are dynamic and not in the static table.
        publish_temperatures(
            &p,
            &Announcer::new(),
            "zendure",
            Some(DeciKelvin(3001)),
            &[],
        );

        let published: BTreeSet<String> = p
            .sent()
            .iter()
            .filter(|m| m.delivery == Delivery::Telemetry)
            .map(|m| m.topic.clone())
            .collect();

        assert_eq!(announced, published);
    }
}
