use maud::{Markup, html};

use crate::web::charger::{ChargerPanelView, ChargerReadingView};
use crate::web::templates::mini_stat;

/// The panel's shell, rendered once with the page and live only inside its
/// `__live` wrapper, as the battery panel is. Not a link, so the wrapper
/// needs no `hx-target` of its own.
pub fn render(view: &ChargerPanelView, swap: &str) -> Markup {
    html! {
        div class="charger-panel" {
            div class="charger-panel__live" sse-swap=(swap) { (contents(view)) }
        }
    }
}

/// What a tick replaces.
pub fn contents(view: &ChargerPanelView) -> Markup {
    match view {
        ChargerPanelView::NotConfigured => empty("No charger configured"),
        ChargerPanelView::Waiting => empty("No charger data yet"),
        ChargerPanelView::Reading(reading) => panel(reading),
    }
}

fn empty(message: &str) -> Markup {
    html! {
        div class="charger-panel__empty-message" { (message) }
    }
}

fn panel(view: &ChargerReadingView) -> Markup {
    html! {
        div class="charger-panel__head" {
            div class="charger-panel__title" { "EV charger" }
            div class=(format!("charger-panel__badge charger-panel__badge--{}", view.badge_variant)) {
                (view.status_label)
            }
        }
        @if let Some(hold) = &view.discharge_hold {
            div class="charger-panel__hold" { (hold) }
        }
        div class="charger-panel__stats" {
            (mini_stat::render(&view.power))
            (mini_stat::render(&view.currents))
            (mini_stat::render(&view.voltages))
            (mini_stat::render(&view.phases))
            (mini_stat::render(&view.session_energy))
            (mini_stat::render(&view.session_duration))
            (mini_stat::render(&view.last_seen))
        }
    }
}
