//! What the offline subcommands do, once `cli` has said which one.
//!
//! Separate from `main.rs`, which is the async coordinator loop and shares
//! nothing with these — they are all synchronous and touch no device.
//! `export` and `replay_fixture` read no configuration at all — see
//! `cli.rs`'s module doc comment for why `Invocation` cannot even hand them
//! one by accident. `check_config` is the exception: reading configuration,
//! strictly, is its entire job. Separate from `cli.rs`, which deliberately
//! holds parsing alone so the whole argument surface stays testable against a
//! vector of strings. The run half needed a home of its own rather than
//! whichever file happened to have a `main` in it.

use std::io::Write;
use std::path::Path;

use crate::config::Config;
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

/// `zendure --check` — parse a config file the way the daemon would, but
/// strictly.
///
/// The daemon itself is lenient: a wrong-typed tuning knob or an unknown key
/// warns and falls back to a default rather than exiting, because `Config` is
/// read at process startup and systemd restarts a failed unit — so a typo
/// there would restart-loop a controller that is holding a battery command
/// steady. Loud and running beats silent and stopped.
///
/// `--check` inverts that on purpose: it runs at *deploy* time, not runtime,
/// so there is no battery command it could strand by refusing to proceed.
/// Ansible runs this as a `validate:` hook before a rendered config is moved
/// into place, and the whole point is that a typo fails the play and never
/// reaches production, rather than reaching it as a silent warning in a log
/// nobody is watching yet. So a parse error is fatal here exactly as it would
/// be for the daemon, but *any* warning is promoted to a failure too — that
/// asymmetry (lenient at runtime, strict at deploy time) is the keystone the
/// rest of the config design leans on; without it, the daemon's leniency
/// would just be a way for a typo to reach production quietly instead of not
/// reaching it at all.
///
/// The effective config is printed to stdout via `Config`'s hand-written
/// `Debug` — which redacts `mqtt_password` — regardless of outcome, so a
/// warning that fails the check still shows what value was actually chosen
/// instead of just naming the problem.
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
