//! Re-running a recorded event stream through the decision engine, offline.
//!
//! This is **decision diff**, and it is the only replay mode that exists here.
//! Feed the engine the events it was fed, from the state it was in, and compare
//! the commands that come out against the ones the daemon actually issued. It
//! answers exactly one question — *did this change alter behaviour?* — which is
//! the question every refactor in this codebase has raised.
//!
//! It is **not** forward simulation, and the difference is not a matter of
//! degree. A recorded meter reading was caused in part by the old controller's
//! own output: the grid figure includes the battery flow the controller
//! commanded a second earlier. Replaying a *different* controller against those
//! readings asks what it would have done in a world that its own actions would
//! have changed, and the answer is fiction. Simulating forward needs a battery
//! model and the old controller de-convolved back out of the recording —
//! `pre_battery_net_w` is stored from day one so that stays possible, but
//! nothing here does it, and the two modes must never be conflated.
//!
//! A fixture is therefore hermetic by construction: it carries the tuning
//! (`SessionConfig`, the decision knobs and nothing that says how to reach a
//! device), the snapshot to resume from, and the events. No environment, no
//! network, no clock — `Engine::step` reads none of them.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::allocate::Directive;
use crate::config::SessionConfig;
use crate::controller::Controller;
use crate::engine::{Engine, EngineState};
use crate::event::Event;
use crate::journal::{RecordedDecision, SeqEvent, Slice};
use crate::units::Timestamp;
use crate::world::World;

/// Bumped only for a change that an older build would mis-read. New fields
/// arrive with `serde(default)` and do not move it.
const FORMAT: u32 = 1;

/// Rendered in place of a command list when a step produced none. A blank would
/// be indistinguishable from a missing line in a diff.
const NOTHING: &str = "—";

/// A self-contained replay: everything needed, nothing that reaches outside.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fixture {
    pub format: u32,
    pub session: SessionMeta,
    pub seed: Seed,
    pub events: Vec<Event>,
    /// What the daemon actually commanded, one line per event, in `render`'s
    /// format. Built from the recorded decision rows — **not** from a replay,
    /// which would make `--verify` compare a replay against itself.
    #[serde(default)]
    pub expected: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMeta {
    pub started_at_ms: i64,
    pub version: String,
    pub config: SessionConfig,
}

/// The fold's state before the first event. `EngineState` whole, rather than
/// its three fields spelled out: that is the shape the journal stores in one
/// column, and splitting it is how `mqtt_timed_out` got lost once already.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Seed {
    pub at_ms: i64,
    pub state: EngineState,
}

/// One event's worth of fold output. Kept even when empty: an event that
/// decides nothing is a fact about the fold, and dropping those would let a
/// regression that stops deciding look like a shorter list instead of a diff.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub at: Timestamp,
    pub directives: Vec<Directive>,
}

/// Fold the events, one frame per event.
pub fn replay(engine: &mut Engine, events: &[Event]) -> Vec<Frame> {
    events
        .iter()
        .map(|event| Frame {
            at: event.at(),
            directives: engine.step(event).directives,
        })
        .collect()
}

/// `<at_ms>ms: <device> <command>`, one line per frame, no trailing newline.
///
/// The device id is on every line even though there is one battery today. The
/// allocator deliberately leaves extra devices uncommanded until a split policy
/// exists (`allocate`), so a render that showed only commands would hide
/// exactly the thing that fence was built to make loud.
pub fn render(frames: &[Frame]) -> String {
    frames
        .iter()
        .map(|frame| {
            let body = frame
                .directives
                .iter()
                .map(|d| format!("{} {}", d.device(), d.describe()))
                .collect::<Vec<_>>();
            line(frame.at.as_millis(), body)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn line(at_ms: i64, body: Vec<String>) -> String {
    if body.is_empty() {
        format!("{at_ms}ms: {NOTHING}")
    } else {
        format!("{at_ms}ms: {}", body.join(", "))
    }
}

/// What the daemon commanded, in `render`'s format, aligned to the events.
///
/// Alignment is by `seq`, not by timestamp. A decision row carries the same
/// millisecond as the event that produced it, so timestamps cannot separate two
/// events that shared one — and the raw `shelly` capture is written from a
/// different task, so it can land between an event and its decision and make
/// "the next row" the wrong row. `seq` is assigned by the single writer in
/// arrival order, which makes "the decision rows after this event and before
/// the next" exact.
fn recorded_lines(events: &[SeqEvent], decisions: &[RecordedDecision]) -> Vec<String> {
    events
        .iter()
        .enumerate()
        .map(|(i, event)| {
            let until = events.get(i + 1).map(|next| next.seq).unwrap_or(i64::MAX);
            let body = decisions
                .iter()
                .filter(|d| d.seq > event.seq && d.seq < until)
                .filter_map(|d| match (&d.device, &d.command) {
                    // A decision that commanded nothing still gets a row, with
                    // both columns null. That is an empty command list, not a
                    // missing one.
                    (Some(device), Some(command)) => Some(format!("{device} {command}")),
                    _ => None,
                })
                .collect();
            line(event.event.at().as_millis(), body)
        })
        .collect()
}

/// Turn a slice of the journal into a fixture.
pub fn from_slice(slice: Slice) -> Result<Fixture, String> {
    let Slice {
        version,
        config,
        seed,
        events,
        decisions,
        spans_sessions,
    } = slice;

    if events.is_empty() {
        return Err("no replayable events in that range".to_string());
    }
    if spans_sessions {
        eprintln!(
            "warning: this range spans a restart; the fixture carries one session's tuning, \
             so part of it may replay under knobs it was not decided with"
        );
    }

    let expected = recorded_lines(&events, &decisions);

    // Without a recorded snapshot there is nothing to resume from, so the seed
    // is what a controller starting just before the first event would hold. The
    // day ordinal has to come from that event's own clock — it is the only
    // place in a fixture that knows one, and the midnight reset reads it.
    let seed = match seed {
        Some(seed) => Seed {
            at_ms: seed.at.as_millis(),
            state: seed.state,
        },
        None => {
            let first = events[0].event.clock();
            eprintln!(
                "warning: no decision recorded at or before the start of this range; \
                 seeding from a fresh controller at {}ms",
                first.now.as_millis()
            );
            Seed {
                at_ms: first.now.as_millis(),
                state: EngineState {
                    world: World::new(),
                    controller: Controller::from_session(&config, first).state(),
                    mqtt_timed_out: false,
                },
            }
        }
    };

    Ok(Fixture {
        format: FORMAT,
        session: SessionMeta {
            started_at_ms: seed.at_ms,
            version,
            config,
        },
        seed,
        events: events.into_iter().map(|e| e.event).collect(),
        expected,
    })
}

/// Replace one tuning knob by name.
///
/// Goes through `SessionConfig`'s own serialization rather than a match over
/// field names, so there is one list of knobs and it is the one the journal
/// already writes. A key that is not already present is an error: a typo that
/// silently changed nothing would make a what-if quietly answer the original
/// question.
pub fn apply_overrides(
    config: &SessionConfig,
    overrides: &[(String, String)],
) -> Result<SessionConfig, String> {
    let mut value = serde_json::to_value(config).map_err(|e| e.to_string())?;
    let map = value
        .as_object_mut()
        .ok_or("session config is not an object")?;

    for (key, raw) in overrides {
        if !map.contains_key(key) {
            let mut known: Vec<&String> = map.keys().collect();
            known.sort();
            let known = known
                .iter()
                .map(|k| k.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(format!("unknown tuning knob `{key}`; known knobs: {known}"));
        }
        // Parsed as JSON so numbers stay numbers and `null` reaches an
        // `Option`; anything else is taken as a string, which is what
        // `--set balance_weekday=Mon` means.
        let parsed =
            serde_json::from_str(raw).unwrap_or_else(|_| serde_json::Value::String(raw.clone()));
        map.insert(key.clone(), parsed);
    }

    serde_json::from_value(value).map_err(|e| format!("override rejected: {e}"))
}

/// Build the engine a fixture describes and run its events through it.
pub fn run(fixture: &Fixture, overrides: &[(String, String)]) -> Result<Vec<Frame>, String> {
    if fixture.format != FORMAT {
        return Err(format!(
            "fixture format {} is not supported (this build reads {FORMAT})",
            fixture.format
        ));
    }

    let config = apply_overrides(&fixture.session.config, overrides)?;

    // No events, no fold. Returning here rather than inventing a clock to build
    // a controller that would never be asked anything.
    let Some(clock) = fixture.events.first().map(|e| *e.clock()) else {
        return Ok(Vec::new());
    };

    // That clock only seeds a controller `restore` immediately overwrites; the
    // fixture's snapshot is the real starting state.
    let mut engine = Engine::new(
        Controller::from_session(&config, &clock),
        World::new(),
        Duration::from_secs(config.mqtt_timeout_secs),
    );
    engine.restore(fixture.seed.state.clone());

    Ok(replay(&mut engine, &fixture.events))
}

/// Diff a replay against what was recorded. `Ok(())` when they agree.
pub fn verify(fixture: &Fixture, frames: &[Frame]) -> Result<(), String> {
    let actual: Vec<String> = render(frames).lines().map(str::to_string).collect();

    if fixture.expected.is_empty() {
        return Err("fixture has no `expected` to verify against".to_string());
    }
    if fixture.expected.len() != actual.len() {
        return Err(format!(
            "replay produced {} frames, the recording has {}",
            actual.len(),
            fixture.expected.len()
        ));
    }
    for (i, (want, got)) in fixture.expected.iter().zip(&actual).enumerate() {
        if want != got {
            return Err(format!(
                "diverged at frame {i}:\n  recorded: {want}\n  replayed: {got}"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "replay_tests.rs"]
mod tests;
