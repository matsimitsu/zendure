use std::time::{Duration, Instant};

use chrono::{Datelike, Timelike, Utc};
use chrono_tz::Tz;

use crate::battery::BatteryState;
use crate::config::Config;
use crate::models::{ControlDecision, ControlMode, CycleCounts};

const MAX_CHARGE_POWER: i32 = 2400;
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
    min_idle_before_discharge: Duration,
    daily_transitions: u32,
    daily_cooldown_suppressions: u32,
    cycle_warn_threshold: u32,
    last_cycle_reset_day: u32,
    min_soc: u32,
    max_soc: u32,
    timezone: Tz,
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
            min_idle_before_discharge: Duration::from_secs(config.min_idle_before_discharge_secs),
            daily_transitions: 0,
            daily_cooldown_suppressions: 0,
            cycle_warn_threshold: config.cycle_warn_threshold,
            last_cycle_reset_day: Utc::now().with_timezone(&config.timezone).ordinal(),
            min_soc: config.min_soc,
            max_soc: config.max_soc,
            timezone: config.timezone,
        }
    }

    pub fn cycle_counts(&self) -> CycleCounts {
        CycleCounts {
            daily_transitions: self.daily_transitions,
            daily_cooldown_suppressions: self.daily_cooldown_suppressions,
        }
    }

    /// Returns `None` if the minimum decision interval hasn't elapsed yet.
    pub fn decide(&mut self, grid_power: f64, battery: &BatteryState) -> Option<ControlDecision> {
        if self.last_decision_time.elapsed() < self.min_decision_interval {
            return None;
        }

        let hour = Utc::now().with_timezone(&self.timezone).hour();
        Some(self.decide_at_hour(grid_power, battery, hour))
    }

    pub(crate) fn decide_at_hour(
        &mut self,
        grid_power: f64,
        battery: &BatteryState,
        hour: u32,
    ) -> ControlDecision {
        self.last_decision_time = Instant::now();

        // 0. SOC calibration — reported SOC is unreliable, stay idle
        if battery.soc_calibrating {
            tracing::info!("SOC calibration in progress, idling");
            if self.last_mode != ControlMode::Idle {
                self.last_mode = ControlMode::Idle;
                self.last_mode_change = Instant::now();
                self.last_idle_start = Some(Instant::now());
            }
            return ControlDecision {
                mode: ControlMode::Idle,
                power_watts: 0,
                reason: "SOC calibration in progress, idling".to_string(),
                grid_power,
            };
        }

        // 1. What mode should we be in?
        let mode = self.target_mode(grid_power, battery, hour);

        // 2. At what power level?
        let power = self.target_power(mode, grid_power, battery);

        // 3. Apply guards (cooldown, ramp, standby timeout)
        self.apply_guards(mode, power, grid_power, hour)
    }

    /// Determines the desired mode based on grid state, battery SOC, and time.
    ///
    /// Uses hysteresis: the threshold to *start* charging/discharging is more
    /// aggressive than the threshold to *keep* doing so. This prevents
    /// oscillation when the battery's own grid effect pushes the meter reading
    /// close to the start threshold.
    fn target_mode(&self, grid_power: f64, battery: &BatteryState, _hour: u32) -> ControlMode {
        // Adjust for battery's own grid effect: the meter reading includes
        // the battery's consumption (charging) or production (discharging).
        // current_power: negative = charging, positive = discharging.
        let underlying_grid = grid_power + battery.current_power as f64;

        // Hysteresis: once charging, keep going as long as we're still exporting (< 0W).
        // Only require the full start threshold to *begin* charging.
        let charge_threshold = if self.last_mode == ControlMode::Charge {
            0.0
        } else {
            self.charge_start_threshold
        };

        // Exporting to grid, battery below max SOC, and battery accepts charge → charge
        if battery.soc < self.max_soc
            && !battery.soc_limit_reached
            && underlying_grid < charge_threshold
        {
            return ControlMode::Charge;
        }

        // Hysteresis: once discharging, keep going as long as we're still importing (> 0W).
        let discharge_threshold = if self.last_mode == ControlMode::Discharge {
            0.0
        } else {
            self.discharge_start_threshold
        };

        // Require minimum idle duration before starting discharge (prevents
        // charge→idle→discharge oscillation during variable solar conditions).
        // Already-discharging is exempt (hysteresis keeps it going).
        let idle_long_enough = match self.last_mode {
            ControlMode::Discharge => true,
            _ => self
                .last_idle_start
                .is_some_and(|t| t.elapsed() >= self.min_idle_before_discharge),
        };

        // Importing from grid and battery above min SOC → discharge
        if idle_long_enough && battery.soc > self.min_soc && underlying_grid > discharge_threshold {
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
        hour: u32,
    ) -> ControlDecision {
        // Reset daily counters at midnight
        let today = Utc::now().with_timezone(&self.timezone).ordinal();
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
            };
        }

        let reason = build_reason(mode, final_power, grid_power, hour, ramped);
        ControlDecision {
            mode,
            power_watts: final_power,
            reason,
            grid_power,
        }
    }
}

fn build_reason(mode: ControlMode, power: i32, grid_power: f64, hour: u32, ramped: bool) -> String {
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
            format!("No action needed (grid: {grid_power:.0}W, hour: {hour})")
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
            soc_calibrating: false,
            soc_limit_reached: false,
        }
    }

    fn battery_discharging(soc: u32, power: i32) -> BatteryState {
        BatteryState {
            soc,
            max_discharge_power: 800,
            max_charge_power: 2400,
            current_power: power,
            soc_calibrating: false,
            soc_limit_reached: false,
        }
    }

    fn battery_charging(soc: u32, power: i32) -> BatteryState {
        BatteryState {
            soc,
            max_discharge_power: 800,
            max_charge_power: 2400,
            current_power: -power,
            soc_calibrating: false,
            soc_limit_reached: false,
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
            discharge_start_threshold: 0.0,
            idle_timeout: Duration::from_secs(5 * 60),
            min_idle_before_discharge: Duration::from_secs(300),
            daily_transitions: 0,
            daily_cooldown_suppressions: 0,
            cycle_warn_threshold: 200,
            last_cycle_reset_day: Utc::now().ordinal(),
            min_soc: 10,
            max_soc: 100,
            timezone: Tz::UTC,
        }
    }

    /// Controller with no cooldown and no idle-before-discharge requirement
    /// (for tests that only care about mode/power logic).
    fn controller_no_cooldown() -> Controller {
        Controller {
            min_mode_duration: Duration::ZERO,
            min_idle_before_discharge: Duration::ZERO,
            last_idle_start: Some(Instant::now() - Duration::from_secs(60)),
            ..default_controller()
        }
    }

    /// Controller that has been in `mode` for the given duration.
    fn controller_in_mode(mode: ControlMode, elapsed: Duration) -> Controller {
        Controller {
            last_mode: mode,
            last_mode_change: Instant::now() - elapsed,
            last_idle_start: match mode {
                ControlMode::Idle | ControlMode::Standby => Some(Instant::now() - elapsed),
                _ => None,
            },
            ..default_controller()
        }
    }

    // --- Mode selection tests ---

    #[test]
    fn soc_at_max_never_charges() {
        let mut ctrl = controller_no_cooldown();
        let decision = ctrl.decide_at_hour(-500.0, &battery(100), 20);
        assert_ne!(decision.mode, ControlMode::Charge);
    }

    #[test]
    fn soc_below_max_can_charge() {
        let mut ctrl = controller_no_cooldown();
        let decision = ctrl.decide_at_hour(-500.0, &battery(99), 12);
        assert_eq!(decision.mode, ControlMode::Charge);
    }

    #[test]
    fn soc_at_max_can_still_discharge() {
        let mut ctrl = controller_no_cooldown();
        let decision = ctrl.decide_at_hour(400.0, &battery(100), 20);
        assert_eq!(decision.mode, ControlMode::Discharge);
    }

    #[test]
    fn soc_at_min_never_discharges() {
        let mut ctrl = controller_no_cooldown();
        let decision = ctrl.decide_at_hour(400.0, &battery(10), 20);
        assert_ne!(decision.mode, ControlMode::Discharge);
    }

    #[test]
    fn soc_above_min_can_discharge() {
        let mut ctrl = controller_no_cooldown();
        let decision = ctrl.decide_at_hour(400.0, &battery(11), 20);
        assert_eq!(decision.mode, ControlMode::Discharge);
    }

    #[test]
    fn charging_transitions_to_idle_when_soc_reaches_max() {
        let mut ctrl = controller_no_cooldown();
        // Start charging at 99%
        let d1 = ctrl.decide_at_hour(-500.0, &battery(99), 12);
        assert_eq!(d1.mode, ControlMode::Charge);

        // SOC reaches 100% → should stop charging and go idle
        let d2 = ctrl.decide_at_hour(-500.0, &battery(100), 12);
        assert_eq!(d2.mode, ControlMode::Idle);
    }

    #[test]
    fn discharging_transitions_to_idle_when_soc_reaches_min() {
        let mut ctrl = controller_no_cooldown();
        // Start discharging at 11%
        let d1 = ctrl.decide_at_hour(400.0, &battery(11), 20);
        assert_eq!(d1.mode, ControlMode::Discharge);

        // SOC drops to 10% → should stop discharging and go idle
        let d2 = ctrl.decide_at_hour(400.0, &battery(10), 20);
        assert_eq!(d2.mode, ControlMode::Idle);
    }

    #[test]
    fn soc_at_min_can_still_charge() {
        let mut ctrl = controller_no_cooldown();
        let decision = ctrl.decide_at_hour(-500.0, &battery(10), 12);
        assert_eq!(decision.mode, ControlMode::Charge);
    }

    #[test]
    fn idle_within_deadband() {
        let mut ctrl = controller_no_cooldown();
        // 0W is at discharge_start_threshold (0W) and above charge_start_threshold (-100W)
        let decision = ctrl.decide_at_hour(0.0, &battery(50), 12);
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    #[test]
    fn no_discharge_before_idle_duration_met() {
        let mut ctrl = controller_no_cooldown();
        // Recently went idle — not idle long enough for discharge
        ctrl.last_idle_start = Some(Instant::now() - Duration::from_secs(60));
        ctrl.min_idle_before_discharge = Duration::from_secs(300);
        let decision = ctrl.decide_at_hour(400.0, &battery(80), 12);
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    #[test]
    fn discharge_allowed_after_idle_duration_met() {
        let mut ctrl = controller_no_cooldown();
        // Idle for long enough
        ctrl.last_idle_start = Some(Instant::now() - Duration::from_secs(600));
        ctrl.min_idle_before_discharge = Duration::from_secs(300);
        let decision = ctrl.decide_at_hour(400.0, &battery(80), 12);
        assert_eq!(decision.mode, ControlMode::Discharge);
    }

    #[test]
    fn discharge_allowed_when_already_discharging() {
        // Already discharging — should keep going regardless of idle duration
        let mut ctrl = controller_in_mode(ControlMode::Discharge, Duration::from_secs(60));
        ctrl.min_idle_before_discharge = Duration::from_secs(300);
        let decision = ctrl.decide_at_hour(400.0, &battery(80), 12);
        assert_eq!(decision.mode, ControlMode::Discharge);
    }

    #[test]
    fn deadband_no_charge_at_minus_80() {
        let mut ctrl = controller_no_cooldown();
        let decision = ctrl.decide_at_hour(-80.0, &battery(50), 12);
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    #[test]
    fn deadband_no_discharge_at_zero() {
        let mut ctrl = controller_no_cooldown();
        // 0W grid power is not > 0 threshold → idle
        let decision = ctrl.decide_at_hour(0.0, &battery(50), 20);
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    // --- Power calculation tests ---

    #[test]
    fn charges_on_solar_excess() {
        let mut ctrl = controller_in_mode(ControlMode::Charge, Duration::from_secs(60));
        // Grid at -300W, margin=50W → (300-50) = 250W
        let decision = ctrl.decide_at_hour(-300.0, &battery(50), 12);
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
            soc_calibrating: false,
            soc_limit_reached: false,
        };
        let mut ctrl = controller_in_mode(ControlMode::Charge, Duration::from_secs(60));
        let decision = ctrl.decide_at_hour(-1500.0, &state, 12);
        assert_eq!(decision.power_watts, 1000);
    }

    #[test]
    fn discharge_capped_by_battery_limit() {
        let state = BatteryState {
            soc: 80,
            max_discharge_power: 500,
            max_charge_power: 2400,
            current_power: 0,
            soc_calibrating: false,
            soc_limit_reached: false,
        };
        let mut ctrl = controller_in_mode(ControlMode::Discharge, Duration::from_secs(60));
        let decision = ctrl.decide_at_hour(1000.0, &state, 20);
        assert_eq!(decision.power_watts, 500);
    }

    #[test]
    fn charge_margin_reduces_power() {
        let mut ctrl = controller_in_mode(ControlMode::Charge, Duration::from_secs(60));
        ctrl.charge_margin = 100;
        // Grid at -400W, margin=100W → (400-100) = 300W
        let decision = ctrl.decide_at_hour(-400.0, &battery(50), 12);
        assert_eq!(decision.power_watts, 300);
    }

    #[test]
    fn discharge_margin_reduces_power() {
        let mut ctrl = controller_in_mode(ControlMode::Discharge, Duration::from_secs(60));
        ctrl.discharge_margin = 20;
        // Grid at +300W, margin=20W → (300-20) = 280W
        let decision = ctrl.decide_at_hour(300.0, &battery(50), 20);
        assert_eq!(decision.power_watts, 280);
    }

    // --- Battery feedback tests ---

    #[test]
    fn discharge_accounts_for_current_output() {
        let mut ctrl = controller_in_mode(ControlMode::Discharge, Duration::from_secs(60));
        // Battery already discharging 200W, grid still importing 100W
        // Need: 200 + (100 - 5) = 295W
        let bat = battery_discharging(50, 200);
        let decision = ctrl.decide_at_hour(100.0, &bat, 20);
        assert_eq!(decision.power_watts, 295);
    }

    #[test]
    fn charge_accounts_for_current_input() {
        let mut ctrl = controller_in_mode(ControlMode::Charge, Duration::from_secs(60));
        // Battery already charging 200W, grid still exporting 150W
        // Need: 200 + (150 - 50) = 300W
        let bat = battery_charging(50, 200);
        let decision = ctrl.decide_at_hour(-150.0, &bat, 12);
        assert_eq!(decision.power_watts, 300);
    }

    #[test]
    fn discharge_reduces_power_when_overproducing() {
        let mut ctrl = controller_in_mode(ControlMode::Discharge, Duration::from_secs(60));
        // Battery discharging 400W but grid exporting 50W (overshot).
        // underlying_grid = -50 + 400 = 350W → real demand still high, stay discharging.
        // Power: 400 + (-50 - 5) = 345W (reduces toward balance).
        let bat = battery_discharging(50, 400);
        let decision = ctrl.decide_at_hour(-50.0, &bat, 20);
        assert_eq!(decision.mode, ControlMode::Discharge);
        assert_eq!(decision.power_watts, 345);
    }

    // --- Ramp tests ---

    #[test]
    fn first_decision_after_mode_change_uses_75_percent() {
        let mut ctrl = controller_no_cooldown();
        // Idle → Charge: 0.75 ramp on mode change
        // target_power: (400-50) = 350W, ramped: 350*0.75 = 262W
        let d1 = ctrl.decide_at_hour(-400.0, &battery(50), 12);
        assert_eq!(d1.mode, ControlMode::Charge);
        assert_eq!(d1.power_watts, 262);
        assert!(d1.reason.contains("ramped"));
    }

    #[test]
    fn second_decision_in_same_mode_uses_full_power() {
        let mut ctrl = controller_no_cooldown();
        let _d1 = ctrl.decide_at_hour(-400.0, &battery(50), 12);
        // Same mode → full power
        let d2 = ctrl.decide_at_hour(-400.0, &battery(50), 12);
        assert_eq!(d2.power_watts, 350); // (400-50)*1.0
        assert!(!d2.reason.contains("ramped"));
    }

    #[test]
    fn ramp_on_discharge_mode_change() {
        let mut ctrl = controller_no_cooldown();
        // Idle → Discharge: ramped
        // target_power: (400-5) = 395W, ramped: 395*0.75 = 296W
        let d1 = ctrl.decide_at_hour(400.0, &battery(50), 20);
        assert_eq!(d1.mode, ControlMode::Discharge);
        assert_eq!(d1.power_watts, 296);

        // Same mode → full power
        let d2 = ctrl.decide_at_hour(400.0, &battery(50), 20);
        assert_eq!(d2.power_watts, 395);
    }

    // --- Decision interval tests ---

    #[test]
    fn min_decision_interval_throttles() {
        let mut ctrl = default_controller();
        ctrl.min_decision_interval = Duration::from_secs(5);
        ctrl.last_decision_time = Instant::now();

        assert!(ctrl.decide(-300.0, &battery(50)).is_none());
    }

    #[test]
    fn decision_allowed_after_interval() {
        let mut ctrl = default_controller();
        ctrl.min_decision_interval = Duration::from_secs(5);
        ctrl.last_decision_time = Instant::now() - Duration::from_secs(6);

        assert!(ctrl.decide(-300.0, &battery(50)).is_some());
    }

    // --- Cooldown tests ---

    #[test]
    fn charge_to_discharge_blocked_by_idle_duration() {
        // In Charge mode → target_mode returns Idle (no idle time for discharge)
        let mut ctrl = controller_in_mode(ControlMode::Charge, Duration::from_secs(5));
        let decision = ctrl.decide_at_hour(300.0, &battery(50), 20);
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    #[test]
    fn toggle_discharge_to_charge_suppressed() {
        let mut ctrl = controller_in_mode(ControlMode::Discharge, Duration::from_secs(5));
        let decision = ctrl.decide_at_hour(-200.0, &battery(50), 20);
        assert_eq!(decision.mode, ControlMode::Idle);
        assert!(decision.reason.contains("Cooldown"));
    }

    #[test]
    fn discharge_allowed_after_sufficient_idle() {
        // Was in Charge, then idle for 10 minutes (> 5 min default)
        let mut ctrl = controller_in_mode(ControlMode::Idle, Duration::from_secs(600));
        let decision = ctrl.decide_at_hour(300.0, &battery(50), 20);
        assert_eq!(decision.mode, ControlMode::Discharge);
    }

    #[test]
    fn idle_to_charge_always_allowed() {
        let mut ctrl = controller_in_mode(ControlMode::Idle, Duration::from_secs(1));
        let decision = ctrl.decide_at_hour(-300.0, &battery(50), 12);
        assert_eq!(decision.mode, ControlMode::Charge);
    }

    #[test]
    fn idle_to_discharge_allowed_after_idle_duration() {
        let mut ctrl = controller_in_mode(ControlMode::Idle, Duration::from_secs(600));
        let decision = ctrl.decide_at_hour(300.0, &battery(50), 20);
        assert_eq!(decision.mode, ControlMode::Discharge);
    }

    #[test]
    fn charge_to_idle_always_allowed() {
        let mut ctrl = controller_in_mode(ControlMode::Charge, Duration::from_secs(1));
        let decision = ctrl.decide_at_hour(20.0, &battery(50), 12);
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    #[test]
    fn rapid_oscillation_stays_idle() {
        let mut ctrl = controller_in_mode(ControlMode::Idle, Duration::from_secs(0));
        ctrl.min_idle_before_discharge = Duration::from_secs(300);
        let bat = battery(50);

        let d1 = ctrl.decide_at_hour(-200.0, &bat, 20);
        assert_eq!(d1.mode, ControlMode::Charge);

        // After charging, idle duration not met → stays idle (not discharge)
        let d2 = ctrl.decide_at_hour(200.0, &bat, 20);
        assert_eq!(d2.mode, ControlMode::Idle, "should go idle, not discharge");

        let d3 = ctrl.decide_at_hour(-200.0, &bat, 20);
        assert_eq!(d3.mode, ControlMode::Charge);

        let d4 = ctrl.decide_at_hour(200.0, &bat, 20);
        assert_eq!(
            d4.mode,
            ControlMode::Idle,
            "still idle, not enough idle time"
        );
    }

    // --- Standby tests ---

    #[test]
    fn idle_timeout_triggers_standby() {
        let mut ctrl = controller_in_mode(ControlMode::Idle, Duration::from_secs(16 * 60));
        ctrl.idle_timeout = Duration::from_secs(15 * 60);
        // Grid at 0W — no discharge demand, so idle persists until standby triggers
        let decision = ctrl.decide_at_hour(0.0, &battery(50), 12);
        assert_eq!(decision.mode, ControlMode::Standby);
        assert!(decision.reason.contains("standby"));
    }

    #[test]
    fn no_standby_before_timeout() {
        let mut ctrl = controller_in_mode(ControlMode::Idle, Duration::from_secs(4 * 60));
        let decision = ctrl.decide_at_hour(20.0, &battery(50), 12);
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    #[test]
    fn standby_exits_on_demand() {
        let mut ctrl = controller_in_mode(ControlMode::Idle, Duration::from_secs(20 * 60));
        ctrl.idle_timeout = Duration::from_secs(15 * 60);
        let decision = ctrl.decide_at_hour(300.0, &battery(50), 20);
        assert_eq!(decision.mode, ControlMode::Discharge);
    }

    // --- Cycle counting tests ---

    #[test]
    fn transition_increments_daily_cycles() {
        let mut ctrl = controller_no_cooldown();
        assert_eq!(ctrl.daily_transitions, 0);

        // Idle → Charge
        ctrl.decide_at_hour(-200.0, &battery(50), 12);
        assert_eq!(ctrl.daily_transitions, 1);

        // Charge → Idle (within deadband)
        ctrl.decide_at_hour(20.0, &battery(50), 12);
        assert_eq!(ctrl.daily_transitions, 2);
    }

    #[test]
    fn same_mode_does_not_increment() {
        let mut ctrl = controller_in_mode(ControlMode::Charge, Duration::from_secs(60));
        ctrl.decide_at_hour(-300.0, &battery(50), 12);
        assert_eq!(ctrl.daily_transitions, 0);
    }

    #[test]
    fn cooldown_suppression_increments_counter() {
        // Discharge→Charge toggle within cooldown
        let mut ctrl = controller_in_mode(ControlMode::Discharge, Duration::from_secs(5));
        assert_eq!(ctrl.daily_cooldown_suppressions, 0);

        ctrl.decide_at_hour(-200.0, &battery(50), 20);
        assert_eq!(ctrl.daily_cooldown_suppressions, 1);
        // Suppression doesn't count as a transition
        assert_eq!(ctrl.daily_transitions, 0);
    }

    #[test]
    fn cycle_counts_returns_current_state() {
        let mut ctrl = controller_no_cooldown();
        ctrl.decide_at_hour(-200.0, &battery(50), 12);

        let counts = ctrl.cycle_counts();
        assert_eq!(counts.daily_transitions, 1);
        assert_eq!(counts.daily_cooldown_suppressions, 0);
    }

    #[test]
    fn cycle_limit_forces_standby() {
        let mut ctrl = controller_no_cooldown();
        ctrl.cycle_warn_threshold = 3;

        // 3 transitions: Idle→Charge, Charge→Idle, Idle→Charge
        ctrl.decide_at_hour(-200.0, &battery(50), 12);
        ctrl.decide_at_hour(20.0, &battery(50), 12);
        ctrl.decide_at_hour(-200.0, &battery(50), 12);
        assert_eq!(ctrl.daily_transitions, 3);

        // Next decision should be forced to Standby
        let decision = ctrl.decide_at_hour(-200.0, &battery(50), 12);
        assert_eq!(decision.mode, ControlMode::Standby);
        assert!(decision.reason.contains("Cycle limit"));
    }

    #[test]
    fn cycle_limit_standby_persists() {
        let mut ctrl = controller_no_cooldown();
        ctrl.cycle_warn_threshold = 1;

        // 1 transition hits the limit
        ctrl.decide_at_hour(-200.0, &battery(50), 12);
        assert_eq!(ctrl.daily_transitions, 1);

        // All subsequent decisions stay in standby
        let d1 = ctrl.decide_at_hour(300.0, &battery(50), 20);
        assert_eq!(d1.mode, ControlMode::Standby);

        let d2 = ctrl.decide_at_hour(-500.0, &battery(50), 12);
        assert_eq!(d2.mode, ControlMode::Standby);
    }

    // --- SOC calibration tests ---

    #[test]
    fn calibrating_forces_idle() {
        let mut ctrl = controller_no_cooldown();
        let mut bat = battery(50);
        bat.soc_calibrating = true;
        // Would normally charge, but calibration overrides
        let decision = ctrl.decide_at_hour(-500.0, &bat, 12);
        assert_eq!(decision.mode, ControlMode::Idle);
        assert_eq!(decision.power_watts, 0);
        assert!(decision.reason.contains("calibration"));
    }

    #[test]
    fn calibrating_prevents_discharge() {
        let mut ctrl = controller_no_cooldown();
        let mut bat = battery(80);
        bat.soc_calibrating = true;
        let decision = ctrl.decide_at_hour(400.0, &bat, 20);
        assert_eq!(decision.mode, ControlMode::Idle);
        assert!(decision.reason.contains("calibration"));
    }

    #[test]
    fn normal_soc_status_allows_decisions() {
        let mut ctrl = controller_no_cooldown();
        let bat = battery(50); // soc_calibrating: false
        let decision = ctrl.decide_at_hour(-500.0, &bat, 12);
        assert_eq!(decision.mode, ControlMode::Charge);
    }

    #[test]
    fn cycle_limit_zero_disables() {
        let mut ctrl = controller_no_cooldown();
        ctrl.cycle_warn_threshold = 0;

        // Many transitions should still work
        ctrl.decide_at_hour(-200.0, &battery(50), 12);
        ctrl.decide_at_hour(20.0, &battery(50), 12);
        let decision = ctrl.decide_at_hour(-200.0, &battery(50), 12);
        assert_eq!(decision.mode, ControlMode::Charge);
    }

    // --- Charge hysteresis tests ---

    #[test]
    fn charge_hysteresis_keeps_charging_within_deadband() {
        // underlying_grid = -50W, which is between charge_start_threshold (-100W) and 0W.
        // From idle: -50 > -100 → would NOT start charging.
        // But already charging: threshold drops to 0W, -50 < 0 → keeps charging.
        let mut ctrl = controller_in_mode(ControlMode::Charge, Duration::from_secs(60));
        let decision = ctrl.decide_at_hour(-50.0, &battery(50), 12);
        assert_eq!(decision.mode, ControlMode::Charge);
    }

    #[test]
    fn charge_hysteresis_does_not_start_within_deadband() {
        // Same grid power (-50W) but starting from idle.
        // underlying_grid = -50 > charge_start_threshold (-100) → stays idle.
        let mut ctrl = controller_no_cooldown();
        let decision = ctrl.decide_at_hour(-50.0, &battery(50), 12);
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    #[test]
    fn charge_hysteresis_stops_when_importing() {
        // Already charging, but underlying_grid >= 0 → even hysteresis can't save it.
        // Battery charging at 200W, grid reads +10W → underlying = 10 + (-200) = -190W.
        // Wait, let's use a simpler case: battery idle, grid +10W → underlying = +10 >= 0.
        let mut ctrl = controller_in_mode(ControlMode::Charge, Duration::from_secs(60));
        let decision = ctrl.decide_at_hour(10.0, &battery(50), 12);
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    #[test]
    fn charge_hysteresis_boundary_at_zero() {
        // Already charging, underlying_grid = 0.0 exactly → 0.0 < 0.0 is false → stops.
        let mut ctrl = controller_in_mode(ControlMode::Charge, Duration::from_secs(60));
        let decision = ctrl.decide_at_hour(0.0, &battery(50), 12);
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    // --- Discharge hysteresis tests ---

    #[test]
    fn discharge_hysteresis_keeps_discharging_near_zero() {
        // Already discharging. Set a higher start threshold to make the deadband visible.
        let mut ctrl = controller_in_mode(ControlMode::Discharge, Duration::from_secs(60));
        ctrl.discharge_start_threshold = 100.0;
        // underlying_grid = 50W: below start threshold (100W) but above hysteresis (0W).
        let decision = ctrl.decide_at_hour(50.0, &battery(50), 20);
        assert_eq!(decision.mode, ControlMode::Discharge);
    }

    #[test]
    fn discharge_hysteresis_does_not_start_below_threshold() {
        // Same grid power but from idle — should NOT start discharging.
        let mut ctrl = controller_no_cooldown();
        ctrl.discharge_start_threshold = 100.0;
        let decision = ctrl.decide_at_hour(50.0, &battery(50), 20);
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    #[test]
    fn discharge_hysteresis_stops_when_exporting() {
        // Already discharging, but underlying_grid = -10 <= 0 → not > 0 → stops discharging.
        // -10 is also > charge_start_threshold (-100) → not enough export to charge → idle.
        let mut ctrl = controller_in_mode(ControlMode::Discharge, Duration::from_secs(60));
        let decision = ctrl.decide_at_hour(-10.0, &battery(50), 20);
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    #[test]
    fn discharge_hysteresis_boundary_at_zero() {
        // Already discharging, underlying_grid = 0.0 exactly → 0.0 > 0.0 is false → stops.
        let mut ctrl = controller_in_mode(ControlMode::Discharge, Duration::from_secs(60));
        ctrl.discharge_start_threshold = 100.0;
        let decision = ctrl.decide_at_hour(0.0, &battery(50), 20);
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    // --- SOC limit tests ---

    #[test]
    fn soc_limit_reached_prevents_charging() {
        // Battery reports socLimit: 1 at 99% — should not charge
        let mut ctrl = controller_no_cooldown();
        let mut bat = battery(99);
        bat.soc_limit_reached = true;
        let decision = ctrl.decide_at_hour(-500.0, &bat, 12);
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    #[test]
    fn soc_limit_reached_stops_active_charging() {
        // Already charging, but battery now reports socLimit: 1
        let mut ctrl = controller_in_mode(ControlMode::Charge, Duration::from_secs(60));
        let mut bat = battery(99);
        bat.soc_limit_reached = true;
        let decision = ctrl.decide_at_hour(-500.0, &bat, 12);
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    #[test]
    fn soc_limit_not_reached_allows_charging() {
        // Battery reports socLimit: 0 at 99% — charging allowed
        let mut ctrl = controller_no_cooldown();
        let bat = battery(99); // soc_limit_reached: false
        let decision = ctrl.decide_at_hour(-500.0, &bat, 12);
        assert_eq!(decision.mode, ControlMode::Charge);
    }

    #[test]
    fn soc_limit_does_not_block_discharge() {
        // socLimit should only affect charging, not discharging
        let mut ctrl = controller_no_cooldown();
        let mut bat = battery(99);
        bat.soc_limit_reached = true;
        let decision = ctrl.decide_at_hour(400.0, &bat, 20);
        assert_eq!(decision.mode, ControlMode::Discharge);
    }

    // --- Hysteresis with battery feedback tests ---

    #[test]
    fn charge_hysteresis_with_battery_draw_prevents_oscillation() {
        // Battery charging at 300W. Grid meter reads -20W (small export).
        // underlying_grid = -20 + (-300) = -320W → well below 0 → keeps charging.
        // Without hysteresis (threshold -100): -320 < -100 → would also charge.
        // The real value of hysteresis shows when grid reads +80W:
        // underlying = 80 + (-300) = -220W. Without hysteresis: -220 < -100 → charge.
        // But what about +80 from idle? underlying = 80 → not < -100 → idle. Good.
        //
        // Key scenario: grid reads -20W while charging 300W.
        // From idle this would be: underlying = -20, -20 > -100 → idle (correct, too little export).
        // While charging: underlying = -20 + (-300) = -320, -320 < 0 → keep charging (correct).
        let mut ctrl = controller_in_mode(ControlMode::Charge, Duration::from_secs(60));
        let bat = battery_charging(50, 300);
        let decision = ctrl.decide_at_hour(-20.0, &bat, 12);
        assert_eq!(decision.mode, ControlMode::Charge);
    }

    #[test]
    fn discharge_hysteresis_with_battery_output_prevents_oscillation() {
        // Battery discharging 400W. Grid reads -30W (slight export = overshot).
        // underlying_grid = -30 + 400 = 370W → still > 0 → keep discharging.
        // From idle: underlying = -30 → not > 0 threshold → idle. Hysteresis prevents flip.
        let mut ctrl = controller_in_mode(ControlMode::Discharge, Duration::from_secs(60));
        let bat = battery_discharging(50, 400);
        let decision = ctrl.decide_at_hour(-30.0, &bat, 20);
        assert_eq!(decision.mode, ControlMode::Discharge);
    }

    // --- Integration scenario tests ---

    #[test]
    fn charging_continues_when_own_draw_reduces_export() {
        // Solar 250W, house 100W. Battery already charging at 75W.
        // Grid reads -75W (= -150 + 75 from battery draw).
        // Without the battery, export would be -150W → still above charge threshold.
        // Bug: controller sees -75W > -100W threshold → incorrectly goes Idle.
        let mut ctrl = controller_in_mode(ControlMode::Charge, Duration::from_secs(60));
        let bat = battery_charging(50, 75);
        let decision = ctrl.decide_at_hour(-75.0, &bat, 12);
        assert_eq!(decision.mode, ControlMode::Charge);
    }

    #[test]
    fn discharging_continues_when_own_output_reduces_import() {
        // House 300W, battery already discharging 250W.
        // Grid reads 50W (= 300 - 250 from battery output).
        // Without battery, import would be 300W → still above discharge threshold.
        // Same bug pattern: raw grid_power near threshold causes toggling.
        let mut ctrl = controller_in_mode(ControlMode::Discharge, Duration::from_secs(60));
        let bat = battery_discharging(50, 250);
        let decision = ctrl.decide_at_hour(50.0, &bat, 20);
        assert_eq!(decision.mode, ControlMode::Discharge);
    }

    /// Simulates a discharge scenario with Shelly Pro 3EM providing
    /// direct signed grid power every second.
    ///
    /// Scenario: House consuming 150W, battery idle long enough.
    /// The battery should ramp up from idle to ~150W discharge, covering the
    /// house demand and bringing net grid power close to zero.
    #[test]
    fn discharge_converges_to_house_consumption() {
        let mut ctrl = controller_no_cooldown();

        let house_total = 150.0_f64;
        let hour = 12;

        // Step 1: Battery idle, house importing 150W from grid.
        let d1 = ctrl.decide_at_hour(150.0, &battery(80), hour);
        assert_eq!(d1.mode, ControlMode::Discharge, "step 1: should discharge");
        // Idle → Discharge mode change → 75% ramp: (150 - 5) × 0.75 = 108W
        assert_eq!(d1.power_watts, 108, "step 1: ramped first decision");
        let battery_discharge = d1.power_watts;

        // Step 2: Battery discharging 108W, grid still importing 42W.
        let net = house_total - battery_discharge as f64; // 42W
        let bat = battery_discharging(80, battery_discharge);
        let d2 = ctrl.decide_at_hour(net, &bat, hour);
        assert_eq!(d2.mode, ControlMode::Discharge, "step 2: still discharging");
        // Same mode, no ramp: 108 + (42 - 5) = 145W
        assert_eq!(d2.power_watts, 145, "step 2: converging");
        let battery_discharge = d2.power_watts;

        // Step 3: Battery at 145W. Net = 150-145 = 5W (nearly balanced).
        let net = house_total - battery_discharge as f64; // 5W
        let bat = battery_discharging(80, battery_discharge);
        let d3 = ctrl.decide_at_hour(net, &bat, hour);
        assert_eq!(d3.mode, ControlMode::Discharge, "step 3: still discharging");
        // Same mode: 145 + (5 - 5) = 145W — stable!
        assert_eq!(d3.power_watts, 145, "step 3: steady state");

        // Final check: battery is discharging within the discharge margin of house demand.
        let final_net = house_total - d3.power_watts as f64;
        assert!(
            final_net.abs() < 10.0,
            "final net should be near zero, got {final_net:.0}W"
        );
    }
}
