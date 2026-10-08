use maud::{Markup, html};

use crate::web::templates::{line_chart, mini_stat, pack_table};
use crate::web::view::DetailView;

/// The modal body for one entity: its stats over the window, any per-pack
/// table, then its charts.
pub fn render(view: &DetailView) -> Markup {
    html! {
        div class="detail-view" {
            @if let Some(body) = &view.body {
                div class="detail-view__stats" {
                    @for stat in &body.stats {
                        (mini_stat::render(stat))
                    }
                }
                @if !body.packs.is_empty() {
                    (pack_table::render(&body.packs))
                }
                @for chart in &body.charts {
                    (line_chart::render(chart))
                }
            } @else {
                div class="detail-view__empty" { "No readings yet. History appears here once the first ones arrive." }
            }
        }
    }
}
