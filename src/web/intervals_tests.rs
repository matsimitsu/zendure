//! The interval ring: what lands in which bucket, what the ring forgets, and
//! that the journal seed and the live fold agree.

use super::*;

use crate::battery::BatteryState;
use crate::fixtures::journey::{self, battery_event, device_update, meter_event};
use crate::fixtures::utc;
use crate::world::DeviceId;

fn at(secs: i64) -> Timestamp {
    Timestamp::from_millis(journey::NOW_MS + secs * 1000)
}

fn configured() -> [DeviceId; 1] {
    [DeviceId::new(journey::BATTERY_ID)]
}

fn ring() -> IntervalHistory {
    IntervalHistory::new(configured())
}

fn base() -> IntervalIndex {
    IntervalIndex::containing(at(0))
}

fn into_interval(index: IntervalIndex, secs: i64) -> Timestamp {
    Timestamp::from_millis(index.start().as_millis() + secs * 1000)
}

#[test]
fn readings_average_within_an_interval_and_the_next_one_starts_fresh() {
    let mut history = ring();
    history.record(&meter_event(into_interval(base(), 0), 100.0, 200.0));
    history.record(&meter_event(into_interval(base(), 60), 300.0, 400.0));
    history.record(&battery_event(
        into_interval(base(), 90),
        BatteryPower(-400),
        Soc::new(60),
    ));
    history.record(&battery_event(
        into_interval(base(), 120),
        BatteryPower(-200),
        Soc::new(61),
    ));
    history.record(&meter_event(into_interval(base().offset(1), 0), 250.0, 0.0));

    let first = history.averages(base());
    assert_eq!(first.grid, Some(GridPower(200.0)));
    assert_eq!(first.solar, Some(SolarPower::new(300.0)));
    assert_eq!(first.home, Some(Watts(500)));
    assert_eq!(first.battery, Some(BatteryPower(-300)));
    assert_eq!(first.soc, Some(Soc::new(61)));

    let second = history.averages(base().offset(1));
    assert_eq!(second.grid, Some(GridPower(250.0)));
    assert_eq!(second.battery, None);
    // The last flow seen, 200 W into the pack, is not the house's demand.
    assert_eq!(second.home, Some(Watts(50)));
}

#[test]
fn an_interval_older_than_the_ring_is_gone() {
    let mut history = ring();
    history.record(&meter_event(into_interval(base(), 0), 100.0, 0.0));
    history.record(&meter_event(into_interval(base().offset(99), 0), 1.0, 0.0));
    assert_eq!(history.averages(base()).grid, Some(GridPower(100.0)));

    // Same slot as `base`, one lap later.
    history.record(&meter_event(into_interval(base().offset(100), 0), 2.0, 0.0));
    assert_eq!(history.averages(base()), IntervalAverages::default());

    // A slot nothing overwrote, which the ring has moved past all the same.
    let mut gapped = ring();
    gapped.record(&meter_event(into_interval(base(), 0), 100.0, 0.0));
    gapped.record(&meter_event(into_interval(base().offset(150), 0), 2.0, 0.0));
    assert_eq!(gapped.averages(base()), IntervalAverages::default());
}

#[test]
fn a_late_event_from_before_the_ring_cannot_clobber_its_slot() {
    let mut history = ring();
    history.record(&meter_event(into_interval(base(), 0), 100.0, 0.0));
    history.record(&meter_event(
        into_interval(base().offset(-100), 0),
        9.0,
        0.0,
    ));

    assert_eq!(history.averages(base()).grid, Some(GridPower(100.0)));
    assert_eq!(
        history.averages(base().offset(-100)),
        IntervalAverages::default()
    );
}

/// 2026-10-25 in Amsterdam runs 02:00–03:00 twice. Keyed on local time the two
/// 02:15s would share a bucket; keyed on the absolute index they are an hour
/// apart, and the 25-hour day fills all 100 slots.
#[test]
fn the_repeated_hour_of_a_dst_change_keeps_its_own_intervals() {
    let tz = chrono_tz::Europe::Amsterdam;
    let mut history = ring();
    history.record(&meter_event(utc(24, 22, 0), 50.0, 0.0)); // 00:00 CEST
    history.record(&meter_event(utc(25, 0, 15), 100.0, 0.0)); // 02:15 CEST
    history.record(&meter_event(utc(25, 1, 15), 200.0, 0.0)); // 02:15 CET
    history.record(&meter_event(utc(25, 22, 45), 300.0, 0.0)); // 23:45 CET

    let day = chrono::NaiveDate::from_ymd_opt(2026, 10, 25).unwrap();
    let slots = history.completed_on(day, tz, utc(25, 23, 0));

    assert_eq!(slots.len(), 100);
    let grid = |i: usize| slots[i].averages.grid;
    assert_eq!(grid(0), Some(GridPower(50.0)));
    assert_eq!(grid(9), Some(GridPower(100.0)));
    assert_eq!(grid(13), Some(GridPower(200.0)));
    assert_eq!(grid(99), Some(GridPower(300.0)));
    assert_eq!(slots[13].index, slots[9].index.offset(4));
}

#[test]
fn completed_on_stops_before_the_interval_still_in_progress() {
    let tz = chrono_tz::Europe::Amsterdam;
    let mut history = ring();
    history.record(&meter_event(utc(24, 22, 0), 50.0, 0.0));
    history.record(&meter_event(utc(24, 22, 20), 60.0, 0.0));

    let day = chrono::NaiveDate::from_ymd_opt(2026, 10, 25).unwrap();
    let slots = history.completed_on(day, tz, utc(24, 22, 20));

    assert_eq!(slots.len(), 1);
    assert_eq!(slots[0].averages.grid, Some(GridPower(50.0)));
}

#[test]
fn last_24h_is_96_intervals_ending_with_the_current_one() {
    let mut history = ring();
    history.record(&meter_event(into_interval(base(), 0), 100.0, 0.0));

    let slots = history.last_24h(into_interval(base(), 30));

    assert_eq!(slots.len(), 96);
    assert_eq!(slots[0].index, base().offset(-95));
    assert_eq!(slots[95].index, base());
    assert_eq!(slots[95].averages.grid, Some(GridPower(100.0)));
}

/// A restart must show the same chart the process it replaced was showing.
#[tokio::test]
async fn the_journal_seed_equals_the_live_fold() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    let events = journey::session();
    crate::journal::testing::record(&path, &events).await;

    let mut live = ring();
    events.iter().for_each(|event| live.record(event));
    let seeded = seed_interval_history(&path, at(1_000), configured());

    assert_ne!(live, ring());
    assert_eq!(seeded, live);
}

#[test]
fn an_unreadable_journal_seeds_an_empty_history() {
    let dir = tempfile::tempdir().unwrap();
    let seeded = seed_interval_history(&dir.path().join("missing.db"), at(0), configured());
    assert_eq!(seeded, ring());
}

// --- Configured devices -----------------------------------------------------

/// A box swapped out, or a simulator run on the same journal, leaves its last
/// report in the window. It must not count toward battery flow or SOC.
fn foreign_update(now: Timestamp) -> Event {
    device_update(
        journey::clock_on(now),
        "retired-battery",
        BatteryState {
            current_power: BatteryPower(900),
            soc: Soc::new(5),
            ..BatteryState::test_sample()
        },
    )
}

#[test]
fn a_battery_that_is_not_configured_counts_for_nothing() {
    let mut history = ring();
    history.record(&foreign_update(into_interval(base(), 0)));
    history.record(&battery_event(
        into_interval(base(), 30),
        BatteryPower(-200),
        Soc::new(60),
    ));
    history.record(&meter_event(into_interval(base(), 60), 100.0, 0.0));

    let averages = history.averages(base());
    assert_eq!(averages.battery, Some(BatteryPower(-200)));
    assert_eq!(averages.soc, Some(Soc::new(60)));
    // 100 W imported while 200 W goes into the pack.
    assert_eq!(averages.home, Some(Watts(-100)));
}

#[tokio::test]
async fn the_journal_seed_skips_a_battery_that_is_not_configured() {
    let dir = tempfile::tempdir().unwrap();
    let clean = dir.path().join("clean.db");
    let mixed = dir.path().join("mixed.db");
    let events = journey::session();
    let with_foreign: Vec<Event> = std::iter::once(foreign_update(at(10)))
        .chain(events.iter().cloned())
        .collect();
    crate::journal::testing::record(&clean, &events).await;
    crate::journal::testing::record(&mixed, &with_foreign).await;

    assert_eq!(
        seed_interval_history(&mixed, at(1_000), configured()),
        seed_interval_history(&clean, at(1_000), configured()),
    );
}

// --- Hourly ------------------------------------------------------------------

fn quarter(
    index: IntervalIndex,
    grid: f64,
    battery: Option<i32>,
    soc: Option<u32>,
) -> IntervalSlot {
    IntervalSlot {
        index,
        averages: IntervalAverages {
            solar: Some(SolarPower::new(grid.abs())),
            home: Some(Watts::rounded(grid / 10.0)),
            grid: Some(GridPower(grid)),
            battery: battery.map(BatteryPower),
            soc: soc.map(Soc::new),
        },
    }
}

#[test]
fn an_hour_is_the_mean_of_its_four_quarters() {
    let first = base().hour_start();
    let quarters = [
        quarter(first, 100.0, Some(0), Some(50)),
        quarter(first.offset(1), -300.0, Some(-40), Some(51)),
        quarter(first.offset(2), 500.0, None, None),
        quarter(first.offset(3), 700.0, Some(-120), Some(53)),
        quarter(first.offset(4), 1.0, None, None),
    ];

    let hours = hourly(&quarters);

    assert_eq!(hours.len(), 2);
    assert_eq!(hours[0].index, first);
    assert_eq!(
        hours[0].averages,
        IntervalAverages {
            solar: Some(SolarPower::new(400.0)),
            home: Some(Watts(25)),
            grid: Some(GridPower(250.0)),
            // The quarter with no battery reading is a gap, not a zero.
            battery: Some(BatteryPower(-53)),
            soc: Some(Soc::new(53)),
        }
    );
    assert_eq!(hours[1].index, first.offset(4));
    assert_eq!(hours[1].averages.grid, Some(GridPower(1.0)));
}

/// `last_24h` starts wherever `now` puts it, so its first hour is partial.
/// Grouping by the hour each quarter is in keeps every later hour whole.
#[test]
fn hours_follow_the_clock_when_the_quarters_start_mid_hour() {
    let first = base().hour_start();
    let quarters = [
        quarter(first.offset(2), 100.0, None, None),
        quarter(first.offset(3), 300.0, None, None),
        quarter(first.offset(4), 10.0, None, None),
        quarter(first.offset(5), 20.0, None, None),
    ];

    let hours = hourly(&quarters);

    assert_eq!(hours.len(), 2);
    assert_eq!(hours[0].index, first);
    assert_eq!(hours[0].averages.grid, Some(GridPower(200.0)));
    assert_eq!(hours[1].index, first.offset(4));
    assert_eq!(hours[1].averages.grid, Some(GridPower(15.0)));
}

#[test]
fn hourly_of_the_last_24h_lands_every_hour_on_the_hour() {
    let slots = ring().last_24h(into_interval(base(), 30));
    let hours = hourly(&slots);

    assert_eq!(hours[0].index, slots[0].index.hour_start());
    assert_eq!(hours[hours.len() - 1].index, base().hour_start());
    assert!(hours.iter().all(|h| h.index == h.index.hour_start()));
}
