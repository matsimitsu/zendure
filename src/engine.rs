use std::time::Duration;

use crate::battery::BatteryState;
use crate::clock::Clock;
use crate::command::Command;
use crate::controller::Controller;
use crate::event::Event;
use crate::models::{ControlDecision, ControlMode, CycleCounts};
use crate::units::{GridPower, Setpoint, SolarPower};

/// The event-driven fold at the heart of the controller. Owns exactly the
/// state that used to live as locals in `main.rs`'s coordinator loop: the
/// latest battery reading, and whether we're currently standing down on an
/// MQTT timeout.
pub struct Engine {
    controller: Controller,
    battery: BatteryState,
    mqtt_timed_out: bool,
    mqtt_timeout: Duration,
}

/// What `Engine::step` wants done for one event: the command(s) to actuate
/// (zero or one — never more than one decision per step), the decision to
/// publish if the controller made one, and a status transition to publish if
/// the event itself caused one. Actuating a command and observing whether it
/// succeeded stays outside the engine — that's I/O, done by the caller.
#[derive(Debug, Default)]
pub struct Step {
    pub commands: Vec<Command>,
    pub decision: Option<ControlDecision>,
    pub status: Option<&'static str>,
}

impl Engine {
    pub fn new(controller: Controller, battery: BatteryState, mqtt_timeout: Duration) -> Self {
        Self {
            controller,
            battery,
            mqtt_timed_out: false,
            mqtt_timeout,
        }
    }

    /// The battery state the engine is currently deciding against, for
    /// callers that just want to log it (e.g. alongside a decision).
    pub fn battery(&self) -> &BatteryState {
        &self.battery
    }

    pub fn cycle_counts(&self) -> CycleCounts {
        self.controller.cycle_counts()
    }

    pub fn step(&mut self, event: &Event) -> Step {
        match event {
            Event::GridPower { at, total, solar } => self.step_grid_power(at, *total, *solar),
            Event::BatteryUpdate { state, .. } => {
                self.battery = state.clone();
                Step::default()
            }
            Event::MqttTimeout { .. } => self.step_mqtt_timeout(),
        }
    }

    fn step_grid_power(&mut self, at: &Clock, total: GridPower, solar: SolarPower) -> Step {
        let mut status = None;
        if self.mqtt_timed_out {
            self.mqtt_timed_out = false;
            status = Some("operational");
        }

        let decision = self.controller.decide(total, solar, &self.battery, at);
        let commands = decision.iter().map(Command::from).collect();

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
        let command = Command::from(&decision);

        Step {
            commands: vec![command],
            decision: Some(decision),
            status: first_tick.then_some("mqtt_timeout"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::units::{BatteryPower, GridPower, PowerCap, Soc, SolarPower, Timestamp};
    use chrono::Weekday;

    const NOW_MS: i64 = 1_000_000_000;
    const DAY: u32 = 100;

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

    fn engine() -> Engine {
        Engine::new(
            Controller::test_default(NOW_MS, DAY),
            battery(),
            Duration::from_secs(120),
        )
    }

    #[test]
    fn mqtt_timeout_forces_idle() {
        let mut engine = engine();
        let step = engine.step(&Event::MqttTimeout { at: clock() });

        assert_eq!(step.commands, vec![Command::SetIdle]);
        assert_eq!(step.decision.unwrap().mode, ControlMode::Idle);
        assert_eq!(step.status, Some("mqtt_timeout"));
    }

    #[test]
    fn repeated_timeout_re_asserts_idle_but_reports_once() {
        let mut engine = engine();
        engine.step(&Event::MqttTimeout { at: clock() });
        let step = engine.step(&Event::MqttTimeout { at: clock() });

        // The command keeps being issued: the engine never learns whether the
        // first write landed, so a failed one must not disable the failsafe.
        assert_eq!(step.commands, vec![Command::SetIdle]);
        assert_eq!(step.decision.unwrap().mode, ControlMode::Idle);
        // ...but the transition is reported only once per outage.
        assert_eq!(step.status, None);
    }

    #[test]
    fn timeout_re_asserts_idle_for_as_long_as_the_outage_lasts() {
        let mut engine = engine();
        for _ in 0..5 {
            let step = engine.step(&Event::MqttTimeout { at: clock() });
            assert_eq!(step.commands, vec![Command::SetIdle]);
        }
    }

    #[test]
    fn status_is_reported_again_after_a_resume() {
        let mut engine = engine();
        engine.step(&Event::MqttTimeout { at: clock() });
        engine.step(&Event::MqttTimeout { at: clock() });
        engine.step(&Event::GridPower {
            at: clock(),
            total: GridPower(500.0),
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

        let step = engine.step(&Event::GridPower {
            at: clock(),
            total: GridPower(500.0),
            solar: SolarPower::new(0.0),
        });

        assert_eq!(step.status, Some("operational"));
    }

    #[test]
    fn grid_reading_without_prior_timeout_has_no_status() {
        let mut engine = engine();
        let step = engine.step(&Event::GridPower {
            at: clock(),
            total: GridPower(500.0),
            solar: SolarPower::new(0.0),
        });

        assert_eq!(step.status, None);
    }

    #[test]
    fn battery_update_refreshes_state_without_a_decision() {
        let mut engine = engine();
        let mut updated = battery();
        updated.soc = Soc::new(80);

        let step = engine.step(&Event::BatteryUpdate {
            at: clock(),
            state: updated,
        });

        assert!(step.commands.is_empty());
        assert!(step.decision.is_none());
        assert!(step.status.is_none());
        assert_eq!(engine.battery().soc, Soc::new(80));
    }
}
