//! The broker connection and its eventloop.
//!
//! rumqttc only moves queued publishes onto the socket while something drives
//! its eventloop, so this task runs for the publisher's sake even though
//! nothing incoming is subscribed to.

use std::sync::Arc;
use std::time::Duration;

use rumqttc::{AsyncClient, Event, EventLoop, MqttOptions, Packet};

use crate::announce::Announcer;
use crate::config::MqttConfig;
use crate::publish::Publisher;

use super::discovery::publish_ha_discovery;

pub fn create_mqtt_client(mqtt: &MqttConfig) -> (AsyncClient, EventLoop) {
    let mut opts = MqttOptions::new(&mqtt.client_id, &mqtt.host, mqtt.port);
    opts.set_keep_alive(Duration::from_secs(30));
    if let (Some(user), Some(pass)) = (&mqtt.username, &mqtt.password) {
        opts.set_credentials(user, pass);
    }
    AsyncClient::new(opts, 50)
}

pub async fn run_connection(
    mut eventloop: EventLoop,
    ha_prefix: String,
    publisher: Arc<dyn Publisher>,
    announcer: Arc<Announcer>,
) {
    loop {
        match eventloop.poll().await {
            Ok(Event::Incoming(Packet::ConnAck(_))) => {
                tracing::info!("MQTT connected");
                // A broker that restarted may have lost its retained store.
                announcer.reset();
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

        publish_ha_discovery(&*publisher, &announcer, &ha_prefix);
    }
}
