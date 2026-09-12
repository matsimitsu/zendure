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

/// A sink for outgoing readings.
///
/// `&self` rather than `&mut self` for the same reason `BatteryController` takes
/// it: the coordinator holds this immutably across a `select!` and the
/// implementation keeps its own interior mutability. No return value, because
/// there is nothing a caller in the decision path could usefully do about a
/// broker that is not there — the implementation counts what it drops and says
/// so, which is the honest version of handling it.
pub trait Publisher: Send + Sync {
    fn publish(&self, message: Message);
}

#[cfg(test)]
pub(crate) struct RecordingPublisher {
    sent: std::sync::Mutex<Vec<Message>>,
}

#[cfg(test)]
impl RecordingPublisher {
    pub(crate) fn new() -> Self {
        RecordingPublisher {
            sent: std::sync::Mutex::new(Vec::new()),
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
    fn publish(&self, message: Message) {
        self.sent.lock().unwrap().push(message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mapping the ten publish helpers used to spell out positionally at
    /// every call site. Pinned because it is a wire-format contract with Home
    /// Assistant: a discovery document that stops being retained disappears
    /// from HA on the next broker restart.
    #[test]
    fn delivery_maps_to_the_qos_and_retain_it_always_had() {
        assert_eq!(Delivery::Telemetry.qos(), QoS::AtMostOnce);
        assert!(!Delivery::Telemetry.retain());

        assert_eq!(Delivery::Discovery.qos(), QoS::AtLeastOnce);
        assert!(Delivery::Discovery.retain());
    }
}
