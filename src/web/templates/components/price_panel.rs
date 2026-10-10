use maud::{Markup, html};

use crate::web::templates::{axis, mini_stat};
use crate::web::view::{
    PRICE_CHART_HEIGHT, PRICE_CHART_WIDTH, PriceBarView, PriceDayView, PricePanelView,
};

/// The electricity price chart: a bar per price interval for today, and for
/// tomorrow once its prices are published, with the current interval marked.
pub fn render(view: &PricePanelView) -> Markup {
    html! {
        div class="price-panel" {
            div class="price-panel__header" {
                h2 class="price-panel__title" { "Electricity prices" }
                @if view.has_data {
                    p class="price-panel__subtitle" { (view.as_of) }
                }
            }

            @if view.has_data {
                div class="price-panel__readout" {
                    (mini_stat::render(&view.current))
                    (mini_stat::render(&view.today_min))
                    (mini_stat::render(&view.today_max))
                }
                div class="price-panel__chart-label" { (view.chart_label) }
                @for day in &view.days {
                    (day_chart(day))
                }
            } @else {
                p class="price-panel__empty" { (view.as_of) }
            }
        }
    }
}

fn day_chart(day: &PriceDayView) -> Markup {
    html! {
        div class="price-panel__day" {
            div class="price-panel__day-label" { (day.label) }
            svg class="price-panel__chart"
                viewBox=(format!("0 0 {PRICE_CHART_WIDTH:.0} {PRICE_CHART_HEIGHT:.0}"))
                preserveAspectRatio="none" {
                @for bar in &day.bars {
                    rect class=(bar_class(bar))
                        x=(format!("{:.1}", bar.x))
                        y=(format!("{:.1}", bar.y))
                        width=(format!("{:.1}", bar.width))
                        height=(format!("{:.1}", bar.height)) {}
                }
                line class="price-panel__zero"
                    x1="0" x2=(format!("{PRICE_CHART_WIDTH:.0}"))
                    y1=(format!("{:.1}", day.zero_y)) y2=(format!("{:.1}", day.zero_y))
                    vector-effect="non-scaling-stroke" {}
            }
            (axis::render(&day.axis))
        }
    }
}

fn bar_class(bar: &PriceBarView) -> String {
    let mut class = String::from("price-panel__bar");
    if bar.negative {
        class.push_str(" price-panel__bar--negative");
    }
    if bar.current {
        class.push_str(" price-panel__bar--current");
    }
    class
}
