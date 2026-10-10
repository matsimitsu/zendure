//! Electricity prices for the dashboard and offline analysis.
//!
//! `PriceSource` is the capability, shaped like `crate::prediction::Prediction`:
//! one trait, several backends, dispatched through an enum because the trait's
//! `impl Future + Send` return isn't `dyn`-safe. Everything past this seam only
//! sees `Vec<PricePoint>`, never which backend produced them.
//!
//! `run_price_poller` fetches local today and tomorrow at each `poll_times`
//! anchor, journals every successful fetch as an `energy_price` row, and on
//! startup backfills past days the journal has no prices for.
//!
//! Display and analysis only: nothing here feeds `crate::controller`.

use std::collections::BTreeSet;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::NaiveDate;
use chrono_tz::Tz;

use crate::clock::{local_date, local_day_bounds, local_day_start, local_midnight};
use crate::config::{PriceKind, PricesConfig};
use crate::journal::Journal;
use crate::prediction::{AnchorSchedule, LocalNow, TimeOfDay};
use crate::units::{PricePoint, PriceSeries, Timestamp};
use crate::web::{DashboardStateSender, PriceSnapshot};

pub mod energyzero;
pub mod simulated;

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;

/// The journal `events.kind` every successful fetch is recorded under, as a
/// JSON array of `PricePoint`.
pub const JOURNAL_KIND: &str = "energy_price";

/// What can go wrong fetching prices. A response this build cannot decode is
/// the one most worth keeping, so the raw body rides along on a parse failure.
#[derive(Debug)]
pub enum PriceError {
    Request(String),
    Parse { body: String, error: String },
}

impl std::fmt::Display for PriceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PriceError::Request(e) => write!(f, "request failed: {e}"),
            PriceError::Parse { error, .. } => write!(f, "parse error: {error}"),
        }
    }
}

/// Wholesale prices for the half-open range `[from, until)`. A backend may
/// return points reaching outside the range; callers key them by interval in a
/// `PriceSeries`, which tolerates that.
pub trait PriceSource {
    fn prices(
        &self,
        from: Timestamp,
        until: Timestamp,
    ) -> impl Future<Output = Result<Vec<PricePoint>, PriceError>> + Send;
}

/// Whichever backend `[prices]` selected.
pub enum PriceFeed {
    EnergyZero(energyzero::EnergyZeroPrices),
    Simulated(simulated::SimulatedPrices),
}

impl PriceFeed {
    /// The only place a `[prices]` kind becomes a live backend, so callers
    /// never match on `PriceKind` themselves.
    pub fn from_config(config: &PricesConfig) -> PriceFeed {
        match config.kind {
            PriceKind::EnergyZero => PriceFeed::EnergyZero(energyzero::EnergyZeroPrices::new()),
            PriceKind::Simulated => PriceFeed::Simulated(simulated::SimulatedPrices::new()),
        }
    }
}

impl PriceSource for PriceFeed {
    async fn prices(
        &self,
        from: Timestamp,
        until: Timestamp,
    ) -> Result<Vec<PricePoint>, PriceError> {
        match self {
            PriceFeed::EnergyZero(s) => s.prices(from, until).await,
            PriceFeed::Simulated(s) => s.prices(from, until).await,
        }
    }
}

/// The local dates any recorded fetch has a price for. A row's `ts_ms` is
/// when it was fetched, not which day it prices, so coverage comes from the
/// payloads. A payload that no longer parses covers nothing.
pub fn covered_days<'a>(
    payloads: impl IntoIterator<Item = &'a str>,
    tz: Tz,
) -> BTreeSet<NaiveDate> {
    payloads
        .into_iter()
        .filter_map(|payload| serde_json::from_str::<Vec<PricePoint>>(payload).ok())
        .flatten()
        .filter_map(|point| local_date(point.from, tz))
        .collect()
}

/// The last `backfill_days` days before `today` that `covered` lacks,
/// oldest first.
pub fn missing_days(
    today: NaiveDate,
    backfill_days: u16,
    covered: &BTreeSet<NaiveDate>,
) -> Vec<NaiveDate> {
    let mut days: Vec<NaiveDate> = (1..=u64::from(backfill_days))
        .filter_map(|back| today.checked_sub_days(chrono::Days::new(back)))
        .filter(|day| !covered.contains(day))
        .collect();
    days.reverse();
    days
}

/// The series the dashboard shows: today onwards, freshest value per interval.
#[derive(Debug, Default)]
pub struct PriceCache {
    series: PriceSeries,
    as_of: Option<Timestamp>,
}

impl PriceCache {
    /// Anything over before local midnight of `at` is dropped: the journal
    /// holds history, this only covers what the dashboard displays.
    pub fn merge(&mut self, points: &[PricePoint], at: Timestamp, tz: Tz) {
        for point in points {
            self.series.insert(*point);
        }
        self.series.drop_ending_before(local_midnight(at, tz));
        self.as_of = Some(self.as_of.map_or(at, |prev| prev.max(at)));
    }

    pub fn snapshot(&self) -> PriceSnapshot {
        PriceSnapshot {
            points: self.series.clone(),
            as_of: self.as_of,
        }
    }
}

/// Local `today` and tomorrow, as `[from, until)`.
fn today_and_tomorrow(today: NaiveDate, tz: Tz) -> Option<(Timestamp, Timestamp)> {
    let day_after_tomorrow = today.checked_add_days(chrono::Days::new(2))?;
    Some((
        local_day_start(today, tz)?,
        local_day_start(day_after_tomorrow, tz)?,
    ))
}

/// One fetch, journalled on success. A failure is logged here so every
/// caller reports it the same way.
async fn fetch(
    feed: &PriceFeed,
    from: Timestamp,
    until: Timestamp,
    journal: &Journal,
    what: &str,
) -> Option<Vec<PricePoint>> {
    match feed.prices(from, until).await {
        Ok(points) => {
            match serde_json::to_string(&points) {
                Ok(json) => journal.raw(JOURNAL_KIND, &json),
                Err(e) => tracing::warn!("Cannot serialize prices for the journal: {e}"),
            }
            Some(points)
        }
        Err(e) => {
            tracing::warn!("Price fetch failed for {what}: {e}");
            if let PriceError::Parse { body, .. } = &e {
                tracing::debug!("Undecodable price body: {body}");
            }
            None
        }
    }
}

/// How a startup backfill went, for its one-line summary.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct BackfillSummary {
    pub fetched: usize,
    pub failed: usize,
}

/// Fetches each of `days` in turn, one request per day. `None` when
/// `shutdown` fired first.
async fn backfill(
    feed: &PriceFeed,
    days: &[NaiveDate],
    tz: Tz,
    journal: &Journal,
    shutdown: &mut tokio::sync::oneshot::Receiver<()>,
) -> Option<BackfillSummary> {
    let mut summary = BackfillSummary::default();
    for day in days {
        let Some((from, until)) = local_day_bounds(*day, tz) else {
            summary.failed += 1;
            continue;
        };
        let what = format!("backfill of {day}");
        tokio::select! {
            biased;
            _ = &mut *shutdown => return None,
            result = fetch(feed, from, until, journal, &what) => match result {
                Some(_) => summary.fetched += 1,
                None => summary.failed += 1,
            },
        }
    }
    Some(summary)
}

/// Every `energy_price` row on record. A journal that cannot be read means
/// nothing is known to be covered, so the backfill fetches everything.
async fn recorded_payloads(journal_path: PathBuf) -> Vec<String> {
    let now = Timestamp::from(chrono::Utc::now());
    let read = tokio::task::spawn_blocking(move || {
        crate::journal::read::read_raw_in_range(
            &journal_path,
            JOURNAL_KIND,
            Timestamp::from_millis(0),
            now,
        )
    })
    .await;
    match read {
        Ok(Ok(rows)) => rows.into_iter().map(|(_, payload)| payload).collect(),
        Ok(Err(e)) => {
            tracing::warn!("Cannot read recorded prices from the journal: {e}");
            Vec::new()
        }
        Err(e) => {
            tracing::warn!("Reading recorded prices failed: {e}");
            Vec::new()
        }
    }
}

/// How often the poller wakes to check whether an anchor is due; the anchors
/// themselves gate the fetch.
const CHECK_INTERVAL: Duration = Duration::from_secs(60);

/// Runs until `shutdown` resolves: seeds the dashboard from the journal,
/// backfills missing past days, then fetches local today and tomorrow at
/// each due anchor. The dashboard is its only live consumer, so it reaches
/// it directly rather than through the engine.
#[allow(clippy::too_many_arguments)]
pub async fn run_price_poller(
    feed: PriceFeed,
    timezone: Tz,
    poll_times: Vec<TimeOfDay>,
    backfill_days: u16,
    journal_path: PathBuf,
    dashboard_tx: DashboardStateSender,
    journal: Arc<Journal>,
    mut shutdown: tokio::sync::oneshot::Receiver<()>,
) {
    let payloads = recorded_payloads(journal_path).await;
    let now = Timestamp::from(chrono::Utc::now());
    let mut cache = PriceCache::default();
    for payload in &payloads {
        if let Ok(points) = serde_json::from_str::<Vec<PricePoint>>(payload) {
            cache.merge(&points, now, timezone);
        }
    }
    dashboard_tx.send_modify(|s| s.prices_tick(cache.snapshot()));

    let today = LocalNow::now(timezone).date;
    let covered = covered_days(payloads.iter().map(String::as_str), timezone);
    let missing = missing_days(today, backfill_days, &covered);
    let present = usize::from(backfill_days) - missing.len();
    let Some(summary) = backfill(&feed, &missing, timezone, &journal, &mut shutdown).await else {
        return;
    };
    tracing::info!(
        "Price backfill: {} days fetched, {present} already present, {} failed",
        summary.fetched,
        summary.failed,
    );

    let mut schedule = AnchorSchedule::new(poll_times);
    let mut check = tokio::time::interval(CHECK_INTERVAL);
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            _ = check.tick() => {
                let now = LocalNow::now(timezone);
                let Some(slot) = schedule.next_due(&now) else { continue };
                // Marked before fetching: a failed fetch still used the anchor.
                schedule.mark_used(&now, slot);
                let Some((from, until)) = today_and_tomorrow(now.date, timezone) else { continue };
                tracing::info!("Fetching prices ({slot} anchor)");
                let what = format!("{slot} anchor");
                if let Some(points) = fetch(&feed, from, until, &journal, &what).await {
                    cache.merge(&points, Timestamp::from(chrono::Utc::now()), timezone);
                    dashboard_tx.send_modify(|s| s.prices_tick(cache.snapshot()));
                }
            }
        }
    }
}
