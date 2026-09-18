//! The MQTT connection: the eventloop, the subscription, and the meter feed.

use std::sync::Arc;
use std::time::Duration;

use rumqttc::{AsyncClient, Event, EventLoop, MqttOptions, Packet, QoS};
use tokio::sync::mpsc;

use crate::announce::Announcer;
use crate::config::MqttConfig;
use crate::journal::Journal;
use crate::publish::Publisher;
use crate::source::MeterObservation;
use crate::source::shelly::{self, SolarPhase};

use super::discovery::publish_ha_discovery;

#[derive(Debug, Clone)]
pub enum MqttEvent {
    /// Already normalized, not the meter's own JSON. The undecoded payload is
    /// captured verbatim by the raw log one line before it is parsed, so
    /// pushing the DTO down the channel as well would buy nothing — and would
    /// cost the coordinator loop its ignorance of what a Shelly is.
    Meter(MeterObservation),
}

pub fn create_mqtt_client(mqtt: &MqttConfig) -> (AsyncClient, EventLoop) {
    let mut opts = MqttOptions::new(&mqtt.client_id, &mqtt.host, mqtt.port);
    opts.set_keep_alive(Duration::from_secs(30));
    if let (Some(user), Some(pass)) = (&mqtt.username, &mqtt.password) {
        opts.set_credentials(user, pass);
    }
    AsyncClient::new(opts, 50)
}

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

        // Both offered after every event, non-blocking. Awaiting `subscribe()`
        // sends on rumqttc's 50-slot request channel, drained only by this
        // task's own `poll`, so after an outage it parks this task forever.
        // `try_subscribe` can't park; rumqttc's own capacity-0 advice would deadlock
        // every connect instead, since we're inside poll's caller.
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
