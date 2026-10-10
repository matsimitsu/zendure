use super::*;

use chrono::Datelike;

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
        .flat_map(|&day| Day::of(day, tz).slot_starts(SOLAR_SLOT))
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

fn days_at(forecast: &ForecastSnapshot, intervals: &IntervalHistory, now: Timestamp) -> SolarDays {
    solar_days(forecast, intervals, amsterdam(), now).unwrap()
}

// --- Slots ------------------------------------------------------------------

#[test]
fn a_day_has_one_slot_per_half_hour_of_its_real_length() {
    for (day, slots) in [(date(3, 29), 46), (date(6, 15), 48), (date(10, 25), 50)] {
        let forecast = whole_days(&[day], amsterdam());
        let noon = local(day.month(), day.day(), 12, 0);

        let today = days_at(&forecast, &no_history(), noon).today;

        assert_eq!(today.date, day);
        assert_eq!(today.slots.len(), slots, "{day}");
        assert!(today.forecast().all(|slot| slot.is_some()), "{day}");
    }
}

#[test]
fn a_complete_tomorrow_on_a_dst_day_has_fifty_slots_and_no_actuals() {
    let forecast = whole_days(&[date(10, 24), date(10, 25)], amsterdam());
    let mut intervals = no_history();
    intervals.record(&meter(local(10, 24, 12, 0), 900.0));

    let days = days_at(&forecast, &intervals, local(10, 24, 12, 0));
    let tomorrow = days.tomorrow.unwrap();

    assert_eq!(tomorrow.date, date(10, 25));
    assert_eq!(tomorrow.slots.len(), 50);
    assert!(tomorrow.slots.iter().all(|slot| slot.actual.is_none()));
    assert!(days.today.slots[24].actual.is_some());
}

// --- Tomorrow ---------------------------------------------------------------

/// A fetch that stops at noon tomorrow leaves its afternoon without bars.
#[test]
fn a_partial_tomorrow_is_not_kept() {
    let mut forecast = whole_days(&[date(6, 15), date(6, 16)], amsterdam());
    let cutoff = local(6, 16, 12, 0);
    forecast.points.retain(|point| point.at < cutoff);

    let days = days_at(&forecast, &no_history(), local(6, 15, 9, 0));

    assert_eq!(days.tomorrow, None);
    assert_eq!(days.get(date(6, 16)), None);
    assert_eq!(days.iter().count(), 1);
}

#[test]
fn a_whole_tomorrow_is_kept_beside_today() {
    let forecast = whole_days(&[date(6, 16)], amsterdam());

    let days = days_at(&forecast, &no_history(), local(6, 15, 9, 0));

    assert_eq!(days.get(date(6, 16)).map(|day| day.date), Some(date(6, 16)));
    assert_eq!(days.get(date(6, 15)).map(|day| day.date), Some(date(6, 15)));
    assert_eq!(days.iter().count(), 2);
}

#[test]
fn an_empty_forecast_has_no_tomorrow() {
    let days = days_at(
        &ForecastSnapshot::default(),
        &no_history(),
        local(6, 15, 9, 0),
    );

    assert_eq!(days.tomorrow, None);
    assert!(days.today.forecast().all(|slot| slot.is_none()));
}

#[test]
fn neither_yesterday_nor_the_day_after_tomorrow_is_served() {
    let forecast = whole_days(&[date(6, 14), date(6, 16), date(6, 17)], amsterdam());

    let days = days_at(&forecast, &no_history(), local(6, 15, 9, 0));

    for day in [date(6, 14), date(6, 17)] {
        assert_eq!(days.get(day), None, "{day}");
    }
}

// --- Forecast ---------------------------------------------------------------

fn utc_today(forecast: &ForecastSnapshot, intervals: &IntervalHistory, now: Timestamp) -> SolarDay {
    solar_days(forecast, intervals, chrono_tz::UTC, now)
        .unwrap()
        .today
}

/// Only a misaligned or duplicated fetch puts two points in one slot.
#[test]
fn two_points_in_one_slot_average() {
    let forecast = ForecastSnapshot {
        points: vec![point(utc(5, 6, 0), 1000.0), point(utc(5, 6, 10), 2000.0)],
        as_of: None,
    };

    let today = utc_today(&forecast, &no_history(), utc(5, 9, 0));

    assert_eq!(today.slots[12].forecast, Some(SolarPower::new(1500.0)));
    assert_eq!(today.slots[13].forecast, None);
}

#[test]
fn points_outside_the_day_are_left_out() {
    let forecast = ForecastSnapshot {
        points: vec![point(utc(4, 23, 0), 500.0), point(utc(6, 0, 0), 500.0)],
        as_of: None,
    };

    let today = utc_today(&forecast, &no_history(), utc(5, 9, 0));

    assert!(today.forecast().all(|slot| slot.is_none()));
}

// --- Actual -----------------------------------------------------------------

fn today_with(events: &[crate::event::Event], now: Timestamp) -> SolarDay {
    let mut intervals = no_history();
    events.iter().for_each(|event| intervals.record(event));
    utc_today(&ForecastSnapshot::default(), &intervals, now)
}

/// 06:00-06:30 is two quarters; every reading in either weighs the same.
#[test]
fn a_slot_is_the_mean_of_every_reading_in_it() {
    let today = today_with(
        &[
            meter(utc(5, 6, 0), 1000.0),
            meter(utc(5, 6, 5), 2000.0),
            meter(utc(5, 6, 20), 3600.0),
        ],
        utc(5, 9, 0),
    );

    assert_eq!(today.slots[12].actual, Some(SolarPower::new(2200.0)));
}

/// A quarter that has only just begun holds one reading against the
/// previous quarter's many, and counts for that one reading.
#[test]
fn a_barely_started_quarter_weighs_by_its_readings() {
    let mut events: Vec<_> = (0..15)
        .map(|minute| meter(utc(5, 9, minute), 600.0))
        .collect();
    events.push(meter(utc(5, 9, 15), 2200.0));

    let today = today_with(&events, utc(5, 9, 15));

    assert_eq!(today.slots[18].actual, Some(SolarPower::new(700.0)));
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
