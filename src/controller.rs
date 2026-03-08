use std::time::{Duration, Instant};

use chrono::{Local, Timelike};

use crate::battery::BatteryState;
use crate::models::{ControlDecision, ControlMode};

const MAX_CHARGE_POWER: i32 = 2400;
const MIN_POWER_THRESHOLD: i32 = 50;
const DISCHARGE_START_HOUR: u32 = 17;
const DISCHARGE_END_HOUR: u32 = 7;

/// Minimum time the battery must stay in a mode before switching between
/// charge and discharge. Switching to/from idle is always allowed.
const MIN_MODE_DURATION: Duration = Duration::from_secs(5 * 60);

pub struct Controller {
    last_mode: ControlMode,
    last_mode_change: Instant,
    min_mode_duration: Duration,
}

impl Controller {
    pub fn new() -> Self {
        Self {
            last_mode: ControlMode::Idle,
            // Allow immediate first transition
            last_mode_change: Instant::now() - MIN_MODE_DURATION,
            min_mode_duration: MIN_MODE_DURATION,
        }
    }

    pub fn decide(
        &mut self,
        grid_power: f64,
        solar_power: f64,
        battery: &BatteryState,
    ) -> ControlDecision {
        let hour = Local::now().hour();
        self.decide_at_hour(grid_power, solar_power, battery, hour)
    }

    fn decide_at_hour(
        &mut self,
        grid_power: f64,
        solar_power: f64,
        battery: &BatteryState,
        hour: u32,
    ) -> ControlDecision {
        let raw = raw_decide(grid_power, solar_power, battery, hour);
        self.apply_cooldown(raw)
    }

    fn apply_cooldown(&mut self, decision: ControlDecision) -> ControlDecision {
        let dominated = is_opposing_switch(self.last_mode, decision.mode);

        if dominated && self.last_mode_change.elapsed() < self.min_mode_duration {
            // Suppress charge↔discharge toggle — go idle instead
            let idle = ControlDecision {
                mode: ControlMode::Idle,
                power_watts: 0,
                reason: format!(
                    "Cooldown: suppressed {} (was {} for {:.0}s, min {}s)",
                    decision.mode,
                    self.last_mode,
                    self.last_mode_change.elapsed().as_secs_f64(),
                    self.min_mode_duration.as_secs(),
                ),
                grid_power: decision.grid_power,
                solar_power: decision.solar_power,
            };
            // Don't update last_mode — we're waiting for the cooldown to expire
            return idle;
        }

        if decision.mode != self.last_mode {
            self.last_mode = decision.mode;
            self.last_mode_change = Instant::now();
        }

        decision
    }
}

/// Returns true if switching from `prev` to `next` is a charge↔discharge
/// toggle that should be rate-limited.
fn is_opposing_switch(prev: ControlMode, next: ControlMode) -> bool {
    matches!(
        (prev, next),
        (ControlMode::Charge, ControlMode::Discharge)
            | (ControlMode::Discharge, ControlMode::Charge)
    )
}

/// Stateless decision logic, without cooldown protection.
fn raw_decide(
    grid_power: f64,
    solar_power: f64,
    battery: &BatteryState,
    hour: u32,
) -> ControlDecision {
    // Battery full — don't charge
    if battery.soc >= 100 {
        return discharge_or_idle(
            grid_power,
            solar_power,
            battery,
            hour,
            "Battery full (100%)",
        );
    }

    // If we're exporting to the grid, we have excess solar — charge the battery
    if grid_power < -(MIN_POWER_THRESHOLD as f64) {
        let excess = (-grid_power) as i32;
        let charge_power = excess.min(MAX_CHARGE_POWER).min(battery.max_charge_power);
        return ControlDecision {
            mode: ControlMode::Charge,
            power_watts: charge_power,
            reason: format!("Solar excess: exporting {excess}W to grid"),
            grid_power,
            solar_power,
        };
    }

    discharge_or_idle(grid_power, solar_power, battery, hour, "")
}

fn discharge_or_idle(
    grid_power: f64,
    solar_power: f64,
    battery: &BatteryState,
    hour: u32,
    extra_reason: &str,
) -> ControlDecision {
    let is_discharge_period = !(DISCHARGE_END_HOUR..DISCHARGE_START_HOUR).contains(&hour);

    if is_discharge_period && grid_power > MIN_POWER_THRESHOLD as f64 {
        let demand = grid_power as i32;
        let discharge_power = demand.min(battery.max_discharge_power);
        let mut reason = format!("Discharge period (hour {hour}): grid demand {demand}W");
        if !extra_reason.is_empty() {
            reason = format!("{extra_reason}; {reason}");
        }
        return ControlDecision {
            mode: ControlMode::Discharge,
            power_watts: discharge_power,
            reason,
            grid_power,
            solar_power,
        };
    }

    let base = format!(
        "No action needed (grid: {grid_power:.0}W, solar: {solar_power:.0}W, hour: {hour})"
    );
    let reason = if extra_reason.is_empty() {
        base
    } else {
        format!("{extra_reason}; {base}")
    };

    ControlDecision {
        mode: ControlMode::Idle,
        power_watts: 0,
        reason,
        grid_power,
        solar_power,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn battery(soc: u32) -> BatteryState {
        BatteryState {
            soc,
            max_discharge_power: 800,
            max_charge_power: 2400,
        }
    }

    /// Build a controller that has been in `mode` for the given duration.
    fn controller_in_mode(mode: ControlMode, elapsed: Duration) -> Controller {
        Controller {
            last_mode: mode,
            last_mode_change: Instant::now() - elapsed,
            min_mode_duration: MIN_MODE_DURATION,
        }
    }

    /// Build a controller with no cooldown (for tests that only care about raw logic).
    fn controller_no_cooldown() -> Controller {
        Controller {
            last_mode: ControlMode::Idle,
            last_mode_change: Instant::now() - MIN_MODE_DURATION,
            min_mode_duration: Duration::ZERO,
        }
    }

    // --- Raw decision logic tests ---

    #[test]
    fn soc_100_never_charges() {
        let mut ctrl = controller_no_cooldown();
        let decision = ctrl.decide_at_hour(-500.0, 600.0, &battery(100), 20);
        assert_ne!(decision.mode, ControlMode::Charge);
        assert!(decision.reason.contains("Battery full"));
    }

    #[test]
    fn soc_100_can_still_discharge() {
        let mut ctrl = controller_no_cooldown();
        // Hour 20 is in discharge period
        let decision = ctrl.decide_at_hour(400.0, 0.0, &battery(100), 20);
        assert_eq!(decision.mode, ControlMode::Discharge);
        assert!(decision.reason.contains("Battery full"));
    }

    #[test]
    fn charges_on_solar_excess() {
        let mut ctrl = controller_no_cooldown();
        let decision = ctrl.decide_at_hour(-300.0, 400.0, &battery(50), 12);
        assert_eq!(decision.mode, ControlMode::Charge);
        assert_eq!(decision.power_watts, 300);
    }

    #[test]
    fn charge_power_capped_by_battery_limit() {
        let state = BatteryState {
            soc: 50,
            max_discharge_power: 800,
            max_charge_power: 1000,
        };
        let mut ctrl = controller_no_cooldown();
        let decision = ctrl.decide_at_hour(-1500.0, 2000.0, &state, 12);
        assert_eq!(decision.mode, ControlMode::Charge);
        assert_eq!(decision.power_watts, 1000);
    }

    #[test]
    fn idle_within_threshold() {
        let mut ctrl = controller_no_cooldown();
        let decision = ctrl.decide_at_hour(30.0, 100.0, &battery(50), 12);
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    #[test]
    fn discharge_capped_by_battery_limit() {
        let state = BatteryState {
            soc: 80,
            max_discharge_power: 500,
            max_charge_power: 2400,
        };
        let mut ctrl = controller_no_cooldown();
        // Hour 20 = discharge period
        let decision = ctrl.decide_at_hour(1000.0, 0.0, &state, 20);
        assert_eq!(decision.mode, ControlMode::Discharge);
        assert_eq!(decision.power_watts, 500);
    }

    #[test]
    fn no_discharge_during_daytime() {
        let mut ctrl = controller_no_cooldown();
        let decision = ctrl.decide_at_hour(400.0, 200.0, &battery(80), 12);
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    // --- Anti-toggle / cooldown tests ---

    #[test]
    fn toggle_charge_to_discharge_suppressed_during_cooldown() {
        // Was charging for 2 minutes — wants to switch to discharge
        let mut ctrl = controller_in_mode(ControlMode::Charge, Duration::from_secs(120));
        // Hour 20, importing 300W → raw decision would be Discharge
        let decision = ctrl.decide_at_hour(300.0, 50.0, &battery(50), 20);
        assert_eq!(decision.mode, ControlMode::Idle);
        assert!(decision.reason.contains("Cooldown"));
    }

    #[test]
    fn toggle_discharge_to_charge_suppressed_during_cooldown() {
        // Was discharging for 1 minute — wants to switch to charge
        let mut ctrl = controller_in_mode(ControlMode::Discharge, Duration::from_secs(60));
        // Exporting 200W → raw decision would be Charge
        let decision = ctrl.decide_at_hour(-200.0, 300.0, &battery(50), 20);
        assert_eq!(decision.mode, ControlMode::Idle);
        assert!(decision.reason.contains("Cooldown"));
    }

    #[test]
    fn toggle_allowed_after_cooldown_expires() {
        // Was charging for 6 minutes — cooldown (5 min) has passed
        let mut ctrl = controller_in_mode(ControlMode::Charge, Duration::from_secs(360));
        let decision = ctrl.decide_at_hour(300.0, 50.0, &battery(50), 20);
        assert_eq!(decision.mode, ControlMode::Discharge);
    }

    #[test]
    fn idle_to_charge_always_allowed() {
        // Was idle for just 10 seconds — switching to charge should be fine
        let mut ctrl = controller_in_mode(ControlMode::Idle, Duration::from_secs(10));
        let decision = ctrl.decide_at_hour(-300.0, 400.0, &battery(50), 12);
        assert_eq!(decision.mode, ControlMode::Charge);
    }

    #[test]
    fn idle_to_discharge_always_allowed() {
        let mut ctrl = controller_in_mode(ControlMode::Idle, Duration::from_secs(10));
        let decision = ctrl.decide_at_hour(300.0, 0.0, &battery(50), 20);
        assert_eq!(decision.mode, ControlMode::Discharge);
    }

    #[test]
    fn charge_to_idle_always_allowed() {
        // Was charging for 10 seconds — going idle is always safe
        let mut ctrl = controller_in_mode(ControlMode::Charge, Duration::from_secs(10));
        // No export, no import within threshold, daytime → Idle
        let decision = ctrl.decide_at_hour(20.0, 100.0, &battery(50), 12);
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    #[test]
    fn rapid_oscillation_stays_idle() {
        // Simulate low solar causing grid power to bounce around ±60W
        let mut ctrl = controller_in_mode(ControlMode::Idle, Duration::from_secs(0));
        let bat = battery(50);

        // First reading: slight export → charge
        let d1 = ctrl.decide_at_hour(-60.0, 80.0, &bat, 20);
        assert_eq!(d1.mode, ControlMode::Charge);

        // Second reading 10s later: slight import → wants discharge, but cooldown blocks it
        let d2 = ctrl.decide_at_hour(60.0, 20.0, &bat, 20);
        assert_eq!(d2.mode, ControlMode::Idle, "should go idle, not discharge");
        assert!(d2.reason.contains("Cooldown"));

        // Third reading: back to export → wants charge again
        // We're now idle (from suppression), so idle→charge is allowed
        let d3 = ctrl.decide_at_hour(-60.0, 80.0, &bat, 20);
        assert_eq!(d3.mode, ControlMode::Charge);

        // Fourth: wants discharge again → suppressed (charge was just set)
        let d4 = ctrl.decide_at_hour(60.0, 20.0, &bat, 20);
        assert_eq!(d4.mode, ControlMode::Idle, "still suppressed");
    }
}
