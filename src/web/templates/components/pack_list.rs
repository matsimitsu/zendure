use maud::{Markup, html};

use crate::web::templates::soc_bar;
use crate::web::view::PackRowView;

pub fn render(packs: &[PackRowView]) -> Markup {
    html! {
        div class="pack-list" {
            div class="pack-list__head" {
                span { "Pack" }
                span { "Charge" }
                span { "Power" }
                span { "Temp" }
                span { "Capacity" }
            }
            @for pack in packs {
                div class="pack-list__row" {
                    div class="pack-list__identity" {
                        div class="pack-list__name" { (pack.name) }
                        div class="pack-list__serial" { (pack.serial) }
                    }
                    div class="pack-list__soc" {
                        div class="pack-list__bar" {
                            (soc_bar::render(&pack.bar))
                        }
                        div class="pack-list__value" { (pack.soc) }
                    }
                    div class="pack-list__value" { (pack.power) }
                    div class="pack-list__value" { (pack.temperature) }
                    div class="pack-list__value pack-list__value--muted" { (pack.capacity) }
                }
            }
        }
    }
}
