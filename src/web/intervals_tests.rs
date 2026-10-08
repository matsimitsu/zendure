//! The interval ring: what lands in which bucket, what the ring forgets, and
//! that the journal seed and the live fold agree.

use super::*;

use crate::battery::BatteryState;
use crate::fixtures::journey::{
    self, battery_event, device_update, interval_ring, meter_event, poll_body,
};
use crate::fixtures::utc;
use crate::units::{DeciKelvin, WattHours};
use crate::web::pack_intervals::{Extent, PackId};
use crate::world::DeviceId;

fn at(secs: i64) -> Timestamp {
    Timestamp::from_millis(journey::NOW_MS + secs * 1000)
}

fn configured() -> [DeviceId; 1] {
    [DeviceId::new(journey::BATTERY_ID)]
}

fn base() -> IntervalIndex {
    IntervalIndex::containing(at(0))
}

fn into_interval(index: IntervalIndex, secs: i64) -> Timestamp {
    Timestamp::from_millis(index.start().as_millis() + secs * 1000)
}

#[test]
fn readings_average_within_an_interval_and_the_next_one_starts_fresh() {
    let mut history = interval_ring();
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
    let mut history = interval_ring();
    history.record(&meter_event(into_interval(base(), 0), 100.0, 0.0));
    history.record(&meter_event(into_interval(base().offset(99), 0), 1.0, 0.0));
    assert_eq!(history.averages(base()).grid, Some(GridPower(100.0)));

    // Same slot as `base`, one lap later.
    history.record(&meter_event(into_interval(base().offset(100), 0), 2.0, 0.0));
    assert_eq!(history.averages(base()), IntervalAverages::default());

    // A slot nothing overwrote, which the ring has moved past all the same.
    let mut gapped = interval_ring();
    gapped.record(&meter_event(into_interval(base(), 0), 100.0, 0.0));
    gapped.record(&meter_event(into_interval(base().offset(150), 0), 2.0, 0.0));
    assert_eq!(gapped.averages(base()), IntervalAverages::default());
}

#[test]
fn a_late_event_from_before_the_ring_cannot_clobber_its_slot() {
    let mut history = interval_ring();
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

#[test]
fn the_ring_recovers_after_the_clock_steps_far_ahead_and_back() {
    let mut history = interval_ring();
    history.record(&meter_event(into_interval(base(), 0), 100.0, 0.0));
    // A step of more than the ring's span, then corrected.
    history.record(&meter_event(into_interval(base().offset(500), 0), 5.0, 0.0));

    // The first corrected event is indistinguishable from a late row.
    history.record(&meter_event(into_interval(base().offset(1), 0), 7.0, 0.0));
    assert_eq!(
        history.averages(base().offset(1)),
        IntervalAverages::default()
    );

    // The second says the clock has settled here.
    history.record(&meter_event(into_interval(base().offset(2), 0), 8.0, 0.0));
    history.record(&meter_event(into_interval(base().offset(3), 0), 9.0, 0.0));
    assert_eq!(
        history.averages(base().offset(2)).grid,
        Some(GridPower(8.0))
    );
    assert_eq!(
        history.averages(base().offset(3)).grid,
        Some(GridPower(9.0))
    );
    assert_eq!(
        history.averages(base().offset(500)),
        IntervalAverages::default()
    );
}

#[test]
fn an_isolated_late_row_does_not_count_towards_a_rebase() {
    let mut history = interval_ring();
    history.record(&meter_event(into_interval(base(), 0), 100.0, 0.0));
    history.record(&meter_event(
        into_interval(base().offset(-200), 0),
        1.0,
        0.0,
    ));
    history.record(&meter_event(into_interval(base().offset(1), 0), 2.0, 0.0));
    history.record(&meter_event(
        into_interval(base().offset(-200), 0),
        3.0,
        0.0,
    ));

    assert_eq!(history.averages(base()).grid, Some(GridPower(100.0)));
    assert_eq!(
        history.averages(base().offset(1)).grid,
        Some(GridPower(2.0))
    );
}

/// 2026-10-25 in Amsterdam runs 02:00–03:00 twice. Keyed on local time the two
/// 02:15s would share a bucket; keyed on the absolute index they are an hour
/// apart, and the 25-hour day fills all 100 slots.
#[test]
fn the_repeated_hour_of_a_dst_change_keeps_its_own_intervals() {
    let tz = chrono_tz::Europe::Amsterdam;
    let mut history = interval_ring();
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
    let mut history = interval_ring();
    history.record(&meter_event(utc(24, 22, 0), 50.0, 0.0));
    history.record(&meter_event(utc(24, 22, 20), 60.0, 0.0));

    let day = chrono::NaiveDate::from_ymd_opt(2026, 10, 25).unwrap();
    let slots = history.completed_on(day, tz, utc(24, 22, 20));

    assert_eq!(slots.len(), 1);
    assert_eq!(slots[0].averages.grid, Some(GridPower(50.0)));
}

#[test]
fn last_24h_is_96_intervals_ending_with_the_current_one() {
    let mut history = interval_ring();
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

    let mut live = interval_ring();
    events.iter().for_each(|event| live.record(event));
    let seeded = seed_interval_history(&path, at(1_000), configured());

    assert_ne!(live, interval_ring());
    assert_eq!(seeded, live);
}

#[test]
fn an_unreadable_journal_seeds_an_empty_history() {
    let dir = tempfile::tempdir().unwrap();
    let seeded = seed_interval_history(&dir.path().join("missing.db"), at(0), configured());
    assert_eq!(seeded, interval_ring());
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
    let mut history = interval_ring();
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
    let slots = interval_ring().last_24h(into_interval(base(), 30));
    let hours = hourly(&slots);

    assert_eq!(hours[0].index, slots[0].index.hour_start());
    assert_eq!(hours[hours.len() - 1].index, base().hour_start());
    assert!(hours.iter().all(|h| h.index == h.index.hour_start()));
}

// --- Packs -------------------------------------------------------------------

fn pack(serial: &str) -> PackKey {
    PackKey {
        device: DeviceId::new(journey::BATTERY_ID),
        pack: PackId::Serial(serial.to_string()),
    }
}

fn polls() -> Vec<(Timestamp, String)> {
    let poll = |at, soc, power, temp| (at, poll_body(journey::BATTERY_ID, soc, power, temp));
    vec![
        poll(into_interval(base(), 0), 50, 600, 2981),
        poll(into_interval(base(), 60), 51, 600, 2991),
        poll(into_interval(base(), 120), 52, 0, 2986),
        poll(into_interval(base().offset(1), 0), 53, 300, 2990),
    ]
}

fn rows(polls: &[(Timestamp, String)]) -> Vec<(Timestamp, &str)> {
    polls
        .iter()
        .map(|(at, body)| (*at, body.as_str()))
        .collect()
}

fn live_fold(polls: &[(Timestamp, String)]) -> IntervalHistory {
    let mut history = interval_ring();
    for (at, body) in polls {
        let packs = crate::zendure::polled_packs(body).expect("a complete pack list");
        history.record_packs(&DeviceId::new(journey::BATTERY_ID), *at, &packs);
    }
    history
}

async fn journal_of(
    dir: &tempfile::TempDir,
    name: &str,
    polls: &[(Timestamp, &str)],
) -> std::path::PathBuf {
    let path = dir.path().join(name);
    let captures: Vec<(Timestamp, &'static str, &str)> = polls
        .iter()
        .map(|&(at, body)| (at, crate::zendure::POLL_CAPTURE, body))
        .collect();
    crate::journal::testing::record_raw(&path, &captures).await;
    path
}

fn assert_energy(actual: WattHours, expected: f64) {
    assert!(
        (actual.get() - expected).abs() < 1e-9,
        "{actual} Wh, expected {expected} Wh"
    );
}

#[test]
fn a_pack_ranges_over_its_levels_and_integrates_its_flow() {
    let history = live_fold(&polls());
    let first = history.packs(base());

    let charging = first[&pack("P1")];
    assert_eq!(
        charging.soc,
        Some(Extent {
            min: Soc::new(50),
            max: Soc::new(52)
        })
    );
    assert_eq!(
        charging.temp,
        Some(Extent {
            min: DeciKelvin(2981),
            max: DeciKelvin(2991)
        })
    );
    // 600 W for a minute, then a minute ramping down to nothing: 10 + 5 Wh.
    assert_energy(charging.charged, 15.0);
    assert_energy(charging.discharged, 0.0);

    let discharging = first[&pack("P2")];
    assert_energy(discharging.discharged, 15.0);
    assert_energy(discharging.charged, 0.0);
}

#[test]
fn a_gap_wider_than_an_interval_integrates_nothing() {
    let mut history = interval_ring();
    let id = DeviceId::new(journey::BATTERY_ID);
    let report = |soc| {
        crate::zendure::polled_packs(&poll_body(journey::BATTERY_ID, soc, 600, 2981)).unwrap()
    };
    history.record_packs(&id, into_interval(base(), 0), &report(50));
    history.record_packs(&id, into_interval(base().offset(2), 0), &report(51));

    assert_energy(history.packs(base().offset(2))[&pack("P1")].charged, 0.0);
}

#[test]
fn packs_last_24h_lines_up_with_the_flow_slots() {
    let history = live_fold(&polls());
    let now = into_interval(base().offset(1), 30);

    let series = history.packs_last_24h(now);
    let flows = history.last_24h(now);

    assert_eq!(
        series.keys().cloned().collect::<Vec<_>>(),
        vec![pack("P1"), pack("P2")]
    );
    let p1 = &series[&pack("P1")];
    assert_eq!(
        p1.iter().map(|slot| slot.index).collect::<Vec<_>>(),
        flows.iter().map(|slot| slot.index).collect::<Vec<_>>()
    );
    assert_eq!(p1[0].figures, None);

    let day = PackInterval::combined(p1.iter().filter_map(|slot| slot.figures)).unwrap();
    assert_eq!(
        day.soc,
        Some(Extent {
            min: Soc::new(50),
            max: Soc::new(53)
        })
    );
    // Then 0 W rising to 300 W over the 13 minutes into the next interval.
    assert_energy(day.charged, 15.0 + 150.0 * 13.0 / 60.0);
}

/// A restart must show the pack figures the process it replaced was showing.
#[tokio::test]
async fn the_seed_from_poll_captures_equals_the_live_fold() {
    let dir = tempfile::tempdir().unwrap();
    let polls = polls();
    let path = journal_of(&dir, "journal.db", &rows(&polls)).await;

    let seeded = seed_interval_history(&path, at(10_000), configured());

    assert_ne!(seeded, interval_ring());
    assert_eq!(seeded, live_fold(&polls));
}

#[tokio::test]
async fn a_capture_that_no_longer_parses_is_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let polls = polls();
    let mut broken = rows(&polls);
    broken.insert(1, (into_interval(base(), 30), r#"{"electricLevel": 4"#));
    broken.insert(2, (into_interval(base(), 40), r#"{"sn":7}"#));

    let clean = journal_of(&dir, "clean.db", &rows(&polls)).await;
    let broken = journal_of(&dir, "broken.db", &broken).await;

    assert_eq!(
        seed_interval_history(&broken, at(10_000), configured()),
        seed_interval_history(&clean, at(10_000), configured()),
    );
}

#[tokio::test]
async fn a_capture_from_a_battery_that_is_not_configured_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let polls = polls();
    let foreign = poll_body("retired-battery", 5, 900, 3100);
    let mut mixed = rows(&polls);
    mixed.insert(1, (into_interval(base(), 30), foreign.as_str()));

    let clean = journal_of(&dir, "clean.db", &rows(&polls)).await;
    let mixed = journal_of(&dir, "mixed.db", &mixed).await;

    let seeded = seed_interval_history(&mixed, at(10_000), configured());
    assert_eq!(
        seeded,
        seed_interval_history(&clean, at(10_000), configured())
    );
}
