//! Publishing that cannot block the decision path.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rumqttc::AsyncClient;
use tokio::sync::mpsc;

use crate::backpressure::tally;
use crate::publish::{Accepted, Message, Publisher};
use crate::sync::guard;

/// How many messages may wait for the broker before we start dropping. Sized
/// against the steady-state rate (~12 messages/10s from polling, up to 7 more
/// per decision): over half a minute of backlog, long enough to ride out a
/// restart, short enough to report a dead broker within the minute it died. Larger than
/// rumqttc's own 50-slot request channel, since ours is the one allowed to fill.
const QUEUE_DEPTH: usize = 256;

/// Awaiting this is what drains whatever is still queued at shutdown.
pub type PublisherTask = tokio::task::JoinHandle<()>;

/// Publishes through a queue and a task so the decision path never waits on a broker.
/// Two queue depths: ours `try_send`s and drops when full, while the task safely
/// `await`s rumqttc's own bounded channel — a gone broker parks the task, and only our
/// queue absorbs the backlog before we drop and count.
/// Two counters: a path failing *every* message drains the queue faster than a healthy
/// one, so `dropped` never moves and total failure looks like a quiet night.
/// `try_send` drops the **newest** message, so on these latest-value topics a stall
/// keeps stale values and discards fresh ones.
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

    /// How many messages are waiting for the broker right now. Only our own
    /// queue — not rumqttc's ≤50-slot request channel, nor whatever its
    /// eventloop has moved to `pending` — but enough to say what a timed-out drain was
    /// up against.
    pub fn queued(&self) -> usize {
        guard(&self.tx)
            .as_ref()
            .map(|tx| tx.max_capacity() - tx.capacity())
            .unwrap_or(0)
    }

    /// Stop accepting messages and let the task finish what is already queued.
    pub fn close(&self) {
        *guard(&self.tx) = None;
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

#[cfg(test)]
mod tests {
    use super::*;
    use rumqttc::{EventLoop, MqttOptions, QoS};
    use std::time::Duration;
    use tokio::time::timeout;

    /// A client pointed at a port with nothing behind it; its eventloop is
    /// returned to the caller and never polled. It must be held, not dropped:
    /// dropping it closes rumqttc's request channel, making every send fail
    /// instantly instead of parking — a different condition than production.
    fn unreachable_client() -> (AsyncClient, EventLoop) {
        let opts = MqttOptions::new("zendure-test", "127.0.0.1", 1);
        AsyncClient::new(opts, 50)
    }

    /// Fixture validity, not a test of this crate: asserts that an awaited
    /// send on a bounded rumqttc channel nobody drains blocks forever. Every
    /// other test in this section is worthless if that doesn't hold, so it's kept
    /// deliberately at the cost of one runtime and a 50ms loop.
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

    /// The regression test for the production defect: same never-drained
    /// eventloop, but publishing through `MqttPublisher`. `publish` is
    /// synchronous so the compiler already guarantees it can't wait; this
    /// asserts the rest — it keeps accepting, finishes, and reports that it degraded
    /// rather than discarding quietly.
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

    /// A path failing every message drains the queue faster than a healthy
    /// one, so `dropped` never moves and total failure looks like a quiet
    /// night. Dropping the eventloop creates exactly that: every send errors
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

    /// A vanished sink must not be reported as backpressure: without separate
    /// refusal kinds, a dead delivery task — after which nothing is ever published
    /// again — would read in the log exactly like a slow broker.
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

    /// With the broker gone the delivery task parks on rumqttc's channel and
    /// never finishes, so an unconditional await would hang until systemd's
    /// `TimeoutStopSec` turns into a SIGKILL. The deadline lives in `run.rs`;
    /// this pins only the precondition — the task really does park with messages still
    /// queued.
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
            publisher.queued() > 0,
            "messages are still queued behind a broker that is not there",
        );
        publisher.close();
        assert!(
            timeout(Duration::from_millis(200), task).await.is_err(),
            "the task is parked, which is exactly why the drain has a deadline",
        );
    }
}
