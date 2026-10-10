use super::*;
use crate::config::default_price_poll_times;
use crate::units::{CentsPerKwh, RetentionDays};

const AMSTERDAM: Tz = chrono_tz::Europe::Amsterdam;

fn date(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

/// `hour` whole hours of elapsed time after `day`'s local midnight.
fn local_hour(day: NaiveDate, hour: i64) -> Timestamp {
    Timestamp::from_millis(
        local_day_start(day, AMSTERDAM).unwrap().as_millis() + hour * Elapsed::HOUR.as_millis(),
    )
}

fn point(from: Timestamp, cents: f64) -> PricePoint {
    PricePoint {
        from,
        until: from + Elapsed::HOUR,
        wholesale: CentsPerKwh(cents),
    }
}

/// Every hour of `day`, `hours` long, and nothing else.
fn whole_day(day: NaiveDate, hours: i64) -> Vec<PricePoint> {
    (0..hours)
        .map(|hour| point(local_hour(day, hour), 10.0))
        .collect()
}

fn payload(points: &[PricePoint]) -> String {
    serde_json::to_string(points).unwrap()
}

// --- Anchor schedule ---------------------------------------------------------

fn at(day: NaiveDate, hour: u32, minute: u32) -> LocalNow {
    LocalNow {
        date: day,
        time: TimeOfDay::new(hour, minute).unwrap(),
    }
}

#[test]
fn a_restart_late_in_the_day_fetches_once_not_per_missed_anchor() {
    let mut schedule = AnchorSchedule::new(default_price_poll_times());
    let evening = at(date(2026, 3, 1), 18, 0);
    let due = schedule.next_due(&evening).unwrap();
    assert_eq!(due, TimeOfDay::new(17, 0).unwrap());
    schedule.mark_used(&evening, due);
    assert_eq!(schedule.next_due(&evening), None);
}

#[test]
fn the_anchors_reset_on_a_new_local_day() {
    let mut schedule = AnchorSchedule::new(default_price_poll_times());
    let evening = at(date(2026, 3, 1), 18, 0);
    schedule.mark_used(&evening, TimeOfDay::new(17, 0).unwrap());
    assert_eq!(
        schedule.next_due(&at(date(2026, 3, 2), 0, 5)),
        Some(TimeOfDay::new(0, 5).unwrap())
    );
}

// --- Backfill coverage -------------------------------------------------------

#[test]
fn a_day_counts_as_covered_once_every_local_hour_is_priced() {
    let day = date(2026, 3, 1);
    let mut hours = whole_day(day, 24);
    let late = payload(&hours.split_off(12));
    let early = payload(&hours);
    assert_eq!(covered_days([early.as_str()], AMSTERDAM), BTreeSet::new());
    assert_eq!(
        covered_days([early.as_str(), late.as_str()], AMSTERDAM),
        BTreeSet::from([day])
    );
}

#[test]
fn a_spill_over_point_does_not_cover_the_next_day() {
    let day = date(2026, 10, 9);
    let mut points = whole_day(day, 24);
    points.push(point(local_hour(day, 24), 10.0));
    let fetched = payload(&points);
    assert_eq!(
        covered_days([fetched.as_str()], AMSTERDAM),
        BTreeSet::from([day])
    );
}

#[test]
fn dst_days_are_covered_by_their_23_or_25_hours() {
    let spring = date(2026, 3, 29);
    let autumn = date(2026, 10, 25);
    let spring_points = payload(&whole_day(spring, 23));
    let autumn_points = payload(&whole_day(autumn, 25));
    assert_eq!(
        covered_days([spring_points.as_str(), autumn_points.as_str()], AMSTERDAM),
        BTreeSet::from([spring, autumn])
    );
    let short = payload(&whole_day(autumn, 24));
    assert!(covered_days([short.as_str()], AMSTERDAM).is_empty());
}

#[test]
fn a_payload_that_no_longer_parses_covers_nothing() {
    let covered = covered_days(["not json", "{\"x\":1}"], AMSTERDAM);
    assert!(covered.is_empty());
}

#[test]
fn only_uncovered_past_days_are_missing_oldest_first_excluding_today() {
    let today = date(2026, 3, 10);
    let covered = BTreeSet::from([date(2026, 3, 8), today]);
    assert_eq!(
        missing_days(today, 4, &covered),
        vec![date(2026, 3, 6), date(2026, 3, 7), date(2026, 3, 9)]
    );
}

#[test]
fn no_backfill_days_means_nothing_is_missing() {
    assert!(missing_days(date(2026, 3, 10), 0, &BTreeSet::new()).is_empty());
}

// --- Series merge ------------------------------------------------------------

#[test]
fn a_merge_drops_points_over_before_local_midnight_and_keeps_the_fresher_value() {
    let today = date(2026, 3, 10);
    let yesterday = point(local_hour(today, -2), 5.0);
    let morning = point(local_hour(today, 8), 5.0);
    let mut cache = PriceCache::default();
    cache.merge(&[yesterday, morning], local_hour(today, -3), AMSTERDAM);

    let revised = point(local_hour(today, 8), 7.0);
    let tomorrow = point(local_hour(today, 30), 9.0);
    let now = local_hour(today, 12);
    cache.merge(&[revised, tomorrow], now, AMSTERDAM);

    let snapshot = cache.snapshot();
    assert_eq!(snapshot.as_of, Some(now));
    assert_eq!(snapshot.points.at(yesterday.from), None);
    assert_eq!(snapshot.points.at(morning.from), Some(&revised));
    assert_eq!(snapshot.points.at(tomorrow.from), Some(&tomorrow));
}

#[test]
fn a_restored_cache_is_as_fresh_as_its_newest_fetch_not_the_restart() {
    let today = date(2026, 3, 10);
    let fetched_early = local_hour(today, 0);
    let fetched_late = local_hour(today, 13);
    let rows = [
        (fetched_late, payload(&[point(local_hour(today, 30), 9.0)])),
        (
            fetched_early,
            payload(&[
                point(local_hour(today, -2), 5.0),
                point(local_hour(today, 8), 5.0),
            ]),
        ),
    ];
    let cache = PriceCache::restore(&rows, local_hour(today, 20), AMSTERDAM);

    let snapshot = cache.snapshot();
    assert_eq!(snapshot.as_of, Some(fetched_late));
    assert_eq!(snapshot.points.at(local_hour(today, -2)), None);
    assert!(snapshot.points.at(local_hour(today, 8)).is_some());
    assert!(snapshot.points.at(local_hour(today, 30)).is_some());
}

#[test]
fn nothing_on_record_restores_an_empty_cache_never_fetched() {
    let cache = PriceCache::restore(&[], local_hour(date(2026, 3, 10), 12), AMSTERDAM);
    assert_eq!(cache.snapshot(), PriceSnapshot::default());
}

// --- Backfill ----------------------------------------------------------------

fn journal(dir: &tempfile::TempDir) -> (Journal, Option<crate::journal::Writer>) {
    Journal::open(
        &dir.path().join("journal.db"),
        RetentionDays::new(30).unwrap(),
        "0.0.0-test",
        &serde_json::json!({}),
    )
}

#[tokio::test]
async fn a_backfill_journals_one_fetch_per_missing_day() {
    let dir = tempfile::tempdir().unwrap();
    let (journal, writer) = journal(&dir);
    let feed = PriceFeed::Simulated(simulated::SimulatedPrices::new());
    let days = [date(2026, 3, 1), date(2026, 3, 3)];
    let (_stop, mut shutdown) = tokio::sync::oneshot::channel();

    let summary = backfill(&feed, &days, AMSTERDAM, &journal, &mut shutdown)
        .await
        .unwrap();
    assert_eq!(
        summary,
        BackfillSummary {
            fetched: 2,
            failed: 0
        }
    );

    drop(journal);
    writer.unwrap().await.unwrap();
    let rows = recorded_payloads(dir.path().join("journal.db")).await;
    assert_eq!(rows.len(), 2);
    let covered = covered_days(rows.iter().map(|(_, payload)| payload.as_str()), AMSTERDAM);
    assert_eq!(covered, BTreeSet::from(days));
}

#[tokio::test]
async fn a_backfill_stops_at_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let (journal, _writer) = journal(&dir);
    let feed = PriceFeed::Simulated(simulated::SimulatedPrices::new());
    let (stop, mut shutdown) = tokio::sync::oneshot::channel();
    stop.send(()).unwrap();

    let days = [date(2026, 3, 1)];
    let outcome = backfill(&feed, &days, AMSTERDAM, &journal, &mut shutdown).await;
    assert_eq!(outcome, None);
}

#[tokio::test]
async fn an_empty_fetch_is_not_journalled() {
    let dir = tempfile::tempdir().unwrap();
    let (journal, writer) = journal(&dir);
    let feed = PriceFeed::Simulated(simulated::SimulatedPrices::new());
    let at = local_hour(date(2026, 3, 1), 0);

    assert_eq!(fetch(&feed, at, at, &journal, "an empty range").await, None);

    drop(journal);
    writer.unwrap().await.unwrap();
    assert!(
        recorded_payloads(dir.path().join("journal.db"))
            .await
            .is_empty()
    );
}
