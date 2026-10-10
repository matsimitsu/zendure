use maud::{Markup, html};

use crate::prices::tiers::Tier;

/// The cheap/normal/expensive pill. Rendered hidden without a tier, so the
/// readout script always has a pill to retint.
pub fn render(tier: Option<Tier>) -> Markup {
    html! {
        @match tier {
            Some(tier) => span class=(format!("price-tier price-tier--{}", tier.class_suffix())) {
                (tier.label())
            },
            None => span class="price-tier" hidden {},
        }
    }
}
