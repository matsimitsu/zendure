//! Everything that talks to the broker.
//!
//! Three jobs that share a transport and little else, so they are three files.
//! [`publisher`] is the queue and the task that drains it; [`discovery`] is the
//! Home Assistant wire format; [`connection`] owns the broker connection and
//! the eventloop that carries the other two onto the socket.

pub mod connection;
pub mod discovery;
pub mod publisher;

pub use connection::{create_mqtt_client, run_connection};
pub use discovery::{
    publish_battery_power, publish_battery_soc, publish_cycle_counts, publish_decision,
    publish_rte, publish_soc_calibrating, publish_status, publish_temperatures,
};
pub use publisher::{MqttPublisher, PublisherTask};
