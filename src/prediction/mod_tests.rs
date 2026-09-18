use super::*;
use crate::config::PredictionConfig;

fn date(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

fn at(y: i32, m: u32, d: u32, hour: u32, minute: u32) -> LocalNow {
    LocalNow {
        date: date(y, m, d),
        time: TimeOfDay::new(hour, minute).unwrap(),
    }
}

fn tracker() -> (tempfile::TempDir, ForecastTracker) {
    let dir = tempfile::tempdir().unwrap();
    let tracker = ForecastTracker::new(
        dir.path().join("forecast.json"),
        default_poll_times().to_vec(),
        chrono_tz::UTC,
    );
    (dir, tracker)
}

fn point(ms: i64, watts: f64) -> crate::units::SolarForecastPoint {
    crate::units::SolarForecastPoint {
        at: Timestamp::from_millis(ms),
        estimate: crate::units::SolarPower::new(watts),
    }
}

// --- Scheduler ---------------------------------------------------------------

#[test]
fn nothing_is_due_before_the_first_anchor() {
    let (_dir, t) = tracker();
    assert_eq!(t.next_due(&at(2026, 1, 1, 5, 59)), None);
}

#[test]
fn the_first_anchor_is_due_once_its_time_has_passed() {
    let (_dir, t) = tracker();
    assert_eq!(
        t.next_due(&at(2026, 1, 1, 6, 0)),
        Some(TimeOfDay::new(6, 0).unwrap())
    );
}

#[test]
fn a_used_anchor_is_not_offered_again_the_same_day() {
    let (_dir, mut t) = tracker();
    let now = at(2026, 1, 1, 6, 0);
    t.mark_used(&now, TimeOfDay::new(6, 0).unwrap());
    assert_eq!(t.next_due(&now), None);
}

/// A restart mid-day must not re-fetch anchors this process already spent
/// today — `ForecastTracker::new` loads whatever `fired` a prior process
/// persisted.
#[test]
fn a_restart_mid_day_skips_anchors_already_fired() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("forecast.json");
    let now = at(2026, 1, 1, 13, 0);

    {
        let mut t =
            ForecastTracker::new(path.clone(), default_poll_times().to_vec(), chrono_tz::UTC);
        t.mark_used(&now, TimeOfDay::new(6, 0).unwrap());
        t.mark_used(&now, TimeOfDay::new(9, 30).unwrap());
    }

    let restarted = ForecastTracker::new(path, default_poll_times().to_vec(), chrono_tz::UTC);
    assert_eq!(
        restarted.next_due(&now),
        Some(TimeOfDay::new(12, 30).unwrap()),
        "the first two anchors were already spent before the restart"
    );
}

#[test]
fn once_every_anchor_is_used_nothing_is_due_for_the_rest_of_the_day() {
    let (_dir, mut t) = tracker();
    let evening = at(2026, 1, 1, 23, 0);
    for slot in default_poll_times() {
        t.mark_used(&evening, slot);
    }
    assert_eq!(t.next_due(&evening), None);
}

/// A new calendar day resets the budget even though `fired` still lists
/// yesterday's anchors — `next_due`/`mark_used` both key off `now.date`.
#[test]
fn a_new_day_treats_the_budget_as_fresh() {
    let (_dir, mut t) = tracker();
    let yesterday = at(2026, 1, 1, 23, 0);
    for slot in default_poll_times() {
        t.mark_used(&yesterday, slot);
    }

    let today = at(2026, 1, 2, 6, 0);
    assert_eq!(
        t.next_due(&today),
        Some(TimeOfDay::new(6, 0).unwrap()),
        "a new day must not inherit yesterday's spent budget"
    );
}

/// If the process was down through the first two anchors, only the latest
/// one that has passed is offered — not all three collapsed into a burst of
/// requests — and `mark_used` for it marks the earlier ones fired too, so
/// they are never offered later in the day either.
#[test]
fn a_catch_up_after_downtime_collapses_into_a_single_fetch() {
    let (_dir, mut t) = tracker();
    let now = at(2026, 1, 1, 13, 0); // past anchors 1-3 (06:00, 09:30, 12:30)

    let due = t.next_due(&now);
    assert_eq!(due, Some(TimeOfDay::new(12, 30).unwrap()));

    t.mark_used(&now, due.unwrap());
    assert_eq!(
        t.next_due(&now),
        None,
        "the earlier anchors must not be offered after the catch-up"
    );
}

// --- Persistence --------------------------------------------------------------

#[test]
fn state_round_trips_through_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("forecast.json");
    let now = at(2026, 1, 1, 6, 0);
    let points = vec![crate::units::SolarForecastPoint {
        at: Timestamp::from_millis(1_700_000_000_000),
        estimate: crate::units::SolarPower::new(1234.0),
    }];

    {
        let mut t =
            ForecastTracker::new(path.clone(), default_poll_times().to_vec(), chrono_tz::UTC);
        t.set_forecast(points.clone(), Timestamp::from_millis(1_700_000_000_000));
        t.mark_used(&now, TimeOfDay::new(6, 0).unwrap());
    }

    let restored = ForecastTracker::new(path, default_poll_times().to_vec(), chrono_tz::UTC);
    let snapshot = restored.snapshot();
    assert_eq!(snapshot.points, points);
    assert_eq!(
        snapshot.as_of,
        Some(Timestamp::from_millis(1_700_000_000_000))
    );
    assert_eq!(
        restored.next_due(&now),
        None,
        "the fired anchor survived the restart"
    );
}

#[test]
fn a_missing_state_directory_is_created() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested/state/forecast.json");
    let mut t = ForecastTracker::new(path.clone(), default_poll_times().to_vec(), chrono_tz::UTC);
    t.mark_used(&at(2026, 1, 1, 6, 0), TimeOfDay::new(6, 0).unwrap());
    assert!(path.exists());
}

#[test]
fn an_unwritable_path_does_not_panic() {
    let dir = tempfile::tempdir().unwrap();
    let blocker = dir.path().join("iam-a-file");
    std::fs::write(&blocker, b"x").unwrap();

    let mut t = ForecastTracker::new(
        blocker.join("state.json"),
        default_poll_times().to_vec(),
        chrono_tz::UTC,
    );
    let now = at(2026, 1, 1, 6, 0);
    t.mark_used(&now, TimeOfDay::new(6, 0).unwrap());
    t.mark_used(&now, TimeOfDay::new(9, 30).unwrap()); // second failure takes the quiet path
}

#[test]
fn a_corrupt_state_file_is_handled() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("forecast.json");
    std::fs::write(&path, b"not valid json").unwrap();

    let t = ForecastTracker::new(path, default_poll_times().to_vec(), chrono_tz::UTC);
    assert!(t.snapshot().points.is_empty());
}

// --- Merging a fresh fetch -----------------------------------------------------

/// The bug this exists to fix: Solcast's forward-looking response drops
/// hours that have already elapsed, and a wholesale replace used to take the
/// cached bar for that hour down with it.
#[test]
fn a_second_fetch_keeps_a_timestamp_the_new_fetch_no_longer_covers() {
    let (_dir, mut t) = tracker();
    let first_at = Timestamp::from_millis(1_700_000_000_000); // an arbitrary "now"
    t.set_forecast(vec![point(1_700_000_000_000, 500.0)], first_at);

    // The next fetch, an hour later, no longer mentions that earlier slot —
    // exactly what Solcast's forward-looking API does.
    let second_at = Timestamp::from_millis(1_700_003_600_000);
    t.set_forecast(vec![point(1_700_007_200_000, 800.0)], second_at);

    let points = t.snapshot().points;
    assert!(
        points.iter().any(|p| p.at.as_millis() == 1_700_000_000_000),
        "the earlier point must survive a fetch that no longer mentions it: {points:?}"
    );
    assert!(points.iter().any(|p| p.at.as_millis() == 1_700_007_200_000));
}

/// A timestamp both fetches share gets the fresh estimate, not the stale one.
#[test]
fn a_second_fetch_overwrites_a_shared_timestamp_with_fresh_data() {
    let (_dir, mut t) = tracker();
    let at_ms = 1_700_000_000_000;
    t.set_forecast(vec![point(at_ms, 500.0)], Timestamp::from_millis(at_ms));
    t.set_forecast(vec![point(at_ms, 900.0)], Timestamp::from_millis(at_ms));

    let points = t.snapshot().points;
    assert_eq!(points.len(), 1);
    assert_eq!(points[0].estimate, crate::units::SolarPower::new(900.0));
}

/// A fetch on a new calendar day prunes yesterday's points — durable history
/// lives in the journal now, so this cache only needs to cover today.
#[test]
fn a_fetch_on_a_new_day_prunes_points_from_before_today() {
    use chrono::TimeZone;
    let (_dir, mut t) = tracker();

    let yesterday_evening = chrono_tz::UTC
        .with_ymd_and_hms(2026, 1, 1, 20, 0, 0)
        .unwrap();
    t.set_forecast(
        vec![point(yesterday_evening.timestamp_millis(), 500.0)],
        Timestamp::from(yesterday_evening),
    );

    let this_morning = chrono_tz::UTC
        .with_ymd_and_hms(2026, 1, 2, 6, 0, 0)
        .unwrap();
    t.set_forecast(
        vec![point(this_morning.timestamp_millis(), 800.0)],
        Timestamp::from(this_morning),
    );

    let points = t.snapshot().points;
    assert_eq!(
        points.len(),
        1,
        "yesterday's point must be pruned once today's fetch lands: {points:?}"
    );
    assert_eq!(points[0].at, Timestamp::from(this_morning));
}

/// A wire-format pin: a later refactor of `PersistedForecastState` can't
/// silently change what's already on disk in production.
#[test]
fn the_persisted_state_wire_format_is_pinned() {
    let json = r#"{"date":"2026-01-01","fired":["06:00"],"points":[{"at":1700000000000,"estimate":1234.0}],"fetched_at":1700000000000}"#;

    let state: PersistedForecastState = serde_json::from_str(json).unwrap();
    assert_eq!(state.date, "2026-01-01");
    assert_eq!(state.fired, vec![TimeOfDay::new(6, 0).unwrap()]);
    assert_eq!(state.points.len(), 1);
    assert_eq!(state.fetched_at, Some(1_700_000_000_000));

    let round_tripped = serde_json::to_string(&state).unwrap();
    assert_eq!(round_tripped, json);
}

// --- Dispatch -----------------------------------------------------------------

/// `from_config` builds the matching `Forecaster` arm — the only place a
/// `PredictionConfig` becomes a live backend.
#[test]
fn from_config_builds_the_matching_backend() {
    let solcast = from_config(&PredictionConfig::Solcast {
        api_key: "k".to_string(),
        site_east: "east".to_string(),
        site_west: "west".to_string(),
        state_path: "unused.json".into(),
        poll_times: default_poll_times().to_vec(),
    });
    assert!(matches!(solcast, Forecaster::Solcast(_)));

    let simulated = from_config(&PredictionConfig::Simulated {
        state_path: "unused.json".into(),
        poll_times: default_poll_times().to_vec(),
    });
    assert!(matches!(simulated, Forecaster::Simulated(_)));
}
