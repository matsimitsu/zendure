use maud::{Markup, html};

use crate::web::templates::{line_chart, mini_stat};
use crate::web::view::DetailView;

/// The modal body for one entity: its stats over the window, then its charts.
pub fn render(view: &DetailView) -> Markup {
    html! {
        @if let Some(body) = &view.body {
            div class="detail-view" {
                div class="detail-view__stats" {
                    @for stat in &body.stats {
                        (mini_stat::render(stat))
                    }
                }
                @for chart in &body.charts {
                    (line_chart::render(chart))
                }
            }
        }
    }
}
