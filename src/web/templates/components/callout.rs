use maud::{Markup, html};

/// A static teaser banner — copy only, no functionality in this build.
pub fn render() -> Markup {
    html! {
        div class="callout callout--info" {
            span class="callout__icon" { "ⓘ" }
            div class="callout__body" {
                div class="callout__title" { "Goal-based scheduling is coming" }
                div class="callout__text" { "Set targets like \"car at 80% by 07:00\" and the planner will replot this schedule around them." }
            }
        }
    }
}
