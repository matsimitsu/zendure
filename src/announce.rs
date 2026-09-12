//! What Home Assistant has already been told about, on this connection.
//!
//! Home Assistant learns about a sensor from a retained discovery document, and
//! needs it again whenever the broker's retained store is lost. So the rule is
//! "announce until it sticks, once per connection" — and that is a fact about
//! Home Assistant, not about MQTT.
//!
//! It lived inside the publisher first, which was wrong in three visible ways:
//! a sink documented as taking messages silently discarded some of them; the
//! subscriber had to name the concrete publisher type just to reach the reset;
//! and the test double, having no such behaviour, stopped resembling the thing
//! it doubled. Keeping it here leaves the publisher a queue and two counters.
//!
//! Tracking *ids* rather than payloads is what makes the retry cheap: the
//! caller hands over a closure, so a document that is already announced is
//! never built. Announcing by payload meant `publish_temperatures` serialising
//! a document per pack per poll for the sink to throw away.

use std::collections::HashSet;

use crate::publish::{Accepted, Message, Publisher};
use crate::sync::guard;

#[derive(Default)]
pub struct Announcer {
    announced: std::sync::Mutex<HashSet<String>>,
}

impl Announcer {
    pub fn new() -> Self {
        Announcer::default()
    }

    /// Forget everything, because the broker may have forgotten it too.
    ///
    /// Called on every ConnAck: a broker that restarted may have lost its
    /// retained store, and a broker we reconnected to may not be the same
    /// broker. Re-announcing costs a few retained publishes; failing to
    /// re-announce means every entity silently disappears from Home Assistant
    /// until someone restarts the controller.
    pub fn reset(&self) {
        guard(&self.announced).clear();
    }

    /// Announce `id` unless it is already announced on this connection.
    ///
    /// Marked on acceptance, never on attempt. A document the sink refused has
    /// not reached the broker, and marking it here would suppress every retry
    /// for the life of the connection — the sensor missing from Home Assistant
    /// with nothing in the log to say why. Because callers re-offer on every
    /// poll and every MQTT event, a refused document simply lands on the next
    /// one.
    pub fn announce(&self, publisher: &dyn Publisher, id: &str, build: impl FnOnce() -> Message) {
        if guard(&self.announced).contains(id) {
            return;
        }
        if publisher.publish(build()) == Accepted::Queued {
            guard(&self.announced).insert(id.to_string());
        }
    }

    #[cfg(test)]
    pub(crate) fn count(&self) -> usize {
        guard(&self.announced).len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::publish::RecordingPublisher;

    fn doc(id: &str) -> Message {
        Message::discovery(
            format!("homeassistant/sensor/{id}/config"),
            "{}".to_string(),
        )
    }

    #[test]
    fn an_id_is_announced_once_per_connection() {
        let p = RecordingPublisher::new();
        let a = Announcer::new();

        for _ in 0..5 {
            a.announce(&p, "battery_soc", || doc("battery_soc"));
        }

        assert_eq!(p.sent().len(), 1);
    }

    #[test]
    fn a_reset_announces_everything_again() {
        let p = RecordingPublisher::new();
        let a = Announcer::new();

        a.announce(&p, "battery_soc", || doc("battery_soc"));
        a.reset();
        a.announce(&p, "battery_soc", || doc("battery_soc"));

        assert_eq!(p.sent().len(), 2);
        assert_eq!(a.count(), 1, "the second one is announced again, not twice");
    }

    /// The document is not built unless it is going to be sent.
    ///
    /// Announcing by payload instead of by id meant `publish_temperatures`
    /// serialising a JSON document per pack on every poll purely so the sink
    /// could compare it and throw it away.
    #[test]
    fn an_already_announced_id_does_not_build_its_document() {
        let p = RecordingPublisher::new();
        let a = Announcer::new();
        let mut built = 0;

        for _ in 0..5 {
            a.announce(&p, "battery_soc", || {
                built += 1;
                doc("battery_soc")
            });
        }

        assert_eq!(built, 1);
    }

    /// A document the sink refused has not reached the broker.
    ///
    /// Marking on attempt would suppress every retry for the life of the
    /// connection, which is a sensor missing from Home Assistant with nothing
    /// in the log to say why.
    #[test]
    fn a_refused_document_is_offered_again() {
        let p = RecordingPublisher::refusing();
        let a = Announcer::new();

        a.announce(&p, "battery_soc", || doc("battery_soc"));
        a.announce(&p, "battery_soc", || doc("battery_soc"));

        assert_eq!(p.sent().len(), 2, "refused is not announced");
        assert_eq!(a.count(), 0);
    }
}
