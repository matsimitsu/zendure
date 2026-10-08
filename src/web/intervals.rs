//! Energy flows and SOC per fifteen minutes: the ring the flows chart reads.
//!
//! [`IntervalHistory`] keeps its own [`World`] rather than reading the
//! engine's, because one fold serves the live loop, the startup seed and a
//! past day read back from the journal, and the last two replay history the
//! engine's world has already moved past.

use std::collections::{BTreeMap, BTreeSet};
use std::marker::PhantomData;
use std::time::Duration;

use chrono::NaiveDate;
use chrono_tz::Tz;

use crate::clock::local_day_bounds;
use crate::device::PackStatus;
use crate::event::Event;
use crate::journal::read::{ReadError, read_events_in_range, read_raw_in_range};
use crate::units::{
    BatteryPower, Elapsed, GridPower, Soc, SolarPower, Timestamp, WattHours, Watts,
};
use crate::world::{DeviceId, World};
use crate::zendure::{POLL_CAPTURE, packs_in_capture};

use super::pack_intervals::{PackInterval, PackIntervals, PackKey, PackTrace};
use super::state::{Plottable, seed_from};

/// A [`Plottable`] that can be rebuilt from a mean of its scalars, so an
/// average stays the role it was taken over (`RUST-2`).
pub(crate) trait Averaged: Plottable {
    fn from_mean(mean: f64) -> Self;
}

impl Averaged for SolarPower {
    fn from_mean(mean: f64) -> Self {
        SolarPower::new(mean)
    }
}

impl Averaged for GridPower {
    fn from_mean(mean: f64) -> Self {
        GridPower(mean)
    }
}

/// Rounded: the mean of whole watts is a display figure, not a setpoint.
impl Averaged for Watts {
    fn from_mean(mean: f64) -> Self {
        Watts::rounded(mean)
    }
}

impl Averaged for BatteryPower {
    fn from_mean(mean: f64) -> Self {
        BatteryPower::rounded(mean)
    }
}

/// A running mean of one role's samples.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Mean<T> {
    sum: f64,
    count: u32,
    role: PhantomData<T>,
}

impl<T> Default for Mean<T> {
    fn default() -> Self {
        Mean {
            sum: 0.0,
            count: 0,
            role: PhantomData,
        }
    }
}

impl<T: Averaged> Mean<T> {
    pub(crate) fn add(&mut self, value: T) {
        self.sum += value.plot_value();
        self.count += 1;
    }

    /// `None` until something has been added.
    pub(crate) fn get(&self) -> Option<T> {
        (self.count > 0).then(|| T::from_mean(self.sum / f64::from(self.count)))
    }

    /// The mean of whichever `values` are present.
    pub(crate) fn of(values: impl IntoIterator<Item = Option<T>>) -> Option<T> {
        let mut mean = Mean::default();
        values.into_iter().flatten().for_each(|v| mean.add(v));
        mean.get()
    }
}

/// The width of one [`IntervalHistory`] bucket.
pub(crate) const INTERVAL: Duration = Duration::from_secs(15 * 60);

pub(crate) const HOUR: Duration = Duration::from_secs(60 * 60);

/// 100 rather than 96 so a 25-hour DST day fits whole.
const INTERVAL_RING: usize = 100;

/// The rolling window [`IntervalHistory::last_24h`] reads.
const ROLLING_DAY: Duration = Duration::from_secs(24 * 60 * 60);

/// Fifteen-minute intervals since the unix epoch. Absolute rather than counted
/// from local midnight, so a DST change neither repeats nor skips an index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IntervalIndex(i64);

impl IntervalIndex {
    fn width() -> Elapsed {
        Elapsed::of(INTERVAL)
    }

    pub fn containing(at: Timestamp) -> Self {
        IntervalIndex(at.as_millis().div_euclid(Self::width().as_millis()))
    }

    pub fn start(self) -> Timestamp {
        Timestamp::from_millis(self.0.saturating_mul(Self::width().as_millis()))
    }

    /// The index `count` intervals later; a negative count goes back.
    pub fn offset(self, count: i64) -> Self {
        IntervalIndex(self.0.saturating_add(count))
    }

    /// The first interval of the hour this one falls in.
    pub fn hour_start(self) -> Self {
        let per_hour = intervals_in(HOUR);
        IntervalIndex(self.0.div_euclid(per_hour) * per_hour)
    }

    fn slot(self) -> usize {
        // `rem_euclid` by the ring's length is always a valid slot.
        self.0.rem_euclid(INTERVAL_RING as i64) as usize
    }
}

/// What `power`, averaged over `index`'s interval, delivers in the part of it
/// that had elapsed by `now`: the bucket in progress has averaged only that.
pub(crate) fn interval_energy(power: Watts, index: IntervalIndex, now: Timestamp) -> WattHours {
    let elapsed = Duration::try_from_secs_f64((now - index.start()).as_secs_f64())
        .unwrap_or(Duration::ZERO)
        .min(INTERVAL);
    WattHours::integrate(power, power, elapsed)
}

/// How many whole [`INTERVAL`]s fit in `span`.
fn intervals_in(span: Duration) -> i64 {
    Elapsed::of(span).as_millis() / IntervalIndex::width().as_millis()
}

/// What one interval measured. A field is `None` where no event carrying it
/// landed in the interval.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct IntervalAverages {
    pub solar: Option<SolarPower>,
    pub home: Option<Watts>,
    pub grid: Option<GridPower>,
    pub battery: Option<BatteryPower>,
    /// The last reading rather than a mean: SOC is a level, not a flow.
    pub soc: Option<Soc>,
}

/// One interval on a chart's axis, listed whether or not anything was
/// recorded in it, so a gap renders as a gap.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IntervalSlot {
    pub index: IntervalIndex,
    pub averages: IntervalAverages,
}

/// One pack's figures for one interval, `None` where it reported nothing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PackSlot {
    pub index: IntervalIndex,
    pub figures: Option<PackInterval>,
}

#[derive(Debug, Clone, PartialEq)]
struct IntervalBucket {
    index: IntervalIndex,
    solar: Mean<SolarPower>,
    home: Mean<Watts>,
    grid: Mean<GridPower>,
    battery: Mean<BatteryPower>,
    soc: Option<Soc>,
    packs: PackIntervals,
}

impl IntervalBucket {
    fn new(index: IntervalIndex) -> Self {
        IntervalBucket {
            index,
            solar: Mean::default(),
            home: Mean::default(),
            grid: Mean::default(),
            battery: Mean::default(),
            soc: None,
            packs: BTreeMap::new(),
        }
    }

    fn averages(&self) -> IntervalAverages {
        IntervalAverages {
            solar: self.solar.get(),
            home: self.home.get(),
            grid: self.grid.get(),
            battery: self.battery.get(),
            soc: self.soc,
        }
    }
}

/// Energy flows and SOC per fifteen minutes, for the last [`INTERVAL_RING`]
/// intervals. A pure fold over [`Event`]s.
///
/// Home usage needs the meter and every battery's last flow together, so the
/// fold keeps a [`World`] and derives it exactly as the stat card does.
///
/// Counts only the batteries it was built with: a [`World`] never forgets a
/// device, so a box swapped out or renamed would otherwise add its last flow
/// to every bucket for as long as the journal still holds it.
#[derive(Debug, Clone, PartialEq)]
pub struct IntervalHistory {
    devices: BTreeSet<DeviceId>,
    world: World,
    packs: PackTrace,
    latest: Option<IntervalIndex>,
    /// An interval seen once from beyond the window's back edge, not yet
    /// believed. See [`bucket_mut`](Self::bucket_mut).
    stale: Option<IntervalIndex>,
    ring: [Option<IntervalBucket>; INTERVAL_RING],
}

impl IntervalHistory {
    pub fn new(devices: impl IntoIterator<Item = DeviceId>) -> Self {
        IntervalHistory {
            devices: devices.into_iter().collect(),
            world: World::new(),
            packs: PackTrace::default(),
            latest: None,
            stale: None,
            ring: std::array::from_fn(|_| None),
        }
    }

    /// One poll's packs, as `device` reported them at `at`. Kept apart from
    /// [`record`](Self::record) because an [`Event`] carries only the box's
    /// totals, never its packs.
    pub fn record_packs(&mut self, device: &DeviceId, at: Timestamp, packs: &[PackStatus]) {
        if !self.devices.contains(device) {
            return;
        }
        let samples = self.packs.observe(device, at, packs);
        if let Some(bucket) = self.bucket_mut(IntervalIndex::containing(at)) {
            for sample in samples {
                sample.apply(bucket.packs.entry(sample.key().clone()).or_default());
            }
        }
    }

    pub fn record(&mut self, event: &Event) {
        let index = IntervalIndex::containing(event.at());
        match event {
            Event::Meter {
                sampled_at,
                grid,
                solar,
                ..
            } => {
                self.world.observe_meter(*sampled_at, *grid, *solar);
                let (solar, grid, home) = (
                    self.world.solar,
                    self.world.grid.total,
                    self.world.home_usage(),
                );
                if let Some(bucket) = self.bucket_mut(index) {
                    bucket.solar.add(solar);
                    bucket.grid.add(grid);
                    bucket.home.add(home);
                }
            }
            Event::DeviceUpdate { id, .. } if !self.devices.contains(id) => {}
            Event::DeviceUpdate {
                at,
                id,
                measurement,
            } => {
                self.world
                    .observe_device(id.clone(), at.now, measurement.clone());
                let flow = self.world.battery_flow();
                let soc = self.world.battery().map(|battery| battery.soc);
                if let Some(bucket) = self.bucket_mut(index) {
                    bucket.battery.add(flow);
                    bucket.soc = soc.or(bucket.soc);
                }
            }
            Event::MqttTimeout { .. } => {}
        }
    }

    /// The bucket for `index`, recycling its slot if an older interval held
    /// it. `None` for an interval that has already left the ring.
    ///
    /// `latest` only moves forward, so one event stamped far in the future
    /// (a bad clock step) would leave every correct event behind the window
    /// for good. A single event behind the window is indistinguishable from a
    /// late journal row, so it is dropped; a second one that lands within the
    /// window of the first is a clock that has settled there, and the ring
    /// rebases onto it: buckets past it are dropped, and those still inside
    /// its window (the stretch before the step) are kept. The price is the
    /// one event dropped, and that two late rows in a row from the same
    /// stretch of the past also rebase the ring.
    fn bucket_mut(&mut self, index: IntervalIndex) -> Option<&mut IntervalBucket> {
        match self.latest {
            Some(latest) if index < latest && !Self::within(latest, index) => {
                let confirmed = self
                    .stale
                    .is_some_and(|seen| Self::within(seen, index) || Self::within(index, seen));
                if !confirmed {
                    self.stale = Some(index);
                    return None;
                }
                for slot in &mut self.ring {
                    if slot.as_ref().is_some_and(|b| !Self::within(index, b.index)) {
                        *slot = None;
                    }
                }
                self.latest = None;
            }
            _ => {}
        }
        self.stale = None;
        self.latest = self.latest.max(Some(index));
        let slot = &mut self.ring[index.slot()];
        if slot.as_ref().is_none_or(|bucket| bucket.index != index) {
            *slot = Some(IntervalBucket::new(index));
        }
        slot.as_mut()
    }

    fn within(latest: IntervalIndex, index: IntervalIndex) -> bool {
        index <= latest && index > latest.offset(-(INTERVAL_RING as i64))
    }

    /// Checks the window as well as the key: a slot nothing has overwritten
    /// can still hold an interval the ring has moved past.
    pub fn averages(&self, index: IntervalIndex) -> IntervalAverages {
        self.bucket(index)
            .map(IntervalBucket::averages)
            .unwrap_or_default()
    }

    fn bucket(&self, index: IntervalIndex) -> Option<&IntervalBucket> {
        self.latest
            .filter(|&latest| Self::within(latest, index))
            .and_then(|_| self.ring[index.slot()].as_ref())
            .filter(|bucket| bucket.index == index)
    }

    /// Every pack's figures for one interval.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn packs(&self, index: IntervalIndex) -> BTreeMap<PackKey, PackInterval> {
        self.bucket(index)
            .map(|bucket| bucket.packs.clone())
            .unwrap_or_default()
    }

    /// Each pack seen in the 24 hours [`last_24h`](Self::last_24h) covers,
    /// with one slot per interval of that window. [`PackInterval::combined`]
    /// over a pack's slots gives its figures for the whole day.
    pub fn packs_last_24h(&self, now: Timestamp) -> BTreeMap<PackKey, Vec<PackSlot>> {
        let indices: Vec<IntervalIndex> = self.last_24h(now).iter().map(|s| s.index).collect();
        let buckets: Vec<Option<&IntervalBucket>> =
            indices.iter().map(|&index| self.bucket(index)).collect();
        let keys: BTreeSet<&PackKey> = buckets
            .iter()
            .flatten()
            .flat_map(|bucket| bucket.packs.keys())
            .collect();
        keys.into_iter()
            .map(|key| {
                let slots = indices
                    .iter()
                    .zip(&buckets)
                    .map(|(&index, bucket)| PackSlot {
                        index,
                        figures: bucket.and_then(|b| b.packs.get(key).copied()),
                    })
                    .collect();
                (key.clone(), slots)
            })
            .collect()
    }

    fn slots(&self, from: IntervalIndex, until: IntervalIndex) -> Vec<IntervalSlot> {
        std::iter::successors(Some(from), |index| Some(index.offset(1)))
            .take_while(|&index| index < until)
            .map(|index| IntervalSlot {
                index,
                averages: self.averages(index),
            })
            .collect()
    }

    /// The 24 hours up to and including the interval `now` is in, oldest first.
    pub fn last_24h(&self, now: Timestamp) -> Vec<IntervalSlot> {
        let until = IntervalIndex::containing(now).offset(1);
        self.slots(until.offset(-intervals_in(ROLLING_DAY)), until)
    }

    /// Every interval of local `date` that had finished by `now`, oldest
    /// first. Empty where `tz` cannot place the day's bounds.
    pub fn completed_on(&self, date: NaiveDate, tz: Tz, now: Timestamp) -> Vec<IntervalSlot> {
        let Some((start, end)) = local_day_bounds(date, tz) else {
            return Vec::new();
        };
        let until = IntervalIndex::containing(end).min(IntervalIndex::containing(now));
        self.slots(IntervalIndex::containing(start), until)
    }
}

/// The 1h resolution, derived rather than stored so it can never disagree
/// with the 15-minute buckets. Grouped by the hour each slot falls in, so a
/// run that starts mid-hour opens with a partial hour.
#[cfg_attr(not(test), allow(dead_code))]
pub fn hourly(quarters: &[IntervalSlot]) -> Vec<IntervalSlot> {
    quarters
        .chunk_by(|a, b| a.index.hour_start() == b.index.hour_start())
        .map(|hour| IntervalSlot {
            index: hour[0].index.hour_start(),
            averages: merged(hour),
        })
        .collect()
}

/// Consecutive intervals read as one: the mean of each flow, and the last
/// SOC seen.
pub fn merged(slots: &[IntervalSlot]) -> IntervalAverages {
    let each = || slots.iter().map(|slot| slot.averages);
    IntervalAverages {
        solar: Mean::of(each().map(|a| a.solar)),
        home: Mean::of(each().map(|a| a.home)),
        grid: Mean::of(each().map(|a| a.grid)),
        battery: Mean::of(each().map(|a| a.battery)),
        soc: each().filter_map(|a| a.soc).next_back(),
    }
}

/// One whole local day of the journal folded into a fresh [`IntervalHistory`]
/// through the same [`IntervalHistory::record`] the live loop uses. Flows
/// only: a past day's chart draws no pack figures. Empty for a date `tz`
/// cannot place.
pub fn history_of_day(
    journal_path: &std::path::Path,
    date: NaiveDate,
    tz: Tz,
    devices: impl IntoIterator<Item = DeviceId>,
) -> Result<IntervalHistory, ReadError> {
    let mut history = IntervalHistory::new(devices);
    let Some((start, end)) = local_day_bounds(date, tz) else {
        return Ok(history);
    };
    // The journal's range includes both ends; the next midnight is the next
    // day's.
    let last = end - Elapsed::of(Duration::from_millis(1));
    read_events_in_range(journal_path, start, last)?
        .iter()
        .for_each(|event| history.record(event));
    Ok(history)
}

/// Folds the journal's last ring's worth of events and poll captures into a
/// fresh [`IntervalHistory`] over `devices`, so a restart keeps the flows
/// chart and the pack figures.
pub fn seed_interval_history(
    journal_path: &std::path::Path,
    now: Timestamp,
    devices: impl IntoIterator<Item = DeviceId>,
) -> IntervalHistory {
    let mut history = IntervalHistory::new(devices);
    let span = Elapsed::of(INTERVAL * INTERVAL_RING as u32);
    seed_from(
        read_events_in_range(journal_path, now - span, now),
        "interval history",
        |event| history.record(&event),
    );
    // Packs are folded after the events rather than interleaved: no event
    // reads them, so the order between the two streams changes nothing.
    seed_from(
        read_raw_in_range(journal_path, POLL_CAPTURE, now - span, now),
        "pack history",
        |(at, body)| seed_packs(&mut history, at, &body),
    );
    history
}

fn seed_packs(history: &mut IntervalHistory, at: Timestamp, body: &str) {
    match packs_in_capture(body) {
        Ok(captured) => {
            if let (Some(device), Some(packs)) = (captured.device, captured.packs) {
                history.record_packs(&device, at, &packs);
            }
        }
        Err(e) => tracing::debug!("Dashboard: skipped a poll capture that no longer parses: {e}"),
    }
}

#[cfg(test)]
#[path = "intervals_tests.rs"]
mod tests;
