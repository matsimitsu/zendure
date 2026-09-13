//! Where a published reading goes.
//!
//! `Publisher` is synchronous and infallible by construction: rumqttc's request
//! channel holds only 50 entries and is drained solely by the task polling the
//! eventloop, so an awaited `publish()` with no broker connected never returns
//! past ~50 queued messages — stalling the coordinator's `select!` with no
//! failsafe re-assertion, and the battery stuck on its last command for the
//! length of the outage.

use rumqttc::QoS;

/// How a message is delivered. QoS and retain always move together: telemetry
/// is a value the next reading supersedes (QoS 0, not retained); a discovery
/// document is retained for Home Assistant on reconnect (QoS 1, retained).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// A reading the next one supersedes. QoS 0, not retained.
    Telemetry,
    /// A Home Assistant discovery document. QoS 1, retained.
    Discovery,
}

impl Delivery {
    pub fn qos(self) -> QoS {
        match self {
            Delivery::Telemetry => QoS::AtMostOnce,
            Delivery::Discovery => QoS::AtLeastOnce,
        }
    }

    pub fn retain(self) -> bool {
        match self {
            Delivery::Telemetry => false,
            Delivery::Discovery => true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub topic: String,
    pub payload: String,
    pub delivery: Delivery,
}

impl Message {
    pub fn telemetry(topic: String, payload: String) -> Self {
        Message {
            topic,
            payload,
            delivery: Delivery::Telemetry,
        }
    }

    pub fn discovery(topic: String, payload: String) -> Self {
        Message {
            topic,
            payload,
            delivery: Delivery::Discovery,
        }
    }
}

/// Whether the sink took a message. Not an error: no caller in the decision
/// path could act on a missing broker. Only an announcement, which must repeat
/// until accepted, needs to check this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accepted {
    Queued,
    Dropped,
}

/// A sink for outgoing readings. `&self`, not `&mut self`: the coordinator
/// holds this immutably across a `select!`, so implementations keep their own
/// interior mutability. Synchronous and infallible — no future to await, no
/// error to handle.
pub trait Publisher: Send + Sync {
    fn publish(&self, message: Message) -> Accepted;
}

/// The brokerless sink used when `[mqtt]` is absent, so a laptop with no
/// broker can still run the whole coordinator loop. Returns `Accepted::Queued`,
/// never `Dropped`: `Announcer` marks an id announced only on `Queued`, so
/// `Dropped` here would make it re-offer every discovery document forever on a hot
/// loop.
pub struct NullPublisher;

impl Publisher for NullPublisher {
    fn publish(&self, _message: Message) -> Accepted {
        Accepted::Queued
    }
}

#[cfg(test)]
pub(crate) struct RecordingPublisher {
    sent: std::sync::Mutex<Vec<Message>>,
    /// Refuse everything, for testing the callers that must retry.
    /// `RecordingBattery::failing_at` is the same idea on the write path.
    refusing: bool,
}

#[cfg(test)]
impl RecordingPublisher {
    pub(crate) fn new() -> Self {
        RecordingPublisher {
            sent: std::sync::Mutex::new(Vec::new()),
            refusing: false,
        }
    }

    /// A sink with no room in it. Records the attempt, refuses the message.
    pub(crate) fn refusing() -> Self {
        RecordingPublisher {
            sent: std::sync::Mutex::new(Vec::new()),
            refusing: true,
        }
    }

    pub(crate) fn sent(&self) -> Vec<Message> {
        self.sent.lock().unwrap().clone()
    }

    /// The payload published to `topic`, for a test that cares about one value
    /// rather than the whole transcript.
    pub(crate) fn payload(&self, topic: &str) -> Option<String> {
        self.sent()
            .iter()
            .rev()
            .find(|m| m.topic == topic)
            .map(|m| m.payload.clone())
    }
}

#[cfg(test)]
impl Publisher for RecordingPublisher {
    fn publish(&self, message: Message) -> Accepted {
        self.sent.lock().unwrap().push(message);
        if self.refusing {
            Accepted::Dropped
        } else {
            Accepted::Queued
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pinned because it is a wire-format contract with Home Assistant: a
    /// discovery document that stops being retained disappears from HA on the
    /// next broker restart.
    #[test]
    fn delivery_maps_to_the_qos_and_retain_it_always_had() {
        assert_eq!(Delivery::Telemetry.qos(), QoS::AtMostOnce);
        assert!(!Delivery::Telemetry.retain());

        assert_eq!(Delivery::Discovery.qos(), QoS::AtLeastOnce);
    }

    /// The property `NullPublisher`'s doc comment argues at length: it must
    /// report `Queued`, never `Dropped`, or `Announcer` would rebuild and
    /// re-offer every discovery document on every poll, forever.
    #[test]
    fn the_null_publisher_reports_every_message_queued() {
        let publisher = NullPublisher;
        assert_eq!(
            publisher.publish(Message::telemetry("x".to_string(), "1".to_string())),
            Accepted::Queued,
        );
        assert_eq!(
            publisher.publish(Message::discovery("y".to_string(), "{}".to_string())),
            Accepted::Queued,
        );
        assert!(Delivery::Discovery.retain());
    }
}
