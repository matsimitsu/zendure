//! Reading a recorded run back out, for replay.
//!
//! Separate from the writer, which is on the control path and may never block,
//! while this is an offline tool that may take its time; they share only the
//! column names, reached as a sibling module rather than a public API.
//!
//! `seq` does not leave this module: it answers "which decision belongs to
//! which event" here, in one linear pass by the code that assigns it, so callers
//! receive events already paired with their commands.

use std::path::Path;

use rusqlite::{Connection, OpenFlags, OptionalExtension};

use super::SCHEMA_VERSION;
use crate::config::SessionConfig;
use crate::engine::EngineState;
use crate::event::Event;
use crate::models::ControlDecision;
use crate::units::{SolarPower, Timestamp};
use crate::world::DeviceId;

/// A recorded run, ready to become a replay fixture.
#[derive(Debug)]
pub struct Recording {
    /// The build that wrote the seed's session, for a fixture's provenance.
    pub version: String,
    /// When that session started.
    pub started_at: Timestamp,
    /// The tuning those rows were decided under.
    pub config: SessionConfig,
    /// `None` when nothing was recorded at or before the start of the range —
    /// an empty journal, or a range that opens before the first decision. This
    /// module will not invent a snapshot it did not record; the caller decides
    /// what to do about it.
    pub seed: Option<Seed>,
    /// Each event with what the daemon actually commanded in response, already
    /// paired.
    pub frames: Vec<RecordedFrame>,
    /// Anything the caller should say out loud. Returned as data rather than
    /// printed from here: a reader that prints is a reader that cannot be
    /// called from a test, a server, or anything that wants to decide for
    /// itself how loud to be.
    pub warnings: Vec<String>,
}

/// The state a replay resumes from, and when it was taken.
#[derive(Debug)]
pub struct Seed {
    pub at: Timestamp,
    pub state: EngineState,
}

/// One recorded event and the commands it produced. `outcome` is deliberately
/// absent: it records whether an HTTP write landed, which a replay — performing
/// no writes — structurally cannot produce.
#[derive(Debug)]
pub struct RecordedFrame {
    pub event: Event,
    pub commands: Vec<(DeviceId, String)>,
}

/// Where a slice of the journal starts.
///
/// Two modes, named rather than encoded as sentinel values in a pair of
/// integers.
enum Anchor {
    /// Resume from a recorded snapshot: everything written after it.
    After(i64),
    /// No snapshot exists before the range, so it opens at the range itself.
    From(Timestamp),
}

impl Anchor {
    /// The `WHERE` fragment and its first bound parameter. Both queries use it,
    /// so neither carries a clause meant for the other mode.
    fn clause(&self) -> (&'static str, i64) {
        match self {
            Anchor::After(seq) => ("seq > ?1", *seq),
            Anchor::From(at) => ("ts_ms >= ?1", at.as_millis()),
        }
    }
}

/// What can go wrong reading a journal. Its own type because `read_range` does
/// three things — SQL, JSON decoding, a schema-shape check — and only one is
/// SQLite's; smuggling the other two out through `rusqlite::Error` variants
/// gave callers and users error messages that named the wrong problem.
#[derive(Debug)]
pub enum ReadError {
    Sql(rusqlite::Error),
    Decode {
        what: &'static str,
        source: serde_json::Error,
    },
    /// The file opened but is not a journal this build can read.
    Schema(String),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadError::Sql(e) => write!(f, "{e}"),
            ReadError::Decode { what, source } => write!(f, "{what} is unreadable: {source}"),
            ReadError::Schema(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for ReadError {}

impl From<rusqlite::Error> for ReadError {
    fn from(e: rusqlite::Error) -> Self {
        ReadError::Sql(e)
    }
}

type Result<T> = std::result::Result<T, ReadError>;

fn decode<T: serde::de::DeserializeOwned>(what: &'static str, json: &str) -> Result<T> {
    serde_json::from_str(json).map_err(|source| ReadError::Decode { what, source })
}

/// Opens `path` read-only and refuses a journal this build cannot read.
///
/// Never creating: `Connection::open` would make an empty database out of a
/// mistyped path and then fail with `no such table` instead of `no such file`.
fn open_for_reading(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    // WAL keeps readers off the writer's back, but a checkpoint or an
    // operator's `VACUUM` takes an exclusive lock and the default timeout is
    // zero.
    conn.busy_timeout(std::time::Duration::from_secs(5))?;

    let stamped: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if stamped != SCHEMA_VERSION {
        return Err(ReadError::Schema(format!(
            "{} is schema version {stamped}, this build reads {SCHEMA_VERSION}",
            path.display()
        )));
    }
    Ok(conn)
}

/// Reads everything needed to replay the run between two instants. The range
/// is anchored to the last *decision* at or before `from`, not to `from`
/// itself, since only decision rows carry a snapshot to resume from — so it
/// can start earlier than asked rather than replay from a state that wasn't recorded
/// there.
pub fn read_range(path: &Path, from: Timestamp, to: Timestamp) -> Result<Recording> {
    let conn = open_for_reading(path)?;

    // One transaction across all three reads: as independent snapshots against a
    // database the daemon appends to a few times a second, exporting with `--to`
    // near now could read events, then decisions belonging to an event not yet
    // read, and attribute them to the last frame — a divergence `--verify` would
    // wrongly report.
    let tx = conn.unchecked_transaction()?;

    let mut warnings = Vec::new();

    let seed_row = tx
        .query_row(
            "SELECT seq, ts_ms, state_json, session_id FROM decisions
             WHERE ts_ms <= ?1 ORDER BY seq DESC LIMIT 1",
            [from.as_millis()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .optional()?;

    let anchor = match &seed_row {
        Some((seq, ..)) => Anchor::After(*seq),
        None => Anchor::From(from),
    };
    let (where_clause, anchor_param) = anchor.clause();

    // The newest row in this snapshot, both tables. A decision is written after
    // its event — and after an HTTP round trip, in the daemon — so an event
    // that is the last thing in the file may simply not have its decision yet.
    // Anything before that point has either been decided or never will be.
    let head: i64 = tx
        .query_row(
            "SELECT max(seq) FROM (SELECT max(seq) AS seq FROM events
                               UNION ALL
                               SELECT max(seq) AS seq FROM decisions)",
            [],
            |row| row.get::<_, Option<i64>>(0),
        )?
        .unwrap_or(0);

    let kinds = Event::KINDS.map(|_| "?").join(", ");
    let mut stmt = tx.prepare(&format!(
        "SELECT seq, payload_json, session_id FROM events
         WHERE {where_clause} AND ts_ms <= ?2 AND kind IN ({kinds})
         ORDER BY seq"
    ))?;
    let mut sessions = std::collections::BTreeSet::new();
    let mut events: Vec<(i64, Event)> = Vec::new();
    let mut rows = stmt.query(rusqlite::params_from_iter(
        [anchor_param, to.as_millis()]
            .map(rusqlite::types::Value::from)
            .into_iter()
            .chain(Event::KINDS.map(|k| rusqlite::types::Value::from(k.to_string()))),
    ))?;
    while let Some(row) = rows.next()? {
        let seq: i64 = row.get(0)?;
        let payload: String = row.get(1)?;
        match serde_json::from_str(&payload) {
            Ok(event) => events.push((seq, event)),
            // A row this build cannot parse came from another build. Skipping
            // it is right; saying so is mandatory, because its decision rows
            // are still here and will pair with the event before it, which
            // makes `--verify` blame the fold for a lossy recording.
            Err(e) => warnings.push(format!(
                "skipped an unreadable event at seq {seq} ({e}); \
                 the commands it caused will appear against the event before it"
            )),
        }
        sessions.insert(row.get::<_, i64>(2)?);
    }
    drop(rows);
    drop(stmt);

    let mut stmt = tx.prepare(&format!(
        "SELECT seq, device, command FROM decisions
         WHERE {where_clause} AND ts_ms <= ?2 ORDER BY seq"
    ))?;
    let decisions: Vec<(i64, Option<String>, Option<String>)> = stmt
        .query_map([anchor_param, to.as_millis()], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);

    // Trim an event that is the newest thing in the file: its decision may be
    // in flight behind an HTTP write, and a fixture claiming it commanded
    // nothing would fail `--verify` against a replay that commands.
    if events.last().is_some_and(|(seq, _)| *seq >= head) {
        events.pop();
        warnings.push(
            "dropped the last event: it is the newest row in the journal, so whether it \
             decided anything is not recorded yet"
                .to_string(),
        );
    }

    let frames = pair(events, &decisions);

    let session_id = match &seed_row {
        Some((.., session_id)) => Some(*session_id),
        None => sessions.iter().next().copied(),
    };

    // The seed's own session counts: feeding this set from event rows alone missed
    // the likeliest straddle — a seed that is the last decision before a restart
    // with all events after it, which is what every restart-spanning range looks
    // like, since the daemon's last act before stopping is a decision.
    if let Some(id) = session_id {
        sessions.insert(id);
    }
    if sessions.len() > 1 {
        warnings.push(
            "this range spans a restart: the fixture carries one session's tuning and one \
             session's snapshot, so part of it replays under knobs and history it was not \
             decided with"
                .to_string(),
        );
    }

    let (version, started_ms, config_json) = session_row(&tx, session_id, &mut warnings)?;
    let config = decode("the session's tuning", &config_json)?;

    let seed = seed_row
        .map(|(_, at, state_json, _)| {
            decode("the seed snapshot", &state_json).map(|state| Seed {
                at: Timestamp::from_millis(at),
                state,
            })
        })
        .transpose()?;

    Ok(Recording {
        version,
        started_at: Timestamp::from_millis(started_ms),
        config,
        seed,
        frames,
        warnings,
    })
}

/// Walks both lists once, pairing each event with the decision rows written
/// between it and the next event. Linear, since both arrive in `seq` order;
/// filtering the whole decision list per event instead is quadratic — 3.6
/// seconds for one day of events, hours for the retention window.
fn pair(
    events: Vec<(i64, Event)>,
    decisions: &[(i64, Option<String>, Option<String>)],
) -> Vec<RecordedFrame> {
    let mut next = 0usize;
    let mut frames = Vec::with_capacity(events.len());

    for (i, (seq, event)) in events.iter().enumerate() {
        // Rows before this event belong to an earlier one; skip past them
        // rather than rescanning from the start.
        while next < decisions.len() && decisions[next].0 <= *seq {
            next += 1;
        }
        let until = events.get(i + 1).map(|(s, _)| *s).unwrap_or(i64::MAX);

        let mut commands = Vec::new();
        let mut at = next;
        while at < decisions.len() && decisions[at].0 < until {
            // A decision that commanded nothing still gets a row, with both
            // columns null. That is an empty command list, not a missing one.
            if let (Some(device), Some(command)) = (&decisions[at].1, &decisions[at].2) {
                commands.push((DeviceId::new(device.clone()), command.clone()));
            }
            at += 1;
        }
        frames.push(RecordedFrame {
            event: event.clone(),
            commands,
        });
    }
    frames
}

/// The session row governing the fixture, degrading rather than failing.
/// `prune` deletes `sessions WHERE started_ms < cutoff` while a session row
/// dates at process start and its events/decisions date individually, so a
/// long-uptime daemon deletes its own session row and keeps writing rows that point at
/// it.
fn session_row(
    conn: &Connection,
    session_id: Option<i64>,
    warnings: &mut Vec<String>,
) -> Result<(String, i64, String)> {
    let found = match session_id {
        Some(id) => conn
            .query_row(
                "SELECT version, started_ms, config_json FROM sessions WHERE id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?,
        None => None,
    };
    if let Some(row) = found {
        return Ok(row);
    }

    if session_id.is_some() {
        warnings.push(
            "the session that wrote these rows has been pruned (its row is older than the \
             retention window, which a long-running process outlives); falling back to the \
             newest session's tuning, which may not be what these rows were decided under"
                .to_string(),
        );
    }
    conn.query_row(
        "SELECT version, started_ms, config_json FROM sessions ORDER BY id DESC LIMIT 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )
    .optional()?
    .ok_or_else(|| ReadError::Schema("this journal has no sessions recorded".to_string()))
}

/// One decision row, for the dashboard's decision log — not a replay fixture,
/// so it carries none of `read_range`'s snapshot/pairing apparatus.
#[derive(Debug)]
pub struct DecisionRow {
    pub at: Timestamp,
    pub decision: ControlDecision,
}

/// The last `limit` decisions, oldest first, for the dashboard's decision log.
/// Unlike [`read_range`], this does not anchor to a snapshot or pair events
/// with decisions. [`Journal::decision`](super::Journal::decision) writes one
/// row per commanded device sharing a `ts_ms` and `payload_json`; `GROUP BY` collapses
/// them to one entry.
pub fn read_recent_decisions(path: &Path, limit: usize) -> Result<Vec<DecisionRow>> {
    let conn = open_for_reading(path)?;

    let mut stmt = conn.prepare(
        "SELECT ts_ms, payload_json FROM decisions
         GROUP BY ts_ms, payload_json
         ORDER BY MAX(seq) DESC LIMIT ?1",
    )?;
    let rows: Vec<(i64, String)> = stmt
        .query_map([limit as i64], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut skipped = 0usize;
    let mut decisions: Vec<DecisionRow> = rows
        .into_iter()
        // The decision log is a logging concern; one row the current build
        // cannot decode must not cost the dashboard the other nineteen.
        .filter_map(|(ts_ms, payload)| match decode("a decision", &payload) {
            Ok(decision) => Some(DecisionRow {
                at: Timestamp::from_millis(ts_ms),
                decision,
            }),
            Err(_) => {
                skipped += 1;
                None
            }
        })
        .collect();
    if skipped > 0 {
        tracing::warn!(skipped, path = %path.display(), "skipped undecodable decision rows");
    }
    decisions.reverse();
    Ok(decisions)
}

/// Every `meter`-kind event at or after `since_ms`, oldest first — what the
/// dashboard's actual-solar-production history seeds from at startup, so a
/// restart mid-day doesn't blank today's line. `kind` and `ts_ms` are both
/// indexed columns (see the journal's schema), so this is a cheap scan even
/// across a day's worth of once-a-second meter ticks.
pub fn read_meter_solar_since(path: &Path, since_ms: i64) -> Result<Vec<(i64, SolarPower)>> {
    let conn = open_for_reading(path)?;

    let mut stmt = conn.prepare(
        "SELECT ts_ms, payload_json FROM events WHERE kind = 'meter' AND ts_ms >= ?1 ORDER BY seq",
    )?;
    let rows: Vec<(i64, String)> = stmt
        .query_map([since_ms], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut skipped = 0usize;
    let mut readings = Vec::with_capacity(rows.len());
    for (ts_ms, payload) in rows {
        match decode::<Event>("a meter reading", &payload) {
            Ok(Event::Meter { solar, .. }) => readings.push((ts_ms, solar)),
            Ok(_) => {}
            Err(_) => skipped += 1,
        }
    }
    if skipped > 0 {
        tracing::warn!(skipped, path = %path.display(), "skipped undecodable meter rows");
    }
    Ok(readings)
}

/// Every foldable observation between two instants, oldest first — the raw
/// material `analyze` integrates.
///
/// `meter` and `device_update` only: `mqtt_timeout` carries no measurement, and
/// the raw `shelly`/`zendure_poll` captures are stamped with their write time
/// rather than their observation time, which would put them out of order among
/// rows that aren't. Returned as `Event`s rather than a shape chosen here —
/// what a reading *means* is `analyze`'s business, and this module's job ends
/// at handing over rows it could decode.
pub fn read_events_in_range(path: &Path, from: Timestamp, to: Timestamp) -> Result<Vec<Event>> {
    let conn = open_for_reading(path)?;

    // Ordered by `seq`, not `ts_ms`: `seq` is assigned by the single writer
    // thread, so it is the order these were observed in even where two rows
    // share a millisecond.
    let mut stmt = conn.prepare(
        "SELECT payload_json FROM events \
         WHERE kind IN ('meter', 'device_update') AND ts_ms >= ?1 AND ts_ms <= ?2 \
         ORDER BY seq",
    )?;
    let rows: Vec<String> = stmt
        .query_map([from.as_millis(), to.as_millis()], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut skipped = 0usize;
    let mut events = Vec::with_capacity(rows.len());
    for payload in rows {
        match decode::<Event>("an event", &payload) {
            Ok(event) => events.push(event),
            Err(_) => skipped += 1,
        }
    }
    if skipped > 0 {
        tracing::warn!(skipped, path = %path.display(), "skipped undecodable event rows");
    }
    Ok(events)
}

#[cfg(test)]
#[path = "read_tests.rs"]
mod tests;
