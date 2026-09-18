use maud::{Markup, html};

use crate::web::view::DecisionLogRowView;

pub fn render(rows: &[DecisionLogRowView]) -> Markup {
    html! {
        div class="decision-log" {
            div class="decision-log__header" {
                div class="decision-log__title" { "Decision log" }
                div class="decision-log__subtitle" { "Why the controller did what it did" }
            }
            div class="decision-log__rows" {
                @if rows.is_empty() {
                    div class="decision-log__empty" { "No decisions yet" }
                } @else {
                    @for row in rows {
                        div class="decision-log__row" {
                            span class="decision-log__time" { (row.time) }
                            span class=(format!("decision-log__badge decision-log__badge--{}", row.badge_variant)) {
                                (row.mode_label)
                            }
                            span class="decision-log__reason" {
                                span class="decision-log__reason-text" { (row.reason) }
                                @if let Some(repeat) = &row.repeat {
                                    span class="decision-log__repeat" title=(repeat.span) { (repeat.label) }
                                }
                            }
                            span class="decision-log__power" { (row.power) }
                        }
                    }
                }
            }
        }
    }
}
