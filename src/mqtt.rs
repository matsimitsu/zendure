use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rumqttc::{AsyncClient, Event, EventLoop, MqttOptions, Packet, QoS};
use tokio::sync::mpsc;

use crate::announce::Announcer;
use crate::backpressure::tally;
use crate::config::Config;
use crate::journal::Journal;
use crate::models::{ControlDecision, CycleCounts};
use crate::publish::{Accepted, Message, Publisher};
use crate::source::MeterObservation;
use crate::source::shelly::{self, SolarPhase};
use crate::sync::guard;
use crate::units::{KiloWattHours, Percent, Soc, Watts};

/// How many messages may be waiting for the broker before we start dropping.
///
/// Sized against the steady-state rate rather than a fixed budget, so a change
/// to the publish set does not silently invalidate the arithmetic: a poll sends
/// roughly a dozen messages every 10s and a decision up to 7 more, which puts
/// this at something over half a minute of backlog — long enough to ride out a
/// broker restart, short enough that a dead broker is reported in the same
/// minute it died. Deliberately larger than rumqttc's own 50-slot request
/// channel: ours is the one that is allowed to fill.
const QUEUE_DEPTH: usize = 256;

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

/// Awaiting this is what drains whatever is still queued at shutdown.
pub type PublisherTask = tokio::task::JoinHandle<()>;

/// Publishes through a queue and a task, so the decision path never waits on a
/// broker.
///
/// Two queue depths, deliberately. The control path `try_send`s into ours and
/// drops when it is full; the task then `await`s rumqttc's own bounded channel,
/// where awaiting is correct — when the broker is gone the task parks, our
/// queue absorbs the backlog, and only then do we drop and count. Doing the
/// non-blocking send against rumqttc's channel directly would work too, but it
/// would leave nothing to absorb a reconnect and no place to drain at shutdown.
///
/// Two counters, for the reason `Journal` has two: a delivery path that fails
/// *every* message drains the queue faster than a healthy one, so the queue
/// never fills, `dropped` never moves, and a silent total failure would look
/// exactly like a quiet night.
///
/// Known asymmetry, stated rather than hidden: `try_send` drops the **newest**
/// message, and these are latest-value topics, so a sustained stall keeps stale
/// values and discards fresh ones. Coalescing per topic in the task is the
/// principled fix; it is not here because drops should be rare and FIFO is the
/// idiom already in the tree.
pub struct MqttPublisher {
    /// `Option` so `close` can drop the last sender, which is what ends the
    /// task's `recv` loop and lets a bounded drain finish.
    tx: std::sync::Mutex<Option<mpsc::Sender<Message>>>,
    dropped: Arc<AtomicU64>,
    failed: Arc<AtomicU64>,
    /// Latched once the delivery task is found to be gone, so that discovery is
    /// reported once rather than on every publish for the life of the process.
    sink_gone: std::sync::atomic::AtomicBool,
}

impl MqttPublisher {
    /// The sink and the thing you await to drain it, returned together — the
    /// shape `Journal::open` already established, rather than hiding the task
    /// inside the handle where a caller cannot wait for it.
    pub fn open(client: AsyncClient) -> (Arc<Self>, PublisherTask) {
        let (tx, rx) = mpsc::channel::<Message>(QUEUE_DEPTH);
        let dropped = Arc::new(AtomicU64::new(0));
        let failed = Arc::new(AtomicU64::new(0));

        let task = tokio::spawn(deliver(client, rx, dropped.clone(), failed.clone()));

        let publisher = Arc::new(MqttPublisher {
            tx: std::sync::Mutex::new(Some(tx)),
            dropped,
            failed,
            sink_gone: std::sync::atomic::AtomicBool::new(false),
        });

        (publisher, task)
    }

    /// Stop accepting messages and let the task finish what is already queued,
    /// reporting how many that was so a drain that runs out of time can say
    /// what it left behind.
    pub fn close(&self) -> usize {
        guard(&self.tx)
            .take()
            .map(|tx| tx.max_capacity() - tx.capacity())
            .unwrap_or(0)
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    pub fn failed(&self) -> u64 {
        self.failed.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    fn sink_gone(&self) -> bool {
        self.sink_gone.load(Ordering::Relaxed)
    }
}

/// Why a message was not taken. Three causes that a single "dropped" hides, and
/// they want three different reactions from whoever reads the log.
enum Refused {
    /// The broker is slow or gone and our queue has filled. Backpressure.
    QueueFull,
    /// The receiver is gone while we still hold a sender: the delivery task
    /// ended early. Every publish from here on is discarded.
    SinkGone,
    /// `close` has been called. Expected, and not worth a word.
    ShuttingDown,
}

impl Publisher for MqttPublisher {
    fn publish(&self, message: Message) -> Accepted {
        let refused = match guard(&self.tx).as_ref() {
            Some(tx) => match tx.try_send(message) {
                Ok(()) => return Accepted::Queued,
                Err(mpsc::error::TrySendError::Full(_)) => Refused::QueueFull,
                Err(mpsc::error::TrySendError::Closed(_)) => Refused::SinkGone,
            },
            None => Refused::ShuttingDown,
        };

        match refused {
            Refused::QueueFull => tally(&self.dropped, |n| {
                tracing::warn!("MQTT queue full — {n} messages dropped so far")
            }),
            // Once, latched. Reporting this as backpressure would be a lie in
            // the one direction that costs an operator the most: the queue is
            // not full, the sink is gone, and nothing will ever be published
            // again.
            Refused::SinkGone => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                if !self.sink_gone.swap(true, Ordering::Relaxed) {
                    tracing::error!(
                        "MQTT delivery task has gone — every publish from here is discarded",
                    );
                }
            }
            Refused::ShuttingDown => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }

        Accepted::Dropped
    }
}

/// Moves queued messages to the broker, awaiting rumqttc's channel.
async fn deliver(
    client: AsyncClient,
    mut rx: mpsc::Receiver<Message>,
    dropped: Arc<AtomicU64>,
    failed: Arc<AtomicU64>,
) {
    while let Some(message) = rx.recv().await {
        let result = client
            .publish(
                &message.topic,
                message.delivery.qos(),
                message.delivery.retain(),
                message.payload.as_bytes(),
            )
            .await;

        if let Err(e) = result {
            tally(&failed, |n| {
                tracing::warn!(
                    "Failed to publish {} — {n} publishes failed so far: {e}",
                    message.topic,
                )
            });
        }
    }

    let dropped = dropped.load(Ordering::Relaxed);
    let failed = failed.load(Ordering::Relaxed);
    if dropped > 0 || failed > 0 {
        tracing::warn!(
            "MQTT publisher closing — {dropped} messages dropped, {failed} publishes failed",
        );
    }
}

/// Eight arguments, and the alternative is worse: bundling them into a struct
/// whose only purpose is to be destructured here would hide which of them the
/// Shelly adapter owns and which the broker does. The TOML work reshapes this.
#[allow(clippy::too_many_arguments)]
pub async fn run_subscriber(
    client: AsyncClient,
    mut eventloop: EventLoop,
    shelly_topic: String,
    solar_phase: SolarPhase,
    ha_prefix: String,
    publisher: Arc<dyn Publisher>,
    announcer: Arc<Announcer>,
    tx: mpsc::Sender<MqttEvent>,
    journal: Arc<Journal>,
) {
    // Whether this *connection* has been subscribed. Reset on every ConnAck and
    // retried after every event until it takes — see the loop's tail.
    let mut subscribed = false;

    loop {
        match eventloop.poll().await {
            Ok(Event::Incoming(Packet::ConnAck(_))) => {
                tracing::info!("MQTT connected, subscribing to {shelly_topic}");
                subscribed = false;
                // A broker that restarted may have lost its retained store.
                announcer.reset();
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
                // No point offering anything to a broker that is not there, and
                // `poll` frees no request slot on this path.
                continue;
            }
        }

        // Both of these are offered after *every* event, and both are
        // non-blocking, and that is the whole point.
        //
        // `subscribe().await` is a send on rumqttc's 50-slot request channel,
        // and `poll` — which this very task is inside — is the only thing that
        // drains it. After an outage that channel is full (`clean` moves the
        // backlog to `pending`, the delivery task immediately refills the
        // channel, and `poll` returns `Ok(ConnAck)` on reconnect *before* any
        // `select`), so awaiting the subscribe parked this task forever on a
        // queue only it could drain. No meter readings ever again, no exit, and
        // no restart — the controller would sit re-asserting failsafe idle for
        // good while looking perfectly healthy.
        //
        // `try_subscribe` cannot park. Every `poll` above drains one request,
        // so a slot frees within a few iterations and the retry lands. Setting
        // the channel capacity to 0, which rumqttc's own docs suggest, would be
        // worse here: at 0 the send blocks until `poll` receives it, and we are
        // inside `poll`'s caller, so it would deadlock on every connect.
        if !subscribed {
            match client.try_subscribe(&shelly_topic, QoS::AtMostOnce) {
                Ok(()) => subscribed = true,
                // Not a warning: a full channel right after a reconnect is the
                // expected case, and the next iteration retries.
                Err(e) => tracing::debug!("Subscribe to {shelly_topic} deferred: {e}"),
            }
        }
        publish_ha_discovery(&*publisher, &announcer, &ha_prefix);
    }
}

/// The Home Assistant device every sensor is attached to.
fn ha_device() -> serde_json::Value {
    serde_json::json!({
        "identifiers": ["zendure_controller"],
        "name": "Zendure Controller",
        "manufacturer": "Zendure",
        "model": "AC 2400+"
    })
}

/// One sensor's discovery document, as the bytes Home Assistant expects.
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

pub fn publish_ha_discovery(publisher: &dyn Publisher, announcer: &Announcer, prefix: &str) {
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
        announcer.announce(publisher, id, || {
            sensor_discovery(prefix, id, name, unit, *device_class)
        });
    }

    // Binary sensors
    let binary_config = || {
        serde_json::json!({
        "name": "Battery SOC Calibrating",
        "state_topic": format!("{prefix}/soc_calibrating"),
        "unique_id": "zendure_soc_calibrating",
        "payload_on": "ON",
        "payload_off": "OFF",
        "device": ha_device(),
        })
    };

    announcer.announce(publisher, "soc_calibrating", || {
        Message::discovery(
            "homeassistant/binary_sensor/zendure_soc_calibrating/config".to_string(),
            binary_config().to_string(),
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
            ("decision_mode", decision.mode.to_string()),
            ("decision_power", decision.power_watts.to_string()),
            ("decision_reason", decision.reason.clone()),
            ("decision_grid_power", format!("{:.0}", decision.grid_power)),
        ],
    );
}

pub fn publish_cycle_counts(publisher: &dyn Publisher, prefix: &str, counts: &CycleCounts) {
    publish_values(
        publisher,
        prefix,
        vec![
            ("daily_cycles", counts.daily_transitions.to_string()),
            (
                "daily_cooldown_suppressions",
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
                "rte_percent",
                rte_percent.map_or("unknown".to_string(), |v| format!("{v:.1}")),
            ),
            ("rte_usable_kwh", format!("{usable:.2}")),
            ("rte_total_capacity_kwh", format!("{total_capacity:.2}")),
        ],
    );
}

pub fn publish_soc_calibrating(publisher: &dyn Publisher, prefix: &str, calibrating: bool) {
    let value = if calibrating { "ON" } else { "OFF" };
    publish_values(
        publisher,
        prefix,
        vec![("soc_calibrating", value.to_string())],
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
            ("battery_charge_power", charge.to_string()),
            ("battery_discharge_power", discharge.to_string()),
        ],
    );
}

pub fn publish_status(publisher: &dyn Publisher, prefix: &str, status: &str) {
    publish_values(
        publisher,
        prefix,
        vec![("controller_status", status.to_string())],
    );
}

pub fn publish_battery_soc(publisher: &dyn Publisher, prefix: &str, soc: Soc) {
    publish_values(publisher, prefix, vec![("battery_soc", soc.to_string())]);
}

/// Convert a Zendure temperature (tenths of Kelvin) to degrees Celsius.
fn tenths_kelvin_to_celsius(value: u32) -> f64 {
    (value as f64 / 10.0) - 273.15
}

pub fn publish_temperatures(
    publisher: &dyn Publisher,
    announcer: &Announcer,
    prefix: &str,
    enclosure_temp: Option<u32>,
    pack_temps: &[(usize, u32)],
) {
    // Per-pack sensors are announced from here rather than with the static list
    // because the pack count is only known from a poll. This is the one caller
    // that needs the announcer for a reason other than retrying.
    for &(idx, raw_temp) in pack_temps {
        let id = format!("pack{idx}_temp");
        announcer.announce(publisher, &id, || {
            let name = format!("Battery Pack {idx} Temperature");
            sensor_discovery(prefix, &id, &name, "°C", Some("temperature"))
        });

        let celsius = tenths_kelvin_to_celsius(raw_temp);
        publish_values(
            publisher,
            prefix,
            vec![(id.as_str(), format!("{celsius:.1}"))],
        );
    }

    // Publish enclosure temperature state
    if let Some(raw_temp) = enclosure_temp {
        let celsius = tenths_kelvin_to_celsius(raw_temp);
        publish_values(
            publisher,
            prefix,
            vec![("enclosure_temp", format!("{celsius:.1}"))],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ControlMode;
    use crate::publish::{Delivery, RecordingPublisher};
    use crate::units::Setpoint;
    use std::time::Duration;
    use tokio::time::timeout;

    /// A client pointed at a port with nothing behind it, whose eventloop is
    /// returned to the caller and never polled.
    ///
    /// The eventloop must be *held*, not dropped: dropping it closes rumqttc's
    /// request channel, which makes every send fail instantly — a different
    /// condition entirely, and not the one that reached production.
    fn unreachable_client() -> (AsyncClient, EventLoop) {
        let opts = MqttOptions::new("zendure-test", "127.0.0.1", 1);
        AsyncClient::new(opts, 50)
    }

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
            Some(3001),
            &[(0, 2981), (1, 2995)],
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

    // --- the wedge ------------------------------------------------------

    /// Fixture validity, not a test of this crate.
    ///
    /// It asserts a property of rumqttc — that an awaited send on a bounded
    /// request channel nobody drains blocks forever — and it cannot fail
    /// because of anything in this repo. It is here because every other test in
    /// this section is worthless if that property does not hold: they would be
    /// proving the new code survives a condition the fixture never creates.
    /// Kept knowingly, at the cost of one runtime and a 50ms loop.
    #[tokio::test]
    async fn fixture_check_awaiting_a_publish_blocks_when_nobody_drains() {
        let (client, _eventloop) = unreachable_client();

        let mut accepted = 0;
        loop {
            match timeout(
                Duration::from_millis(50),
                client.publish("zendure/x", QoS::AtMostOnce, false, b"1".as_ref()),
            )
            .await
            {
                Ok(Ok(())) => {
                    accepted += 1;
                    assert!(
                        accepted < 500,
                        "the request channel is bounded at 50; it should have blocked by now",
                    );
                }
                Ok(Err(e)) => panic!("unexpected client error: {e}"),
                // The wedge.
                Err(_) => break,
            }
        }

        assert!(
            accepted > 0,
            "the first publishes should have been accepted"
        );
    }

    /// The regression test for the production defect.
    ///
    /// Same client, same never-drained eventloop, but publishing through
    /// `MqttPublisher`. `publish` is synchronous, so the compiler already
    /// guarantees the decision path cannot wait here — what this asserts is the
    /// rest of the contract: it keeps accepting, it finishes, and it *says* it
    /// degraded rather than discarding quietly.
    #[tokio::test]
    async fn publishing_never_blocks_when_the_broker_never_drains() {
        let (client, _eventloop) = unreachable_client();
        let (publisher, _task) = MqttPublisher::open(client);

        timeout(Duration::from_secs(5), async {
            for i in 0..2_000 {
                publisher.publish(Message::telemetry(
                    "zendure/decision_power".to_string(),
                    i.to_string(),
                ));
                // Let the delivery task run, so it parks on the broker rather
                // than the test finishing before it ever tried.
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("publishing cannot block");

        assert!(
            publisher.dropped() > 0,
            "a queue this far past its depth must have dropped and counted",
        );
    }

    /// Why there are two counters rather than one.
    ///
    /// A delivery path that fails *every* message drains the queue faster than
    /// a healthy one, so the queue never fills and `dropped` never moves. A
    /// total failure would look exactly like a quiet night. Dropping the
    /// eventloop is that condition: the client is dead and every send errors
    /// immediately instead of parking.
    #[tokio::test]
    async fn a_client_whose_eventloop_is_gone_counts_failures_not_drops() {
        let (client, eventloop) = unreachable_client();
        drop(eventloop);
        let (publisher, task) = MqttPublisher::open(client);

        for i in 0..20 {
            publisher.publish(Message::telemetry(
                "zendure/battery_soc".to_string(),
                i.to_string(),
            ));
        }
        publisher.close();
        timeout(Duration::from_secs(2), task)
            .await
            .expect("a dead client does not park, so the drain finishes")
            .expect("the task did not panic");

        assert!(publisher.failed() > 0, "every publish errored");
        assert_eq!(publisher.dropped(), 0, "the queue never filled");
    }

    /// A vanished sink must not be reported as backpressure.
    ///
    /// Both refusals used to fold into one counter and one `"MQTT queue full"`
    /// line, so a delivery task that had died — after which *nothing* is ever
    /// published again — read in the log exactly like a slow broker.
    #[tokio::test]
    async fn a_dead_delivery_task_is_not_reported_as_a_full_queue() {
        let (client, _eventloop) = unreachable_client();
        let (publisher, task) = MqttPublisher::open(client);

        task.abort();
        let _ = task.await;

        assert_eq!(
            publisher.publish(Message::telemetry(
                "zendure/battery_soc".to_string(),
                "81".to_string(),
            )),
            Accepted::Dropped,
        );
        assert!(publisher.sink_gone(), "the sink is gone, not merely full");
        assert_eq!(publisher.dropped(), 1);
    }

    /// A publish after `close` is expected, and must not be counted as though
    /// the broker had failed us.
    #[tokio::test]
    async fn a_publish_during_shutdown_is_not_reported_as_a_dead_sink() {
        let (client, _eventloop) = unreachable_client();
        let (publisher, _task) = MqttPublisher::open(client);
        publisher.close();

        publisher.publish(Message::telemetry(
            "zendure/battery_soc".to_string(),
            "81".to_string(),
        ));

        assert!(!publisher.sink_gone());
    }

    #[tokio::test]
    async fn closing_ends_the_delivery_task_once_the_queue_is_empty() {
        let (client, _eventloop) = unreachable_client();
        let (publisher, task) = MqttPublisher::open(client);

        publisher.publish(Message::telemetry(
            "zendure/battery_soc".to_string(),
            "81".to_string(),
        ));

        publisher.close();
        timeout(Duration::from_secs(2), task)
            .await
            .expect("the drain finished")
            .expect("the task did not panic");
    }

    /// Why the drain needs a deadline at all.
    ///
    /// With the broker gone the delivery task parks on rumqttc's channel and
    /// cannot finish, so a shutdown that awaited it unconditionally would hang
    /// until systemd turned `TimeoutStopSec` into a SIGKILL. The deadline
    /// itself lives in `run.rs` and is exercised by `run`'s own drain test;
    /// what this pins is the precondition — that the task really does park,
    /// with messages still queued behind it.
    #[tokio::test]
    async fn a_parked_delivery_task_leaves_messages_queued() {
        let (client, _eventloop) = unreachable_client();
        let (publisher, task) = MqttPublisher::open(client);

        for i in 0..300 {
            publisher.publish(Message::telemetry(
                "zendure/decision_power".to_string(),
                i.to_string(),
            ));
            tokio::task::yield_now().await;
        }

        assert!(
            publisher.close() > 0,
            "messages are still queued behind a broker that is not there",
        );
        assert!(
            timeout(Duration::from_millis(200), task).await.is_err(),
            "the task is parked, which is exactly why the drain has a deadline",
        );
    }
}
