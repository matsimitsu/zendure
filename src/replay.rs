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

use std::fmt::Display;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::allocate::Directive;
use crate::config::SessionConfig;
use crate::controller::Controller;
use crate::engine::{Engine, EngineState};
use crate::event::Event;
use crate::journal::read::Recording;
use crate::units::Timestamp;
use crate::world::{DeviceId, World};

/// The fixture format this build reads and writes.
///
/// Checked exactly, not as a floor: a fixture from a *newer* build may carry
/// fields that change what its events mean, and one from an older build was
/// written before some invariant this build relies on. Additive changes keep
/// the number and arrive with `serde(default)`; anything that would make either
/// side read the other wrong moves it.
const FORMAT: u32 = 1;

/// Rendered in place of a command list when a step produced none. A blank would
/// be indistinguishable from a missing line in a diff.
///
/// `pub(crate)` rather than private: `run.rs`'s round-trip test checks that a
/// real run's `expected` list is not *entirely* this sentinel, which is the
/// same anti-vacuity guard `replay_tests.rs` already applies to a canned
/// fixture — reusing the constant keeps both checks tied to one literal
/// instead of a second copy that could silently drift from `render`'s own.
pub(crate) const NOTHING: &str = "—";

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
    /// When the recording session started — the journal's own `sessions` row,
    /// not the seed's timestamp. The two answer different questions, and
    /// holding the second under the first's name made a fixture quietly claim a
    /// session began whenever the range happened to be anchored.
    pub started_at: Timestamp,
    pub version: String,
    pub config: SessionConfig,
}

/// The fold's state before the first event. `EngineState` whole, rather than
/// its three fields spelled out: that is the shape the journal stores in one
/// column, and splitting it is how `mqtt_timed_out` got lost once already.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Seed {
    pub at: Timestamp,
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

/// One commanded device, as both a replay and a recording render it.
///
/// The only place this format exists. A replayed step holds a `Directive` and a
/// recording holds two text columns, and if the two rendered differently by so
/// much as a space, `--verify` would report a divergence on every frame that
/// commanded anything — so it is one function rather than two `format!` calls
/// that happen to agree today. `Directive` deliberately has no `Display` of its
/// own: `Command`'s `Display` is the journal's wire format and a device serial
/// has no business in it, which is exactly why the id rides on the `Directive`.
fn addressed(device: &DeviceId, command: &dyn Display) -> String {
    format!("{device} {command}")
}

/// `<at_ms>ms: <device> <command>`, one line per frame, no trailing newline.
///
/// The device id is on every line even though there is one battery today. The
/// allocator deliberately leaves extra devices uncommanded until a split policy
/// exists (`allocate`), so a render that showed only commands would hide
/// exactly the thing that fence was built to make loud.
pub fn render(frames: &[Frame]) -> String {
    lines(frames).join("\n")
}

fn lines(frames: &[Frame]) -> Vec<String> {
    frames
        .iter()
        .map(|frame| {
            line(
                frame.at,
                frame
                    .directives
                    .iter()
                    .map(|d| addressed(d.device(), &d.describe())),
            )
        })
        .collect()
}

fn line(at: Timestamp, body: impl Iterator<Item = String>) -> String {
    let body: Vec<String> = body.collect();
    let at = at.as_millis();
    if body.is_empty() {
        format!("{at}ms: {NOTHING}")
    } else {
        format!("{at}ms: {}", body.join(", "))
    }
}

/// Turn a recorded run into a fixture.
///
/// Returns the reader's warnings along with its own for the caller to print.
/// This module has no business deciding how loud to be, and a function that
/// prints cannot be called from a test without making noise in the suite.
pub fn from_recording(recording: Recording) -> Result<(Fixture, Vec<String>), String> {
    let Recording {
        version,
        started_at,
        config,
        seed,
        frames,
        mut warnings,
    } = recording;

    if frames.is_empty() {
        return Err("no replayable events in that range".to_string());
    }

    let expected = frames
        .iter()
        .map(|f| {
            line(
                f.event.at(),
                f.commands
                    .iter()
                    .map(|(device, cmd)| addressed(device, cmd)),
            )
        })
        .collect();

    // Without a recorded snapshot there is nothing to resume from, so the seed
    // is what a controller starting just before the first event would hold. The
    // day ordinal has to come from that event's own clock — it is the only place
    // in a fixture that knows one, and the midnight reset reads it.
    let seed = match seed {
        Some(seed) => Seed {
            at: seed.at,
            state: seed.state,
        },
        None => {
            let first = frames[0].event.clock();
            warnings.push(format!(
                "no decision was recorded at or before the start of this range, so the seed is a \
                 controller starting fresh at {}ms with an empty world. Unless the range opens on \
                 the session's own startup event, the replay has no battery to decide about and \
                 will diverge at every frame",
                first.now.as_millis()
            ));
            Seed {
                at: first.now,
                state: EngineState {
                    world: World::new(),
                    controller: Controller::from_session(&config, first).state(),
                    mqtt_timed_out: false,
                },
            }
        }
    };

    Ok((
        Fixture {
            format: FORMAT,
            session: SessionMeta {
                started_at,
                version,
                config,
            },
            seed,
            events: frames.into_iter().map(|f| f.event).collect(),
            expected,
        },
        warnings,
    ))
}

/// Replace one tuning knob by name.
///
/// Goes through `SessionConfig`'s own serialization rather than a match over
/// field names, so there is one list of knobs and it is the one the journal
/// already writes. A key that is not already present is an error: a typo that
/// silently changed nothing would make a what-if quietly answer the original
/// question.
///
/// A value still has to survive its knob's own constructor —
/// `--set min_soc=1000` clamps to 100 rather than producing an SOC no battery
/// can reach — because the clamping newtypes deserialize through it. That is
/// not this function's doing, and it has to stay true: see
/// `validating_deserialize` in `units.rs`.
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
            let mut known: Vec<&str> = map.keys().map(String::as_str).collect();
            known.sort_unstable();
            return Err(format!(
                "unknown tuning knob `{key}`; known knobs: {}",
                known.join(", ")
            ));
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
    if fixture.expected.is_empty() {
        return Err("fixture has no `expected` to verify against".to_string());
    }
    let actual = lines(frames);
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
