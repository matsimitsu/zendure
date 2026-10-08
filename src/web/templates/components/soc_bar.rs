use maud::{Markup, html};

use crate::web::soc_bar::{SocBarLabel, SocBarLabels, SocBarView};

pub fn render(view: &SocBarView) -> Markup {
    let min = view.limits.min.get();
    let max = view.limits.max.get();
    let class = if view.labels.is_some() {
        "soc-bar"
    } else {
        "soc-bar soc-bar--compact"
    };
    html! {
        div class=(class) {
            div class="soc-bar__plot" {
            div class="soc-bar__track" {
                div class="soc-bar__fill" style=(format!("width: {}%", view.fill)) {}
                @if view.labels.is_some() {
                    @if min > 0 {
                        div class="soc-bar__stripes soc-bar__stripes--reserve"
                            style=(format!("width: {min}%")) {}
                    }
                    @if max < 100 {
                        div class="soc-bar__stripes soc-bar__stripes--headroom"
                            style=(format!("width: {}%", 100 - max)) {}
                    }
                }
            }
                div class="soc-bar__limit" style=(format!("left: {min}%")) {}
                div class="soc-bar__limit" style=(format!("left: {max}%")) {}
            }
            @if let Some(labels) = &view.labels {
                (label_row(labels, min, max))
            }
        }
    }
}

fn label_row(labels: &SocBarLabels, min: u32, max: u32) -> Markup {
    let class = if labels.stacked {
        "soc-bar__labels soc-bar__labels--stacked"
    } else {
        "soc-bar__labels"
    };
    html! {
        div class=(class) {
            (label(&labels.min, min, false))
            (label(&labels.max, max, labels.stacked))
        }
    }
}

fn label(label: &SocBarLabel, at: u32, lower: bool) -> Markup {
    let class = format!(
        "soc-bar__label soc-bar__label--{}{}",
        label.anchor.modifier(),
        if lower { " soc-bar__label--lower" } else { "" }
    );
    html! {
        span class=(class) style=(format!("left: {at}%")) {
            span class="soc-bar__text--wide" { (label.long) }
            span class="soc-bar__text--narrow" { (label.short) }
        }
    }
}
