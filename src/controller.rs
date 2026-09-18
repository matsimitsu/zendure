use std::time::Duration;

use chrono::Weekday;
use serde::{Deserialize, Serialize};

use crate::battery::BatteryState;
use crate::clock::Clock;
use crate::config::{Config, SessionConfig};
use crate::models::{ControlDecision, ControlMode, CycleCounts};
use crate::units::{Elapsed, GridPower, PowerMargin, Setpoint, Soc, SolarPower, Timestamp};
use crate::world::World;

/// Fraction of the target commanded on the first decision after a mode change.
/// Easing into a new direction rather than stepping straight to full power is a
/// battery-safety measure, and the convention most BMS implementations follow.
const RAMP_FACTOR: f64 = 0.75;

/// Mutable history, split from the config fields (which live in the journal's
/// `sessions` row) so a decision can be replayed: hysteresis/cooldown history,
/// idle timers, and daily counters. Dropping a field replays silently wrong
/// rather than failing loudly; the equivalence test in this module catches that.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControllerState {
    pub last_mode: ControlMode,
    pub last_active_mode: Option<ControlMode>,
    pub last_mode_change: Timestamp,
    pub last_decision: Timestamp,
    pub last_idle_start: Option<Timestamp>,
    pub daily_transitions: u32,
    pub daily_cooldown_suppressions: u32,
    pub last_cycle_reset_day: u32,
}

/// Reactive self-consumption control. Deliberately free of clock reads: every
/// decision takes a [`Clock`] captured at the edge, which is what makes the
/// time-dependent branches (cooldown, standby, midnight reset, balance weekday)
/// testable and a recorded event stream replayable.
pub struct Controller {
    /// Everything that changes as decisions are made. Held as one field rather
    /// than spelled out again here: the list already exists on
    /// [`ControllerState`], and a second copy is a second thing to keep in step.
    state: ControllerState,
    min_mode_duration: Duration,
    min_decision_interval: Duration,
    charge_margin: PowerMargin,
    discharge_margin: PowerMargin,
    charge_start_threshold: GridPower,
    discharge_start_threshold: GridPower,
    idle_timeout: Duration,
    min_idle_before_discharge: Duration,
    cycle_warn_threshold: u32,
    min_soc: Soc,
    max_soc: Soc,
    balance_weekday: Option<Weekday>,
    solar_discharge_block_threshold: SolarPower,
}

impl Controller {
    pub fn from_config(config: &Config, clock: &Clock) -> Self {
        Self::from_session(&config.session(), clock)
    }

    /// Builds from `SessionConfig`'s tuning fields alone (`mqtt_timeout_secs`
    /// belongs to `Engine`), so a fixture with no `Config` can construct an
    /// equivalent controller for replay. `Config::session`'s exhaustive
    /// destructure turns a new field into a compile error here, keeping one knob list.
    pub fn from_session(session: &SessionConfig, clock: &Clock) -> Self {
        let min_mode_duration = Duration::from_secs(session.min_mode_duration_secs);
        let min_decision_interval = Duration::from_secs(session.min_decision_interval_secs);
        Self {
            state: ControllerState {
                last_mode: ControlMode::Idle,
                last_active_mode: None,
                last_mode_change: clock.now - Elapsed::of(min_mode_duration),
                last_decision: clock.now - Elapsed::of(min_decision_interval),
                last_idle_start: Some(clock.now),
                daily_transitions: 0,
                daily_cooldown_suppressions: 0,
                last_cycle_reset_day: clock.day_ordinal,
            },
            min_mode_duration,
            min_decision_interval,
            charge_margin: session.charge_margin,
            discharge_margin: session.discharge_margin,
            charge_start_threshold: session.charge_start_threshold,
            discharge_start_threshold: session.discharge_start_threshold,
            idle_timeout: Duration::from_secs(session.idle_timeout_secs),
            min_idle_before_discharge: Duration::from_secs(session.min_idle_before_discharge_secs),
            cycle_warn_threshold: session.cycle_warn_threshold,
            min_soc: session.min_soc,
            max_soc: session.max_soc,
            balance_weekday: session.balance_weekday,
            solar_discharge_block_threshold: session.solar_discharge_block_threshold,
        }
    }

    pub fn cycle_counts(&self) -> CycleCounts {
        CycleCounts {
            daily_transitions: self.state.daily_transitions,
            daily_cooldown_suppressions: self.state.daily_cooldown_suppressions,
        }
    }

    /// Snapshot the mutable history. Recorded with every decision, so "the state
    /// at time T" is the last decision row at or before T.
    pub fn state(&self) -> ControllerState {
        self.state.clone()
    }

    /// Put a snapshot back. The config half is whatever this controller was
    /// built with — `restore` deliberately cannot change it, so a replay against
    /// different tuning is a different `Controller`, constructed as such, rather
    /// than a half-overwritten one.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn restore(&mut self, state: ControllerState) {
        self.state = state;
    }
    /// A controller with permissive defaults (no cooldown-relevant history,
    /// generous margins) for tests outside this module, e.g. `engine.rs`'s.
    /// This module's own tests build fixtures with more control via
    /// `default_controller`/`controller_in_mode` below.
    #[cfg(test)]
    pub(crate) fn test_default(now_ms: i64, day_ordinal: u32) -> Self {
        let now = Timestamp::from_millis(now_ms);
        let clock = Clock {
            day_ordinal,
            ..Clock::test_at(now_ms)
        };
        // Via `from_session` so knobs match the fixture list; only the history
        // differs, deliberately: a minute of slack on both cooldowns (past
        // `min_mode_duration` and `min_decision_interval`), and no idle start,
        // so a test's first event is never suppressed by timing it didn't ask about.
        let mut controller = Self::from_session(&SessionConfig::test_default(), &clock);
        controller.state.last_mode_change = now - Elapsed::of(Duration::from_secs(60));
        controller.state.last_decision = now - Elapsed::of(Duration::from_secs(60));
        controller.state.last_idle_start = None;
        controller
    }

    /// Returns `None` if the minimum decision interval hasn't elapsed, or if there
    /// is no battery to control.
    pub fn decide(&mut self, world: &World, clock: &Clock) -> Option<ControlDecision> {
        if clock.now - self.state.last_decision < self.min_decision_interval {
            return None;
        }

        // Unreachable in production — main.rs seeds the world from the startup
        // poll, which fails startup if it fails — but the device map makes it
        // representable, and "no decision this tick" is already a state every
        // caller handles.
        let battery = world.battery()?;

        Some(self.decide_at(world, battery, clock))
    }

    /// `battery` is resolved by the caller: this objective is written for
    /// exactly one battery. When a second one lands, that resolution moves to
    /// the allocator, not here.
    pub(crate) fn decide_at(
        &mut self,
        world: &World,
        battery: &BatteryState,
        clock: &Clock,
    ) -> ControlDecision {
        self.state.last_decision = clock.now;

        let grid_power = world.grid.total;

        // 0. SOC calibration — reported SOC is unreliable, stay idle
        if battery.soc_calibrating {
            tracing::info!("SOC calibration in progress, idling");
            self.force_idle(clock);
            return ControlDecision {
                mode: ControlMode::Idle,
                power_watts: Setpoint::ZERO,
                reason: "SOC calibration in progress, idling".to_string(),
                grid_power,
            };
        }

        // 0b. Device-reported fault — stay idle. Idle only writes inputLimit=0 /
        // outputLimit=0, never chargeMaxLimit/inverseMaxPower, so a fault that
        // zeroed those setpoints is left at 0 rather than overwritten by a
        // forced charge.
        if battery.fault {
            tracing::warn!("Device reports a fault, idling");
            self.force_idle(clock);
            return ControlDecision {
                mode: ControlMode::Idle,
                power_watts: Setpoint::ZERO,
                reason: "Device reports a fault, idling".to_string(),
                grid_power,
            };
        }

        let mode = self.target_mode(world, battery, clock);

        let power = self.target_power(mode, grid_power, battery);

        // 3. Apply guards (cooldown, ramp, standby timeout)
        self.apply_guards(mode, power, grid_power, clock)
    }

    /// Drop to idle without going through the guards — used by the overrides
    /// (calibration, fault) that bypass normal mode selection entirely.
    fn force_idle(&mut self, clock: &Clock) {
        if self.state.last_mode != ControlMode::Idle {
            self.state.last_mode = ControlMode::Idle;
            self.state.last_mode_change = clock.now;
            self.state.last_idle_start = Some(clock.now);
        }
    }

    /// Determines the desired mode from grid state, battery SOC, and time.
    /// Hysteresis: starting charge/discharge needs the full threshold, but
    /// once active only crossing back past 0W stops it, since the meter
    /// reading includes the battery's own effect and would otherwise oscillate near the
    /// start threshold.
    fn target_mode(&self, world: &World, battery: &BatteryState, clock: &Clock) -> ControlMode {
        // Adjust for battery's own grid effect: the meter reading includes
        // the battery's consumption (charging) or production (discharging).
        let underlying_grid = world.underlying_grid();

        // Hysteresis: once charging, keep going as long as we're still exporting (<
        // 0W).
        // Only require the full start threshold to *begin* charging.
        let charge_threshold = if self.state.last_mode == ControlMode::Charge {
            GridPower::ZERO
        } else {
            self.charge_start_threshold
        };

        // Exporting to grid, battery below max SOC, and battery accepts charge → charge
        let max_soc = self.effective_max_soc(clock.weekday);
        if battery.soc < max_soc && !battery.soc_limit_reached && underlying_grid < charge_threshold
        {
            return ControlMode::Charge;
        }

        // Hysteresis: once discharging, keep going as long as we're still importing (>
        // 0W).
        let discharge_threshold = if self.state.last_mode == ControlMode::Discharge {
            GridPower::ZERO
        } else {
            self.discharge_start_threshold
        };

        // Require minimum idle duration before discharge, but only when the
        // last active mode was Charge. This prevents charge→idle→discharge
        // oscillation during variable solar, while allowing discharge to
        // resume quickly after a brief idle (e.g. demand dip).
        let needs_idle_guard = self.state.last_active_mode == Some(ControlMode::Charge);
        let idle_long_enough = if self.state.last_mode == ControlMode::Discharge {
            true
        } else if needs_idle_guard {
            self.state
                .last_idle_start
                .is_some_and(|t| clock.now - t >= self.min_idle_before_discharge)
        } else {
            true
        };

        // Solar production guard: while the solar inverter is exporting at or
        // above the configured threshold, skip discharge so large loads (e.g. an
        // EV charger) pull from grid+solar instead of draining the home battery.
        let solar_below_block = self.solar_below_block(world.solar);

        // Importing from grid and battery above min SOC → discharge
        if idle_long_enough
            && solar_below_block
            && battery.soc > self.min_soc
            && underlying_grid > discharge_threshold
        {
            return ControlMode::Discharge;
        }

        ControlMode::Idle
    }

    /// Max SOC for `weekday`, raised to 100% on `balance_weekday` so the pack
    /// gets a periodic full charge for cell balancing even if `max_soc` is
    /// normally kept lower for longevity.
    fn effective_max_soc(&self, weekday: Weekday) -> Soc {
        if self.balance_weekday == Some(weekday) {
            Soc::FULL
        } else {
            self.max_soc
        }
    }

    /// True when the solar discharge-block guard doesn't apply: either it's
    /// disabled (0 is the sentinel) or production is below the threshold.
    fn solar_below_block(&self, solar_power: SolarPower) -> bool {
        self.solar_discharge_block_threshold == SolarPower::ZERO
            || solar_power < self.solar_discharge_block_threshold
    }

    /// Calculates the target power for a given mode, accounting for battery
    /// feedback (what it's already doing) and safety margins.
    fn target_power(
        &self,
        mode: ControlMode,
        grid_power: GridPower,
        battery: &BatteryState,
    ) -> Setpoint {
        match mode {
            ControlMode::Charge => {
                let adjustment = grid_power.exporting() - self.charge_margin.watts();
                let current_charge = battery.current_power.charging();
                Setpoint::clamped(current_charge + adjustment, battery.max_charge_power)
            }
            ControlMode::Discharge => {
                let adjustment = grid_power.importing() - self.discharge_margin.watts();
                let current_discharge = battery.current_power.discharging();
                Setpoint::clamped(current_discharge + adjustment, battery.max_discharge_power)
            }
            ControlMode::Idle | ControlMode::Standby => Setpoint::ZERO,
        }
    }

    /// Applies cooldown, ramp, and standby timeout. All state mutation lives here.
    fn apply_guards(
        &mut self,
        mode: ControlMode,
        power: Setpoint,
        grid_power: GridPower,
        clock: &Clock,
    ) -> ControlDecision {
        // Reset daily counters at midnight
        if clock.day_ordinal != self.state.last_cycle_reset_day {
            self.state.daily_transitions = 0;
            self.state.daily_cooldown_suppressions = 0;
            self.state.last_cycle_reset_day = clock.day_ordinal;
        }

        // Cycle limit: force standby when daily transitions exceed threshold
        if self.cycle_warn_threshold > 0
            && self.state.daily_transitions >= self.cycle_warn_threshold
        {
            if self.state.last_mode != ControlMode::Standby {
                tracing::warn!(
                    "Daily cycle limit reached ({} transitions) — entering standby until midnight",
                    self.state.daily_transitions,
                );
                self.state.last_mode = ControlMode::Standby;
                self.state.last_mode_change = clock.now;
                self.state.last_idle_start = None;
            }
            return ControlDecision {
                mode: ControlMode::Standby,
                power_watts: Setpoint::ZERO,
                reason: format!(
                    "Cycle limit: {} transitions today (max {}), standby until midnight",
                    self.state.daily_transitions, self.cycle_warn_threshold,
                ),
                grid_power,
            };
        }

        // Cooldown: suppress charge↔discharge toggles that happen too fast
        let in_mode = clock.now - self.state.last_mode_change;
        if is_opposing_switch(self.state.last_mode, mode) && in_mode < self.min_mode_duration {
            self.state.daily_cooldown_suppressions += 1;
            if self.state.last_mode != ControlMode::Idle {
                self.state.last_idle_start = Some(clock.now);
            }
            return ControlDecision {
                mode: ControlMode::Idle,
                power_watts: Setpoint::ZERO,
                reason: format!(
                    "Cooldown: suppressed {} (was {} for {:.0}s, min {}s)",
                    mode,
                    self.state.last_mode,
                    in_mode.as_secs_f64(),
                    self.min_mode_duration.as_secs(),
                ),
                grid_power,
            };
        }

        // Track mode changes and apply ramp
        let (final_power, ramped) = if mode != self.state.last_mode {
            self.state.daily_transitions += 1;
            if matches!(mode, ControlMode::Charge | ControlMode::Discharge) {
                self.state.last_active_mode = Some(mode);
            }
            self.state.last_mode = mode;
            self.state.last_mode_change = clock.now;
            self.state.last_idle_start = if mode == ControlMode::Idle {
                Some(clock.now)
            } else {
                None
            };
            // Ramp: 75% power on first decision after mode change
            if power.is_positive() {
                (power.ramped(RAMP_FACTOR), true)
            } else {
                (power, false)
            }
        } else {
            (power, false)
        };

        // Idle timeout → standby
        if mode == ControlMode::Idle
            && let Some(idle_start) = self.state.last_idle_start
            && clock.now - idle_start >= self.idle_timeout
        {
            return ControlDecision {
                mode: ControlMode::Standby,
                power_watts: Setpoint::ZERO,
                reason: format!(
                    "Idle for {}+ minutes, entering standby",
                    self.idle_timeout.as_secs() / 60,
                ),
                grid_power,
            };
        }

        let reason = build_reason(mode, final_power, grid_power, clock.hour, ramped);
        ControlDecision {
            mode,
            power_watts: final_power,
            reason,
            grid_power,
        }
    }
}

fn build_reason(
    mode: ControlMode,
    power: Setpoint,
    grid_power: GridPower,
    hour: u32,
    ramped: bool,
) -> String {
    let ramp = if ramped { " (ramped 75%)" } else { "" };
    match mode {
        ControlMode::Charge => {
            format!(
                "Solar excess: exporting {:.0}W, charging at {power}W{ramp}",
                grid_power.exporting(),
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
#[path = "controller_tests.rs"]
mod tests;
