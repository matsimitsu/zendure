//! Reading a recorded run back out, for replay.
//!
//! Separate from the writer because they are separate jobs that happen to share
//! a schema: one is on the control path and may never block it, the other is an
//! offline tool that may take its time. The column names are the only thing
//! they have in common, and this module is a sibling so that stays a short
//! reach rather than a public API.
//!
//! **`seq` does not leave this module.** The whole reason it exists is to
//! answer "which decision belongs to which event", and answering that here — in
//! one linear pass, by the code that also knows how `seq` is assigned — means
//! the caller receives events already paired with what they commanded and never
//! has to learn the ordering rules. Handing `seq` outward and aligning at the
//! far end was the first shape of this, and it leaked three types across the
//! boundary to do a join the query layer should have done.

use std::path::Path;

use rusqlite::{Connection, OpenFlags, OptionalExtension};

use super::SCHEMA_VERSION;
use crate::config::SessionConfig;
use crate::engine::EngineState;
use crate::event::Event;
use crate::models::ControlDecision;
use crate::units::Timestamp;
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

/// One recorded event and the commands it produced.
///
/// `outcome` is deliberately absent: it records whether an HTTP write landed,
/// and a replay performs no writes. Comparing it would mean comparing a replay
/// against something it structurally cannot produce.
#[derive(Debug)]
pub struct RecordedFrame {
    pub event: Event,
    pub commands: Vec<(DeviceId, String)>,
}

/// Where a slice of the journal starts.
///
/// Two modes, named rather than encoded as sentinel values in a pair of
/// integers. The first shape of this passed `(seed_seq, lower_ts)` where `0`
/// meant "no anchor" — true only because `seq` starts at 1 — and `i64::MIN`
/// meant "no lower bound", with each of the two queries carrying a clause that
/// was dead in one of the modes.
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

/// What can go wrong reading a journal.
///
/// Its own type because `read_range` does three things — SQL, JSON decoding,
/// and a schema-shape check — and only one of them is SQLite's. The first shape
/// of this returned `rusqlite::Result` and smuggled the other two out inside
/// `rusqlite::Error::InvalidParameterName`, whose documented meaning is "you
/// bound a parameter by a name the statement does not have". Anyone matching on
/// that variant got nonsense, and the message a user saw was wrapped in a
/// variant name that had nothing to do with their problem.
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

/// Read everything needed to replay the run between two instants.
///
/// The range is anchored to a *decision*, not to `from`: a replay has to resume
/// from a recorded snapshot, and the only snapshots are on decision rows. So the
/// slice starts at the last decision at or before `from` and runs to `to`, which
/// means it can begin earlier than asked. That is the honest boundary — starting
/// at `from` with the state from some other moment would replay plausible
/// nonsense.
pub fn read_range(path: &Path, from: Timestamp, to: Timestamp) -> Result<Recording> {
    let conn = open_for_reading(path)?;

    // One transaction across all three reads. They were three independent
    // snapshots of a database the daemon appends to a few times a second, so
    // exporting with `--to` near now — the whole point of the tool — could read
    // events, then read decisions belonging to an event it had not read, and
    // attribute them to the last frame. That produced fixtures asserting two
    // commands on one step, which a single-battery replay can never emit, and
    // `--verify` then reported a divergence that never happened.
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

    // The seed's own session counts. Feeding this set from the event rows alone
    // missed the likeliest straddle of all: a range whose seed is the last
    // decision before a restart and whose events are all after it. That is what
    // every range covering a restart looks like, since the daemon's last act
    // before stopping is a decision — and it reported no warning while carrying
    // the pre-restart tuning.
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
            decode(" the seed snapshot", &state_json).map(|state| Seed {
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

/// Walk both lists once, pairing each event with the decision rows written
/// between it and the next event.
///
/// Linear, because both arrive in `seq` order and a decision's rows always sit
/// between its event and the following one. The first shape of this filtered
/// the whole decision list per event, which is quadratic: measured at 3.6
/// seconds for one day of events and hours for the retention window, on a tool
/// whose obvious first use is "export the last month".
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
///
/// `prune` deletes `sessions WHERE started_ms < cutoff`, and a session row is
/// dated at *process start* while its events and decisions are dated
/// individually — so a daemon whose uptime exceeds the retention window deletes
/// its own session row and goes on writing rows that point at it. Erroring here
/// meant every export of a long-running process failed outright with
/// `QueryReturnedNoRows`. The tuning is worth reporting as missing; it is not
/// worth refusing to produce a fixture over.
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

/// The last `limit` decisions, oldest first — what the dashboard's decision
/// log renders on page load. Unlike [`read_range`], this does not anchor to a
/// snapshot or pair events with decisions.
///
/// [`Journal::decision`](super::Journal::decision) writes one row per
/// commanded device, so one decision is several rows sharing a `ts_ms` and a
/// `payload_json`; the `GROUP BY` collapses them to the one entry the
/// dashboard's live path appends.
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

#[cfg(test)]
#[path = "read_tests.rs"]
mod tests;
