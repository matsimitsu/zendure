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
    back_link, battery_panel, callout, decision_log, detail_view, energy_flows, forecast_panel,
    modal, page_header, price_panel, stat_card, top_bar,
};
use crate::web::view::{DashboardView, DetailView};

/// The `assets/js/*.js` file names `build.rs` copied, one per line.
const APP_SCRIPTS: &str = include_str!(concat!(env!("OUT_DIR"), "/app_scripts.txt"));

fn head() -> Markup {
    html! {
    head {
            meta charset="utf-8";
            meta name="viewport" content="width=device-width, initial-scale=1";
            title { "Home energy system" }
            link rel="preconnect" href="https://fonts.googleapis.com";
            link href="https://fonts.googleapis.com/css2?family=Inter:wght@400;500;600;700&family=JetBrains+Mono:wght@400;500;600&display=swap" rel="stylesheet";
            link rel="stylesheet" href="/assets/dashboard.css";
            script src="/assets/htmx.min.js" {}
            script src="/assets/sse.js" {}
            @for name in APP_SCRIPTS.lines() {
                // A module, so each file's top-level names are its own: classic
                // scripts share one global scope, where two `const B`s collide
                // and the second element is never defined. Modules are also
                // deferred, so elements upgrade after the markup is parsed.
                script type="module" src=(format!("/assets/{name}")) {}
            }
        }
    }
}

pub fn page(view: &DashboardView) -> Markup {
    html! {
        (DOCTYPE)
        html {
            (head())
            body hx-ext="sse" sse-connect="/events" {
                div id="top-bar" sse-swap="top-bar" {
                    (top_bar_inner(view))
                }
                div class="page" {
                    div id="page-header" sse-swap="page-header" {
                        (page_header_inner(view))
                    }
                    // Not swapped as a row: each card's link stays put and
                    // only its contents are live (see `stat_card::render`).
                    div id="stat-cards" class="stat-row" {
                        (stat_card::render(&view.stat_cards.solar, "stat-card-solar"))
                        (stat_card::render(&view.stat_cards.home, "stat-card-home"))
                        (stat_card::render(&view.stat_cards.grid, "stat-card-grid"))
                        (stat_card::render(&view.stat_cards.ev, "stat-card-ev"))
                    }
                    (battery_panel::render(view.battery.as_ref(), "battery-panel"))
                    // `data-live` is on the host from the first byte, so a
                    // page opened on a past day drops the stream's first
                    // message too.
                    energy-flows id="energy-flows" class=(energy_flows::host_class(&view.energy_flows))
                        sse-swap="energy-flows"
                        data-day=(view.energy_flows.nav.shown)
                        data-live=(view.energy_flows.nav.is_today()) {
                        (energy_flows_inner(view))
                    }
                    div id="decision-log" sse-swap="decision-log" {
                        (decision_log_inner(view))
                    }
                    div id="forecast-panel" sse-swap="forecast-panel" {
                        (forecast_panel_inner(view))
                    }
                    // A persistent host like `<energy-flows>`: the readout
                    // script's handlers live on it and outlast every swap.
                    price-panel id="price-panel" class="price-panel" sse-swap="price-panel"
                        data-day=[view.prices.data_day()]
                        data-live=(view.prices.data_live()) {
                        (price_panel_inner(view))
                    }
                    (callout::render())
                }
                (modal::shell())
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

/// The solar card's contents — what the `stat-card-solar` SSE event's
/// payload must match.
pub fn solar_card_inner(view: &DashboardView) -> Markup {
    stat_card::contents(&view.stat_cards.solar)
}

/// The home card's contents — what the `stat-card-home` SSE event's payload
/// must match.
pub fn home_card_inner(view: &DashboardView) -> Markup {
    stat_card::contents(&view.stat_cards.home)
}

/// The grid card's contents — what the `stat-card-grid` SSE event's payload
/// must match.
pub fn grid_card_inner(view: &DashboardView) -> Markup {
    stat_card::contents(&view.stat_cards.grid)
}

/// The car card's contents — what the `stat-card-ev` SSE event's payload
/// must match.
pub fn ev_card_inner(view: &DashboardView) -> Markup {
    stat_card::contents(&view.stat_cards.ev)
}

/// The battery panel's contents — what the `battery-panel` SSE event's
/// payload must match.
pub fn battery_panel_inner(view: &DashboardView) -> Markup {
    battery_panel::contents(view.battery.as_ref())
}

/// The flows chart's contents. Its `<energy-flows>` host is the swap target
/// rather than a plain `div`, so the state its modifiers carry outlives a tick.
pub fn energy_flows_inner(view: &DashboardView) -> Markup {
    energy_flows::render(&view.energy_flows)
}

/// The decision log's contents — what the `decision-log` SSE event's payload
/// must match. Re-rendered wholesale rather than diffed, since it is at most
/// twenty short rows.
pub fn decision_log_inner(view: &DashboardView) -> Markup {
    decision_log::render(&view.decision_log)
}

/// The forecast panel's contents — what the `forecast-panel` SSE event's
/// payload must match.
pub fn forecast_panel_inner(view: &DashboardView) -> Markup {
    forecast_panel::render(&view.forecast)
}

/// The price panel's contents — what the `price-panel` SSE event's payload
/// must match.
pub fn price_panel_inner(view: &DashboardView) -> Markup {
    price_panel::render(&view.prices)
}

/// The full-page fallback for `/detail/{entity}`, for a browser that followed
/// the card's link without htmx.
pub fn detail_page(dashboard: &DashboardView, detail: &DetailView) -> Markup {
    html! {
        (DOCTYPE)
        html {
            (head())
            body {
                (top_bar_inner(dashboard))
                div class="page page--detail" {
                    (back_link::render())
                    div class="modal__panel modal__panel--page" {
                        (modal::panel(detail.entity, detail_view::render(detail), modal::Presentation::Page))
                    }
                }
            }
        }
    }
}

/// What `/detail/{entity}` swaps into the dialog's panel.
pub fn detail_fragment(detail: &DetailView) -> Markup {
    modal::panel(
        detail.entity,
        detail_view::render(detail),
        modal::Presentation::Dialog,
    )
}
