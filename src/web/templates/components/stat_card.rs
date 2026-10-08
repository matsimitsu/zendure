use maud::{Markup, html};

use crate::web::templates::modal;
use crate::web::view::StatCardView;

/// The card's shell, rendered once with the page. Only the `__live` wrapper
/// inside it is an `sse-swap` region: were the link itself swapped each tick,
/// a click spanning a swap would land on the row instead, and a focused card
/// would drop focus to `body`. `hx-target="this"` because the SSE extension
/// swaps into the inherited `hx-target`, which on a link is the modal.
pub fn render(view: &StatCardView, swap: &str) -> Markup {
    let class = format!("stat-card stat-card--{}", view.variant);
    html! {
        @if let Some(entity) = view.detail_entity {
            a class=(format!("{class} stat-card--link")) href=(entity.path()) hx-get=(entity.path()) hx-target=(modal::TARGET) {
                div class="stat-card__live" sse-swap=(swap) hx-target="this" { (contents(view)) }
            }
        } @else {
            div class=(class) {
                div class="stat-card__live" sse-swap=(swap) hx-target="this" { (contents(view)) }
            }
        }
    }
}

/// What a tick replaces: everything inside the card but the card.
pub fn contents(view: &StatCardView) -> Markup {
    html! {
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
