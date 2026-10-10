use super::*;

use chrono::Datelike;

use crate::clock::local_day_bounds;
use crate::fixtures::{amsterdam, date, journey, local, utc};

fn point(at: Timestamp, watts: f64) -> SolarForecastPoint {
    SolarForecastPoint {
        at,
        estimate: SolarPower::new(watts),
    }
}

/// One point per half-hour of each of `days`, as a whole-day fetch returns.
fn whole_days(days: &[NaiveDate], tz: Tz) -> ForecastSnapshot {
    let points = days
        .iter()
        .flat_map(|&day| {
            let (start, end) = local_day_bounds(day, tz).unwrap();
            std::iter::successors(Some(start), |&at| Some(at + Elapsed::of(SOLAR_SLOT)))
                .take_while(move |&at| at < end)
        })
        .map(|at| point(at, 500.0))
        .collect();
    ForecastSnapshot {
        points,
        as_of: None,
    }
}

fn no_history() -> IntervalHistory {
    journey::interval_ring()
}

fn meter(at: Timestamp, solar: f64) -> crate::event::Event {
    journey::meter_event(at, 0.0, solar)
}

// --- Slots ------------------------------------------------------------------

#[test]
fn a_day_has_one_slot_per_half_hour_of_its_real_length() {
    for (day, slots) in [(date(3, 29), 46), (date(6, 15), 48), (date(10, 25), 50)] {
        let forecast = whole_days(&[day], amsterdam());
        let noon = local(day.month(), day.day(), 12, 0);

        let today = solar_for(day, &forecast, &no_history(), amsterdam(), noon).unwrap();

        assert_eq!(today.slots.len(), slots, "{day}");
        assert!(today.forecast().all(|slot| slot.is_some()), "{day}");
    }
}

#[test]
fn a_complete_tomorrow_on_a_dst_day_has_fifty_slots_and_no_actuals() {
    let forecast = whole_days(&[date(10, 24), date(10, 25)], amsterdam());
    let mut intervals = no_history();
    intervals.record(&meter(local(10, 24, 12, 0), 900.0));

    let tomorrow = solar_for(
        date(10, 25),
        &forecast,
        &intervals,
        amsterdam(),
        local(10, 24, 12, 0),
    )
    .unwrap();

    assert_eq!(tomorrow.slots.len(), 50);
    assert!(tomorrow.actual().all(|slot| slot.is_none()));
}

// --- Tomorrow ---------------------------------------------------------------

/// A fetch that stops at noon tomorrow leaves its afternoon without bars.
#[test]
fn a_partial_tomorrow_is_not_complete_and_not_shown() {
    let mut forecast = whole_days(&[date(6, 15), date(6, 16)], amsterdam());
    let cutoff = local(6, 16, 12, 0);
    forecast.points.retain(|point| point.at < cutoff);
    let now = local(6, 15, 9, 0);

    assert!(!tomorrow_complete(&forecast, amsterdam(), now));
    assert_eq!(
        solar_for(date(6, 16), &forecast, &no_history(), amsterdam(), now),
        None
    );
}

#[test]
fn a_whole_tomorrow_is_complete() {
    let forecast = whole_days(&[date(6, 16)], amsterdam());
    assert!(tomorrow_complete(
        &forecast,
        amsterdam(),
        local(6, 15, 9, 0)
    ));
}

#[test]
fn an_empty_forecast_has_no_tomorrow() {
    assert!(!tomorrow_complete(
        &ForecastSnapshot::default(),
        amsterdam(),
        local(6, 15, 9, 0)
    ));
}

#[test]
fn neither_yesterday_nor_the_day_after_tomorrow_is_served() {
    let forecast = whole_days(&[date(6, 14), date(6, 16), date(6, 17)], amsterdam());
    let now = local(6, 15, 9, 0);
    for day in [date(6, 14), date(6, 17)] {
        assert_eq!(
            solar_for(day, &forecast, &no_history(), amsterdam(), now),
            None
        );
    }
}

// --- Forecast ---------------------------------------------------------------

/// Only a misaligned or duplicated fetch puts two points in one slot.
#[test]
fn two_points_in_one_slot_average() {
    let forecast = ForecastSnapshot {
        points: vec![point(utc(5, 6, 0), 1000.0), point(utc(5, 6, 10), 2000.0)],
        as_of: None,
    };

    let today = solar_for(
        date(10, 5),
        &forecast,
        &no_history(),
        chrono_tz::UTC,
        utc(5, 9, 0),
    )
    .unwrap();

    assert_eq!(today.slots[12].forecast, Some(SolarPower::new(1500.0)));
    assert_eq!(today.slots[13].forecast, None);
}

#[test]
fn points_outside_the_day_are_left_out() {
    let forecast = ForecastSnapshot {
        points: vec![point(utc(4, 23, 0), 500.0), point(utc(6, 0, 0), 500.0)],
        as_of: None,
    };

    let today = solar_for(
        date(10, 5),
        &forecast,
        &no_history(),
        chrono_tz::UTC,
        utc(5, 9, 0),
    )
    .unwrap();

    assert!(today.forecast().all(|slot| slot.is_none()));
}

// --- Actual -----------------------------------------------------------------

fn today_with(events: &[crate::event::Event], now: Timestamp) -> SolarDay {
    let mut intervals = no_history();
    events.iter().for_each(|event| intervals.record(event));
    solar_for(
        date(10, 5),
        &ForecastSnapshot::default(),
        &intervals,
        chrono_tz::UTC,
        now,
    )
    .unwrap()
}

/// 06:00-06:30 is two quarters; each contributes its own mean.
#[test]
fn a_slot_is_the_mean_of_its_two_quarter_hours() {
    let today = today_with(
        &[
            meter(utc(5, 6, 0), 1000.0),
            meter(utc(5, 6, 5), 2000.0),
            meter(utc(5, 6, 20), 3500.0),
        ],
        utc(5, 9, 0),
    );

    assert_eq!(today.slots[12].actual, Some(SolarPower::new(2500.0)));
}

#[test]
fn a_slot_with_one_quarter_hour_takes_that_quarters_mean() {
    let today = today_with(&[meter(utc(5, 6, 20), 800.0)], utc(5, 9, 0));

    assert_eq!(today.slots[12].actual, Some(SolarPower::new(800.0)));
}

#[test]
fn a_slot_with_no_readings_is_unknown_not_zero() {
    let today = today_with(&[meter(utc(5, 6, 0), 800.0)], utc(5, 9, 0));

    assert_eq!(today.slots[11].actual, None);
    assert_eq!(today.slots[13].actual, None);
}

/// The slot in progress shows what it has so far; the ones after it nothing.
#[test]
fn the_slot_in_progress_has_an_actual_and_the_rest_of_the_day_does_not() {
    let today = today_with(
        &[meter(utc(5, 9, 0), 600.0), meter(utc(5, 9, 5), 700.0)],
        utc(5, 9, 6),
    );

    assert_eq!(today.slots[18].actual, Some(SolarPower::new(650.0)));
    assert!(today.slots[19..].iter().all(|slot| slot.actual.is_none()));
}
