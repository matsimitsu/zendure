//! One local day of solar, forecast against actual, in half-hour slots: the
//! forecast poller's cached series and the 15-minute flows ring, both cut to
//! the day's real length so a DST day has 46 or 50 slots rather than 48.

use std::time::Duration;

use chrono::NaiveDate;
use chrono_tz::Tz;

use crate::clock::local_date;
use crate::units::{Elapsed, SolarForecastPoint, SolarPower, Timestamp};

use super::intervals::{IntervalHistory, IntervalIndex, Mean};
use super::plot::Day;
use super::state::ForecastSnapshot;

/// Solcast's own resolution, so a forecast point normally fills one slot.
pub(super) const SOLAR_SLOT: Duration = Duration::from_secs(30 * 60);

/// One half-hour of the day. `None` is "nothing known", never zero.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SolarSlot {
    pub start: Timestamp,
    pub forecast: Option<SolarPower>,
    /// The mean power measured over the slot, so far for the one in progress.
    pub actual: Option<SolarPower>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SolarDay {
    pub date: NaiveDate,
    pub slots: Vec<SolarSlot>,
}

impl SolarDay {
    pub fn forecast(&self) -> impl ExactSizeIterator<Item = Option<SolarPower>> + '_ {
        self.slots.iter().map(|slot| slot.forecast)
    }

    pub fn actual(&self) -> impl ExactSizeIterator<Item = Option<SolarPower>> + '_ {
        self.slots.iter().map(|slot| slot.actual)
    }
}

/// Today with its actuals, or tomorrow once its forecast covers every slot;
/// `None` for any other date.
pub fn solar_for(
    date: NaiveDate,
    forecast: &ForecastSnapshot,
    intervals: &IntervalHistory,
    tz: Tz,
    now: Timestamp,
) -> Option<SolarDay> {
    let today = local_date(now, tz)?;
    if date == today {
        Some(solar_day(date, &forecast.points, Some(intervals), tz))
    } else if Some(date) == today.succ_opt() && tomorrow_complete(forecast, tz, now) {
        Some(solar_day(date, &forecast.points, None, tz))
    } else {
        None
    }
}

/// Whether every half-hour of tomorrow has a forecast point, so the day can
/// be shown without a ragged tail.
pub fn tomorrow_complete(forecast: &ForecastSnapshot, tz: Tz, now: Timestamp) -> bool {
    let Some(tomorrow) = local_date(now, tz).and_then(|today| today.succ_opt()) else {
        return false;
    };
    solar_day(tomorrow, &forecast.points, None, tz)
        .forecast()
        .all(|slot| slot.is_some())
}

/// A slot's actual comes from the two 15-minute intervals it spans; an
/// interval that has not happened yet holds nothing, so the in-progress slot
/// averages only what it has.
fn solar_day(
    date: NaiveDate,
    points: &[SolarForecastPoint],
    intervals: Option<&IntervalHistory>,
    tz: Tz,
) -> SolarDay {
    let day = Day::of(date, tz);
    let starts: Vec<Timestamp> = day.slot_starts(SOLAR_SLOT).collect();
    let forecast = bucketed(points, day.start, starts.len());
    let slots = starts
        .into_iter()
        .zip(forecast)
        .map(|(start, forecast)| SolarSlot {
            start,
            forecast,
            actual: intervals.and_then(|intervals| actual_over(intervals, start)),
        })
        .collect();
    SolarDay { date, slots }
}

fn actual_over(intervals: &IntervalHistory, start: Timestamp) -> Option<SolarPower> {
    let first = IntervalIndex::containing(start);
    Mean::of([first, first.offset(1)].map(|index| intervals.averages(index).solar))
}

/// Averages where two points share a slot, which only a misaligned or
/// duplicated fetch produces.
fn bucketed(
    points: &[SolarForecastPoint],
    start: Timestamp,
    count: usize,
) -> Vec<Option<SolarPower>> {
    let width = Elapsed::of(SOLAR_SLOT).as_millis();
    let mut means: Vec<Mean<SolarPower>> = (0..count).map(|_| Mean::default()).collect();
    for point in points.iter().filter(|point| point.at >= start) {
        let slot = usize::try_from((point.at - start).as_millis() / width).ok();
        if let Some(mean) = slot.and_then(|slot| means.get_mut(slot)) {
            mean.add(point.estimate);
        }
    }
    means.iter().map(Mean::get).collect()
}

#[cfg(test)]
#[path = "solar_day_tests.rs"]
mod tests;
