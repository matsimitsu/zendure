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

/// A `now` after every slot in `window` has finished.
fn done(window: &[IntervalSlot]) -> Timestamp {
    window
        .last()
        .map_or(utc(8, 12, 0), |slot| slot.index.offset(1).start())
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

    assert_eq!(energy(&window, done(&window), home), WattHours(400.0));
}

#[test]
fn the_interval_in_progress_counts_only_the_part_that_has_elapsed() {
    let window = slots(&[Some(Watts(400)), Some(Watts(1200))]);
    let five_minutes_in = window[1].index.start() + Elapsed::of(Duration::from_secs(5 * 60));

    assert_eq!(energy(&window, five_minutes_in, home), WattHours(200.0));
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
    assert_eq!(energy(&window, done(&window), home), WattHours::ZERO);
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

    let (imported, exported) = (
        imported(&window, done(&window)),
        exported(&window, done(&window)),
    );
    assert_eq!(imported, WattHours(200.0));
    assert_eq!(exported, WattHours(100.0));
    assert_eq!(kwh(imported - exported), "0.1 kWh");
}

// --- The battery ---------------------------------------------------------------

use std::time::Duration;

use crate::battery::BatteryState;
use crate::engine::EngineState;
use crate::fixtures::journey;
use crate::units::{Elapsed, Timestamp};
use crate::web::intervals::IntervalHistory;
use crate::web::state::ActualSolarHistory;
use crate::web::templates::detail_view;
use crate::web::view::detail_view as detail_view_model;
use crate::world::{DeviceId, Measurement, World};

fn limits(min: u32, max: u32) -> SocLimits {
    SocLimits {
        min: Soc::new(min),
        max: Soc::new(max),
        balance_day: false,
    }
}

fn engine(battery: Option<Soc>) -> EngineState {
    let mut world = World::new();
    if let Some(soc) = battery {
        world.observe_device(
            DeviceId::new(journey::BATTERY_ID),
            Timestamp::from_millis(0),
            Measurement::Battery(BatteryState {
                soc,
                ..BatteryState::test_sample()
            }),
        );
    }
    EngineState {
        world,
        controller: crate::controller::Controller::test_default(journey::NOW_MS, journey::DAY)
            .state(),
        mqtt_timed_out: false,
    }
}

fn pack(soc: u32, temp: u32) -> PackStatus {
    PackStatus {
        model: None,
        serial: Some("P1".to_string()),
        capacity: WattHours(1920.0),
        soc: Some(Soc::new(soc)),
        power: Some(BatteryPower(-600)),
        temp: Some(DeciKelvin(temp)),
    }
}

/// Starts five minutes into an interval, so the readings ten minutes apart
/// land in two buckets.
fn after(minutes: u64) -> Timestamp {
    let start = IntervalIndex::containing(Timestamp::from_millis(journey::NOW_MS)).start();
    start + Elapsed::of(Duration::from_secs((5 + minutes) * 60))
}

/// A battery that charged at 800 W for one bucket and discharged at 400 W
/// for the next, with one pack charging at 600 W across both.
fn battery_dashboard() -> DashboardState {
    let device = DeviceId::new(journey::BATTERY_ID);
    let mut intervals = IntervalHistory::new([device.clone()]);
    intervals.record(&journey::battery_event(
        after(0),
        BatteryPower(-800),
        Soc::new(40),
    ));
    intervals.record(&journey::battery_event(
        after(20),
        BatteryPower(400),
        Soc::new(50),
    ));
    for (minutes, soc, temp) in [(0, 40, 2981), (10, 45, 2991), (20, 50, 3001)] {
        intervals.record_packs(&device, after(minutes), &[pack(soc, temp)]);
    }
    let mut state = DashboardState::seed(
        &engine(Some(Soc::new(50))),
        vec![],
        ActualSolarHistory::default(),
        intervals,
        after(20),
    );
    state.soc_limits = limits(10, 95);
    state
}

fn battery_body(state: &DashboardState) -> DetailBodyView {
    detail_body(state, Entity::Battery, chrono_tz::UTC).expect("the battery has reported")
}

#[test]
fn the_battery_stats_cover_its_charge_discharge_and_soc_range() {
    let body = battery_body(&battery_dashboard());

    let stats: Vec<(&str, &str)> = body
        .stats
        .iter()
        .map(|s| (s.label, s.value.as_str()))
        .collect();
    assert_eq!(
        stats,
        vec![
            ("State of charge", "50%"),
            ("Charged", "0.2 kWh"),
            ("Discharged", "0.1 kWh"),
            ("24h range", "40–50%"),
        ]
    );
}

#[test]
fn a_pack_row_totals_its_figures_over_the_whole_window() {
    let state = battery_dashboard();
    let series = state.intervals.packs_last_24h(state.as_of);
    let slots = series.values().next().expect("one pack");
    assert!(
        slots.iter().filter(|slot| slot.figures.is_some()).count() > 1,
        "the readings span more than one bucket"
    );

    let body = battery_body(&state);
    let [row] = body.packs.as_slice() else {
        panic!("one row per pack");
    };
    assert_eq!(row.name, "Pack 1");
    assert_eq!(row.serial, "P1");
    assert_eq!(row.soc_range, "40–50%");
    assert_eq!(row.charged, "0.2 kWh", "600 W for 20 minutes");
    assert_eq!(row.discharged, "0.0 kWh");
    assert_eq!(row.temp_range, "25–27 °C");
}

#[test]
fn a_pack_is_named_by_the_model_currently_reporting_its_serial() {
    let mut state = battery_dashboard();
    state.packs = vec![
        PackStatus {
            serial: Some("P0".to_string()),
            ..pack(50, 3001)
        },
        PackStatus {
            model: Some("AB3000X"),
            ..pack(50, 3001)
        },
    ];

    assert_eq!(battery_body(&state).packs[0].name, "AB3000X");
    state.packs[1].model = None;
    assert_eq!(battery_body(&state).packs[0].name, "Pack 2");
}

#[test]
fn the_soc_chart_dashes_the_limits_and_shades_outside_them() {
    let body = battery_body(&battery_dashboard());

    let [soc, power] = body.charts.as_slice() else {
        panic!("an SOC chart and a power chart");
    };
    assert_eq!(soc.series, Entity::Battery);
    assert_eq!(soc.note.as_deref(), Some("dashed: limits 10% / 95%"));
    assert_eq!(soc.limit_lines.len(), 2);
    assert_eq!(soc.bands.len(), 2);
    assert_eq!(power.note.as_deref(), Some("+ discharge · − charge"));
    assert!(power.limit_lines.is_empty() && power.bands.is_empty());
}

#[test]
fn a_limit_at_the_edge_of_the_scale_leaves_no_line_or_band() {
    let spec = soc_chart(limits(0, 95));

    assert_eq!(spec.limits, vec![Soc::new(95)]);
    assert_eq!(spec.bands, vec![(Soc::new(95), Soc::FULL)]);
    assert_eq!(spec.note.as_deref(), Some("dashed: limits 0% / 95%"));
}

/// The defaults before any limits are known: nothing to dash, so nothing to
/// note either.
#[test]
fn unknown_limits_draw_no_window() {
    let spec = soc_chart(limits(0, 100));

    assert!(spec.limits.is_empty());
    assert!(spec.bands.is_empty());
    assert_eq!(spec.note, None);
}

#[test]
fn the_battery_detail_renders_stats_packs_and_both_charts() {
    let state = battery_dashboard();
    let html = detail_view::render(&detail_view_model(&state, Entity::Battery, chrono_tz::UTC))
        .into_string();

    assert!(html.contains("State of charge"), "{html}");
    assert!(html.contains("class=\"pack-table\""), "{html}");
    assert!(html.contains("Temp range"), "{html}");
    assert_eq!(html.matches("line-chart--battery").count(), 2, "{html}");
    assert_eq!(html.matches("class=\"line-chart__band\"").count(), 2);
    assert_eq!(html.matches("class=\"line-chart__limit\"").count(), 2);
}

#[test]
fn a_battery_that_has_never_reported_shows_an_empty_state() {
    let state = DashboardState::seed(
        &engine(None),
        vec![],
        ActualSolarHistory::default(),
        journey::interval_ring(),
        after(0),
    );

    assert!(detail_body(&state, Entity::Battery, chrono_tz::UTC).is_none());
    let html = detail_view::render(&detail_view_model(&state, Entity::Battery, chrono_tz::UTC))
        .into_string();
    assert!(html.contains("detail-view__empty"), "{html}");
    assert!(!html.contains("line-chart"), "{html}");
}
