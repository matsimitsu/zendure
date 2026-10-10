use chrono::NaiveDate;
use maud::{Markup, html};

use crate::web::prices::{
    PRICE_CHART_HEIGHT, PRICE_CHART_WIDTH, PriceBarView, PriceChartView, PriceNavView,
    PricePanelView, PricedDayView,
};
use crate::web::templates::day_nav::{self, DayNav, DayNavLink};
use crate::web::templates::{axis, mini_stat, price_tier};

/// The panel's contents. The `<price-panel>` host around them persists
/// across SSE swaps; everything here is re-rendered whole.
pub fn render(view: &PricePanelView) -> Markup {
    match view {
        PricePanelView::Empty(reason) => html! {
            header class="price-panel__head" {
                div class="price-panel__titles" {
                    h2 class="price-panel__title" { "Electricity prices" }
                    p class="price-panel__subtitle" { (reason.text()) }
                }
            }
        },
        PricePanelView::Priced(day) => priced(day),
    }
}

fn priced(view: &PricedDayView) -> Markup {
    let readout = &view.readout;
    let tier = readout.tier;
    html! {
        header class="price-panel__head" {
            div class="price-panel__titles" {
                h2 class="price-panel__title" { "Electricity prices" }
                p class="price-panel__subtitle" { (view.subtitle) }
            }
            (day_nav_markup(&view.nav))
        }
        div class="price-panel__readout"
            data-default-label=(readout.label)
            data-default-value=(readout.value)
            data-default-tier=(tier.map_or("", |tier| tier.class_suffix()))
            data-default-tier-label=(tier.map_or("", |tier| tier.label())) {
            div class="price-panel__now" aria-live="polite" {
                span class="price-panel__read-label" { (readout.label) }
                div class="price-panel__read-row" {
                    span class="price-panel__read-value" { (readout.value) }
                    span class="price-panel__read-unit" { "ct/kWh" }
                    (price_tier::render(tier))
                }
            }
            @if let Some(windows) = &view.windows {
                @for window in windows {
                    (mini_stat::render(window))
                }
            }
        }
        (plot(&view.chart))
        ul class="price-panel__legend" {
            @for item in &view.legend {
                li class="price-panel__legend-item" {
                    span class=(format!("price-panel__swatch price-panel__swatch--{}", item.tier.class_suffix())) {}
                    span class="price-panel__legend-label" { (item.tier.label()) }
                    span class="price-panel__legend-range" { (item.range) }
                }
            }
        }
    }
}

/// `data-day` travels on the nav, which every swap replaces, so the host can
/// mirror it and drop the stream while another day is shown.
fn day_nav_markup(nav: &PriceNavView) -> Markup {
    let link = |day: Option<NaiveDate>| DayNavLink {
        href: page_href(day),
        hx_get: fragment_href(day),
    };
    day_nav::render(&DayNav {
        label: &nav.label,
        date: &nav.date,
        prev: nav.previous.map(|day| link(Some(day))),
        next: nav.next.map(|day| link(Some(day))),
        // Only behind today: from tomorrow, › already leads back.
        today: (nav.shown < nav.today).then(|| link(None)),
        hx_target: "#price-panel",
        data_day: Some(nav.data_day()),
        data_live: None,
    })
}

fn page_href(day: Option<NaiveDate>) -> String {
    match day {
        Some(day) => format!("/?price_day={day}"),
        None => "/".to_string(),
    }
}

fn fragment_href(day: Option<NaiveDate>) -> String {
    match day {
        Some(day) => format!("/fragments/price-panel?day={day}"),
        None => "/fragments/price-panel".to_string(),
    }
}

fn plot(chart: &PriceChartView) -> Markup {
    let height = format!("{PRICE_CHART_HEIGHT:.0}");
    html! {
        div class="price-panel__plot" {
            div class="price-panel__y-axis" { (axis::render_vertical(&chart.y_axis)) }
            svg class="price-panel__chart"
                viewBox=(format!("0 0 {PRICE_CHART_WIDTH:.0} {PRICE_CHART_HEIGHT:.0}"))
                preserveAspectRatio="none"
                aria-hidden="true" {
                @for &y in &chart.grid_lines {
                    (rule("price-panel__grid", y))
                }
                // Behind the bars, so the hovered hour washes rather than veils.
                @match chart.highlight {
                    Some(span) => rect class="price-panel__highlight"
                        x=(format!("{:.2}", span.x)) y="0"
                        width=(format!("{:.2}", span.width)) height=(height) {},
                    None => rect class="price-panel__highlight"
                        x="0" y="0" width="0" height=(height) {},
                }
                @for bar in &chart.bars {
                    rect class=(bar_class(bar))
                        x=(format!("{:.2}", bar.span.x))
                        y=(format!("{:.2}", bar.y))
                        width=(format!("{:.2}", bar.span.width))
                        height=(format!("{:.2}", bar.height))
                        rx="1" {}
                }
                @if let Some(y) = chart.zero_y {
                    (rule("price-panel__zero", y))
                }
                @if let Some(x) = chart.now_x {
                    line class="price-panel__now-line"
                        x1=(format!("{x:.2}")) x2=(format!("{x:.2}"))
                        y1="0" y2=(height)
                        vector-effect="non-scaling-stroke" {}
                }
                @for hit in &chart.hits {
                    @let tier = hit.readout.tier;
                    rect class="price-panel__hit"
                        x=(format!("{:.2}", hit.span.x)) y="0"
                        width=(format!("{:.2}", hit.span.width)) height=(height)
                        data-label=(hit.readout.label)
                        data-value=(hit.readout.value)
                        data-tier=(tier.map_or("", |tier| tier.class_suffix()))
                        data-tier-label=(tier.map_or("", |tier| tier.label())) {}
                }
            }
            div class="price-panel__x-axis" { (axis::render(&chart.x_axis)) }
        }
    }
}

fn bar_class(bar: &PriceBarView) -> String {
    let mut class = format!(
        "price-panel__bar price-panel__bar--{}",
        bar.tier.class_suffix()
    );
    if bar.negative {
        class.push_str(" price-panel__bar--negative");
    }
    if bar.past {
        class.push_str(" price-panel__bar--past");
    }
    class
}

/// A full-width horizontal line that stays one pixel however the plot is
/// stretched.
fn rule(class: &str, y: f64) -> Markup {
    html! {
        line class=(class)
            x1="0" x2=(format!("{PRICE_CHART_WIDTH:.0}"))
            y1=(format!("{y:.2}")) y2=(format!("{y:.2}"))
            vector-effect="non-scaling-stroke" {}
    }
}
