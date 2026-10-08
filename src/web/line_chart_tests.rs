//! The line chart's geometry: gaps, the value range and the axes.

use super::*;

use crate::fixtures::utc;
use crate::web::axis::AxisAnchor;
use crate::web::intervals::IntervalIndex;

fn solar_slots(values: &[Option<f64>]) -> Vec<IntervalSlot> {
    let first = IntervalIndex::containing(utc(8, 0, 0));
    values
        .iter()
        .enumerate()
        .map(|(i, watts)| IntervalSlot {
            index: first.offset(i as i64),
            averages: IntervalAverages {
                solar: watts.map(SolarPower::new),
                ..IntervalAverages::default()
            },
        })
        .collect()
}

fn solar_chart(values: &[Option<f64>]) -> LineChartView {
    LineChartView::build(
        LineChartSpec::new(Entity::Solar, "Power (kW)"),
        &solar_slots(values),
        |a| a.solar,
        chrono_tz::UTC,
    )
}

#[test]
fn a_gap_bucket_starts_a_new_subpath() {
    let chart = solar_chart(&[Some(500.0), Some(1000.0), None, Some(500.0), Some(0.0)]);

    assert_eq!(
        chart.line_path.matches('M').count(),
        2,
        "{}",
        chart.line_path
    );
    assert!(
        !chart.line_path.contains("L500.0"),
        "nothing is drawn at the gap's x: {}",
        chart.line_path
    );
    assert_eq!(
        chart.area_path.matches('Z').count(),
        2,
        "{}",
        chart.area_path
    );
}

#[test]
fn a_gap_keeps_its_bucket_for_the_readout() {
    let chart = solar_chart(&[Some(1420.0), None]);

    assert_eq!(chart.points.len(), 2);
    assert_eq!(chart.points[0].value, "+1.42 kW");
    assert_eq!(chart.points[0].time, "00:00–00:15");
    assert_eq!(chart.points[1].value, "—");
    assert_eq!(chart.points[1].y, None);
}

#[test]
fn the_range_includes_zero_and_rounds_out_to_a_whole_step() {
    let chart = solar_chart(&[Some(1200.0), Some(2300.0)]);

    let labels: Vec<&str> = chart.y_ticks.iter().map(|t| t.label.as_str()).collect();
    assert_eq!(labels, ["0", "0.5", "1", "1.5", "2", "2.5"]);
    assert_eq!(chart.zero_y, LINE_CHART_HEIGHT);
    assert_eq!(
        chart.y_ticks[0].anchor,
        AxisAnchor::End,
        "the bottom label leans up"
    );
    assert_eq!(
        chart.y_ticks[5].anchor,
        AxisAnchor::Start,
        "the top label leans down"
    );
}

#[test]
fn a_wide_range_steps_coarser_than_two_kilowatts_rather_than_crowding_labels() {
    let chart = solar_chart(&[Some(9000.0), Some(16000.0)]);

    let labels: Vec<&str> = chart.y_ticks.iter().map(|t| t.label.as_str()).collect();
    assert_eq!(labels, ["0", "4", "8", "12", "16"]);
}

#[test]
fn a_negative_flow_puts_zero_inside_the_plot_and_labels_with_a_minus_sign() {
    let slots: Vec<IntervalSlot> = solar_slots(&[None, None])
        .into_iter()
        .zip([GridPower(-1500.0), GridPower(1500.0)])
        .map(|(slot, grid)| IntervalSlot {
            averages: IntervalAverages {
                grid: Some(grid),
                ..slot.averages
            },
            ..slot
        })
        .collect();
    let chart = LineChartView::build(
        LineChartSpec::new(Entity::Grid, "Power (kW)"),
        &slots,
        |a| a.grid,
        chrono_tz::UTC,
    );

    assert_eq!(chart.zero_y, LINE_CHART_HEIGHT / 2.0);
    assert_eq!(
        chart.y_ticks.first().map(|t| t.label.as_str()),
        Some("−1.5")
    );
    assert_eq!(chart.points[0].value, "−1.50 kW");
}

#[test]
fn an_empty_window_still_has_a_range_to_draw() {
    let chart = solar_chart(&[None, None, None]);

    assert!(chart.line_path.is_empty());
    assert_eq!(chart.y_ticks.len(), 2);
}

#[test]
fn a_soc_chart_spans_the_whole_range_and_draws_its_limits_and_bands() {
    let slots: Vec<IntervalSlot> = solar_slots(&[None])
        .into_iter()
        .map(|slot| IntervalSlot {
            averages: IntervalAverages {
                soc: Some(Soc::new(63)),
                ..slot.averages
            },
            ..slot
        })
        .collect();
    let spec = LineChartSpec {
        limits: vec![Soc::new(10), Soc::new(90)],
        bands: vec![(Soc::ZERO, Soc::new(10)), (Soc::new(90), Soc::FULL)],
        ..LineChartSpec::new(Entity::Battery, "State of charge (%)")
    };
    let chart = LineChartView::build(spec, &slots, |a| a.soc, chrono_tz::UTC);

    let labels: Vec<&str> = chart.y_ticks.iter().map(|t| t.label.as_str()).collect();
    assert_eq!(labels, ["0%", "25%", "50%", "75%", "100%"]);
    assert_eq!(chart.limit_lines, [162.0, 18.0]);
    assert_eq!(chart.bands[0].y, 162.0);
    assert_eq!(chart.bands[0].height, 18.0);
    assert_eq!(chart.bands[1].y, 0.0);
    assert_eq!(chart.points[0].value, "63.0%");
}

#[test]
fn the_narrow_time_axis_keeps_the_windows_start_middle_and_end() {
    let chart = solar_chart(&[None; 96]);

    let narrow: Vec<(&str, AxisAnchor)> = chart
        .x_ticks
        .iter()
        .filter(|t| t.density == AxisDensity::Always)
        .map(|t| (t.label.as_str(), t.anchor))
        .collect();
    assert_eq!(
        narrow,
        [
            ("00:00", AxisAnchor::Start),
            ("12:00", AxisAnchor::Middle),
            ("23:45", AxisAnchor::End),
        ]
    );
    assert_eq!(chart.x_ticks.len(), 9, "three-hourly on a wide screen");
}

fn rendered(values: &[Option<f64>]) -> String {
    crate::web::templates::line_chart::render(&solar_chart(values)).into_string()
}

fn attr(html: &str, name: &str) -> Vec<serde_json::Value> {
    let start = html.find(&format!("{name}=\"")).expect(name) + name.len() + 2;
    let end = start + html[start..].find('"').unwrap();
    serde_json::from_str(&html[start..end].replace("&quot;", "\"")).unwrap()
}

#[test]
fn the_hover_arrays_hold_one_entry_per_bucket() {
    let html = rendered(&[Some(1420.0), None, Some(500.0)]);

    assert_eq!(attr(&html, "data-times").len(), 3);
    assert_eq!(attr(&html, "data-values").len(), 3);
    let ys = attr(&html, "data-y");
    assert_eq!(ys.len(), 3);
    assert!(ys[1].is_null(), "a gap has no y");
}

#[test]
fn a_gap_bucket_reads_as_a_dash_in_the_data() {
    let html = rendered(&[Some(1420.0), None]);

    assert_eq!(attr(&html, "data-values")[1], "—");
}

#[test]
fn the_default_readout_is_the_latest_bucket_labelled_now() {
    let html = rendered(&[Some(1420.0), Some(500.0)]);

    assert!(
        html.contains("<span class=\"line-chart__readout-time\">now</span>"),
        "{html}"
    );
    assert!(
        html.contains("<span class=\"line-chart__readout-value\">+0.50 kW</span>"),
        "{html}"
    );
}
