//! The energy-flows panel on a chosen day: the request's `day=` and
//! `interval=` parsed once at the edge (`CONTROL-1`), and past days folded
//! from the journal and kept, since a finished day never changes.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use chrono::NaiveDate;
use chrono_tz::Tz;

use crate::clock::{local_date, local_day_bounds};
use crate::journal::read::{ReadError, read_oldest_event_at};
use crate::sync::guard;
use crate::units::{Elapsed, Timestamp};
use crate::world::DeviceId;

use super::day_nav::{BadQuery, INTERVAL_KEY, PagePanel, parse_day_param, query_values};
use super::flows::{EnergyFlowsView, FlowResolution, FlowsRequest, requested_flows_view};
use super::intervals::{IntervalHistory, history_of_day};

/// A week of stepping back and forth stays warm.
const CACHED_DAYS: usize = 8;

/// How long after a day's end it is first cached: the journal is written
/// behind the live loop, so the day's last rows can land after its midnight.
const SETTLE: Duration = Duration::from_secs(5 * 60);

/// `?day=YYYY-MM-DD&interval=1h|15m`, both optional, before today is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlowsQuery {
    day: Option<NaiveDate>,
    interval: FlowResolution,
}

impl FlowsQuery {
    /// Other keys are ignored, so the page's own query can carry more.
    pub fn parse(raw: Option<&str>) -> Result<Self, BadQuery> {
        let day = parse_day_param(raw, PagePanel::Flows.day_key())?;
        let interval = query_values(raw, INTERVAL_KEY)
            .try_fold(FlowResolution::Hour, |_, value| {
                value.parse().map_err(BadQuery)
            })?;
        Ok(FlowsQuery { day, interval })
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

/// A least-recently-used cache of at most `capacity` days.
struct DayCache<V> {
    capacity: usize,
    rendered_for: Option<RenderedFor>,
    /// Most recently used last.
    entries: VecDeque<(NaiveDate, V)>,
}

impl<V: Clone> DayCache<V> {
    fn new(capacity: usize) -> Self {
        DayCache {
            capacity,
            rendered_for: None,
            entries: VecDeque::with_capacity(capacity),
        }
    }

    fn get(&mut self, context: RenderedFor, key: NaiveDate) -> Option<V> {
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

    fn insert(&mut self, context: RenderedFor, key: NaiveDate, value: V) {
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
    /// A journal that cannot be read renders the day as unreadable, and is
    /// not cached, so the next visit tries again.
    pub fn view(&self, request: FlowsRequest, now: Timestamp, tz: Tz) -> EnergyFlowsView {
        let today = local_date(now, tz).unwrap_or_default();
        let earliest = self.earliest(tz);
        let context = earliest
            .as_ref()
            .ok()
            .map(|&earliest| RenderedFor { today, earliest });
        if let Some(context) = context
            && let Some(view) = guard(&self.cache).get(context, request.day)
        {
            return view.at_interval(request.interval);
        }
        // With the oldest day unknown, a step back could lead anywhere,
        // including to days the journal no longer holds.
        let earliest = earliest.unwrap_or(Some(request.day));
        match history_of_day(&self.journal, request.day, tz, self.devices.clone()) {
            Ok(history) => {
                let view = requested_flows_view(&history, request, earliest, now, tz);
                if let Some(context) = context
                    && settled(request.day, now, tz)
                {
                    guard(&self.cache).insert(context, request.day, view.clone());
                }
                view
            }
            Err(e) => {
                tracing::warn!("Dashboard: cannot read {} from journal: {e}", request.day);
                let empty = IntervalHistory::new(self.devices.clone());
                EnergyFlowsView {
                    unreadable: true,
                    ..requested_flows_view(&empty, request, earliest, now, tz)
                }
            }
        }
    }

    /// The oldest local day the journal still holds; `Ok(None)` when it holds
    /// nothing.
    fn earliest(&self, tz: Tz) -> Result<Option<NaiveDate>, ReadError> {
        read_oldest_event_at(&self.journal)
            .map(|oldest| oldest.and_then(|at| local_date(at, tz)))
            .inspect_err(|e| {
                tracing::debug!("Dashboard: cannot read the journal's oldest day: {e}")
            })
    }
}

/// Whether `day` ended long enough before `now` that its journal is complete.
fn settled(day: NaiveDate, now: Timestamp, tz: Tz) -> bool {
    local_day_bounds(day, tz).is_some_and(|(_, end)| end + Elapsed::of(SETTLE) <= now)
}

#[cfg(test)]
#[path = "past_days_tests.rs"]
mod tests;
