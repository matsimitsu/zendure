//! One local day of solar, forecast against actual, in half-hour slots: the
//! forecast poller's cached series and the 15-minute flows ring, both cut to
//! the day's real length so a DST day has 46 or 50 slots rather than 48.

use std::time::Duration;

use chrono::NaiveDate;
use chrono_tz::Tz;

use crate::clock::local_date;
use crate::prediction::FORECAST_PERIOD;
use crate::units::{Elapsed, SolarForecastPoint, SolarPower, Timestamp};

use super::intervals::{IntervalHistory, IntervalIndex, Mean};
use super::plot::Day;
use super::state::ForecastSnapshot;

/// Solcast's own resolution, so a forecast point normally fills one slot.
pub(super) const SOLAR_SLOT: Duration = FORECAST_PERIOD;

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
}

/// The days the panel reaches: today, and tomorrow once its forecast covers
/// every slot so it shows without a ragged tail.
#[derive(Debug, Clone, PartialEq)]
pub struct SolarDays {
    pub today: SolarDay,
    pub tomorrow: Option<SolarDay>,
}

impl SolarDays {
    pub fn get(&self, date: NaiveDate) -> Option<&SolarDay> {
        self.iter().find(|day| day.date == date)
    }

    pub fn iter(&self) -> impl Iterator<Item = &SolarDay> {
        std::iter::once(&self.today).chain(&self.tomorrow)
    }
}

/// Today with its actuals and a complete tomorrow; `None` for a clock no
/// calendar can place.
pub fn solar_days(
    forecast: &ForecastSnapshot,
    intervals: &IntervalHistory,
    tz: Tz,
    now: Timestamp,
) -> Option<SolarDays> {
    let today = local_date(now, tz)?;
    let tomorrow = today
        .succ_opt()
        .map(|date| solar_day(date, &forecast.points, None, tz))
        .filter(|day| day.forecast().all(|slot| slot.is_some()));
    Some(SolarDays {
        today: solar_day(today, &forecast.points, Some(intervals), tz),
        tomorrow,
    })
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

/// Pooled over every reading in the slot rather than a mean of the two
/// quarter-hour means, so a quarter with a minute of samples weighs a minute.
fn actual_over(intervals: &IntervalHistory, start: Timestamp) -> Option<SolarPower> {
    let first = IntervalIndex::containing(start);
    let mut mean = intervals.solar(first);
    mean.merge(&intervals.solar(first.offset(1)));
    mean.get()
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
