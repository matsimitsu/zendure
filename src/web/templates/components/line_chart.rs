use maud::{Markup, html};

use crate::web::line_chart::{LINE_CHART_HEIGHT, LINE_CHART_WIDTH, LineChartView};
use crate::web::templates::axis;

/// One series over time, with its value axis beside it and its time axis
/// below. The root is a `line-chart` element so a script can upgrade it in
/// place; until then it is an ordinary light-DOM box. The empty readout is
/// where that script writes the hovered bucket.
pub fn render(view: &LineChartView) -> Markup {
    let full_width = format!("{LINE_CHART_WIDTH:.0}");
    html! {
        line-chart class=(format!("line-chart line-chart--{}", view.series.modifier())) {
            div class="line-chart__head" {
                div class="line-chart__heading" {
                    span class="line-chart__title" { (view.title) }
                    @if let Some(note) = view.note {
                        span class="line-chart__note" { (note) }
                    }
                }
                div class="line-chart__readout" aria-live="polite" {}
            }
            div class="line-chart__body" {
                (axis::render_vertical(&view.y_ticks))
                div class="line-chart__plot" {
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
                    }
                    (axis::render(&view.x_ticks))
                }
            }
        }
    }
}
