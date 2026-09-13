//! What Home Assistant has already been told about, on this connection.
//!
//! Home Assistant needs a retained discovery document again whenever the
//! broker's retained store is lost, so the rule is "announce until it sticks,
//! once per connection" — a fact about Home Assistant, not MQTT. Tracking ids
//! rather than payloads means a caller's closure is never built once its id is already
//! announced.

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

    /// Forget everything, since the broker may have too. Called on every
    /// ConnAck: a restarted or swapped broker may have lost its retained
    /// store. Re-announcing costs a few retained publishes; skipping it means every
    /// entity silently disappears from Home Assistant until a restart.
    pub fn reset(&self) {
        guard(&self.announced).clear();
    }

    /// Announce `id` unless already announced on this connection. Marked on
    /// acceptance, never on attempt: a refused document hasn't reached the
    /// broker, and marking it here would silently suppress every retry for
    /// the connection's life. Callers re-offer on every poll and MQTT event, so a
    /// refused document lands on the next one.
    pub fn announce(&self, publisher: &dyn Publisher, id: &str, build: impl FnOnce() -> Message) {
        if guard(&self.announced).contains(id) {
            return;
        }
        // The lock is dropped between check and insert, so a `reset` from the
        // subscriber task can land in between and be undone by the insert below —
        // benign, since the message is already in the publisher's own queue
        // (survives reconnect) or replayed from rumqttc's `pending` list.
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

    /// A document the sink refused has not reached the broker; marking on
    /// attempt would suppress every retry for the connection's life, leaving a sensor
    /// missing from Home Assistant with nothing in the log to say why.
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
