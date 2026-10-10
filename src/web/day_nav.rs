//! The day a panel shows and the days its nav steps reach, shared by every
//! day-stepping panel, and the `?key=YYYY-MM-DD` each is asked for with,
//! parsed once at the edge (`CONTROL-1`).

use chrono::NaiveDate;

/// A query string a panel cannot parse; the caller answers 400.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadQuery(pub(super) String);

impl std::fmt::Display for BadQuery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A fragment request names its day with this key: the URL already says
/// which panel it is for.
pub(super) const FRAGMENT_DAY_KEY: &str = "day";

/// The flows panel's resolution, `1h` or `15m`.
pub(super) const INTERVAL_KEY: &str = "interval";

/// Each day-stepping panel on the page, by the keys the page's query chooses
/// it with. They share one query, so no two panels share a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PagePanel {
    Flows,
    Prices,
    Solar,
}

impl PagePanel {
    const ALL: [PagePanel; 3] = [PagePanel::Flows, PagePanel::Prices, PagePanel::Solar];

    /// The key naming the day the panel shows on a full page.
    pub fn day_key(self) -> &'static str {
        match self {
            PagePanel::Flows => FRAGMENT_DAY_KEY,
            PagePanel::Prices => "price_day",
            PagePanel::Solar => "solar_day",
        }
    }

    fn keys(self) -> &'static [&'static str] {
        match self {
            PagePanel::Flows => &[FRAGMENT_DAY_KEY, INTERVAL_KEY],
            PagePanel::Prices => &["price_day"],
            PagePanel::Solar => &["solar_day"],
        }
    }

    /// What the panel's full-page links keep of `raw`: every other panel's
    /// choice. Only called once `raw` has parsed, so every value kept is one
    /// a panel accepted.
    pub fn kept_query(self, raw: Option<&str>) -> String {
        Self::ALL
            .into_iter()
            .filter(|panel| *panel != self)
            .flat_map(PagePanel::keys)
            .filter_map(|key| {
                query_values(raw, key)
                    .last()
                    .map(|value| format!("{key}={value}"))
            })
            .collect::<Vec<_>>()
            .join("&")
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

/// The last date `key` names in `raw`; `None` when it names none.
pub fn parse_day_param(raw: Option<&str>, key: &str) -> Result<Option<NaiveDate>, BadQuery> {
    query_values(raw, key).try_fold(None, |_, value| {
        NaiveDate::parse_from_str(value, "%Y-%m-%d")
            .map(Some)
            .map_err(|_| BadQuery(format!("{key}={value} is not a YYYY-MM-DD date")))
    })
}

/// The day a request asks a panel for, before today is known. `None` is
/// today.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DayQuery {
    day: Option<NaiveDate>,
}

impl DayQuery {
    /// `/fragments/<panel>?day=YYYY-MM-DD`.
    pub fn parse_fragment(raw: Option<&str>) -> Result<Self, BadQuery> {
        Ok(DayQuery {
            day: parse_day_param(raw, FRAGMENT_DAY_KEY)?,
        })
    }

    /// The whole page, where each panel reads its own key.
    pub fn parse_page(raw: Option<&str>, panel: PagePanel) -> Result<Self, BadQuery> {
        Ok(DayQuery {
            day: parse_day_param(raw, panel.day_key())?,
        })
    }

    #[cfg(test)]
    pub fn on(day: NaiveDate) -> Self {
        DayQuery { day: Some(day) }
    }

    /// The asked-for day pulled into `earliest..=latest`, so a stale or
    /// hand-typed link still lands on a day the panel can show.
    pub fn resolve(self, today: NaiveDate, earliest: NaiveDate, latest: NaiveDate) -> NaiveDate {
        self.day.map_or(today, |day| day.clamp(earliest, latest))
    }
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
    /// The other panels' query a full-page step link keeps, e.g.
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
