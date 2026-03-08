use std::time::{Duration, Instant};

use chrono::{Datelike, Local, Timelike};

use crate::battery::BatteryState;
use crate::config::Config;
use crate::models::{ControlDecision, ControlMode, CycleCounts};

const MAX_CHARGE_POWER: i32 = 2400;
const DISCHARGE_START_HOUR: u32 = 17;
const DISCHARGE_END_HOUR: u32 = 7;
const RAMP_FACTOR: f64 = 0.75;

pub struct Controller {
    last_mode: ControlMode,
    last_mode_change: Instant,
    last_decision_time: Instant,
    last_idle_start: Option<Instant>,
    min_mode_duration: Duration,
    min_decision_interval: Duration,
    charge_margin: i32,
    discharge_margin: i32,
    charge_start_threshold: f64,
    discharge_start_threshold: f64,
    idle_timeout: Duration,
    daily_transitions: u32,
    daily_cooldown_suppressions: u32,
    cycle_warn_threshold: u32,
    last_cycle_reset_day: u32,
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
            min_mode_duration,
            min_decision_interval: Duration::from_secs(config.min_decision_interval_secs),
            charge_margin: config.charge_margin,
            discharge_margin: config.discharge_margin,
            charge_start_threshold: config.charge_start_threshold,
            discharge_start_threshold: config.discharge_start_threshold,
            idle_timeout: Duration::from_secs(config.idle_timeout_minutes * 60),
            daily_transitions: 0,
            daily_cooldown_suppressions: 0,
            cycle_warn_threshold: config.cycle_warn_threshold,
            last_cycle_reset_day: Local::now().ordinal(),
        }
    }

    pub fn cycle_counts(&self) -> CycleCounts {
        CycleCounts {
            daily_transitions: self.daily_transitions,
            daily_cooldown_suppressions: self.daily_cooldown_suppressions,
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

        // 1. What mode should we be in?
        let mode = self.target_mode(grid_power, battery, hour);

        // 2. At what power level?
        let power = self.target_power(mode, grid_power, battery);

        // 3. Apply guards (cooldown, ramp, standby timeout)
        self.apply_guards(mode, power, grid_power, solar_power, hour)
    }

    /// Determines the desired mode based on grid state, battery SOC, and time.
    fn target_mode(&self, grid_power: f64, battery: &BatteryState, hour: u32) -> ControlMode {
        let is_discharge_period = !(DISCHARGE_END_HOUR..DISCHARGE_START_HOUR).contains(&hour);

        // Exporting to grid and battery not full → charge
        if battery.soc < 100 && grid_power < self.charge_start_threshold {
            return ControlMode::Charge;
        }

        // Importing from grid during discharge hours → discharge
        if is_discharge_period && grid_power > self.discharge_start_threshold {
            return ControlMode::Discharge;
        }

        ControlMode::Idle
    }

    /// Calculates the target power for a given mode, accounting for battery
    /// feedback (what it's already doing) and safety margins.
    fn target_power(&self, mode: ControlMode, grid_power: f64, battery: &BatteryState) -> i32 {
        match mode {
            ControlMode::Charge => {
                let adjustment = (-grid_power) as i32 - self.charge_margin;
                let current_charge = (-battery.current_power).max(0);
                let max_charge = MAX_CHARGE_POWER.min(battery.max_charge_power);
                (current_charge + adjustment).clamp(0, max_charge)
            }
            ControlMode::Discharge => {
                let adjustment = grid_power as i32 - self.discharge_margin;
                let current_discharge = battery.current_power.max(0);
                (current_discharge + adjustment).clamp(0, battery.max_discharge_power)
            }
            ControlMode::Idle | ControlMode::Standby => 0,
        }
    }

    /// Applies cooldown, ramp, and standby timeout. All state mutation lives here.
    fn apply_guards(
        &mut self,
        mode: ControlMode,
        power: i32,
        grid_power: f64,
        solar_power: f64,
        hour: u32,
    ) -> ControlDecision {
        // Reset daily counters at midnight
        let today = Local::now().ordinal();
        if today != self.last_cycle_reset_day {
            self.daily_transitions = 0;
            self.daily_cooldown_suppressions = 0;
            self.last_cycle_reset_day = today;
        }

        // Cycle limit: force standby when daily transitions exceed threshold
        if self.cycle_warn_threshold > 0 && self.daily_transitions >= self.cycle_warn_threshold {
            if self.last_mode != ControlMode::Standby {
                tracing::warn!(
                    "Daily cycle limit reached ({} transitions) — entering standby until midnight",
                    self.daily_transitions,
                );
                self.last_mode = ControlMode::Standby;
                self.last_mode_change = Instant::now();
                self.last_idle_start = None;
            }
            return ControlDecision {
                mode: ControlMode::Standby,
                power_watts: 0,
                reason: format!(
                    "Cycle limit: {} transitions today (max {}), standby until midnight",
                    self.daily_transitions, self.cycle_warn_threshold,
                ),
                grid_power,
                solar_power,
            };
        }

        // Cooldown: suppress charge↔discharge toggles that happen too fast
        if is_opposing_switch(self.last_mode, mode)
            && self.last_mode_change.elapsed() < self.min_mode_duration
        {
            self.daily_cooldown_suppressions += 1;
            if self.last_mode != ControlMode::Idle {
                self.last_idle_start = Some(Instant::now());
            }
            return ControlDecision {
                mode: ControlMode::Idle,
                power_watts: 0,
                reason: format!(
                    "Cooldown: suppressed {} (was {} for {:.0}s, min {}s)",
                    mode,
                    self.last_mode,
                    self.last_mode_change.elapsed().as_secs_f64(),
                    self.min_mode_duration.as_secs(),
                ),
                grid_power,
                solar_power,
            };
        }

        // Track mode changes and apply ramp
        let (final_power, ramped) = if mode != self.last_mode {
            self.daily_transitions += 1;
            self.last_mode = mode;
            self.last_mode_change = Instant::now();
            self.last_idle_start = if mode == ControlMode::Idle {
                Some(Instant::now())
            } else {
                None
            };
            // Ramp: 75% power on first decision after mode change
            if power > 0 {
                ((power as f64 * RAMP_FACTOR) as i32, true)
            } else {
                (power, false)
            }
        } else {
            (power, false)
        };

        // Idle timeout → standby
        if mode == ControlMode::Idle
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
                grid_power,
                solar_power,
            };
        }

        let reason = build_reason(mode, final_power, grid_power, solar_power, hour, ramped);
        ControlDecision {
            mode,
            power_watts: final_power,
            reason,
            grid_power,
            solar_power,
        }
    }
}

fn build_reason(
    mode: ControlMode,
    power: i32,
    grid_power: f64,
    solar_power: f64,
    hour: u32,
    ramped: bool,
) -> String {
    let ramp = if ramped { " (ramped 75%)" } else { "" };
    match mode {
        ControlMode::Charge => {
            format!(
                "Solar excess: exporting {:.0}W, charging at {power}W{ramp}",
                -grid_power,
            )
        }
        ControlMode::Discharge => {
            format!(
                "Grid demand: importing {grid_power:.0}W, discharging at {power}W (hour {hour}){ramp}",
            )
        }
        ControlMode::Idle => {
            format!(
                "No action needed (grid: {grid_power:.0}W, solar: {solar_power:.0}W, hour: {hour})",
            )
        }
        ControlMode::Standby => "Standby".to_string(),
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

    fn default_controller() -> Controller {
        Controller {
            last_mode: ControlMode::Idle,
            last_mode_change: Instant::now() - Duration::from_secs(60),
            last_decision_time: Instant::now() - Duration::from_secs(60),
            last_idle_start: None,
            min_mode_duration: Duration::from_secs(10),
            min_decision_interval: Duration::ZERO,
            charge_margin: 50,
            discharge_margin: 5,
            charge_start_threshold: -100.0,
            discharge_start_threshold: 50.0,
            idle_timeout: Duration::from_secs(5 * 60),
            daily_transitions: 0,
            daily_cooldown_suppressions: 0,
            cycle_warn_threshold: 200,
            last_cycle_reset_day: Local::now().ordinal(),
        }
    }

    /// Controller with no cooldown (for tests that only care about mode/power logic).
    fn controller_no_cooldown() -> Controller {
        Controller {
            min_mode_duration: Duration::ZERO,
            ..default_controller()
        }
    }

    /// Controller that has been in `mode` for the given duration.
    fn controller_in_mode(mode: ControlMode, elapsed: Duration) -> Controller {
        Controller {
            last_mode: mode,
            last_mode_change: Instant::now() - elapsed,
            last_idle_start: if mode == ControlMode::Idle {
                Some(Instant::now() - elapsed)
            } else {
                None
            },
            ..default_controller()
        }
    }

    // --- Mode selection tests ---

    #[test]
    fn soc_100_never_charges() {
        let mut ctrl = controller_no_cooldown();
        let decision = ctrl.decide_at_hour(-500.0, 600.0, &battery(100), 20);
        assert_ne!(decision.mode, ControlMode::Charge);
    }

    #[test]
    fn soc_100_can_still_discharge() {
        let mut ctrl = controller_no_cooldown();
        let decision = ctrl.decide_at_hour(400.0, 0.0, &battery(100), 20);
        assert_eq!(decision.mode, ControlMode::Discharge);
    }

    #[test]
    fn idle_within_deadband() {
        let mut ctrl = controller_no_cooldown();
        // 30W is below discharge_start_threshold (50W) and above charge_start_threshold (-100W)
        let decision = ctrl.decide_at_hour(30.0, 100.0, &battery(50), 12);
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    #[test]
    fn no_discharge_during_daytime() {
        let mut ctrl = controller_no_cooldown();
        let decision = ctrl.decide_at_hour(400.0, 200.0, &battery(80), 12);
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    #[test]
    fn deadband_no_charge_at_minus_80() {
        let mut ctrl = controller_no_cooldown();
        let decision = ctrl.decide_at_hour(-80.0, 100.0, &battery(50), 12);
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    #[test]
    fn deadband_no_discharge_at_40() {
        let mut ctrl = controller_no_cooldown();
        let decision = ctrl.decide_at_hour(40.0, 0.0, &battery(50), 20);
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    // --- Power calculation tests ---

    #[test]
    fn charges_on_solar_excess() {
        let mut ctrl = controller_in_mode(ControlMode::Charge, Duration::from_secs(60));
        // Grid at -300W, margin=50W → (300-50) = 250W
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
        let mut ctrl = controller_in_mode(ControlMode::Charge, Duration::from_secs(60));
        let decision = ctrl.decide_at_hour(-1500.0, 2000.0, &state, 12);
        assert_eq!(decision.power_watts, 1000);
    }

    #[test]
    fn discharge_capped_by_battery_limit() {
        let state = BatteryState {
            soc: 80,
            max_discharge_power: 500,
            max_charge_power: 2400,
            current_power: 0,
        };
        let mut ctrl = controller_in_mode(ControlMode::Discharge, Duration::from_secs(60));
        let decision = ctrl.decide_at_hour(1000.0, 0.0, &state, 20);
        assert_eq!(decision.power_watts, 500);
    }

    #[test]
    fn charge_margin_reduces_power() {
        let mut ctrl = controller_in_mode(ControlMode::Charge, Duration::from_secs(60));
        ctrl.charge_margin = 100;
        // Grid at -400W, margin=100W → (400-100) = 300W
        let decision = ctrl.decide_at_hour(-400.0, 500.0, &battery(50), 12);
        assert_eq!(decision.power_watts, 300);
    }

    #[test]
    fn discharge_margin_reduces_power() {
        let mut ctrl = controller_in_mode(ControlMode::Discharge, Duration::from_secs(60));
        ctrl.discharge_margin = 20;
        // Grid at +300W, margin=20W → (300-20) = 280W
        let decision = ctrl.decide_at_hour(300.0, 0.0, &battery(50), 20);
        assert_eq!(decision.power_watts, 280);
    }

    // --- Battery feedback tests ---

    #[test]
    fn discharge_accounts_for_current_output() {
        let mut ctrl = controller_in_mode(ControlMode::Discharge, Duration::from_secs(60));
        // Battery already discharging 200W, grid still importing 100W
        // Need: 200 + (100 - 5) = 295W
        let bat = battery_discharging(50, 200);
        let decision = ctrl.decide_at_hour(100.0, 0.0, &bat, 20);
        assert_eq!(decision.power_watts, 295);
    }

    #[test]
    fn charge_accounts_for_current_input() {
        let mut ctrl = controller_in_mode(ControlMode::Charge, Duration::from_secs(60));
        // Battery already charging 200W, grid still exporting 150W
        // Need: 200 + (150 - 50) = 300W
        let bat = battery_charging(50, 200);
        let decision = ctrl.decide_at_hour(-150.0, 500.0, &bat, 12);
        assert_eq!(decision.power_watts, 300);
    }

    #[test]
    fn discharge_idles_when_overproducing() {
        let mut ctrl = controller_no_cooldown();
        // Battery discharging 400W but grid exporting 50W (overshot).
        // -50W is in deadband (above -100W threshold) → idle
        let bat = battery_discharging(50, 400);
        let decision = ctrl.decide_at_hour(-50.0, 0.0, &bat, 20);
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    // --- Ramp tests ---

    #[test]
    fn first_decision_after_mode_change_uses_75_percent() {
        let mut ctrl = controller_no_cooldown();
        // Idle → Charge: 0.75 ramp on mode change
        // target_power: (400-50) = 350W, ramped: 350*0.75 = 262W
        let d1 = ctrl.decide_at_hour(-400.0, 500.0, &battery(50), 12);
        assert_eq!(d1.mode, ControlMode::Charge);
        assert_eq!(d1.power_watts, 262);
        assert!(d1.reason.contains("ramped"));
    }

    #[test]
    fn second_decision_in_same_mode_uses_full_power() {
        let mut ctrl = controller_no_cooldown();
        let _d1 = ctrl.decide_at_hour(-400.0, 500.0, &battery(50), 12);
        // Same mode → full power
        let d2 = ctrl.decide_at_hour(-400.0, 500.0, &battery(50), 12);
        assert_eq!(d2.power_watts, 350); // (400-50)*1.0
        assert!(!d2.reason.contains("ramped"));
    }

    #[test]
    fn ramp_on_discharge_mode_change() {
        let mut ctrl = controller_no_cooldown();
        // Idle → Discharge: ramped
        // target_power: (400-5) = 395W, ramped: 395*0.75 = 296W
        let d1 = ctrl.decide_at_hour(400.0, 0.0, &battery(50), 20);
        assert_eq!(d1.mode, ControlMode::Discharge);
        assert_eq!(d1.power_watts, 296);

        // Same mode → full power
        let d2 = ctrl.decide_at_hour(400.0, 0.0, &battery(50), 20);
        assert_eq!(d2.power_watts, 395);
    }

    // --- Decision interval tests ---

    #[test]
    fn min_decision_interval_throttles() {
        let mut ctrl = default_controller();
        ctrl.min_decision_interval = Duration::from_secs(5);
        ctrl.last_decision_time = Instant::now();

        assert!(ctrl.decide(-300.0, 400.0, &battery(50)).is_none());
    }

    #[test]
    fn decision_allowed_after_interval() {
        let mut ctrl = default_controller();
        ctrl.min_decision_interval = Duration::from_secs(5);
        ctrl.last_decision_time = Instant::now() - Duration::from_secs(6);

        assert!(ctrl.decide(-300.0, 400.0, &battery(50)).is_some());
    }

    // --- Cooldown tests ---

    #[test]
    fn toggle_charge_to_discharge_suppressed() {
        let mut ctrl = controller_in_mode(ControlMode::Charge, Duration::from_secs(5));
        let decision = ctrl.decide_at_hour(300.0, 50.0, &battery(50), 20);
        assert_eq!(decision.mode, ControlMode::Idle);
        assert!(decision.reason.contains("Cooldown"));
    }

    #[test]
    fn toggle_discharge_to_charge_suppressed() {
        let mut ctrl = controller_in_mode(ControlMode::Discharge, Duration::from_secs(5));
        let decision = ctrl.decide_at_hour(-200.0, 300.0, &battery(50), 20);
        assert_eq!(decision.mode, ControlMode::Idle);
        assert!(decision.reason.contains("Cooldown"));
    }

    #[test]
    fn toggle_allowed_after_cooldown_expires() {
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

        let d1 = ctrl.decide_at_hour(-200.0, 280.0, &bat, 20);
        assert_eq!(d1.mode, ControlMode::Charge);

        let d2 = ctrl.decide_at_hour(200.0, 20.0, &bat, 20);
        assert_eq!(d2.mode, ControlMode::Idle, "should go idle, not discharge");
        assert!(d2.reason.contains("Cooldown"));

        let d3 = ctrl.decide_at_hour(-200.0, 280.0, &bat, 20);
        assert_eq!(d3.mode, ControlMode::Charge);

        let d4 = ctrl.decide_at_hour(200.0, 20.0, &bat, 20);
        assert_eq!(d4.mode, ControlMode::Idle, "still suppressed");
    }

    // --- Standby tests ---

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
        let mut ctrl = controller_in_mode(ControlMode::Idle, Duration::from_secs(4 * 60));
        let decision = ctrl.decide_at_hour(20.0, 0.0, &battery(50), 12);
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    #[test]
    fn standby_exits_on_demand() {
        let mut ctrl = controller_in_mode(ControlMode::Idle, Duration::from_secs(20 * 60));
        ctrl.idle_timeout = Duration::from_secs(15 * 60);
        let decision = ctrl.decide_at_hour(300.0, 0.0, &battery(50), 20);
        assert_eq!(decision.mode, ControlMode::Discharge);
    }

    // --- Cycle counting tests ---

    #[test]
    fn transition_increments_daily_cycles() {
        let mut ctrl = controller_no_cooldown();
        assert_eq!(ctrl.daily_transitions, 0);

        // Idle → Charge
        ctrl.decide_at_hour(-200.0, 300.0, &battery(50), 12);
        assert_eq!(ctrl.daily_transitions, 1);

        // Charge → Idle (within deadband)
        ctrl.decide_at_hour(20.0, 100.0, &battery(50), 12);
        assert_eq!(ctrl.daily_transitions, 2);
    }

    #[test]
    fn same_mode_does_not_increment() {
        let mut ctrl = controller_in_mode(ControlMode::Charge, Duration::from_secs(60));
        ctrl.decide_at_hour(-300.0, 400.0, &battery(50), 12);
        assert_eq!(ctrl.daily_transitions, 0);
    }

    #[test]
    fn cooldown_suppression_increments_counter() {
        let mut ctrl = controller_in_mode(ControlMode::Charge, Duration::from_secs(5));
        assert_eq!(ctrl.daily_cooldown_suppressions, 0);

        ctrl.decide_at_hour(300.0, 50.0, &battery(50), 20);
        assert_eq!(ctrl.daily_cooldown_suppressions, 1);
        // Suppression doesn't count as a transition
        assert_eq!(ctrl.daily_transitions, 0);
    }

    #[test]
    fn cycle_counts_returns_current_state() {
        let mut ctrl = controller_no_cooldown();
        ctrl.decide_at_hour(-200.0, 300.0, &battery(50), 12);

        let counts = ctrl.cycle_counts();
        assert_eq!(counts.daily_transitions, 1);
        assert_eq!(counts.daily_cooldown_suppressions, 0);
    }

    #[test]
    fn cycle_limit_forces_standby() {
        let mut ctrl = controller_no_cooldown();
        ctrl.cycle_warn_threshold = 3;

        // 3 transitions: Idle→Charge, Charge→Idle, Idle→Charge
        ctrl.decide_at_hour(-200.0, 300.0, &battery(50), 12);
        ctrl.decide_at_hour(20.0, 100.0, &battery(50), 12);
        ctrl.decide_at_hour(-200.0, 300.0, &battery(50), 12);
        assert_eq!(ctrl.daily_transitions, 3);

        // Next decision should be forced to Standby
        let decision = ctrl.decide_at_hour(-200.0, 300.0, &battery(50), 12);
        assert_eq!(decision.mode, ControlMode::Standby);
        assert!(decision.reason.contains("Cycle limit"));
    }

    #[test]
    fn cycle_limit_standby_persists() {
        let mut ctrl = controller_no_cooldown();
        ctrl.cycle_warn_threshold = 1;

        // 1 transition hits the limit
        ctrl.decide_at_hour(-200.0, 300.0, &battery(50), 12);
        assert_eq!(ctrl.daily_transitions, 1);

        // All subsequent decisions stay in standby
        let d1 = ctrl.decide_at_hour(300.0, 0.0, &battery(50), 20);
        assert_eq!(d1.mode, ControlMode::Standby);

        let d2 = ctrl.decide_at_hour(-500.0, 600.0, &battery(50), 12);
        assert_eq!(d2.mode, ControlMode::Standby);
    }

    #[test]
    fn cycle_limit_zero_disables() {
        let mut ctrl = controller_no_cooldown();
        ctrl.cycle_warn_threshold = 0;

        // Many transitions should still work
        ctrl.decide_at_hour(-200.0, 300.0, &battery(50), 12);
        ctrl.decide_at_hour(20.0, 100.0, &battery(50), 12);
        let decision = ctrl.decide_at_hour(-200.0, 300.0, &battery(50), 12);
        assert_eq!(decision.mode, ControlMode::Charge);
    }
}
