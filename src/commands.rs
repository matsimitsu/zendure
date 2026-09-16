//! What the offline subcommands do, once `cli` has said which one.
//!
//! Separate from `main.rs` (the async coordinator loop): these are all
//! synchronous and touch no device. `export` and `replay_fixture` read no
//! configuration at all — see `cli.rs`'s module doc for why `Invocation`
//! cannot even hand them one. `check_config` is the exception: strictly reading
//! configuration is its entire job.

use std::io::Write;
use std::path::Path;

use crate::analyze;
use crate::config::Config;
use crate::journal::read;
use crate::replay::{self, Fixture};
use crate::units::Timestamp;

/// `zendure export` — a stretch of the journal as a replay fixture. Reads no
/// configuration: a fixture carries the tuning it was decided under, recorded
/// in the journal's own session row, and nothing about how to reach a device — runnable
/// against a copied database on a laptop with no broker in sight.
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

/// `zendure analyze` — the journal between two instants, as daily energy.
/// Reads no configuration for the same reason `export` doesn't: the rows carry
/// everything the arithmetic needs, so this runs against a copied database
/// anywhere.
pub fn analyze(
    db: &Path,
    from: Timestamp,
    to: Timestamp,
) -> Result<(), Box<dyn std::error::Error>> {
    let events = read::read_events_in_range(db, from, to)?;
    write_stdout(&analyze::render(&analyze::daily(&events)))?;
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

/// `zendure --check` — parses a config file like the daemon, but strictly.
/// The daemon is lenient (a wrong-typed knob warns and falls back, since a typo must
/// not restart-loop a controller holding a battery command steady); `--check` runs at
/// deploy time with nothing to strand, and Ansible uses it as a `validate:` hook, so a
/// parse error or any warning is fatal instead.
/// Prints the effective config via `Config`'s `Debug` (redacting `mqtt_password`)
/// regardless of outcome.
pub fn check_config(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let (config, warnings) = Config::from_toml(path)?;

    println!("{config:?}");
    for warning in &warnings {
        eprintln!("warning: {warning}");
    }

    if warnings.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} warning(s) in {}; see above",
            warnings.len(),
            path.display()
        )
        .into())
    }
}

/// A closed pipe is the reader's decision, not our failure.
fn write_stdout(s: &str) -> std::io::Result<()> {
    match std::io::stdout().write_all(s.as_bytes()) {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        other => other,
    }
}
