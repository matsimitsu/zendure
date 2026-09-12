use super::*;

use crate::command::Command;
use crate::fixtures::journey;
use crate::journal::read;
use crate::world::DeviceId;

/// The fixture checked in at `tests/fixtures/journey.json`.
///
/// Strictly a **format** pin: a fixture exported by an older build has to stay
/// readable, so renaming a field of `EngineState`, `SessionConfig`, `Event` or
/// anything they contain fails here. Deliberately *not* a behaviour pin —
/// `a_recording_replays_to_the_commands_it_recorded` owns that, records and
/// replays in one process and so can never go stale, whereas a golden that
/// pinned behaviour would fail on any deliberate change with "regenerate me" as
/// the documented fix, which erases the signal it just gave.
///
/// Regenerate with `cargo test regenerate_the_checked_in_fixture -- --ignored`.
const CHECKED_IN: &str = include_str!("../tests/fixtures/journey.json");

fn config() -> SessionConfig {
    SessionConfig::test_default()
}

/// A fixture built the way `export` builds one, from a recorded journey.
async fn recorded_fixture(dir: &tempfile::TempDir, from: Timestamp, to: Timestamp) -> Fixture {
    let path = dir.path().join("journal.db");
    if !path.exists() {
        crate::journal::testing::record(&path, &journey::session()).await;
    }
    let recording = read::read_range(&path, from, to).unwrap();
    from_recording(recording).unwrap().0
}

fn start() -> Timestamp {
    Timestamp::from_millis(journey::NOW_MS - 1)
}

fn end() -> Timestamp {
    Timestamp::from_millis(journey::NOW_MS + 1_000_000)
}

fn frame(at_ms: i64, commands: &[(&str, Command)]) -> Frame {
    Frame {
        at: Timestamp::from_millis(at_ms),
        directives: commands
            .iter()
            .map(|(id, command)| Directive::Battery {
                device: DeviceId::new(*id),
                command: *command,
            })
            .collect(),
    }
}

#[test]
fn render_names_the_device_on_every_command() {
    let frames = vec![frame(
        1000,
        &[(
            "SN1",
            Command::SetDischarge(crate::units::Setpoint::new(145)),
        )],
    )];
    assert_eq!(render(&frames), "1000ms: SN1 set_discharge(145W)");
}

/// Two devices on one line is the case the device prefix exists for: without
/// it, a setpoint reaching only the primary and a setpoint reaching both would
/// render identically.
#[test]
fn render_lists_every_device_a_step_commanded_in_order() {
    let frames = vec![frame(
        7,
        &[
            ("a-first", Command::SetIdle),
            ("b-second", Command::SetIdle),
        ],
    )];
    assert_eq!(render(&frames), "7ms: a-first set_idle, b-second set_idle");
}

/// An event that decided nothing is a fact about the fold. Rendering it as a
/// blank would make it indistinguishable from a frame that went missing.
#[test]
fn render_marks_a_step_that_commanded_nothing() {
    assert_eq!(render(&[frame(42, &[])]), "42ms: —");
}

/// A replayed `Directive` and a recorded pair of text columns have to render
/// identically or `--verify` diverges on every frame that commanded anything.
/// They go through one function; this is the test that says so.
#[test]
fn a_replayed_directive_and_a_recorded_row_render_the_same() {
    let device = DeviceId::new("SN1");
    let directive = Directive::Battery {
        device: device.clone(),
        command: Command::SetIdle,
    };
    assert_eq!(
        addressed(directive.device(), &directive.describe()),
        addressed(&device, &"set_idle")
    );
}

#[test]
fn replay_produces_one_frame_per_event_including_empty_ones() {
    let events = journey::session();
    let mut engine = Engine::new(
        Controller::from_session(&config(), &journey::clock_at(0)),
        World::new(),
        Duration::from_secs(config().mqtt_timeout_secs),
    );
    let frames = replay(&mut engine, &events);

    assert_eq!(frames.len(), events.len());
    for (frame, event) in frames.iter().zip(&events) {
        assert_eq!(frame.at, event.at());
    }
    // Both `DeviceUpdate`s — the startup seed and the mid-journey one — fold
    // into the world and decide nothing. They are still frames.
    assert!(frames[0].directives.is_empty());
    assert!(frames[4].directives.is_empty());
    assert!(frames.iter().any(|f| !f.directives.is_empty()));
}

#[tokio::test]
async fn a_fixture_round_trips_through_json() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = recorded_fixture(&dir, start(), end()).await;

    let json = serde_json::to_string(&fixture).unwrap();
    let back: Fixture = serde_json::from_str(&json).unwrap();

    assert_eq!(back, fixture);
}

/// **The property step 8 exists for.** Record a run, export it, replay it, and
/// the commands must be the ones the daemon actually issued — compared against
/// the recorded rows, not against a second replay.
#[tokio::test]
async fn a_recording_replays_to_the_commands_it_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = recorded_fixture(&dir, start(), end()).await;
    let frames = run(&fixture, &[]).unwrap();

    verify(&fixture, &frames).expect("a replay of a recording must match it");

    // And the comparison is not trivially true: the journey does command
    // something, so `expected` is not a list of em dashes.
    assert!(
        fixture.expected.iter().any(|line| !line.ends_with(NOTHING)),
        "{:?}",
        fixture.expected
    );
}

/// The anti-vacuity guard. It is *not* redundant with
/// `engine.rs`'s `the_same_journey_diverges_without_the_snapshot`: that one
/// proves `EngineState` carries enough to resume, this one proves `run`
/// actually consults `fixture.seed`. A `run` that ignored the seed entirely
/// would pass every other test in this file.
#[tokio::test]
async fn a_fixture_seeded_from_a_fresh_engine_replays_differently() {
    let dir = tempfile::tempdir().unwrap();

    // Export only the tail, so the seed carries history a fresh engine could
    // not have. The tail opens on a discharge-shaped reading, which is the
    // sharpest case: a controller that just started holds `last_idle_start`, so
    // `min_idle_before_discharge` suppresses it to idle, while one that was
    // already charging is free to turn around.
    let events = journey::session();
    let fixture = recorded_fixture(&dir, events[2].at(), end()).await;

    let seeded = run(&fixture, &[]).unwrap();
    verify(&fixture, &seeded).expect("the tail must verify from its own seed");

    let fresh = Fixture {
        seed: Seed {
            at: fixture.seed.at,
            state: EngineState {
                world: World::new(),
                controller: Controller::from_session(&config(), &journey::clock_at(0)).state(),
                mqtt_timed_out: false,
            },
        },
        ..fixture.clone()
    };
    let unseeded = run(&fresh, &[]).unwrap();

    assert_ne!(
        render(&seeded),
        render(&unseeded),
        "the seed is not affecting the replay, so verifying against it proves nothing"
    );
}

/// Built literally rather than through a recorded journey: changing one integer
/// does not need a temp database and nine journalled events.
fn bare_fixture() -> Fixture {
    Fixture {
        format: FORMAT,
        session: SessionMeta {
            started_at: Timestamp::from_millis(journey::NOW_MS),
            version: "0.0.0-test".to_string(),
            config: config(),
        },
        seed: Seed {
            at: Timestamp::from_millis(journey::NOW_MS),
            state: EngineState {
                world: World::new(),
                controller: Controller::from_session(&config(), &journey::clock_at(0)).state(),
                mqtt_timed_out: false,
            },
        },
        events: vec![Event::MqttTimeout {
            at: journey::clock_at(0),
        }],
        expected: vec![format!("{}ms: {NOTHING}", journey::NOW_MS)],
    }
}

#[test]
fn an_unsupported_format_is_refused() {
    let fixture = Fixture {
        format: FORMAT + 1,
        ..bare_fixture()
    };
    let err = run(&fixture, &[]).unwrap_err();
    assert!(err.contains("format"), "{err}");
}

/// A fixture whose `expected` and events disagree in length is a corrupt
/// fixture, and saying so beats comparing the frames that happen to line up.
#[test]
fn verify_rejects_a_recording_of_a_different_length() {
    let fixture = Fixture {
        expected: vec!["0ms: —".to_string(), "1ms: —".to_string()],
        ..bare_fixture()
    };
    let frames = run(&fixture, &[]).unwrap();
    let err = verify(&fixture, &frames).unwrap_err();
    assert!(err.contains("frames"), "{err}");
}

/// A typo that silently changed nothing would let a what-if quietly answer the
/// original question.
#[test]
fn an_unknown_knob_is_an_error_that_lists_the_real_ones() {
    let err = apply_overrides(&config(), &[("min_sock".into(), "40".into())]).unwrap_err();
    assert!(err.contains("min_sock"), "{err}");
    assert!(err.contains("min_soc"), "{err}");
}

#[test]
fn an_override_replaces_exactly_one_knob() {
    let changed = apply_overrides(&config(), &[("min_soc".into(), "40".into())]).unwrap();

    assert_eq!(changed.min_soc, crate::units::Soc::new(40));
    assert_eq!(
        SessionConfig {
            min_soc: config().min_soc,
            ..changed
        },
        config()
    );
}

/// A value the knob's *type* cannot hold is rejected.
#[test]
fn an_override_of_the_wrong_type_is_refused() {
    assert!(apply_overrides(&config(), &[("min_soc".into(), "later".into())]).is_err());
    assert!(apply_overrides(&config(), &[("min_soc".into(), "-5".into())]).is_err());
    assert!(apply_overrides(&config(), &[("balance_weekday".into(), "Funday".into())]).is_err());
}

/// A value the knob's type *can* hold but its domain cannot is clamped by the
/// constructor, not waved through.
///
/// This is the case the old test's name claimed and did not cover: `1000` is a
/// perfectly good `u32`, and `Soc`'s derived `Deserialize` used to write it
/// straight into the field. `min_soc = Soc(1000)` makes `soc > min_soc` false
/// forever, so a replay answering "why did it never discharge?" answered about
/// a controller that cannot exist.
#[test]
fn an_override_outside_a_knobs_domain_is_clamped_by_its_constructor() {
    let clamped = apply_overrides(&config(), &[("min_soc".into(), "1000".into())]).unwrap();
    assert_eq!(clamped.min_soc, crate::units::Soc::FULL);

    // And the one that inverts a guard rather than saturating it: the README
    // documents a negative solar threshold as "disables the guard", which holds
    // only because the constructor clamps it to the `0` sentinel.
    let off = apply_overrides(
        &config(),
        &[("solar_discharge_block_threshold".into(), "-500".into())],
    )
    .unwrap();
    assert_eq!(
        off.solar_discharge_block_threshold,
        crate::units::SolarPower::ZERO
    );
}

/// `--set` and `--verify` cannot be combined — `cli` rejects that pairing — but
/// `--set` alone has to visibly change the answer or it is doing nothing.
#[tokio::test]
async fn an_override_changes_what_the_replay_decides() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = recorded_fixture(&dir, start(), end()).await;

    let before = run(&fixture, &[]).unwrap();
    let after = run(&fixture, &[("max_soc".into(), "0".into())]).unwrap();

    assert_ne!(render(&before), render(&after));
}

/// A range with nothing in it is an error, not an empty fixture that would
/// `--verify` successfully against nothing.
#[tokio::test]
async fn an_empty_range_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    crate::journal::testing::record(&path, &journey::session()).await;

    let before = Timestamp::from_millis(journey::NOW_MS - 10_000);
    let recording = read::read_range(&path, before, before).unwrap();
    assert!(from_recording(recording).is_err());
}

/// The reader's warnings reach the caller rather than being printed from
/// inside a library function that tests call.
#[tokio::test]
async fn warnings_are_returned_not_printed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    crate::journal::testing::record(&path, &journey::session()).await;

    // A range opening before any decision takes the no-seed path.
    let recording = read::read_range(&path, start(), end()).unwrap();
    let (_, warnings) = from_recording(recording).unwrap();
    assert!(
        warnings.iter().any(|w| w.contains("starting fresh")),
        "{warnings:?}"
    );
}

#[tokio::test]
#[ignore = "rewrites tests/fixtures/journey.json"]
async fn regenerate_the_checked_in_fixture() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    crate::journal::testing::record(&path, &journey::session()).await;

    // Through `export`, so the golden is produced by the path a user runs, and
    // from a *seeded* range so the seed carries a populated world and a
    // controller with history rather than two defaults.
    let events = journey::session();
    crate::commands::export(
        &path,
        events[2].at(),
        end(),
        Some(std::path::Path::new("tests/fixtures/journey.json")),
    )
    .unwrap();
}

/// A fixture written by an earlier build still parses into today's types. The
/// format assertion is part of it: without it, bumping `FORMAT` and
/// regenerating would keep this green while every fixture in the wild broke.
#[test]
fn the_checked_in_fixture_still_parses() {
    let fixture: Fixture = serde_json::from_str(CHECKED_IN).expect("fixture format changed");

    assert_eq!(fixture.format, FORMAT);
    assert!(!fixture.events.is_empty());
    assert_eq!(fixture.expected.len(), fixture.events.len());

    // The seed has to carry a populated world, or renaming a field inside
    // `Measurement` would not be caught here.
    assert!(
        fixture.seed.state.world.battery().is_some(),
        "the golden was exported without a seeded world"
    );
}
