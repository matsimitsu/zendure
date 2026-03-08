use std::time::{Duration, Instant};

use chrono::{Local, Timelike};

use crate::battery::BatteryState;
use crate::config::Config;
use crate::models::{ControlDecision, ControlMode};

const MAX_CHARGE_POWER: i32 = 2400;
const DISCHARGE_START_HOUR: u32 = 17;
const DISCHARGE_END_HOUR: u32 = 7;

pub struct Controller {
    last_mode: ControlMode,
    last_mode_change: Instant,
    last_decision_time: Instant,
    last_idle_start: Option<Instant>,
    /// Whether the current decision is the first after a mode change (for 75% ramp).
    first_in_mode: bool,
    min_mode_duration: Duration,
    min_decision_interval: Duration,
    charge_margin: i32,
    discharge_margin: i32,
    charge_start_threshold: f64,
    discharge_start_threshold: f64,
    idle_timeout: Duration,
}

impl Controller {
    pub fn from_config(config: &Config) -> Self {
        let min_mode_duration = Duration::from_secs(config.min_mode_duration_secs);
        Self {
            last_mode: ControlMode::Idle,
            last_mode_change: Instant::now() - min_mode_duration,
            last_decision_time: Instant::now()
                - Duration::from_secs(config.min_decision_interval_secs),
            last_idle_start: Some(Instant::now()),
            first_in_mode: false,
            min_mode_duration,
            min_decision_interval: Duration::from_secs(config.min_decision_interval_secs),
            charge_margin: config.charge_margin,
            discharge_margin: config.discharge_margin,
            charge_start_threshold: config.charge_start_threshold,
            discharge_start_threshold: config.discharge_start_threshold,
            idle_timeout: Duration::from_secs(config.idle_timeout_minutes * 60),
        }
    }

    /// Returns `None` if the minimum decision interval hasn't elapsed yet.
    pub fn decide(
        &mut self,
        grid_power: f64,
        solar_power: f64,
        battery: &BatteryState,
    ) -> Option<ControlDecision> {
        if self.last_decision_time.elapsed() < self.min_decision_interval {
            return None;
        }

        let hour = Local::now().hour();
        Some(self.decide_at_hour(grid_power, solar_power, battery, hour))
    }

    fn decide_at_hour(
        &mut self,
        grid_power: f64,
        solar_power: f64,
        battery: &BatteryState,
        hour: u32,
    ) -> ControlDecision {
        self.last_decision_time = Instant::now();
        let raw = self.raw_decide(grid_power, solar_power, battery, hour);
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
            // Track idle start for standby timeout
            if self.last_mode != ControlMode::Idle {
                self.last_idle_start = Some(Instant::now());
            }
            // Don't update last_mode — we're waiting for the cooldown to expire
            return idle;
        }

        if decision.mode != self.last_mode {
            self.first_in_mode = true;
            self.last_mode = decision.mode;
            self.last_mode_change = Instant::now();

            // Track idle start/end for standby timeout
            if decision.mode == ControlMode::Idle {
                self.last_idle_start = Some(Instant::now());
            } else {
                self.last_idle_start = None;
            }
        } else {
            self.first_in_mode = false;
        }

        // Check idle timeout → standby
        if decision.mode == ControlMode::Idle
            && let Some(idle_start) = self.last_idle_start
            && idle_start.elapsed() >= self.idle_timeout
        {
            return ControlDecision {
                mode: ControlMode::Standby,
                power_watts: 0,
                reason: format!(
                    "Idle for {}+ minutes, entering standby",
                    self.idle_timeout.as_secs() / 60,
                ),
                grid_power: decision.grid_power,
                solar_power: decision.solar_power,
            };
        }

        decision
    }

    /// Stateless decision logic with battery feedback and margins.
    fn raw_decide(
        &self,
        grid_power: f64,
        solar_power: f64,
        battery: &BatteryState,
        hour: u32,
    ) -> ControlDecision {
        let factor = if self.first_in_mode { 0.75 } else { 1.0 };

        // Battery full — don't charge
        if battery.soc >= 100 {
            return self.discharge_or_idle(
                grid_power,
                solar_power,
                battery,
                hour,
                factor,
                "Battery full (100%)",
            );
        }

        // If we're exporting to the grid, we have excess solar — charge the battery
        if grid_power < self.charge_start_threshold {
            // Calculate how much to charge, accounting for what the battery is already doing.
            // grid_power is negative when exporting. battery.current_power is negative when charging.
            // adjustment = how much MORE we should charge beyond current level.
            let adjustment = ((-grid_power) as i32 - self.charge_margin) as f64 * factor;
            let current_charge = (-battery.current_power).max(0); // current charge rate (positive)
            let max_charge = MAX_CHARGE_POWER.min(battery.max_charge_power);
            let charge_power = (current_charge + adjustment as i32).clamp(0, max_charge);

            let excess = (-grid_power) as i32;
            return ControlDecision {
                mode: ControlMode::Charge,
                power_watts: charge_power,
                reason: format!(
                    "Solar excess: exporting {excess}W to grid (margin: {}W, factor: {factor:.2})",
                    self.charge_margin,
                ),
                grid_power,
                solar_power,
            };
        }

        self.discharge_or_idle(grid_power, solar_power, battery, hour, factor, "")
    }

    fn discharge_or_idle(
        &self,
        grid_power: f64,
        solar_power: f64,
        battery: &BatteryState,
        hour: u32,
        factor: f64,
        extra_reason: &str,
    ) -> ControlDecision {
        let is_discharge_period = !(DISCHARGE_END_HOUR..DISCHARGE_START_HOUR).contains(&hour);

        if is_discharge_period && grid_power > self.discharge_start_threshold {
            // Calculate how much to discharge, accounting for what the battery is already doing.
            // grid_power is positive when importing. battery.current_power is positive when discharging.
            // adjustment = how much MORE we should discharge beyond current level.
            let adjustment = (grid_power as i32 - self.discharge_margin) as f64 * factor;
            let current_discharge = battery.current_power.max(0); // current discharge rate
            let discharge_power =
                (current_discharge + adjustment as i32).clamp(0, battery.max_discharge_power);

            let demand = grid_power as i32;
            let mut reason = format!(
                "Discharge period (hour {hour}): grid demand {demand}W (margin: {}W, factor: {factor:.2})",
                self.discharge_margin,
            );
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

#[cfg(test)]
mod tests {
    use super::*;

    fn battery(soc: u32) -> BatteryState {
        BatteryState {
            soc,
            max_discharge_power: 800,
            max_charge_power: 2400,
            current_power: 0,
        }
    }

    fn battery_discharging(soc: u32, power: i32) -> BatteryState {
        BatteryState {
            soc,
            max_discharge_power: 800,
            max_charge_power: 2400,
            current_power: power,
        }
    }

    fn battery_charging(soc: u32, power: i32) -> BatteryState {
        BatteryState {
            soc,
            max_discharge_power: 800,
            max_charge_power: 2400,
            current_power: -power,
        }
    }

    fn default_config() -> Controller {
        Controller {
            last_mode: ControlMode::Idle,
            last_mode_change: Instant::now() - Duration::from_secs(60),
            last_decision_time: Instant::now() - Duration::from_secs(60),
            last_idle_start: None,
            first_in_mode: false,
            min_mode_duration: Duration::from_secs(10),
            min_decision_interval: Duration::ZERO,
            charge_margin: 50,
            discharge_margin: 5,
            charge_start_threshold: -100.0,
            discharge_start_threshold: 50.0,
            idle_timeout: Duration::from_secs(5 * 60),
        }
    }

    /// Build a controller with no cooldown (for tests that only care about raw logic).
    fn controller_no_cooldown() -> Controller {
        Controller {
            min_mode_duration: Duration::ZERO,
            ..default_config()
        }
    }

    /// Build a controller that has been in `mode` for the given duration.
    fn controller_in_mode(mode: ControlMode, elapsed: Duration) -> Controller {
        Controller {
            last_mode: mode,
            last_mode_change: Instant::now() - elapsed,
            last_idle_start: if mode == ControlMode::Idle {
                Some(Instant::now() - elapsed)
            } else {
                None
            },
            ..default_config()
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
        let decision = ctrl.decide_at_hour(400.0, 0.0, &battery(100), 20);
        assert_eq!(decision.mode, ControlMode::Discharge);
        assert!(decision.reason.contains("Battery full"));
    }

    #[test]
    fn charges_on_solar_excess() {
        let mut ctrl = controller_no_cooldown();
        // Grid at -300W, margin=50W → charge at (300-50)*1.0 = 250W
        let decision = ctrl.decide_at_hour(-300.0, 400.0, &battery(50), 12);
        assert_eq!(decision.mode, ControlMode::Charge);
        assert_eq!(decision.power_watts, 250);
    }

    #[test]
    fn charge_power_capped_by_battery_limit() {
        let state = BatteryState {
            soc: 50,
            max_discharge_power: 800,
            max_charge_power: 1000,
            current_power: 0,
        };
        let mut ctrl = controller_no_cooldown();
        let decision = ctrl.decide_at_hour(-1500.0, 2000.0, &state, 12);
        assert_eq!(decision.mode, ControlMode::Charge);
        assert_eq!(decision.power_watts, 1000);
    }

    #[test]
    fn idle_within_threshold() {
        let mut ctrl = controller_no_cooldown();
        // 30W is below discharge_start_threshold (50W) and above charge_start_threshold (-100W)
        let decision = ctrl.decide_at_hour(30.0, 100.0, &battery(50), 12);
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    #[test]
    fn discharge_capped_by_battery_limit() {
        let state = BatteryState {
            soc: 80,
            max_discharge_power: 500,
            max_charge_power: 2400,
            current_power: 0,
        };
        let mut ctrl = controller_no_cooldown();
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

    // --- Margin tests ---

    #[test]
    fn charge_margin_reduces_power() {
        let mut ctrl = controller_no_cooldown();
        ctrl.charge_margin = 100;
        // Grid at -400W, margin=100W → charge at (400-100)*1.0 = 300W
        let decision = ctrl.decide_at_hour(-400.0, 500.0, &battery(50), 12);
        assert_eq!(decision.mode, ControlMode::Charge);
        assert_eq!(decision.power_watts, 300);
    }

    #[test]
    fn discharge_margin_reduces_power() {
        let mut ctrl = controller_no_cooldown();
        ctrl.discharge_margin = 20;
        // Grid at +300W, margin=20W → discharge at (300-20)*1.0 = 280W
        let decision = ctrl.decide_at_hour(300.0, 0.0, &battery(50), 20);
        assert_eq!(decision.mode, ControlMode::Discharge);
        assert_eq!(decision.power_watts, 280);
    }

    // --- Battery feedback tests ---

    #[test]
    fn discharge_accounts_for_current_output() {
        let mut ctrl = controller_no_cooldown();
        // Battery already discharging 200W, grid still shows +100W import
        // Need: 200 (current) + (100 - 5) * 1.0 = 295W total
        let bat = battery_discharging(50, 200);
        let decision = ctrl.decide_at_hour(100.0, 0.0, &bat, 20);
        assert_eq!(decision.mode, ControlMode::Discharge);
        assert_eq!(decision.power_watts, 295);
    }

    #[test]
    fn charge_accounts_for_current_input() {
        let mut ctrl = controller_no_cooldown();
        // Battery already charging 200W, grid still shows -150W export
        // Need: 200 (current) + (150 - 50) * 1.0 = 300W total
        let bat = battery_charging(50, 200);
        let decision = ctrl.decide_at_hour(-150.0, 500.0, &bat, 12);
        assert_eq!(decision.mode, ControlMode::Charge);
        assert_eq!(decision.power_watts, 300);
    }

    #[test]
    fn discharge_reduces_when_overproducing() {
        let mut ctrl = controller_no_cooldown();
        // Battery discharging 400W, but grid now exporting 50W (overshot)
        // grid is -50W which is above charge_start_threshold (-100), so in deadband
        // This means idle, not discharge
        let bat = battery_discharging(50, 400);
        let decision = ctrl.decide_at_hour(-50.0, 0.0, &bat, 20);
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    // --- Two-stage ramp tests ---

    #[test]
    fn first_decision_uses_75_percent_factor() {
        let mut ctrl = controller_no_cooldown();
        ctrl.first_in_mode = true;
        // Grid at -400W, margin=50W → adjustment = (400-50)*0.75 = 262W
        let decision = ctrl.decide_at_hour(-400.0, 500.0, &battery(50), 12);
        assert_eq!(decision.mode, ControlMode::Charge);
        assert_eq!(decision.power_watts, 262);
    }

    #[test]
    fn third_decision_uses_full_factor() {
        let mut ctrl = controller_no_cooldown();
        // 1st: mode change idle→charge, raw_decide uses factor=1.0 (first_in_mode starts false)
        //      apply_cooldown detects mode change → sets first_in_mode=true
        let _d1 = ctrl.decide_at_hour(-400.0, 500.0, &battery(50), 12);
        // 2nd: raw_decide sees first_in_mode=true → uses 0.75
        //      apply_cooldown sees same mode → sets first_in_mode=false
        let d2 = ctrl.decide_at_hour(-400.0, 500.0, &battery(50), 12);
        assert_eq!(d2.power_watts, 262); // (400-50)*0.75 = 262
        // 3rd: raw_decide sees first_in_mode=false → uses 1.0
        let d3 = ctrl.decide_at_hour(-400.0, 500.0, &battery(50), 12);
        assert_eq!(d3.mode, ControlMode::Charge);
        assert_eq!(d3.power_watts, 350); // (400-50)*1.0 = 350
    }

    #[test]
    fn ramp_on_mode_change() {
        let mut ctrl = controller_no_cooldown();
        // Start idle, then switch to discharge at hour 20
        // Mode change → first_in_mode becomes true
        let d1 = ctrl.decide_at_hour(400.0, 0.0, &battery(50), 20);
        assert_eq!(d1.mode, ControlMode::Discharge);
        // first_in_mode was set true by apply_cooldown, but raw_decide ran before that.
        // The first call in a new mode gets factor=1.0 because first_in_mode starts false,
        // then apply_cooldown sets first_in_mode=true. The SECOND call gets 0.75.
        // Actually let me re-check: apply_cooldown detects mode change → sets first_in_mode=true.
        // Next call: raw_decide sees first_in_mode=true → uses 0.75, then apply_cooldown
        // sees same mode → sets first_in_mode=false.
        let d2 = ctrl.decide_at_hour(400.0, 0.0, &battery(50), 20);
        assert_eq!(d2.mode, ControlMode::Discharge);
        assert_eq!(d2.power_watts, 296); // (400-5)*0.75 = 296.25 → 296
    }

    // --- Hysteresis / deadband tests ---

    #[test]
    fn deadband_no_charge_at_minus_80() {
        let mut ctrl = controller_no_cooldown();
        // -80W is above charge_start_threshold (-100W), so should stay idle
        let decision = ctrl.decide_at_hour(-80.0, 100.0, &battery(50), 12);
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    #[test]
    fn deadband_no_discharge_at_40() {
        let mut ctrl = controller_no_cooldown();
        // 40W is below discharge_start_threshold (50W), so should stay idle
        let decision = ctrl.decide_at_hour(40.0, 0.0, &battery(50), 20);
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    // --- Min decision interval tests ---

    #[test]
    fn min_decision_interval_throttles() {
        let mut ctrl = default_config();
        ctrl.min_decision_interval = Duration::from_secs(5);
        ctrl.last_decision_time = Instant::now(); // just decided

        let result = ctrl.decide(-300.0, 400.0, &battery(50));
        assert!(result.is_none());
    }

    #[test]
    fn decision_allowed_after_interval() {
        let mut ctrl = default_config();
        ctrl.min_decision_interval = Duration::from_secs(5);
        ctrl.last_decision_time = Instant::now() - Duration::from_secs(6);

        let result = ctrl.decide(-300.0, 400.0, &battery(50));
        assert!(result.is_some());
    }

    // --- Anti-toggle / cooldown tests ---

    #[test]
    fn toggle_charge_to_discharge_suppressed_during_cooldown() {
        let mut ctrl = controller_in_mode(ControlMode::Charge, Duration::from_secs(5));
        let decision = ctrl.decide_at_hour(300.0, 50.0, &battery(50), 20);
        assert_eq!(decision.mode, ControlMode::Idle);
        assert!(decision.reason.contains("Cooldown"));
    }

    #[test]
    fn toggle_discharge_to_charge_suppressed_during_cooldown() {
        let mut ctrl = controller_in_mode(ControlMode::Discharge, Duration::from_secs(5));
        let decision = ctrl.decide_at_hour(-200.0, 300.0, &battery(50), 20);
        assert_eq!(decision.mode, ControlMode::Idle);
        assert!(decision.reason.contains("Cooldown"));
    }

    #[test]
    fn toggle_allowed_after_cooldown_expires() {
        // Cooldown is 10s, was charging for 15s
        let mut ctrl = controller_in_mode(ControlMode::Charge, Duration::from_secs(15));
        let decision = ctrl.decide_at_hour(300.0, 50.0, &battery(50), 20);
        assert_eq!(decision.mode, ControlMode::Discharge);
    }

    #[test]
    fn idle_to_charge_always_allowed() {
        let mut ctrl = controller_in_mode(ControlMode::Idle, Duration::from_secs(1));
        let decision = ctrl.decide_at_hour(-300.0, 400.0, &battery(50), 12);
        assert_eq!(decision.mode, ControlMode::Charge);
    }

    #[test]
    fn idle_to_discharge_always_allowed() {
        let mut ctrl = controller_in_mode(ControlMode::Idle, Duration::from_secs(1));
        let decision = ctrl.decide_at_hour(300.0, 0.0, &battery(50), 20);
        assert_eq!(decision.mode, ControlMode::Discharge);
    }

    #[test]
    fn charge_to_idle_always_allowed() {
        let mut ctrl = controller_in_mode(ControlMode::Charge, Duration::from_secs(1));
        let decision = ctrl.decide_at_hour(20.0, 100.0, &battery(50), 12);
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    #[test]
    fn rapid_oscillation_stays_idle() {
        let mut ctrl = controller_in_mode(ControlMode::Idle, Duration::from_secs(0));
        let bat = battery(50);

        // First reading: export → charge
        let d1 = ctrl.decide_at_hour(-200.0, 280.0, &bat, 20);
        assert_eq!(d1.mode, ControlMode::Charge);

        // Second reading: import → wants discharge, but cooldown blocks it
        let d2 = ctrl.decide_at_hour(200.0, 20.0, &bat, 20);
        assert_eq!(d2.mode, ControlMode::Idle, "should go idle, not discharge");
        assert!(d2.reason.contains("Cooldown"));

        // Third reading: back to export → wants charge again (idle→charge allowed)
        let d3 = ctrl.decide_at_hour(-200.0, 280.0, &bat, 20);
        assert_eq!(d3.mode, ControlMode::Charge);

        // Fourth: wants discharge again → suppressed
        let d4 = ctrl.decide_at_hour(200.0, 20.0, &bat, 20);
        assert_eq!(d4.mode, ControlMode::Idle, "still suppressed");
    }

    // --- Idle timeout / standby tests ---

    #[test]
    fn idle_timeout_triggers_standby() {
        let mut ctrl = controller_in_mode(ControlMode::Idle, Duration::from_secs(16 * 60));
        ctrl.idle_timeout = Duration::from_secs(15 * 60);
        let decision = ctrl.decide_at_hour(20.0, 0.0, &battery(50), 12);
        assert_eq!(decision.mode, ControlMode::Standby);
        assert!(decision.reason.contains("standby"));
    }

    #[test]
    fn no_standby_before_timeout() {
        let mut ctrl = controller_in_mode(ControlMode::Idle, Duration::from_secs(5 * 60));
        ctrl.idle_timeout = Duration::from_secs(15 * 60);
        let decision = ctrl.decide_at_hour(20.0, 0.0, &battery(50), 12);
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    #[test]
    fn standby_exits_on_demand() {
        let mut ctrl = controller_in_mode(ControlMode::Idle, Duration::from_secs(20 * 60));
        ctrl.idle_timeout = Duration::from_secs(15 * 60);
        // Grid demand above threshold should still trigger discharge
        let decision = ctrl.decide_at_hour(300.0, 0.0, &battery(50), 20);
        // Raw decision is discharge, not idle → standby check doesn't apply
        assert_eq!(decision.mode, ControlMode::Discharge);
    }
}
