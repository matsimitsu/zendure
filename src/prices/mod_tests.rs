use super::*;
use crate::journal::testing;
use crate::units::CentsPerKwh;

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

fn series(points: &[PricePoint]) -> PriceSeries {
    let mut series = PriceSeries::default();
    points.iter().for_each(|p| series.insert(*p));
    series
}

/// A row as the journal hands it back, fetched at the epoch.
fn row(points: &[PricePoint]) -> (Timestamp, String) {
    (Timestamp::from_millis(0), payload(points))
}

// --- Decoding ----------------------------------------------------------------

/// A later fetch of the same interval is a revision and wins; a row that does
/// not decode is counted, not fatal.
#[test]
fn rows_fold_newest_wins_and_count_what_does_not_decode() {
    let at = local_hour(date(2026, 3, 1), 0);
    let rows = vec![
        row(&[point(at, 10.0)]),
        (Timestamp::from_millis(0), "not json".to_string()),
        (Timestamp::from_millis(0), "{\"x\":1}".to_string()),
        row(&[point(at, 12.0)]),
    ];

    let (series, undecodable) = decode_rows(&rows);
    assert_eq!(undecodable, 2);
    assert_eq!(series.at(at).map(|p| p.wholesale), Some(CentsPerKwh(12.0)));
}

// --- Backfill coverage -------------------------------------------------------

#[test]
fn a_day_counts_as_covered_once_every_local_hour_is_priced() {
    let day = date(2026, 3, 1);
    let mut hours = whole_day(day, 24);
    let late = hours.split_off(12);
    assert_eq!(covered_days(&series(&hours), AMSTERDAM), BTreeSet::new());
    hours.extend(late);
    assert_eq!(
        covered_days(&series(&hours), AMSTERDAM),
        BTreeSet::from([day])
    );
}

#[test]
fn a_spill_over_point_does_not_cover_the_next_day() {
    let day = date(2026, 10, 9);
    let mut points = whole_day(day, 24);
    points.push(point(local_hour(day, 24), 10.0));
    assert_eq!(
        covered_days(&series(&points), AMSTERDAM),
        BTreeSet::from([day])
    );
}

#[test]
fn dst_days_are_covered_by_their_23_or_25_hours() {
    let spring = date(2026, 3, 29);
    let autumn = date(2026, 10, 25);
    let mut points = whole_day(spring, 23);
    points.extend(whole_day(autumn, 25));
    assert_eq!(
        covered_days(&series(&points), AMSTERDAM),
        BTreeSet::from([spring, autumn])
    );
    let short = series(&whole_day(autumn, 24));
    assert!(covered_days(&short, AMSTERDAM).is_empty());
}

#[test]
fn only_uncovered_past_days_are_missing_oldest_first_excluding_today() {
    let today = date(2026, 3, 10);
    let covered = BTreeSet::from([date(2026, 3, 8), today]);
    assert_eq!(
        missing_days(today, BackfillDays::new(4), &covered),
        vec![date(2026, 3, 6), date(2026, 3, 7), date(2026, 3, 9)]
    );
}

#[test]
fn no_backfill_days_means_nothing_is_missing() {
    assert!(missing_days(date(2026, 3, 10), BackfillDays::new(0), &BTreeSet::new()).is_empty());
}

// --- Series merge ------------------------------------------------------------

#[test]
fn a_merge_drops_points_older_than_the_history_window_and_keeps_the_fresher_value() {
    let today = date(2026, 3, 10);
    let yesterday = point(local_hour(today, -7 * 24 - 2), 5.0);
    let morning = point(local_hour(today, 8), 5.0);
    let mut snapshot = PriceSnapshot::default();
    snapshot.merge(&[yesterday, morning], local_hour(today, -3), AMSTERDAM);

    let revised = point(local_hour(today, 8), 7.0);
    let tomorrow = point(local_hour(today, 30), 9.0);
    let now = local_hour(today, 12);
    snapshot.merge(&[revised, tomorrow], now, AMSTERDAM);

    assert_eq!(snapshot.as_of, Some(now));
    assert_eq!(snapshot.points.at(yesterday.from), None);
    assert_eq!(snapshot.points.at(morning.from), Some(&revised));
    assert_eq!(snapshot.points.at(tomorrow.from), Some(&tomorrow));
}

#[test]
fn a_restored_snapshot_drops_points_older_than_the_history_window() {
    let today = date(2026, 3, 10);
    let fetched = local_hour(today, 13);
    let recorded = series(&[
        point(local_hour(today, -7 * 24 - 2), 5.0),
        point(local_hour(today, 8), 5.0),
        point(local_hour(today, 30), 9.0),
    ]);
    let snapshot =
        PriceSnapshot::restore(recorded, Some(fetched), local_hour(today, 20), AMSTERDAM);

    assert_eq!(snapshot.as_of, Some(fetched));
    assert_eq!(snapshot.points.at(local_hour(today, -7 * 24 - 2)), None);
    assert!(snapshot.points.at(local_hour(today, 8)).is_some());
    assert!(snapshot.points.at(local_hour(today, 30)).is_some());
}

#[test]
fn the_newest_fetch_is_the_latest_row_whatever_the_order() {
    let early = local_hour(date(2026, 3, 10), 0);
    let late = local_hour(date(2026, 3, 10), 13);
    let rows = [(late, payload(&[])), (early, payload(&[]))];
    assert_eq!(newest_fetch(&rows), Some(late));
    assert_eq!(newest_fetch(&[]), None);
}

#[test]
fn nothing_on_record_restores_an_empty_snapshot_never_fetched() {
    let snapshot = PriceSnapshot::restore(
        PriceSeries::default(),
        None,
        local_hour(date(2026, 3, 10), 12),
        AMSTERDAM,
    );
    assert_eq!(snapshot, PriceSnapshot::default());
}

// --- Backfill ----------------------------------------------------------------

#[tokio::test]
async fn a_backfill_journals_one_fetch_per_missing_day() {
    let dir = tempfile::tempdir().unwrap();
    let (journal, writer, path) = testing::open(&dir, &serde_json::json!({}));
    let feed = PriceFeed::Simulated(simulated::SimulatedPrices::new());
    let days = [date(2026, 3, 1), date(2026, 3, 3)];
    let (_stop, mut shutdown) = tokio::sync::oneshot::channel();

    let mut handed_over = Vec::new();
    let summary = backfill(
        &feed,
        &days,
        AMSTERDAM,
        &journal,
        &mut |points| handed_over.push(points.len()),
        &mut shutdown,
    )
    .await
    .unwrap();
    assert_eq!(handed_over.len(), 2);
    assert_eq!(
        summary,
        BackfillSummary {
            fetched: 2,
            failed: 0
        }
    );

    testing::close(journal, writer).await;
    let rows = recorded_payloads(path).await;
    assert_eq!(rows.len(), 2);
    let (recorded, _) = decode_rows(&rows);
    assert_eq!(covered_days(&recorded, AMSTERDAM), BTreeSet::from(days));
}

#[tokio::test]
async fn a_backfill_stops_at_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let (journal, _writer, _path) = testing::open(&dir, &serde_json::json!({}));
    let feed = PriceFeed::Simulated(simulated::SimulatedPrices::new());
    let (stop, mut shutdown) = tokio::sync::oneshot::channel();
    stop.send(()).unwrap();

    let days = [date(2026, 3, 1)];
    let outcome = backfill(
        &feed,
        &days,
        AMSTERDAM,
        &journal,
        &mut |_| {},
        &mut shutdown,
    )
    .await;
    assert_eq!(outcome, None);
}

#[tokio::test]
async fn an_empty_fetch_is_not_journalled() {
    let dir = tempfile::tempdir().unwrap();
    let (journal, writer, path) = testing::open(&dir, &serde_json::json!({}));
    let feed = PriceFeed::Simulated(simulated::SimulatedPrices::new());
    let at = local_hour(date(2026, 3, 1), 0);

    assert_eq!(fetch(&feed, at, at, &journal, "an empty range").await, None);

    testing::close(journal, writer).await;
    assert!(recorded_payloads(path).await.is_empty());
}

// --- History window and per-day view -------------------------------------------

#[test]
fn history_starts_at_local_midnight_six_days_back_and_keeps_that_whole_day() {
    let today = date(2026, 3, 10);
    let oldest = date(2026, 3, 4);
    let now = local_hour(today, 12);
    let before = point(local_hour(oldest, -1), 1.0);
    let first = point(local_hour(oldest, 0), 2.0);
    let snapshot = PriceSnapshot::restore(series(&[before, first]), None, now, AMSTERDAM);

    assert_eq!(snapshot.points.at(before.from), None);
    assert_eq!(snapshot.points.at(first.from), Some(&first));
}

#[test]
fn history_across_a_dst_change_still_starts_at_a_local_midnight() {
    let today = date(2026, 4, 2);
    let oldest = date(2026, 3, 27);
    let now = local_hour(today, 12);
    let kept = point(local_day_start(oldest, AMSTERDAM).unwrap(), 2.0);
    let dropped = point(kept.from - Elapsed::HOUR, 1.0);
    let snapshot = PriceSnapshot::restore(series(&[dropped, kept]), None, now, AMSTERDAM);

    assert_eq!(snapshot.points.at(dropped.from), None);
    assert!(snapshot.points.at(kept.from).is_some());
}

#[test]
fn merging_keeps_backfilled_days_inside_the_window() {
    let today = date(2026, 3, 10);
    let now = local_hour(today, 12);
    let mut snapshot = PriceSnapshot::default();
    snapshot.merge(&whole_day(date(2026, 3, 5), 24), now, AMSTERDAM);
    snapshot.merge(&whole_day(date(2026, 3, 1), 24), now, AMSTERDAM);

    assert!(snapshot.prices_for(date(2026, 3, 5), AMSTERDAM).is_some());
    assert!(snapshot.prices_for(date(2026, 3, 1), AMSTERDAM).is_none());
}

#[test]
fn a_backfilled_day_keeps_as_of_and_reports_whether_it_was_kept() {
    let today = date(2026, 3, 10);
    let now = local_hour(today, 12);
    let fetched = local_hour(today, 0);
    let mut snapshot = PriceSnapshot::default();
    snapshot.merge(&whole_day(today, 24), fetched, AMSTERDAM);

    assert!(snapshot.backfill(&whole_day(date(2026, 3, 5), 24), now, AMSTERDAM));
    assert!(!snapshot.backfill(&whole_day(date(2026, 1, 20), 24), now, AMSTERDAM));
    assert!(snapshot.prices_for(date(2026, 3, 5), AMSTERDAM).is_some());
    assert!(snapshot.prices_for(date(2026, 1, 20), AMSTERDAM).is_none());
    assert_eq!(snapshot.as_of, Some(fetched));
}

#[test]
fn a_day_has_one_slot_per_local_hour_whatever_dst_does() {
    let mut snapshot = PriceSnapshot::default();
    for (day, hours) in [
        (date(2026, 3, 28), 24),
        (date(2026, 3, 29), 23),
        (date(2026, 10, 25), 25),
    ] {
        snapshot.points = series(&whole_day(day, hours));
        let prices = snapshot.prices_for(day, AMSTERDAM).unwrap();
        assert_eq!(prices.hours().len(), hours as usize, "{day}");
        assert!(
            prices.hours().iter().all(|hour| hour.wholesale.is_some()),
            "{day}"
        );
        assert_eq!(
            (prices.end - prices.start).as_millis(),
            hours * Elapsed::HOUR.as_millis(),
            "{day}"
        );
    }
}

#[test]
fn an_unpriced_hour_is_a_none_slot_and_an_unpriced_day_is_none() {
    let day = date(2026, 3, 10);
    let mut snapshot = PriceSnapshot::default();
    assert_eq!(snapshot.prices_for(day, AMSTERDAM), None);

    snapshot.points = series(&[point(local_hour(day, 5), 8.0)]);
    let prices = snapshot.prices_for(day, AMSTERDAM).unwrap();
    assert_eq!(prices.hours()[5].wholesale, Some(CentsPerKwh(8.0)));
    assert_eq!(
        prices
            .hours()
            .iter()
            .filter(|hour| hour.wholesale.is_some())
            .count(),
        1
    );
}

#[test]
fn tomorrow_is_published_only_when_fully_priced() {
    let today = date(2026, 3, 10);
    let tomorrow = today.succ_opt().unwrap();
    let mut snapshot = PriceSnapshot {
        points: series(&whole_day(tomorrow, 23)),
        ..Default::default()
    };
    assert!(!snapshot.tomorrow_published(today, AMSTERDAM));

    snapshot.points = series(&whole_day(tomorrow, 24));
    assert!(snapshot.tomorrow_published(today, AMSTERDAM));
}
