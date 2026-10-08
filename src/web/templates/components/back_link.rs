use maud::{Markup, html};

/// The way home from a page that has no dialog to close.
pub fn render() -> Markup {
    html! {
        a class="back-link" href="/" { "← Dashboard" }
    }
}
