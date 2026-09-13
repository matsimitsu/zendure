//! Everything that talks to the broker.
//!
//! Three jobs that share a transport and little else, so they are three files.
//! [`publisher`] is the queue and the task that drains it; [`discovery`] is the
//! Home Assistant wire format; [`subscriber`] owns the eventloop, the
//! subscription and the meter feed. They were one module of a thousand lines,
//! which is how the wire format came to share a file with the backpressure
//! policy of a `select!` arm.

pub mod discovery;
pub mod publisher;
pub mod subscriber;

pub use discovery::{
    publish_battery_power, publish_battery_soc, publish_cycle_counts, publish_decision,
    publish_rte, publish_soc_calibrating, publish_status, publish_temperatures,
};
pub use publisher::{MqttPublisher, PublisherTask};
pub use subscriber::{MqttEvent, create_mqtt_client, run_subscriber};
