use maud::{Markup, html};

use crate::web::view::StatCardView;

pub fn render(view: &StatCardView) -> Markup {
    html! {
        div class=(format!("stat-card stat-card--{}", view.variant)) {
            div class="stat-card__head" {
                div class="stat-card__icon" { (view.glyph) }
                div class="stat-card__label" { (view.label) }
            }
            div class="stat-card__value-row" {
                span class="stat-card__value" { (view.value) }
                span class="stat-card__unit" { (view.unit) }
            }
            svg class="stat-card__sparkline" viewBox="0 0 96 28" preserveAspectRatio="none" {
                path d=(view.sparkline_path) fill="none" stroke-width="1.5" stroke-linejoin="round" stroke-linecap="round" {}
            }
            div class="stat-card__detail" { (view.detail) }
        }
    }
}
