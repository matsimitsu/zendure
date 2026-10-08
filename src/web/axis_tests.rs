use super::*;

/// Mobile hides every `wide-only` tick, so what is left is the binding
/// 00:00 · 12:00 · 23:59 rule, with the ends anchored inward.
#[test]
fn the_day_axis_keeps_three_labels_on_a_narrow_screen() {
    let ticks = day_axis();
    let narrow: Vec<_> = ticks
        .iter()
        .filter(|t| t.density == AxisDensity::Always)
        .collect();

    let labels: Vec<_> = narrow.iter().map(|t| t.label.as_str()).collect();
    assert_eq!(labels, ["00:00", "12:00", "23:59"]);
    assert_eq!(narrow[0].anchor, AxisAnchor::Start);
    assert_eq!(narrow[1].anchor, AxisAnchor::Middle);
    assert_eq!(narrow[2].anchor, AxisAnchor::End);
    assert_eq!(narrow[1].position.percent(), 50.0);
}

#[test]
fn the_rendered_axis_places_labels_by_percentage_not_grid_column() {
    let html = crate::web::templates::axis::render(&day_axis()).into_string();

    assert!(html.contains("axis__tick--start axis__tick--always\" style=\"left: 0.00%\""));
    assert!(html.contains("left: 100.00%"));
    assert!(!html.contains("grid"));
}

#[test]
fn a_power_step_past_two_kilowatts_keeps_doubling() {
    let fits = |range: i32| move |step: Watts| range / step.get() <= 6;

    assert_eq!(power_step(fits(2_500)), Watts(500));
    assert_eq!(power_step(fits(6_000)), Watts(1000));
    assert_eq!(power_step(fits(16_000)), Watts(4000));
    assert_eq!(power_step(fits(40_000)), Watts(8000));
}
