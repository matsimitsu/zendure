use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::Utc;
use rusqlite::Connection;
use serde::Serialize;
use serde::de::IgnoredAny;
use tokio::sync::mpsc;

use crate::device::Outcome;
use crate::engine::EngineState;
use crate::event::Event;
use crate::models::ControlDecision;
use crate::units::RetentionDays;

/// How many records may be in flight before the control loop starts dropping
/// them. At roughly one meter reading a second plus a poll and a decision, this
/// is minutes of backlog — if it ever fills, the writer is not slow, it is
/// stuck, and waiting for it would be worse than losing the record.
const QUEUE_DEPTH: usize = 1024;

/// Bumped whenever the table shapes change. Nothing migrates on it yet; it
/// exists so that a future change *can*, instead of silently inserting against
/// a database whose columns predate it.
const SCHEMA_VERSION: i64 = 1;

/// Append-only record of everything entering and leaving the controller.
///
/// Supersedes the NDJSON raw capture. Same contract, different storage: the
/// reason for the change is that reconstructing an incident means asking
/// questions across time ("every decision in the ten minutes before the mode
/// started flapping"), and that is a query, not a grep.
///
/// Two things are deliberately preserved from the NDJSON version, because they
/// are what made it useful:
///
/// - **Pre-parse payloads are stored as received.** A meter reading or device
///   response we failed to decode is precisely the one worth having, and our own
///   types would discard exactly the undocumented fields that explain it.
/// - **Every failure degrades to "stop journalling", never to "stop
///   controlling".** An unusable database disables the journal and the
///   controller starts normally.
///
/// The control loop never touches SQLite. It hands a record to a bounded
/// channel and returns; a writer thread owns the connection. `rusqlite`'s
/// `Connection` is `!Sync`, so single ownership is not a style choice — and the
/// writes are blocking I/O with no business on a runtime worker. If the channel
/// is full the record is dropped and counted, because the decision path waiting
/// on a logger is the one failure mode this design exists to rule out.
pub struct Journal {
    tx: mpsc::Sender<Record>,
    dropped: Arc<AtomicU64>,
    /// Only tests need to know when the writer has finished. Production never
    /// joins it: the process exits and SQLite's WAL is already consistent.
    #[cfg(test)]
    writer: Option<tokio::task::JoinHandle<()>>,
}

/// One row, already serialized. Serialization happens on the calling side so a
/// malformed value costs the caller a `debug!` rather than killing the writer.
enum Record {
    Event {
        ts_ms: i64,
        kind: String,
        payload_json: String,
    },
    Decision(Box<DecisionRow>),
}

/// One actuated command, or one decision that commanded nothing.
struct DecisionRow {
    ts_ms: i64,
    kind: &'static str,
    device: Option<String>,
    payload_json: String,
    /// The whole `EngineState`, not its parts. Splitting it into `world_json`
    /// and `ctrl_state_json` silently dropped `mqtt_timed_out`, which decides
    /// whether a resuming meter reading announces `"operational"` — so a row
    /// taken mid-outage replayed *almost* right. One column cannot lose a field
    /// the struct later gains.
    state_json: String,
    command: Option<String>,
    outcome: Option<String>,
    error: Option<String>,
    pre_battery_net_w: Option<f64>,
}

/// Which path produced a decision. Recorded so the failsafe's re-asserted idles
/// can be told from the objective's own decisions without inferring it from the
/// reason string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionKind {
    Decision,
    Failsafe,
}

impl DecisionKind {
    fn as_str(self) -> &'static str {
        match self {
            DecisionKind::Decision => "decision",
            DecisionKind::Failsafe => "failsafe",
        }
    }
}

impl Journal {
    /// Open the journal and start its writer. Returns `None` (with a warning)
    /// if the database cannot be opened or prepared, so the caller carries on
    /// without a journal rather than failing to start.
    ///
    /// `session_config` is the decision-relevant configuration, recorded once so
    /// a replay knows what tuning produced these rows. It is not the whole
    /// `Config`: connection settings are not decision inputs and do not belong
    /// in a fixture.
    pub fn open<T: Serialize>(
        path: &Path,
        retention: RetentionDays,
        version: &str,
        session_config: &T,
    ) -> Option<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            tracing::warn!("Journal disabled: cannot create {}: {e}", parent.display());
            return None;
        }

        let conn = match Connection::open(path) {
            Ok(conn) => conn,
            Err(e) => {
                tracing::warn!("Journal disabled: cannot open {}: {e}", path.display());
                return None;
            }
        };

        let config_json = serde_json::to_string(session_config).unwrap_or_else(|e| {
            tracing::warn!("Journal: cannot serialize session config: {e}");
            "null".to_string()
        });

        let session_id = match prepare(&conn, version, &config_json) {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!("Journal disabled: cannot prepare {}: {e}", path.display());
                return None;
            }
        };

        let (tx, rx) = mpsc::channel(QUEUE_DEPTH);
        let dropped = Arc::new(AtomicU64::new(0));

        let handle = tokio::task::spawn_blocking({
            let dropped = Arc::clone(&dropped);
            move || writer(conn, session_id, rx, retention, &dropped)
        });
        #[cfg(not(test))]
        drop(handle);

        tracing::info!("Journal open at {}", path.display());
        Some(Self {
            tx,
            dropped,
            #[cfg(test)]
            writer: Some(handle),
        })
    }

    /// Record an engine event. Called *before* the event is stepped, so a crash
    /// mid-decision still leaves the input that caused it on record.
    pub fn event(&self, event: &Event) {
        match serde_json::to_string(event) {
            Ok(payload_json) => self.send(Record::Event {
                ts_ms: event.at().as_millis(),
                kind: event.kind().to_string(),
                payload_json,
            }),
            Err(e) => tracing::debug!("Journal: cannot serialize {}: {e}", event.kind()),
        }
    }

    /// Record a payload exactly as it arrived, before anything tried to parse
    /// it. Anything that is not valid JSON is stored as a JSON string, so a
    /// truncated device response is captured rather than lost.
    pub fn raw(&self, kind: &str, payload: &str) {
        // Validates the syntax without building a `Value` or allocating.
        let payload_json = match serde_json::from_str::<IgnoredAny>(payload) {
            Ok(_) => payload.to_string(),
            Err(_) => match serde_json::to_string(payload) {
                Ok(quoted) => quoted,
                Err(e) => {
                    tracing::debug!("Journal: cannot encode {kind}: {e}");
                    return;
                }
            },
        };
        self.send(Record::Event {
            ts_ms: Utc::now().timestamp_millis(),
            kind: kind.to_string(),
            payload_json,
        });
    }

    /// Record a decision and what it actually did. Called *after* actuation, so
    /// each outcome reflects whether that device's write landed.
    ///
    /// One row per commanded device, which repeats the decision across a
    /// multi-device fleet — deliberate, because the point of a table over a log
    /// is `WHERE device = ?`, and there is one battery today. A decision that
    /// commanded nothing still gets a row, with no device: that is reachable
    /// (an empty world makes the failsafe emit no directives) and is exactly
    /// the case you would go looking for.
    pub fn decision(
        &self,
        ts_ms: i64,
        kind: DecisionKind,
        decision: &ControlDecision,
        state: &EngineState,
        outcomes: &[Outcome],
    ) {
        let (Some(payload_json), Some(state_json)) =
            (json("decision", decision), json("engine state", state))
        else {
            return;
        };

        // The grid figure with the battery's own flow removed — what the house
        // would have been drawing without it. Derivable from `state_json`, kept
        // as a column because every question about whether a decision was right
        // starts by asking for it. Non-finite becomes NULL deliberately: SQLite
        // stores a bound NaN as NULL regardless, which under `NOT NULL` failed
        // the whole insert and took the decision with it.
        let pre_battery_net_w = Some(state.world.underlying_grid().get()).filter(|w| w.is_finite());

        let row = |device, command, outcome, error| {
            Record::Decision(Box::new(DecisionRow {
                ts_ms,
                kind: kind.as_str(),
                device,
                payload_json: payload_json.clone(),
                state_json: state_json.clone(),
                command,
                outcome,
                error,
                pre_battery_net_w,
            }))
        };

        if outcomes.is_empty() {
            self.send(row(None, None, None, None));
            return;
        }
        for outcome in outcomes {
            self.send(row(
                Some(outcome.device.to_string()),
                Some(outcome.command.clone()),
                Some(outcome.applied.as_str().to_string()),
                outcome.error.clone(),
            ));
        }
    }

    /// Non-blocking by construction. A full queue means the writer is stuck, and
    /// the control loop is not the place to find that out by waiting.
    fn send(&self, record: Record) {
        if self.tx.try_send(record).is_err() {
            let n = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            // Every power of two, so a persistent stall is loud without a
            // wedged writer flooding the log at one line per reading.
            if n.is_power_of_two() {
                tracing::warn!("Journal queue full — {n} records dropped so far");
            }
        }
    }
}

/// Serialize one value for a column, naming it if that fails.
///
/// `warn!`, not `debug!`: serializing our own types is a programming error that
/// either never happens or happens every time, so it cannot flood the log the
/// way a recurring write failure could — and a permanently unserializable
/// journal going quiet is the outcome this whole module exists to avoid.
fn json<T: Serialize>(what: &str, value: &T) -> Option<String> {
    match serde_json::to_string(value) {
        Ok(json) => Some(json),
        Err(e) => {
            tracing::warn!("Journal: cannot serialize {what}: {e}");
            None
        }
    }
}

/// `auto_vacuum` has to be set before the first table exists, so a database
/// created by an older build keeps its old setting. Harmless: without it the
/// file holds its high-water mark instead of shrinking after a prune.
fn prepare(conn: &Connection, version: &str, config_json: &str) -> rusqlite::Result<i64> {
    // First, before anything writes a page: `auto_vacuum` can only be set while
    // the database is still empty, and switching to WAL is itself a write. Set
    // after, it silently reports success and leaves the setting at NONE.
    conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    // NORMAL, not FULL: the writer must not fsync per row. A power cut can lose
    // the last transactions; WAL still recovers a consistent database, and the
    // alternative is SD-card fsync latency on a box that writes every second.
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    // WAL lets readers run without blocking us, but anything taking a write
    // lock — an operator's `VACUUM`, a second process — would otherwise make
    // the very next insert fail instantly with SQLITE_BUSY, since the default
    // timeout is zero.
    conn.busy_timeout(std::time::Duration::from_secs(5))?;

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS sessions (
             id          INTEGER PRIMARY KEY,
             started_ms  INTEGER NOT NULL,
             version     TEXT    NOT NULL,
             config_json TEXT    NOT NULL
         );
         CREATE TABLE IF NOT EXISTS events (
             id           INTEGER PRIMARY KEY,
             session_id   INTEGER NOT NULL,
             ts_ms        INTEGER NOT NULL,
             kind         TEXT    NOT NULL,
             payload_json TEXT    NOT NULL
         );
         CREATE TABLE IF NOT EXISTS decisions (
             id                INTEGER PRIMARY KEY,
             session_id        INTEGER NOT NULL,
             ts_ms             INTEGER NOT NULL,
             device            TEXT,
             kind              TEXT    NOT NULL,
             payload_json      TEXT    NOT NULL,
             state_json        TEXT    NOT NULL,
             command           TEXT,
             outcome           TEXT,
             error             TEXT,
             pre_battery_net_w REAL
         );
         CREATE INDEX IF NOT EXISTS events_ts        ON events (ts_ms);
         CREATE INDEX IF NOT EXISTS events_kind      ON events (kind);
         CREATE INDEX IF NOT EXISTS decisions_ts     ON decisions (ts_ms);
         CREATE INDEX IF NOT EXISTS decisions_device ON decisions (device);",
    )?;

    // Stamped so a future column addition has somewhere to branch on. Without
    // it, `CREATE TABLE IF NOT EXISTS` silently accepts a database written by an
    // older build and then fails every insert against the missing column.
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;

    // One row per process. A replay seeded from before the first decision has
    // this to fall back on, and it dates every restart. Its id is stamped onto
    // every row written afterwards: the association is known here and would
    // otherwise have to be reconstructed by timestamp later.
    conn.execute(
        "INSERT INTO sessions (started_ms, version, config_json) VALUES (?1, ?2, ?3)",
        (Utc::now().timestamp_millis(), version, config_json),
    )?;
    Ok(conn.last_insert_rowid())
}

/// Owns the connection for the life of the process. Ends when every `Journal`
/// handle is dropped, which in practice means the process is going down.
fn writer(
    conn: Connection,
    session_id: i64,
    mut rx: mpsc::Receiver<Record>,
    retention: RetentionDays,
    dropped: &AtomicU64,
) {
    let mut last_prune = Utc::now().date_naive();
    prune(&conn, retention);

    // Counted, not just logged. A writer failing *every* insert drains the
    // queue faster than a healthy one, so the queue never fills and the dropped
    // counter never moves — the failure that most needs announcing was the one
    // that announced itself least.
    let mut failed: u64 = 0;

    while let Some(record) = rx.blocking_recv() {
        if let Err(e) = write(&conn, session_id, &record) {
            failed += 1;
            // First one always, then powers of two. A full disk or a read-only
            // SD card fails every insert, so a line per record would flood; a
            // line per record *silently dropped* was the alternative, and that
            // is how journalling stopped for the life of a process with logs
            // identical to a healthy run.
            if failed.is_power_of_two() {
                tracing::warn!("Journal: write failed ({failed} so far): {e}");
            }
        }

        // Once a day, on whichever record happens to cross midnight.
        let today = Utc::now().date_naive();
        if today != last_prune {
            last_prune = today;
            prune(&conn, retention);
        }
    }

    let dropped = dropped.load(Ordering::Relaxed);
    if dropped > 0 || failed > 0 {
        tracing::warn!("Journal closing — {dropped} records dropped, {failed} writes failed");
    }
}

fn write(conn: &Connection, session_id: i64, record: &Record) -> rusqlite::Result<()> {
    match record {
        Record::Event {
            ts_ms,
            kind,
            payload_json,
        } => conn.execute(
            "INSERT INTO events (session_id, ts_ms, kind, payload_json) VALUES (?1, ?2, ?3, ?4)",
            (session_id, ts_ms, kind, payload_json),
        )?,
        Record::Decision(row) => conn.execute(
            "INSERT INTO decisions
                 (session_id, ts_ms, device, kind, payload_json, state_json,
                  command, outcome, error, pre_battery_net_w)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            (
                session_id,
                row.ts_ms,
                &row.device,
                row.kind,
                &row.payload_json,
                &row.state_json,
                &row.command,
                &row.outcome,
                &row.error,
                row.pre_battery_net_w,
            ),
        )?,
    };
    Ok(())
}

/// The ring buffer. Retention is the only thing keeping this file bounded, since
/// nothing else ever deletes a row.
fn prune(conn: &Connection, retention: RetentionDays) {
    let cutoff = retention.cutoff(Utc::now()).as_millis();
    let mut removed = 0usize;
    // `sessions` is in the list because the doc below claims retention is the
    // only thing bounding this file, and it was not: one row per restart
    // accumulated forever. Its time column is named differently, hence the pair.
    //
    // `continue`, not `return`: failing on the first table used to skip
    // `decisions` — the larger one, the one retention exists to bound — and the
    // vacuum with it, until the next midnight.
    for (table, column) in [
        ("events", "ts_ms"),
        ("decisions", "ts_ms"),
        ("sessions", "started_ms"),
    ] {
        match conn.execute(
            &format!("DELETE FROM {table} WHERE {column} < ?1"),
            [cutoff],
        ) {
            Ok(n) => removed += n,
            Err(e) => tracing::warn!("Journal: cannot prune {table}: {e}"),
        }
    }
    if removed > 0 {
        tracing::info!("Journal: pruned {removed} rows beyond {retention} days");
        // Hands the freed pages back rather than leaving the file at its
        // high-water mark. Only does anything if the database was created with
        // `auto_vacuum=INCREMENTAL`.
        if let Err(e) = conn.execute_batch("PRAGMA incremental_vacuum") {
            tracing::debug!("Journal: incremental_vacuum failed: {e}");
        }
    }
}

/// Ages every row so `prune` can be exercised against a real date boundary
/// without a test waiting a day. `sessions` is keyed on `started_ms` rather than
/// `ts_ms`, which is the schema's one asymmetry.
#[cfg(test)]
fn backdate(conn: &Connection, days: i64) {
    let ts = (Utc::now() - chrono::Duration::days(days)).timestamp_millis();
    conn.execute("UPDATE events SET ts_ms = ?1", [ts]).unwrap();
    conn.execute("UPDATE decisions SET ts_ms = ?1", [ts])
        .unwrap();
    conn.execute("UPDATE sessions SET started_ms = ?1", [ts])
        .unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::battery::BatteryState;
    use crate::clock::Clock;
    use crate::controller::Controller;
    use crate::device::Applied;
    use crate::engine::Engine;
    use crate::models::ControlMode;
    use crate::units::{BatteryPower, GridPower, PowerCap, Setpoint, Soc, SolarPower, Timestamp};
    use crate::world::{DeviceId, Measurement, MeterReading, World};
    use chrono::Weekday;

    const NOW_MS: i64 = 1_757_000_000_000;

    fn clock() -> Clock {
        Clock {
            now: Timestamp::from_millis(NOW_MS),
            hour: 19,
            day_ordinal: 255,
            weekday: Weekday::Wed,
        }
    }

    fn battery() -> BatteryState {
        BatteryState {
            soc: Soc::new(50),
            max_discharge_power: PowerCap::new(800),
            max_charge_power: PowerCap::new(2400),
            current_power: BatteryPower(-300),
            soc_calibrating: false,
            soc_limit_reached: false,
            fault: false,
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

    fn decision() -> ControlDecision {
        ControlDecision {
            mode: ControlMode::Discharge,
            power_watts: Setpoint::new(145),
            reason: "Grid demand".to_string(),
            grid_power: GridPower(150.5),
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

    /// Opens a journal in a temp dir and returns it with a connection for
    /// reading back. The journal is dropped by the caller to end the writer.
    fn open(dir: &tempfile::TempDir) -> (Journal, std::path::PathBuf) {
        let path = dir.path().join("journal.db");
        let journal = Journal::open(&path, days(90), "0.0.0-test", &serde_json::json!({"k": 1}))
            .expect("journal opens in a temp dir");
        (journal, path)
    }

    /// Closes the sender and *waits for the writer to finish*, then reopens the
    /// file for reading. Sleeping instead would make every test here a race
    /// that happens to pass on a fast machine.
    async fn drain(mut journal: Journal, path: &Path) -> Connection {
        let handle = journal.writer.take().expect("writer handle");
        drop(journal);
        handle.await.expect("writer panicked");
        Connection::open(path).unwrap()
    }

    fn days(n: i64) -> RetentionDays {
        RetentionDays::new(n).unwrap()
    }

    fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    #[tokio::test]
    async fn opening_records_a_session_row() {
        let dir = tempfile::tempdir().unwrap();
        let (journal, path) = open(&dir);
        let conn = drain(journal, &path).await;

        let (version, config): (String, String) = conn
            .query_row("SELECT version, config_json FROM sessions", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(version, "0.0.0-test");
        assert_eq!(config, r#"{"k":1}"#);
    }

    /// All three pragmas, pinned by value rather than trusted.
    ///
    /// `auto_vacuum` is the reason this test exists: it can only be set while
    /// the database is still empty, and `pragma_update` reports success either
    /// way. Setting it after `journal_mode=WAL` — which writes a page — leaves
    /// it silently at NONE, so the file would have grown forever while the code
    /// looked like it pruned. `synchronous` matters just as much in the other
    /// direction: FULL would put an SD-card fsync behind every record.
    /// Asserted against the connection `prepare` ran on, not a reopened one:
    /// `synchronous` is per-connection and is not stored in the file, so
    /// checking it anywhere else would pass or fail on the reader's default and
    /// say nothing about the writer's.
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
        let (journal, path) = open(&dir);
        journal.raw("shelly", r#"{"total_act_power":150.5,"unmodelled":"kept"}"#);
        let conn = drain(journal, &path).await;

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
        let (journal, path) = open(&dir);
        journal.raw("zendure_poll", r#"{"electricLevel": 4"#);
        journal.raw("shelly", r#"{"ok":true}"#);
        let conn = drain(journal, &path).await;

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
    /// payload reads back as the event that produced it — which is what step 8
    /// replays from.
    #[tokio::test]
    async fn events_round_trip_through_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let (journal, path) = open(&dir);
        let event = Event::Meter {
            at: clock(),
            grid: MeterReading::total_only(GridPower(150.5)),
            solar: SolarPower::new(200.0),
        };
        journal.event(&event);
        let conn = drain(journal, &path).await;

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
        let (journal, path) = open(&dir);
        journal.decision(
            NOW_MS,
            DecisionKind::Decision,
            &decision(),
            &engine_state(),
            &[
                outcome("battery-a", Applied::Ok, None),
                outcome("battery-b", Applied::Error, Some("timed out")),
            ],
        );
        let conn = drain(journal, &path).await;

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
        let (journal, path) = open(&dir);
        journal.decision(
            NOW_MS,
            DecisionKind::Failsafe,
            &decision(),
            &engine_state(),
            &[],
        );
        let conn = drain(journal, &path).await;

        let (kind, device): (String, Option<String>) = conn
            .query_row("SELECT kind, device FROM decisions", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(kind, "failsafe");
        assert_eq!(device, None);
    }

    /// **A decision row can seed a replay.** The claim three doc comments made
    /// while the code did not implement it.
    ///
    /// Goes all the way round rather than checking columns: write a row, read
    /// `state_json` back out of SQLite, and `restore` a real `Engine` from it.
    /// The gap this closes was invisible precisely because nobody tested the
    /// composition — `engine.rs` proved `EngineState` round-trips through JSON,
    /// this module proved two columns persisted, and `mqtt_timed_out` fell
    /// between them. Anything the snapshot gains from here is carried or this
    /// fails.
    #[tokio::test]
    async fn a_decision_row_restores_a_working_engine() {
        let dir = tempfile::tempdir().unwrap();
        let (journal, path) = open(&dir);

        // Latched, so the field that used to be dropped is not its default.
        let state = EngineState {
            mqtt_timed_out: true,
            ..engine_state()
        };
        journal.decision(
            NOW_MS,
            DecisionKind::Failsafe,
            &decision(),
            &state,
            &[outcome("SN123", Applied::Ok, None)],
        );
        let conn = drain(journal, &path).await;

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

    /// Every row is stamped with the session that wrote it, so `config_json`
    /// joins to the rows it actually governed instead of being matched up by
    /// timestamp after the fact.
    #[tokio::test]
    async fn rows_are_attributed_to_their_session() {
        let dir = tempfile::tempdir().unwrap();
        let (journal, path) = open(&dir);
        journal.raw("shelly", r#"{"ok":true}"#);
        journal.decision(
            NOW_MS,
            DecisionKind::Decision,
            &decision(),
            &engine_state(),
            &[],
        );
        let conn = drain(journal, &path).await;

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
        let (journal, path) = open(&dir);
        journal.raw("shelly", r#"{"old":true}"#);
        journal.decision(
            NOW_MS,
            DecisionKind::Decision,
            &decision(),
            &engine_state(),
            &[outcome("SN123", Applied::Ok, None)],
        );
        let conn = drain(journal, &path).await;
        assert_eq!(count(&conn, "SELECT count(*) FROM decisions"), 1);

        backdate(&conn, 100);
        prune(&conn, days(90));
        // All three tables, not just `events`: a prune that forgot `decisions`
        // — the table holding the large rows, the one retention exists to bound
        // — used to pass this test.
        assert_eq!(count(&conn, "SELECT count(*) FROM events"), 0);
        assert_eq!(count(&conn, "SELECT count(*) FROM decisions"), 0);
        assert_eq!(count(&conn, "SELECT count(*) FROM sessions"), 0);
    }

    /// **The property the whole architecture exists for**, and it had no test.
    ///
    /// The control loop must never wait on the journal, so a queue that cannot
    /// be drained has to drop and count rather than block. Proven by never
    /// starting a writer: the receiver is dropped on the spot, so every `send`
    /// fails exactly as a wedged writer would, and the calls still return.
    #[tokio::test]
    async fn a_full_queue_drops_records_instead_of_blocking() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let journal = Journal {
            tx,
            dropped: Arc::new(AtomicU64::new(0)),
            writer: None,
        };

        for _ in 0..50 {
            journal.raw("shelly", r#"{"ok":true}"#);
        }
        journal.decision(
            NOW_MS,
            DecisionKind::Decision,
            &decision(),
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
        let (journal, path) = open(&dir);
        journal.raw("shelly", r#"{"recent":true}"#);
        let conn = drain(journal, &path).await;

        backdate(&conn, 10);
        prune(&conn, days(90));
        assert_eq!(count(&conn, "SELECT count(*) FROM events"), 1);
    }

    /// An unusable path disables the journal rather than failing startup —
    /// the invariant carried over from the NDJSON capture.
    #[test]
    fn an_unusable_path_disables_the_journal() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-a-dir");
        std::fs::write(&file, b"x").unwrap();

        // A directory component that is actually a file.
        let path = file.join("journal.db");
        assert!(Journal::open(&path, days(90), "0.0.0-test", &serde_json::json!({})).is_none());
    }
}
