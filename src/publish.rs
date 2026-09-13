//! Where a published reading goes.
//!
//! The trait is **synchronous and infallible by construction**, and that is the
//! whole point of this module. Every publish used to be an `async fn` awaited
//! inside the coordinator's `select!`, against a client whose request channel
//! holds 50 entries and is drained only by the task polling the MQTT eventloop.
//! With no broker that task never drains it, so once ~50 publishes had queued,
//! `publish().await` never returned: no polls, no decisions, and **no failsafe
//! re-assertion** — the battery held its last command for the length of the
//! outage, which is the exact state the failsafe exists to prevent.
//!
//! The old shape made "do not block the decision path" a rule a reviewer had to
//! enforce, and the 50-slot channel quietly enforced the opposite. A sink with
//! no future to await and no error to handle cannot be misused that way: the
//! decision path structurally cannot wait on a broker.

use rumqttc::QoS;

/// How a message is delivered, as one role rather than two loose booleans.
///
/// QoS and retain only ever move together here — telemetry is a value the next
/// reading supersedes, a discovery document is a retained description Home
/// Assistant needs on reconnect — so they travel as one word. Passing
/// `(QoS::AtLeastOnce, true)` positionally at three call sites was the same
/// class of uncheckable pair that `ControlPath` replaced for the two status
/// strings.
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

/// Whether the sink took a message.
///
/// Not an error: there is nothing a caller in the decision path could usefully
/// do about a broker that is not there, and no caller is obliged to look. It is
/// a report, and exactly one kind of caller needs it — an announcement, which
/// must be repeated until it lands, where a reading is superseded by the next
/// one a second later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accepted {
    Queued,
    Dropped,
}

/// A sink for outgoing readings.
///
/// `&self` rather than `&mut self` for the same reason `BatteryController` takes
/// it: the coordinator holds this immutably across a `select!` and the
/// implementation keeps its own interior mutability. Synchronous and
/// infallible: no future to await, no error to handle.
pub trait Publisher: Send + Sync {
    fn publish(&self, message: Message) -> Accepted;
}

/// The brokerless sink: `run.rs` reaches for this when `[mqtt]` is absent from
/// configuration, so a laptop with no broker can still run the whole
/// coordinator loop.
///
/// **Returns `Accepted::Queued`, never `Accepted::Dropped`.** That looks
/// backwards for something that discards every message, but it is the whole
/// reason this type is safe to substitute for a real sink: `Announcer::announce`
/// (`announce.rs`) only marks an id announced once the sink reports `Queued`,
/// specifically so a refused document is retried rather than silently given up
/// on. Answering `Dropped` here would not mean "nothing was sent" — it would
/// make every discovery document, every poll, look like a broker that is
/// permanently full, and `Announcer` would rebuild and re-offer all of them,
/// forever, on a hot loop that goes nowhere. `Queued` says "this sink took the
/// message," which is the one thing actually true about a sink whose entire
/// job is to take a message and do nothing with it. Do not "simplify" this to
/// `Dropped` because nothing is being delivered — that reasoning is exactly
/// the bug this comment exists to prevent.
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
