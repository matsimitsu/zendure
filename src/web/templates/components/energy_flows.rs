use chrono::NaiveDate;
use maud::{Markup, html};

use crate::web::entity::Entity;
use crate::web::flows::{
    DayNavView, EnergyFlowsView, FLOWS_CHART_HEIGHT, FLOWS_CHART_WIDTH, FlowPlotView,
    FlowResolution,
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
            div class="energy-flows__controls" {
                (day_nav(&view.nav, view.interval))
                div class="energy-flows__segmented" role="group" aria-label="Interval" {
                    @for plot in &view.plots {
                        @let key = plot.resolution.key();
                        button
                            type="button"
                            class=(format!("energy-flows__segment energy-flows__segment--{key}"))
                            data-interval=(key)
                            aria-pressed=(plot.resolution == view.interval) { (key) }
                    }
                }
            }
        }
        div class="energy-flows__legend" aria-live="polite" {
            span class="energy-flows__readout-time" { (view.readout.label) " · kW" }
            @for (series, value) in &view.readout.values {
                @let key = series.slug();
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

/// The host's classes as the server renders them; after load the script owns
/// the interval modifier.
pub fn host_class(view: &EnergyFlowsView) -> String {
    match view.interval {
        FlowResolution::Hour => "energy-flows".to_string(),
        FlowResolution::Quarter => "energy-flows energy-flows--15m".to_string(),
    }
}

/// `data-day` and `data-live` travel on the nav, which every swap replaces;
/// the script mirrors them onto the host, whose `data-live` decides whether
/// the stream may overwrite the panel.
fn day_nav(nav: &DayNavView, interval: FlowResolution) -> Markup {
    html! {
        nav class="energy-flows__day-nav" aria-label="Day" data-day=(nav.shown) data-live=(nav.live()) {
            (step(nav.previous, "‹", "Previous day", interval))
            div class="energy-flows__day" {
                span class="energy-flows__day-label" { (nav.label) }
                span class="energy-flows__day-date" { (nav.date) }
            }
            (step(nav.next, "›", "Next day", interval))
            @if !nav.live() {
                a class="energy-flows__today"
                    href=(page_href(None, interval))
                    hx-get=(fragment_href(None))
                    hx-target="closest energy-flows" { "Today" }
            }
        }
    }
}

/// A link to `target`, or an inert glyph where there is no day to go to.
fn step(target: Option<NaiveDate>, glyph: &str, label: &str, interval: FlowResolution) -> Markup {
    html! {
        @match target {
            Some(day) => a class="energy-flows__step"
                href=(page_href(Some(day), interval))
                hx-get=(fragment_href(Some(day)))
                hx-target="closest energy-flows"
                aria-label=(label) { (glyph) },
            None => span class="energy-flows__step energy-flows__step--disabled"
                role="link"
                aria-disabled="true"
                aria-label=(label) { (glyph) },
        }
    }
}

/// The whole page on `day`, for a browser following the link itself. The
/// interval rides along because only the script remembers it otherwise.
fn page_href(day: Option<NaiveDate>, interval: FlowResolution) -> String {
    match day {
        Some(day) => format!("/?day={day}&interval={}", interval.key()),
        None => format!("/?interval={}", interval.key()),
    }
}

/// The panel alone; the script adds the interval it is showing.
fn fragment_href(day: Option<NaiveDate>) -> String {
    match day {
        Some(day) => format!("/fragments/energy-flows?day={day}"),
        None => "/fragments/energy-flows".to_string(),
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
                    class=(format!("energy-flows__bar energy-flows__bar--{}", bar.series.slug()))
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
                    data-solar=(hit.readout.value(Entity::Solar))
                    data-home=(hit.readout.value(Entity::Home))
                    data-grid=(hit.readout.value(Entity::Grid))
                    data-battery=(hit.readout.value(Entity::Battery)) {}
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
