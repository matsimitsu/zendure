//! The skeleton every day panel draws: its header with the day nav, and a
//! plot of grid, highlight, now line and axes around the panel's own series.
//! A panel passes its series and hover targets in as markup, and renders its
//! readout into `day-chart__readout`, which the `DayChart` script reads.

use chrono::NaiveDate;
use maud::{Markup, html};

use crate::web::day_nav::{DayNavView, PagePanel};
use crate::web::day_panel::DayPlotView;
use crate::web::plot::{CHART_WIDTH, DAY_CHART_HEIGHT};
use crate::web::templates::axis;
use crate::web::templates::day_nav::{self, DayNav, DayNavLink};

/// Which panel a skeleton belongs to: what it is called, and where its step
/// links lead.
pub struct DayChartKind {
    pub title: &'static str,
    /// Whose key a full-page step link sets.
    pub panel: PagePanel,
    /// `/fragments/price-panel`, which htmx swaps into `hx_target`.
    pub fragment: &'static str,
    pub hx_target: &'static str,
}

/// The header of a panel with no day to show. Nav-less, so the marker the
/// host mirrors travels on the header.
pub fn empty_head(kind: &DayChartKind, text: &str) -> Markup {
    html! {
        header class="day-chart__head" data-live="true" {
            (titles(kind, text))
        }
    }
}

pub fn head(kind: &DayChartKind, subtitle: &str, nav: &DayNavView) -> Markup {
    html! {
        header class="day-chart__head" {
            (titles(kind, subtitle))
            (day_nav_markup(kind, nav))
        }
    }
}

fn titles(kind: &DayChartKind, subtitle: &str) -> Markup {
    html! {
        div class="day-chart__titles" {
            h2 class="day-chart__title" { (kind.title) }
            p class="day-chart__subtitle" { (subtitle) }
        }
    }
}

/// `data-day` and `data-live` travel on the nav, which every swap replaces,
/// so the host can mirror them and drop the stream while another day is
/// shown.
fn day_nav_markup(kind: &DayChartKind, nav: &DayNavView) -> Markup {
    let link = |day: Option<NaiveDate>| {
        let own = day
            .map(|day| format!("{}={day}", kind.panel.day_key()))
            .unwrap_or_default();
        DayNavLink {
            href: day_nav::page_href(&[&own, &nav.keep]),
            hx_get: match day {
                Some(day) => format!("{}?day={day}", kind.fragment),
                None => kind.fragment.to_string(),
            },
        }
    };
    day_nav::render(&DayNav {
        label: &nav.label,
        date: &nav.date,
        prev: nav.previous.map(|day| link(Some(day))),
        next: nav.next.map(|day| link(Some(day))),
        // Only behind today: from tomorrow, › already leads back.
        today: (nav.shown < nav.today).then(|| link(None)),
        hx_target: kind.hx_target,
        data_day: Some(nav.shown.to_string()),
        data_live: Some(nav.is_today()),
    })
}

/// `layers` are drawn over the highlight and under the now line; `hits` on
/// top, so they take every pointer event.
pub fn plot(plot: &DayPlotView, layers: Markup, hits: Markup) -> Markup {
    let height = format!("{DAY_CHART_HEIGHT:.0}");
    html! {
        div class="day-chart__plot" {
            div class="day-chart__y-axis" { (axis::render_vertical(&plot.y_axis)) }
            svg class="day-chart__chart"
                viewBox=(format!("0 0 {CHART_WIDTH:.0} {DAY_CHART_HEIGHT:.0}"))
                preserveAspectRatio="none"
                aria-hidden="true" {
                @for &y in &plot.grid_lines {
                    (rule("day-chart__grid", y))
                }
                // Behind the series, so the hovered slot washes rather than
                // veils. The data-default pair is where the script returns it
                // when nothing is hovered, so the current slot stays marked.
                @let (x, width) = plot.highlight.map_or(
                    ("0".to_owned(), "0".to_owned()),
                    |span| (format!("{:.2}", span.x), format!("{:.2}", span.width)),
                );
                rect class="day-chart__highlight"
                    x=(x) y="0" width=(width) height=(height)
                    data-default-x=(x) data-default-width=(width) {}
                (layers)
                @if let Some(x) = plot.now_x {
                    line class="day-chart__now-line"
                        x1=(format!("{x:.2}")) x2=(format!("{x:.2}"))
                        y1="0" y2=(height)
                        vector-effect="non-scaling-stroke" {}
                }
                (hits)
            }
            div class="day-chart__x-axis" { (axis::render(&plot.x_axis)) }
        }
    }
}

/// A full-width horizontal line that stays one pixel however the plot is
/// stretched.
pub fn rule(class: &str, y: f64) -> Markup {
    html! {
        line class=(class)
            x1="0" x2=(format!("{CHART_WIDTH:.0}"))
            y1=(format!("{y:.2}")) y2=(format!("{y:.2}"))
            vector-effect="non-scaling-stroke" {}
    }
}

/// One legend entry: the panel's own swatch, a label, and an optional range.
pub fn legend_item(swatch_class: &str, label: &str, range: Option<&str>) -> Markup {
    html! {
        li class="day-chart__legend-item" {
            span class=(swatch_class) {}
            span class="day-chart__legend-label" { (label) }
            @if let Some(range) = range {
                span class="day-chart__legend-range" { (range) }
            }
        }
    }
}

pub fn legend(items: Markup) -> Markup {
    html! {
        ul class="day-chart__legend" { (items) }
    }
}
