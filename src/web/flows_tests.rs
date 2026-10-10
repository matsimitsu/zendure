//! The flows chart's geometry and readout: which way a bar grows, where zero
//! sits, how many slots a day has, and what the legend shows unhovered.

use super::*;

use chrono::TimeZone;

use crate::fixtures::journey::{battery_event, interval_ring, meter_event};
use crate::fixtures::utc;
use crate::units::{BatteryPower, Soc};

fn tz() -> Tz {
    chrono_tz::Europe::Amsterdam
}

fn bars_of(plot: &FlowPlotView, series: Entity) -> Vec<FlowRect> {
    plot.bars
        .iter()
        .filter(|bar| bar.series == series)
        .map(|bar| bar.rect)
        .collect()
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

#[test]
fn an_ordinary_day_has_24_hours_and_96_quarters() {
    // 8 October 2026, 12:00 CEST.
    let view = energy_flows_view(&interval_ring(), utc(8, 10, 0), tz());
    let [hours, quarters] = &view.plots;

    assert_eq!(hours.hits.len(), 24);
    assert_eq!(quarters.hits.len(), 96);
}

#[test]
fn the_autumn_dst_day_has_25_hours_and_100_quarters() {
    let view = energy_flows_view(&interval_ring(), utc(25, 10, 0), tz());
    let [hours, quarters] = &view.plots;

    assert_eq!(hours.hits.len(), 25);
    assert_eq!(quarters.hits.len(), 100);
}

/// Grid importing and battery discharging grow up from zero; exporting and
/// charging grow down from it.
#[test]
fn a_positive_flow_stands_on_the_zero_line_and_a_negative_one_hangs_from_it() {
    let mut history = interval_ring();
    // 10:00 CEST: importing 400 W while the battery charges at 300 W.
    history.record(&meter_event(utc(8, 8, 1), 400.0, 0.0));
    history.record(&battery_event(
        utc(8, 8, 2),
        BatteryPower(-300),
        Soc::new(50),
    ));

    let view = energy_flows_view(&history, utc(8, 8, 20), tz());
    let quarters = &view.plots[1];

    let [grid] = bars_of(quarters, Entity::Grid)[..] else {
        panic!("one grid bar");
    };
    assert!(close(grid.y + grid.height, view.zero_y), "{grid:?}");

    let [battery] = bars_of(quarters, Entity::Battery)[..] else {
        panic!("one battery bar");
    };
    assert!(close(battery.y, view.zero_y), "{battery:?}");
    assert!(close(grid.height / battery.height, 400.0 / 300.0));
}

#[test]
fn a_wide_range_steps_coarser_than_two_kilowatts_rather_than_crowding_labels() {
    let mut history = interval_ring();
    history.record(&meter_event(utc(8, 8, 1), -16000.0, 16000.0));

    let view = energy_flows_view(&history, utc(8, 8, 20), tz());

    let labels: Vec<_> = view.y_axis.iter().map(|t| t.label.as_str()).collect();
    assert_eq!(labels, ["16.0", "8.0", "0", "−8.0", "−16.0"]);
}

#[test]
fn zero_sits_where_the_scale_puts_it() {
    let mut history = interval_ring();
    history.record(&meter_event(utc(8, 8, 1), 900.0, 0.0));
    history.record(&battery_event(
        utc(8, 8, 2),
        BatteryPower(-450),
        Soc::new(50),
    ));

    let view = energy_flows_view(&history, utc(8, 8, 20), tz());

    // 1000 W above zero and 500 W below, on steps of 500 W.
    assert!(
        close(view.zero_y, FLOWS_CHART_HEIGHT * 2.0 / 3.0),
        "{}",
        view.zero_y
    );
    let labels: Vec<_> = view.y_axis.iter().map(|t| t.label.as_str()).collect();
    assert_eq!(labels, ["1.0", "0.5", "0", "−0.5"]);
    assert_eq!(view.grid_lines.len(), 3);
}

#[test]
fn with_nothing_below_zero_the_zero_line_is_the_floor() {
    let view = energy_flows_view(&interval_ring(), utc(8, 10, 0), tz());

    assert!(close(view.zero_y, FLOWS_CHART_HEIGHT));
}

#[test]
fn a_wide_range_takes_a_larger_step() {
    let scale = FlowScale::fitting([Watts(3000), Watts(-2400)].into_iter());

    assert_eq!(scale.step, Watts(1000));
    assert_eq!((scale.top, scale.bottom), (Watts(3000), Watts(-3000)));
}

#[test]
fn a_tiny_flow_still_shows() {
    let scale = FlowScale::fitting([Watts(2000)].into_iter());

    assert!(close(scale.bar(Watts(1)).1, MIN_BAR_HEIGHT));
}

/// The legend opens on the newest hour that has finished, not on the one
/// still filling.
#[test]
fn the_default_readout_is_the_last_completed_interval() {
    let mut history = interval_ring();
    // 09:xx and 10:xx CEST complete; 11:05 is in progress.
    history.record(&meter_event(utc(8, 7, 10), 100.0, 0.0));
    history.record(&meter_event(utc(8, 8, 10), 200.0, 0.0));
    history.record(&meter_event(utc(8, 9, 5), 700.0, 0.0));

    let view = energy_flows_view(&history, utc(8, 9, 20), tz());

    assert_eq!(view.readout.label, "10:00–11:00");
    assert_eq!(view.readout.value(Entity::Grid), "+0.20");
    let [hours, quarters] = &view.plots;
    assert_eq!(hours.hits.iter().filter(|hit| hit.latest).count(), 1);
    assert!(hours.hits[10].latest);
    assert!(quarters.hits[4 * 11].latest, "11:00–11:15 has finished");
}

#[test]
fn an_unfinished_hour_draws_no_bars_but_marks_now() {
    let mut history = interval_ring();
    history.record(&meter_event(utc(8, 9, 5), 700.0, 0.0));

    let view = energy_flows_view(&history, utc(8, 9, 20), tz());
    let [hours, quarters] = &view.plots;

    assert!(hours.bars.is_empty());
    assert_eq!(bars_of(quarters, Entity::Grid).len(), 1);
    // 11:00 and 11:15 local, out of a 24-hour day 1000 units wide.
    assert!(close(hours.now_x.unwrap(), 11.0 / 24.0 * FLOWS_CHART_WIDTH));
    assert!(close(
        quarters.now_x.unwrap(),
        11.25 / 24.0 * FLOWS_CHART_WIDTH
    ));
}

#[test]
fn readouts_carry_signed_kilowatts() {
    assert_eq!(readout_value(Some(Watts(1820))), "+1.82");
    assert_eq!(readout_value(Some(Watts(-400))), "−0.40");
    assert_eq!(readout_value(Some(Watts(-1))), "+0.00");
    assert_eq!(readout_value(None), MISSING);
}

#[test]
fn every_hit_column_carries_its_readout_for_the_script() {
    let mut history = interval_ring();
    history.record(&meter_event(utc(8, 8, 1), 400.0, 0.0));

    let view = energy_flows_view(&history, utc(8, 9, 20), tz());
    let html = crate::web::templates::energy_flows::render(&view).into_string();

    assert!(html.contains(r#"data-label="10:00–11:00""#), "{html}");
    assert!(html.contains(r#"data-grid="+0.40""#));
    assert!(html.contains("energy-flows__hit--latest"));
    assert!(html.contains(r#"aria-live="polite""#));
}

fn on(day: NaiveDate, interval: FlowResolution) -> FlowsRequest {
    FlowsRequest { day, interval }
}

#[test]
fn the_spring_dst_day_has_23_hours_and_92_quarters() {
    let day = NaiveDate::from_ymd_opt(2026, 3, 29).unwrap();
    let now = Timestamp::from(chrono::Utc.with_ymd_and_hms(2026, 4, 2, 12, 0, 0).unwrap());

    let view = requested_flows_view(
        &interval_ring(),
        on(day, FlowResolution::Hour),
        None,
        now,
        tz(),
    );
    let [hours, quarters] = &view.plots;

    assert_eq!(hours.hits.len(), 23);
    assert_eq!(quarters.hits.len(), 92);
}

fn tick_at(view: &EnergyFlowsView, label: &str) -> f64 {
    view.x_axis
        .iter()
        .find(|tick| tick.label == label)
        .map(|tick| tick.position.percent())
        .unwrap_or_else(|| panic!("no {label} tick"))
}

/// On the 25-hour day, 02:00–03:00 happens twice, so every label from 03:00
/// on sits an hour further along than a 24-hour axis would put it.
#[test]
fn a_dst_day_places_its_labels_by_the_local_hour() {
    let view = energy_flows_view(&interval_ring(), utc(25, 20, 0), tz());

    assert!(close(tick_at(&view, "00:00"), 0.0));
    assert!(close(tick_at(&view, "03:00"), 4.0 / 25.0 * 100.0));
    assert!(close(tick_at(&view, "12:00"), 13.0 / 25.0 * 100.0));
    assert!(close(tick_at(&view, "23:59"), 100.0));
}

#[test]
fn an_ordinary_day_places_its_labels_every_three_hours() {
    let view = energy_flows_view(&interval_ring(), utc(8, 10, 0), tz());

    assert!(close(tick_at(&view, "12:00"), 50.0));
    assert!(close(tick_at(&view, "21:00"), 87.5));
}

/// India is offset by half an hour, so its local hours straddle two UTC
/// hours; the hourly plot must still group 10:00–11:00 on the local clock.
#[test]
fn a_half_hour_zone_groups_hours_on_its_own_clock() {
    let kolkata = chrono_tz::Asia::Kolkata;
    let mut history = interval_ring();
    // 10:10 and 10:50 IST.
    history.record(&meter_event(utc(8, 4, 40), 200.0, 0.0));
    history.record(&meter_event(utc(8, 5, 20), 600.0, 0.0));

    // 11:30 IST.
    let view = energy_flows_view(&history, utc(8, 6, 0), kolkata);
    let [hours, _] = &view.plots;

    assert_eq!(hours.hits.len(), 24);
    assert_eq!(view.readout.label, "10:00–11:00");
    assert_eq!(view.readout.value(Entity::Grid), "+0.40");
    assert!(hours.hits[10].latest);
    assert_eq!(bars_of(hours, Entity::Grid).len(), 1);
    assert!(close(hours.now_x.unwrap(), 11.0 / 24.0 * FLOWS_CHART_WIDTH));
}

#[test]
fn a_past_day_is_finished_and_not_live() {
    let mut history = interval_ring();
    history.record(&meter_event(utc(7, 21, 50), 300.0, 0.0));

    let view = requested_flows_view(
        &history,
        on(
            NaiveDate::from_ymd_opt(2026, 10, 7).unwrap(),
            FlowResolution::Quarter,
        ),
        None,
        utc(8, 10, 0),
        tz(),
    );
    let [hours, quarters] = &view.plots;

    assert!(hours.now_x.is_none() && quarters.now_x.is_none());
    assert_eq!(view.readout.label, "23:00–00:00");
    assert!(!view.nav.live());
    assert_eq!(view.nav.label, "Yesterday");
    assert_eq!(view.interval, FlowResolution::Quarter);

    let html = crate::web::templates::energy_flows::render(&view).into_string();
    assert!(html.contains(r#"data-day="2026-10-07""#), "{html}");
    assert!(html.contains(r#"data-live="false""#));
    assert!(html.contains(r#"href="/?day=2026-10-08&amp;interval=15m""#));
    assert!(html.contains(r#"hx-get="/fragments/energy-flows?day=2026-10-06""#));
    assert!(html.contains("day-nav__today"));
}

#[test]
fn today_cannot_step_forward() {
    let view = energy_flows_view(&interval_ring(), utc(8, 10, 0), tz());

    assert_eq!(view.nav.next, None);
    assert!(view.nav.live());
    let html = crate::web::templates::energy_flows::render(&view).into_string();
    assert!(html.contains(r#"aria-disabled="true""#));
    assert!(!html.contains("day-nav__today"));
}
