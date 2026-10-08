//! The energy-flows panel on a chosen day: the request's `day=` and
//! `interval=` parsed once at the edge (`CONTROL-1`), and past days folded
//! from the journal and kept, since a finished day never changes.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Mutex;

use chrono::NaiveDate;
use chrono_tz::Tz;

use crate::journal::read::read_oldest_event_at;
use crate::sync::guard;
use crate::units::Timestamp;
use crate::world::DeviceId;

use super::flows::{
    EnergyFlowsView, FlowResolution, FlowsRequest, local_date, requested_flows_view,
};
use super::intervals::{IntervalHistory, history_of_day};

/// A week of stepping back and forth at both intervals stays warm.
const CACHED_DAYS: usize = 8;

/// A query string the panel cannot show; the caller answers 400.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadFlowsQuery(String);

impl std::fmt::Display for BadFlowsQuery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// `?day=YYYY-MM-DD&interval=1h|15m`, both optional, before today is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlowsQuery {
    day: Option<NaiveDate>,
    interval: FlowResolution,
}

impl FlowsQuery {
    /// Other keys are ignored, so the page's own query can carry more. An
    /// empty value reads as absent, the way an empty form field submits.
    pub fn parse(raw: Option<&str>) -> Result<Self, BadFlowsQuery> {
        let mut query = FlowsQuery {
            day: None,
            interval: FlowResolution::Hour,
        };
        for pair in raw.unwrap_or_default().split('&') {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            match key {
                _ if value.is_empty() => {}
                "day" => {
                    let day = NaiveDate::parse_from_str(value, "%Y-%m-%d").map_err(|_| {
                        BadFlowsQuery(format!("day={value} is not a YYYY-MM-DD date"))
                    })?;
                    query.day = Some(day);
                }
                "interval" => query.interval = value.parse().map_err(BadFlowsQuery)?,
                _ => {}
            }
        }
        Ok(query)
    }

    /// A day after today clamps to today: nothing has been measured there,
    /// and the live view is the nearest thing that has.
    pub fn resolve(self, today: NaiveDate) -> FlowsRequest {
        FlowsRequest {
            day: self.day.map_or(today, |day| day.min(today)),
            interval: self.interval,
        }
    }
}

/// What every cached view's day navigation was rendered against. When either
/// moves, each entry's labels or step links may be stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RenderedFor {
    today: NaiveDate,
    earliest: Option<NaiveDate>,
}

/// A least-recently-used cache of at most `capacity` entries.
struct DayCache<V> {
    capacity: usize,
    rendered_for: Option<RenderedFor>,
    /// Most recently used last.
    entries: VecDeque<(FlowsRequest, V)>,
}

impl<V: Clone> DayCache<V> {
    fn new(capacity: usize) -> Self {
        DayCache {
            capacity,
            rendered_for: None,
            entries: VecDeque::with_capacity(capacity),
        }
    }

    fn get(&mut self, context: RenderedFor, key: FlowsRequest) -> Option<V> {
        if self.rendered_for != Some(context) {
            self.entries.clear();
            self.rendered_for = Some(context);
            return None;
        }
        let at = self.entries.iter().position(|(each, _)| *each == key)?;
        let entry = self.entries.remove(at)?;
        let value = entry.1.clone();
        self.entries.push_back(entry);
        Some(value)
    }

    fn insert(&mut self, context: RenderedFor, key: FlowsRequest, value: V) {
        if self.rendered_for != Some(context) {
            self.entries.clear();
            self.rendered_for = Some(context);
        }
        self.entries.retain(|(each, _)| *each != key);
        self.entries.push_back((key, value));
        while self.entries.len() > self.capacity {
            self.entries.pop_front();
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Past days of the flows chart, read from the journal the live loop writes.
pub struct PastDays {
    journal: PathBuf,
    devices: Vec<DeviceId>,
    cache: Mutex<DayCache<EnergyFlowsView>>,
}

impl PastDays {
    /// `devices` are the configured batteries, so a past day counts the same
    /// boxes the live chart does.
    pub fn new(journal: PathBuf, devices: impl IntoIterator<Item = DeviceId>) -> Self {
        PastDays {
            journal,
            devices: devices.into_iter().collect(),
            cache: Mutex::new(DayCache::new(CACHED_DAYS)),
        }
    }

    /// The panel for `request`, a day before today. Blocks on SQLite.
    ///
    /// A journal that cannot be read renders an empty day rather than an
    /// error, and is not cached, so the next visit tries again.
    pub fn view(&self, request: FlowsRequest, now: Timestamp, tz: Tz) -> EnergyFlowsView {
        let earliest = self.earliest(tz);
        let context = RenderedFor {
            today: local_date(now, tz).unwrap_or_default(),
            earliest,
        };
        if let Some(view) = guard(&self.cache).get(context, request) {
            return view;
        }
        match history_of_day(&self.journal, request.day, tz, self.devices.clone()) {
            Ok(history) => {
                let view = requested_flows_view(&history, request, earliest, now, tz);
                guard(&self.cache).insert(context, request, view.clone());
                view
            }
            Err(e) => {
                tracing::warn!("Dashboard: cannot read {} from journal: {e}", request.day);
                let empty = IntervalHistory::new(self.devices.clone());
                requested_flows_view(&empty, request, earliest, now, tz)
            }
        }
    }

    /// The oldest local day the journal still holds; `None` when it holds
    /// nothing or cannot be read, which leaves the step back enabled.
    fn earliest(&self, tz: Tz) -> Option<NaiveDate> {
        match read_oldest_event_at(&self.journal) {
            Ok(oldest) => oldest.and_then(|at| local_date(at, tz)),
            Err(e) => {
                tracing::debug!("Dashboard: cannot read the journal's oldest day: {e}");
                None
            }
        }
    }
}

#[cfg(test)]
#[path = "past_days_tests.rs"]
mod tests;
