//! Solar production forecasts for the dashboard.
//!
//! `Prediction` is the capability, mirroring `BatteryController`/
//! `BatteryMonitor` (`crate::device`): one trait, several backends, dispatched
//! through an enum because the trait's `impl Future + Send` return isn't
//! `dyn`-safe. `[prediction] kind = "solcast"` selects the real backend
//! (`solcast.rs`), `"simulated"` a synthetic one (`simulated.rs`) for local
//! dev and testing with no network call and no daily quota to spend.
//!
//! Everything in *this* file is backend-agnostic: the persisted daily budget
//! and the poller loop only ever see
//! `Vec<SolarForecastPoint>`, never which backend produced them. This is
//! display-only — nothing here feeds `crate::controller`.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::NaiveDate;
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

use crate::config::PredictionConfig;
use crate::fetch::FetchError;
use crate::journal::Journal;
use crate::schedule::{
    AnchorKey, AnchorSchedule, Failure, Fetch, LocalNow, RetryPolicy, TimeOfDay,
};
use crate::units::{SolarForecastPoint, Timestamp};
use crate::web::{DashboardStateSender, ForecastSnapshot};

pub mod simulated;
pub mod solcast;

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;

/// How long each forecast point averages over: Solcast's `PT30M`, and the
/// slot every consumer buckets a point into.
pub const FORECAST_PERIOD: Duration = Duration::from_secs(30 * 60);

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

/// One combined forecast series for the whole day. What "combined" means —
/// one site, several sites summed, a synthetic curve — is entirely the
/// implementor's affair; this trait's only contract is "the day's predicted
/// solar production." Same shape as `BatteryMonitor::poll` (`&self`, `impl
/// Future + Send`) and for the same reason: an unboxed future per adapter, no
/// `dyn`.
pub trait Prediction {
    /// `anchor` names the fetch being made, so a backend that fetches in
    /// parts can tell a repeat of the same anchor from a new one.
    fn forecast(
        &self,
        anchor: AnchorKey,
    ) -> impl Future<Output = Result<Vec<SolarForecastPoint>, FetchError>> + Send;
}

/// Whichever backend `[prediction]` selected. Mirrors `registry::Battery`:
/// `Prediction::forecast`'s `impl Future` return isn't `dyn`-safe, so a real
/// seam needs an enum, not a trait object.
pub enum Forecaster {
    Solcast(solcast::SolcastForecaster),
    Simulated(simulated::SimulatedForecaster),
}

impl Prediction for Forecaster {
    async fn forecast(&self, anchor: AnchorKey) -> Result<Vec<SolarForecastPoint>, FetchError> {
        match self {
            Forecaster::Solcast(f) => f.forecast(anchor).await,
            Forecaster::Simulated(f) => f.forecast(anchor).await,
        }
    }
}

/// The only place a [`PredictionConfig`] becomes a live backend — `run.rs`
/// never matches on it directly, the same rule `registry::from_config`
/// follows for devices.
pub fn from_config(config: &PredictionConfig, timezone: Tz) -> Forecaster {
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
            Forecaster::Simulated(simulated::SimulatedForecaster::new(timezone))
        }
    }
}

/// On-disk shape of [`ForecastTracker`]'s state. `date` as `"YYYY-MM-DD"`
/// rather than `NaiveDate`'s own serde (which this crate doesn't otherwise
/// depend on) — one fewer format to keep stable across a dependency bump.
#[derive(Debug, Serialize, Deserialize)]
struct PersistedForecastState {
    date: String,
    // Named `fired` on disk since before retries existed; it holds the
    // settled anchors, and renaming it would orphan every existing state file.
    #[serde(rename = "fired")]
    settled: Vec<TimeOfDay>,
    points: Vec<SolarForecastPoint>,
    fetched_at: Option<i64>,
}

/// Tracks the daily poll budget and the last cached forecast, surviving a
/// restart the same way `rte::RteTracker` does: `load` on construction (a
/// missing file or corrupt JSON warns and starts fresh, never panics), `save`
/// after every change (a write failure warns once via a latch, then goes
/// quiet).
pub struct ForecastTracker {
    schedule: AnchorSchedule,
    points: Vec<SolarForecastPoint>,
    fetched_at: Option<Timestamp>,
    state_path: PathBuf,
    save_failed: AtomicBool,
    timezone: Tz,
}

impl ForecastTracker {
    pub fn new(state_path: PathBuf, poll_times: Vec<TimeOfDay>, timezone: Tz) -> Self {
        let mut tracker = ForecastTracker {
            schedule: AnchorSchedule::new(poll_times),
            points: Vec::new(),
            fetched_at: None,
            state_path,
            save_failed: AtomicBool::new(false),
            timezone,
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

    pub fn schedule(&self) -> &AnchorSchedule {
        &self.schedule
    }

    /// Settles `slot` and persists it. Runs on a successful fetch, or when a
    /// failure is one a retry cannot fix; only a successful response spends
    /// Solcast's daily request cap, and a settled anchor is never fetched again.
    pub fn mark_used(&mut self, now: &LocalNow, slot: TimeOfDay) {
        self.schedule.mark_used(now, slot);
        self.save();
    }

    /// Records a failed fetch for `anchor`. Persists only when the retries ran
    /// out, because that settles the anchor. A pending retry is not persisted:
    /// a restart fetches the anchor again, both sites included.
    pub fn mark_failed(
        &mut self,
        now: &LocalNow,
        anchor: AnchorKey,
        policy: RetryPolicy,
    ) -> Failure {
        let failure = self.schedule.mark_failed(now, anchor, policy);
        if failure == Failure::GaveUp {
            self.save();
        }
        failure
    }

    /// Merges a fresh fetch into the cached series by timestamp — a fresh
    /// point wins on a timestamp collision, but a timestamp only the old
    /// series has (an hour Solcast's forward-looking response no longer
    /// covers) is kept, not dropped, so an already-elapsed hour doesn't lose
    /// its bar the moment a new poll lands. Anything from before today is
    /// pruned at the same time: durable history lives in the journal (see
    /// `run_forecast_poller`), so this cache only needs to cover what the
    /// dashboard currently displays.
    pub fn set_forecast(&mut self, points: Vec<SolarForecastPoint>, at: Timestamp) {
        let cutoff = crate::clock::local_midnight(at, self.timezone);
        let merged: std::collections::BTreeMap<Timestamp, SolarForecastPoint> = self
            .points
            .iter()
            .chain(points.iter())
            .filter(|p| p.at >= cutoff)
            .map(|p| (p.at, *p))
            .collect();
        self.points = merged.into_values().collect();
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
            date: self.schedule.date().format("%Y-%m-%d").to_string(),
            settled: self.schedule.settled().collect(),
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
        self.schedule.resume(date, state.settled);
        self.points = state.points;
        self.fetched_at = state.fetched_at.map(Timestamp::from_millis);
    }
}

/// How often the poller wakes to check whether an anchor is due — cheap (no
/// network), and much finer than the anchors themselves. The schedule's
/// decision is what actually gates a fetch.
const CHECK_INTERVAL: Duration = Duration::from_secs(60);

/// A failed anchor fetch is retried because the next anchor is hours away and
/// a 06:00 hiccup would otherwise leave the morning without a forecast. Two
/// retries 10 minutes apart ride out a network hiccup, and the per-site retry
/// (`SolcastForecaster` keeps the site that answered and refetches only the
/// other) keeps successful requests within the 10/day cap, short of a restart
/// mid-retry, which forgets the site that answered.
const RETRY_POLICY: RetryPolicy = RetryPolicy {
    delay: Duration::from_secs(10 * 60),
    attempts: 2,
};

/// What the poller carries between anchors.
struct Poller<P> {
    forecaster: P,
    timezone: Tz,
    tracker: ForecastTracker,
    dashboard_tx: DashboardStateSender,
    journal: Arc<Journal>,
}

impl<P: Prediction> Poller<P> {
    /// Fetches for an anchor that is due, or for the retry of a failed one
    /// whose delay has elapsed. A retry passes its anchor's own key, so the
    /// forecaster refetches only what that anchor is still missing.
    async fn poll_at(&mut self, now: LocalNow) {
        let (anchor, retry) = match self.tracker.schedule().decide(&now) {
            Fetch::Nothing => return,
            Fetch::Anchor(anchor) => (anchor, None),
            Fetch::Retry {
                anchor,
                attempt,
                of,
            } => (anchor, Some((attempt, of))),
        };
        let slot = anchor.slot;
        match retry {
            None => tracing::info!("Fetching solar forecast ({slot} anchor)"),
            Some((attempt, of)) => {
                tracing::info!("Fetching solar forecast ({slot} anchor, retry {attempt}/{of})");
            }
        }
        match self.forecaster.forecast(anchor).await {
            Ok(points) => {
                // Durable history, independent of `tracker`'s own
                // today-only cache — the future day-per-page view
                // reads this back, not `forecast.json`.
                match serde_json::to_string(&points) {
                    Ok(json) => self.journal.raw("solar_forecast", &json),
                    Err(e) => {
                        tracing::warn!("Cannot serialize solar forecast for the journal: {e}")
                    }
                }
                self.tracker
                    .set_forecast(points, Timestamp::from(chrono::Utc::now()));
                self.tracker.mark_used(&now, slot);
                self.dashboard_tx
                    .send_modify(|s| s.forecast_tick(self.tracker.snapshot()));
            }
            Err(e) => {
                tracing::warn!("Solar forecast fetch failed for {slot} anchor: {e}");
                if let FetchError::Parse { body, .. } = &e {
                    tracing::debug!("Undecodable forecast body: {body}");
                }
                if !e.is_transient() {
                    self.tracker.mark_used(&now, slot);
                } else if self.tracker.mark_failed(&now, anchor, RETRY_POLICY) == Failure::GaveUp {
                    tracing::warn!("Giving up on the {slot} anchor's solar forecast");
                }
            }
        }
    }
}

/// Runs until `shutdown` resolves: seeds the dashboard from whatever's
/// cached, then checks every [`CHECK_INTERVAL`] whether the schedule has an
/// anchor, or the retry of a failed one, to fetch, updating both the tracker
/// and the dashboard on success. Independent of `crate::event`/
/// `crate::engine::Engine::step` — the dashboard is this poller's only
/// consumer, so it reaches it directly rather than through the control
/// loop's channel.
pub async fn run_forecast_poller(
    forecaster: Forecaster,
    timezone: Tz,
    state_path: PathBuf,
    poll_times: Vec<TimeOfDay>,
    dashboard_tx: DashboardStateSender,
    journal: Arc<Journal>,
    mut shutdown: tokio::sync::oneshot::Receiver<()>,
) {
    let tracker = ForecastTracker::new(state_path, poll_times, timezone);
    dashboard_tx.send_modify(|s| s.forecast_tick(tracker.snapshot()));
    let mut poller = Poller {
        forecaster,
        timezone,
        tracker,
        dashboard_tx,
        journal,
    };

    let mut check = tokio::time::interval(CHECK_INTERVAL);
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            _ = check.tick() => {
                let now = LocalNow::now(poller.timezone);
                // Shutdown cancels a fetch in flight rather than waiting it out.
                tokio::select! {
                    biased;
                    _ = &mut shutdown => break,
                    () = poller.poll_at(now) => {}
                }
            }
        }
    }
}
