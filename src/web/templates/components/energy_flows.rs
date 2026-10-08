use maud::{Markup, html};

use crate::web::flows::{
    EnergyFlowsView, FLOWS_CHART_HEIGHT, FLOWS_CHART_WIDTH, FlowPlotView, FlowResolution,
    FlowSeries,
};
use crate::web::templates::axis;

/// The panel's contents. The `<energy-flows>` host around them persists
/// across SSE swaps, so the interval and hidden-series modifiers live on it
/// and survive every tick; everything here is re-rendered whole.
pub fn render(view: &EnergyFlowsView) -> Markup {
    html! {
        div class="energy-flows__head" {
            div class="energy-flows__titles" {
                h2 class="energy-flows__title" { "Energy flows" }
                p class="energy-flows__subtitle" {
                    "Average power per interval, 00:00–23:59. Below zero: grid export and battery charging."
                }
            }
            div class="energy-flows__segmented" role="group" aria-label="Interval" {
                @for plot in &view.plots {
                    @let key = plot.resolution.key();
                    button
                        type="button"
                        class=(format!("energy-flows__segment energy-flows__segment--{key}"))
                        data-interval=(key)
                        aria-pressed=(plot.resolution == FlowResolution::Hour) { (key) }
                }
            }
        }
        div class="energy-flows__legend" aria-live="polite" {
            span class="energy-flows__readout-time" { (view.readout.label) " · kW" }
            @for (series, value) in &view.readout.values {
                @let key = series.key();
                button type="button" class="energy-flows__legend-item" data-series=(key) {
                    span class=(format!("energy-flows__swatch energy-flows__swatch--{key}")) {}
                    span class="energy-flows__series" { (series.label()) }
                    span class="energy-flows__readout-value" data-series=(key) { (value) }
                }
            }
        }
        div class="energy-flows__chart" {
            div class="energy-flows__y-axis" { (axis::render_vertical(&view.y_axis)) }
            div class="energy-flows__plots" {
                @for plot in &view.plots {
                    (render_plot(view, plot))
                }
            }
            div class="energy-flows__x-axis" { (axis::render(&view.x_axis)) }
        }
    }
}

fn render_plot(view: &EnergyFlowsView, plot: &FlowPlotView) -> Markup {
    let key = plot.resolution.key();
    html! {
        svg
            class=(format!("energy-flows__plot energy-flows__plot--{key}"))
            viewBox=(format!("0 0 {FLOWS_CHART_WIDTH:.0} {FLOWS_CHART_HEIGHT:.0}"))
            preserveAspectRatio="none"
            aria-hidden="true" {
            @for &y in &view.grid_lines {
                (rule("energy-flows__grid", y))
            }
            // Behind the bars, so a hovered column washes rather than veils.
            rect class="energy-flows__highlight" x="0" y="0" width="0" height=(format!("{FLOWS_CHART_HEIGHT:.0}")) {}
            @for bar in &plot.bars {
                rect
                    class=(format!("energy-flows__bar energy-flows__bar--{}", bar.series.key()))
                    x=(format!("{:.2}", bar.rect.x))
                    y=(format!("{:.2}", bar.rect.y))
                    width=(format!("{:.2}", bar.rect.width))
                    height=(format!("{:.2}", bar.rect.height)) {}
            }
            (rule("energy-flows__zero", view.zero_y))
            @if let Some(x) = plot.now_x {
                line
                    class="energy-flows__now"
                    x1=(format!("{x:.2}")) x2=(format!("{x:.2}"))
                    y1="0" y2=(format!("{FLOWS_CHART_HEIGHT:.0}"))
                    vector-effect="non-scaling-stroke" {}
            }
            @for hit in &plot.hits {
                rect
                    class=(if hit.latest { "energy-flows__hit energy-flows__hit--latest" } else { "energy-flows__hit" })
                    x=(format!("{:.2}", hit.x))
                    y="0"
                    width=(format!("{:.2}", hit.width))
                    height=(format!("{FLOWS_CHART_HEIGHT:.0}"))
                    data-label=(hit.readout.label)
                    data-solar=(hit.readout.value(FlowSeries::Solar))
                    data-home=(hit.readout.value(FlowSeries::Home))
                    data-grid=(hit.readout.value(FlowSeries::Grid))
                    data-battery=(hit.readout.value(FlowSeries::Battery)) {}
            }
        }
    }
}

/// A full-width horizontal line that stays one pixel however the plot is
/// stretched.
fn rule(class: &str, y: f64) -> Markup {
    html! {
        line
            class=(class)
            x1="0" x2=(format!("{FLOWS_CHART_WIDTH:.0}"))
            y1=(format!("{y:.2}")) y2=(format!("{y:.2}"))
            vector-effect="non-scaling-stroke" {}
    }
}
