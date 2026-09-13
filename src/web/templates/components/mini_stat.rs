use maud::{Markup, html};

use crate::web::view::MiniStatView;

pub fn render(view: &MiniStatView) -> Markup {
    html! {
        div class="mini-stat" {
            div class="mini-stat__label" { (view.label) }
            div class="mini-stat__value" { (view.value) }
        }
    }
}
