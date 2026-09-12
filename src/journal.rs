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
    world_json: String,
    ctrl_state_json: String,
    command: Option<String>,
    outcome: Option<String>,
    error: Option<String>,
    pre_battery_net_w: f64,
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

        if let Err(e) = prepare(&conn, version, &config_json) {
            tracing::warn!("Journal disabled: cannot prepare {}: {e}", path.display());
            return None;
        }

        let (tx, rx) = mpsc::channel(QUEUE_DEPTH);
        let dropped = Arc::new(AtomicU64::new(0));

        let handle = tokio::task::spawn_blocking({
            let dropped = Arc::clone(&dropped);
            move || writer(conn, rx, retention, &dropped)
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
        let payload_json = match serde_json::to_string(decision) {
            Ok(json) => json,
            Err(e) => {
                tracing::debug!("Journal: cannot serialize decision: {e}");
                return;
            }
        };
        let world_json = match serde_json::to_string(&state.world) {
            Ok(json) => json,
            Err(e) => {
                tracing::debug!("Journal: cannot serialize world: {e}");
                return;
            }
        };
        let ctrl_state_json = match serde_json::to_string(&state.controller) {
            Ok(json) => json,
            Err(e) => {
                tracing::debug!("Journal: cannot serialize controller state: {e}");
                return;
            }
        };
        // The grid figure with the battery's own flow removed — what the house
        // would have been drawing without it. Derivable from `world_json`, kept
        // as a column because every question about whether a decision was right
        // starts by asking for it.
        let pre_battery_net_w = state.world.underlying_grid().get();

        let row = |device, command, outcome, error| {
            Record::Decision(Box::new(DecisionRow {
                ts_ms,
                kind: kind.as_str(),
                device,
                payload_json: payload_json.clone(),
                world_json: world_json.clone(),
                ctrl_state_json: ctrl_state_json.clone(),
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
                serde_json::to_string(&outcome.applied)
                    .ok()
                    .map(|s| s.trim_matches('"').to_string()),
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

/// `auto_vacuum` has to be set before the first table exists, so a database
/// created by an older build keeps its old setting. Harmless: without it the
/// file holds its high-water mark instead of shrinking after a prune.
fn prepare(conn: &Connection, version: &str, config_json: &str) -> rusqlite::Result<()> {
    // First, before anything writes a page: `auto_vacuum` can only be set while
    // the database is still empty, and switching to WAL is itself a write. Set
    // after, it silently reports success and leaves the setting at NONE.
    conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    // NORMAL, not FULL: the writer must not fsync per row. A power cut can lose
    // the last transactions; WAL still recovers a consistent database, and the
    // alternative is SD-card fsync latency on a box that writes every second.
    conn.pragma_update(None, "synchronous", "NORMAL")?;

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS sessions (
             id          INTEGER PRIMARY KEY,
             started_ms  INTEGER NOT NULL,
             version     TEXT    NOT NULL,
             config_json TEXT    NOT NULL
         );
         CREATE TABLE IF NOT EXISTS events (
             id           INTEGER PRIMARY KEY,
             ts_ms        INTEGER NOT NULL,
             kind         TEXT    NOT NULL,
             payload_json TEXT    NOT NULL
         );
         CREATE TABLE IF NOT EXISTS decisions (
             id                INTEGER PRIMARY KEY,
             ts_ms             INTEGER NOT NULL,
             device            TEXT,
             kind              TEXT    NOT NULL,
             payload_json      TEXT    NOT NULL,
             world_json        TEXT    NOT NULL,
             ctrl_state_json   TEXT    NOT NULL,
             command           TEXT,
             outcome           TEXT,
             error             TEXT,
             pre_battery_net_w REAL    NOT NULL
         );
         CREATE INDEX IF NOT EXISTS events_ts    ON events (ts_ms);
         CREATE INDEX IF NOT EXISTS decisions_ts ON decisions (ts_ms);",
    )?;

    // One row per process. A replay seeded from before the first decision has
    // this to fall back on, and it dates every restart.
    conn.execute(
        "INSERT INTO sessions (started_ms, version, config_json) VALUES (?1, ?2, ?3)",
        (Utc::now().timestamp_millis(), version, config_json),
    )?;
    Ok(())
}

/// Owns the connection for the life of the process. Ends when every `Journal`
/// handle is dropped, which in practice means the process is going down.
fn writer(
    conn: Connection,
    mut rx: mpsc::Receiver<Record>,
    retention: RetentionDays,
    dropped: &AtomicU64,
) {
    let mut last_prune = Utc::now().date_naive();
    prune(&conn, retention);

    while let Some(record) = rx.blocking_recv() {
        if let Err(e) = write(&conn, &record) {
            // Debug, not warn: a failing database would otherwise emit a line
            // per reading. The dropped counter above is the loud signal.
            tracing::debug!("Journal: write failed: {e}");
        }

        // Once a day, on whichever record happens to cross midnight.
        let today = Utc::now().date_naive();
        if today != last_prune {
            last_prune = today;
            prune(&conn, retention);
        }
    }

    let n = dropped.load(Ordering::Relaxed);
    if n > 0 {
        tracing::warn!("Journal closing — {n} records were dropped");
    }
}

fn write(conn: &Connection, record: &Record) -> rusqlite::Result<()> {
    match record {
        Record::Event {
            ts_ms,
            kind,
            payload_json,
        } => conn.execute(
            "INSERT INTO events (ts_ms, kind, payload_json) VALUES (?1, ?2, ?3)",
            (ts_ms, kind, payload_json),
        )?,
        Record::Decision(row) => conn.execute(
            "INSERT INTO decisions
                 (ts_ms, device, kind, payload_json, world_json, ctrl_state_json,
                  command, outcome, error, pre_battery_net_w)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            (
                row.ts_ms,
                &row.device,
                row.kind,
                &row.payload_json,
                &row.world_json,
                &row.ctrl_state_json,
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
    for table in ["events", "decisions"] {
        match conn.execute(&format!("DELETE FROM {table} WHERE ts_ms < ?1"), [cutoff]) {
            Ok(n) => removed += n,
            Err(e) => {
                tracing::warn!("Journal: cannot prune {table}: {e}");
                return;
            }
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

/// Unused: proves `prune` deletes by date rather than by row count, without
/// making a test wait a day.
#[cfg(test)]
fn backdate(conn: &Connection, days: i64) {
    let ts = (Utc::now() - chrono::Duration::days(days)).timestamp_millis();
    conn.execute("UPDATE events SET ts_ms = ?1", [ts]).unwrap();
    conn.execute("UPDATE decisions SET ts_ms = ?1", [ts])
        .unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::battery::BatteryState;
    use crate::clock::Clock;
    use crate::controller::Controller;
    use crate::device::Applied;
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

    /// The world and the controller's state travel with the decision, so a row
    /// carries the inputs it was made from and can seed a replay on its own.
    #[tokio::test]
    async fn a_decision_row_carries_the_world_and_the_controller_state() {
        let dir = tempfile::tempdir().unwrap();
        let (journal, path) = open(&dir);
        let state = engine_state();
        journal.decision(
            NOW_MS,
            DecisionKind::Decision,
            &decision(),
            &state,
            &[outcome("SN123", Applied::Ok, None)],
        );
        let conn = drain(journal, &path).await;

        let (world_json, ctrl_json, pre_net): (String, String, f64) = conn
            .query_row(
                "SELECT world_json, ctrl_state_json, pre_battery_net_w FROM decisions",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();

        assert_eq!(state.world, serde_json::from_str(&world_json).unwrap());
        assert_eq!(state.controller, serde_json::from_str(&ctrl_json).unwrap());
        // 150.5 grid + (-300) battery: what the house drew without the battery.
        assert_eq!(pre_net, -149.5);
    }

    /// Retention is the only thing bounding this file. Deleting by date rather
    /// than by count is what makes the window mean a window.
    #[tokio::test]
    async fn prune_deletes_beyond_the_retention_window() {
        let dir = tempfile::tempdir().unwrap();
        let (journal, path) = open(&dir);
        journal.raw("shelly", r#"{"old":true}"#);
        let conn = drain(journal, &path).await;

        backdate(&conn, 100);
        prune(&conn, days(90));
        assert_eq!(count(&conn, "SELECT count(*) FROM events"), 0);
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
