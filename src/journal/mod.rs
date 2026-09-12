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

/// How many records may be in flight before the control loop starts dropping
/// them. At roughly one meter reading a second plus a poll and a decision, this
/// is minutes of backlog — if it ever fills, the writer is not slow, it is
/// stuck, and waiting for it would be worse than losing the record.
const QUEUE_DEPTH: usize = 1024;

/// Bumped whenever the table shapes change, and *checked* on open: a database
/// stamped with anything else is refused rather than inserted against, because
/// `CREATE TABLE IF NOT EXISTS` accepts a file whose columns predate the build
/// and then fails every insert instead.
///
/// Version 2 added `seq`. Nothing migrates between versions — there is no
/// deployed v1 database to migrate, and inventing a migration path for a file
/// that only ever existed on a development machine would be pure ceremony.
const SCHEMA_VERSION: i64 = 2;

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
    /// `None` when the journal is disabled — an unusable path, a database that
    /// would not open. Kept *inside* the type rather than handing callers an
    /// `Option<Journal>`: every method here is `&self`, infallible and already
    /// swallows its own errors, so "there is no journal" is this module's
    /// business and not a conditional at seven call sites.
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

impl Journal {
    /// Open the journal and start its writer.
    ///
    /// Always returns a usable `Journal`. If the database cannot be opened or
    /// prepared it warns and returns a disabled one, so the caller carries on
    /// without a journal rather than failing to start — and without having to
    /// know which it got. The `Writer` is `None` in that case, since there is
    /// nothing to drain.
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
            // The wall clock, not an event's own: these are captured upstream
            // of the engine, before anything has parsed them, so there is no
            // `Clock` attached yet. `events.ts_ms` therefore mixes observed time
            // (engine events) with write time (`shelly`, `zendure_poll`); they
            // differ by the few milliseconds between receiving a payload and
            // folding it in. `seq` is the strict order if you need one, and it
            // spans both tables where `events.id` only orders this one.
            at: Utc::now().into(),
            kind,
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

        // The grid figure with the battery's own flow removed — what the house
        // would have been drawing without it. Derivable from `state_json`, kept
        // as a column because every question about whether a decision was right
        // starts by asking for it. Non-finite becomes NULL deliberately: SQLite
        // stores a bound NaN as NULL regardless, which under `NOT NULL` failed
        // the whole insert and took the decision with it.
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

/// Open a database, check it is one we can write, and start a session in it.
///
/// Returns the session id stamped onto every row, and the next `seq` to hand
/// out.
///
/// `auto_vacuum` has to be set before the first table exists, so a database
/// created by an older build keeps its old setting. Harmless: without it the
/// file holds its high-water mark instead of shrinking after a prune.
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

    // Before creating anything: a file stamped with a different schema has
    // columns this build does not know about, or lacks ones it writes. Left
    // alone, `CREATE TABLE IF NOT EXISTS` would accept it silently and every
    // insert would then fail against the missing column — which degrades to a
    // warning per power of two and a journal that records nothing. Refusing
    // here disables the journal once, with the reason.
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
         -- UNIQUE, because `seq` is an ordering and a duplicate silently
         -- destroys it. One writer per process cannot collide with itself, but
         -- two processes opening the same journal seed their counters from the
         -- same `max(seq)` and hand out the same numbers — a misconfiguration
         -- rather than a supported mode. This turns most of that into failed
         -- inserts, which degrade journalling and never control.
         --
         -- Partial, and deliberately so: the two indexes are per-table, so one
         -- process's event and another's decision can still share a number.
         -- Closing that needs a sequence table both writers take a row lock on,
         -- which is a transaction per record on the control path's behalf —
         -- far too much for a case systemd cannot produce.
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

    // `seq` continues across sessions rather than restarting, so it orders the
    // whole file and not just one process's rows. A replay seeded from a
    // decision written before a restart has to be able to ask for "everything
    // after that row" without also knowing which session each side came from.
    // Gaps left by a prune are fine; only the ordering is load-bearing.
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

/// What a freshly opened database hands the writer.
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
        // Assigned here, by the one thread that writes, in the order the
        // records left the channel. That is what makes `seq` an ordering across
        // both tables: a decision's rows are handed numbers strictly after the
        // event that produced them, even though the two live in separate tables
        // with independent row ids. Consumed whether or not the insert lands, so
        // a failed write leaves a gap rather than a repeated number.
        let seq = next_seq;
        next_seq += 1;

        if let Err(e) = write(&conn, session_id, seq, &record) {
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
    // `sessions` is in the list because the doc below claims retention is the
    // only thing bounding this file, and it was not: one row per restart
    // accumulated forever. Its time column is named differently, hence the pair.
    //
    // `continue`, not `return`: failing on the first table used to skip
    // `decisions` — the larger one, the one retention exists to bound — and the
    // vacuum with it, until the next midnight.
    // The running session is exempt from its own prune. A session row is dated
    // at *process start* while its events and decisions are dated individually,
    // so a daemon whose uptime exceeds the retention window — months, for an
    // unattended controller with a 30-day window — deleted the row describing
    // it and carried on writing rows that pointed at nothing. Nothing read
    // `sessions` back until the replay tool did, at which point every export of
    // a long-running process failed outright.
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

/// Helpers every test that writes a journal needs.
///
/// At module scope and `pub(crate)`, not inside this file's own `mod tests`,
/// because the reader's tests and the replay tests write journals too and were
/// otherwise reduced to re-inlining `Journal::open` plus the drop-and-await
/// dance — down to the same `expect("writer panicked")` string on both sides.
/// `backdate` below already set the precedent.
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

    /// Everything `main.rs`'s loop does to the journal, minus the I/O: journal
    /// the event, step, then journal the decision with the state *after* the
    /// step. That order is what `seq` alignment depends on, so a helper writing
    /// them any other way would be exercising a daemon that does not exist.
    ///
    /// The outcomes come from `device::actuate` against a recording double
    /// rather than being built here, so the `command` column is filled by the
    /// same code that fills it in production. Hand-building them made a
    /// recorded column and a replayed render two expressions of one local
    /// variable, which is a comparison that cannot fail.
    ///
    /// `raw_after` injects a pre-parse capture after the nth event — what the
    /// subscriber task does from another task in production, and the thing
    /// `seq` exists to survive.
    pub(crate) async fn record_with(
        path: &Path,
        events: &[Event],
        raw_after: Option<usize>,
    ) -> crate::config::SessionConfig {
        use crate::config::SessionConfig;
        use crate::controller::Controller;
        use crate::device::{RecordingBattery, actuate};
        use crate::engine::Engine;
        use crate::fixtures::journey;
        use crate::world::World;

        let config = SessionConfig::test_default();
        let (j, writer) = Journal::open(path, days(3650), "0.0.0-test", &config);
        let writer = writer.expect("journal opens in a temp dir");
        let mut engine = Engine::new(
            Controller::from_session(&config, &journey::clock_at(0)),
            World::new(),
            std::time::Duration::from_secs(config.mqtt_timeout_secs),
        );
        let battery = RecordingBattery::new(journey::BATTERY_ID);

        for (i, event) in events.iter().enumerate() {
            j.event(event);
            let step = engine.step(event);

            if raw_after == Some(i) {
                j.raw("shelly", r#"{"total_act_power":0}"#);
            }

            if let Some(decision) = step.decision {
                let outcomes = actuate(&battery, &step.directives, ControlPath::Objective).await;
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
mod tests {
    use super::testing::{count, days, drain};
    use super::*;
    use crate::battery::BatteryState;
    use crate::clock::Clock;
    use crate::controller::Controller;
    use crate::device::Applied;
    use crate::engine::Engine;
    use crate::models::ControlMode;
    use crate::units::{BatteryPower, GridPower, Setpoint, SolarPower, Timestamp};
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

    /// This file's journals carry a stand-in config; the reader's tests use a
    /// real `SessionConfig` because they read it back.
    fn open(dir: &tempfile::TempDir) -> (Journal, Writer, std::path::PathBuf) {
        testing::open(dir, &serde_json::json!({"k": 1}))
    }

    /// Every row in the file, both tables, in `seq` order.
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

    /// The property step 8's export depends on: a decision is ordered strictly
    /// after the event that produced it, across two tables whose row ids run
    /// independently. Without this, "replay everything after this snapshot" can
    /// only be asked in milliseconds, and two records sharing one millisecond
    /// make the boundary ambiguous.
    #[tokio::test]
    async fn seq_orders_events_and_decisions_against_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let (journal, writer, path) = open(&dir);

        journal.event(&Event::MqttTimeout { at: clock() });
        journal.decision(
            at(),
            ControlPath::Failsafe,
            &decision(),
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

    /// A database written by a different build has columns this one does not
    /// write, or lacks ones it does. `CREATE TABLE IF NOT EXISTS` accepts it
    /// silently and every insert then fails against a missing column — a
    /// warning per power of two and a journal that records nothing. Refusing
    /// once, with the reason, is the failure the `user_version` stamp was put
    /// there to make possible.
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
    /// payload reads back as the event that produced it — which is what step 8
    /// replays from.
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
            &decision(),
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
            &decision(),
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
        let (journal, writer, path) = open(&dir);

        // Latched, so the field that used to be dropped is not its default.
        let state = EngineState {
            mqtt_timed_out: true,
            ..engine_state()
        };
        journal.decision(
            at(),
            ControlPath::Failsafe,
            &decision(),
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

    /// A row reads back as the types that wrote it.
    ///
    /// `ControlDecision`, `Outcome` and `Applied` were `Serialize`-only, so
    /// `payload_json` and `outcome` could be written and never parsed — while a
    /// commit message claimed everything stored was reachable through them. The
    /// replay tool this journal exists to feed cannot work against write-only
    /// columns, and the format is append-only, so the rows being readable is a
    /// property of the rows, not of the tool that comes later.
    #[tokio::test]
    async fn a_decision_row_reads_back_as_the_types_that_wrote_it() {
        let dir = tempfile::tempdir().unwrap();
        let (journal, writer, path) = open(&dir);
        journal.decision(
            at(),
            ControlPath::Objective,
            &decision(),
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
            decision()
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
            &decision(),
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
            &decision(),
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

    /// A session row is dated at *process start* while its events and decisions
    /// are dated individually, so a daemon whose uptime exceeds the retention
    /// window would delete the row describing itself and go on writing rows
    /// pointing at nothing. Nothing read `sessions` back until the replay tool
    /// did, at which point every export of a long-running process failed
    /// outright with `QueryReturnedNoRows`.
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
            tx: Some(tx),
            dropped: Arc::new(AtomicU64::new(0)),
        };

        for _ in 0..50 {
            journal.raw("shelly", r#"{"ok":true}"#);
        }
        journal.decision(
            at(),
            ControlPath::Objective,
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
        let (journal, writer, path) = open(&dir);
        journal.raw("shelly", r#"{"recent":true}"#);
        let conn = drain(journal, writer, &path).await;

        backdate(&conn, 10);
        prune(&conn, days(90), 1);
        assert_eq!(count(&conn, "SELECT count(*) FROM events"), 1);
    }

    /// An unusable path disables the journal rather than failing startup —
    /// the invariant carried over from the NDJSON capture.
    ///
    /// The disabled journal is still a `Journal`, and still accepts records.
    /// That is the point: no caller has to know which one it holds, so there is
    /// no conditional to forget at a new call site.
    #[test]
    fn an_unusable_path_disables_the_journal_without_disabling_the_caller() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-a-dir");
        std::fs::write(&file, b"x").unwrap();

        // A directory component that is actually a file.
        let path = file.join("journal.db");
        let (journal, writer) =
            Journal::open(&path, days(90), "0.0.0-test", &serde_json::json!({}));
        assert!(writer.is_none(), "nothing to drain when disabled");

        // Every entry point still takes a record and returns.
        journal.raw("shelly", r#"{"ok":true}"#);
        journal.decision(
            at(),
            ControlPath::Objective,
            &decision(),
            &engine_state(),
            &[],
        );
        assert_eq!(
            journal.dropped.load(Ordering::Relaxed),
            0,
            "a disabled journal discards, it does not count drops"
        );
    }
}
