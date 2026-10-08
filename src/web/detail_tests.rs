//! The detail panels' summaries, taken from the interval buckets.

use super::*;

use crate::fixtures::utc;
use crate::units::GridPower;

fn slots(values: &[Option<Watts>]) -> Vec<IntervalSlot> {
    let first = IntervalIndex::containing(utc(8, 12, 0));
    values
        .iter()
        .enumerate()
        .map(|(i, home)| IntervalSlot {
            index: first.offset(i as i64),
            averages: IntervalAverages {
                home: *home,
                ..IntervalAverages::default()
            },
        })
        .collect()
}

fn home(a: &IntervalAverages) -> Option<Watts> {
    a.home
}

#[test]
fn the_peak_is_the_highest_bucket_and_names_its_interval() {
    let window = slots(&[Some(Watts(300)), Some(Watts(900)), None, Some(Watts(900))]);

    let peak = peak(&window, home).expect("a bucket has data");
    assert_eq!(peak.value, Watts(900));
    assert_eq!(peak.at, window[1].index, "the earlier of a tie");
    assert_eq!(peak_string(Some(peak), chrono_tz::UTC), "900 W · 12:15");
}

#[test]
fn energy_counts_each_bucket_as_a_quarter_hour_and_a_gap_as_nothing() {
    let window = slots(&[Some(Watts(400)), None, Some(Watts(1200))]);

    assert_eq!(energy(&window, home), WattHours(400.0));
}

#[test]
fn the_average_is_over_the_buckets_that_have_data() {
    let window = slots(&[Some(Watts(400)), None, Some(Watts(800))]);

    assert_eq!(average(&window, home), Some(Watts(600)));
}

#[test]
fn an_empty_window_has_no_peak_or_average_and_no_energy() {
    let window = slots(&[None, None]);

    assert_eq!(peak(&window, home), None);
    assert_eq!(average(&window, home), None);
    assert_eq!(energy(&window, home), WattHours::ZERO);
    assert_eq!(peak_string(None, chrono_tz::UTC), "—");
}

#[test]
fn import_and_export_split_a_grid_series_by_sign() {
    let window: Vec<IntervalSlot> = slots(&[None, None])
        .into_iter()
        .zip([GridPower(800.0), GridPower(-400.0)])
        .map(|(slot, grid)| IntervalSlot {
            averages: IntervalAverages {
                grid: Some(grid),
                ..slot.averages
            },
            ..slot
        })
        .collect();

    let (imported, exported) = (imported(&window), exported(&window));
    assert_eq!(imported, WattHours(200.0));
    assert_eq!(exported, WattHours(100.0));
    assert_eq!(kwh(imported - exported), "0.1 kWh");
}
