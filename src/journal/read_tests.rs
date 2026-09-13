use super::*;

use crate::config::SessionConfig;
use crate::device::{Applied, ControlPath, Outcome};
use crate::engine::Engine;
use crate::fixtures::journey;
use crate::journal::Journal;
use crate::journal::backdate;
use crate::journal::testing;
use crate::journal::testing::{record, record_with};
use crate::models::{ControlDecision, ControlMode};
use crate::units::{GridPower, Setpoint};
use crate::world::World;

fn config() -> SessionConfig {
    SessionConfig::test_default()
}

fn engine() -> Engine {
    Engine::new(
        crate::controller::Controller::from_session(&config(), &journey::clock_at(0)),
        World::new(),
        std::time::Duration::from_secs(config().mqtt_timeout_secs),
    )
}

fn start() -> Timestamp {
    Timestamp::from_millis(journey::NOW_MS - 1)
}

fn end() -> Timestamp {
    Timestamp::from_millis(journey::NOW_MS + 1_000_000)
}

fn commands(frame: &RecordedFrame) -> Vec<String> {
    frame
        .commands
        .iter()
        .map(|(d, c)| format!("{d} {c}"))
        .collect()
}

#[tokio::test]
async fn a_recorded_run_reads_back_as_events_paired_with_their_commands() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    record(&path, &journey::session()).await;

    let recording = read_range(&path, start(), end()).unwrap();

    // The startup `device_update` decides nothing; the meters that follow do.
    assert!(recording.frames[0].commands.is_empty());
    assert!(recording.frames.iter().any(|f| !f.commands.is_empty()));
    assert_eq!(recording.version, "0.0.0-test");
    assert_eq!(recording.config, config());
}

/// **The property the whole `seq` column exists for.**
///
/// `mqtt.rs` writes its pre-parse `shelly` capture from a different task, so it
/// can land between an event and the decision that event caused. Alignment by
/// "the next row" would then attribute the raw row's position to the decision,
/// and alignment by timestamp cannot separate them at all — a decision carries
/// its event's millisecond. This is the only test that fails if either
/// regression is made.
#[tokio::test]
async fn a_raw_capture_between_an_event_and_its_decision_does_not_misalign_it() {
    let dir = tempfile::tempdir().unwrap();
    let clean = dir.path().join("clean.db");
    let interleaved = dir.path().join("interleaved.db");

    let events = journey::session();
    record(&clean, &events).await;
    // Index 1 is the first meter reading, the first event that decides.
    record_with(&interleaved, &events, Some(1)).await;

    let a = read_range(&clean, start(), end()).unwrap();
    let b = read_range(&interleaved, start(), end()).unwrap();

    let a: Vec<Vec<String>> = a.frames.iter().map(commands).collect();
    let b: Vec<Vec<String>> = b.frames.iter().map(commands).collect();
    assert_eq!(
        a, b,
        "an interleaved raw row moved a command to another frame"
    );
    assert!(a[1].len() == 1, "frame 1 should carry exactly one command");
}

/// The seed is a decision row, so the range opens at that decision and not at
/// `from` — it can begin earlier than asked, and that is the honest boundary.
#[tokio::test]
async fn a_range_is_anchored_to_the_last_decision_at_or_before_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    let events = journey::session();
    record(&path, &events).await;

    let from = events[7].at();
    let recording = read_range(&path, from, end()).unwrap();

    let seed = recording.seed.expect("a decision precedes this range");
    assert!(seed.at <= from);
    assert_eq!(recording.frames[0].event.at(), events[8].at());
}

/// A restart between the seed and the events is the *likeliest* straddle, not
/// an edge case: the daemon's last act before stopping is a decision, so the
/// seed is always the pre-restart session. A session set fed from event rows
/// alone misses this, and the fixture then carries the wrong tuning silently.
#[tokio::test]
async fn a_restart_between_the_seed_and_the_events_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    let events = journey::session();

    // Session A records up to and including a decision.
    record(&path, &events[..3]).await;
    // Session B records the rest, into the same file.
    record(&path, &events[3..]).await;

    let recording = read_range(&path, events[2].at(), end()).unwrap();
    assert!(
        recording
            .warnings
            .iter()
            .any(|w| w.contains("spans a restart")),
        "{:?}",
        recording.warnings
    );
}

#[tokio::test]
async fn one_session_is_not_reported_as_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    record(&path, &journey::session()).await;

    let recording = read_range(&path, start(), end()).unwrap();
    assert!(
        !recording
            .warnings
            .iter()
            .any(|w| w.contains("spans a restart")),
        "{:?}",
        recording.warnings
    );
}

/// `prune` deletes `sessions WHERE started_ms < cutoff`, and a session row is
/// dated at process start while its rows are dated individually — so a daemon
/// outliving the retention window would delete the row describing itself. The
/// writer exempts its own session; a reader meeting an already-orphaned row
/// must still produce a fixture rather than failing outright.
#[tokio::test]
async fn a_pruned_session_degrades_instead_of_failing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    record(&path, &journey::session()).await;

    // A second session so there is something to fall back to, then delete the
    // first one's row out from under its rows.
    record(&path, &journey::session()).await;
    let conn = Connection::open(&path).unwrap();
    conn.execute("DELETE FROM sessions WHERE id = 1", [])
        .unwrap();
    drop(conn);

    let recording = read_range(&path, start(), end()).unwrap();
    assert!(
        recording.warnings.iter().any(|w| w.contains("pruned")),
        "{:?}",
        recording.warnings
    );
    assert!(!recording.frames.is_empty());
}

/// The writer must not delete the row describing the process that is running.
#[tokio::test]
async fn prune_keeps_the_running_sessions_own_row() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");

    // One session's worth of rows, aged past any window.
    let (j, writer, _) = testing::open(&dir, &config());
    j.event(&journey::startup());
    testing::close(j, writer).await;
    let conn = Connection::open(&path).unwrap();
    backdate(&conn, 5000);
    drop(conn);

    // A second start prunes on open. Its own session row is younger than the
    // cutoff anyway, so what this pins is that the *exemption* does not also
    // rescue the old one.
    let (j, writer, _) = testing::open(&dir, &config());
    testing::close(j, writer).await;

    let conn = Connection::open(&path).unwrap();
    assert_eq!(testing::count(&conn, "SELECT count(*) FROM sessions"), 1);
}

#[tokio::test]
async fn an_empty_range_reads_back_empty_rather_than_erroring() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    record(&path, &journey::session()).await;

    let before = Timestamp::from_millis(journey::NOW_MS - 10_000);
    let recording = read_range(&path, before, before).unwrap();
    assert!(recording.frames.is_empty());
}

/// A journal with a session row and nothing else is a daemon that started and
/// was stopped. It has no fold to replay, and saying so beats a SQL error.
#[tokio::test]
async fn a_journal_with_no_events_reads_back_empty() {
    let dir = tempfile::tempdir().unwrap();
    let (j, writer, path) = testing::open(&dir, &config());
    testing::close(j, writer).await;

    let recording = read_range(&path, start(), end()).unwrap();
    assert!(recording.frames.is_empty());
    assert!(recording.seed.is_none());
    assert_eq!(recording.config, config());
}

/// An event row this build cannot parse is skipped — and must say so, because
/// its decision rows are still there and will pair with the event before it.
/// Silently, `--verify` would blame the fold for a lossy recording.
#[tokio::test]
async fn an_unreadable_event_is_skipped_loudly() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    record(&path, &journey::session()).await;

    let conn = Connection::open(&path).unwrap();
    conn.execute(
        "UPDATE events SET payload_json = '{\"kind\":\"from_the_future\"}' WHERE seq = 2",
        [],
    )
    .unwrap();
    drop(conn);

    let recording = read_range(&path, start(), end()).unwrap();
    assert!(
        recording
            .warnings
            .iter()
            .any(|w| w.contains("unreadable event")),
        "{:?}",
        recording.warnings
    );
}

/// The tail of the file is where a decision may still be in flight behind an
/// HTTP write, so the newest event is dropped rather than recorded as having
/// commanded nothing — which would fail `--verify` against a replay that does.
#[tokio::test]
async fn the_newest_event_is_left_out_because_its_decision_may_not_exist_yet() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    let events = journey::session();
    record(&path, &events).await;

    let recording = read_range(&path, start(), end()).unwrap();
    let last = recording.frames.last().unwrap().event.at();

    // The journey's final event is a meter reading, which decides — so the last
    // row in the file is its decision, not the event, and nothing is dropped.
    assert_eq!(last, events[8].at());

    // Now a journal whose last row *is* an event: one that decides nothing.
    let path2 = dir.path().join("tail.db");
    let mut trailing = events.clone();
    trailing.push(journey::startup());
    record(&path2, &trailing).await;

    let recording = read_range(&path2, start(), end()).unwrap();
    assert!(
        recording.warnings.iter().any(|w| w.contains("newest row")),
        "{:?}",
        recording.warnings
    );
    assert_eq!(recording.frames.last().unwrap().event.at(), events[8].at());
}

/// A database this build cannot write is a database it must not read either —
/// the schema guard protected only the writer, so a v1 file failed later, with
/// `no such column: seq`.
#[tokio::test]
async fn a_database_from_another_schema_version_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    record(&path, &journey::session()).await;

    let conn = Connection::open(&path).unwrap();
    conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
        .unwrap();
    drop(conn);

    let err = read_range(&path, start(), end()).unwrap_err();
    assert!(matches!(err, ReadError::Schema(_)), "{err}");
}

/// A reader must not create the database it is asked to read. Opening
/// read-write left a 0-byte file behind on a mistyped `--db` and then failed
/// with `no such table`, which describes the file we just made rather than the
/// one that is missing.
#[test]
fn a_missing_database_is_not_created_by_reading_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("absent.db");

    assert!(read_range(&path, start(), end()).is_err());
    assert!(!path.exists(), "reading created a database");
}

/// Alignment is linear, not a rescan per event. Pinned as a property rather
/// than a benchmark: every decision has to land on exactly one frame, so a
/// walk that lost its place would show up as a missing or duplicated command.
#[test]
fn pairing_assigns_every_decision_to_exactly_one_frame() {
    let events: Vec<(i64, Event)> = (0..5)
        .map(|i| {
            (
                i * 10,
                Event::MqttTimeout {
                    at: journey::clock_at(i),
                },
            )
        })
        .collect();
    let decisions: Vec<(i64, Option<String>, Option<String>)> = (0..5)
        .map(|i| {
            (
                i * 10 + 1,
                Some(format!("dev{i}")),
                Some("set_idle".to_string()),
            )
        })
        .collect();

    let frames = pair(events, &decisions);
    assert_eq!(frames.len(), 5);
    for (i, frame) in frames.iter().enumerate() {
        assert_eq!(commands(frame), vec![format!("dev{i} set_idle")]);
    }
}

/// A decision that commanded nothing still gets a row, with both columns null.
/// That is an empty command list, not a missing one, and the two render
/// differently.
#[test]
fn a_decision_that_commanded_nothing_pairs_as_an_empty_list() {
    let events = vec![(
        1,
        Event::MqttTimeout {
            at: journey::clock_at(0),
        },
    )];
    let frames = pair(events, &[(2, None, None)]);
    assert_eq!(frames.len(), 1);
    assert!(frames[0].commands.is_empty());
}

/// Rows belonging to an event outside the slice must not be swept onto the last
/// frame. `pair` bounds the final frame at `i64::MAX` on purpose — it is the
/// query that keeps the two lists consistent, not the walk — so this pins the
/// end-to-end property rather than the helper in isolation.
///
/// Both halves of that consistency are load-bearing: a decision carries its
/// event's `ts_ms`, so the same `ts_ms <= to` bound excludes both; and the read
/// happens in one transaction, so the daemon cannot append a decision between
/// the two queries and leave it with no event to belong to. That interleaving
/// is what produced fixtures asserting two commands on a single-battery step.
#[tokio::test]
async fn a_bounded_range_does_not_pick_up_a_later_events_commands() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    let events = journey::session();
    record(&path, &events).await;

    // Cut the range at an event that decides, with several more after it.
    let to = events[3].at();
    let recording = read_range(&path, start(), to).unwrap();

    let last = recording.frames.last().unwrap();
    assert_eq!(last.event.at(), to);
    assert!(
        last.commands.len() <= 1,
        "a later event's commands landed on the last frame: {:?}",
        commands(last)
    );
}

/// Kept from the writer's own suite, now expressed through the reader: a
/// decision row is enough to resume the fold.
#[tokio::test]
async fn a_seed_restores_a_working_engine() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    record(&path, &journey::session()).await;

    let recording = read_range(&path, end(), end()).unwrap();
    let seed = recording.seed.expect("the journey decides");

    let mut engine = engine();
    engine.restore(seed.state.clone());
    assert_eq!(engine.state(), seed.state);
    assert!(engine.battery().is_some(), "the seed carries the world");
}

/// `decision()` is journalled with the same `Clock` the event carries, so the
/// two timestamps are equal — which is precisely why timestamps cannot align
/// them and `seq` has to.
#[tokio::test]
async fn a_decision_carries_its_events_timestamp() {
    let dir = tempfile::tempdir().unwrap();
    let (j, writer, path) = testing::open(&dir, &config());
    let event = Event::MqttTimeout {
        at: journey::clock_at(0),
    };
    j.event(&event);
    j.decision(
        event.at(),
        ControlPath::Failsafe,
        &ControlDecision {
            mode: ControlMode::Idle,
            power_watts: Setpoint::ZERO,
            reason: "test".to_string(),
            grid_power: GridPower::ZERO,
        },
        &engine().state(),
        &[Outcome {
            device: DeviceId::new("dev"),
            command: "set_idle".to_string(),
            applied: Applied::Ok,
            error: None,
        }],
    );
    testing::close(j, writer).await;

    let conn = Connection::open(&path).unwrap();
    let (e, d): (i64, i64) = conn
        .query_row(
            "SELECT (SELECT ts_ms FROM events), (SELECT ts_ms FROM decisions)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(e, d);
}

// --- `read_recent_decisions` (the dashboard's seeded log) ---------------------

/// One `Journal::decision` call against `devices`, at `at_ms`, reasoned so the
/// assertions below can tell the rows apart.
fn log_decision(j: &Journal, at_ms: i64, reason: &str, devices: &[&str]) {
    let outcomes: Vec<Outcome> = devices
        .iter()
        .map(|d| Outcome {
            device: DeviceId::new(*d),
            command: "set_output_limit".to_string(),
            applied: Applied::Ok,
            error: None,
        })
        .collect();

    j.decision(
        Timestamp::from_millis(at_ms),
        ControlPath::Objective,
        &ControlDecision {
            reason: reason.to_string(),
            ..ControlDecision::test_sample()
        },
        &engine().state(),
        &outcomes,
    );
}

fn reasons(rows: &[DecisionRow]) -> Vec<String> {
    rows.iter().map(|r| r.decision.reason.clone()).collect()
}

/// `Journal::decision` writes one row per commanded device, so a two-battery
/// site records two rows for one decision. The dashboard's live path appends
/// one entry per decision, and the seeded log has to agree with it.
#[tokio::test]
async fn recent_decisions_reads_one_row_per_decision_not_per_device() {
    let dir = tempfile::tempdir().unwrap();
    let (j, writer, path) = testing::open(&dir, &config());

    log_decision(&j, 1_000, "first", &["SN1", "SN2", "SN3"]);
    log_decision(&j, 2_000, "second", &["SN1", "SN2", "SN3"]);
    testing::close(j, writer).await;

    let rows = read_recent_decisions(&path, 20).unwrap();
    assert_eq!(reasons(&rows), vec!["first", "second"]);
}

/// Oldest first, which is the order the dashboard's `VecDeque` is built in —
/// the query walks `seq` backwards and the reader reverses it.
#[tokio::test]
async fn recent_decisions_come_back_oldest_first() {
    let dir = tempfile::tempdir().unwrap();
    let (j, writer, path) = testing::open(&dir, &config());

    for n in 1..=4 {
        log_decision(&j, n * 1_000, &format!("d{n}"), &["SN1"]);
    }
    testing::close(j, writer).await;

    let rows = read_recent_decisions(&path, 20).unwrap();
    assert_eq!(reasons(&rows), vec!["d1", "d2", "d3", "d4"]);
    assert_eq!(rows[0].at, Timestamp::from_millis(1_000));
}

/// The limit counts decisions, not rows, and keeps the newest ones.
#[tokio::test]
async fn recent_decisions_limit_keeps_the_newest_decisions() {
    let dir = tempfile::tempdir().unwrap();
    let (j, writer, path) = testing::open(&dir, &config());

    for n in 1..=5 {
        log_decision(&j, n * 1_000, &format!("d{n}"), &["SN1", "SN2"]);
    }
    testing::close(j, writer).await;

    let rows = read_recent_decisions(&path, 2).unwrap();
    assert_eq!(reasons(&rows), vec!["d4", "d5"]);
}

/// A decision that commanded nothing writes a single row with a NULL `device`
/// and still belongs in the log.
#[tokio::test]
async fn recent_decisions_include_one_that_commanded_no_device() {
    let dir = tempfile::tempdir().unwrap();
    let (j, writer, path) = testing::open(&dir, &config());

    log_decision(&j, 1_000, "commanded nothing", &[]);
    testing::close(j, writer).await;

    let rows = read_recent_decisions(&path, 20).unwrap();
    assert_eq!(reasons(&rows), vec!["commanded nothing"]);
}

/// A row this build cannot decode costs the dashboard that one entry, not the
/// whole log.
#[tokio::test]
async fn recent_decisions_skips_an_undecodable_row() {
    let dir = tempfile::tempdir().unwrap();
    let (j, writer, path) = testing::open(&dir, &config());

    for n in 1..=3 {
        log_decision(&j, n * 1_000, &format!("d{n}"), &["SN1"]);
    }
    testing::close(j, writer).await;

    let conn = Connection::open(&path).unwrap();
    conn.execute(
        "UPDATE decisions SET payload_json = '{\"from\":\"the future\"}' WHERE ts_ms = 2000",
        [],
    )
    .unwrap();
    drop(conn);

    let rows = read_recent_decisions(&path, 20).unwrap();
    assert_eq!(reasons(&rows), vec!["d1", "d3"]);
}

/// The dashboard's reader refuses a journal from another schema version for the
/// same reason `read_range` does.
#[tokio::test]
async fn recent_decisions_of_another_schema_version_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (j, writer, path) = testing::open(&dir, &config());
    log_decision(&j, 1_000, "first", &["SN1"]);
    testing::close(j, writer).await;

    let conn = Connection::open(&path).unwrap();
    conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
        .unwrap();
    drop(conn);

    let err = read_recent_decisions(&path, 20).unwrap_err();
    assert!(matches!(err, ReadError::Schema(_)), "{err}");
}

/// An empty journal is a valid journal — the dashboard renders an empty log,
/// not an error.
#[tokio::test]
async fn recent_decisions_of_a_journal_with_none_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    let (j, writer, path) = testing::open(&dir, &config());
    testing::close(j, writer).await;

    assert!(read_recent_decisions(&path, 20).unwrap().is_empty());
}
