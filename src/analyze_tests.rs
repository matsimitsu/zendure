use super::*;

use crate::units::{BatteryPower, GridPower, PowerCap};
use crate::world::DeviceId;

/// 2026-09-13T10:00:00Z, whose local day-of-year in Amsterdam is 256.
const BASE_MS: i64 = 1_789_293_600_000;
const ORDINAL: u32 = 256;

fn clock(offset_secs: i64, ordinal: u32) -> Clock {
    Clock {
        day_ordinal: ordinal,
        ..Clock::test_at(BASE_MS + offset_secs * 1000)
    }
}

fn meter_at(offset_secs: i64, ordinal: u32, total: f64, phases: [f64; 3]) -> Event {
    Event::Meter {
        at: clock(offset_secs, ordinal),
        grid: MeterReading::new(GridPower(total), phases.map(GridPower)),
        solar: crate::units::SolarPower::new(0.0),
    }
}

/// Grid total only; the per-phase breakdown is irrelevant to most of these and
/// spelling out three zeroes everywhere obscures the number under test.
fn meter(offset_secs: i64, total: f64) -> Event {
    meter_at(offset_secs, ORDINAL, total, [0.0, 0.0, 0.0])
}

fn battery(offset_secs: i64, current_power: i32, soc: u32) -> Event {
    Event::DeviceUpdate {
        at: clock(offset_secs, ORDINAL),
        id: DeviceId::new("battery-a"),
        measurement: Measurement::Battery(BatteryState {
            soc: Soc::new(soc),
            max_discharge_power: PowerCap::new(800),
            max_charge_power: PowerCap::new(2400),
            current_power: BatteryPower(current_power),
            soc_calibrating: false,
            soc_limit_reached: false,
            fault: false,
        }),
    }
}

fn assert_wh(actual: WattHours, expected: f64) {
    assert!(
        (actual.get() - expected).abs() < 0.01,
        "expected {expected} Wh, got {} Wh",
        actual.get()
    );
}

/// Watts times hours: 1 kW held across two 30 s steps is 1000 * (60/3600) Wh.
#[test]
fn constant_power_integrates_to_energy() {
    let days = daily(&[meter(0, 1000.0), meter(30, 1000.0), meter(60, 1000.0)]);

    assert_eq!(days.len(), 1);
    assert_wh(days[0].import, 1000.0 * 60.0 / 3600.0);
    assert_eq!(days[0].export, WattHours::ZERO);
    assert_eq!(days[0].covered, Duration::from_secs(60));
}

/// Export is the same integral on the other side of zero, and the two never
/// both run: a reading is one or the other.
#[test]
fn export_is_counted_separately_from_import() {
    let days = daily(&[meter(0, -600.0), meter(30, -600.0)]);

    assert_wh(days[0].export, 600.0 * 30.0 / 3600.0);
    assert_eq!(days[0].import, WattHours::ZERO);
}

/// The property that matters most here. A hole in the journal — a restart, a
/// prune, a dropped write — must not be integrated across as though the power
/// at its edges had held throughout, and the day must say its totals are short.
#[test]
fn a_gap_is_skipped_rather_than_integrated_across() {
    let hour = 3600;
    let days = daily(&[
        meter(0, 1000.0),
        meter(30, 1000.0),
        // Nothing for an hour, then the same power again.
        meter(30 + hour, 1000.0),
        meter(60 + hour, 1000.0),
    ]);

    // Only the two 30 s intervals counted; the hour between them did not.
    assert_wh(days[0].import, 1000.0 * 60.0 / 3600.0);
    assert_eq!(days[0].covered, Duration::from_secs(60));
    assert!(
        days[0].coverage() < 0.05,
        "an hour-long hole should leave coverage obviously short, got {}",
        days[0].coverage()
    );
}

/// Wall-clock timestamps can step backwards under NTP, which would otherwise
/// subtract energy from a day.
#[test]
fn a_backwards_step_contributes_nothing() {
    let days = daily(&[meter(60, 1000.0), meter(0, 1000.0)]);

    assert_eq!(days[0].import, WattHours::ZERO);
    assert_eq!(days[0].covered, Duration::ZERO);
}

/// `unstored` is the headline: surplus that reached the grid because the
/// battery was not taking it. While the battery charges, export is not
/// unstored, however much of it there is.
#[test]
fn export_while_charging_is_not_unstored() {
    let days = daily(&[battery(0, -500, 50), meter(0, -900.0), meter(30, -900.0)]);

    assert_wh(days[0].export, 900.0 * 30.0 / 3600.0);
    assert_eq!(days[0].unstored_export, WattHours::ZERO);
}

#[test]
fn export_while_idle_is_unstored() {
    let days = daily(&[battery(0, 0, 80), meter(0, -900.0), meter(30, -900.0)]);

    assert_wh(days[0].unstored_export, 900.0 * 30.0 / 3600.0);
}

/// Before the first `device_update` there is no battery flow to correct for, so
/// nothing that depends on one is counted — rather than reading an absent
/// battery as an idle one.
#[test]
fn readings_before_the_first_poll_contribute_no_battery_figures() {
    let days = daily(&[meter(0, -900.0), meter(30, -900.0)]);

    assert_wh(days[0].export, 900.0 * 30.0 / 3600.0);
    assert_eq!(days[0].unstored_export, WattHours::ZERO);
    assert_eq!(days[0].above_cap, WattHours::ZERO);
    assert_eq!(days[0].soc_min, None);
}

/// `>cap` measures the house against the inverter's own reported ceiling, on
/// the grid *underlying* the battery — 1500 W of demand while the battery
/// already supplies 800 leaves 700 above an 800 W cap.
#[test]
fn above_cap_measures_underlying_demand_against_the_reported_limit() {
    let days = daily(&[
        battery(0, 800, 50),
        // Meter shows 700 W still coming from the grid; underlying demand is
        // 700 + 800 = 1500 W, which is 700 W above the 800 W cap.
        meter(0, 700.0),
        meter(30, 700.0),
    ]);

    assert_wh(days[0].above_cap, 700.0 * 30.0 / 3600.0);
}

#[test]
fn demand_within_the_cap_is_not_counted_above_it() {
    let days = daily(&[battery(0, 300, 50), meter(0, 100.0), meter(30, 100.0)]);

    assert_eq!(days[0].above_cap, WattHours::ZERO);
}

/// A poll that flips the battery from charging to discharging is blended
/// across the interval it lands in, not attributed whole to either end: the
/// first 30 s is a flat 1200 W charge, the second ramps 1200 → 0 charging while
/// discharge ramps 0 → 600.
#[test]
fn charge_and_discharge_are_integrated_from_the_polled_flow() {
    let days = daily(&[
        battery(0, -1200, 40),
        meter(0, 0.0),
        meter(30, 0.0),
        battery(30, 600, 45),
        meter(60, 0.0),
    ]);

    let step = 30.0 / 3600.0;
    assert_wh(days[0].charged, 1200.0 * step + (1200.0 + 0.0) / 2.0 * step);
    assert_wh(days[0].discharged, (0.0 + 600.0) / 2.0 * step);
}

#[test]
fn soc_range_spans_what_was_observed() {
    let days = daily(&[
        battery(0, 0, 40),
        meter(0, 0.0),
        battery(30, 0, 80),
        meter(30, 0.0),
        battery(60, 0, 12),
        meter(60, 0.0),
    ]);

    assert_eq!(days[0].soc_min, Some(Soc::new(12)));
    assert_eq!(days[0].soc_max, Some(Soc::new(80)));
}

#[test]
fn phases_are_integrated_independently() {
    let days = daily(&[
        meter_at(0, ORDINAL, 0.0, [-900.0, 500.0, 400.0]),
        meter_at(30, ORDINAL, 0.0, [-900.0, 500.0, 400.0]),
    ]);

    let [a, b, c] = days[0].phases;
    assert_wh(a.export, 900.0 * 30.0 / 3600.0);
    assert_eq!(a.import, WattHours::ZERO);
    assert_wh(b.import, 500.0 * 30.0 / 3600.0);
    assert_wh(c.import, 400.0 * 30.0 / 3600.0);
}

#[test]
fn each_local_day_gets_its_own_row() {
    let days = daily(&[
        meter_at(0, ORDINAL, 1000.0, [0.0; 3]),
        meter_at(30, ORDINAL, 1000.0, [0.0; 3]),
        meter_at(86_400, ORDINAL + 1, 1000.0, [0.0; 3]),
        meter_at(86_430, ORDINAL + 1, 1000.0, [0.0; 3]),
    ]);

    assert_eq!(days.len(), 2);
    assert_eq!(days[0].day, NaiveDate::from_ymd_opt(2026, 9, 13).unwrap());
    assert_eq!(days[1].day, NaiveDate::from_ymd_opt(2026, 9, 14).unwrap());
}

/// `Clock` carries a local day-of-year and no year, so the turn of the year is
/// where pairing it with the UTC year goes wrong: half an hour into 1 January
/// in Amsterdam it is still 31 December in UTC.
#[test]
fn a_local_new_year_is_dated_in_the_year_it_is_local_to() {
    // 2026-12-31T23:30:00Z is 2027-01-01T00:30 in Amsterdam: ordinal 1.
    let new_year_eve_utc = 1_798_759_800_000;
    let at = Clock {
        day_ordinal: 1,
        ..Clock::test_at(new_year_eve_utc)
    };

    assert_eq!(
        local_date(&at),
        NaiveDate::from_ymd_opt(2027, 1, 1),
        "ordinal 1 belongs to the new year, not the UTC year it is still in"
    );
}

#[test]
fn an_empty_range_renders_as_such() {
    assert!(render(&[]).contains("no meter readings"));
}

/// The rendered table is what a human actually reads, so the numbers have to
/// survive the trip through it.
#[test]
fn the_table_carries_the_figures_it_computed() {
    let days = daily(&[battery(0, 0, 80), meter(0, -3600.0), meter(30, -3600.0)]);
    let table = render(&days);

    assert!(table.contains("2026-09-13"), "{table}");
    // 3600 W for 30 s is 30 Wh, i.e. 0.03 kWh, in both export and unstored.
    assert!(table.contains("0.03"), "{table}");
    assert!(table.contains("80-80%"), "{table}");
}
