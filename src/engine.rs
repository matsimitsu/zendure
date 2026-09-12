use std::time::Duration;

use crate::allocate::{Directive, allocate};
use crate::battery::BatteryState;
use crate::clock::Clock;
use crate::controller::Controller;
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
#[derive(Debug, Default)]
pub struct Step {
    pub commands: Vec<Directive>,
    pub decision: Option<ControlDecision>,
    pub status: Option<&'static str>,
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
    /// once the world can hold something other than exactly one battery.
    pub fn battery(&self) -> Option<&BatteryState> {
        self.world.battery()
    }

    /// The whole projection, for the caller that wants to record it: the raw
    /// log writes it alongside every decision, so a journal line carries the
    /// inputs the decision was made from and not just its conclusion.
    pub fn world(&self) -> &World {
        &self.world
    }

    pub fn cycle_counts(&self) -> CycleCounts {
        self.controller.cycle_counts()
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

        let decision = self.controller.decide_world(&self.world, at);
        // Allocated here rather than by the caller: how many devices a decision
        // touches is the world's business, and `main.rs` actuating whatever
        // list it is handed is what keeps the dropped-command bug from coming
        // back.
        let commands = decision
            .as_ref()
            .map(|d| allocate(d, &self.world))
            .unwrap_or_default();

        Step {
            commands,
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
        let commands = allocate(&decision, &self.world);

        Step {
            commands,
            decision: Some(decision),
            status: first_tick.then_some("mqtt_timeout"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::Command;
    use crate::units::{BatteryPower, GridPower, PowerCap, Soc, SolarPower, Timestamp};
    use crate::world::{DeviceId, Measurement};
    use chrono::Weekday;

    const NOW_MS: i64 = 1_000_000_000;
    const DAY: u32 = 100;
    const BATTERY_ID: &str = "test-battery";

    fn clock() -> Clock {
        Clock {
            now: Timestamp::from_millis(NOW_MS),
            hour: 12,
            day_ordinal: DAY,
            weekday: Weekday::Wed,
        }
    }

    fn battery() -> BatteryState {
        BatteryState {
            soc: Soc::new(50),
            max_discharge_power: PowerCap::new(800),
            max_charge_power: PowerCap::new(2400),
            current_power: BatteryPower(0),
            soc_calibrating: false,
            soc_limit_reached: false,
            fault: false,
        }
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

        assert_eq!(step.commands, vec![idle_for(BATTERY_ID)]);
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
            step.commands,
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
        assert_eq!(step.commands, vec![idle_for(BATTERY_ID)]);
        assert_eq!(step.decision.unwrap().mode, ControlMode::Idle);
        // ...but the transition is reported only once per outage.
        assert_eq!(step.status, None);
    }

    #[test]
    fn timeout_re_asserts_idle_for_as_long_as_the_outage_lasts() {
        let mut engine = engine();
        for _ in 0..5 {
            let step = engine.step(&Event::MqttTimeout { at: clock() });
            assert_eq!(step.commands, vec![idle_for(BATTERY_ID)]);
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

    /// Superseded in step 7 by: snapshot/restore equivalence
    ///
    /// Two `Engine`s built identically and fed the same events must decide
    /// identically at every step, in order. `step` takes `&self` state and a
    /// borrowed event and nothing else, so this is really a test that no
    /// hidden clock read or thread-local snuck into the decision path — a
    /// leak there would make the two engines agree at first and drift once
    /// their private timers ticked at different wall-clock moments, which a
    /// single-engine test could never expose. Step 7's snapshot/restore test
    /// strengthens this same property by proving it across a process
    /// restart, not just across two in-memory instances.
    ///
    /// The sequence exercises several branches on purpose: a meter reading
    /// that may decide, a device update that never does, a timeout that
    /// forces idle and reports it, a repeat timeout that forces idle again
    /// but stays quiet, and a resuming meter reading that reports again.
    #[test]
    fn the_fold_is_deterministic() {
        let events = [
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
        ];

        let mut engine_a = engine();
        let mut engine_b = engine();

        for event in &events {
            let step_a = engine_a.step(event);
            let step_b = engine_b.step(event);

            assert_eq!(step_a.commands, step_b.commands);
            assert_eq!(
                step_a.decision.as_ref().map(|d| d.mode),
                step_b.decision.as_ref().map(|d| d.mode),
            );
            assert_eq!(
                step_a.decision.as_ref().map(|d| d.power_watts),
                step_b.decision.as_ref().map(|d| d.power_watts),
            );
            assert_eq!(
                step_a.decision.as_ref().map(|d| &d.reason),
                step_b.decision.as_ref().map(|d| &d.reason),
            );
            assert_eq!(step_a.status, step_b.status);
        }
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

        assert!(step.commands.is_empty());
        assert!(step.decision.is_none());
        assert!(step.status.is_none());
        assert_eq!(engine.battery().unwrap().soc, Soc::new(80));
    }
}
