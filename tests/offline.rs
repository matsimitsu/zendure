//! Integration tests for offline subcommands.
//!
//! `export`, `analyze` and `replay` are documented in three places — README.md,
//! src/cli.rs, and src/commands.rs — as reading **no configuration at all**.
//! This is what lets them run on a machine with no broker, no battery, and no
//! `ZENDURE_IP` or `MQTT_HOST` — you can copy a database to your laptop and
//! reason about a problem.
//!
//! Until now, nothing tested it. These assertions pin the promise: a fixture is
//! hermetic, and the two offline subcommands are the ones you reach for when
//! diagnosing on a machine that isn't the controller.

use std::process::Command;

fn offline_binary() -> String {
    env!("CARGO_BIN_EXE_zendure").to_string()
}

fn fixture_path() -> String {
    format!("{}/tests/fixtures/journey.json", env!("CARGO_MANIFEST_DIR"))
}

/// `replay` runs with no environment — not even `PATH`, not even `HOME`.
///
/// The promise is hermetic: a fixture carries its own state and the events that
/// led to it; there is nothing to read from the system. If this test fails, the
/// subcommand has accidentally learned to depend on a variable.
#[test]
fn replay_runs_with_no_environment() {
    let output = Command::new(offline_binary())
        .arg("replay")
        .arg(fixture_path())
        .env_clear()
        .output()
        .expect("failed to run replay");

    assert!(
        output.status.success(),
        "replay with no env should exit 0\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// `export` on a nonexistent database fails with a file error, not configuration
/// error — proving it got far enough to try the file.
///
/// The point is not that a file error is better: it's that `export` never
/// wanted a config to begin with. If this fails with text mentioning
/// configuration, the subcommand has drifted into reading settings it shouldn't.
#[test]
fn export_fails_on_missing_db_with_file_error() {
    let output = Command::new(offline_binary())
        .arg("export")
        .arg("--from")
        .arg("0")
        .arg("--to")
        .arg("1")
        .arg("--db")
        .arg("/nonexistent/zendure.db")
        .env_clear()
        .output()
        .expect("failed to run export");

    assert!(
        !output.status.success(),
        "export on missing db should exit non-zero"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    // Keep this loose so we don't pin an exact sqlite message — the assertion
    // is only that "config" does not appear anywhere in the error, which means
    // the error is about trying to open the file, not about configuration.
    assert!(
        !stderr.to_lowercase().contains("config"),
        "export should fail with a file error, not a configuration error: {}",
        stderr
    );
}

/// `export` rejects the `--config` flag, even though the daemon accepts it.
///
/// This is structural: the `Invocation::Export` enum variant carries no config
/// field (see cli.rs for why), so there is no way for `commands::export` to
/// read one even if it wanted to. The flag is rejected at parse time, which is
/// more honest than silently accepting and ignoring it.
#[test]
fn export_rejects_config_flag() {
    let output = Command::new(offline_binary())
        .arg("export")
        .arg("--from")
        .arg("0")
        .arg("--to")
        .arg("1")
        .arg("--config")
        .arg("/tmp/x.toml")
        .env_clear()
        .output()
        .expect("failed to run export");

    assert!(
        !output.status.success(),
        "export --config should exit non-zero"
    );
}

/// `replay` rejects the `--config` flag, just like `export` does.
///
/// Same reasoning: `Invocation::Replay` has no config field, so the flag is
/// rejected at parse time. An offline subcommand that accidentally accepted
/// `--config` would silently ignore it instead of warning the operator.
#[test]
fn replay_rejects_config_flag() {
    let output = Command::new(offline_binary())
        .arg("replay")
        .arg(fixture_path())
        .arg("--config")
        .arg("/tmp/x.toml")
        .env_clear()
        .output()
        .expect("failed to run replay");

    assert!(
        !output.status.success(),
        "replay --config should exit non-zero"
    );
}

/// `analyze` is offline for the same reason `export` is: energy is integrated
/// from rows that already happened, and nothing about how to reach a broker or
/// a battery could change the answer. A missing database must therefore fail as
/// a file error, never as a configuration one.
#[test]
fn analyze_fails_on_missing_db_with_file_error() {
    let output = Command::new(offline_binary())
        .arg("analyze")
        .arg("--from")
        .arg("0")
        .arg("--to")
        .arg("1")
        .arg("--db")
        .arg("/nonexistent/zendure.db")
        .env_clear()
        .output()
        .expect("failed to run analyze");

    assert!(
        !output.status.success(),
        "analyze on missing db should exit non-zero"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.to_lowercase().contains("config"),
        "analyze should fail with a file error, not a configuration error: {}",
        stderr
    );
}

/// `analyze` rejects `--config` too. `Invocation::Analyze` carries no config
/// field, so this is structural rather than a check someone has to remember.
#[test]
fn analyze_rejects_config_flag() {
    let output = Command::new(offline_binary())
        .arg("analyze")
        .arg("--from")
        .arg("0")
        .arg("--to")
        .arg("1")
        .arg("--config")
        .arg("/tmp/x.toml")
        .env_clear()
        .output()
        .expect("failed to run analyze");

    assert!(
        !output.status.success(),
        "analyze --config should exit non-zero"
    );
}
