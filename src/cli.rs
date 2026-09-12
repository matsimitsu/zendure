//! What the binary was asked to do.
//!
//! Parsing only — no I/O, no `std::env`, so the whole surface is testable
//! against an argument vector. Hand-rolled rather than pulled from a crate:
//! there are two subcommands and six flags, and the one thing that has to be
//! exactly right is that **no arguments still starts the daemon**, unchanged,
//! since that is how systemd invokes it.
//!
//! The offline subcommands are parsed *before* any configuration is read, which
//! is what lets `export` and `replay` run on a laptop with no `MQTT_HOST`,
//! no `ZENDURE_IP` and no broker. A fixture is hermetic; so is the tool that
//! makes one.

use std::path::PathBuf;

use chrono::DateTime;

use crate::config::DEFAULT_JOURNAL_PATH;
use crate::units::Timestamp;

pub const HELP: &str = "\
zendure — home battery controller

    zendure
        Run the controller. This is what the service does, and what no
        arguments has always meant.

    zendure export --from <when> --to <when> [--db <path>] [--out <file>]
        Write the journal between two instants as a replay fixture. Starts at
        the last decision at or before --from, since that is the most recent
        state a replay can resume from. Writes to stdout without --out.

    zendure replay <fixture> [--verify] [--set <knob>=<value>]...
        Re-run a fixture's events through the decision engine and print the
        commands. --verify diffs them against what the daemon actually did and
        exits non-zero if they differ. --set changes one tuning knob first, for
        asking what a different setting would have done.

    <when> is unix milliseconds or RFC 3339 (2026-09-12T19:50:00Z).
    --db defaults to /var/lib/zendure/journal.db.
";

#[derive(Debug, PartialEq)]
pub enum Invocation {
    Daemon,
    Help,
    Export {
        from: Timestamp,
        to: Timestamp,
        db: PathBuf,
        out: Option<PathBuf>,
    },
    Replay {
        fixture: PathBuf,
        verify: bool,
        overrides: Vec<(String, String)>,
    },
}

/// `args` is everything after the program name.
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Invocation, String> {
    let mut args = args.into_iter().peekable();

    let Some(first) = args.next() else {
        return Ok(Invocation::Daemon);
    };

    match first.as_str() {
        "-h" | "--help" | "help" => Ok(Invocation::Help),
        "export" => parse_export(args),
        "replay" => parse_replay(args),
        other => Err(format!("unknown command `{other}`\n\n{HELP}")),
    }
}

fn parse_export<I: Iterator<Item = String>>(mut args: I) -> Result<Invocation, String> {
    let mut from = None;
    let mut to = None;
    let mut db = None;
    let mut out = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--from" => from = Some(instant(&value(&mut args, "--from")?)?),
            "--to" => to = Some(instant(&value(&mut args, "--to")?)?),
            "--db" => db = Some(PathBuf::from(value(&mut args, "--db")?)),
            "--out" => out = Some(PathBuf::from(value(&mut args, "--out")?)),
            other => return Err(format!("export: unexpected argument `{other}`")),
        }
    }

    let from = from.ok_or("export: --from is required")?;
    let to = to.ok_or("export: --to is required")?;
    if to < from {
        return Err("export: --to is before --from".to_string());
    }

    Ok(Invocation::Export {
        from,
        to,
        db: db.unwrap_or_else(|| PathBuf::from(DEFAULT_JOURNAL_PATH)),
        out,
    })
}

fn parse_replay<I: Iterator<Item = String>>(mut args: I) -> Result<Invocation, String> {
    let mut fixture = None;
    let mut verify = false;
    let mut overrides = Vec::new();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--verify" => verify = true,
            "--set" => {
                let raw = value(&mut args, "--set")?;
                let (key, val) = raw
                    .split_once('=')
                    .ok_or_else(|| format!("replay: --set wants knob=value, got `{raw}`"))?;
                overrides.push((key.to_string(), val.to_string()));
            }
            other if other.starts_with('-') => {
                return Err(format!("replay: unexpected argument `{other}`"));
            }
            path if fixture.is_none() => fixture = Some(PathBuf::from(path)),
            other => return Err(format!("replay: unexpected argument `{other}`")),
        }
    }

    Ok(Invocation::Replay {
        fixture: fixture.ok_or("replay: a fixture path is required")?,
        verify,
        overrides,
    })
}

fn value<I: Iterator<Item = String>>(args: &mut I, flag: &str) -> Result<String, String> {
    args.next().ok_or_else(|| format!("{flag} wants a value"))
}

/// Unix milliseconds or RFC 3339.
///
/// Both, because the two callers are different people: a timestamp copied out
/// of the journal is milliseconds, and a time remembered from a log line is a
/// date. Digits are unambiguous — no RFC 3339 instant is all digits — so the
/// two can share one argument without a flag to say which.
fn instant(raw: &str) -> Result<Timestamp, String> {
    if raw.chars().all(|c| c.is_ascii_digit()) {
        return raw
            .parse()
            .map(Timestamp::from_millis)
            .map_err(|_| format!("`{raw}` is too large to be unix milliseconds"));
    }
    DateTime::parse_from_rfc3339(raw)
        .map(|dt| Timestamp::from_millis(dt.timestamp_millis()))
        .map_err(|e| format!("`{raw}` is neither unix milliseconds nor RFC 3339: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_args(args: &[&str]) -> Result<Invocation, String> {
        parse(args.iter().map(|s| s.to_string()))
    }

    /// The one behaviour that must never change: this is how systemd starts it.
    #[test]
    fn no_arguments_runs_the_daemon() {
        assert_eq!(parse_args(&[]), Ok(Invocation::Daemon));
    }

    #[test]
    fn export_takes_a_range_and_defaults_the_database() {
        assert_eq!(
            parse_args(&["export", "--from", "1000", "--to", "2000"]),
            Ok(Invocation::Export {
                from: Timestamp::from_millis(1000),
                to: Timestamp::from_millis(2000),
                db: PathBuf::from(DEFAULT_JOURNAL_PATH),
                out: None,
            })
        );
    }

    #[test]
    fn export_accepts_rfc3339_and_millis_interchangeably() {
        let Ok(Invocation::Export { from, to, .. }) = parse_args(&[
            "export",
            "--from",
            "2026-09-12T19:50:00Z",
            "--to",
            "1789242600000",
        ]) else {
            panic!("should parse");
        };
        assert_eq!(from, Timestamp::from_millis(1_789_242_600_000));
        assert_eq!(to, Timestamp::from_millis(1_789_242_600_000));
    }

    #[test]
    fn export_requires_both_ends_and_an_ordered_range() {
        assert!(parse_args(&["export", "--to", "2000"]).is_err());
        assert!(parse_args(&["export", "--from", "1000"]).is_err());
        assert!(parse_args(&["export", "--from", "2000", "--to", "1000"]).is_err());
    }

    #[test]
    fn replay_collects_overrides() {
        assert_eq!(
            parse_args(&["replay", "f.json", "--verify", "--set", "min_soc=40"]),
            Ok(Invocation::Replay {
                fixture: PathBuf::from("f.json"),
                verify: true,
                overrides: vec![("min_soc".to_string(), "40".to_string())],
            })
        );
    }

    /// `--set` without an `=` is a typo, and guessing at it would silently
    /// answer a different question than the one asked.
    #[test]
    fn replay_rejects_a_malformed_override() {
        assert!(parse_args(&["replay", "f.json", "--set", "min_soc"]).is_err());
    }

    #[test]
    fn replay_needs_a_fixture() {
        assert!(parse_args(&["replay"]).is_err());
        assert!(parse_args(&["replay", "a.json", "b.json"]).is_err());
    }

    #[test]
    fn an_unknown_command_is_not_silently_the_daemon() {
        let err = parse_args(&["expport"]).unwrap_err();
        assert!(err.contains("unknown command"), "{err}");
    }

    #[test]
    fn help_is_asked_for_the_three_usual_ways() {
        for arg in ["-h", "--help", "help"] {
            assert_eq!(parse_args(&[arg]), Ok(Invocation::Help), "{arg}");
        }
    }
}
