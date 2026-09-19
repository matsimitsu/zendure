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
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use chrono_tz::Tz;
use tokio::sync::watch;

use crate::backpressure;
use crate::clock::Clock;
use crate::device::{BatteryReading, PollError, RawCapture};
use crate::journal::Journal;
use crate::source::MeterSample;
use crate::sync::guard;
use crate::units::{Elapsed, Timestamp};
use crate::world::DeviceId;

/// What [`request_timeout`] derives within: the ceiling bounds how long one
/// unresponsive device can hold the loop, and the floor stays under the
/// shortest period configuration allows, so a request is abandoned before the
/// next round is due.
pub(crate) const MIN_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
pub(crate) const MAX_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Half the period the caller scans at, bounded: a request still in flight
/// when the next round goes out overlaps it, which the Zendure firmware
/// answers with delayed replies and `ECONNRESET`.
pub(crate) fn request_timeout(scan_period: Duration) -> Duration {
    (scan_period / 2).clamp(MIN_REQUEST_TIMEOUT, MAX_REQUEST_TIMEOUT)
}

/// Which round every source is being asked for. Only ever compared for
/// change, so rounds a busy source slept through coalesce into one request.
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

/// How many consecutive failures a source may have before the loop stops
/// trusting it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FailureTolerance(u32);

impl FailureTolerance {
    /// As many rounds as fit in the window the loop may go blind for. Never
    /// zero: at a period longer than that window a single failed read would
    /// otherwise stand the whole fleet down.
    pub fn over(blind_window: Duration, tick: Duration) -> Self {
        let rounds = blind_window.as_millis() / tick.as_millis().max(1);
        FailureTolerance(u32::try_from(rounds).unwrap_or(u32::MAX).max(1))
    }

    pub fn get(self) -> u32 {
        self.0
    }

    fn exceeded_by(self, failures: Failures) -> bool {
        failures.0 > self.0
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

    /// Consecutive failures this source may have before the loop stops
    /// trusting it.
    fn tolerance(&self, loop_default: FailureTolerance) -> FailureTolerance {
        loop_default
    }
}

/// What a tick finds in a slot.
#[derive(Debug)]
pub enum Delivery<T> {
    /// A sample taken since the last read.
    Fresh(Clock, T),
    /// Nothing new, and the source is still within tolerance.
    Quiet,
    /// Failed more often than its tolerance allows, or has gone unanswered
    /// for longer than the loop may be blind.
    Down(Failures),
    /// Never sampled.
    Cold,
}

impl<T> Delivery<T> {
    pub fn is_down(&self) -> bool {
        matches!(self, Delivery::Down(_))
    }
}

/// What one source has most recently produced, and its standing.
pub struct Slot<T> {
    state: Mutex<SlotState<T>>,
    tolerance: FailureTolerance,
    blind_window: Elapsed,
}

struct SlotState<T> {
    pending: Option<(Clock, T)>,
    failures: Failures,
    /// The last good sample, or the inbox's opening instant while there has
    /// been none — so a source that never comes up still ages out.
    last_good: Timestamp,
    answered: bool,
}

impl<T> Slot<T> {
    fn new(tolerance: FailureTolerance, blind_window: Duration, opened_at: Timestamp) -> Self {
        Slot {
            state: Mutex::new(SlotState {
                pending: None,
                failures: Failures(0),
                last_good: opened_at,
                answered: false,
            }),
            tolerance,
            blind_window: Elapsed::of(blind_window),
        }
    }

    fn deliver(&self, at: Clock, reading: T) {
        let mut state = guard(&self.state);
        state.pending = Some((at, reading));
        state.failures = Failures(0);
        state.last_good = at.now;
        state.answered = true;
    }

    fn fail(&self) {
        let mut state = guard(&self.state);
        state.failures = Failures(state.failures.0.saturating_add(1));
    }

    /// What this tick may read. `Fresh` is handed out once: a sample an
    /// earlier tick already decided on would date the decision's inputs
    /// differently from each other, and `RteTracker::record` integrates
    /// between calls, so re-recording one counts the same joules twice.
    pub fn take(&self, now: Timestamp) -> Delivery<T> {
        let mut state = guard(&self.state);

        // Failures alone has a hole: a source that hangs never reports one,
        // so the count would stay at zero through an outage of any length.
        if self.tolerance.exceeded_by(state.failures) || now - state.last_good > self.blind_window {
            return Delivery::Down(state.failures);
        }

        match state.pending.take() {
            Some((at, reading)) => Delivery::Fresh(at, reading),
            None if state.answered => Delivery::Quiet,
            None => Delivery::Cold,
        }
    }
}

/// Every source's most recent sample, as one tick reads them.
pub struct Inbox {
    meter: Arc<Slot<MeterSample>>,
    devices: BTreeMap<DeviceId, Arc<Slot<BatteryReading>>>,
}

impl Inbox {
    pub fn new(
        meter: FailureTolerance,
        devices: impl IntoIterator<Item = (DeviceId, FailureTolerance)>,
        opened_at: Timestamp,
        blind_window: Duration,
    ) -> Self {
        Inbox {
            meter: Arc::new(Slot::new(meter, blind_window, opened_at)),
            devices: devices
                .into_iter()
                .map(|(id, tolerance)| {
                    (id, Arc::new(Slot::new(tolerance, blind_window, opened_at)))
                })
                .collect(),
        }
    }

    pub fn meter_slot(&self) -> Arc<Slot<MeterSample>> {
        self.meter.clone()
    }

    pub fn device_slot(&self, id: &DeviceId) -> Option<Arc<Slot<BatteryReading>>> {
        self.devices.get(id).cloned()
    }

    pub fn take_meter(&self, now: Timestamp) -> Delivery<MeterSample> {
        self.meter.take(now)
    }

    /// Every device slot, in id order, so a tick folds two batteries the same
    /// way twice running whichever answered first.
    pub fn take_devices(&self, now: Timestamp) -> Vec<(DeviceId, Delivery<BatteryReading>)> {
        self.devices
            .iter()
            .map(|(id, slot)| (id.clone(), slot.take(now)))
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
    let failures = AtomicU64::new(0);

    while requests.changed().await.is_ok() {
        match source.sample().await {
            Ok(reading) => {
                if let Some(raw) = reading.raw() {
                    journal.raw(raw.kind, &raw.body);
                }
                if failures.swap(0, Ordering::Relaxed) > 0 {
                    tracing::info!("{} is answering again", source.id());
                }
                slot.deliver(Clock::now(timezone), reading);
            }
            Err(e) => {
                if let Some(raw) = &e.raw {
                    journal.raw(raw.kind, &raw.body);
                }
                slot.fail();
                backpressure::tally(&failures, |n| {
                    tracing::warn!("Failed to read {} ({n} in a row): {}", source.id(), e.error);
                });
            }
        }

        // A sample that outlived its round answers the next request, not the
        // rounds it slept through.
        requests.borrow_and_update();
    }
}

#[cfg(test)]
#[path = "scan_tests.rs"]
mod tests;
