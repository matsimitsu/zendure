//! The scan cycle: who may ask a source for a reading, what a round costs the
//! requests inside it, and the inbox a tick reads.
//!
//! What a round costs the requests inside it is a property of the cycle, not
//! of one vendor's wire format, so the meter and the battery derive their
//! HTTP timeouts from the same place.
//!
//! One sampler task per source, and that task is the only holder of a slot's
//! write half — [`Slot::deliver`] and [`Slot::fail`] are private to this
//! module. The loop holds the [`Inbox`] and can only take from it, so "a tick
//! made a dead source look alive" is not something a caller can express.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use chrono_tz::Tz;
use tokio::sync::watch;
use tokio::time::Instant;

use crate::backpressure;
use crate::clock::Clock;
use crate::device::{BatteryReading, PollError, RawCapture};
use crate::journal::Journal;
use crate::source::MeterSample;
use crate::sync::guard;
use crate::world::DeviceId;

/// What [`request_timeout`] derives within: the floor keeps a request from
/// being abandoned faster than either box answers, and the ceiling bounds how
/// long a sampler waits on an unresponsive source before it may try again.
pub(crate) const MIN_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
pub(crate) const MAX_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Half the period the caller scans at, bounded — and raised to
/// [`MIN_REQUEST_TIMEOUT`] only where that still leaves it under the period. A
/// request still in flight when the next round goes out overlaps it, which the
/// Zendure firmware answers with delayed replies and `ECONNRESET`.
pub(crate) fn request_timeout(scan_period: Duration) -> Duration {
    let half = scan_period / 2;

    if MIN_REQUEST_TIMEOUT < scan_period {
        half.clamp(MIN_REQUEST_TIMEOUT, MAX_REQUEST_TIMEOUT)
    } else {
        half
    }
}

/// The HTTP client a polled source reaches its device with, timed out by
/// [`request_timeout`] so every adapter abandons a request on the same rule.
pub(crate) fn http_client(scan_period: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(request_timeout(scan_period))
        .build()
        .expect("failed to create HTTP client")
}

/// Which round every source is being asked for. A sample is stamped with the
/// round it answered, so a tick can tell one taken for it from one that
/// arrived too late for the round before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TickSeq(u64);

impl TickSeq {
    pub const FIRST: TickSeq = TickSeq(0);

    pub fn next(self) -> Self {
        TickSeq(self.0.wrapping_add(1))
    }
}

/// Consecutive failed reads of one source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Failures(u32);

impl Failures {
    pub fn get(self) -> u32 {
        self.0
    }
}

/// A reading that can hand back the bytes it was parsed from. The journal
/// captures those before anything interprets them.
pub trait Captured {
    fn raw(&self) -> Option<&RawCapture>;
}

/// Anything the scan cycle can ask for a reading. Mirrors
/// [`crate::device::BatteryMonitor`]'s shape, and is not `dyn`-safe for the
/// same reason — these futures are `impl Future + Send`.
pub trait Sampler: Send + Sync + 'static {
    type Reading: Captured + Send + 'static;

    fn id(&self) -> &str;

    fn sample(&self) -> impl Future<Output = Result<Self::Reading, PollError>> + Send;
}

/// What a tick finds in a slot.
#[derive(Debug)]
pub enum Delivery<T> {
    /// A sample taken for the round this tick asked about.
    Fresh(Clock, T),
    /// Nothing taken for this round, and the source is still inside the
    /// window the loop may go blind for.
    Quiet,
    /// Unanswered for longer than the loop may be blind, with what it had
    /// failed consecutively by then.
    Down(Failures),
}

impl<T> Delivery<T> {
    /// What a source that has stopped answering had failed consecutively —
    /// zero for one that hung, which never reports a failure at all.
    pub fn down(&self) -> Option<Failures> {
        match self {
            Delivery::Down(failures) => Some(*failures),
            Delivery::Fresh(_, _) | Delivery::Quiet => None,
        }
    }
}

/// What one source has most recently produced, and its standing. Ages on
/// [`Instant`], a monotonic clock — never on the [`Clock`] stamped on each
/// delivery, which is wall-clock and journalled. An NTP step moves the
/// latter without moving the former, so a jump can neither idle a healthy
/// source nor keep a dead one looking alive until the wall clock catches up.
pub struct Slot<T> {
    state: Mutex<SlotState<T>>,
    blind_window: Duration,
}

struct SlotState<T> {
    pending: Option<(TickSeq, Clock, T)>,
    failures: Failures,
    /// The last good sample's monotonic instant, or the inbox's opening
    /// instant while there has been none — so a source that never comes up
    /// still ages out.
    last_good: Instant,
}

impl<T> Slot<T> {
    fn new(blind_window: Duration, opened_at: Instant) -> Self {
        Slot {
            state: Mutex::new(SlotState {
                pending: None,
                failures: Failures(0),
                last_good: opened_at,
            }),
            blind_window,
        }
    }

    /// Returns what the source had failed consecutively before this sample,
    /// which is the one count an operator is ever told. `at` is the wall
    /// clock this delivery is journalled with; `now` is the monotonic
    /// instant it landed at, which is what staleness is measured against —
    /// the two are captured together at the edge (`sample_loop`) and diverge
    /// only when the wall clock has just stepped.
    fn deliver(&self, round: TickSeq, at: Clock, now: Instant, reading: T) -> Failures {
        let mut state = guard(&self.state);
        state.pending = Some((round, at, reading));
        state.last_good = now;
        std::mem::replace(&mut state.failures, Failures(0))
    }

    fn fail(&self) -> Failures {
        let mut state = guard(&self.state);
        state.failures = Failures(state.failures.0.saturating_add(1));
        state.failures
    }

    /// What this tick may read. Only a sample stamped with `round` is `Fresh`,
    /// and only once: one taken for an earlier round would date the decision's
    /// inputs differently from each other, and `RteTracker::record` integrates
    /// between calls, so re-recording one counts the same joules twice.
    pub fn take(&self, round: TickSeq, now: Instant) -> Delivery<T> {
        let mut state = guard(&self.state);

        // Age, not a failure count: a source that hangs never reports a
        // failure, so a count would stay at zero through an outage of any
        // length. `now` is monotonic, so this is blind to whatever the wall
        // clock is doing.
        if now.duration_since(state.last_good) > self.blind_window {
            return Delivery::Down(state.failures);
        }

        match state.pending.take() {
            Some((stamp, at, reading)) if stamp == round => Delivery::Fresh(at, reading),
            _ => Delivery::Quiet,
        }
    }
}

/// Every source's most recent sample, as one tick reads them.
pub struct Inbox {
    meter: Arc<Slot<MeterSample>>,
    devices: BTreeMap<DeviceId, Arc<Slot<BatteryReading>>>,
}

impl Inbox {
    /// One slot per device the registry holds, so a tick that reads every
    /// slot has read every box it is about to command.
    pub fn new(
        devices: impl IntoIterator<Item = DeviceId>,
        opened_at: Instant,
        blind_window: Duration,
    ) -> Self {
        Inbox {
            meter: Arc::new(Slot::new(blind_window, opened_at)),
            devices: devices
                .into_iter()
                .map(|id| (id, Arc::new(Slot::new(blind_window, opened_at))))
                .collect(),
        }
    }

    pub fn meter_slot(&self) -> Arc<Slot<MeterSample>> {
        self.meter.clone()
    }

    pub fn device_slot(&self, id: &DeviceId) -> Option<Arc<Slot<BatteryReading>>> {
        self.devices.get(id).cloned()
    }

    pub fn take_meter(&self, round: TickSeq, now: Instant) -> Delivery<MeterSample> {
        self.meter.take(round, now)
    }

    /// Every device slot, in id order, so a tick folds two batteries the same
    /// way twice running whichever answered first.
    pub fn take_devices(
        &self,
        round: TickSeq,
        now: Instant,
    ) -> Vec<(DeviceId, Delivery<BatteryReading>)> {
        self.devices
            .iter()
            .map(|(id, slot)| (id.clone(), slot.take(round, now)))
            .collect()
    }
}

/// Reads `source` once per requested round, for as long as rounds are asked
/// for. The only writer of `slot`.
pub async fn sample_loop<S: Sampler>(
    source: Arc<S>,
    slot: Arc<Slot<S::Reading>>,
    mut requests: watch::Receiver<TickSeq>,
    journal: Arc<Journal>,
    timezone: Tz,
) {
    while requests.changed().await.is_ok() {
        // Read before the request, not after: the round a sample answers is
        // the one that asked for it, and rounds that pass while it is in
        // flight coalesce into the next `changed()`.
        let round = *requests.borrow_and_update();

        match source.sample().await {
            Ok(reading) => {
                journal.capture(reading.raw());
                if slot.deliver(round, Clock::now(timezone), Instant::now(), reading) > Failures(0)
                {
                    tracing::info!("{} is answering again", source.id());
                }
            }
            Err(e) => {
                journal.capture(e.raw.as_ref());
                let failures = slot.fail();
                backpressure::report(u64::from(failures.get()), |n| {
                    tracing::warn!("Failed to read {} ({n} in a row): {}", source.id(), e.error);
                });
            }
        }
    }
}

#[cfg(test)]
#[path = "scan_tests.rs"]
mod tests;
