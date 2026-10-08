use maud::{Markup, html};

use crate::web::entity::Entity;

/// Where a link that opens a detail swaps it: the `hx-target` every card
/// carries.
pub const TARGET: &str = "#detail-modal .modal__panel";

/// How the panel is dismissed: the dialog's own close button, or a link home
/// when the panel is a page of its own and there is no dialog to close.
#[derive(Clone, Copy)]
pub enum Presentation {
    Dialog,
    Page,
}

/// Rendered once, outside every `sse-swap` region, so a tick never closes it
/// or swaps its contents. The scrim is a `method="dialog"` form so a click
/// beside the panel closes it with no script.
pub fn shell() -> Markup {
    html! {
        dialog id="detail-modal" class="modal" hx-on::after-swap="if (!this.open) this.showModal()" {
            form method="dialog" class="modal__scrim" {
                button class="modal__scrim-button" tabindex="-1" aria-hidden="true" {}
            }
            div class="modal__panel" {}
        }
    }
}

/// The panel's contents: what `/detail/{entity}` swaps into
/// `.modal__panel`.
pub fn panel(entity: Entity, body: Markup, presentation: Presentation) -> Markup {
    html! {
        div class="modal__header" {
            div class=(format!("modal__icon modal__icon--{}", entity.slug())) { (entity.glyph()) }
            div class="modal__heading" {
                div class="modal__title" { (entity.title()) }
                div class="modal__subtitle" { "Last 24 hours · 15-minute resolution" }
            }
            @if let Presentation::Dialog = presentation {
                form method="dialog" {
                    button class="modal__close" aria-label="Close" { "✕" }
                }
            }
        }
        div class="modal__body" { (body) }
    }
}
