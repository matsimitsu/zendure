//! The page shell, and the fragment renderers the SSE stream reuses — every
//! fragment goes through the same component functions the full page does, so
//! there is no parallel "SSE renderer" to keep in sync.
//!
//! Each `*_inner` function renders only what goes *inside* its `sse-swap`
//! wrapper `div`. htmx's SSE extension does an innerHTML swap of the element
//! carrying `sse-swap`, so the payload must be that element's contents, not
//! another copy of the element itself — sending the whole wrapper nests a
//! duplicate copy inside the original on every update instead of replacing
//! it.

use maud::{DOCTYPE, Markup, html};

use super::components::{
    battery_panel, callout, decision_log, forecast_panel, page_header, stat_card, top_bar,
};
use crate::web::view::DashboardView;

pub fn page(view: &DashboardView) -> Markup {
    html! {
        (DOCTYPE)
        html {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "Home energy system" }
                link rel="preconnect" href="https://fonts.googleapis.com";
                link href="https://fonts.googleapis.com/css2?family=Inter:wght@400;500;600;700&family=JetBrains+Mono:wght@400;500;600&display=swap" rel="stylesheet";
                link rel="stylesheet" href="/assets/dashboard.css";
                script src="/assets/htmx.min.js" {}
                script src="/assets/sse.js" {}
            }
            body hx-ext="sse" sse-connect="/events" {
                div id="top-bar" sse-swap="top-bar" {
                    (top_bar_inner(view))
                }
                div class="page" {
                    div id="page-header" sse-swap="page-header" {
                        (page_header_inner(view))
                    }
                    div id="stat-cards" class="stat-row" sse-swap="stat-cards" {
                        (stat_cards_inner(view))
                    }
                    div id="battery-panel" sse-swap="battery-panel" {
                        (battery_panel_inner(view))
                    }
                    div id="decision-log" sse-swap="decision-log" {
                        (decision_log_inner(view))
                    }
                    (forecast_panel::render())
                    (callout::render())
                }
            }
        }
    }
}

/// The status badge's contents — the page's only claim about whether the
/// controller is still hearing from the meter, so it has to be swappable.
pub fn top_bar_inner(view: &DashboardView) -> Markup {
    top_bar::render(&view.top_bar)
}

/// The page header's contents. "As of 09:14" states the stream's freshness,
/// so a frozen one reads as a working dashboard with nothing to report.
pub fn page_header_inner(view: &DashboardView) -> Markup {
    page_header::render(&view.page_header)
}

/// The stat card row's contents — what the `stat-cards` SSE event's payload
/// must match, since it replaces exactly this.
pub fn stat_cards_inner(view: &DashboardView) -> Markup {
    html! {
        @for card in &view.stat_cards {
            (stat_card::render(card))
        }
    }
}

/// The battery panel's contents — what the `battery-panel` SSE event's
/// payload must match.
pub fn battery_panel_inner(view: &DashboardView) -> Markup {
    html! {
        @match &view.battery {
            Some(battery) => (battery_panel::render(battery)),
            None => (battery_panel::empty()),
        }
    }
}

/// The decision log's contents — what the `decision-log` SSE event's payload
/// must match. Re-rendered wholesale rather than diffed, since it is at most
/// twenty short rows.
pub fn decision_log_inner(view: &DashboardView) -> Markup {
    decision_log::render(&view.decision_log)
}
