use maud::{Markup, html};

use crate::web::view::TopBarView;

pub fn render(view: &TopBarView) -> Markup {
    let (status_text, status_modifier) = if view.operational {
        ("Operational", "operational")
    } else {
        ("Offline", "offline")
    };

    html! {
        div class="top-bar" {
            div class="top-bar__brand" {
                div class="top-bar__logo" { "⚡" }
                div class="top-bar__title" { "Home energy system" }
            }
            div class=(format!("top-bar__status top-bar__status--{}", status_modifier)) {
                span class=(format!("top-bar__status-dot top-bar__status-dot--{}", status_modifier)) {}
                (status_text)
            }
        }
    }
}
