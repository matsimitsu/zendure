//! The day a panel shows and the days its nav steps reach, shared by the
//! energy-flows and price panels, and the `?key=YYYY-MM-DD` either is asked
//! for with, parsed once at the edge (`CONTROL-1`).

use chrono::NaiveDate;

/// A query string a panel cannot parse; the caller answers 400.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadQuery(pub(super) String);

impl std::fmt::Display for BadQuery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Every non-empty value `key` has in `raw`, in order. Other keys are
/// ignored, so a page's query can carry more than one panel's, and an empty
/// value reads as absent, the way an empty form field submits.
pub(super) fn query_values<'a>(
    raw: Option<&'a str>,
    key: &'a str,
) -> impl Iterator<Item = &'a str> + 'a {
    raw.unwrap_or_default().split('&').filter_map(move |pair| {
        let (each, value) = pair.split_once('=').unwrap_or((pair, ""));
        (each == key && !value.is_empty()).then_some(value)
    })
}

/// `key=value` for the last value each of `keys` has in `raw`, joined with
/// `&`, so a full-page link can keep another panel's choice. Only called once
/// `raw` has parsed, so every value kept is one a panel accepted.
pub fn kept_query(raw: Option<&str>, keys: &[&str]) -> String {
    keys.iter()
        .filter_map(|key| {
            query_values(raw, key)
                .last()
                .map(|value| format!("{key}={value}"))
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// The last date `key` names in `raw`; `None` when it names none.
pub fn parse_day_param(raw: Option<&str>, key: &str) -> Result<Option<NaiveDate>, BadQuery> {
    query_values(raw, key).try_fold(None, |_, value| {
        NaiveDate::parse_from_str(value, "%Y-%m-%d")
            .map(Some)
            .map_err(|_| BadQuery(format!("{key}={value} is not a YYYY-MM-DD date")))
    })
}

/// Which day a panel shows, and which days its steps reach.
#[derive(Debug, Clone, PartialEq)]
pub struct DayNavView {
    pub shown: NaiveDate,
    pub today: NaiveDate,
    /// `None` on the earliest day the panel reaches.
    pub previous: Option<NaiveDate>,
    /// `None` on the latest day the panel reaches.
    pub next: Option<NaiveDate>,
    /// "Today", "Tomorrow", "Yesterday" or "Wed 7 Oct".
    pub label: String,
    /// "7 Oct".
    pub date: String,
    /// The other panel's query a full-page step link keeps, e.g.
    /// `price_day=2025-09-05`; empty outside a full page.
    pub keep: String,
}

impl DayNavView {
    /// `earliest` is `None` when no lower bound is known.
    pub fn new(
        shown: NaiveDate,
        today: NaiveDate,
        earliest: Option<NaiveDate>,
        latest: NaiveDate,
    ) -> Self {
        let label = if shown == today {
            "Today".to_string()
        } else if today.succ_opt() == Some(shown) {
            "Tomorrow".to_string()
        } else if today.pred_opt() == Some(shown) {
            "Yesterday".to_string()
        } else {
            shown.format("%a %-d %b").to_string()
        };
        DayNavView {
            shown,
            today,
            previous: shown
                .pred_opt()
                .filter(|_| earliest.is_none_or(|earliest| shown > earliest)),
            next: shown.succ_opt().filter(|_| shown < latest),
            label,
            date: shown.format("%-d %b").to_string(),
            keep: String::new(),
        }
    }

    /// Only today has a now, and only today takes the live stream.
    pub fn is_today(&self) -> bool {
        self.shown == self.today
    }
}
