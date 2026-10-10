use chrono::NaiveDate;
use maud::{Markup, html};

use crate::web::day_nav::DayNavView;
use crate::web::plot::DAY_CHART_HEIGHT;
use crate::web::solar::{
    FORECAST_BAR_RADIUS, FORECAST_CHART_WIDTH, ForecastChartView, ForecastDayView,
    ForecastPanelView, SolarFigure,
};
use crate::web::templates::day_nav::{self, DayNav, DayNavLink};
use crate::web::templates::{axis, mini_stat};

/// The panel's contents. The `<forecast-panel>` host around them persists
/// across SSE swaps; everything here is re-rendered whole.
pub fn render(view: &ForecastPanelView) -> Markup {
    match view {
        // Nav-less, so the marker the host mirrors travels on the header.
        ForecastPanelView::Empty(reason) => html! {
            header class="forecast-panel__head" data-live="true" {
                div class="forecast-panel__titles" {
                    h2 class="forecast-panel__title" { "Solar forecast" }
                    p class="forecast-panel__subtitle" { (reason.text()) }
                }
            }
        },
        ForecastPanelView::Forecast(day) => forecast(day),
    }
}

fn forecast(view: &ForecastDayView) -> Markup {
    let readout = &view.readout;
    let actual = readout.actual.as_ref();
    html! {
        header class="forecast-panel__head" {
            div class="forecast-panel__titles" {
                h2 class="forecast-panel__title" { "Solar forecast" }
                p class="forecast-panel__subtitle" { (view.subtitle) }
            }
            (day_nav_markup(&view.nav))
        }
        div class="forecast-panel__readout"
            data-default-label=(readout.label)
            data-default-forecast=(readout.forecast.value)
            data-default-forecast-unit=(readout.forecast.unit)
            data-default-actual=[actual.map(|figure| &figure.value)]
            data-default-actual-unit=[actual.map(|figure| figure.unit)] {
            // No live region, for the reason the price panel's has none.
            div class="forecast-panel__now" {
                span class="forecast-panel__read-label" { (readout.label) }
                div class="forecast-panel__reads" {
                    (read("forecast", "Forecast", &readout.forecast))
                    @if let Some(actual) = actual {
                        (read("actual", "Actual", actual))
                    }
                }
            }
            @for stat in &view.stats {
                (mini_stat::render(stat))
            }
        }
        (plot(&view.chart))
        ul class="forecast-panel__legend" {
            (legend_item("forecast", "Forecast"))
            (legend_item("actual", "Actual"))
        }
    }
}

/// One readout figure: the value, its unit and a captioned swatch.
fn read(series: &str, caption: &str, figure: &SolarFigure) -> Markup {
    html! {
        div class=(format!("forecast-panel__read forecast-panel__read--{series}")) {
            div class="forecast-panel__read-row" {
                span class="forecast-panel__read-value" { (figure.value) }
                span class="forecast-panel__read-unit" { (figure.unit) }
            }
            span class="forecast-panel__read-caption" {
                span class=(format!("forecast-panel__swatch forecast-panel__swatch--{series}")) {}
                (caption)
            }
        }
    }
}

fn legend_item(series: &str, label: &str) -> Markup {
    html! {
        li class="forecast-panel__legend-item" {
            span class=(format!("forecast-panel__swatch forecast-panel__swatch--{series}")) {}
            span class="forecast-panel__legend-label" { (label) }
        }
    }
}

/// `data-day` and `data-live` travel on the nav, which every swap replaces,
/// so the host can mirror them and drop the stream while tomorrow is shown.
fn day_nav_markup(nav: &DayNavView) -> Markup {
    let link = |day: NaiveDate| DayNavLink {
        href: day_nav::page_href(&[&format!("solar_day={day}"), &nav.keep]),
        hx_get: format!("/fragments/forecast-panel?day={day}"),
    };
    day_nav::render(&DayNav {
        label: &nav.label,
        date: &nav.date,
        prev: nav.previous.map(link),
        next: nav.next.map(link),
        // Two days only: › and ‹ already lead back and forth.
        today: None,
        hx_target: "#forecast-panel",
        data_day: Some(nav.shown.to_string()),
        data_live: Some(nav.is_today()),
    })
}

fn plot(chart: &ForecastChartView) -> Markup {
    let height = format!("{DAY_CHART_HEIGHT:.0}");
    html! {
        div class="forecast-panel__plot" {
            div class="forecast-panel__y-axis" { (axis::render_vertical(&chart.y_axis)) }
            svg class="forecast-panel__chart"
                viewBox=(format!("0 0 {FORECAST_CHART_WIDTH:.0} {DAY_CHART_HEIGHT:.0}"))
                preserveAspectRatio="none"
                aria-hidden="true" {
                @for &y in &chart.grid_lines {
                    line class="forecast-panel__grid"
                        x1="0" x2=(format!("{FORECAST_CHART_WIDTH:.0}"))
                        y1=(format!("{y:.2}")) y2=(format!("{y:.2}"))
                        vector-effect="non-scaling-stroke" {}
                }
                // Behind the bars, so the hovered slot washes rather than
                // veils; the script returns it to the data-default pair.
                @let (x, width) = chart.highlight.map_or(
                    ("0".to_owned(), "0".to_owned()),
                    |span| (format!("{:.2}", span.x), format!("{:.2}", span.width)),
                );
                rect class="forecast-panel__highlight"
                    x=(x) y="0" width=(width) height=(height)
                    data-default-x=(x) data-default-width=(width) {}
                @for bar in &chart.bars {
                    rect class="forecast-panel__bar"
                        x=(format!("{:.2}", bar.span.x))
                        y=(format!("{:.2}", bar.y))
                        width=(format!("{:.2}", bar.span.width))
                        height=(format!("{:.2}", bar.height))
                        rx=(FORECAST_BAR_RADIUS) {}
                }
                @if !chart.actual_path.is_empty() {
                    path class="forecast-panel__actual" d=(chart.actual_path)
                        vector-effect="non-scaling-stroke" {}
                }
                @if let Some(x) = chart.now_x {
                    line class="forecast-panel__now-line"
                        x1=(format!("{x:.2}")) x2=(format!("{x:.2}"))
                        y1="0" y2=(height)
                        vector-effect="non-scaling-stroke" {}
                }
                @for hit in &chart.hits {
                    rect class="forecast-panel__hit"
                        x=(format!("{:.2}", hit.span.x)) y="0"
                        width=(format!("{:.2}", hit.span.width)) height=(height)
                        data-label=(hit.label)
                        data-forecast=(hit.forecast.value)
                        data-forecast-unit=(hit.forecast.unit)
                        data-actual=(hit.actual.value)
                        data-actual-unit=(hit.actual.unit)
                        data-slot=(hit.slot) {}
                }
            }
            div class="forecast-panel__x-axis" { (axis::render(&chart.x_axis)) }
        }
    }
}
