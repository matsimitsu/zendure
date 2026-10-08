use maud::{Markup, html};

use crate::web::view::PackSummaryView;

/// Each pack's day at a glance. Rows are grid rows rather than a `<table>`
/// so the header and every row share one column template.
pub fn render(packs: &[PackSummaryView]) -> Markup {
    html! {
        div class="pack-table" {
            div class="pack-table__caption" { "Packs, last 24 hours" }
            div class="pack-table__scroll" {
                div class="pack-table__table" role="table" {
                    div class="pack-table__row pack-table__row--head" role="row" {
                        span role="columnheader" { "Pack" }
                        span role="columnheader" { "SOC range" }
                        span role="columnheader" { "Charged" }
                        span role="columnheader" { "Discharged" }
                        span role="columnheader" { "Temp range" }
                    }
                    @for pack in packs {
                        div class="pack-table__row" role="row" {
                            span class="pack-table__identity" role="cell" {
                                span class="pack-table__name" { (pack.name) }
                                @if !pack.serial.is_empty() {
                                    span class="pack-table__serial" { (pack.serial) }
                                }
                            }
                            span class="pack-table__value" role="cell" { (pack.soc_range) }
                            span class="pack-table__value" role="cell" { (pack.charged) }
                            span class="pack-table__value" role="cell" { (pack.discharged) }
                            span class="pack-table__value" role="cell" { (pack.temp_range) }
                        }
                    }
                }
            }
        }
    }
}
