use super::*;

use crate::allocate::allocate;
use crate::battery::BatteryState;
use crate::clock::Clock;
use crate::command::Command;
use crate::device::{Applied, ControlPath, Outcome};
use crate::event::journey;
use crate::journal::{self, Journal};
use crate::models::ControlDecision;
use crate::units::{RetentionDays, Setpoint};
use crate::world::{DeviceId, Measurement};

/// `main.rs`'s startup seed: the battery the journey's world contains, arriving
/// as the event the daemon now journals it as.
fn startup() -> Event {
    Event::DeviceUpdate {
        at: journey::clock_at(0),
        id: DeviceId::new(journey::BATTERY_ID),
        measurement: Measurement::Battery(BatteryState::test_sample()),
    }
}

/// The journey with that seed in front of it — the whole recorded stream, as a
/// session actually produces it.
fn session_events() -> Vec<Event> {
    std::iter::once(startup())
        .chain(journey::events())
        .collect()
}

fn config() -> SessionConfig {
    SessionConfig::test_default()
}

/// What `main.rs` builds at startup: a controller with no history, from the
/// tuning alone. Deliberately not `Controller::test_default`, whose invented
/// history no daemon ever has — a recording made against it could not be
/// replayed from the start, because the state it began in was never recorded
/// anywhere.
fn engine() -> Engine {
    Engine::new(
        Controller::from_session(&config(), &journey::clock_at(0)),
        World::new(),
        Duration::from_secs(config().mqtt_timeout_secs),
    )
}

/// Everything `main.rs`'s loop does to the journal, minus the I/O.
///
/// Journal the event, step, then journal the decision with the state *after*
/// the step and one outcome per directive — in that order, because that
/// ordering is what `seq` alignment depends on and a test that wrote them in
/// any other order would be testing a daemon that does not exist.
async fn record(path: &std::path::Path, events: &[Event]) {
    let (j, writer) = Journal::open(
        path,
        RetentionDays::new(3650).unwrap(),
        "0.0.0-test",
        &config(),
    );
    let writer = writer.expect("journal opens in a temp dir");
    let mut engine = engine();

    for event in events {
        j.event(event);
        let step = engine.step(event);
        if let Some(decision) = step.decision {
            let outcomes: Vec<Outcome> = step
                .directives
                .iter()
                .map(|d| Outcome {
                    device: d.device().clone(),
                    command: d.describe(),
                    applied: Applied::Ok,
                    error: None,
                })
                .collect();
            j.decision(
                event.at(),
                ControlPath::Objective,
                &decision,
                &engine.state(),
                &outcomes,
            );
        }
    }

    drop(j);
    writer.await.expect("writer panicked");
}

/// The fixture checked in at `tests/fixtures/journey.json`, which is the
/// on-disk format itself under test: renaming a field of `EngineState` or
/// `SessionConfig` would make this file unreadable, and a fixture exported
/// today has to stay readable by tomorrow's build. Regenerate with
/// `cargo test regenerate_the_checked_in_fixture -- --ignored`.
const CHECKED_IN: &str = include_str!("../tests/fixtures/journey.json");

#[tokio::test]
#[ignore = "rewrites tests/fixtures/journey.json"]
async fn regenerate_the_checked_in_fixture() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    record(&path, &session_events()).await;
    let fixture = from_slice(journal::read_range(&path, start(), end()).unwrap()).unwrap();
    std::fs::write(
        "tests/fixtures/journey.json",
        serde_json::to_string_pretty(&fixture).unwrap() + "\n",
    )
    .unwrap();
}

/// A fixture written by an earlier build still replays, and still agrees with
/// what that build recorded. This is the only test that would fail on a change
/// to the *format* rather than to the fold.
#[test]
fn the_checked_in_fixture_still_replays_and_verifies() {
    let fixture: Fixture = serde_json::from_str(CHECKED_IN).expect("fixture format changed");
    let frames = run(&fixture, &[]).unwrap();
    verify(&fixture, &frames).expect("the fold changed, or the fixture is stale");
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
        &[("SN1", Command::SetDischarge(Setpoint::new(145)))],
    )];
    assert_eq!(render(&frames), "1000ms: SN1 set_discharge(145W)");
}

/// Two devices on one line is the case the device prefix exists for: without
/// it, a setpoint reaching only the primary and a setpoint reaching both would
/// render identically.
#[test]
fn render_lists_every_device_a_step_commanded() {
    let frames = vec![frame(
        7,
        &[("SN1", Command::SetIdle), ("SN2", Command::SetIdle)],
    )];
    assert_eq!(render(&frames), "7ms: SN1 set_idle, SN2 set_idle");
}

/// An event that decided nothing is a fact about the fold. Rendering it as a
/// blank would make it indistinguishable from a frame that went missing.
#[test]
fn render_marks_a_step_that_commanded_nothing() {
    assert_eq!(render(&[frame(42, &[])]), "42ms: —");
}

#[test]
fn replay_produces_one_frame_per_event_including_empty_ones() {
    let events = session_events();
    let frames = replay(&mut engine(), &events);

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
    let path = dir.path().join("journal.db");
    record(&path, &session_events()).await;

    let fixture = from_slice(journal::read_range(&path, start(), end()).unwrap()).unwrap();
    let json = serde_json::to_string(&fixture).unwrap();
    let back: Fixture = serde_json::from_str(&json).unwrap();

    assert_eq!(back, fixture);
}

fn start() -> Timestamp {
    Timestamp::from_millis(journey::NOW_MS - 1)
}

fn end() -> Timestamp {
    Timestamp::from_millis(journey::NOW_MS + 1_000_000)
}

/// **The property step 8 exists for.** Record a run, export it, replay it, and
/// the commands must be the ones the daemon actually issued — compared against
/// the recorded rows, not against a second replay.
#[tokio::test]
async fn a_recording_replays_to_the_commands_it_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    record(&path, &session_events()).await;

    let fixture = from_slice(journal::read_range(&path, start(), end()).unwrap()).unwrap();
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

/// The anti-vacuity guard, mirroring `the_same_journey_diverges_without_the_snapshot`.
///
/// A `--verify` that quietly ignored the seed and started from a fresh engine
/// would still pass every test above, because the journey begins at a moment a
/// fresh controller can reach. Replacing the seed with a fresh engine's state
/// has to change the answer, or the seed is not under test.
#[tokio::test]
async fn a_fixture_seeded_from_a_fresh_engine_replays_differently() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");

    // Record the whole journey, then export only its tail, so the fixture's
    // seed carries history a fresh engine could not have. The tail opens on a
    // discharge, which is the sharpest case: a controller that just started
    // holds `last_idle_start`, so `min_idle_before_discharge` blocks it, while
    // one that was already charging is free to turn around.
    let events = session_events();
    record(&path, &events).await;
    let tail = events[2].at();
    let fixture = from_slice(journal::read_range(&path, tail, end()).unwrap()).unwrap();

    let seeded = run(&fixture, &[]).unwrap();
    verify(&fixture, &seeded).expect("the tail must verify from its own seed");

    let fresh = Fixture {
        seed: Seed {
            at_ms: fixture.seed.at_ms,
            state: engine().state(),
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

#[tokio::test]
async fn an_unsupported_format_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    record(&path, &session_events()).await;

    let mut fixture = from_slice(journal::read_range(&path, start(), end()).unwrap()).unwrap();
    fixture.format = FORMAT + 1;

    let err = run(&fixture, &[]).unwrap_err();
    assert!(err.contains("format"), "{err}");
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

/// A value the knob's type cannot hold is rejected rather than coerced.
#[test]
fn an_override_that_does_not_fit_its_knob_is_refused() {
    assert!(apply_overrides(&config(), &[("min_soc".into(), "later".into())]).is_err());
}

/// The point of `--set`: the same events, different tuning, a visibly different
/// answer. `max_soc` at 0 blocks charging outright, and the journey charges.
#[tokio::test]
async fn an_override_changes_what_the_replay_decides() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    record(&path, &session_events()).await;

    let fixture = from_slice(journal::read_range(&path, start(), end()).unwrap()).unwrap();
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
    record(&path, &session_events()).await;

    let before_everything = Timestamp::from_millis(journey::NOW_MS - 10_000);
    let slice = journal::read_range(&path, before_everything, before_everything).unwrap();
    assert!(from_slice(slice).is_err());
}

/// The seed comes from a decision row, so the exported range starts at that
/// decision and not at `--from`. Asking from the middle of the journey has to
/// produce a fixture whose first event is at or before the requested start.
#[tokio::test]
async fn a_range_is_anchored_to_the_last_decision_before_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    let events = session_events();
    record(&path, &events).await;

    let from = events[7].at();
    let fixture = from_slice(journal::read_range(&path, from, end()).unwrap()).unwrap();

    assert!(
        fixture.seed.at_ms <= from.as_millis(),
        "seeded at {}ms, asked from {}ms",
        fixture.seed.at_ms,
        from.as_millis()
    );
    assert_eq!(fixture.events.first().map(Event::at), Some(events[8].at()));
}

/// A fixture whose `expected` and events disagree in length is a corrupt
/// fixture, and saying so beats comparing the frames that happen to line up.
#[test]
fn verify_rejects_a_recording_of_a_different_length() {
    let fixture = Fixture {
        format: FORMAT,
        session: SessionMeta {
            started_at_ms: 0,
            version: "0.0.0-test".to_string(),
            config: config(),
        },
        seed: Seed {
            at_ms: 0,
            state: engine().state(),
        },
        events: vec![Event::MqttTimeout {
            at: Clock::test_at(journey::NOW_MS),
        }],
        expected: vec!["0ms: —".to_string(), "1ms: —".to_string()],
    };

    let frames = run(&fixture, &[]).unwrap();
    let err = verify(&fixture, &frames).unwrap_err();
    assert!(err.contains("frames"), "{err}");
}

/// Allocation order follows the world's device order, which is sorted by id —
/// so the same fixture renders the same lines in the same sequence every run,
/// which is what makes a diff meaningful.
#[test]
fn a_multi_device_step_renders_in_a_stable_order() {
    let mut world = World::new();
    for id in ["b-second", "a-first"] {
        world.observe_device(
            DeviceId::new(id),
            Measurement::Battery(BatteryState::test_sample()),
        );
    }
    let decision = ControlDecision {
        mode: crate::models::ControlMode::Idle,
        power_watts: Setpoint::ZERO,
        reason: "test".to_string(),
        grid_power: crate::units::GridPower::ZERO,
    };

    let frames = vec![Frame {
        at: Timestamp::from_millis(5),
        directives: allocate(&decision, &world),
    }];
    assert_eq!(render(&frames), "5ms: a-first set_idle, b-second set_idle");
}
