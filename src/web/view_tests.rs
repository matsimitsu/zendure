//! The dashboard's view layer, which is where every "the page said X while the
//! controller was doing Y" bug lives. All of it is pure: a `DashboardState`
//! goes in, formatted strings come out, so none of this needs a server.

use super::*;

use crate::battery::BatteryState;
use crate::engine::EngineState;
use crate::fixtures::journey;
use crate::models::ControlDecision;
use crate::units::{BatteryPower, GridPower, SolarPower, Watts};
use crate::web::sse::FRAGMENTS;
use crate::web::state::DashboardState;
use crate::web::templates::layout;
use crate::world::{DeviceId, Measurement, MeterReading, World};

fn tz() -> chrono_tz::Tz {
    chrono_tz::Europe::Amsterdam
}

fn at(secs: i64) -> Timestamp {
    Timestamp::from_millis(journey::NOW_MS + secs * 1000)
}

fn clock(secs: i64) -> crate::clock::Clock {
    journey::clock_at(secs)
}

/// A world with one battery and a meter reading, which is what the battery
/// panel and the stat cards need before they render anything at all.
fn engine_state(battery_power: BatteryPower) -> EngineState {
    let mut world = World::new();
    world.observe_meter(
        MeterReading::total_only(GridPower(400.0)),
        SolarPower::new(750.0),
    );
    world.observe_device(
        DeviceId::new(journey::BATTERY_ID),
        Measurement::Battery(BatteryState {
            current_power: battery_power,
            ..BatteryState::test_sample()
        }),
    );

    EngineState {
        world,
        controller: crate::controller::Controller::test_default(journey::NOW_MS, journey::DAY)
            .state(),
        mqtt_timed_out: false,
    }
}

fn state(history: Vec<(Timestamp, ControlDecision)>) -> DashboardState {
    DashboardState::seed(
        &engine_state(BatteryPower::ZERO),
        history,
        crate::web::ActualSolarHistory::default(),
        at(0),
    )
}

fn decision(mode: ControlMode, reason: &str) -> ControlDecision {
    ControlDecision {
        mode,
        reason: reason.to_string(),
        ..ControlDecision::test_sample()
    }
}

// --- The page and the stream have to agree -----------------------------------

/// Every element carrying `sse-swap` must name a fragment the stream emits,
/// and every fragment must land somewhere on the page. A live section that is
/// wired into one but not the other freezes in the browser without failing.
#[test]
fn page_is_live_everywhere_it_claims_to_be() {
    let view = dashboard_view(&state(vec![]), tz());
    let html = layout::page(&view).into_string();

    let mut on_page: Vec<String> = html
        .split("sse-swap=\"")
        .skip(1)
        .filter_map(|rest| rest.split('"').next().map(str::to_string))
        .collect();
    on_page.sort();

    let mut on_stream: Vec<String> = FRAGMENTS.iter().map(|(name, _)| name.to_string()).collect();
    on_stream.sort();

    assert_eq!(on_page, on_stream);
    assert!(!on_page.is_empty(), "the page swapped nothing at all");
}

/// Each fragment replaces the *contents* of its wrapper, so the payload must
/// not repeat the wrapper — htmx would nest a copy inside the original on
/// every tick rather than replacing it.
#[test]
fn no_fragment_repeats_its_own_sse_swap_wrapper() {
    let view = dashboard_view(&state(vec![]), tz());

    for (name, render) in FRAGMENTS {
        let fragment = render(&view).into_string();
        assert!(
            !fragment.contains("sse-swap="),
            "fragment `{name}` carries an sse-swap wrapper: {fragment}"
        );
    }
}

// --- The battery badge is a claim about *now* --------------------------------

/// The badge must not speak for a decision this process did not make: the
/// seeded log's newest row can be from last night.
#[test]
fn the_mode_badge_awaits_a_decision_rather_than_inheriting_a_journalled_one() {
    let seeded = state(vec![(
        at(-50_000),
        decision(ControlMode::Discharge, "yesterday evening"),
    )]);
    let view = dashboard_view(&seeded, tz());
    let battery = view.battery.expect("the fixture world has a battery");

    assert_eq!(battery.mode_label, "Awaiting decision");
    assert_eq!(battery.badge_variant, "idle");

    // The row itself is still shown — it is history, and history is the point
    // of seeding the log.
    assert_eq!(view.decision_log.len(), 1);
    assert_eq!(view.decision_log[0].mode_label, "DISCHARGE");
}

#[test]
fn the_mode_badge_follows_the_first_real_decision() {
    let mut charging = state(vec![(at(-50_000), decision(ControlMode::Idle, "old"))]);
    charging.meter_tick(
        &engine_state(BatteryPower(-1200)),
        Some((&decision(ControlMode::Charge, "solar surplus"), at(1))),
        &clock(1),
        tz(),
    );

    let battery = dashboard_view(&charging, tz())
        .battery
        .expect("the fixture world has a battery");
    assert_eq!(battery.mode_label, "Charging");
    assert_eq!(battery.badge_variant, "charge");
}

/// The variant colors the badge and the labels name it; a mode that drifts into
/// the wrong pairing shows a green "DISCHARGE".
#[test]
fn every_mode_pairs_one_variant_with_both_of_its_labels() {
    let cases = [
        (ControlMode::Charge, "charge", "CHARGE", "Charging"),
        (
            ControlMode::Discharge,
            "discharge",
            "DISCHARGE",
            "Discharging",
        ),
        (ControlMode::Idle, "idle", "IDLE", "Idle"),
        (ControlMode::Standby, "idle", "STANDBY", "Standby"),
    ];

    for (mode, variant, log_label, panel_label) in cases {
        let badge = badge(mode);
        assert_eq!(badge.variant, variant, "{mode:?}");
        assert_eq!(badge.log_label, log_label, "{mode:?}");
        assert_eq!(badge.panel_label, panel_label, "{mode:?}");
        assert_eq!(panel_badge(Some(mode)), (panel_label, variant), "{mode:?}");
    }
}

/// Standby is the one mode whose label and variant disagree: it is its own
/// state, but the badge has no color for it.
#[test]
fn standby_reads_as_itself_while_wearing_the_idle_variant() {
    assert_eq!(badge(ControlMode::Standby).variant, "idle");
    assert_ne!(
        badge(ControlMode::Standby).log_label,
        badge(ControlMode::Idle).log_label
    );
}

#[test]
fn no_decision_yet_is_its_own_panel_label() {
    assert_eq!(panel_badge(None), ("Awaiting decision", "idle"));
}

// --- Timestamps --------------------------------------------------------------

#[test]
fn a_log_row_from_today_shows_only_the_time() {
    let now = at(0);
    assert_eq!(format_log_time(at(-3600), now, tz()).len(), 5);
    assert!(!format_log_time(at(-3600), now, tz()).contains(' '));
}

/// A row from another day must not read as though it were minutes old.
#[test]
fn a_log_row_from_another_day_is_dated() {
    let now = at(0);
    let yesterday = format_log_time(at(-60 * 60 * 26), now, tz());

    assert!(
        yesterday.contains(' ') && yesterday.len() > 5,
        "expected a dated timestamp, got {yesterday}"
    );
    assert_ne!(yesterday, format_log_time(at(-60 * 60 * 2), now, tz()));
}

/// Day boundaries, not 24-hour windows: 23:50 yesterday is dated when read at
/// 00:10, even though it is twenty minutes old.
#[test]
fn dating_follows_the_calendar_day_in_the_configured_timezone() {
    use chrono::TimeZone;
    let midnight = tz().with_ymd_and_hms(2026, 9, 13, 0, 10, 0).unwrap();
    let now = Timestamp::from_millis(midnight.timestamp_millis());
    let twenty_minutes_ago = Timestamp::from_millis(midnight.timestamp_millis() - 20 * 60 * 1000);

    assert!(
        format_log_time(twenty_minutes_ago, now, tz()).contains(' '),
        "a row from the previous calendar day must be dated"
    );
}

// --- Formatting --------------------------------------------------------------

#[test]
fn thousands_groups_from_the_right() {
    assert_eq!(thousands(0), "0");
    assert_eq!(thousands(999), "999");
    assert_eq!(thousands(1_000), "1,000");
    assert_eq!(thousands(12_345), "12,345");
    assert_eq!(thousands(1_234_567), "1,234,567");
}

#[test]
fn the_sign_style_decides_what_a_negative_reads_as() {
    assert_eq!(format_watts(Watts(-1_234), SignStyle::Negative), "-1,234");
    assert_eq!(format_watts(Watts(-1_234), SignStyle::Explicit), "-1,234");
    assert_eq!(format_watts(Watts(-1_234), SignStyle::Magnitude), "1,234");
}

/// Zero has no direction, so no style may render it as a loss.
#[test]
fn zero_never_carries_a_minus() {
    assert_eq!(format_watts(Watts::ZERO, SignStyle::Negative), "0");
    assert_eq!(format_watts(Watts::ZERO, SignStyle::Explicit), "+0");
    assert_eq!(format_watts(Watts::ZERO, SignStyle::Magnitude), "0");
}

#[test]
fn a_positive_reads_the_same_as_its_magnitude_except_when_signed() {
    assert_eq!(format_watts(Watts(1_234), SignStyle::Negative), "1,234");
    assert_eq!(format_watts(Watts(1_234), SignStyle::Explicit), "+1,234");
    assert_eq!(format_watts(Watts(1_234), SignStyle::Magnitude), "1,234");
}

#[test]
fn millions_group_on_both_boundaries() {
    assert_eq!(
        format_watts(Watts(1_000_000), SignStyle::Negative),
        "1,000,000"
    );
    assert_eq!(
        format_watts(Watts(-2_345_678), SignStyle::Explicit),
        "-2,345,678"
    );
    assert_eq!(
        format_watts(Watts(i32::MIN), SignStyle::Negative),
        "-2,147,483,648"
    );
}

#[test]
fn a_missing_round_trip_efficiency_reads_as_unknown_not_as_zero() {
    assert_eq!(efficiency_string(None), "—");
    assert_eq!(efficiency_string(Some(Percent(91.4))), "91%");
}

/// An empty buffer draws nothing rather than a degenerate path, and a single
/// sample cannot divide by `len - 1`.
#[test]
fn a_sparkline_survives_having_too_few_samples_to_draw() {
    let mut spark = Sparkline::default();
    assert_eq!(sparkline_path(&spark), "");

    spark.push(GridPower(42.0));
    assert_eq!(sparkline_path(&spark), "M0.0,14.0 L96.0,14.0");

    spark.push(GridPower(43.0));
    let path = sparkline_path(&spark);
    assert!(path.starts_with('M') && path.contains('L'), "{path}");
    assert!(!path.contains("NaN") && !path.contains("inf"), "{path}");
}

/// A flat series has no range to normalise against; dividing by it would put
/// every point at infinity.
#[test]
fn a_flat_sparkline_does_not_divide_by_a_zero_range() {
    let mut spark = Sparkline::default();
    for _ in 0..10 {
        spark.push(GridPower(1500.0));
    }

    let path = sparkline_path(&spark);
    assert!(!path.contains("NaN") && !path.contains("inf"), "{path}");
}

// --- Stat cards --------------------------------------------------------------

/// The grid card's detail line is the one place the sign convention is spelled
/// out for the reader, so it has to match the sign it is describing.
#[test]
fn the_grid_card_names_the_direction_its_sign_means() {
    let importing = dashboard_view(&state(vec![]), tz());
    assert_eq!(importing.stat_cards[2].detail, "Importing from grid");

    let mut exporting_state = state(vec![]);
    exporting_state.engine.world.observe_meter(
        MeterReading::total_only(GridPower(-900.0)),
        SolarPower::ZERO,
    );
    let exporting = dashboard_view(&exporting_state, tz());
    assert_eq!(exporting.stat_cards[2].detail, "Exporting to grid");
}

/// A fraction of a watt is not an export worth a minus sign in front of a zero.
#[test]
fn a_sub_watt_grid_reading_reads_as_zero_without_a_sign() {
    let mut drifting = state(vec![]);
    drifting
        .engine
        .world
        .observe_meter(MeterReading::total_only(GridPower(-0.4)), SolarPower::ZERO);

    assert_eq!(dashboard_view(&drifting, tz()).stat_cards[2].value, "0");
}

// --- Forecast panel ----------------------------------------------------------

fn forecast_point(hours_after_midnight: i64, minutes: i64, watts: f64) -> SolarForecastPoint {
    SolarForecastPoint {
        at: Timestamp::from_millis(hours_after_midnight * 3_600_000 + minutes * 60_000),
        estimate: SolarPower::new(watts),
    }
}

/// Two samples landing in the same half-hour slot average — normally only
/// reachable with a misaligned or duplicated fetch, since Solcast's own
/// resolution is one sample per slot.
#[test]
fn bucketed_forecast_watts_averages_two_samples_in_the_same_slot() {
    let points = vec![forecast_point(6, 0, 1000.0), forecast_point(6, 10, 2000.0)];
    let buckets = bucketed_forecast_watts(&points, Timestamp::from_millis(0));

    assert_eq!(buckets[12], Some(1500.0), "06:00-06:30 is bucket 12");
    assert_eq!(buckets[13], None, "06:30-07:00 has no sample");
}

#[test]
fn bucketed_forecast_watts_excludes_points_outside_the_24h_window() {
    let today_start = Timestamp::from_millis(0);
    // One hour before today, and exactly at tomorrow's start.
    let points = vec![
        SolarForecastPoint {
            at: today_start - Elapsed::of(std::time::Duration::from_secs(3600)),
            estimate: SolarPower::new(500.0),
        },
        SolarForecastPoint {
            at: today_start + Elapsed::of(std::time::Duration::from_secs(24 * 3600)),
            estimate: SolarPower::new(500.0),
        },
    ];

    let buckets = bucketed_forecast_watts(&points, today_start);
    assert!(buckets.iter().all(Option::is_none));
}

#[test]
fn bucketed_forecast_watts_of_an_empty_series_is_all_none() {
    let buckets = bucketed_forecast_watts(&[], Timestamp::from_millis(0));
    assert!(buckets.iter().all(Option::is_none));
}

#[test]
fn forecast_panel_view_of_an_empty_snapshot_has_no_data() {
    let view = forecast_panel_view(
        &ForecastSnapshot::default(),
        &ActualSolarHistory::default(),
        at(0),
        tz(),
    );

    assert!(!view.has_data);
    assert!(view.bar_heights.iter().all(|&h| h == 0.0));
    assert!(view.line_path.is_empty());
}

/// A gap (a slot with no recorded actual sample) must start a new `M`
/// subpath rather than drawing a line straight across it.
#[test]
fn actual_line_path_starts_a_new_subpath_across_a_gap() {
    let mut buckets: [Option<f64>; 48] = [None; 48];
    buckets[12] = Some(1000.0);
    buckets[13] = Some(1200.0);
    // bucket 14 is a gap
    buckets[15] = Some(900.0);

    let path = actual_line_path(&buckets, 2000.0, 100.0);

    let subpaths: Vec<&str> = path.split('M').filter(|s| !s.is_empty()).collect();
    assert_eq!(subpaths.len(), 2, "expected two subpaths, got: {path}");
    assert!(
        subpaths[0].contains('L'),
        "the first subpath joins slots 12 and 13: {path}"
    );
    assert!(
        !subpaths[1].contains('L'),
        "a single-slot subpath has nothing to join: {path}"
    );
}

/// Home usage is the one stat card carrying real arithmetic rather than a
/// straight meter reading — 400 W still imported, 750 W of solar and 600 W out
/// of the pack is a house drawing 1750 W.
#[test]
fn the_home_card_reports_the_houses_own_draw() {
    let discharging = DashboardState::seed(
        &engine_state(BatteryPower(600)),
        vec![],
        ActualSolarHistory::default(),
        at(0),
    );
    let view = dashboard_view(&discharging, tz());

    assert_eq!(view.stat_cards[1].label, "Home usage");
    assert_eq!(view.stat_cards[1].value, "1,750");
}
