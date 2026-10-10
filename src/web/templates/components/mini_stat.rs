use maud::{Markup, html};

use crate::prices::tiers::Tier;
use crate::web::view::MiniStatView;

pub fn render(view: &MiniStatView) -> Markup {
    html! {
        div class="mini-stat" {
            div class="mini-stat__label" { (view.label) }
            div class="mini-stat__value" { (view.value) }
            @if let Some(sub) = &view.sub {
                div class=(sub_class(sub.tone)) { (sub.text) }
            }
        }
    }
}

fn sub_class(tone: Option<Tier>) -> String {
    match tone {
        Some(tone) => format!("mini-stat__sub mini-stat__sub--{}", tone.class_suffix()),
        None => "mini-stat__sub".to_string(),
    }
}
