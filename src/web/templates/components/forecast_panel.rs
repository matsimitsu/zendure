use maud::{Markup, html};

use crate::web::day_nav::PagePanel;
use crate::web::day_panel::{DayPanel, EmptyReason};
use crate::web::plot::{BAR_RADIUS, DAY_CHART_HEIGHT};
use crate::web::solar::{
    ForecastChartView, ForecastDayView, ForecastPanelView, SolarFigure, empty_text,
};
use crate::web::templates::day_chart::{self, DayChartKind};
use crate::web::templates::mini_stat;

const KIND: DayChartKind = DayChartKind {
    title: "Solar forecast",
    panel: PagePanel::Solar,
    fragment: "/fragments/forecast-panel",
    hx_target: "#forecast-panel",
};

/// The panel's contents. The `<forecast-panel>` host around them persists
/// across SSE swaps; everything here is re-rendered whole.
pub fn render(view: &ForecastPanelView) -> Markup {
    match view {
        DayPanel::Empty(reason) => day_chart::empty_head(&KIND, empty_text(*reason)),
        DayPanel::Blank(nav) => day_chart::head(&KIND, empty_text(EmptyReason::Waiting), nav),
        DayPanel::Shown(day) => forecast(day),
    }
}

fn forecast(view: &ForecastDayView) -> Markup {
    let readout = &view.readout;
    let actual = readout.actual.as_ref();
    html! {
        (day_chart::head(&KIND, &view.subtitle, &view.nav))
        div class="day-chart__readout"
            data-default-label=(readout.label)
            data-default-forecast=(readout.forecast.value)
            data-default-forecast-unit=(readout.forecast.unit)
            data-default-actual=[actual.map(|figure| &figure.value)]
            data-default-actual-unit=[actual.map(|figure| figure.unit)] {
            // No live region: every SSE tick replaces it, and the hover
            // targets that change it sit in an aria-hidden chart.
            div class="day-chart__now" {
                span class="day-chart__read-label" { (readout.label) }
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
        (day_chart::plot(&view.chart.plot, layers(&view.chart), hits(&view.chart)))
        (day_chart::legend(html! {
            (day_chart::legend_item(&swatch_class("forecast"), "Forecast", None))
            (day_chart::legend_item(&swatch_class("actual"), "Actual", None))
        }))
    }
}

fn swatch_class(series: &str) -> String {
    format!("forecast-panel__swatch forecast-panel__swatch--{series}")
}

/// One readout figure: the value, its unit and a captioned swatch.
fn read(series: &str, caption: &str, figure: &SolarFigure) -> Markup {
    html! {
        div class=(format!("forecast-panel__read forecast-panel__read--{series}")) {
            div class="day-chart__read-row" {
                span class="day-chart__read-value" { (figure.value) }
                span class="day-chart__read-unit" { (figure.unit) }
            }
            span class="forecast-panel__read-caption" {
                span class=(swatch_class(series)) {}
                (caption)
            }
        }
    }
}

fn layers(chart: &ForecastChartView) -> Markup {
    html! {
        @for bar in &chart.bars {
            rect class="forecast-panel__bar"
                x=(format!("{:.2}", bar.span.x))
                y=(format!("{:.2}", bar.y))
                width=(format!("{:.2}", bar.span.width))
                height=(format!("{:.2}", bar.height))
                rx=(BAR_RADIUS) {}
        }
        @if !chart.actual_path.is_empty() {
            path class="forecast-panel__actual" d=(chart.actual_path)
                vector-effect="non-scaling-stroke" {}
        }
    }
}

fn hits(chart: &ForecastChartView) -> Markup {
    html! {
        @for hit in &chart.hits {
            rect class="day-chart__hit"
                x=(format!("{:.2}", hit.span.x)) y="0"
                width=(format!("{:.2}", hit.span.width)) height=(format!("{DAY_CHART_HEIGHT:.0}"))
                data-label=(hit.label)
                data-forecast=(hit.forecast.value)
                data-forecast-unit=(hit.forecast.unit)
                data-actual=(hit.actual.value)
                data-actual-unit=(hit.actual.unit)
                data-slot=(hit.slot) {}
        }
    }
}
