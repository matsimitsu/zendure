use maud::{Markup, html};

use crate::web::day_nav::PagePanel;
use crate::web::day_panel::DayPanel;
use crate::web::plot::{BAR_RADIUS, DAY_CHART_HEIGHT};
use crate::web::prices::{
    PriceBarView, PriceChartView, PricePanelView, PricedDayView, blank_text, empty_text,
};
use crate::web::templates::day_chart::{self, DayChartKind};
use crate::web::templates::{mini_stat, price_tier};

const KIND: DayChartKind = DayChartKind {
    title: "Electricity prices",
    panel: PagePanel::Prices,
    fragment: "/fragments/price-panel",
    hx_target: "#price-panel",
};

/// The panel's contents. The `<price-panel>` host around them persists
/// across SSE swaps; everything here is re-rendered whole.
pub fn render(view: &PricePanelView) -> Markup {
    match view {
        DayPanel::Empty(reason) => day_chart::empty_head(&KIND, empty_text(*reason)),
        DayPanel::Blank(nav) => day_chart::head(&KIND, blank_text(nav), nav),
        DayPanel::Shown(day) => priced(day),
    }
}

fn priced(view: &PricedDayView) -> Markup {
    let readout = &view.readout;
    let tier = readout.tier;
    html! {
        (day_chart::head(&KIND, &view.subtitle, &view.nav))
        div class="day-chart__readout"
            data-default-label=(readout.label)
            data-default-value=(readout.value)
            data-default-tier=(tier.map_or("", |tier| tier.class_suffix()))
            data-default-tier-label=(tier.map_or("", |tier| tier.label())) {
            // No live region: every SSE tick replaces it, which would
            // re-announce the price, and the hover targets that change it
            // sit in an aria-hidden chart.
            div class="day-chart__now price-panel__now" {
                span class="day-chart__read-label" { (readout.label) }
                div class="day-chart__read-row" {
                    span class="day-chart__read-value price-panel__read-value" { (readout.value) }
                    span class="day-chart__read-unit" { "ct/kWh" }
                    (price_tier::render(tier))
                }
            }
            @if let Some(windows) = &view.windows {
                @for window in windows {
                    (mini_stat::render(window))
                }
            }
        }
        (day_chart::plot(&view.chart.plot, layers(&view.chart), hits(&view.chart)))
        (day_chart::legend(html! {
            @for item in &view.legend {
                (day_chart::legend_item(
                    &format!("price-panel__swatch price-panel__swatch--{}", item.tier.class_suffix()),
                    item.tier.label(),
                    Some(&item.range),
                ))
            }
        }))
    }
}

fn layers(chart: &PriceChartView) -> Markup {
    html! {
        @for bar in &chart.bars {
            rect class=(bar_class(bar))
                x=(format!("{:.2}", bar.span.x))
                y=(format!("{:.2}", bar.y))
                width=(format!("{:.2}", bar.span.width))
                height=(format!("{:.2}", bar.height))
                rx=(BAR_RADIUS) {}
        }
        @if let Some(y) = chart.zero_y {
            (day_chart::rule("price-panel__zero", y))
        }
    }
}

fn hits(chart: &PriceChartView) -> Markup {
    html! {
        @for hit in &chart.hits {
            @let tier = hit.readout.tier;
            rect class="day-chart__hit"
                x=(format!("{:.2}", hit.span.x)) y="0"
                width=(format!("{:.2}", hit.span.width)) height=(format!("{DAY_CHART_HEIGHT:.0}"))
                data-label=(hit.readout.label)
                data-value=(hit.readout.value)
                data-tier=(tier.map_or("", |tier| tier.class_suffix()))
                data-tier-label=(tier.map_or("", |tier| tier.label()))
                data-slot=(hit.slot.0) {}
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
