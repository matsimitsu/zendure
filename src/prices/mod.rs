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
use crate::fetch::FetchError;
use crate::journal::Journal;
use crate::schedule::{AnchorSchedule, LocalNow};
use crate::units::{BackfillDays, CentsPerKwh, Elapsed, Timestamp};
use crate::web::DashboardStateSender;

pub mod energyzero;
pub mod series;
pub mod simulated;
pub mod tiers;

pub use series::{PricePoint, PriceSeries};

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;

/// The journal `events.kind` every successful fetch is recorded under, as a
/// JSON array of `PricePoint`.
pub const JOURNAL_KIND: &str = "energy_price";

/// Journalled fetches as `(fetched at, payload)`, folded into one series and
/// the count of rows that did not decode. Rows arrive in the order they were
/// written, so a later fetch of the same interval — a revised day-ahead price
/// — replaces the earlier one. A row that does not decode costs only its own
/// hours.
pub fn decode_rows(rows: &[(Timestamp, String)]) -> (PriceSeries, usize) {
    let mut series = PriceSeries::default();
    let mut undecodable = 0;
    for (_, payload) in rows {
        match serde_json::from_str::<Vec<PricePoint>>(payload) {
            Ok(points) => points.into_iter().for_each(|p| series.insert(p)),
            Err(_) => undecodable += 1,
        }
    }
    (series, undecodable)
}

/// Wholesale prices for the half-open range `[from, until)`. A backend may
/// return points reaching outside the range; callers key them by interval in a
/// `PriceSeries`, which tolerates that.
pub trait PriceSource {
    fn prices(
        &self,
        from: Timestamp,
        until: Timestamp,
    ) -> impl Future<Output = Result<Vec<PricePoint>, FetchError>> + Send;
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
    ) -> Result<Vec<PricePoint>, FetchError> {
        match self {
            PriceFeed::EnergyZero(s) => s.prices(from, until).await,
            PriceFeed::Simulated(s) => s.prices(from, until).await,
        }
    }
}

/// The local dates `series` prices every hour of. Coverage comes from the
/// points, never from when a row was fetched; a day priced only in part is
/// not covered, so a stray point across midnight never stops that day being
/// backfilled.
pub fn covered_days(series: &PriceSeries, tz: Tz) -> BTreeSet<NaiveDate> {
    let days: BTreeSet<NaiveDate> = series
        .iter()
        .filter_map(|point| local_date(point.from, tz))
        .collect();
    days.into_iter()
        .filter(|day| fully_priced(series, *day, tz))
        .collect()
}

/// The start of each local hour in `[start, end)`.
fn hour_starts(start: Timestamp, end: Timestamp) -> impl Iterator<Item = Timestamp> {
    std::iter::successors(Some(start), |at| Some(*at + Elapsed::HOUR))
        .take_while(move |at| *at < end)
}

fn fully_priced(series: &PriceSeries, day: NaiveDate, tz: Tz) -> bool {
    let Some((start, end)) = local_day_bounds(day, tz) else {
        return false;
    };
    hour_starts(start, end).all(|at| series.at(at).is_some())
}

/// How many days before local today the dashboard keeps prices for; the day
/// navigation reaches exactly this far back.
pub const PRICE_HISTORY_DAYS: BackfillDays = BackfillDays::new(6);

/// Local start of the oldest day the dashboard keeps prices for.
fn history_start(now: Timestamp, tz: Tz) -> Timestamp {
    let fallback = local_midnight(now, tz);
    local_date(now, tz)
        .and_then(|today| {
            today.checked_sub_days(chrono::Days::new(u64::from(PRICE_HISTORY_DAYS.count())))
        })
        .and_then(|oldest| local_day_start(oldest, tz))
        .unwrap_or(fallback)
}

/// One local day as a renderer needs it: the day's bounds and one slot per
/// local hour, so 23, 24 or 25 of them across DST changes.
#[derive(Debug, Clone, PartialEq)]
pub struct DayPrices {
    pub date: NaiveDate,
    pub start: Timestamp,
    pub end: Timestamp,
    slots: Vec<Option<CentsPerKwh>>,
}

impl DayPrices {
    /// The wholesale price per local-hour slot, `None` for an hour the feed
    /// has not priced. This is the shape `tiers` takes.
    // Consumed by the price panel (E4) and the day navigation (E5).
    #[allow(dead_code)]
    pub fn slots(&self) -> &[Option<CentsPerKwh>] {
        &self.slots
    }
}

/// The last `backfill_days` days before `today` that `covered` lacks,
/// oldest first.
pub fn missing_days(
    today: NaiveDate,
    backfill_days: BackfillDays,
    covered: &BTreeSet<NaiveDate>,
) -> Vec<NaiveDate> {
    let mut days: Vec<NaiveDate> = (1..=u64::from(backfill_days.count()))
        .filter_map(|back| today.checked_sub_days(chrono::Days::new(back)))
        .filter(|day| !covered.contains(day))
        .collect();
    days.reverse();
    days
}

/// The latest prices the poller fetched, keyed by interval: the last
/// `PRICE_HISTORY_DAYS` days onwards, freshest value per interval. Empty and `as_of: None` until the first fetch
/// lands.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PriceSnapshot {
    pub points: PriceSeries,
    pub as_of: Option<Timestamp>,
}

impl PriceSnapshot {
    /// Rebuilt from what the journal holds: pruned against `now`, but only as
    /// fresh as `as_of`, the newest fetch.
    pub fn restore(
        mut points: PriceSeries,
        as_of: Option<Timestamp>,
        now: Timestamp,
        tz: Tz,
    ) -> PriceSnapshot {
        points.drop_ending_before(history_start(now, tz));
        PriceSnapshot { points, as_of }
    }

    /// Anything over before the start of the oldest kept day is dropped: the
    /// journal holds all history, this only covers what the dashboard shows.
    pub fn merge(&mut self, points: &[PricePoint], at: Timestamp, tz: Tz) {
        for point in points {
            self.points.insert(*point);
        }
        self.points.drop_ending_before(history_start(at, tz));
        self.as_of = Some(self.as_of.map_or(at, |prev| prev.max(at)));
    }

    /// `date`'s local hours, `None` when none of them is priced.
    // Consumed by the price panel (E4) and the day navigation (E5).
    #[allow(dead_code)]
    pub fn prices_for(&self, date: NaiveDate, tz: Tz) -> Option<DayPrices> {
        let (start, end) = local_day_bounds(date, tz)?;
        let slots: Vec<Option<CentsPerKwh>> = hour_starts(start, end)
            .map(|at| self.points.at(at).map(|p| p.wholesale))
            .collect();
        slots.iter().any(Option::is_some).then_some(DayPrices {
            date,
            start,
            end,
            slots,
        })
    }

    /// Whether every hour of the day after `today` is priced.
    // Consumed by the price panel (E4).
    #[allow(dead_code)]
    pub fn tomorrow_published(&self, today: NaiveDate, tz: Tz) -> bool {
        today
            .succ_opt()
            .is_some_and(|tomorrow| fully_priced(&self.points, tomorrow, tz))
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
            if let FetchError::Parse { body, .. } = &e {
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

/// Fetches each of `days` in turn, one request per day, handing every
/// fetched day to `on_fetched`. `None` when `shutdown` fired first.
async fn backfill(
    feed: &PriceFeed,
    days: &[NaiveDate],
    tz: Tz,
    journal: &Journal,
    on_fetched: &mut impl FnMut(&[PricePoint]),
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
                Some(points) => {
                    on_fetched(&points);
                    summary.fetched += 1;
                }
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

/// When the freshest of `rows` was fetched: a restored snapshot is only as
/// fresh as that, not the restart.
fn newest_fetch(rows: &[(Timestamp, String)]) -> Option<Timestamp> {
    rows.iter().map(|(at, _)| *at).max()
}

/// How often the poller wakes to check whether an anchor is due; the anchors
/// themselves gate the fetch.
const CHECK_INTERVAL: Duration = Duration::from_secs(60);

/// Runs until `shutdown` resolves: seeds the dashboard from the journal,
/// fetches today and tomorrow if an anchor is due, backfills missing past
/// days, then keeps fetching at each due anchor. The dashboard is its only
/// live consumer, so it reaches it directly rather than through the engine.
pub fn run_price_poller(
    config: &PricesConfig,
    timezone: Tz,
    journal_path: PathBuf,
    dashboard_tx: DashboardStateSender,
    journal: Arc<Journal>,
    shutdown: tokio::sync::oneshot::Receiver<()>,
) -> impl Future<Output = ()> + Send + 'static {
    let poller = Poller {
        feed: PriceFeed::from_config(config),
        timezone,
        schedule: AnchorSchedule::new(config.poll_times.clone()),
        snapshot: PriceSnapshot::default(),
        dashboard_tx,
        journal,
    };
    poller.run(config.backfill_days, journal_path, shutdown)
}

/// What the poller carries between anchors.
struct Poller {
    feed: PriceFeed,
    timezone: Tz,
    schedule: AnchorSchedule,
    snapshot: PriceSnapshot,
    dashboard_tx: DashboardStateSender,
    journal: Arc<Journal>,
}

impl Poller {
    fn publish(&self) {
        let snapshot = self.snapshot.clone();
        self.dashboard_tx.send_modify(|s| s.prices_tick(snapshot));
    }

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
            self.snapshot
                .merge(&points, Timestamp::from(chrono::Utc::now()), self.timezone);
            self.publish();
        }
        Some(())
    }

    async fn run(
        mut self,
        backfill_days: BackfillDays,
        journal_path: PathBuf,
        mut shutdown: tokio::sync::oneshot::Receiver<()>,
    ) {
        let rows = tokio::select! {
            biased;
            _ = &mut shutdown => return,
            rows = recorded_payloads(journal_path) => rows,
        };
        let (series, _) = decode_rows(&rows);
        let covered = covered_days(&series, self.timezone);
        self.snapshot = PriceSnapshot::restore(
            series,
            newest_fetch(&rows),
            Timestamp::from(chrono::Utc::now()),
            self.timezone,
        );
        self.publish();

        // Before the backfill, so a first install shows today without waiting
        // on every past day.
        if self.poll_due_anchor(&mut shutdown).await.is_none() {
            return;
        }

        let today = LocalNow::now(self.timezone).date;
        let missing = missing_days(today, backfill_days, &covered);
        let present = usize::from(backfill_days.count()) - missing.len();
        let timezone = self.timezone;
        let snapshot = &mut self.snapshot;
        let dashboard_tx = &self.dashboard_tx;
        let mut merge_and_publish = |points: &[PricePoint]| {
            snapshot.merge(points, Timestamp::from(chrono::Utc::now()), timezone);
            let current = snapshot.clone();
            dashboard_tx.send_modify(|s| s.prices_tick(current));
        };
        let Some(summary) = backfill(
            &self.feed,
            &missing,
            timezone,
            &self.journal,
            &mut merge_and_publish,
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
                    if self.poll_due_anchor(&mut shutdown).await.is_none() {
                        break;
                    }
                }
            }
        }
    }
}
