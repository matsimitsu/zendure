//! The dashboard's view layer, which is where every "the page said X while the
//! controller was doing Y" bug lives. All of it is pure: a `DashboardState`
//! goes in, formatted strings come out, so none of this needs a server.

use super::*;

use crate::battery::BatteryState;
use crate::controller::SocLimits;
use crate::device::PackStatus;
use crate::engine::EngineState;
use crate::fixtures::journey;
use crate::models::ControlDecision;
use crate::units::{BatteryPower, GridPower, Soc, SolarPower, Timestamp, Watts};
use crate::web::sse::{FRAGMENTS, SentFragments};
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
        None,
        MeterReading::total_only(GridPower(400.0)),
        SolarPower::new(750.0),
    );
    world.observe_device(
        DeviceId::new(journey::BATTERY_ID),
        Timestamp::from_millis(0),
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
        crate::web::state::ActualSolarHistory::default(),
        journey::interval_ring(),
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

// --- What the browser does with the page -------------------------------------

/// One start tag in rendered markup: its name and its raw attribute text.
#[derive(Clone, Copy)]
struct StartTag<'a> {
    name: &'a str,
    attrs: &'a str,
}

impl StartTag<'_> {
    fn attr(&self, name: &str) -> Option<&str> {
        let needle = format!(" {name}=\"");
        let start = self.attrs.find(&needle)? + needle.len();
        self.attrs[start..].split('"').next()
    }
}

/// Every start tag in `html`, each with the start tags still open around it,
/// outermost first. Maud's output is well-formed, so a stack is enough.
fn start_tags_with_ancestors(html: &str) -> Vec<(StartTag<'_>, Vec<StartTag<'_>>)> {
    const VOID: [&str; 6] = ["meta", "link", "br", "img", "input", "hr"];
    let mut open: Vec<StartTag> = Vec::new();
    let mut found = Vec::new();
    for chunk in html.split('<').skip(1) {
        let Some((tag, _)) = chunk.split_once('>') else {
            continue;
        };
        if let Some(closing) = tag.strip_prefix('/') {
            let position = open.iter().rposition(|t| t.name == closing);
            open.truncate(position.expect("a closing tag matches an open one"));
            continue;
        }
        if tag.starts_with('!') {
            continue;
        }
        let name_end = tag.find([' ', '/']).unwrap_or(tag.len());
        let start = StartTag {
            name: &tag[..name_end],
            attrs: &tag[name_end..],
        };
        found.push((start, open.clone()));
        if !VOID.contains(&start.name) && !tag.ends_with('/') {
            open.push(start);
        }
    }
    found
}

/// The SSE extension swaps into `hx-target` resolved with inheritance, so a
/// live region inside a link that opens the modal would swap each tick into
/// the modal instead of itself.
#[test]
fn no_sse_swap_inherits_an_hx_target_from_an_ancestor() {
    let view = dashboard_view(&state(vec![]), tz());
    let html = layout::page(&view).into_string();

    let mut inside_a_target = 0;
    for (tag, ancestors) in start_tags_with_ancestors(&html) {
        let Some(swap) = tag.attr("sse-swap") else {
            continue;
        };
        if !ancestors.iter().any(|a| a.attr("hx-target").is_some()) {
            continue;
        }
        inside_a_target += 1;
        assert_eq!(
            tag.attr("hx-target"),
            Some("this"),
            "`{swap}` inherits an hx-target from an ancestor"
        );
    }
    assert!(
        inside_a_target > 0,
        "no live region sits inside a link, so this test checked nothing"
    );
}

/// The app's own scripts as the page loads them: `(is_module, file name)`.
fn app_script_tags(html: &str) -> Vec<(bool, String)> {
    const VENDORED: [&str; 2] = ["/assets/htmx.min.js", "/assets/sse.js"];
    start_tags_with_ancestors(html)
        .into_iter()
        .filter(|(tag, _)| tag.name == "script")
        .filter_map(|(tag, _)| {
            let src = tag.attr("src")?;
            if VENDORED.contains(&src) {
                return None;
            }
            let name = src.strip_prefix("/assets/")?.to_string();
            Some((tag.attr("type") == Some("module"), name))
        })
        .collect()
}

/// Classic scripts share one global lexical scope, so two files declaring the
/// same top-level `const` throw and the second element is never defined.
#[test]
fn every_app_script_loads_as_a_module_with_its_own_scope() {
    let html = layout::page(&dashboard_view(&state(vec![]), tz())).into_string();
    let scripts = app_script_tags(&html);

    let names: Vec<&str> = scripts.iter().map(|(_, name)| name.as_str()).collect();
    assert!(names.contains(&"energy-flows.js"), "{names:?}");
    assert!(names.contains(&"line-chart.js"), "{names:?}");
    for (module, name) in &scripts {
        assert!(module, "{name} loads as a classic script");
    }
}

/// Loads every app script into one realm the way the page declares it (a
/// module import, or a classic script sharing the global scope) and checks
/// each defined its element. Skips when `node` is not installed.
#[test]
fn every_app_script_defines_its_element_when_loaded_together() {
    const HARNESS: &str = r#"
        import { readFileSync } from "node:fs";
        import vm from "node:vm";
        const defined = [];
        globalThis.HTMLElement = class {};
        globalThis.customElements = { define: (name) => defined.push(name) };
        for (const arg of process.argv.slice(1)) {
          const split = arg.indexOf(":");
          const [kind, path] = [arg.slice(0, split), arg.slice(split + 1)];
          const source = readFileSync(path, "utf8");
          if (kind === "module") {
            await import("data:text/javascript," + encodeURIComponent(source));
          } else {
            vm.runInThisContext(source, { filename: path });
          }
        }
        console.log(defined.join(","));
    "#;

    let html = layout::page(&dashboard_view(&state(vec![]), tz())).into_string();
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/js");
    let args = app_script_tags(&html).into_iter().map(|(module, name)| {
        let kind = if module { "module" } else { "classic" };
        format!("{kind}:{}", dir.join(name).display())
    });

    let output = match std::process::Command::new("node")
        .args(["--input-type=module", "-e", HARNESS])
        .args(args)
        .output()
    {
        Ok(output) => output,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("skipped: node is not installed");
            return;
        }
        Err(e) => panic!("failed to run node: {e}"),
    };
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut defined: Vec<&str> = stdout.trim().split(',').collect();
    defined.sort_unstable();
    assert_eq!(defined, ["energy-flows", "line-chart"]);
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
    assert!(view.decision_log[0].repeat.is_none());
}

/// A collapsed run has to say so on the page: without the marker the log
/// would look like a single decision and the minutes it covered would vanish.
#[test]
fn a_collapsed_run_renders_its_repeat_count_and_the_span_it_covers() {
    let mut state = state(vec![]);
    for secs in [-10, -5, 0] {
        state.failsafe_tick(
            &engine_state(BatteryPower::ZERO),
            Some((&decision(ControlMode::Idle, "nothing to do"), at(secs))),
            at(secs),
            SocLimits::default(),
        );
    }

    let view = dashboard_view(&state, tz());
    assert_eq!(view.decision_log.len(), 1);
    let repeat = view.decision_log[0]
        .repeat
        .as_ref()
        .expect("three identical commands collapsed into one row");
    assert_eq!(repeat.label, "×3");
    assert!(
        repeat.span.starts_with("3 identical decisions since "),
        "unexpected span text: {}",
        repeat.span
    );

    let html = layout::decision_log_inner(&view).into_string();
    assert!(
        html.contains(r#"<span class="decision-log__repeat" title="#),
        "the repeat marker never reached the markup: {html}"
    );
    assert!(html.contains("×3"));
}

#[test]
fn the_mode_badge_follows_the_first_real_decision() {
    let mut charging = state(vec![(at(-50_000), decision(ControlMode::Idle, "old"))]);
    charging.meter_tick(
        &engine_state(BatteryPower(-1200)),
        &journey::meter_event(at(1), 400.0, 750.0),
        Some((&decision(ControlMode::Charge, "solar surplus"), at(1))),
        &clock(1),
        tz(),
        SocLimits::default(),
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
    assert_eq!(format_watts(Watts(-1_234), SignStyle::Negative), "−1,234");
    assert_eq!(format_watts(Watts(-1_234), SignStyle::Explicit), "−1,234");
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
        "−2,345,678"
    );
    assert_eq!(
        format_watts(Watts(i32::MIN), SignStyle::Negative),
        "−2,147,483,648"
    );
}

#[test]
fn a_missing_round_trip_efficiency_reads_as_unknown_not_as_zero() {
    assert_eq!(efficiency_string(None), "—");
    assert_eq!(efficiency_string(Some(Percent(91.4))), "91%");
}

// --- Per-pack rows ---------------------------------------------------------

fn pack_status(model: Option<&'static str>, power: Option<BatteryPower>) -> PackStatus {
    PackStatus {
        model,
        serial: Some("GO2ALP1P1008296".to_string()),
        capacity: crate::units::WattHours(2880.0),
        soc: Some(Soc::new(68)),
        power,
        temp: Some(crate::units::DeciKelvin(2911)),
    }
}

fn limits() -> SocLimits {
    SocLimits {
        min: Soc::new(10),
        max: Soc::new(95),
        balance_day: false,
    }
}

/// A charging pack reads as a negative flow, the same sign convention the
/// panel's own rate uses, so the rows and the total above them agree.
#[test]
fn a_pack_row_shows_what_its_pack_reported() {
    let charging = BatteryPower::from_flows(Watts::ZERO, Watts(1240));

    let row = pack_row(1, &pack_status(Some("AB3000L"), Some(charging)), limits());

    assert_eq!(row.name, "AB3000L");
    assert_eq!(row.serial, "GO2ALP1P1008296");
    assert_eq!(row.bar.fill, Soc::new(68));
    assert_eq!(row.soc, "68%");
    assert_eq!(row.power, "−1,240 W");
    assert_eq!(row.temperature, "18.0 °C");
    assert_eq!(row.capacity, "2.9 kWh");
}

/// An unidentified pack is named by its position, counted from one as a
/// person would, and a flow it never reported reads as unknown, not as idle.
#[test]
fn an_unidentified_pack_row_is_named_by_position_and_dashes_what_is_missing() {
    let row = pack_row(1, &pack_status(None, None), limits());

    assert_eq!(row.name, "Pack 2");
    assert_eq!(row.power, "—");
}

#[test]
fn the_battery_panel_lists_every_pack_in_order() {
    let mut state = state(vec![]);
    state.packs = vec![
        pack_status(Some("AC2400+"), None),
        pack_status(Some("AB3000L"), None),
    ];

    let battery = dashboard_view(&state, tz())
        .battery
        .expect("the fixture world has a battery");

    let names: Vec<_> = battery.packs.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["AC2400+", "AB3000L"]);
}

#[test]
fn pack_rows_carry_compact_bars_at_the_system_limits_under_a_header() {
    let mut seeded = state(vec![]);
    seeded.soc_limits = SocLimits {
        min: Soc::new(20),
        max: Soc::new(90),
        balance_day: false,
    };
    seeded.packs = vec![pack_status(Some("AB3000L"), None)];

    let battery = dashboard_view(&seeded, tz())
        .battery
        .expect("the fixture world has a battery");
    let bar = &battery.packs[0].bar;
    assert!(bar.labels.is_none());
    assert_eq!(
        (bar.limits.min, bar.limits.max),
        (Soc::new(20), Soc::new(90))
    );

    let html = crate::web::templates::pack_list::render(&battery.packs).into_string();
    assert!(html.contains("soc-bar--compact"), "{html}");
    assert!(
        html.contains("left: 20%") && html.contains("left: 90%"),
        "{html}"
    );
    assert!(!html.contains("soc-bar__stripes"), "{html}");
    for column in ["Pack", "Charge", "Power", "Temp", "Capacity"] {
        assert!(html.contains(&format!(">{column}<")), "{column}: {html}");
    }
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
    assert_eq!(importing.stat_cards.grid.detail, "Importing from grid");

    let mut exporting_state = state(vec![]);
    exporting_state.engine.world.observe_meter(
        None,
        MeterReading::total_only(GridPower(-900.0)),
        SolarPower::ZERO,
    );
    let exporting = dashboard_view(&exporting_state, tz());
    assert_eq!(exporting.stat_cards.grid.detail, "Exporting to grid");
}

/// A fraction of a watt is not an export worth a minus sign in front of a zero.
#[test]
fn a_sub_watt_grid_reading_reads_as_zero_without_a_sign() {
    let mut drifting = state(vec![]);
    drifting.engine.world.observe_meter(
        None,
        MeterReading::total_only(GridPower(-0.4)),
        SolarPower::ZERO,
    );

    assert_eq!(dashboard_view(&drifting, tz()).stat_cards.grid.value, "0");
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
        journey::interval_ring(),
        at(0),
    );
    let view = dashboard_view(&discharging, tz());

    assert_eq!(view.stat_cards.home.label, "Home usage");
    assert_eq!(view.stat_cards.home.value, "1,750");
}

/// No `[car_battery]` configured and no poll yet look identical from the
/// view layer — both are `car_soc: None` — and render the same placeholder.
#[test]
fn the_ev_card_placeholders_with_no_reading_yet() {
    let s = state(vec![]);
    let view = dashboard_view(&s, tz());

    assert_eq!(view.stat_cards.ev.variant, "ev");
    assert_eq!(view.stat_cards.ev.value, "--");
    assert_eq!(view.stat_cards.ev.detail, "No vehicle configured");
}

/// Once the car-battery poller has landed a reading, the card shows the
/// real percent and when it was read.
#[test]
fn the_ev_card_reports_the_last_polled_soc() {
    let mut s = state(vec![]);
    s.car_soc_tick(crate::units::Soc::new(62), at(300));
    let view = dashboard_view(&s, tz());

    assert_eq!(view.stat_cards.ev.value, "62");
    assert!(
        view.stat_cards.ev.detail.starts_with("Updated "),
        "detail was {:?}",
        view.stat_cards.ev.detail
    );
}

// --- SSE dedupe --------------------------------------------------------------

#[test]
fn an_unchanged_fragment_is_not_sent_again_but_a_changed_one_is() {
    let mut sent = SentFragments::default();
    let first = dashboard_view(&state(vec![]), tz());

    assert_eq!(sent.changed(&first).len(), FRAGMENTS.len());
    assert!(sent.changed(&first).is_empty());

    let logged = state(vec![(at(0), decision(ControlMode::Idle, "now"))]);
    let second = dashboard_view(&logged, tz());
    let names: Vec<_> = sent.changed(&second).into_iter().map(|(n, _)| n).collect();
    assert!(names.contains(&"decision-log"), "{names:?}");
    assert!(names.len() < FRAGMENTS.len(), "{names:?}");
}

#[test]
fn a_new_connection_is_sent_every_fragment_again() {
    let view = dashboard_view(&state(vec![]), tz());
    SentFragments::default().changed(&view);

    assert_eq!(
        SentFragments::default().changed(&view).len(),
        FRAGMENTS.len()
    );
}

// --- SOC limits --------------------------------------------------------------

fn bar_html(min: u32, max: u32, balance_day: bool) -> String {
    use crate::web::soc_bar::SocBarView;
    let view = SocBarView::labelled(
        Soc::new(50),
        SocLimits {
            min: Soc::new(min),
            max: Soc::new(max),
            balance_day,
        },
    );
    crate::web::templates::soc_bar::render(&view).into_string()
}

#[test]
fn the_battery_panel_reads_usable_energy_against_a_full_bar() {
    use crate::units::{KiloWattHours, Soc};

    let mut seeded = state(vec![]);
    seeded.soc_limits = SocLimits {
        min: Soc::new(20),
        max: Soc::FULL,
        balance_day: true,
    };
    seeded.pack_capacity = KiloWattHours(5.0);

    let battery = dashboard_view(&seeded, tz())
        .battery
        .expect("the fixture world has a battery");

    assert_eq!(battery.bar.limits.min, Soc::new(20));
    assert_eq!(battery.bar.limits.max, Soc::FULL);
    // 80% of 5 kWh, discounted by the 85% round trip a fresh tracker assumes.
    assert!(battery.usable_energy.value.ends_with("of 3.4 kWh"));
}

#[test]
fn the_limit_ticks_sit_at_the_limits_and_the_stripes_cover_what_is_unused() {
    let html = bar_html(10, 95, false);

    assert!(html.contains("soc-bar__limit\" style=\"left: 10%\""));
    assert!(html.contains("soc-bar__limit\" style=\"left: 95%\""));
    assert!(html.contains("stripes--reserve"));
    assert!(html.contains("width: 10%"));
    assert!(html.contains("width: 5%"));
    assert!(html.contains("min 10%") && html.contains("max 95%"));
}

#[test]
fn a_balance_day_bar_reaches_100_with_a_label_and_no_headroom() {
    let html = bar_html(10, 100, true);

    assert!(html.contains("left: 100%"));
    assert!(html.contains("max 100% · balance day"));
    assert!(html.contains("max 100% ⚖"));
    assert!(!html.contains("stripes--headroom"));
}

#[test]
fn limit_labels_anchor_inward_at_the_bar_edges() {
    let html = bar_html(0, 100, false);

    assert!(html.contains("soc-bar__label soc-bar__label--start\" style=\"left: 0%\""));
    assert!(html.contains("soc-bar__label soc-bar__label--end\" style=\"left: 100%\""));
    assert!(!html.contains("stripes--reserve"));
}

/// "max 85% · balance day" needs more than the 15 points beside an 85% limit,
/// so it turns back over the bar; the plain "max 85%" still fits outward.
#[test]
fn a_label_too_long_for_the_room_beside_its_limit_turns_back_over_the_bar() {
    assert!(bar_html(10, 85, true).contains("soc-bar__label--end\" style=\"left: 85%\""));
    assert!(bar_html(10, 85, false).contains("soc-bar__label--start\" style=\"left: 85%\""));
}

#[test]
fn close_limits_stack_their_labels_instead_of_overlapping() {
    assert!(bar_html(40, 60, false).contains("soc-bar__label--lower"));
    assert!(!bar_html(10, 95, false).contains("soc-bar__label--lower"));
}

// --- The detail modal ---------------------------------------------------------

/// A tick swaps the contents of every `sse-swap` wrapper; a dialog inside one
/// would be rebuilt closed on the next tick.
#[test]
fn the_detail_dialog_sits_outside_every_swap_region() {
    let view = dashboard_view(&state(vec![]), tz());
    let html = layout::page(&view).into_string();

    assert_eq!(html.matches("id=\"detail-modal\"").count(), 1);
    // Right after `.page` closes, which is after every swap wrapper does.
    assert!(
        html.contains("</div></div><dialog id=\"detail-modal\""),
        "the dialog is not a sibling of the page"
    );
}

#[test]
fn every_card_but_the_car_opens_its_detail() {
    let view = dashboard_view(&state(vec![]), tz());
    let html = layout::page(&view).into_string();

    for slug in ["solar", "home", "grid", "battery"] {
        assert_eq!(
            html.matches(&format!("hx-get=\"/detail/{slug}\"")).count(),
            1,
            "{slug}"
        );
    }
    assert_eq!(html.matches("hx-get=\"/detail/").count(), 4);
}

/// The `(start, end)` byte span of every element carrying `sse-swap`, from its
/// opening tag to its closing one. Nesting is counted per tag name, which is
/// enough for maud's output: it never leaves a non-void element unclosed.
fn swap_regions(html: &str) -> Vec<(usize, usize)> {
    html.match_indices("sse-swap=\"")
        .map(|(at, _)| {
            let start = html[..at].rfind('<').expect("an attribute sits in a tag");
            let tag: String = html[start + 1..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
                .collect();
            let (open, close) = (format!("<{tag}"), format!("</{tag}>"));
            let mut depth = 0;
            let mut cursor = start;
            loop {
                let next_open = html[cursor + 1..].find(&open).map(|i| i + cursor + 1);
                let next_close = html[cursor..]
                    .find(&close)
                    .map(|i| i + cursor)
                    .expect("closed");
                match next_open {
                    Some(o) if o < next_close => {
                        depth += 1;
                        cursor = o;
                    }
                    _ if depth > 0 => {
                        depth -= 1;
                        cursor = next_close + 1;
                    }
                    _ => return (start, next_close + close.len()),
                }
            }
        })
        .collect()
}

/// A tick replaces everything inside an `sse-swap` element. A card link in
/// there would be destroyed between a mousedown and its mouseup, losing the
/// click, and would take keyboard focus down with it.
#[test]
fn card_links_persist_across_ticks() {
    let view = dashboard_view(&state(vec![]), tz());
    let html = layout::page(&view).into_string();
    let regions = swap_regions(&html);
    assert_eq!(regions.len(), FRAGMENTS.len());

    let links: Vec<usize> = html
        .match_indices("hx-get=\"/detail/")
        .map(|(at, _)| at)
        .collect();
    assert_eq!(links.len(), 4);
    for link in links {
        assert!(
            regions
                .iter()
                .all(|&(start, end)| !(start..end).contains(&link)),
            "a card link sits inside a swap region: {}",
            &html[link..link + 30]
        );
    }
}

/// The stat cards and the battery panel are live through their contents, one
/// fragment each, so a reading still reaches them without replacing the link.
#[test]
fn each_card_is_live_inside_its_shell() {
    let view = dashboard_view(&state(vec![]), tz());
    let html = layout::page(&view).into_string();

    for (shell, swap) in [
        (
            "stat-card stat-card--solar stat-card--link",
            "stat-card-solar",
        ),
        (
            "stat-card stat-card--home stat-card--link",
            "stat-card-home",
        ),
        (
            "stat-card stat-card--grid stat-card--link",
            "stat-card-grid",
        ),
        ("stat-card stat-card--ev", "stat-card-ev"),
        ("battery-panel battery-panel--link", "battery-panel"),
    ] {
        assert!(
            FRAGMENTS.iter().any(|(name, _)| *name == swap),
            "{swap} is not a fragment"
        );
        let opened = html
            .find(&format!("class=\"{shell}\""))
            .unwrap_or_else(|| panic!("no {shell} shell"));
        let live = html
            .find(&format!("sse-swap=\"{swap}\""))
            .unwrap_or_else(|| panic!("no {swap} region"));
        assert!(opened < live, "{swap} is not inside its shell");
        assert!(
            !html[opened..live].contains("sse-swap="),
            "{swap} is not the shell's own region"
        );
    }
}
