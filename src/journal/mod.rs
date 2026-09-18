use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::Utc;
use rusqlite::Connection;
use serde::Serialize;
use serde::de::IgnoredAny;
use tokio::sync::mpsc;

use crate::device::{ControlPath, Outcome};
use crate::engine::EngineState;
use crate::event::Event;
use crate::models::ControlDecision;
use crate::units::{RetentionDays, Timestamp};

pub mod read;

/// Backlog before the control loop starts dropping records — minutes at
/// roughly one meter reading a second. A full queue means the writer is stuck,
/// not slow, and waiting for it would be worse than losing the record.
const QUEUE_DEPTH: usize = 1024;

/// Bumped when table shapes change and checked on open: `CREATE TABLE IF NOT
/// EXISTS` would otherwise accept a file with stale columns and fail every
/// insert. Nothing migrates between versions.
const SCHEMA_VERSION: i64 = 2;

/// Append-only, queryable record of everything entering and leaving the controller.
/// Payloads
/// are stored pre-parse, since our own types would drop the fields a decode failure
/// needs.
/// Every failure degrades to "stop journalling", never "stop controlling": the control
/// loop
/// only sends to a bounded channel, and the (`!Sync`, blocking) connection lives on a
/// writer thread.
pub struct Journal {
    /// `None` when disabled (unusable path, database wouldn't open); kept
    /// inside the type so callers never branch on "no journal" themselves.
    tx: Option<mpsc::Sender<Record>>,
    dropped: Arc<AtomicU64>,
}

/// The writer's handle, returned alongside the journal rather than held inside
/// it. Awaiting it is how a caller waits for the queue to drain, and that only
/// works once every `Journal` — and so every sender — has been dropped.
pub type Writer = tokio::task::JoinHandle<()>;

/// One row, already serialized. Serialization happens on the calling side so a
/// malformed value costs the caller a `debug!` rather than killing the writer.
enum Record {
    Event {
        at: Timestamp,
        kind: &'static str,
        payload_json: String,
    },
    Decision(Box<DecisionRow>),
}

/// One actuated command, or one decision that commanded nothing.
struct DecisionRow {
    at: Timestamp,
    kind: &'static str,
    device: Option<String>,
    payload_json: String,
    /// The whole `EngineState` in one column: splitting it into `world_json` and
    /// `ctrl_state_json` silently dropped `mqtt_timed_out` (which gates whether a
    /// resuming meter reading announces `"operational"`), so a row taken
    /// mid-outage replayed *almost* right.
    state_json: String,
    command: Option<String>,
    outcome: Option<String>,
    error: Option<String>,
    pre_battery_net_w: Option<f64>,
}

impl Journal {
    /// Opens the journal and starts its writer. Always returns a usable `Journal`:
    /// a database that cannot be opened warns and yields a disabled one, with
    /// `Writer` as `None`. `session_config` is the decision-relevant config,
    /// recorded once so a replay knows what tuning produced these rows.
    pub fn open<T: Serialize>(
        path: &Path,
        retention: RetentionDays,
        version: &str,
        session_config: &T,
    ) -> (Self, Option<Writer>) {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            tracing::warn!("Journal disabled: cannot create {}: {e}", parent.display());
            return (Self::disabled(), None);
        }

        let conn = match Connection::open(path) {
            Ok(conn) => conn,
            Err(e) => {
                tracing::warn!("Journal disabled: cannot open {}: {e}", path.display());
                return (Self::disabled(), None);
            }
        };

        let config_json = serde_json::to_string(session_config).unwrap_or_else(|e| {
            tracing::warn!("Journal: cannot serialize session config: {e}");
            "null".to_string()
        });

        let prepared = match prepare(&conn, version, &config_json) {
            Ok(prepared) => prepared,
            Err(e) => {
                tracing::warn!("Journal disabled: cannot prepare {}: {e}", path.display());
                return (Self::disabled(), None);
            }
        };

        let (tx, rx) = mpsc::channel(QUEUE_DEPTH);
        let dropped = Arc::new(AtomicU64::new(0));

        let handle = tokio::task::spawn_blocking({
            let dropped = Arc::clone(&dropped);
            move || writer(conn, prepared, rx, retention, &dropped)
        });

        tracing::info!("Journal open at {}", path.display());
        (
            Self {
                tx: Some(tx),
                dropped,
            },
            Some(handle),
        )
    }

    /// A journal that accepts every record and keeps none.
    fn disabled() -> Self {
        Self {
            tx: None,
            dropped: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Record an engine event. Called *before* the event is stepped, so a crash
    /// mid-decision still leaves the input that caused it on record.
    pub fn event(&self, event: &Event) {
        match serde_json::to_string(event) {
            Ok(payload_json) => self.send(Record::Event {
                at: event.at(),
                kind: event.kind(),
                payload_json,
            }),
            Err(e) => tracing::debug!("Journal: cannot serialize {}: {e}", event.kind()),
        }
    }

    /// Record a payload exactly as it arrived, before anything tried to parse
    /// it. Anything that is not valid JSON is stored as a JSON string, so a
    /// truncated device response is captured rather than lost.
    pub fn raw(&self, kind: &'static str, payload: &str) {
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
            // The wall clock: captured upstream of the engine, before parsing, so
            // there is no `Clock` yet. `events.ts_ms` therefore mixes observed time
            // with write time by a few milliseconds; `seq` spans both tables and
            // is the strict order, where `events.id` only orders this one.
            at: Utc::now().into(),
            kind,
            payload_json,
        });
    }

    /// Records a decision and what it actually did. Called *after* actuation, so
    /// each outcome reflects whether that device's write landed. One row per
    /// commanded device (a table, not a log, for `WHERE device = ?`); a decision
    /// commanding nothing — reachable via the failsafe on an empty world — still gets a
    /// row with no device.
    pub fn decision(
        &self,
        at: Timestamp,
        path: ControlPath,
        decision: &ControlDecision,
        state: &EngineState,
        outcomes: &[Outcome],
    ) {
        let (Some(payload_json), Some(state_json)) =
            (json("decision", decision), json("engine state", state))
        else {
            return;
        };

        // What the house would have drawn without the battery; kept as its own
        // column since every question about a decision's correctness starts here.
        // Non-finite becomes NULL deliberately: SQLite stores a bound NaN as NULL
        // regardless, which under `NOT NULL` failed the whole insert.
        let pre_battery_net_w = Some(state.world.underlying_grid().get()).filter(|w| w.is_finite());

        let row = |device, command, outcome, error| {
            Record::Decision(Box::new(DecisionRow {
                at,
                kind: path.journal_kind(),
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
        let Some(tx) = &self.tx else {
            return;
        };
        if tx.try_send(record).is_err() {
            crate::backpressure::tally(&self.dropped, |n| {
                tracing::warn!("Journal queue full — {n} records dropped so far")
            });
        }
    }
}

/// Serialize one value for a column, naming it if that fails. `warn!`, not
/// `debug!`: serializing our own types is a programming error that either never
/// happens or happens every time, so it can't flood the log the way a recurring
/// write failure could.
fn json<T: Serialize>(what: &str, value: &T) -> Option<String> {
    match serde_json::to_string(value) {
        Ok(json) => Some(json),
        Err(e) => {
            tracing::warn!("Journal: cannot serialize {what}: {e}");
            None
        }
    }
}

/// Opens a database, checks it is one we can write, and starts a session in it.
/// Returns the session id stamped onto every row, and the next `seq` to hand
/// out. `auto_vacuum` must be set before the first table exists; a database
/// from an older build simply keeps its old setting.
fn prepare(conn: &Connection, version: &str, config_json: &str) -> rusqlite::Result<Prepared> {
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

    // Before creating anything: a file stamped with a different schema has columns
    // this build does not know about, or lacks ones it writes. `CREATE TABLE IF NOT
    // EXISTS` would otherwise accept it silently and every insert then fail against
    // the missing column; refusing here disables the journal once, with the reason.
    let stamped: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if stamped != 0 && stamped != SCHEMA_VERSION {
        return Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_MISMATCH),
            Some(format!(
                "database is schema version {stamped}, this build writes {SCHEMA_VERSION}"
            )),
        ));
    }

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
             seq          INTEGER NOT NULL,
             ts_ms        INTEGER NOT NULL,
             kind         TEXT    NOT NULL,
             payload_json TEXT    NOT NULL
         );
         CREATE TABLE IF NOT EXISTS decisions (
             id                INTEGER PRIMARY KEY,
             session_id        INTEGER NOT NULL,
             seq               INTEGER NOT NULL,
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
         -- UNIQUE, because a duplicate `seq` silently destroys the ordering: two
         -- processes opening the same journal would seed from the same `max(seq)` and
         -- hand out the same numbers, turning that misconfiguration into failed inserts.
         -- Per-table only, so an event and another process's decision can still share a number.
         CREATE UNIQUE INDEX IF NOT EXISTS events_seq       ON events (seq);
         CREATE INDEX IF NOT EXISTS decisions_ts     ON decisions (ts_ms);
         CREATE INDEX IF NOT EXISTS decisions_device ON decisions (device);
         CREATE UNIQUE INDEX IF NOT EXISTS decisions_seq    ON decisions (seq);",
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
    let session_id = conn.last_insert_rowid();

    // `seq` continues across sessions rather than restarting, so a replay seeded
    // from a pre-restart decision can ask for "everything after that row" without
    // knowing which session either side came from. Gaps left by a prune are fine;
    // only the ordering is load-bearing.
    let next_seq: i64 = conn
        .query_row(
            "SELECT max(seq) FROM (SELECT max(seq) AS seq FROM events
                               UNION ALL
                               SELECT max(seq) AS seq FROM decisions)",
            [],
            |row| row.get::<_, Option<i64>>(0),
        )?
        .unwrap_or(0)
        + 1;

    Ok(Prepared {
        session_id,
        next_seq,
    })
}

struct Prepared {
    session_id: i64,
    next_seq: i64,
}

/// Owns the connection for the life of the process. Ends when every `Journal`
/// handle is dropped, which in practice means the process is going down.
fn writer(
    conn: Connection,
    prepared: Prepared,
    mut rx: mpsc::Receiver<Record>,
    retention: RetentionDays,
    dropped: &AtomicU64,
) {
    let Prepared {
        session_id,
        mut next_seq,
    } = prepared;
    let mut last_prune = Utc::now().date_naive();
    prune(&conn, retention, session_id);

    // Counted, not just logged. A writer failing *every* insert drains the
    // queue faster than a healthy one, so the queue never fills and the dropped
    // counter never moves — the failure that most needs announcing was the one
    // that announced itself least.
    let mut failed: u64 = 0;

    while let Some(record) = rx.blocking_recv() {
        // Assigned by the one writing thread, in channel order, so `seq`
        // orders both tables against each other. Consumed whether or not the
        // insert lands: a failed write leaves a gap, never a repeated number.
        let seq = next_seq;
        next_seq += 1;

        if let Err(e) = write(&conn, session_id, seq, &record) {
            failed += 1;
            // First one always, then powers of two: a full disk fails every
            // insert, so a line per record floods — and silence would make
            // journalling stop with logs identical to a healthy run.
            if failed.is_power_of_two() {
                tracing::warn!("Journal: write failed ({failed} so far): {e}");
            }
        }

        // Once a day, on whichever record happens to cross midnight.
        let today = Utc::now().date_naive();
        if today != last_prune {
            last_prune = today;
            prune(&conn, retention, session_id);
        }
    }

    let dropped = dropped.load(Ordering::Relaxed);
    if dropped > 0 || failed > 0 {
        tracing::warn!("Journal closing — {dropped} records dropped, {failed} writes failed");
    }
}

fn write(conn: &Connection, session_id: i64, seq: i64, record: &Record) -> rusqlite::Result<()> {
    match record {
        Record::Event {
            at,
            kind,
            payload_json,
        } => conn.execute(
            "INSERT INTO events (session_id, seq, ts_ms, kind, payload_json)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            (session_id, seq, at.as_millis(), kind, payload_json),
        )?,
        Record::Decision(row) => conn.execute(
            "INSERT INTO decisions
                 (session_id, seq, ts_ms, device, kind, payload_json, state_json,
                  command, outcome, error, pre_battery_net_w)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            (
                session_id,
                seq,
                row.at.as_millis(),
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
fn prune(conn: &Connection, retention: RetentionDays, keep_session: i64) {
    let cutoff = retention.cutoff(Utc::now()).as_millis();
    let mut removed = 0usize;
    // `sessions` bounds restart rows too (differently named time column, hence the
    // pair).
    // `continue`, not `return`: a failed table must not skip `decisions` — the one
    // retention
    // exists to bound — or the vacuum after. The running session is spared: its row
    // dates at
    // process start while its rows date individually; a long-lived daemon would else
    // delete the row describing itself.
    for (table, column, spare_running) in [
        ("events", "ts_ms", false),
        ("decisions", "ts_ms", false),
        ("sessions", "started_ms", true),
    ] {
        // Bound to match the statement. Handing two parameters to a one-
        // parameter `DELETE` is an error rusqlite raises and this loop's
        // `warn!` swallows, which is a quiet way to stop pruning entirely.
        let (extra, params) = if spare_running {
            (" AND id != ?2", vec![cutoff, keep_session])
        } else {
            ("", vec![cutoff])
        };
        match conn.execute(
            &format!("DELETE FROM {table} WHERE {column} < ?1{extra}"),
            rusqlite::params_from_iter(params),
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

/// Helpers every test that writes a journal needs. At module scope because the
/// reader's tests and the replay tests write journals too.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    /// Open a journal in a temp dir. The caller drops it to end the writer.
    pub(crate) fn open<T: Serialize>(
        dir: &tempfile::TempDir,
        config: &T,
    ) -> (Journal, Writer, std::path::PathBuf) {
        let path = dir.path().join("journal.db");
        let (journal, writer) = Journal::open(&path, days(3650), "0.0.0-test", config);
        let writer = writer.expect("journal opens in a temp dir");
        (journal, writer, path)
    }

    /// Close the sender and *wait for the writer to finish*, then reopen for
    /// reading. Sleeping instead would make every test here a race that happens
    /// to pass on a fast machine.
    pub(crate) async fn drain(journal: Journal, writer: Writer, path: &Path) -> Connection {
        drop(journal);
        writer.await.expect("writer panicked");
        Connection::open(path).unwrap()
    }

    /// Drop the journal and wait, without reopening — for callers that will
    /// read through `read_range` rather than a raw connection.
    pub(crate) async fn close(journal: Journal, writer: Writer) {
        drop(journal);
        writer.await.expect("writer panicked");
    }

    pub(crate) fn days(n: i64) -> RetentionDays {
        RetentionDays::new(n).unwrap()
    }

    pub(crate) fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    /// Everything `main.rs`'s loop does to the journal, minus the I/O: journal the
    /// event,
    /// step, then journal the decision with the state *after* the step — the order
    /// `seq`
    /// alignment depends on. Outcomes come from `registry::actuate` against a recording
    /// double so `command` is filled by production code, and `raw_after` injects a
    /// pre-parse capture after the nth event, mirroring the subscriber's cross-task
    /// write that `seq` exists to survive.
    pub(crate) async fn record_with(
        path: &Path,
        events: &[Event],
        raw_after: Option<usize>,
    ) -> crate::config::SessionConfig {
        use crate::config::SessionConfig;
        use crate::controller::Controller;
        use crate::device::RecordingBattery;
        use crate::engine::Engine;
        use crate::fixtures::journey;
        use crate::registry::{self, Battery, Devices};
        use crate::world::World;

        let config = SessionConfig::test_default();
        let (j, writer) = Journal::open(path, days(3650), "0.0.0-test", &config);
        let writer = writer.expect("journal opens in a temp dir");
        let mut engine = Engine::new(
            Controller::from_session(&config, &journey::clock_at(0)),
            World::new(),
            std::time::Duration::from_secs(config.mqtt_timeout_secs),
        );
        let devices = Devices::new([Battery::Recording(RecordingBattery::new(
            journey::BATTERY_ID,
        ))]);

        for (i, event) in events.iter().enumerate() {
            j.event(event);
            let step = engine.step(event);

            if raw_after == Some(i) {
                j.raw("shelly", r#"{"total_act_power":0}"#);
            }

            if let Some(decision) = step.decision {
                let outcomes =
                    registry::actuate(&devices, &step.directives, ControlPath::Objective).await;
                j.decision(
                    event.at(),
                    ControlPath::Objective,
                    &decision,
                    &engine.state(),
                    &outcomes,
                );
            }
        }
        close(j, writer).await;
        config
    }

    pub(crate) async fn record(path: &Path, events: &[Event]) -> crate::config::SessionConfig {
        record_with(path, events, None).await
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
#[path = "mod_tests.rs"]
mod tests;
