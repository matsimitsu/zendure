use maud::{Markup, html};

use crate::web::templates::mini_stat;
use crate::web::view::BatteryPanelView;

pub fn render(view: &BatteryPanelView) -> Markup {
    html! {
        div class="battery-panel" {
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
            div class="battery-panel__bar" {
                div class="battery-panel__bar-fill" style=(format!("width: {}%;", view.soc_percent)) {}
            }
            div class="battery-panel__stats" {
                (mini_stat::render(&view.rate))
                (mini_stat::render(&view.usable_energy))
                (mini_stat::render(&view.capacity))
                (mini_stat::render(&view.round_trip_efficiency))
            }
        }
    }
}

/// Rendered when there is no battery in the world yet (e.g. at startup
/// before the first device reading arrives).
pub fn empty() -> Markup {
    html! {
        div class="battery-panel battery-panel--empty" {
            div class="battery-panel__empty-message" {
                "No battery data yet"
            }
        }
    }
}
