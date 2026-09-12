use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::allocate::{Directive, allocate};
use crate::battery::BatteryState;
use crate::clock::Clock;
use crate::controller::{Controller, ControllerState};
use crate::event::Event;
use crate::models::{ControlDecision, ControlMode, CycleCounts};
use crate::units::{GridPower, Setpoint, SolarPower};
use crate::world::{MeterReading, World};

/// The event-driven fold at the heart of the controller. Owns exactly the
/// state that used to live as locals in `main.rs`'s coordinator loop: the
/// latest view of the world, and whether we're currently standing down on an
/// MQTT timeout.
pub struct Engine {
    controller: Controller,
    world: World,
    mqtt_timed_out: bool,
    mqtt_timeout: Duration,
}

/// What `Engine::step` wants done for one event: the directives to actuate
/// (at most one decision per step, allocated across however many devices the
/// world holds), the decision to publish if the controller made one, and a
/// status transition to publish if the event itself caused one. Actuating a
/// directive and observing whether it succeeded stays outside the engine —
/// that's I/O, done by the caller.
#[derive(Debug, Default, PartialEq)]
pub struct Step {
    pub directives: Vec<Directive>,
    pub decision: Option<ControlDecision>,
    pub status: Option<&'static str>,
}

/// Everything needed to resume the fold from a point in time: the world the
/// next decision reads, the controller's history, and the failsafe latch.
///
/// These are the three pieces that make `step` a fold rather than a function —
/// feed the same event to two engines holding the same `EngineState` and they
/// produce the same `Step`. That is the property the journal exists to preserve
/// across a process restart, and the shape step 8's fixture `seed` is built from.
///
/// `mqtt_timed_out` looks like an implementation detail and is not: it decides
/// whether a timeout tick reports a status transition and whether a resuming
/// meter reading announces `"operational"`. Two engines differing only in this
/// flag produce different steps.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EngineState {
    pub world: World,
    pub controller: ControllerState,
    pub mqtt_timed_out: bool,
}

impl Engine {
    pub fn new(controller: Controller, world: World, mqtt_timeout: Duration) -> Self {
        Self {
            controller,
            world,
            mqtt_timed_out: false,
            mqtt_timeout,
        }
    }

    /// The battery state the engine is currently deciding against, for
    /// callers that just want to log it (e.g. alongside a decision). `None`
    /// only when no device has reported yet — which cannot happen in
    /// production, since `main.rs` seeds the world from a startup poll that
    /// fails startup on error.
    pub fn battery(&self) -> Option<&BatteryState> {
        self.world.battery()
    }

    pub fn cycle_counts(&self) -> CycleCounts {
        self.controller.cycle_counts()
    }

    /// Snapshot the fold. Recorded with every decision so the journal can seed a
    /// replay from any decision row.
    pub fn state(&self) -> EngineState {
        EngineState {
            world: self.world.clone(),
            controller: self.controller.state(),
            mqtt_timed_out: self.mqtt_timed_out,
        }
    }

    /// Resume from a snapshot. The controller's *configuration* comes from
    /// whatever this engine was constructed with, not from the snapshot — see
    /// [`Controller::restore`].
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn restore(&mut self, state: EngineState) {
        let EngineState {
            world,
            controller,
            mqtt_timed_out,
        } = state;
        self.world = world;
        self.controller.restore(controller);
        self.mqtt_timed_out = mqtt_timed_out;
    }

    pub fn step(&mut self, event: &Event) -> Step {
        match event {
            Event::Meter { at, grid, solar } => self.step_meter(at, *grid, *solar),
            Event::DeviceUpdate {
                id, measurement, ..
            } => {
                self.world.observe_device(id.clone(), measurement.clone());
                Step::default()
            }
            Event::MqttTimeout { .. } => self.step_mqtt_timeout(),
        }
    }

    fn step_meter(&mut self, at: &Clock, grid: MeterReading, solar: SolarPower) -> Step {
        let mut status = None;
        if self.mqtt_timed_out {
            self.mqtt_timed_out = false;
            status = Some("operational");
        }

        // Fold before deciding, not as an argument to the decision: the meter
        // reading has to outlive this step so a `DeviceUpdate` arriving in
        // between doesn't leave the world with a stale grid figure.
        self.world.observe_meter(grid, solar);

        let decision = self.controller.decide(&self.world, at);
        // Allocated here rather than by the caller: how many devices a decision
        // touches is the world's business, and `main.rs` actuating whatever
        // list it is handed is what keeps the dropped-command bug from coming
        // back.
        let directives = decision
            .as_ref()
            .map(|d| allocate(d, &self.world))
            .unwrap_or_default();

        Step {
            directives,
            decision,
            status,
        }
    }

    /// Latches the *reporting*, not the command. Idle is re-asserted on every
    /// timeout tick because the engine cannot know whether the write landed:
    /// latching on the decision meant a single failed `apply_command` left the
    /// device running its last command for the rest of the outage, with the
    /// controller believing it had stood down. The two failures correlate in
    /// practice — whatever kills the meter feed often takes the battery's
    /// network with it.
    ///
    /// `status` is first-tick-only, so the warning and the `mqtt_timeout`
    /// transition are logged once per outage rather than once per interval.
    fn step_mqtt_timeout(&mut self) -> Step {
        let first_tick = !self.mqtt_timed_out;
        self.mqtt_timed_out = true;

        let decision = ControlDecision {
            mode: ControlMode::Idle,
            power_watts: Setpoint::ZERO,
            reason: format!(
                "MQTT timeout: no updates for {}s",
                self.mqtt_timeout.as_secs(),
            ),
            grid_power: GridPower::ZERO,
        };
        // Through `allocate` like any other decision, so the failsafe idles
        // *every* battery. A hand-built `vec![command]` here would stand one
        // box down and leave the others running during the exact outage the
        // failsafe exists for.
        let directives = allocate(&decision, &self.world);

        Step {
            directives,
            decision: Some(decision),
            status: first_tick.then_some("mqtt_timeout"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::Command;
    use crate::event::journey::{BATTERY_ID, DAY, NOW_MS};
    use crate::units::{GridPower, Soc, SolarPower};
    use crate::world::{DeviceId, Measurement};

    fn clock() -> Clock {
        // `DAY` spelled out rather than relying on the shared fixture happening
        // to use the same ordinal: `Controller::test_default(NOW_MS, DAY)` below
        // has to agree with it, or the midnight reset fires on the first step.
        Clock {
            day_ordinal: DAY,
            ..Clock::test_at(NOW_MS)
        }
    }

    fn battery() -> BatteryState {
        BatteryState::test_sample()
    }

    fn world() -> World {
        let mut world = World::new();
        world.observe_device(DeviceId::new(BATTERY_ID), Measurement::Battery(battery()));
        world
    }

    /// The one-battery world's idle directive — what every assertion below
    /// used to spell as a bare `Command::SetIdle`.
    fn idle_for(id: &str) -> Directive {
        Directive::Battery {
            device: DeviceId::new(id),
            command: Command::SetIdle,
        }
    }

    fn meter(total: f64) -> MeterReading {
        MeterReading::total_only(GridPower(total))
    }

    fn engine() -> Engine {
        Engine::new(
            Controller::test_default(NOW_MS, DAY),
            world(),
            Duration::from_secs(120),
        )
    }

    #[test]
    fn mqtt_timeout_forces_idle() {
        let mut engine = engine();
        let step = engine.step(&Event::MqttTimeout { at: clock() });

        assert_eq!(step.directives, vec![idle_for(BATTERY_ID)]);
        assert_eq!(step.decision.unwrap().mode, ControlMode::Idle);
        assert_eq!(step.status, Some("mqtt_timeout"));
    }

    /// The failsafe is a property of the outage, not of one box: every battery
    /// in the world is stood down, not just whichever one happens to sort
    /// first. This is the case the old `commands.first()` actuation could not
    /// have served even if the engine had produced it.
    #[test]
    fn mqtt_timeout_idles_every_battery() {
        let mut world = World::new();
        world.observe_device(DeviceId::new("battery-a"), Measurement::Battery(battery()));
        world.observe_device(DeviceId::new("battery-b"), Measurement::Battery(battery()));
        let mut engine = Engine::new(
            Controller::test_default(NOW_MS, DAY),
            world,
            Duration::from_secs(120),
        );

        let step = engine.step(&Event::MqttTimeout { at: clock() });

        assert_eq!(
            step.directives,
            vec![idle_for("battery-a"), idle_for("battery-b")],
        );
    }

    #[test]
    fn repeated_timeout_re_asserts_idle_but_reports_once() {
        let mut engine = engine();
        engine.step(&Event::MqttTimeout { at: clock() });
        let step = engine.step(&Event::MqttTimeout { at: clock() });

        // The command keeps being issued: the engine never learns whether the
        // first write landed, so a failed one must not disable the failsafe.
        assert_eq!(step.directives, vec![idle_for(BATTERY_ID)]);
        assert_eq!(step.decision.unwrap().mode, ControlMode::Idle);
        // ...but the transition is reported only once per outage.
        assert_eq!(step.status, None);
    }

    #[test]
    fn timeout_re_asserts_idle_for_as_long_as_the_outage_lasts() {
        let mut engine = engine();
        for _ in 0..5 {
            let step = engine.step(&Event::MqttTimeout { at: clock() });
            assert_eq!(step.directives, vec![idle_for(BATTERY_ID)]);
        }
    }

    #[test]
    fn status_is_reported_again_after_a_resume() {
        let mut engine = engine();
        engine.step(&Event::MqttTimeout { at: clock() });
        engine.step(&Event::MqttTimeout { at: clock() });
        engine.step(&Event::Meter {
            at: clock(),
            grid: meter(500.0),
            solar: SolarPower::new(0.0),
        });

        // A second outage is a new episode, so it announces itself again.
        let step = engine.step(&Event::MqttTimeout { at: clock() });
        assert_eq!(step.status, Some("mqtt_timeout"));
    }

    #[test]
    fn grid_reading_after_timeout_restores_operational() {
        let mut engine = engine();
        engine.step(&Event::MqttTimeout { at: clock() });

        let step = engine.step(&Event::Meter {
            at: clock(),
            grid: meter(500.0),
            solar: SolarPower::new(0.0),
        });

        assert_eq!(step.status, Some("operational"));
    }

    #[test]
    fn grid_reading_without_prior_timeout_has_no_status() {
        let mut engine = engine();
        let step = engine.step(&Event::Meter {
            at: clock(),
            grid: meter(500.0),
            solar: SolarPower::new(0.0),
        });

        assert_eq!(step.status, None);
    }

    /// Two `Engine`s built identically and fed the same event sequence, one
    /// run to completion before the other starts, must produce identical
    /// steps at every position. `step` takes `&mut self` and a borrowed event
    /// and nothing else — no `Clock::now` call, no `static mut`, no
    /// thread-local — so the only thing that can vary between the runs is state
    /// the engine is carrying that it should not be.
    ///
    /// It does **not** catch an engine that secretly reads ambient time, and an
    /// earlier version of this comment claimed it did. The two runs are five
    /// `step` calls apart — microseconds — while `Clock` has millisecond
    /// resolution, so a hidden `Utc::now()` would very likely read the same
    /// value twice and pass. Making that true would need a deliberate sleep,
    /// which is not worth a second of test time; what rules it out is that
    /// `step`'s signature gives it nothing to read.
    ///
    /// Comparing whole `Step`s (via `Step`'s and `ControlDecision`'s derived
    /// `PartialEq`) rather than picking out individual fields means a field
    /// `ControlDecision` gains later is covered here automatically, with no
    /// need to remember to add it to this test.
    ///
    /// The sequence exercises several branches on purpose: a meter reading
    /// that may decide, a device update that never does, a timeout that
    /// forces idle and reports it, a repeat timeout that forces idle again
    /// but stays quiet, and a resuming meter reading that reports again.
    #[test]
    fn the_fold_is_deterministic() {
        fn events() -> [Event; 5] {
            [
                Event::Meter {
                    at: clock(),
                    grid: meter(500.0),
                    solar: SolarPower::new(0.0),
                },
                Event::DeviceUpdate {
                    at: clock(),
                    id: DeviceId::new(BATTERY_ID),
                    measurement: Measurement::Battery(battery()),
                },
                Event::MqttTimeout { at: clock() },
                Event::MqttTimeout { at: clock() },
                Event::Meter {
                    at: clock(),
                    grid: meter(500.0),
                    solar: SolarPower::new(0.0),
                },
            ]
        }

        let mut engine_a = engine();
        let steps_a: Vec<Step> = events().iter().map(|event| engine_a.step(event)).collect();

        let mut engine_b = engine();
        let steps_b: Vec<Step> = events().iter().map(|event| engine_b.step(event)).collect();

        assert_eq!(steps_a, steps_b);
    }

    /// A clock `secs` after `NOW_MS`, so a sequence can actually advance the
    /// controller's timers. `clock()` alone holds time still, which is fine for
    /// single-step tests and useless for a fold.
    /// Split after the timeout, so the second half opens with the engine
    /// latched into the failsafe and the first meter reading after the boundary
    /// has to produce the `"operational"` transition.
    const SPLIT: usize = 5;

    /// **The property the journal exists for.** A decision is a fold over
    /// everything that came before it, so a controller restarted mid-stream
    /// either resumes the fold exactly or silently becomes a different
    /// controller that happens to share a config file.
    ///
    /// Engine A runs the whole journey in one process. Engine C is a *fresh*
    /// engine — built from the same config but with none of the history — that
    /// is handed A's snapshot at the split point and runs the rest. Every step
    /// after the boundary has to match, including the status transitions, which
    /// is what proves `mqtt_timed_out` came across with everything else.
    #[test]
    fn a_restored_engine_resumes_the_fold_exactly() {
        let events = crate::event::journey::events();

        let mut continuous = engine();
        let expected: Vec<Step> = events.iter().map(|e| continuous.step(e)).collect();

        let mut recorded = engine();
        let snapshot = events[..SPLIT]
            .iter()
            .map(|e| recorded.step(e))
            .last()
            .map(|_| recorded.state())
            .expect("split is inside the journey");

        // The snapshot goes through JSON, because that is how it reaches the
        // journal — an `EngineState` that only survives in memory is not a
        // recovery story.
        let json = serde_json::to_string(&snapshot).unwrap();
        let restored: EngineState = serde_json::from_str(&json).unwrap();

        let mut resumed = engine();
        resumed.restore(restored);

        let actual: Vec<Step> = events[SPLIT..].iter().map(|e| resumed.step(e)).collect();
        assert_eq!(actual, expected[SPLIT..]);

        // Equal steps are necessary and not sufficient — the two engines also
        // have to arrive at the same place, or the next event diverges. Field
        // coverage of the snapshot itself is not this test's job and it cannot
        // do it: see `state_and_restore_are_exact_inverses` in `controller.rs`.
        assert_eq!(resumed.state(), continuous.state());
    }

    /// Guards the test above against passing for the wrong reason. If a fresh
    /// engine produced the same steps anyway, the journey would not be
    /// exercising any state and the equivalence would be vacuous.
    #[test]
    fn the_same_journey_diverges_without_the_snapshot() {
        let events = crate::event::journey::events();

        let mut continuous = engine();
        let expected: Vec<Step> = events.iter().map(|e| continuous.step(e)).collect();

        let mut cold = engine();
        let actual: Vec<Step> = events[SPLIT..].iter().map(|e| cold.step(e)).collect();

        assert_ne!(actual, expected[SPLIT..]);
    }

    #[test]
    fn battery_update_refreshes_state_without_a_decision() {
        let mut engine = engine();
        let mut updated = battery();
        updated.soc = Soc::new(80);

        let step = engine.step(&Event::DeviceUpdate {
            at: clock(),
            id: DeviceId::new(BATTERY_ID),
            measurement: Measurement::Battery(updated),
        });

        assert!(step.directives.is_empty());
        assert!(step.decision.is_none());
        assert!(step.status.is_none());
        assert_eq!(engine.battery().unwrap().soc, Soc::new(80));
    }
}
