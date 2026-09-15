//! Solar production forecasts for the dashboard.
//!
//! `Prediction` is the capability, mirroring `BatteryController`/
//! `BatteryMonitor` (`crate::device`): one trait, several backends, dispatched
//! through an enum because the trait's `impl Future + Send` return isn't
//! `dyn`-safe. `[prediction] kind = "solcast"` selects the real backend
//! (`solcast.rs`), `"simulated"` a synthetic one (`simulated.rs`) for local
//! dev and testing with no network call and no daily quota to spend.
//!
//! Everything in *this* file is backend-agnostic: the poll schedule, the
//! persisted daily budget, and the poller loop only ever see
//! `Vec<SolarForecastPoint>`, never which backend produced them. This is
//! display-only — nothing here feeds `crate::controller`.

use std::collections::BTreeSet;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{NaiveDate, Timelike};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

use crate::config::PredictionConfig;
use crate::units::{SolarForecastPoint, Timestamp};
use crate::web::{DashboardStateSender, ForecastSnapshot};

pub mod simulated;
pub mod solcast;

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;

/// A local time-of-day anchor (hour, minute) — an entry in `poll_times`, and
/// what `fired` remembers having used today. Hand-written `"HH:MM"`
/// `Serialize`/`Deserialize` so the TOML config and the persisted state's
/// `fired` list share one representation. `Ord`/`Eq` derived so the scheduler
/// can compare anchors and a `BTreeSet` can hold them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TimeOfDay {
    hour: u32,
    minute: u32,
}

impl TimeOfDay {
    pub fn new(hour: u32, minute: u32) -> Result<Self, String> {
        if hour >= 24 {
            return Err(format!("hour must be 0-23, found {hour}"));
        }
        if minute >= 60 {
            return Err(format!("minute must be 0-59, found {minute}"));
        }
        Ok(TimeOfDay { hour, minute })
    }
}

impl std::fmt::Display for TimeOfDay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:02}:{:02}", self.hour, self.minute)
    }
}

impl std::str::FromStr for TimeOfDay {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let (h, m) = s
            .split_once(':')
            .ok_or_else(|| format!("must be \"HH:MM\", found {s:?}"))?;
        let hour = h
            .parse::<u32>()
            .map_err(|_| format!("must be \"HH:MM\", found {s:?}"))?;
        let minute = m
            .parse::<u32>()
            .map_err(|_| format!("must be \"HH:MM\", found {s:?}"))?;
        TimeOfDay::new(hour, minute)
    }
}

impl Serialize for TimeOfDay {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for TimeOfDay {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// The daily budget's default shape: five anchors spread across daylight,
/// spending Solcast's 10-request/day account cap (5/site) where cloud cover
/// actually changes the forecast, rather than evenly across the whole day
/// including the night hours nothing changes in.
pub fn default_poll_times() -> [TimeOfDay; 5] {
    [
        TimeOfDay::new(6, 0).expect("6:00 is valid"),
        TimeOfDay::new(9, 30).expect("9:30 is valid"),
        TimeOfDay::new(12, 30).expect("12:30 is valid"),
        TimeOfDay::new(15, 30).expect("15:30 is valid"),
        TimeOfDay::new(18, 30).expect("18:30 is valid"),
    ]
}

/// This poller's own clock read — deliberately not `crate::clock::Clock`,
/// which only resolves the hour (0-23) and is journalled/replayed
/// byte-for-byte; extending it with minutes would touch every journalled
/// `Event`. Read once per check, at the poller's own edge; every scheduling
/// decision below takes this as a plain argument, so it stays pure and
/// testable with no wall clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalNow {
    pub date: NaiveDate,
    pub time: TimeOfDay,
}

impl LocalNow {
    pub fn now(tz: Tz) -> Self {
        let local = chrono::Utc::now().with_timezone(&tz);
        LocalNow {
            date: local.date_naive(),
            time: TimeOfDay::new(local.hour(), local.minute())
                .expect("chrono's own hour()/minute() are always in range"),
        }
    }
}

/// What can go wrong fetching a forecast. Mirrors `PollError`'s rule: a
/// response this build cannot decode is the one most worth keeping, so the
/// raw body rides along on a parse failure.
#[derive(Debug)]
pub enum ForecastError {
    Request(String),
    Parse { body: String, error: String },
}

impl std::fmt::Display for ForecastError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ForecastError::Request(e) => write!(f, "request failed: {e}"),
            ForecastError::Parse { error, .. } => write!(f, "parse error: {error}"),
        }
    }
}

/// One combined forecast series for the whole day. What "combined" means —
/// one site, several sites summed, a synthetic curve — is entirely the
/// implementor's affair; this trait's only contract is "the day's predicted
/// solar production." Same shape as `BatteryMonitor::poll` (`&self`, `impl
/// Future + Send`) and for the same reason: an unboxed future per adapter, no
/// `dyn`.
pub trait Prediction {
    fn forecast(
        &self,
    ) -> impl Future<Output = Result<Vec<SolarForecastPoint>, ForecastError>> + Send;
}

/// Whichever backend `[prediction]` selected. Mirrors `registry::Battery`:
/// `Prediction::forecast`'s `impl Future` return isn't `dyn`-safe, so a real
/// seam needs an enum, not a trait object.
pub enum Forecaster {
    Solcast(solcast::SolcastForecaster),
    Simulated(simulated::SimulatedForecaster),
}

impl Prediction for Forecaster {
    async fn forecast(&self) -> Result<Vec<SolarForecastPoint>, ForecastError> {
        match self {
            Forecaster::Solcast(f) => f.forecast().await,
            Forecaster::Simulated(f) => f.forecast().await,
        }
    }
}

/// The only place a [`PredictionConfig`] becomes a live backend — `run.rs`
/// never matches on it directly, the same rule `registry::from_config`
/// follows for devices.
pub fn from_config(config: &PredictionConfig) -> Forecaster {
    match config {
        PredictionConfig::Solcast {
            api_key,
            site_east,
            site_west,
            ..
        } => Forecaster::Solcast(solcast::SolcastForecaster::new(
            api_key.clone(),
            site_east.clone(),
            site_west.clone(),
        )),
        PredictionConfig::Simulated { .. } => {
            Forecaster::Simulated(simulated::SimulatedForecaster::new())
        }
    }
}

/// On-disk shape of [`ForecastTracker`]'s state. `date` as `"YYYY-MM-DD"`
/// rather than `NaiveDate`'s own serde (which this crate doesn't otherwise
/// depend on) — one fewer format to keep stable across a dependency bump.
#[derive(Debug, Serialize, Deserialize)]
struct PersistedForecastState {
    date: String,
    fired: Vec<TimeOfDay>,
    points: Vec<SolarForecastPoint>,
    fetched_at: Option<i64>,
}

/// Tracks the daily poll budget and the last cached forecast, surviving a
/// restart the same way `rte::RteTracker` does: `load` on construction (a
/// missing file or corrupt JSON warns and starts fresh, never panics), `save`
/// after every change (a write failure warns once via a latch, then goes
/// quiet).
pub struct ForecastTracker {
    date: NaiveDate,
    fired: BTreeSet<TimeOfDay>,
    poll_times: Vec<TimeOfDay>,
    points: Vec<SolarForecastPoint>,
    fetched_at: Option<Timestamp>,
    state_path: PathBuf,
    save_failed: AtomicBool,
}

impl ForecastTracker {
    pub fn new(state_path: PathBuf, poll_times: Vec<TimeOfDay>) -> Self {
        let mut tracker = ForecastTracker {
            // A date with nothing fired yet — `next_due`/`mark_used` both
            // treat any date mismatch as "budget is fresh", so this default
            // never needs to be a real placeholder.
            date: NaiveDate::from_ymd_opt(1970, 1, 1).expect("1970-01-01 is a valid date"),
            fired: BTreeSet::new(),
            poll_times,
            points: Vec::new(),
            fetched_at: None,
            state_path,
            save_failed: AtomicBool::new(false),
        };
        if let Some(parent) = tracker.state_path.parent()
            && !parent.as_os_str().is_empty()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            tracing::warn!(
                "Forecast state directory {} unusable: {e}",
                parent.display()
            );
        }
        tracker.load();
        tracker
    }

    /// The latest anchor at-or-before `now.time` that hasn't fired today, or
    /// `None` if every anchor up to now has already fired (or none has
    /// passed yet). Returning the *latest* rather than the earliest collapses
    /// a catch-up after downtime into a single fetch: if the process was down
    /// through two earlier anchors, only the one closest to now is offered.
    pub fn next_due(&self, now: &LocalNow) -> Option<TimeOfDay> {
        if self.date != now.date {
            // A new day: nothing has fired yet, regardless of what `fired`
            // still holds from yesterday.
            return self
                .poll_times
                .iter()
                .rev()
                .find(|slot| **slot <= now.time)
                .copied();
        }
        self.poll_times
            .iter()
            .rev()
            .find(|slot| **slot <= now.time && !self.fired.contains(slot))
            .copied()
    }

    /// Records that `slot` (and every earlier anchor, per `next_due`'s
    /// catch-up collapse) has been used today. Rolls `fired` over first if
    /// `now` is a new day. Called after both a successful and a failed
    /// fetch — a failed fetch still spent the request.
    pub fn mark_used(&mut self, now: &LocalNow, slot: TimeOfDay) {
        if self.date != now.date {
            self.date = now.date;
            self.fired.clear();
        }
        for anchor in self.poll_times.iter().copied().filter(|a| *a <= slot) {
            self.fired.insert(anchor);
        }
        self.save();
    }

    /// Replaces the cached series on a successful fetch.
    pub fn set_forecast(&mut self, points: Vec<SolarForecastPoint>, at: Timestamp) {
        self.points = points;
        self.fetched_at = Some(at);
        self.save();
    }

    /// The dashboard's view of the cached series — whatever was last fetched
    /// successfully, empty until then.
    pub fn snapshot(&self) -> ForecastSnapshot {
        ForecastSnapshot {
            points: self.points.clone(),
            as_of: self.fetched_at,
        }
    }

    fn save(&self) {
        let state = PersistedForecastState {
            date: self.date.format("%Y-%m-%d").to_string(),
            fired: self.fired.iter().copied().collect(),
            points: self.points.clone(),
            fetched_at: self.fetched_at.map(Timestamp::as_millis),
        };
        match serde_json::to_string(&state) {
            Ok(json) => match std::fs::write(&self.state_path, json) {
                Ok(()) => self.save_failed.store(false, Ordering::Relaxed),
                Err(e) => {
                    if !self.save_failed.swap(true, Ordering::Relaxed) {
                        tracing::warn!(
                            "Failed to persist forecast state to {}: {e} (further failures logged at debug)",
                            self.state_path.display(),
                        );
                    } else {
                        tracing::debug!("Failed to persist forecast state: {e}");
                    }
                }
            },
            Err(e) => tracing::warn!("Failed to serialize forecast state: {e}"),
        }
    }

    fn load(&mut self) {
        let data = match std::fs::read_to_string(&self.state_path) {
            Ok(d) => d,
            Err(_) => return,
        };
        let state: PersistedForecastState = match serde_json::from_str(&data) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("Failed to parse forecast state file: {e}");
                return;
            }
        };
        let Ok(date) = NaiveDate::parse_from_str(&state.date, "%Y-%m-%d") else {
            tracing::warn!("Failed to parse forecast state date {:?}", state.date);
            return;
        };
        self.date = date;
        self.fired = state.fired.into_iter().collect();
        self.points = state.points;
        self.fetched_at = state.fetched_at.map(Timestamp::from_millis);
    }
}

/// How often the poller wakes to check whether an anchor is due — cheap (no
/// network), and much finer than the anchors themselves. `next_due` is what
/// actually gates a fetch.
const CHECK_INTERVAL: Duration = Duration::from_secs(60);

/// Runs until `shutdown` resolves: seeds the dashboard from whatever's
/// cached, then checks every [`CHECK_INTERVAL`] whether the next configured
/// anchor is due, fetching and updating both the tracker and the dashboard
/// when it is. Independent of `crate::event`/`crate::engine::Engine::step` —
/// the dashboard is this poller's only consumer, so it reaches it directly
/// rather than through the control loop's channel.
pub async fn run_forecast_poller(
    forecaster: Forecaster,
    timezone: Tz,
    state_path: PathBuf,
    poll_times: Vec<TimeOfDay>,
    dashboard_tx: DashboardStateSender,
    mut shutdown: tokio::sync::oneshot::Receiver<()>,
) {
    let mut tracker = ForecastTracker::new(state_path, poll_times);
    dashboard_tx.send_modify(|s| s.forecast_tick(tracker.snapshot()));

    let mut check = tokio::time::interval(CHECK_INTERVAL);
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            _ = check.tick() => {
                let now = LocalNow::now(timezone);
                let Some(slot) = tracker.next_due(&now) else { continue };
                tracing::info!("Fetching solar forecast ({slot} anchor)");
                match forecaster.forecast().await {
                    Ok(points) => {
                        tracker.set_forecast(points, Timestamp::from(chrono::Utc::now()));
                        tracker.mark_used(&now, slot);
                        dashboard_tx.send_modify(|s| s.forecast_tick(tracker.snapshot()));
                    }
                    Err(e) => {
                        tracing::warn!("Solar forecast fetch failed for {slot} anchor: {e}");
                        if let ForecastError::Parse { body, .. } = &e {
                            tracing::debug!("Undecodable forecast body: {body}");
                        }
                        tracker.mark_used(&now, slot);
                    }
                }
            }
        }
    }
}
