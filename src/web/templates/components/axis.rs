use maud::{Markup, html};

use crate::web::axis::AxisTick;

/// A row of labels placed by percentage along a relative track, so a label's
/// width can never widen the chart the way a grid column's min-content would.
pub fn render(ticks: &[AxisTick]) -> Markup {
    track("axis", "left", ticks)
}

/// The same track standing upright: positions run top to bottom, and the
/// parent's height is the track's length.
pub fn render_vertical(ticks: &[AxisTick]) -> Markup {
    track("axis axis--vertical", "top", ticks)
}

fn track(class: &str, edge: &str, ticks: &[AxisTick]) -> Markup {
    html! {
        div class=(class) {
            @for tick in ticks {
                span
                    class=(format!(
                        "axis__tick axis__tick--{} axis__tick--{}",
                        tick.anchor.modifier(),
                        tick.density.modifier()
                    ))
                    style=(format!("{edge}: {:.2}%", tick.position.percent())) { (tick.label) }
            }
        }
    }
}
