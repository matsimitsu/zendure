use maud::{Markup, html};

use crate::web::axis::AxisTick;

/// A row of labels placed by percentage along a relative track, so a label's
/// width can never widen the chart the way a grid column's min-content would.
pub fn render(ticks: &[AxisTick]) -> Markup {
    html! {
        div class="axis" {
            @for tick in ticks {
                span
                    class=(format!(
                        "axis__tick axis__tick--{} axis__tick--{}",
                        tick.anchor.modifier(),
                        tick.density.modifier()
                    ))
                    style=(format!("left: {:.2}%", tick.position.percent())) { (tick.label) }
            }
        }
    }
}
