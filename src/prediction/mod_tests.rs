use super::*;
use crate::config::PredictionConfig;
use crate::fixtures::local_now as at;
use crate::journal::testing;

/// The slot of the anchor the schedule would fetch on a fresh tick.
fn due(tracker: &ForecastTracker, now: &LocalNow) -> Option<TimeOfDay> {
    match tracker.schedule().decide(now) {
        Fetch::Anchor(anchor) => Some(anchor.slot),
        Fetch::Retry { .. } | Fetch::Nothing => None,
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
    assert_eq!(due(&t, &at(2026, 1, 1, 5, 59)), None);
}

#[test]
fn the_first_anchor_is_due_once_its_time_has_passed() {
    let (_dir, t) = tracker();
    assert_eq!(
        due(&t, &at(2026, 1, 1, 6, 0)),
        Some(TimeOfDay::new(6, 0).unwrap())
    );
}

#[test]
fn a_used_anchor_is_not_offered_again_the_same_day() {
    let (_dir, mut t) = tracker();
    let now = at(2026, 1, 1, 6, 0);
    t.mark_used(&now, TimeOfDay::new(6, 0).unwrap());
    assert_eq!(due(&t, &now), None);
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
        due(&restarted, &now),
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
    assert_eq!(due(&t, &evening), None);
}

/// A new calendar day resets the budget even though `fired` still lists
/// yesterday's anchors — `decide`/`mark_used` both key off `now.date`.
#[test]
fn a_new_day_treats_the_budget_as_fresh() {
    let (_dir, mut t) = tracker();
    let yesterday = at(2026, 1, 1, 23, 0);
    for slot in default_poll_times() {
        t.mark_used(&yesterday, slot);
    }

    let today = at(2026, 1, 2, 6, 0);
    assert_eq!(
        due(&t, &today),
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

    let first = due(&t, &now);
    assert_eq!(first, Some(TimeOfDay::new(12, 30).unwrap()));

    t.mark_used(&now, first.unwrap());
    assert_eq!(
        due(&t, &now),
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
        due(&restored, &now),
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
    assert_eq!(state.settled, vec![TimeOfDay::new(6, 0).unwrap()]);
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
    let solcast = from_config(
        &PredictionConfig::Solcast {
            api_key: "k".to_string(),
            site_east: "east".to_string(),
            site_west: "west".to_string(),
            state_path: "unused.json".into(),
            poll_times: default_poll_times().to_vec(),
        },
        chrono_tz::UTC,
    );
    assert!(matches!(solcast, Forecaster::Solcast(_)));

    let simulated = from_config(
        &PredictionConfig::Simulated {
            state_path: "unused.json".into(),
            poll_times: default_poll_times().to_vec(),
        },
        chrono_tz::UTC,
    );
    assert!(matches!(simulated, Forecaster::Simulated(_)));
}

// --- Retrying a failed anchor ----------------------------------------------------

/// Fails with each scripted error in turn, then answers with one point;
/// remembers every anchor asked for.
struct ScriptedForecaster {
    script: std::sync::Mutex<std::collections::VecDeque<FetchError>>,
    anchors: std::sync::Mutex<Vec<AnchorKey>>,
}

impl ScriptedForecaster {
    fn failing(errors: impl IntoIterator<Item = FetchError>) -> Self {
        ScriptedForecaster {
            script: std::sync::Mutex::new(errors.into_iter().collect()),
            anchors: std::sync::Mutex::default(),
        }
    }

    fn anchors(&self) -> Vec<AnchorKey> {
        self.anchors.lock().unwrap().clone()
    }
}

impl Prediction for ScriptedForecaster {
    async fn forecast(&self, anchor: AnchorKey) -> Result<Vec<SolarForecastPoint>, FetchError> {
        self.anchors.lock().unwrap().push(anchor);
        match self.script.lock().unwrap().pop_front() {
            Some(error) => Err(error),
            None => Ok(vec![point(1_700_000_000_000, 100.0)]),
        }
    }
}

fn transient() -> FetchError {
    FetchError::Request("timed out".into())
}

fn poller(forecaster: ScriptedForecaster, dir: &tempfile::TempDir) -> Poller<ScriptedForecaster> {
    let (journal, _writer, _path) = testing::open(dir, &serde_json::json!({}));
    let engine = crate::engine::EngineState {
        world: crate::world::World::new(),
        controller: crate::controller::Controller::test_default(
            crate::fixtures::journey::NOW_MS,
            crate::fixtures::journey::DAY,
        )
        .state(),
        mqtt_timed_out: false,
    };
    let seed = crate::web::DashboardState::seed(
        &engine,
        vec![],
        crate::fixtures::journey::interval_ring(),
        Timestamp::from_millis(crate::fixtures::journey::NOW_MS),
    );
    let (dashboard_tx, _) = tokio::sync::watch::channel(seed);
    Poller {
        forecaster,
        timezone: chrono_tz::UTC,
        tracker: ForecastTracker::new(
            dir.path().join("forecast.json"),
            default_poll_times().to_vec(),
            chrono_tz::UTC,
        ),
        dashboard_tx,
        journal: Arc::new(journal),
    }
}

/// Polls at each of `times`, returning how many fetches had been made after
/// each poll.
async fn poll_through(poller: &mut Poller<ScriptedForecaster>, times: &[(u32, u32)]) -> Vec<usize> {
    let mut counts = Vec::new();
    for (hour, minute) in times {
        poller.poll_at(at(2026, 1, 1, *hour, *minute)).await;
        counts.push(poller.forecaster.anchors().len());
    }
    counts
}

fn restarted(dir: &tempfile::TempDir) -> ForecastTracker {
    ForecastTracker::new(
        dir.path().join("forecast.json"),
        default_poll_times().to_vec(),
        chrono_tz::UTC,
    )
}

#[tokio::test]
async fn a_transient_failure_is_retried_with_the_same_anchor_and_success_settles_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut poller = poller(ScriptedForecaster::failing([transient()]), &dir);

    let counts = poll_through(
        &mut poller,
        &[(6, 0), (6, 1), (6, 9), (6, 10), (6, 11), (6, 30), (9, 0)],
    )
    .await;

    assert_eq!(counts, [1, 1, 1, 2, 2, 2, 2]);
    let anchors = poller.forecaster.anchors();
    assert_eq!(anchors[0], anchors[1]);
    assert!(poller.tracker.snapshot().as_of.is_some());
    assert_eq!(due(&poller.tracker, &at(2026, 1, 1, 6, 30)), None);
}

#[tokio::test]
async fn a_pending_retry_is_not_persisted_so_a_restart_still_finds_the_anchor_due() {
    let dir = tempfile::tempdir().unwrap();
    let mut poller = poller(ScriptedForecaster::failing([transient()]), &dir);

    poll_through(&mut poller, &[(6, 0)]).await;

    assert_eq!(
        due(&restarted(&dir), &at(2026, 1, 1, 6, 1)),
        Some(TimeOfDay::new(6, 0).unwrap())
    );
}

#[tokio::test]
async fn a_status_or_parse_failure_is_settled_without_a_retry() {
    let rejected = |code| FetchError::Status(reqwest::StatusCode::from_u16(code).unwrap());
    let garbled = || FetchError::Parse {
        body: "<html>".into(),
        error: "expected value".into(),
    };
    for error in [rejected(429), rejected(401), garbled()] {
        let dir = tempfile::tempdir().unwrap();
        let mut poller = poller(ScriptedForecaster::failing([error]), &dir);

        let counts = poll_through(&mut poller, &[(6, 0), (6, 10), (6, 20), (9, 0)]).await;

        assert_eq!(counts, [1, 1, 1, 1]);
        assert_eq!(due(&restarted(&dir), &at(2026, 1, 1, 6, 30)), None);
    }
}

#[tokio::test]
async fn giving_up_settles_the_anchor_and_persists_it() {
    let dir = tempfile::tempdir().unwrap();
    let failures = ScriptedForecaster::failing(std::iter::repeat_with(transient).take(3));
    let mut poller = poller(failures, &dir);

    let counts = poll_through(
        &mut poller,
        &[(6, 0), (6, 10), (6, 20), (6, 30), (9, 0), (9, 30)],
    )
    .await;

    // The anchor and its two retries, then silence until 09:30.
    assert_eq!(counts, [1, 2, 3, 3, 3, 4]);
    assert_eq!(due(&restarted(&dir), &at(2026, 1, 1, 6, 40)), None);
}
