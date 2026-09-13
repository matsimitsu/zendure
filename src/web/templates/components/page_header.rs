use maud::{Markup, html};

use crate::web::view::PageHeaderView;

pub fn render(view: &PageHeaderView) -> Markup {
    html! {
        div class="page-header" {
            div class="page-header__eyebrow" { "Overview" }
            div class="page-header__title" { "Live system state" }
            div class="page-header__description" { (view.last_updated) }
        }
    }
}
