use maud::{Markup, html};

use crate::web::detail::DetailEntity;
use crate::web::templates::{mini_stat, pack_list, soc_bar};
use crate::web::view::BatteryPanelView;

/// The panel's shell, rendered once with the page and live only inside its
/// `__live` wrapper, for the same reason as `stat_card::render`. It is a link
/// even before the first battery reading, since the shell is never
/// re-rendered to become one later.
pub fn render(view: Option<&BatteryPanelView>, swap: &str) -> Markup {
    html! {
        a class="battery-panel battery-panel--link" href=(DetailEntity::Battery.path()) hx-get=(DetailEntity::Battery.path()) hx-target="#detail-modal .modal__panel" {
            div class="battery-panel__live" sse-swap=(swap) { (contents(view)) }
        }
    }
}

/// What a tick replaces. `None` until the first device reading arrives
/// (e.g. at startup).
pub fn contents(view: Option<&BatteryPanelView>) -> Markup {
    match view {
        Some(view) => panel(view),
        None => html! {
            div class="battery-panel__empty-message" {
                "No battery data yet"
            }
        },
    }
}

fn panel(view: &BatteryPanelView) -> Markup {
    html! {
        div class="battery-panel__head" {
            div class="battery-panel__identity" {
                div class="battery-panel__icon" {
                    "▮"
                }
                div {
                    div class="battery-panel__label" {
                        "Home battery"
                    }
                    div class="battery-panel__soc" {
                        (view.soc_percent) "%"
                    }
                }
            }
            div class=(format!("battery-panel__badge battery-panel__badge--{}", view.badge_variant)) {
                (view.mode_label)
            }
        }
        div class="battery-panel__history" { "24h history ›" }
        (soc_bar::render(&view.bar))
        div class="battery-panel__stats" {
            (mini_stat::render(&view.rate))
            (mini_stat::render(&view.usable_energy))
            (mini_stat::render(&view.capacity))
            (mini_stat::render(&view.round_trip_efficiency))
        }
        @if !view.packs.is_empty() {
            (pack_list::render(&view.packs))
        }
    }
}
