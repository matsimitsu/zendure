//! What the offline subcommands do, once `cli` has said which one.
//!
//! Separate from `main.rs`, which is the async coordinator loop and shares
//! nothing with these — they are synchronous, touch no device and read no
//! configuration. Separate from `cli.rs`, which deliberately holds parsing
//! alone so the whole argument surface stays testable against a vector of
//! strings. The run half needed a home of its own rather than whichever file
//! happened to have a `main` in it.

use std::io::Write;
use std::path::Path;

use crate::journal::read;
use crate::replay::{self, Fixture};
use crate::units::Timestamp;

/// `zendure export` — a stretch of the journal as a replay fixture.
///
/// Reads no configuration: a fixture carries the tuning it was decided under,
/// recorded in the journal's own session row, and nothing about how to reach a
/// device. That is what makes this runnable against a copied database on a
/// laptop with no broker in sight.
pub fn export(
    db: &Path,
    from: Timestamp,
    to: Timestamp,
    out: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let recording = read::read_range(db, from, to)?;
    let (fixture, warnings) = replay::from_recording(recording)?;
    for warning in &warnings {
        eprintln!("warning: {warning}");
    }

    let json = serde_json::to_string_pretty(&fixture)? + "\n";
    match out {
        Some(path) => {
            std::fs::write(path, &json)?;
            eprintln!(
                "wrote {} events to {} (seeded at {}ms)",
                fixture.events.len(),
                path.display(),
                fixture.seed.at.as_millis()
            );
        }
        // Not `println!`, which panics when the reader goes away — and piping
        // to something that stops reading early (`zendure export … | head`) is
        // an ordinary thing to do with a tool whose default is stdout.
        None => write_stdout(&json)?,
    }
    Ok(())
}

/// `zendure replay` — the same events through the same fold, printed.
pub fn replay_fixture(
    path: &Path,
    verify: bool,
    overrides: &[(String, String)],
) -> Result<(), Box<dyn std::error::Error>> {
    let fixture: Fixture = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    let frames = replay::run(&fixture, overrides)?;

    write_stdout(&(replay::render(&frames) + "\n"))?;

    if verify {
        replay::verify(&fixture, &frames)?;
        eprintln!("verified: {} frames match the recording", frames.len());
    }
    Ok(())
}

/// A closed pipe is the reader's decision, not our failure.
fn write_stdout(s: &str) -> std::io::Result<()> {
    match std::io::stdout().write_all(s.as_bytes()) {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        other => other,
    }
}
