use maud::{Markup, html};

/// Where a nav control leads: the whole page for a browser following the link
/// itself, and the panel's fragment for htmx to swap in place.
#[derive(Debug, Clone, PartialEq)]
pub struct DayNavLink {
    pub href: String,
    pub hx_get: String,
}

/// What a panel's day nav shows. The caller owns every URL, so a panel
/// decides for itself which days exist: `None` renders a step disabled, or
/// leaves the Today button out.
pub struct DayNav<'a> {
    pub label: &'a str,
    pub date: &'a str,
    pub prev: Option<DayNavLink>,
    pub next: Option<DayNavLink>,
    pub today: Option<DayNavLink>,
    /// The htmx target the links swap into, e.g. `closest energy-flows`.
    pub hx_target: &'a str,
    /// `data-day` and `data-live` travel on the nav, which every swap
    /// replaces, so a persistent host can mirror them.
    pub data_day: Option<String>,
    pub data_live: Option<bool>,
}

/// The whole page with `parts` as its query, skipping empty ones, e.g.
/// `/?price_day=2025-09-05&day=2025-09-01`.
pub fn page_href(parts: &[&str]) -> String {
    let query: Vec<&str> = parts
        .iter()
        .copied()
        .filter(|part| !part.is_empty())
        .collect();
    if query.is_empty() {
        "/".to_string()
    } else {
        format!("/?{}", query.join("&"))
    }
}

pub fn render(nav: &DayNav) -> Markup {
    html! {
        nav class="day-nav" aria-label="Day" data-day=[&nav.data_day] data-live=[nav.data_live] {
            (step(nav.prev.as_ref(), "‹", "Previous day", nav.hx_target))
            div class="day-nav__day" {
                span class="day-nav__day-label" { (nav.label) }
                span class="day-nav__day-date" { (nav.date) }
            }
            (step(nav.next.as_ref(), "›", "Next day", nav.hx_target))
            @if let Some(today) = &nav.today {
                a class="day-nav__today"
                    href=(today.href)
                    hx-get=(today.hx_get)
                    hx-target=(nav.hx_target) { "Today" }
            }
        }
    }
}

/// A link to `target`, or an inert glyph where there is no day to go to.
fn step(target: Option<&DayNavLink>, glyph: &str, label: &str, hx_target: &str) -> Markup {
    html! {
        @match target {
            Some(link) => a class="day-nav__step"
                href=(link.href)
                hx-get=(link.hx_get)
                hx-target=(hx_target)
                aria-label=(label) { (glyph) },
            None => span class="day-nav__step day-nav__step--disabled"
                role="link"
                aria-disabled="true"
                aria-label=(label) { (glyph) },
        }
    }
}
