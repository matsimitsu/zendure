use maud::{Markup, html};

use crate::web::line_chart::LinePointView;

use crate::web::line_chart::{LINE_CHART_HEIGHT, LINE_CHART_WIDTH, LineChartView};
use crate::web::templates::axis;

/// One series over time, with its value axis beside it and its time axis
/// below. The root is a `line-chart` element so a script can upgrade it in
/// place; until then it is an ordinary light-DOM box showing the latest
/// bucket. The script only copies the `data-*` strings into the readout.
pub fn render(view: &LineChartView) -> Markup {
    let full_width = format!("{LINE_CHART_WIDTH:.0}");
    html! {
        line-chart class=(format!("line-chart line-chart--{}", view.series.modifier()))
            data-times=(json(view, |p| &p.time))
            data-values=(json(view, |p| &p.value))
            data-y=(json_y(view))
            tabindex="0" {
            div class="line-chart__head" {
                div class="line-chart__heading" {
                    span class="line-chart__title" { (view.title) }
                    @if let Some(note) = &view.note {
                        span class="line-chart__note" { (note) }
                    }
                }
                div class="line-chart__readout" aria-live="polite" {
                    span class="line-chart__readout-time" { "now" }
                    span class="line-chart__readout-value" { (view.default_value()) }
                }
            }
            div class="line-chart__body" {
                (axis::render_vertical(&view.y_ticks))
                div class="line-chart__plot" {
                    div class="line-chart__canvas" {
                    svg class="line-chart__svg"
                        viewBox=(format!("0 0 {LINE_CHART_WIDTH:.0} {LINE_CHART_HEIGHT:.0}"))
                        preserveAspectRatio="none" {
                        @for band in &view.bands {
                            rect class="line-chart__band" x="0" y=(format!("{:.1}", band.y)) width=(full_width) height=(format!("{:.1}", band.height)) {}
                        }
                        @for y in &view.grid_lines {
                            line class="line-chart__grid" x1="0" x2=(full_width) y1=(format!("{y:.1}")) y2=(format!("{y:.1}")) {}
                        }
                        @for y in &view.limit_lines {
                            line class="line-chart__limit" x1="0" x2=(full_width) y1=(format!("{y:.1}")) y2=(format!("{y:.1}")) {}
                        }
                        path class="line-chart__area" d=(view.area_path) {}
                        line class="line-chart__zero" x1="0" x2=(full_width) y1=(format!("{:.1}", view.zero_y)) y2=(format!("{:.1}", view.zero_y)) {}
                        path class="line-chart__line" d=(view.line_path) {}
                        line class="line-chart__crosshair" x1="0" x2="0" y1="0" y2=(format!("{LINE_CHART_HEIGHT:.0}")) {}
                    }
                    div class="line-chart__dot" {}
                    }
                    (axis::render(&view.x_ticks))
                }
            }
        }
    }
}

fn json(view: &LineChartView, field: fn(&LinePointView) -> &String) -> String {
    let items: Vec<&String> = view.points.iter().map(field).collect();
    serde_json::to_string(&items).expect("strings serialize")
}

/// `null` for a gap, so the array stays one entry per bucket.
fn json_y(view: &LineChartView) -> String {
    let ys: Vec<Option<f64>> = view
        .points
        .iter()
        .map(|p| p.y.map(|y| (y * 10.0).round() / 10.0))
        .collect();
    serde_json::to_string(&ys).expect("numbers serialize")
}
