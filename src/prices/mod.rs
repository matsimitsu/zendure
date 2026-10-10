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
use crate::units::{Elapsed, PricePoint, PriceSeries, Timestamp};
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

/// The local dates the recorded fetches price every hour of. A row's `ts_ms`
/// is when it was fetched, not which day it prices, so coverage comes from
/// the payloads; a day priced only in part is not covered, so a stray point
/// across midnight never stops that day being backfilled. A payload that no
/// longer parses covers nothing.
pub fn covered_days<'a>(
    payloads: impl IntoIterator<Item = &'a str>,
    tz: Tz,
) -> BTreeSet<NaiveDate> {
    let mut series = PriceSeries::default();
    for point in payloads
        .into_iter()
        .filter_map(|payload| serde_json::from_str::<Vec<PricePoint>>(payload).ok())
        .flatten()
    {
        series.insert(point);
    }
    let days: BTreeSet<NaiveDate> = series
        .iter()
        .filter_map(|point| local_date(point.from, tz))
        .collect();
    days.into_iter()
        .filter(|day| fully_priced(&series, *day, tz))
        .collect()
}

fn fully_priced(series: &PriceSeries, day: NaiveDate, tz: Tz) -> bool {
    let Some((start, end)) = local_day_bounds(day, tz) else {
        return false;
    };
    std::iter::successors(Some(start), |at| Some(*at + Elapsed::HOUR))
        .take_while(|at| *at < end)
        .all(|at| series.at(at).is_some())
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
    /// Rebuilt from journalled fetches as `(fetched at, payload)`: pruned
    /// against `now`, but only as fresh as the newest fetch.
    pub fn restore(rows: &[(Timestamp, String)], now: Timestamp, tz: Tz) -> PriceCache {
        let mut series = PriceSeries::default();
        for (_, payload) in rows {
            for point in serde_json::from_str::<Vec<PricePoint>>(payload).unwrap_or_default() {
                series.insert(point);
            }
        }
        series.drop_ending_before(local_midnight(now, tz));
        PriceCache {
            series,
            as_of: rows.iter().map(|(at, _)| *at).max(),
        }
    }

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

/// One fetch, journalled when it priced anything. A failure is logged here
/// so every caller reports it the same way; an empty answer counts as one,
/// since it holds nothing worth recording or showing.
async fn fetch(
    feed: &PriceFeed,
    from: Timestamp,
    until: Timestamp,
    journal: &Journal,
    what: &str,
) -> Option<Vec<PricePoint>> {
    match feed.prices(from, until).await {
        Ok(points) if points.is_empty() => {
            tracing::warn!("Price feed returned no prices for {what}");
            None
        }
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

/// Every `energy_price` row on record, as when it was fetched and its
/// payload. A journal that cannot be read means nothing is known to be
/// covered, so the backfill fetches everything.
async fn recorded_payloads(journal_path: PathBuf) -> Vec<(Timestamp, String)> {
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
        Ok(Ok(rows)) => rows,
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

/// What the poller carries between anchors.
struct Poller {
    feed: PriceFeed,
    timezone: Tz,
    schedule: AnchorSchedule,
    cache: PriceCache,
    dashboard_tx: DashboardStateSender,
    journal: Arc<Journal>,
}

impl Poller {
    /// Fetches local today and tomorrow if an anchor is due. `None` when
    /// `shutdown` fired first.
    async fn poll_due_anchor(
        &mut self,
        shutdown: &mut tokio::sync::oneshot::Receiver<()>,
    ) -> Option<()> {
        let now = LocalNow::now(self.timezone);
        let Some(slot) = self.schedule.next_due(&now) else {
            return Some(());
        };
        // Marked before fetching: a failed fetch still used the anchor.
        self.schedule.mark_used(&now, slot);
        let Some((from, until)) = today_and_tomorrow(now.date, self.timezone) else {
            return Some(());
        };
        tracing::info!("Fetching prices ({slot} anchor)");
        let what = format!("{slot} anchor");
        let fetched = tokio::select! {
            biased;
            _ = &mut *shutdown => return None,
            fetched = fetch(&self.feed, from, until, &self.journal, &what) => fetched,
        };
        if let Some(points) = fetched {
            self.cache
                .merge(&points, Timestamp::from(chrono::Utc::now()), self.timezone);
            let snapshot = self.cache.snapshot();
            self.dashboard_tx.send_modify(|s| s.prices_tick(snapshot));
        }
        Some(())
    }
}

/// Runs until `shutdown` resolves: seeds the dashboard from the journal,
/// fetches today and tomorrow if an anchor is due, backfills missing past
/// days, then keeps fetching at each due anchor. The dashboard is its only
/// live consumer, so it reaches it directly rather than through the engine.
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
    let rows = tokio::select! {
        biased;
        _ = &mut shutdown => return,
        rows = recorded_payloads(journal_path) => rows,
    };
    let cache = PriceCache::restore(&rows, Timestamp::from(chrono::Utc::now()), timezone);
    let snapshot = cache.snapshot();
    dashboard_tx.send_modify(|s| s.prices_tick(snapshot));

    let mut poller = Poller {
        feed,
        timezone,
        schedule: AnchorSchedule::new(poll_times),
        cache,
        dashboard_tx,
        journal,
    };
    // Before the backfill, so a first install shows today without waiting
    // on every past day.
    if poller.poll_due_anchor(&mut shutdown).await.is_none() {
        return;
    }

    let today = LocalNow::now(timezone).date;
    let covered = covered_days(rows.iter().map(|(_, payload)| payload.as_str()), timezone);
    let missing = missing_days(today, backfill_days, &covered);
    let present = usize::from(backfill_days) - missing.len();
    let Some(summary) = backfill(
        &poller.feed,
        &missing,
        timezone,
        &poller.journal,
        &mut shutdown,
    )
    .await
    else {
        return;
    };
    tracing::info!(
        "Price backfill: {} days fetched, {present} already present, {} failed",
        summary.fetched,
        summary.failed,
    );

    let mut check = tokio::time::interval(CHECK_INTERVAL);
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            _ = check.tick() => {
                if poller.poll_due_anchor(&mut shutdown).await.is_none() {
                    break;
                }
            }
        }
    }
}
