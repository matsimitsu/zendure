//! What Home Assistant is told, and the bytes it reads.

use crate::announce::Announcer;
use crate::charger::ChargerState;
use crate::charger::state::{CableState, ChargePointStatus};
use crate::models::{ControlDecision, CycleCounts};
use crate::publish::{Message, Publisher};
use crate::units::{DeciKelvin, KiloWattHours, PackTemperature, Percent, Soc, Watts};

/// The Home Assistant device every sensor is attached to.
fn ha_device() -> serde_json::Value {
    serde_json::json!({
        "identifiers": ["zendure_controller"],
        "name": "Zendure Controller",
        "manufacturer": "Zendure",
        "model": "AC 2400+"
    })
}

/// One sensor, and the only place its id is spelled: needed both in the
/// discovery document (which topic to watch) and the publish (which topic
/// gets the value), and the announce-once map keys on the same string — a
/// mismatch between them would be silent. One constant per sensor makes a mismatch fail
/// to compile instead.
struct Sensor {
    id: &'static str,
    name: &'static str,
    unit: &'static str,
    device_class: Option<&'static str>,
    state_class: Option<&'static str>,
    /// The closed set of values an `enum` sensor can show.
    options: &'static [&'static str],
}

impl Sensor {
    /// A sensor with a device class is a `measurement` unless told otherwise.
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
            state_class: default_state_class(device_class),
            options: &[],
        }
    }

    const fn with_state_class(self, state_class: Option<&'static str>) -> Self {
        Sensor {
            state_class,
            ..self
        }
    }

    const fn with_options(self, options: &'static [&'static str]) -> Self {
        Sensor { options, ..self }
    }

    /// This sensor's discovery document, as the bytes Home Assistant expects.
    fn discovery(&self, prefix: &str) -> Message {
        self.discovery_on(prefix, ha_device())
    }

    fn discovery_on(&self, prefix: &str, device: serde_json::Value) -> Message {
        let mut config = sensor_config(
            prefix,
            self.id,
            self.name,
            self.unit,
            self.device_class,
            self.state_class,
            device,
        );
        if !self.options.is_empty() {
            config["options"] = serde_json::json!(self.options);
        }
        sensor_message(self.id, &config)
    }
}

const MEASUREMENT: &str = "measurement";

const fn default_state_class(device_class: Option<&str>) -> Option<&'static str> {
    if device_class.is_some() {
        Some(MEASUREMENT)
    } else {
        None
    }
}
const TOTAL_INCREASING: &str = "total_increasing";

/// The document itself, for sensors whose id is only known at runtime (pack
/// temperatures: the pack count comes from a poll). The table's static rows delegate
/// here rather than duplicating the shape.
fn sensor_discovery(
    prefix: &str,
    id: &str,
    name: &str,
    unit: &str,
    device_class: Option<&str>,
) -> Message {
    let state_class = default_state_class(device_class);
    let config = sensor_config(
        prefix,
        id,
        name,
        unit,
        device_class,
        state_class,
        ha_device(),
    );
    sensor_message(id, &config)
}

fn sensor_config(
    prefix: &str,
    id: &str,
    name: &str,
    unit: &str,
    device_class: Option<&str>,
    state_class: Option<&str>,
    device: serde_json::Value,
) -> serde_json::Value {
    let mut config = serde_json::json!({
        "name": name,
        "state_topic": format!("{prefix}/{id}"),
        "unique_id": format!("zendure_{id}"),
        "device": device,
    });

    if !unit.is_empty() {
        config["unit_of_measurement"] = serde_json::json!(unit);
    }
    if let Some(dc) = device_class {
        config["device_class"] = serde_json::json!(dc);
    }
    if let Some(sc) = state_class {
        config["state_class"] = serde_json::json!(sc);
    }
    config
}

fn sensor_message(id: &str, config: &serde_json::Value) -> Message {
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

/// A Home Assistant binary sensor: its own type of discovery document, hence
/// not a row in the `Sensor` tables.
struct BinarySensor {
    id: &'static str,
    name: &'static str,
}

impl BinarySensor {
    fn discovery_on(&self, prefix: &str, device: serde_json::Value) -> Message {
        let id = self.id;
        let config = serde_json::json!({
            "name": self.name,
            "state_topic": format!("{prefix}/{id}"),
            "unique_id": format!("zendure_{id}"),
            "payload_on": "ON",
            "payload_off": "OFF",
            "device": device,
        });
        Message::discovery(
            format!("homeassistant/binary_sensor/zendure_{id}/config"),
            config.to_string(),
        )
    }

    /// Home Assistant's payload for a binary sensor, not `true`/`false`.
    fn payload(on: bool) -> String {
        if on { "ON" } else { "OFF" }.to_string()
    }
}

const SOC_CALIBRATING: BinarySensor = BinarySensor {
    id: "soc_calibrating",
    name: "Battery SOC Calibrating",
};

/// The car's own HA device — separate from `ha_device()`'s "Zendure
/// Controller", since this reading describes a different physical thing
/// (the car, via `crate::car_battery`) that happens to be announced over the
/// same MQTT connection.
fn car_ha_device() -> serde_json::Value {
    serde_json::json!({
        "identifiers": ["zendure_car_battery"],
        "name": "Car Battery (via Zendure Controller)",
        "manufacturer": "Volkswagen",
    })
}

const CAR_BATTERY_SOC: Sensor = Sensor::new(
    "car_battery_soc",
    "Car Battery State of Charge",
    "%",
    Some("battery"),
);

pub fn publish_ha_discovery(publisher: &dyn Publisher, announcer: &Announcer, prefix: &str) {
    for sensor in SENSORS {
        announcer.announce(publisher, sensor.id, || sensor.discovery(prefix));
    }

    announcer.announce(publisher, SOC_CALIBRATING.id, || {
        SOC_CALIBRATING.discovery_on(prefix, ha_device())
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
    publish_values(
        publisher,
        prefix,
        vec![(SOC_CALIBRATING.id, BinarySensor::payload(calibrating))],
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

/// Announced (once per connection — `Announcer::announce` is idempotent)
/// and published together, from the car-battery poller's own schedule
/// (`crate::car_battery::run_car_battery_poller`) rather than from
/// `publish_ha_discovery`'s on-connect table: that poller runs independently
/// of the main decision loop's ConnAck-triggered announce cycle, the same
/// reason `publish_temperatures` announces per-pack sensors from here rather
/// than the static list.
pub fn publish_car_battery_soc(
    publisher: &dyn Publisher,
    announcer: &Announcer,
    prefix: &str,
    soc: Soc,
) {
    announcer.announce(publisher, CAR_BATTERY_SOC.id, || {
        let config = serde_json::json!({
            "name": CAR_BATTERY_SOC.name,
            "state_topic": format!("{prefix}/{}", CAR_BATTERY_SOC.id),
            "unique_id": format!("zendure_{}", CAR_BATTERY_SOC.id),
            "device": car_ha_device(),
            "unit_of_measurement": CAR_BATTERY_SOC.unit,
            "device_class": "battery",
            "state_class": "measurement",
        });
        Message::discovery(
            format!("homeassistant/sensor/zendure_{}/config", CAR_BATTERY_SOC.id),
            config.to_string(),
        )
    });
    publish_values(
        publisher,
        prefix,
        vec![(CAR_BATTERY_SOC.id, soc.to_string())],
    );
}

/// The EV charger's own HA device, apart from the stationary battery's, for
/// the reason `car_ha_device` is.
fn charger_ha_device() -> serde_json::Value {
    serde_json::json!({
        "identifiers": ["zendure_ev_charger"],
        "name": "EV Charger (via Zendure Controller)",
    })
}

const CHARGER_STATUS: Sensor =
    Sensor::new("ev_charger_status", "EV Charger Status", "", Some("enum"))
        .with_state_class(None)
        .with_options(ChargePointStatus::LABELS);
const CHARGER_CABLE: Sensor = Sensor::new("ev_charger_cable", "EV Charger Cable", "", Some("enum"))
    .with_state_class(None)
    .with_options(CableState::LABELS);
const CHARGER_POWER: Sensor = Sensor::new("ev_charger_power", "EV Charger Power", "W", POWER);
const CHARGER_CURRENTS: [Sensor; 3] = [
    Sensor::new(
        "ev_charger_current_l1",
        "EV Charger Current L1",
        "A",
        Some("current"),
    ),
    Sensor::new(
        "ev_charger_current_l2",
        "EV Charger Current L2",
        "A",
        Some("current"),
    ),
    Sensor::new(
        "ev_charger_current_l3",
        "EV Charger Current L3",
        "A",
        Some("current"),
    ),
];
const CHARGER_VOLTAGES: [Sensor; 3] = [
    Sensor::new(
        "ev_charger_voltage_l1",
        "EV Charger Voltage L1",
        "V",
        Some("voltage"),
    ),
    Sensor::new(
        "ev_charger_voltage_l2",
        "EV Charger Voltage L2",
        "V",
        Some("voltage"),
    ),
    Sensor::new(
        "ev_charger_voltage_l3",
        "EV Charger Voltage L3",
        "V",
        Some("voltage"),
    ),
];
const CHARGER_SESSION_ENERGY: Sensor = Sensor::new(
    "ev_charger_session_energy",
    "EV Charger Session Energy",
    "kWh",
    ENERGY,
)
.with_state_class(Some(TOTAL_INCREASING));
const CHARGER_SESSION_DURATION: Sensor = Sensor::new(
    "ev_charger_session_duration",
    "EV Charger Session Duration",
    "s",
    Some("duration"),
)
.with_state_class(None);
const CHARGER_PHASES: Sensor = Sensor::new("ev_charger_phases", "EV Charger Phases", "", None);

const CHARGER_DISCHARGE_HOLD: BinarySensor = BinarySensor {
    id: "ev_charger_discharge_hold",
    name: "EV Charger Discharge Hold",
};

/// Announced and published together from the scan round that read the
/// charger, as `publish_car_battery_soc` is from its poller: the charger is
/// optional, so its documents are not in `publish_ha_discovery`'s on-connect
/// table. `discharge_hold` is `controller::car_charging`'s answer, so the
/// binary sensor says what the controller did rather than re-deriving it.
pub fn publish_charger(
    publisher: &dyn Publisher,
    announcer: &Announcer,
    prefix: &str,
    state: &ChargerState,
    discharge_hold: bool,
) {
    let sensors = [
        CHARGER_STATUS,
        CHARGER_CABLE,
        CHARGER_POWER,
        CHARGER_SESSION_ENERGY,
        CHARGER_SESSION_DURATION,
        CHARGER_PHASES,
    ]
    .into_iter()
    .chain(CHARGER_CURRENTS)
    .chain(CHARGER_VOLTAGES);
    for sensor in sensors {
        announcer.announce(publisher, sensor.id, || {
            sensor.discovery_on(prefix, charger_ha_device())
        });
    }
    announcer.announce(publisher, CHARGER_DISCHARGE_HOLD.id, || {
        CHARGER_DISCHARGE_HOLD.discovery_on(prefix, charger_ha_device())
    });

    let mut values = vec![
        (CHARGER_STATUS.id, state.status.label().to_string()),
        (CHARGER_CABLE.id, state.cable.label().to_string()),
        (CHARGER_POWER.id, state.power.to_string()),
        (
            CHARGER_SESSION_ENERGY.id,
            or_unknown(state.session_energy.map(|wh| format!("{:.3}", wh.to_kwh()))),
        ),
        (
            CHARGER_SESSION_DURATION.id,
            or_unknown(state.session_duration.map(|d| d.as_secs().to_string())),
        ),
        (
            CHARGER_PHASES.id,
            or_unknown(state.phases.map(|p| p.count().to_string())),
        ),
    ];
    for (sensor, current) in CHARGER_CURRENTS.iter().zip(state.currents) {
        values.push((sensor.id, format!("{:.2}", current.as_amps())));
    }
    for (sensor, voltage) in CHARGER_VOLTAGES.iter().zip(state.voltages) {
        values.push((sensor.id, voltage.to_string()));
    }
    values.push((
        CHARGER_DISCHARGE_HOLD.id,
        BinarySensor::payload(discharge_hold),
    ));
    publish_values(publisher, prefix, values);
}

/// Home Assistant's MQTT sensor reads the payload `None` as unknown; any
/// other non-numeric text on a numeric sensor is logged there as an error.
fn or_unknown(value: Option<String>) -> String {
    value.unwrap_or_else(|| "None".to_string())
}

/// The hold's value alone, for a charger that has stopped answering: the
/// guard fails open, so the sensor must not stay ON for a car nobody can see.
pub fn publish_charger_discharge_hold(publisher: &dyn Publisher, prefix: &str, hold: bool) {
    publish_values(
        publisher,
        prefix,
        vec![(CHARGER_DISCHARGE_HOLD.id, BinarySensor::payload(hold))],
    );
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
    // These pin the bytes Home Assistant reads.

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

    #[test]
    fn car_battery_soc_announces_under_its_own_device_and_publishes_the_value() {
        let p = RecordingPublisher::new();
        let announcer = Announcer::new();
        publish_car_battery_soc(&p, &announcer, "zendure", Soc::new(62));

        assert_eq!(p.payload("zendure/car_battery_soc").unwrap(), "62");

        let sent = p.sent();
        let doc = sent
            .iter()
            .find(|m| m.topic == "homeassistant/sensor/zendure_car_battery_soc/config")
            .expect("the car battery SOC sensor is announced");
        let config: serde_json::Value = serde_json::from_str(&doc.payload).unwrap();
        assert_eq!(config["state_topic"], "zendure/car_battery_soc");
        assert_eq!(
            config["device"]["identifiers"][0], "zendure_car_battery",
            "the car is a separate HA device from the stationary battery",
        );
    }

    #[test]
    fn car_battery_soc_is_announced_once_per_connection() {
        let p = RecordingPublisher::new();
        let announcer = Announcer::new();
        publish_car_battery_soc(&p, &announcer, "zendure", Soc::new(62));
        publish_car_battery_soc(&p, &announcer, "zendure", Soc::new(63));

        let discovery_docs = p
            .sent()
            .iter()
            .filter(|m| m.topic.starts_with("homeassistant/"))
            .count();
        assert_eq!(
            discovery_docs, 1,
            "the second call re-offers, not re-announces"
        );
    }

    fn config_of(p: &RecordingPublisher, topic: &str) -> serde_json::Value {
        let sent = p.sent();
        let doc = sent
            .iter()
            .find(|m| m.topic == topic)
            .unwrap_or_else(|| panic!("{topic} is announced"));
        serde_json::from_str(&doc.payload).unwrap()
    }

    #[test]
    fn charger_sensors_announce_under_their_own_device_with_their_classes() {
        let p = RecordingPublisher::new();
        let announcer = Announcer::new();
        publish_charger(
            &p,
            &announcer,
            "zendure",
            &ChargerState::test_sample(),
            false,
        );

        let sensor = |id: &str| config_of(&p, &format!("homeassistant/sensor/zendure_{id}/config"));

        let energy = sensor("ev_charger_session_energy");
        assert_eq!(energy["device"]["identifiers"][0], "zendure_ev_charger");
        assert_eq!(energy["device_class"], "energy");
        assert_eq!(energy["state_class"], "total_increasing");

        let duration = sensor("ev_charger_session_duration");
        assert_eq!(duration["device_class"], "duration");
        assert_eq!(duration["unit_of_measurement"], "s");

        let status = sensor("ev_charger_status");
        assert_eq!(status["device_class"], "enum");
        assert!(status.get("state_class").is_none());
        assert!(
            status["options"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("suspended_ev"))
        );

        assert_eq!(sensor("ev_charger_current_l3")["device_class"], "current");
        assert_eq!(sensor("ev_charger_voltage_l2")["unit_of_measurement"], "V");
        assert_eq!(sensor("ev_charger_power")["state_class"], "measurement");

        let hold = config_of(
            &p,
            "homeassistant/binary_sensor/zendure_ev_charger_discharge_hold/config",
        );
        assert_eq!(hold["device"]["identifiers"][0], "zendure_ev_charger");
    }

    #[test]
    fn charger_values_are_published_in_the_units_the_sensors_announce() {
        let p = RecordingPublisher::new();
        publish_charger(
            &p,
            &Announcer::new(),
            "zendure",
            &ChargerState::test_sample(),
            true,
        );

        let value = |id: &str| p.payload(&format!("zendure/{id}")).unwrap();
        assert_eq!(value("ev_charger_status"), "charging");
        assert_eq!(value("ev_charger_cable"), "vehicle_locked");
        assert_eq!(value("ev_charger_power"), "3650");
        assert_eq!(value("ev_charger_current_l1"), "15.90");
        assert_eq!(value("ev_charger_voltage_l3"), "233");
        assert_eq!(value("ev_charger_session_energy"), "4.200");
        assert_eq!(value("ev_charger_session_duration"), "4140");
        assert_eq!(value("ev_charger_phases"), "1");
        assert_eq!(value("ev_charger_discharge_hold"), "ON");
    }

    #[test]
    fn charger_fields_the_firmware_did_not_answer_publish_as_unknown() {
        let p = RecordingPublisher::new();
        let state = ChargerState {
            phases: None,
            session_energy: None,
            session_duration: None,
            ..ChargerState::test_sample()
        };
        publish_charger(&p, &Announcer::new(), "zendure", &state, false);

        let value = |id: &str| p.payload(&format!("zendure/{id}")).unwrap();
        assert_eq!(value("ev_charger_session_energy"), "None");
        assert_eq!(value("ev_charger_session_duration"), "None");
        assert_eq!(value("ev_charger_phases"), "None");
        assert_eq!(value("ev_charger_power"), "3650");
    }

    #[test]
    fn charger_is_announced_once_per_connection_and_again_after_a_reset() {
        let p = RecordingPublisher::new();
        let announcer = Announcer::new();
        let state = ChargerState::test_sample();
        let docs = |p: &RecordingPublisher| {
            p.sent()
                .iter()
                .filter(|m| m.topic.starts_with("homeassistant/"))
                .count()
        };

        publish_charger(&p, &announcer, "zendure", &state, false);
        let first = docs(&p);
        assert_eq!(first, 13, "twelve sensors and the hold");
        publish_charger(&p, &announcer, "zendure", &state, false);
        assert_eq!(docs(&p), first);

        announcer.reset();
        publish_charger(&p, &announcer, "zendure", &state, false);
        assert_eq!(docs(&p), 2 * first);
    }

    #[test]
    fn existing_sensors_keep_their_state_class() {
        let power = config_of_message(&DECISION_POWER.discovery("zendure"));
        assert_eq!(power["state_class"], "measurement");
        let mode = config_of_message(&DECISION_MODE.discovery("zendure"));
        assert!(mode.get("state_class").is_none());
    }

    fn config_of_message(message: &Message) -> serde_json::Value {
        serde_json::from_str(&message.payload).unwrap()
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

    /// Every announced sensor gets values, and every value has a sensor. Ids
    /// are constants, so a typo won't compile — but a sensor added to the
    /// table and never published, or published and never announced, still compiles
    /// fine: an entity stuck at "unknown" forever, or a topic nothing subscribes to.
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
