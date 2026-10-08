//! The flows panel's query boundary, the past-day fold through the journal,
//! and the cache those folds are kept in.

use super::*;

use crate::event::Event;
use crate::fixtures::journey::{self, battery_event, meter_event};
use crate::fixtures::utc;
use crate::units::{BatteryPower, Soc};
use crate::web::templates::energy_flows;

fn tz() -> Tz {
    chrono_tz::Europe::Amsterdam
}

fn date(day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 10, day).unwrap()
}

fn configured() -> [DeviceId; 1] {
    [DeviceId::new(journey::BATTERY_ID)]
}

fn request(day: u32, interval: FlowResolution) -> FlowsRequest {
    FlowsRequest {
        day: date(day),
        interval,
    }
}

#[test]
fn no_query_is_today_by_the_hour() {
    let query = FlowsQuery::parse(None).unwrap();

    assert_eq!(query.resolve(date(8)), request(8, FlowResolution::Hour));
}

#[test]
fn a_query_names_its_day_and_interval() {
    let query = FlowsQuery::parse(Some("day=2026-10-05&interval=15m")).unwrap();

    assert_eq!(query.resolve(date(8)), request(5, FlowResolution::Quarter));
}

#[test]
fn a_future_day_clamps_to_today() {
    let query = FlowsQuery::parse(Some("day=2099-01-01")).unwrap();

    assert_eq!(query.resolve(date(8)), request(8, FlowResolution::Hour));
}

#[test]
fn an_unparsable_day_or_interval_is_refused() {
    for raw in [
        "day=yesterday",
        "day=2026-13-01",
        "day=2026-10-08T00:00",
        "interval=1d",
    ] {
        assert!(FlowsQuery::parse(Some(raw)).is_err(), "{raw}");
    }
}

#[test]
fn empty_values_and_other_keys_are_ignored() {
    let query = FlowsQuery::parse(Some("day=&interval=&utm=x&flag")).unwrap();

    assert_eq!(query.resolve(date(8)), request(8, FlowResolution::Hour));
}

fn context(today: u32) -> RenderedFor {
    RenderedFor {
        today: date(today),
        earliest: None,
    }
}

#[test]
fn the_cache_keeps_at_most_its_capacity_and_drops_the_least_recent() {
    let mut cache = DayCache::new(3);
    for day in 1..=3 {
        cache.insert(context(9), date(day), day);
    }
    // Reading day 1 makes day 2 the least recently used.
    assert_eq!(cache.get(context(9), date(1)), Some(1));

    cache.insert(context(9), date(4), 4);

    assert_eq!(cache.len(), 3);
    assert_eq!(cache.get(context(9), date(2)), None);
    assert_eq!(cache.get(context(9), date(1)), Some(1));
    assert_eq!(cache.get(context(9), date(4)), Some(4));
}

/// "Yesterday" stops being yesterday at midnight.
#[test]
fn a_new_today_empties_the_cache() {
    let mut cache = DayCache::new(3);
    cache.insert(context(9), date(8), 8);

    assert_eq!(cache.get(context(10), date(8)), None);
    assert_eq!(cache.len(), 0);
}

/// Two days of readings around midnight, local time.
fn two_days() -> Vec<Event> {
    vec![
        // 6 October, 23:50 CEST: the day before.
        meter_event(utc(6, 21, 50), 900.0, 0.0),
        // 7 October.
        meter_event(utc(6, 22, 5), 300.0, 0.0),
        battery_event(utc(7, 8, 0), BatteryPower(-250), Soc::new(40)),
        meter_event(utc(7, 8, 1), -200.0, 1200.0),
        meter_event(utc(7, 21, 55), 150.0, 0.0),
        // 8 October, 00:05 CEST: the day after.
        meter_event(utc(7, 22, 5), 2000.0, 0.0),
    ]
}

/// A past day is exactly that day's journal events folded through the same
/// `record` the live loop uses: nothing from either side of its midnights.
#[tokio::test]
async fn a_past_day_is_the_fold_of_that_days_journal_events() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    let events = two_days();
    crate::journal::testing::record(&path, &events).await;
    let now = utc(8, 10, 0);

    let mut folded = IntervalHistory::new(configured());
    events[1..events.len() - 1]
        .iter()
        .for_each(|event| folded.record(event));
    let past = PastDays::new(path, configured());

    for interval in [FlowResolution::Hour, FlowResolution::Quarter] {
        let request = request(7, interval);
        let expected = requested_flows_view(&folded, request, Some(date(6)), now, tz());
        let actual = past.view(request, now, tz());

        assert!(!actual.plots[1].bars.is_empty());
        assert_eq!(
            energy_flows::render(&actual).into_string(),
            energy_flows::render(&expected).into_string(),
        );
    }
}

#[tokio::test]
async fn the_oldest_journal_day_has_no_step_back() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    crate::journal::testing::record(&path, &two_days()).await;
    let past = PastDays::new(path, configured());
    let now = utc(8, 10, 0);

    let oldest = past.view(request(6, FlowResolution::Hour), now, tz());
    let next = past.view(request(7, FlowResolution::Hour), now, tz());

    assert_eq!(oldest.nav.previous, None);
    assert_eq!(next.nav.previous, Some(date(6)));
    assert_eq!(next.nav.next, Some(date(8)));
    assert_eq!(next.nav.label, "Yesterday");
    assert!(!next.nav.live());
}

/// Both plots are rendered either way, so one entry serves both intervals.
#[tokio::test]
async fn a_cached_day_serves_either_interval() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    crate::journal::testing::record(&path, &two_days()).await;
    let past = PastDays::new(path, configured());
    let now = utc(8, 10, 0);

    let hourly = past.view(request(7, FlowResolution::Hour), now, tz());
    let quarterly = past.view(request(7, FlowResolution::Quarter), now, tz());

    assert_eq!(guard(&past.cache).len(), 1);
    assert_eq!(hourly.interval, FlowResolution::Hour);
    assert_eq!(quarterly.interval, FlowResolution::Quarter);
    assert_eq!(
        energy_flows::host_class(&quarterly),
        "energy-flows energy-flows--15m"
    );
}

/// The journal is written behind the live loop, so yesterday read just
/// after midnight may still be missing its last rows.
#[tokio::test]
async fn a_day_that_has_only_just_ended_is_not_cached() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    crate::journal::testing::record(&path, &two_days()).await;
    let past = PastDays::new(path, configured());

    // 00:02 CEST on 8 October.
    past.view(request(7, FlowResolution::Hour), utc(7, 22, 2), tz());
    assert_eq!(guard(&past.cache).len(), 0);

    past.view(request(7, FlowResolution::Hour), utc(7, 22, 10), tz());
    assert_eq!(guard(&past.cache).len(), 1);
}

#[test]
fn an_unreadable_journal_says_so_and_offers_no_step_back() {
    let dir = tempfile::tempdir().unwrap();
    let past = PastDays::new(dir.path().join("missing.db"), configured());

    let view = past.view(request(7, FlowResolution::Hour), utc(8, 10, 0), tz());

    assert!(view.unreadable);
    assert_eq!(view.nav.shown, date(7));
    assert_eq!(view.nav.previous, None);
    assert_eq!(guard(&past.cache).len(), 0, "a failed read is not cached");
    let html = energy_flows::render(&view).into_string();
    assert!(html.contains("energy-flows__unreadable"), "{html}");
    assert!(!html.contains("energy-flows__plot"), "{html}");
}
