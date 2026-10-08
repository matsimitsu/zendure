//! Energy flows and SOC per fifteen minutes: the ring the flows chart reads.
//!
//! [`IntervalHistory`] keeps its own [`World`] rather than reading the
//! engine's, because one fold serves the live loop, the startup seed and a
//! past day read back from the journal, and the last two replay history the
//! engine's world has already moved past.

use std::collections::BTreeSet;
use std::marker::PhantomData;
use std::time::Duration;

use chrono::NaiveDate;
use chrono_tz::Tz;

use crate::event::Event;
use crate::journal::read::read_events_in_range;
use crate::units::{BatteryPower, Elapsed, GridPower, Soc, SolarPower, Timestamp, Watts};
use crate::world::{DeviceId, World};

use super::state::Plottable;

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
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn of(values: impl IntoIterator<Item = Option<T>>) -> Option<T> {
        let mut mean = Mean::default();
        values.into_iter().flatten().for_each(|v| mean.add(v));
        mean.get()
    }
}

/// The width of one [`IntervalHistory`] bucket.
const INTERVAL: Duration = Duration::from_secs(15 * 60);

const HOUR: Duration = Duration::from_secs(60 * 60);

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

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn start(self) -> Timestamp {
        Timestamp::from_millis(self.0.saturating_mul(Self::width().as_millis()))
    }

    /// The index `count` intervals later; a negative count goes back.
    pub fn offset(self, count: i64) -> Self {
        IntervalIndex(self.0.saturating_add(count))
    }

    /// The first interval of the hour this one falls in.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn hour_start(self) -> Self {
        let per_hour = intervals_in(HOUR);
        IntervalIndex(self.0.div_euclid(per_hour) * per_hour)
    }

    fn slot(self) -> usize {
        // `rem_euclid` by the ring's length is always a valid slot.
        self.0.rem_euclid(INTERVAL_RING as i64) as usize
    }
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

#[derive(Debug, Clone, PartialEq)]
struct IntervalBucket {
    index: IntervalIndex,
    solar: Mean<SolarPower>,
    home: Mean<Watts>,
    grid: Mean<GridPower>,
    battery: Mean<BatteryPower>,
    soc: Option<Soc>,
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
    latest: Option<IntervalIndex>,
    ring: [Option<IntervalBucket>; INTERVAL_RING],
}

impl IntervalHistory {
    pub fn new(devices: impl IntoIterator<Item = DeviceId>) -> Self {
        IntervalHistory {
            devices: devices.into_iter().collect(),
            world: World::new(),
            latest: None,
            ring: std::array::from_fn(|_| None),
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
    fn bucket_mut(&mut self, index: IntervalIndex) -> Option<&mut IntervalBucket> {
        if self
            .latest
            .is_some_and(|latest| index < latest && !Self::within(latest, index))
        {
            return None;
        }
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
        self.latest
            .filter(|&latest| Self::within(latest, index))
            .and_then(|_| self.ring[index.slot()].as_ref())
            .filter(|bucket| bucket.index == index)
            .map(IntervalBucket::averages)
            .unwrap_or_default()
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
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn last_24h(&self, now: Timestamp) -> Vec<IntervalSlot> {
        let until = IntervalIndex::containing(now).offset(1);
        self.slots(until.offset(-intervals_in(ROLLING_DAY)), until)
    }

    /// Every interval of local `date` that had finished by `now`, oldest
    /// first. Empty where `tz` cannot place the day's bounds.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn completed_on(&self, date: NaiveDate, tz: Tz, now: Timestamp) -> Vec<IntervalSlot> {
        let bounds = date.succ_opt().and_then(|next| {
            Some((
                crate::clock::local_day_start(date, tz)?,
                crate::clock::local_day_start(next, tz)?,
            ))
        });
        let Some((start, end)) = bounds else {
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
        .map(|hour| {
            let each = || hour.iter().map(|slot| slot.averages);
            IntervalSlot {
                index: hour[0].index.hour_start(),
                averages: IntervalAverages {
                    solar: Mean::of(each().map(|a| a.solar)),
                    home: Mean::of(each().map(|a| a.home)),
                    grid: Mean::of(each().map(|a| a.grid)),
                    battery: Mean::of(each().map(|a| a.battery)),
                    soc: each().filter_map(|a| a.soc).next_back(),
                },
            }
        })
        .collect()
}

/// Folds the journal's last ring's worth of events into a fresh
/// [`IntervalHistory`] over `devices`, so a restart keeps the flows chart.
/// Degrades to an empty history on a read failure, as `seed_decision_log`
/// does.
pub fn seed_interval_history(
    journal_path: &std::path::Path,
    now: Timestamp,
    devices: impl IntoIterator<Item = DeviceId>,
) -> IntervalHistory {
    let mut history = IntervalHistory::new(devices);
    let span = Elapsed::of(INTERVAL * INTERVAL_RING as u32);
    match read_events_in_range(journal_path, now - span, now) {
        Ok(events) => events.iter().for_each(|event| history.record(event)),
        Err(e) => tracing::warn!("Dashboard: cannot seed interval history from journal: {e}"),
    }
    history
}

#[cfg(test)]
#[path = "intervals_tests.rs"]
mod tests;
