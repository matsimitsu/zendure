use super::testing::{count, days, drain};
use super::*;
use crate::battery::BatteryState;
use crate::clock::Clock;
use crate::controller::Controller;
use crate::device::Applied;
use crate::engine::Engine;
use crate::units::{BatteryPower, GridPower, SolarPower, Timestamp};
use crate::world::{DeviceId, Measurement, MeterReading, World};

const NOW_MS: i64 = 1_757_000_000_000;

fn at() -> Timestamp {
    Timestamp::from_millis(NOW_MS)
}

fn clock() -> Clock {
    Clock {
        hour: 19,
        day_ordinal: 255,
        ..Clock::test_at(NOW_MS)
    }
}

fn battery() -> BatteryState {
    BatteryState {
        current_power: BatteryPower(-300),
        ..BatteryState::test_sample()
    }
}

fn world() -> World {
    let mut world = World::new();
    world.observe_meter(
        MeterReading::total_only(GridPower(150.5)),
        SolarPower::new(200.0),
    );
    world.observe_device(DeviceId::new("SN123"), Measurement::Battery(battery()));
    world
}

fn engine_state() -> EngineState {
    EngineState {
        world: world(),
        controller: Controller::test_default(NOW_MS, 255).state(),
        mqtt_timed_out: false,
    }
}

fn outcome(device: &str, applied: Applied, error: Option<&str>) -> Outcome {
    Outcome {
        device: DeviceId::new(device),
        command: "set_discharge(145W)".to_string(),
        applied,
        error: error.map(str::to_string),
    }
}

/// This file's journals carry a stand-in config; the reader's tests use a
/// real `SessionConfig` because they read it back.
fn open(dir: &tempfile::TempDir) -> (Journal, Writer, std::path::PathBuf) {
    testing::open(dir, &serde_json::json!({"k": 1}))
}

fn seq_order(conn: &Connection) -> Vec<(i64, String)> {
    let mut stmt = conn
        .prepare(
            "SELECT seq, kind FROM events
                 UNION ALL
                 SELECT seq, 'decision:' || kind FROM decisions
                 ORDER BY seq",
        )
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// A decision is ordered strictly after the event that produced it, across
/// two tables whose row ids run independently. Without it, "everything after
/// this snapshot" can only be asked in milliseconds, and two records sharing
/// one millisecond make the boundary ambiguous.
#[tokio::test]
async fn seq_orders_events_and_decisions_against_each_other() {
    let dir = tempfile::tempdir().unwrap();
    let (journal, writer, path) = open(&dir);

    journal.event(&Event::MqttTimeout { at: clock() });
    journal.decision(
        at(),
        ControlPath::Failsafe,
        &ControlDecision::test_sample(),
        &engine_state(),
        &[outcome("SN123", Applied::Ok, None)],
    );
    journal.event(&Event::MqttTimeout { at: clock() });

    let conn = drain(journal, writer, &path).await;
    let rows = seq_order(&conn);

    assert_eq!(
        rows,
        vec![
            (1, "mqtt_timeout".to_string()),
            (2, "decision:failsafe".to_string()),
            (3, "mqtt_timeout".to_string()),
        ]
    );
}

/// `seq` numbers the file, not the process. A replay seeded from a decision
/// written before a restart asks for "everything after that row" without
/// knowing which session either side came from — which only works if the
/// counter carries over.
#[tokio::test]
async fn seq_continues_across_sessions() {
    let dir = tempfile::tempdir().unwrap();
    // Not `clock()`: that fixture is pinned to a fixed past instant, and the
    // second session's startup prune would delete the first session's row
    // before this could look at it.
    let now = Clock::test_at(Utc::now().timestamp_millis());

    let (journal, writer, path) = open(&dir);
    journal.event(&Event::MqttTimeout { at: now });
    drain(journal, writer, &path).await;

    let (journal, writer, path) = open(&dir);
    journal.event(&Event::MqttTimeout { at: now });
    let conn = drain(journal, writer, &path).await;

    let seqs: Vec<i64> = seq_order(&conn).into_iter().map(|(seq, _)| seq).collect();
    assert_eq!(seqs, vec![1, 2]);
    assert_eq!(
        count(&conn, "SELECT count(DISTINCT session_id) FROM events"),
        2
    );
}

/// `CREATE TABLE IF NOT EXISTS` would otherwise accept a database with a
/// mismatched column set and fail every insert; the `user_version` stamp
/// makes refusing it, once, with the reason, possible instead.
#[test]
fn a_database_from_another_schema_version_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");

    let conn = Connection::open(&path).unwrap();
    prepare(&conn, "0.0.0-test", "{}").unwrap();
    conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
        .unwrap();
    drop(conn);

    let (journal, writer) =
        Journal::open(&path, days(90), "0.0.0-test", &serde_json::json!({"k": 1}));
    assert!(
        writer.is_none(),
        "a mismatched database must not be written to"
    );

    // Disabled, not panicking: the controller starts without a journal.
    journal.event(&Event::MqttTimeout { at: clock() });

    let conn = Connection::open(&path).unwrap();
    assert_eq!(count(&conn, "SELECT count(*) FROM sessions"), 1);
}

#[tokio::test]
async fn opening_records_a_session_row() {
    let dir = tempfile::tempdir().unwrap();
    let (journal, writer, path) = open(&dir);
    let conn = drain(journal, writer, &path).await;

    let (version, config): (String, String) = conn
        .query_row("SELECT version, config_json FROM sessions", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(version, "0.0.0-test");
    assert_eq!(config, r#"{"k":1}"#);
}

/// All three pragmas, pinned by value. `auto_vacuum` can only be set while the
/// database is empty and `pragma_update` reports success either way, so setting
/// it after `journal_mode=WAL` (itself a write) would silently leave it at NONE.
/// `synchronous` is asserted on `prepare`'s own connection, not a reopened one,
/// since it is per-connection and not stored in the file.
#[test]
fn prepare_configures_the_connection_for_a_control_loop() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("j.db");
    let conn = Connection::open(&path).unwrap();
    prepare(&conn, "0.0.0-test", "{}").unwrap();

    let pragma = |name: &str| -> i64 {
        conn.query_row(&format!("PRAGMA {name}"), [], |r| r.get(0))
            .unwrap()
    };
    // 2 = INCREMENTAL, 1 = NORMAL.
    assert_eq!(pragma("auto_vacuum"), 2, "auto_vacuum must be INCREMENTAL");
    assert_eq!(pragma("synchronous"), 1, "synchronous must be NORMAL");

    // These two are in the file header, so they also have to survive being
    // reopened — which is what the writer's own connection does at startup.
    drop(conn);
    let reopened = Connection::open(&path).unwrap();
    let mode: String = reopened
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    assert_eq!(
        reopened
            .query_row("PRAGMA auto_vacuum", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2,
    );
}

/// The whole reason the pre-parse capture survived the move off NDJSON: a
/// payload we cannot decode is the one worth having, and an unmodelled
/// field has to still be there when we discover we want it.
#[tokio::test]
async fn raw_payloads_are_stored_verbatim() {
    let dir = tempfile::tempdir().unwrap();
    let (journal, writer, path) = open(&dir);
    journal.raw("shelly", r#"{"total_act_power":150.5,"unmodelled":"kept"}"#);
    let conn = drain(journal, writer, &path).await;

    let stored: String = conn
        .query_row(
            "SELECT payload_json FROM events WHERE kind = 'shelly'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stored, r#"{"total_act_power":150.5,"unmodelled":"kept"}"#);
}

/// A truncated device response still gets recorded, as a string, rather
/// than being dropped for failing to parse.
#[tokio::test]
async fn malformed_payloads_are_kept_as_a_string() {
    let dir = tempfile::tempdir().unwrap();
    let (journal, writer, path) = open(&dir);
    journal.raw("zendure_poll", r#"{"electricLevel": 4"#);
    journal.raw("shelly", r#"{"ok":true}"#);
    let conn = drain(journal, writer, &path).await;

    let stored: String = conn
        .query_row(
            "SELECT payload_json FROM events WHERE kind = 'zendure_poll'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stored, r#""{\"electricLevel\": 4""#);
    // The bad row did not take the good one with it.
    assert_eq!(count(&conn, "SELECT count(*) FROM events"), 2);
}

/// Events are journaled under the same kind the enum reports, and the
/// payload reads back as the event that produced it.
#[tokio::test]
async fn events_round_trip_through_the_database() {
    let dir = tempfile::tempdir().unwrap();
    let (journal, writer, path) = open(&dir);
    let event = Event::Meter {
        at: clock(),
        grid: MeterReading::total_only(GridPower(150.5)),
        solar: SolarPower::new(200.0),
    };
    journal.event(&event);
    let conn = drain(journal, writer, &path).await;

    let (ts_ms, kind, payload): (i64, String, String) = conn
        .query_row("SELECT ts_ms, kind, payload_json FROM events", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap();
    assert_eq!(
        ts_ms, NOW_MS,
        "ts comes from the event's clock, not the wall"
    );
    assert_eq!(kind, "meter");
    assert_eq!(event, serde_json::from_str::<Event>(&payload).unwrap());
}

/// One row per commanded device, each carrying that device's own outcome.
/// Recording the decision once with a merged outcome would lose which box a
/// failure belonged to.
#[tokio::test]
async fn a_decision_writes_one_row_per_device() {
    let dir = tempfile::tempdir().unwrap();
    let (journal, writer, path) = open(&dir);
    journal.decision(
        at(),
        ControlPath::Objective,
        &ControlDecision::test_sample(),
        &engine_state(),
        &[
            outcome("battery-a", Applied::Ok, None),
            outcome("battery-b", Applied::Error, Some("timed out")),
        ],
    );
    let conn = drain(journal, writer, &path).await;

    let mut stmt = conn
        .prepare("SELECT device, outcome, error FROM decisions ORDER BY device")
        .unwrap();
    let rows: Vec<(String, String, Option<String>)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();

    assert_eq!(
        rows,
        vec![
            ("battery-a".to_string(), "ok".to_string(), None),
            (
                "battery-b".to_string(),
                "error".to_string(),
                Some("timed out".to_string())
            ),
        ]
    );

    // Pinned alongside `"failsafe"` below, so both vocabulary strings the
    // `kind` column can hold are guarded rather than just one.
    assert_eq!(
        conn.query_row("SELECT DISTINCT kind FROM decisions", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "decision"
    );
}

/// A decision that commanded nothing is still a decision. This is reachable
/// — an empty world makes the failsafe produce no directives — and losing
/// it would hide exactly the outage worth investigating.
#[tokio::test]
async fn a_decision_that_commanded_nothing_still_gets_a_row() {
    let dir = tempfile::tempdir().unwrap();
    let (journal, writer, path) = open(&dir);
    journal.decision(
        at(),
        ControlPath::Failsafe,
        &ControlDecision::test_sample(),
        &engine_state(),
        &[],
    );
    let conn = drain(journal, writer, &path).await;

    let (kind, device): (String, Option<String>) = conn
        .query_row("SELECT kind, device FROM decisions", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(kind, "failsafe");
    assert_eq!(device, None);
}

/// A decision row seeds a replay: write a row, read `state_json` back out of
/// SQLite, and `restore` a real `Engine` from it. `engine.rs` alone proved
/// `EngineState` round-trips through JSON, and this module alone proved two
/// columns persisted; `mqtt_timed_out` fell between the two until this composed
/// them.
#[tokio::test]
async fn a_decision_row_restores_a_working_engine() {
    let dir = tempfile::tempdir().unwrap();
    let (journal, writer, path) = open(&dir);

    // Latched, so the field is not its default.
    let state = EngineState {
        mqtt_timed_out: true,
        ..engine_state()
    };
    journal.decision(
        at(),
        ControlPath::Failsafe,
        &ControlDecision::test_sample(),
        &state,
        &[outcome("SN123", Applied::Ok, None)],
    );
    let conn = drain(journal, writer, &path).await;

    let (state_json, pre_net): (String, f64) = conn
        .query_row(
            "SELECT state_json, pre_battery_net_w FROM decisions",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();

    let recovered: EngineState = serde_json::from_str(&state_json).unwrap();
    assert_eq!(recovered, state, "the row must carry the whole snapshot");

    let mut engine = Engine::new(
        Controller::test_default(NOW_MS, 255),
        World::new(),
        std::time::Duration::from_secs(120),
    );
    engine.restore(recovered);
    assert_eq!(
        engine.state(),
        state,
        "an engine restored from the row is the engine"
    );

    // 150.5 grid + (-300) battery: what the house drew without the battery.
    assert_eq!(pre_net, -149.5);
}

/// A row reads back as the types that wrote it. `ControlDecision`, `Outcome`
/// and `Applied` being `Serialize`-only would let `payload_json` and `outcome`
/// be written and never parsed, which the replay tool this journal feeds
/// cannot work against, and the format is append-only.
#[tokio::test]
async fn a_decision_row_reads_back_as_the_types_that_wrote_it() {
    let dir = tempfile::tempdir().unwrap();
    let (journal, writer, path) = open(&dir);
    journal.decision(
        at(),
        ControlPath::Objective,
        &ControlDecision::test_sample(),
        &engine_state(),
        &[outcome("SN123", Applied::Error, Some("boom"))],
    );
    let conn = drain(journal, writer, &path).await;

    let (payload, applied): (String, String) = conn
        .query_row("SELECT payload_json, outcome FROM decisions", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();

    assert_eq!(
        serde_json::from_str::<ControlDecision>(&payload).unwrap(),
        ControlDecision::test_sample()
    );
    assert_eq!(
        serde_json::from_str::<Applied>(&format!("\"{applied}\"")).unwrap(),
        Applied::Error
    );
}

/// Every row is stamped with the session that wrote it, so `config_json`
/// joins to the rows it actually governed instead of being matched up by
/// timestamp after the fact.
#[tokio::test]
async fn rows_are_attributed_to_their_session() {
    let dir = tempfile::tempdir().unwrap();
    let (journal, writer, path) = open(&dir);
    journal.raw("shelly", r#"{"ok":true}"#);
    journal.decision(
        at(),
        ControlPath::Objective,
        &ControlDecision::test_sample(),
        &engine_state(),
        &[],
    );
    let conn = drain(journal, writer, &path).await;

    let session: i64 = conn
        .query_row("SELECT id FROM sessions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM events WHERE session_id IS NOT NULL"
        ),
        1
    );
    assert_eq!(
        conn.query_row("SELECT session_id FROM events", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        session
    );
    assert_eq!(
        conn.query_row("SELECT session_id FROM decisions", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        session
    );
}

/// Retention is the only thing bounding this file. Deleting by date rather
/// than by count is what makes the window mean a window.
#[tokio::test]
async fn prune_deletes_beyond_the_retention_window() {
    let dir = tempfile::tempdir().unwrap();
    let (journal, writer, path) = open(&dir);
    journal.raw("shelly", r#"{"old":true}"#);
    journal.decision(
        at(),
        ControlPath::Objective,
        &ControlDecision::test_sample(),
        &engine_state(),
        &[outcome("SN123", Applied::Ok, None)],
    );
    let conn = drain(journal, writer, &path).await;
    assert_eq!(count(&conn, "SELECT count(*) FROM decisions"), 1);

    backdate(&conn, 100);
    // Pruning on behalf of some *other* session, so the exemption below is
    // not what is being measured here.
    prune(&conn, days(90), 999);
    // All three tables, not just `events`: a prune that forgot `decisions`
    // — the table holding the large rows, the one retention exists to bound
    // — used to pass this test.
    assert_eq!(count(&conn, "SELECT count(*) FROM events"), 0);
    assert_eq!(count(&conn, "SELECT count(*) FROM decisions"), 0);
    assert_eq!(count(&conn, "SELECT count(*) FROM sessions"), 0);
}

/// A session row is dated at process start while its events/decisions are
/// dated individually, so a long-uptime daemon would else delete the row
/// describing itself and keep writing rows pointing at nothing — which the
/// replay tool discovered as `QueryReturnedNoRows` on every long-running export.
#[tokio::test]
async fn prune_spares_the_running_sessions_own_row() {
    let dir = tempfile::tempdir().unwrap();
    let (journal, writer, path) = open(&dir);
    journal.raw("shelly", r#"{"old":true}"#);
    let conn = drain(journal, writer, &path).await;

    let session: i64 = conn
        .query_row("SELECT id FROM sessions", [], |r| r.get(0))
        .unwrap();
    backdate(&conn, 100);
    prune(&conn, days(90), session);

    assert_eq!(count(&conn, "SELECT count(*) FROM events"), 0);
    assert_eq!(
        count(&conn, "SELECT count(*) FROM sessions"),
        1,
        "the running session deleted the row describing itself"
    );
}

/// The control loop must never wait on the journal: a queue that cannot be
/// drained drops and counts rather than blocks. Proven by never starting a
/// writer — the receiver is dropped on the spot, so every `send` fails
/// exactly as a wedged writer would, and the calls still return.
#[tokio::test]
async fn a_full_queue_drops_records_instead_of_blocking() {
    let (tx, rx) = mpsc::channel(1);
    drop(rx);
    let journal = Journal {
        tx: Some(tx),
        dropped: Arc::new(AtomicU64::new(0)),
    };

    for _ in 0..50 {
        journal.raw("shelly", r#"{"ok":true}"#);
    }
    journal.decision(
        at(),
        ControlPath::Objective,
        &ControlDecision::test_sample(),
        &engine_state(),
        &[],
    );

    assert_eq!(journal.dropped.load(Ordering::Relaxed), 51);
}

/// The complement: a row inside the window survives a prune. Without this,
/// a `DELETE FROM events` with a broken predicate would pass the test above.
#[tokio::test]
async fn prune_keeps_rows_inside_the_window() {
    let dir = tempfile::tempdir().unwrap();
    let (journal, writer, path) = open(&dir);
    journal.raw("shelly", r#"{"recent":true}"#);
    let conn = drain(journal, writer, &path).await;

    backdate(&conn, 10);
    prune(&conn, days(90), 1);
    assert_eq!(count(&conn, "SELECT count(*) FROM events"), 1);
}

/// An unusable path disables the journal rather than failing startup. The
/// disabled journal is still a `Journal` and still accepts records, so no
/// caller has to know which one it holds or add a conditional at a new call site.
#[test]
fn an_unusable_path_disables_the_journal_without_disabling_the_caller() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("not-a-dir");
    std::fs::write(&file, b"x").unwrap();

    // A directory component that is actually a file.
    let path = file.join("journal.db");
    let (journal, writer) = Journal::open(&path, days(90), "0.0.0-test", &serde_json::json!({}));
    assert!(writer.is_none(), "nothing to drain when disabled");

    // Every entry point still takes a record and returns.
    journal.raw("shelly", r#"{"ok":true}"#);
    journal.decision(
        at(),
        ControlPath::Objective,
        &ControlDecision::test_sample(),
        &engine_state(),
        &[],
    );
    assert_eq!(
        journal.dropped.load(Ordering::Relaxed),
        0,
        "a disabled journal discards, it does not count drops"
    );
}
