use std::time::Duration;

use chrono::Weekday;
use serde::{Deserialize, Serialize};

use crate::battery::BatteryState;
use crate::clock::Clock;
use crate::config::{Config, SessionConfig};
use crate::models::{ControlDecision, ControlMode, CycleCounts};
use crate::units::{Elapsed, GridPower, PowerMargin, Setpoint, Soc, SolarPower, Timestamp};
use crate::world::World;

const RAMP_FACTOR: f64 = 0.75;

/// The controller's mutable history, split out so a decision can be replayed.
///
/// `Controller`'s other fields are configuration — read once from [`Config`] and
/// never written — so they belong to the session, not to the moment, and travel
/// in the journal's `sessions` row instead of on every decision. What is left
/// here is exactly the state that makes the same input produce a different
/// output depending on what came before: the hysteresis and cooldown history,
/// the idle timers, and the daily counters.
///
/// Restoring these into a controller built from the same config reproduces the
/// decision. Dropping any one of them does not fail loudly — it replays *almost*
/// right, which is worse, so the equivalence test in this module exists to make
/// an omission fail.
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

    /// Build from the tuning half alone — thirteen of `SessionConfig`'s
    /// fourteen fields, taken from the shape the journal records and a fixture
    /// carries. The fourteenth, `mqtt_timeout_secs`, belongs to `Engine` rather
    /// than here.
    ///
    /// This is what makes "a fixture is hermetic" true rather than asserted: a
    /// replay has no `Config` and must not need one, and a controller that can
    /// be *constructed* from the fixture's own tuning cannot be secretly
    /// reading a connection setting. It does not make the controller pure —
    /// `decide` still reads a `World` and a `Clock`, both of which arrive with
    /// the events. What keeps the knob list honest is `Config::session`'s
    /// exhaustive destructure, which turns a new field into a compile error
    /// there; routing `from_config` through here means there is one list rather
    /// than two that have to agree.
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
        // Through `from_session` so the knobs come from the one list a fixture
        // would also carry. Only the history differs from a freshly started
        // controller, and it differs deliberately: a minute of slack on both
        // cooldowns — comfortably past `min_mode_duration` and
        // `min_decision_interval` — and no idle start, so a test's first event
        // is never suppressed by timing it did not ask about.
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

    /// `battery` is resolved by the caller rather than looked up here: this
    /// objective is written for exactly one battery, and saying so at the
    /// signature is more honest than hiding a `.next()` inside the pipeline.
    /// When a second one lands, that resolution moves into the allocator, not
    /// here.
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
        // outputLimit=0, so we neither command power nor overwrite the device's
        // chargeMaxLimit/inverseMaxPower setpoints. If a genuine fault zeroed
        // those limits, we leave them at 0 and stand down rather than forcing a
        // charge.
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

    /// Determines the desired mode based on grid state, battery SOC, and time.
    ///
    /// Uses hysteresis: the threshold to *start* charging/discharging is more
    /// aggressive than the threshold to *keep* doing so. This prevents
    /// oscillation when the battery's own grid effect pushes the meter reading
    /// close to the start threshold.
    fn target_mode(&self, world: &World, battery: &BatteryState, clock: &Clock) -> ControlMode {
        // Adjust for battery's own grid effect: the meter reading includes
        // the battery's consumption (charging) or production (discharging).
        let underlying_grid = world.underlying_grid();

        // Hysteresis: once charging, keep going as long as we're still exporting (< 0W).
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

        // Hysteresis: once discharging, keep going as long as we're still importing (> 0W).
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
mod tests {
    use super::*;
    use crate::units::{BatteryPower, PowerCap};
    use crate::world::{DeviceId, Measurement, MeterReading};

    /// A snapshot whose every field differs from a freshly built controller's,
    /// so that dropping any one of them from `state`/`restore` is visible.
    /// The values are arbitrary and deliberately all distinct.
    fn distinctive_state() -> ControllerState {
        ControllerState {
            last_mode: ControlMode::Discharge,
            last_active_mode: Some(ControlMode::Charge),
            last_mode_change: Timestamp::from_millis(11),
            last_decision: Timestamp::from_millis(22),
            last_idle_start: Some(Timestamp::from_millis(33)),
            daily_transitions: 44,
            daily_cooldown_suppressions: 55,
            last_cycle_reset_day: 66,
        }
    }

    /// `state` and `restore` have to be exact inverses, field for field.
    #[test]
    fn state_and_restore_are_exact_inverses() {
        let mut controller = Controller::test_default(NOW_MS, DAY);
        assert_ne!(
            controller.state(),
            distinctive_state(),
            "fixture must differ from a fresh controller or this proves nothing"
        );

        controller.restore(distinctive_state());
        assert_eq!(controller.state(), distinctive_state());
    }

    /// The snapshot is JSON in the journal, so the trip through serde has to
    /// close too — including `last_mode`, whose `Deserialize` exists only for
    /// this, and the `Option` fields, where a `None`/absent mix-up would read
    /// back as a controller that had never idled.
    #[test]
    fn controller_state_round_trips_through_json() {
        let state = distinctive_state();
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(state, serde_json::from_str(&json).unwrap());

        let fresh = Controller::test_default(NOW_MS, DAY).state();
        let json = serde_json::to_string(&fresh).unwrap();
        assert_eq!(fresh, serde_json::from_str(&json).unwrap());
    }

    /// A one-battery world, the shape every test in this module decides against.
    /// `MeterReading::total_only` zeroes the phases, which is exact rather than
    /// approximate: nothing in the objective reads a phase.
    fn world(grid_power: GridPower, solar_power: SolarPower, battery: &BatteryState) -> World {
        let mut world = World::new();
        world.observe_meter(MeterReading::total_only(grid_power), solar_power);
        world.observe_device(
            DeviceId::new("test-battery"),
            Measurement::Battery(battery.clone()),
        );
        world
    }

    /// `decide_at` with the battery resolved from the world, for the tests that
    /// want a decision without the min-interval guard. An absent battery is a
    /// broken fixture, not a runtime state, so it panics here — the production
    /// path returns `None` instead (see `decide`).
    fn decide_at(ctrl: &mut Controller, world: &World, clock: &Clock) -> ControlDecision {
        let battery = world.battery().expect("test world has no battery").clone();
        ctrl.decide_at(world, &battery, clock)
    }

    fn battery(soc: u32) -> BatteryState {
        BatteryState {
            soc: Soc::new(soc),
            max_discharge_power: PowerCap::new(800),
            max_charge_power: PowerCap::new(2400),
            current_power: BatteryPower::ZERO,
            soc_calibrating: false,
            soc_limit_reached: false,
            fault: false,
        }
    }

    fn battery_discharging(soc: u32, power: i32) -> BatteryState {
        BatteryState {
            soc: Soc::new(soc),
            max_discharge_power: PowerCap::new(800),
            max_charge_power: PowerCap::new(2400),
            current_power: BatteryPower(power),
            soc_calibrating: false,
            soc_limit_reached: false,
            fault: false,
        }
    }

    fn battery_charging(soc: u32, power: i32) -> BatteryState {
        BatteryState {
            soc: Soc::new(soc),
            max_discharge_power: PowerCap::new(800),
            max_charge_power: PowerCap::new(2400),
            current_power: BatteryPower(-power),
            soc_calibrating: false,
            soc_limit_reached: false,
            fault: false,
        }
    }

    /// Fixed reference "now" for every test. Stored instants are `NOW_MS -
    /// elapsed`, and decisions are made with `clock(hour)` — so nothing here
    /// depends on the real clock or on how long the test takes to run.
    const NOW_MS: i64 = 1_000_000_000;
    const MINUTE_MS: i64 = 60_000;

    /// Fixed day-of-year, matching each fixture's `last_cycle_reset_day`, so the
    /// midnight reset only fires in the test that asks for it.
    const DAY: u32 = 100;

    /// Fixed weekday. `default_controller` leaves `balance_weekday: None`, so
    /// this only matters to the balance-day tests.
    const WEEKDAY: Weekday = Weekday::Wed;

    fn clock(hour: u32) -> Clock {
        Clock {
            now: Timestamp::from_millis(NOW_MS),
            hour,
            day_ordinal: DAY,
            weekday: WEEKDAY,
        }
    }

    /// A clock on a different calendar day, for the midnight-rollover tests.
    fn clock_on_day(hour: u32, day_ordinal: u32) -> Clock {
        Clock {
            day_ordinal,
            ..clock(hour)
        }
    }

    fn default_controller() -> Controller {
        Controller::test_default(NOW_MS, DAY)
    }

    /// Controller with no cooldown and no idle-before-discharge requirement
    /// (for tests that only care about mode/power logic).
    fn controller_no_cooldown() -> Controller {
        let base = default_controller();
        Controller {
            min_mode_duration: Duration::ZERO,
            min_idle_before_discharge: Duration::ZERO,
            state: ControllerState {
                last_idle_start: Some(Timestamp::from_millis(NOW_MS - MINUTE_MS)),
                ..base.state
            },
            ..base
        }
    }

    /// Controller that has been in `mode` for the given duration. Takes a
    /// `Duration` so the call sites read the same as they always have.
    fn controller_in_mode(mode: ControlMode, elapsed: Duration) -> Controller {
        let mode_change = Timestamp::from_millis(NOW_MS) - Elapsed::of(elapsed);
        let base = default_controller();
        Controller {
            state: ControllerState {
                last_mode: mode,
                last_active_mode: match mode {
                    ControlMode::Charge | ControlMode::Discharge => Some(mode),
                    _ => None,
                },
                last_mode_change: mode_change,
                last_idle_start: match mode {
                    ControlMode::Idle | ControlMode::Standby => Some(mode_change),
                    _ => None,
                },
                ..base.state
            },
            ..base
        }
    }

    // --- Mode selection tests ---

    #[test]
    fn soc_at_max_never_charges() {
        let mut ctrl = controller_no_cooldown();
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(-500.0), SolarPower::new(0.0), &battery(100)),
            &clock(20),
        );
        assert_ne!(decision.mode, ControlMode::Charge);
    }

    #[test]
    fn soc_below_max_can_charge() {
        let mut ctrl = controller_no_cooldown();
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(-500.0), SolarPower::new(0.0), &battery(99)),
            &clock(12),
        );
        assert_eq!(decision.mode, ControlMode::Charge);
    }

    #[test]
    fn soc_at_max_can_still_discharge() {
        let mut ctrl = controller_no_cooldown();
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(400.0), SolarPower::new(0.0), &battery(100)),
            &clock(20),
        );
        assert_eq!(decision.mode, ControlMode::Discharge);
    }

    #[test]
    fn soc_at_min_never_discharges() {
        let mut ctrl = controller_no_cooldown();
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(400.0), SolarPower::new(0.0), &battery(10)),
            &clock(20),
        );
        assert_ne!(decision.mode, ControlMode::Discharge);
    }

    #[test]
    fn soc_above_min_can_discharge() {
        let mut ctrl = controller_no_cooldown();
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(400.0), SolarPower::new(0.0), &battery(11)),
            &clock(20),
        );
        assert_eq!(decision.mode, ControlMode::Discharge);
    }

    #[test]
    fn charging_transitions_to_idle_when_soc_reaches_max() {
        let mut ctrl = controller_no_cooldown();
        // Start charging at 99%
        let d1 = decide_at(
            &mut ctrl,
            &world(GridPower(-500.0), SolarPower::new(0.0), &battery(99)),
            &clock(12),
        );
        assert_eq!(d1.mode, ControlMode::Charge);

        // SOC reaches 100% → should stop charging and go idle
        let d2 = decide_at(
            &mut ctrl,
            &world(GridPower(-500.0), SolarPower::new(0.0), &battery(100)),
            &clock(12),
        );
        assert_eq!(d2.mode, ControlMode::Idle);
    }

    #[test]
    fn discharging_transitions_to_idle_when_soc_reaches_min() {
        let mut ctrl = controller_no_cooldown();
        // Start discharging at 11%
        let d1 = decide_at(
            &mut ctrl,
            &world(GridPower(400.0), SolarPower::new(0.0), &battery(11)),
            &clock(20),
        );
        assert_eq!(d1.mode, ControlMode::Discharge);

        // SOC drops to 10% → should stop discharging and go idle
        let d2 = decide_at(
            &mut ctrl,
            &world(GridPower(400.0), SolarPower::new(0.0), &battery(10)),
            &clock(20),
        );
        assert_eq!(d2.mode, ControlMode::Idle);
    }

    #[test]
    fn soc_at_min_can_still_charge() {
        let mut ctrl = controller_no_cooldown();
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(-500.0), SolarPower::new(0.0), &battery(10)),
            &clock(12),
        );
        assert_eq!(decision.mode, ControlMode::Charge);
    }

    #[test]
    fn idle_within_deadband() {
        let mut ctrl = controller_no_cooldown();
        // 0W is at discharge_start_threshold (0W) and above charge_start_threshold (-100W)
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(0.0), SolarPower::new(0.0), &battery(50)),
            &clock(12),
        );
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    #[test]
    fn no_discharge_after_charge_before_idle_duration_met() {
        let mut ctrl = controller_no_cooldown();
        // Last active mode was Charge, recently went idle — guard applies
        ctrl.state.last_active_mode = Some(ControlMode::Charge);
        ctrl.state.last_idle_start = Some(Timestamp::from_millis(NOW_MS - 60_000));
        ctrl.min_idle_before_discharge = Duration::from_secs(300);
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(400.0), SolarPower::new(0.0), &battery(80)),
            &clock(12),
        );
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    #[test]
    fn discharge_allowed_after_charge_when_idle_duration_met() {
        let mut ctrl = controller_no_cooldown();
        // Last active mode was Charge, but idle long enough
        ctrl.state.last_active_mode = Some(ControlMode::Charge);
        ctrl.state.last_idle_start = Some(Timestamp::from_millis(NOW_MS - 600_000));
        ctrl.min_idle_before_discharge = Duration::from_secs(300);
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(400.0), SolarPower::new(0.0), &battery(80)),
            &clock(12),
        );
        assert_eq!(decision.mode, ControlMode::Discharge);
    }

    #[test]
    fn discharge_resumes_quickly_after_idle() {
        // Last active mode was Discharge, briefly went idle — no guard needed
        let mut ctrl = controller_in_mode(ControlMode::Idle, Duration::from_secs(5));
        ctrl.state.last_active_mode = Some(ControlMode::Discharge);
        ctrl.min_idle_before_discharge = Duration::from_secs(300);
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(400.0), SolarPower::new(0.0), &battery(80)),
            &clock(12),
        );
        assert_eq!(decision.mode, ControlMode::Discharge);
    }

    #[test]
    fn discharge_allowed_when_already_discharging() {
        // Already discharging — should keep going regardless of idle duration
        let mut ctrl = controller_in_mode(ControlMode::Discharge, Duration::from_secs(60));
        ctrl.min_idle_before_discharge = Duration::from_secs(300);
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(400.0), SolarPower::new(0.0), &battery(80)),
            &clock(12),
        );
        assert_eq!(decision.mode, ControlMode::Discharge);
    }

    #[test]
    fn deadband_no_charge_at_minus_80() {
        let mut ctrl = controller_no_cooldown();
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(-80.0), SolarPower::new(0.0), &battery(50)),
            &clock(12),
        );
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    #[test]
    fn deadband_no_discharge_at_zero() {
        let mut ctrl = controller_no_cooldown();
        // 0W grid power is not > 0 threshold → idle
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(0.0), SolarPower::new(0.0), &battery(50)),
            &clock(20),
        );
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    // --- Power calculation tests ---

    #[test]
    fn charges_on_solar_excess() {
        let mut ctrl = controller_in_mode(ControlMode::Charge, Duration::from_secs(60));
        // Grid at -300W, margin=50W → (300-50) = 250W
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(-300.0), SolarPower::new(0.0), &battery(50)),
            &clock(12),
        );
        assert_eq!(decision.mode, ControlMode::Charge);
        assert_eq!(decision.power_watts, Setpoint::new(250));
    }

    #[test]
    fn charge_power_capped_by_battery_limit() {
        let state = BatteryState {
            soc: Soc::new(50),
            max_discharge_power: PowerCap::new(800),
            max_charge_power: PowerCap::new(1000),
            current_power: BatteryPower::ZERO,
            soc_calibrating: false,
            soc_limit_reached: false,
            fault: false,
        };
        let mut ctrl = controller_in_mode(ControlMode::Charge, Duration::from_secs(60));
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(-1500.0), SolarPower::new(0.0), &state),
            &clock(12),
        );
        assert_eq!(decision.power_watts, Setpoint::new(1000));
    }

    #[test]
    fn discharge_capped_by_battery_limit() {
        let state = BatteryState {
            soc: Soc::new(80),
            max_discharge_power: PowerCap::new(500),
            max_charge_power: PowerCap::new(2400),
            current_power: BatteryPower::ZERO,
            soc_calibrating: false,
            soc_limit_reached: false,
            fault: false,
        };
        let mut ctrl = controller_in_mode(ControlMode::Discharge, Duration::from_secs(60));
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(1000.0), SolarPower::new(0.0), &state),
            &clock(20),
        );
        assert_eq!(decision.power_watts, Setpoint::new(500));
    }

    #[test]
    fn charge_margin_reduces_power() {
        let mut ctrl = controller_in_mode(ControlMode::Charge, Duration::from_secs(60));
        ctrl.charge_margin = PowerMargin::new(100);
        // Grid at -400W, margin=100W → (400-100) = 300W
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(-400.0), SolarPower::new(0.0), &battery(50)),
            &clock(12),
        );
        assert_eq!(decision.power_watts, Setpoint::new(300));
    }

    #[test]
    fn discharge_margin_reduces_power() {
        let mut ctrl = controller_in_mode(ControlMode::Discharge, Duration::from_secs(60));
        ctrl.discharge_margin = PowerMargin::new(20);
        // Grid at +300W, margin=20W → (300-20) = 280W
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(300.0), SolarPower::new(0.0), &battery(50)),
            &clock(20),
        );
        assert_eq!(decision.power_watts, Setpoint::new(280));
    }

    // --- Battery feedback tests ---

    #[test]
    fn discharge_accounts_for_current_output() {
        let mut ctrl = controller_in_mode(ControlMode::Discharge, Duration::from_secs(60));
        // Battery already discharging 200W, grid still importing 100W
        // Need: 200 + (100 - 5) = 295W
        let bat = battery_discharging(50, 200);
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(100.0), SolarPower::new(0.0), &bat),
            &clock(20),
        );
        assert_eq!(decision.power_watts, Setpoint::new(295));
    }

    #[test]
    fn charge_accounts_for_current_input() {
        let mut ctrl = controller_in_mode(ControlMode::Charge, Duration::from_secs(60));
        // Battery already charging 200W, grid still exporting 150W
        // Need: 200 + (150 - 50) = 300W
        let bat = battery_charging(50, 200);
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(-150.0), SolarPower::new(0.0), &bat),
            &clock(12),
        );
        assert_eq!(decision.power_watts, Setpoint::new(300));
    }

    #[test]
    fn discharge_reduces_power_when_overproducing() {
        let mut ctrl = controller_in_mode(ControlMode::Discharge, Duration::from_secs(60));
        // Battery discharging 400W but grid exporting 50W (overshot).
        // underlying_grid = -50 + 400 = 350W → real demand still high, stay discharging.
        // Power: 400 + (-50 - 5) = 345W (reduces toward balance).
        let bat = battery_discharging(50, 400);
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(-50.0), SolarPower::new(0.0), &bat),
            &clock(20),
        );
        assert_eq!(decision.mode, ControlMode::Discharge);
        assert_eq!(decision.power_watts, Setpoint::new(345));
    }

    // --- Ramp tests ---

    #[test]
    fn first_decision_after_mode_change_uses_75_percent() {
        let mut ctrl = controller_no_cooldown();
        // Idle → Charge: 0.75 ramp on mode change
        // target_power: (400-50) = 350W, ramped: 350*0.75 = 262W
        let d1 = decide_at(
            &mut ctrl,
            &world(GridPower(-400.0), SolarPower::new(0.0), &battery(50)),
            &clock(12),
        );
        assert_eq!(d1.mode, ControlMode::Charge);
        assert_eq!(d1.power_watts, Setpoint::new(262));
        assert!(d1.reason.contains("ramped"));
    }

    #[test]
    fn second_decision_in_same_mode_uses_full_power() {
        let mut ctrl = controller_no_cooldown();
        let _d1 = decide_at(
            &mut ctrl,
            &world(GridPower(-400.0), SolarPower::new(0.0), &battery(50)),
            &clock(12),
        );
        // Same mode → full power
        let d2 = decide_at(
            &mut ctrl,
            &world(GridPower(-400.0), SolarPower::new(0.0), &battery(50)),
            &clock(12),
        );
        assert_eq!(d2.power_watts, Setpoint::new(350)); // (400-50)*1.0
        assert!(!d2.reason.contains("ramped"));
    }

    #[test]
    fn ramp_on_discharge_mode_change() {
        let mut ctrl = controller_no_cooldown();
        // Idle → Discharge: ramped
        // target_power: (400-5) = 395W, ramped: 395*0.75 = 296W
        let d1 = decide_at(
            &mut ctrl,
            &world(GridPower(400.0), SolarPower::new(0.0), &battery(50)),
            &clock(20),
        );
        assert_eq!(d1.mode, ControlMode::Discharge);
        assert_eq!(d1.power_watts, Setpoint::new(296));

        // Same mode → full power
        let d2 = decide_at(
            &mut ctrl,
            &world(GridPower(400.0), SolarPower::new(0.0), &battery(50)),
            &clock(20),
        );
        assert_eq!(d2.power_watts, Setpoint::new(395));
    }

    // --- Decision interval tests ---

    #[test]
    fn min_decision_interval_throttles() {
        let mut ctrl = default_controller();
        ctrl.min_decision_interval = Duration::from_secs(5);
        ctrl.state.last_decision = Timestamp::from_millis(NOW_MS);

        assert!(
            ctrl.decide(
                &world(GridPower(-300.0), SolarPower::new(0.0), &battery(50)),
                &clock(12)
            )
            .is_none()
        );
    }

    #[test]
    fn decision_allowed_after_interval() {
        let mut ctrl = default_controller();
        ctrl.min_decision_interval = Duration::from_secs(5);
        ctrl.state.last_decision = Timestamp::from_millis(NOW_MS - 6_000);

        assert!(
            ctrl.decide(
                &world(GridPower(-300.0), SolarPower::new(0.0), &battery(50)),
                &clock(12)
            )
            .is_some()
        );
    }

    // --- Cooldown tests ---

    #[test]
    fn charge_to_discharge_blocked_by_idle_duration() {
        // In Charge mode → target_mode returns Idle (no idle time for discharge)
        let mut ctrl = controller_in_mode(ControlMode::Charge, Duration::from_secs(5));
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(300.0), SolarPower::new(0.0), &battery(50)),
            &clock(20),
        );
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    #[test]
    fn toggle_discharge_to_charge_suppressed() {
        let mut ctrl = controller_in_mode(ControlMode::Discharge, Duration::from_secs(5));
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(-200.0), SolarPower::new(0.0), &battery(50)),
            &clock(20),
        );
        assert_eq!(decision.mode, ControlMode::Idle);
        assert!(decision.reason.contains("Cooldown"));
    }

    #[test]
    fn discharge_allowed_after_sufficient_idle() {
        // Was in Charge, then idle for 10 minutes (> 5 min default)
        let mut ctrl = controller_in_mode(ControlMode::Idle, Duration::from_secs(600));
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(300.0), SolarPower::new(0.0), &battery(50)),
            &clock(20),
        );
        assert_eq!(decision.mode, ControlMode::Discharge);
    }

    #[test]
    fn idle_to_charge_always_allowed() {
        let mut ctrl = controller_in_mode(ControlMode::Idle, Duration::from_secs(1));
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(-300.0), SolarPower::new(0.0), &battery(50)),
            &clock(12),
        );
        assert_eq!(decision.mode, ControlMode::Charge);
    }

    #[test]
    fn idle_to_discharge_allowed_after_idle_duration() {
        let mut ctrl = controller_in_mode(ControlMode::Idle, Duration::from_secs(600));
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(300.0), SolarPower::new(0.0), &battery(50)),
            &clock(20),
        );
        assert_eq!(decision.mode, ControlMode::Discharge);
    }

    #[test]
    fn charge_to_idle_always_allowed() {
        let mut ctrl = controller_in_mode(ControlMode::Charge, Duration::from_secs(1));
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(20.0), SolarPower::new(0.0), &battery(50)),
            &clock(12),
        );
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    #[test]
    fn rapid_oscillation_stays_idle() {
        let mut ctrl = controller_in_mode(ControlMode::Idle, Duration::from_secs(0));
        ctrl.min_idle_before_discharge = Duration::from_secs(300);
        let bat = battery(50);

        let d1 = decide_at(
            &mut ctrl,
            &world(GridPower(-200.0), SolarPower::new(0.0), &bat),
            &clock(20),
        );
        assert_eq!(d1.mode, ControlMode::Charge);

        // After charging, idle duration not met → stays idle (not discharge)
        let d2 = decide_at(
            &mut ctrl,
            &world(GridPower(200.0), SolarPower::new(0.0), &bat),
            &clock(20),
        );
        assert_eq!(d2.mode, ControlMode::Idle, "should go idle, not discharge");

        let d3 = decide_at(
            &mut ctrl,
            &world(GridPower(-200.0), SolarPower::new(0.0), &bat),
            &clock(20),
        );
        assert_eq!(d3.mode, ControlMode::Charge);

        let d4 = decide_at(
            &mut ctrl,
            &world(GridPower(200.0), SolarPower::new(0.0), &bat),
            &clock(20),
        );
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
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(0.0), SolarPower::new(0.0), &battery(50)),
            &clock(12),
        );
        assert_eq!(decision.mode, ControlMode::Standby);
        assert!(decision.reason.contains("standby"));
    }

    #[test]
    fn no_standby_before_timeout() {
        let mut ctrl = controller_in_mode(ControlMode::Idle, Duration::from_secs(4 * 60));
        // Grid at 0W — no discharge demand
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(0.0), SolarPower::new(0.0), &battery(50)),
            &clock(12),
        );
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    #[test]
    fn standby_exits_on_demand() {
        let mut ctrl = controller_in_mode(ControlMode::Idle, Duration::from_secs(20 * 60));
        ctrl.idle_timeout = Duration::from_secs(15 * 60);
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(300.0), SolarPower::new(0.0), &battery(50)),
            &clock(20),
        );
        assert_eq!(decision.mode, ControlMode::Discharge);
    }

    // --- Cycle counting tests ---

    #[test]
    fn transition_increments_daily_cycles() {
        let mut ctrl = controller_no_cooldown();
        assert_eq!(ctrl.state.daily_transitions, 0);

        // Idle → Charge
        decide_at(
            &mut ctrl,
            &world(GridPower(-200.0), SolarPower::new(0.0), &battery(50)),
            &clock(12),
        );
        assert_eq!(ctrl.state.daily_transitions, 1);

        // Charge → Idle (within deadband)
        decide_at(
            &mut ctrl,
            &world(GridPower(20.0), SolarPower::new(0.0), &battery(50)),
            &clock(12),
        );
        assert_eq!(ctrl.state.daily_transitions, 2);
    }

    #[test]
    fn same_mode_does_not_increment() {
        let mut ctrl = controller_in_mode(ControlMode::Charge, Duration::from_secs(60));
        decide_at(
            &mut ctrl,
            &world(GridPower(-300.0), SolarPower::new(0.0), &battery(50)),
            &clock(12),
        );
        assert_eq!(ctrl.state.daily_transitions, 0);
    }

    #[test]
    fn cooldown_suppression_increments_counter() {
        // Discharge→Charge toggle within cooldown
        let mut ctrl = controller_in_mode(ControlMode::Discharge, Duration::from_secs(5));
        assert_eq!(ctrl.state.daily_cooldown_suppressions, 0);

        decide_at(
            &mut ctrl,
            &world(GridPower(-200.0), SolarPower::new(0.0), &battery(50)),
            &clock(20),
        );
        assert_eq!(ctrl.state.daily_cooldown_suppressions, 1);
        // Suppression doesn't count as a transition
        assert_eq!(ctrl.state.daily_transitions, 0);
    }

    #[test]
    fn cycle_counts_returns_current_state() {
        let mut ctrl = controller_no_cooldown();
        decide_at(
            &mut ctrl,
            &world(GridPower(-200.0), SolarPower::new(0.0), &battery(50)),
            &clock(12),
        );

        let counts = ctrl.cycle_counts();
        assert_eq!(counts.daily_transitions, 1);
        assert_eq!(counts.daily_cooldown_suppressions, 0);
    }

    #[test]
    fn cycle_limit_forces_standby() {
        let mut ctrl = controller_no_cooldown();
        ctrl.cycle_warn_threshold = 3;

        // 3 transitions: Idle→Charge, Charge→Idle, Idle→Charge
        decide_at(
            &mut ctrl,
            &world(GridPower(-200.0), SolarPower::new(0.0), &battery(50)),
            &clock(12),
        );
        decide_at(
            &mut ctrl,
            &world(GridPower(20.0), SolarPower::new(0.0), &battery(50)),
            &clock(12),
        );
        decide_at(
            &mut ctrl,
            &world(GridPower(-200.0), SolarPower::new(0.0), &battery(50)),
            &clock(12),
        );
        assert_eq!(ctrl.state.daily_transitions, 3);

        // Next decision should be forced to Standby
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(-200.0), SolarPower::new(0.0), &battery(50)),
            &clock(12),
        );
        assert_eq!(decision.mode, ControlMode::Standby);
        assert!(decision.reason.contains("Cycle limit"));
    }

    #[test]
    fn cycle_limit_standby_persists() {
        let mut ctrl = controller_no_cooldown();
        ctrl.cycle_warn_threshold = 1;

        // 1 transition hits the limit
        decide_at(
            &mut ctrl,
            &world(GridPower(-200.0), SolarPower::new(0.0), &battery(50)),
            &clock(12),
        );
        assert_eq!(ctrl.state.daily_transitions, 1);

        // All subsequent decisions stay in standby
        let d1 = decide_at(
            &mut ctrl,
            &world(GridPower(300.0), SolarPower::new(0.0), &battery(50)),
            &clock(20),
        );
        assert_eq!(d1.mode, ControlMode::Standby);

        let d2 = decide_at(
            &mut ctrl,
            &world(GridPower(-500.0), SolarPower::new(0.0), &battery(50)),
            &clock(12),
        );
        assert_eq!(d2.mode, ControlMode::Standby);
    }

    // --- SOC calibration tests ---

    #[test]
    fn calibrating_forces_idle() {
        let mut ctrl = controller_no_cooldown();
        let mut bat = battery(50);
        bat.soc_calibrating = true;
        // Would normally charge, but calibration overrides
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(-500.0), SolarPower::new(0.0), &bat),
            &clock(12),
        );
        assert_eq!(decision.mode, ControlMode::Idle);
        assert_eq!(decision.power_watts, Setpoint::ZERO);
        assert!(decision.reason.contains("calibration"));
    }

    #[test]
    fn calibrating_prevents_discharge() {
        let mut ctrl = controller_no_cooldown();
        let mut bat = battery(80);
        bat.soc_calibrating = true;
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(400.0), SolarPower::new(0.0), &bat),
            &clock(20),
        );
        assert_eq!(decision.mode, ControlMode::Idle);
        assert!(decision.reason.contains("calibration"));
    }

    #[test]
    fn fault_forces_idle_and_does_not_charge() {
        let mut ctrl = controller_no_cooldown();
        let mut bat = battery(34);
        bat.fault = true;
        // Would normally charge on this much solar export, but a fault overrides.
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(-1653.0), SolarPower::new(1722.0), &bat),
            &clock(12),
        );
        assert_eq!(decision.mode, ControlMode::Idle);
        assert_eq!(decision.power_watts, Setpoint::ZERO);
        assert!(decision.reason.contains("fault"));
    }

    #[test]
    fn fault_prevents_discharge() {
        let mut ctrl = controller_no_cooldown();
        let mut bat = battery(80);
        bat.fault = true;
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(400.0), SolarPower::new(0.0), &bat),
            &clock(20),
        );
        assert_eq!(decision.mode, ControlMode::Idle);
        assert!(decision.reason.contains("fault"));
    }

    #[test]
    fn normal_soc_status_allows_decisions() {
        let mut ctrl = controller_no_cooldown();
        let bat = battery(50); // soc_calibrating: false
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(-500.0), SolarPower::new(0.0), &bat),
            &clock(12),
        );
        assert_eq!(decision.mode, ControlMode::Charge);
    }

    #[test]
    fn cycle_limit_zero_disables() {
        let mut ctrl = controller_no_cooldown();
        ctrl.cycle_warn_threshold = 0;

        // Many transitions should still work
        decide_at(
            &mut ctrl,
            &world(GridPower(-200.0), SolarPower::new(0.0), &battery(50)),
            &clock(12),
        );
        decide_at(
            &mut ctrl,
            &world(GridPower(20.0), SolarPower::new(0.0), &battery(50)),
            &clock(12),
        );
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(-200.0), SolarPower::new(0.0), &battery(50)),
            &clock(12),
        );
        assert_eq!(decision.mode, ControlMode::Charge);
    }

    // --- Charge hysteresis tests ---

    #[test]
    fn charge_hysteresis_keeps_charging_within_deadband() {
        // underlying_grid = -50W, which is between charge_start_threshold (-100W) and 0W.
        // From idle: -50 > -100 → would NOT start charging.
        // But already charging: threshold drops to 0W, -50 < 0 → keeps charging.
        let mut ctrl = controller_in_mode(ControlMode::Charge, Duration::from_secs(60));
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(-50.0), SolarPower::new(0.0), &battery(50)),
            &clock(12),
        );
        assert_eq!(decision.mode, ControlMode::Charge);
    }

    #[test]
    fn charge_hysteresis_does_not_start_within_deadband() {
        // Same grid power (-50W) but starting from idle.
        // underlying_grid = -50 > charge_start_threshold (-100) → stays idle.
        let mut ctrl = controller_no_cooldown();
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(-50.0), SolarPower::new(0.0), &battery(50)),
            &clock(12),
        );
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    #[test]
    fn charge_hysteresis_stops_when_importing() {
        // Already charging, but underlying_grid >= 0 → even hysteresis can't save it.
        // Battery charging at 200W, grid reads +10W → underlying = 10 + (-200) = -190W.
        // Wait, let's use a simpler case: battery idle, grid +10W → underlying = +10 >= 0.
        let mut ctrl = controller_in_mode(ControlMode::Charge, Duration::from_secs(60));
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(10.0), SolarPower::new(0.0), &battery(50)),
            &clock(12),
        );
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    #[test]
    fn charge_hysteresis_boundary_at_zero() {
        // Already charging, underlying_grid = 0.0 exactly → 0.0 < 0.0 is false → stops.
        let mut ctrl = controller_in_mode(ControlMode::Charge, Duration::from_secs(60));
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(0.0), SolarPower::new(0.0), &battery(50)),
            &clock(12),
        );
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    // --- Discharge hysteresis tests ---

    #[test]
    fn discharge_hysteresis_keeps_discharging_near_zero() {
        // Already discharging. Set a higher start threshold to make the deadband visible.
        let mut ctrl = controller_in_mode(ControlMode::Discharge, Duration::from_secs(60));
        ctrl.discharge_start_threshold = GridPower(100.0);
        // underlying_grid = 50W: below start threshold (100W) but above hysteresis (0W).
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(50.0), SolarPower::new(0.0), &battery(50)),
            &clock(20),
        );
        assert_eq!(decision.mode, ControlMode::Discharge);
    }

    #[test]
    fn discharge_hysteresis_does_not_start_below_threshold() {
        // Same grid power but from idle — should NOT start discharging.
        let mut ctrl = controller_no_cooldown();
        ctrl.discharge_start_threshold = GridPower(100.0);
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(50.0), SolarPower::new(0.0), &battery(50)),
            &clock(20),
        );
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    #[test]
    fn discharge_hysteresis_stops_when_exporting() {
        // Already discharging, but underlying_grid = -10 <= 0 → not > 0 → stops discharging.
        // -10 is also > charge_start_threshold (-100) → not enough export to charge → idle.
        let mut ctrl = controller_in_mode(ControlMode::Discharge, Duration::from_secs(60));
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(-10.0), SolarPower::new(0.0), &battery(50)),
            &clock(20),
        );
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    #[test]
    fn discharge_hysteresis_boundary_at_zero() {
        // Already discharging, underlying_grid = 0.0 exactly → 0.0 > 0.0 is false → stops.
        let mut ctrl = controller_in_mode(ControlMode::Discharge, Duration::from_secs(60));
        ctrl.discharge_start_threshold = GridPower(100.0);
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(0.0), SolarPower::new(0.0), &battery(50)),
            &clock(20),
        );
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    // --- SOC limit tests ---

    #[test]
    fn soc_limit_reached_prevents_charging() {
        // Battery reports socLimit: 1 at 99% — should not charge
        let mut ctrl = controller_no_cooldown();
        let mut bat = battery(99);
        bat.soc_limit_reached = true;
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(-500.0), SolarPower::new(0.0), &bat),
            &clock(12),
        );
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    #[test]
    fn soc_limit_reached_stops_active_charging() {
        // Already charging, but battery now reports socLimit: 1
        let mut ctrl = controller_in_mode(ControlMode::Charge, Duration::from_secs(60));
        let mut bat = battery(99);
        bat.soc_limit_reached = true;
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(-500.0), SolarPower::new(0.0), &bat),
            &clock(12),
        );
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    #[test]
    fn soc_limit_not_reached_allows_charging() {
        // Battery reports socLimit: 0 at 99% — charging allowed
        let mut ctrl = controller_no_cooldown();
        let bat = battery(99); // soc_limit_reached: false
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(-500.0), SolarPower::new(0.0), &bat),
            &clock(12),
        );
        assert_eq!(decision.mode, ControlMode::Charge);
    }

    #[test]
    fn soc_limit_does_not_block_discharge() {
        // socLimit should only affect charging, not discharging
        let mut ctrl = controller_no_cooldown();
        let mut bat = battery(99);
        bat.soc_limit_reached = true;
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(400.0), SolarPower::new(0.0), &bat),
            &clock(20),
        );
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
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(-20.0), SolarPower::new(0.0), &bat),
            &clock(12),
        );
        assert_eq!(decision.mode, ControlMode::Charge);
    }

    #[test]
    fn discharge_hysteresis_with_battery_output_prevents_oscillation() {
        // Battery discharging 400W. Grid reads -30W (slight export = overshot).
        // underlying_grid = -30 + 400 = 370W → still > 0 → keep discharging.
        // From idle: underlying = -30 → not > 0 threshold → idle. Hysteresis prevents flip.
        let mut ctrl = controller_in_mode(ControlMode::Discharge, Duration::from_secs(60));
        let bat = battery_discharging(50, 400);
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(-30.0), SolarPower::new(0.0), &bat),
            &clock(20),
        );
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
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(-75.0), SolarPower::new(0.0), &bat),
            &clock(12),
        );
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
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(50.0), SolarPower::new(0.0), &bat),
            &clock(20),
        );
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
        let d1 = decide_at(
            &mut ctrl,
            &world(GridPower(150.0), SolarPower::new(0.0), &battery(80)),
            &clock(hour),
        );
        assert_eq!(d1.mode, ControlMode::Discharge, "step 1: should discharge");
        // Idle → Discharge mode change → 75% ramp: (150 - 5) × 0.75 = 108W
        assert_eq!(
            d1.power_watts,
            Setpoint::new(108),
            "step 1: ramped first decision"
        );
        let battery_discharge = d1.power_watts;

        // Step 2: Battery discharging 108W, grid still importing 42W.
        let net = house_total - f64::from(battery_discharge.get()); // 42W
        let bat = battery_discharging(80, battery_discharge.get());
        let d2 = decide_at(
            &mut ctrl,
            &world(GridPower(net), SolarPower::new(0.0), &bat),
            &clock(hour),
        );
        assert_eq!(d2.mode, ControlMode::Discharge, "step 2: still discharging");
        // Same mode, no ramp: 108 + (42 - 5) = 145W
        assert_eq!(d2.power_watts, Setpoint::new(145), "step 2: converging");
        let battery_discharge = d2.power_watts;

        // Step 3: Battery at 145W. Net = 150-145 = 5W (nearly balanced).
        let net = house_total - f64::from(battery_discharge.get()); // 5W
        let bat = battery_discharging(80, battery_discharge.get());
        let d3 = decide_at(
            &mut ctrl,
            &world(GridPower(net), SolarPower::new(0.0), &bat),
            &clock(hour),
        );
        assert_eq!(d3.mode, ControlMode::Discharge, "step 3: still discharging");
        // Same mode: 145 + (5 - 5) = 145W — stable!
        assert_eq!(d3.power_watts, Setpoint::new(145), "step 3: steady state");

        // Final check: battery is discharging within the discharge margin of house demand.
        let final_net = house_total - f64::from(d3.power_watts.get());
        assert!(
            final_net.abs() < 10.0,
            "final net should be near zero, got {final_net:.0}W"
        );
    }

    // --- Solar discharge block tests ---

    #[test]
    fn solar_above_block_skips_discharge() {
        // Solar inverter exporting 2000W (car charging on another phase) →
        // don't drain the battery, let grid+solar cover the load.
        let mut ctrl = controller_no_cooldown();
        ctrl.solar_discharge_block_threshold = SolarPower::new(1000.0);
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(400.0), SolarPower::new(2000.0), &battery(80)),
            &clock(12),
        );
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    #[test]
    fn solar_below_block_still_discharges() {
        // Solar below the threshold → discharge as usual to cover the load.
        let mut ctrl = controller_no_cooldown();
        ctrl.solar_discharge_block_threshold = SolarPower::new(1000.0);
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(400.0), SolarPower::new(500.0), &battery(80)),
            &clock(20),
        );
        assert_eq!(decision.mode, ControlMode::Discharge);
    }

    #[test]
    fn solar_block_at_threshold_boundary() {
        // Exactly at the threshold → blocked (>= is the trigger).
        let mut ctrl = controller_no_cooldown();
        ctrl.solar_discharge_block_threshold = SolarPower::new(1000.0);
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(400.0), SolarPower::new(1000.0), &battery(80)),
            &clock(12),
        );
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    #[test]
    fn solar_block_disabled_discharges_at_high_solar() {
        // Threshold of 0 disables the guard → discharge regardless of solar.
        let mut ctrl = controller_no_cooldown();
        ctrl.solar_discharge_block_threshold = SolarPower::new(0.0);
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(400.0), SolarPower::new(3000.0), &battery(80)),
            &clock(20),
        );
        assert_eq!(decision.mode, ControlMode::Discharge);
    }

    #[test]
    fn solar_above_block_stops_active_discharge() {
        // Already discharging when solar climbs above the threshold → stop
        // draining and go idle.
        let mut ctrl = controller_in_mode(ControlMode::Discharge, Duration::from_secs(60));
        ctrl.solar_discharge_block_threshold = SolarPower::new(1000.0);
        let bat = battery_discharging(80, 400);
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(100.0), SolarPower::new(1500.0), &bat),
            &clock(20),
        );
        assert_eq!(decision.mode, ControlMode::Idle);
    }

    #[test]
    fn solar_block_does_not_affect_charging() {
        // The guard only gates discharge; charging on solar excess is unaffected.
        let mut ctrl = controller_no_cooldown();
        ctrl.solar_discharge_block_threshold = SolarPower::new(1000.0);
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(-500.0), SolarPower::new(2000.0), &battery(50)),
            &clock(12),
        );
        assert_eq!(decision.mode, ControlMode::Charge);
    }

    // --- Balance weekday tests ---

    #[test]
    fn effective_max_soc_raised_on_balance_weekday() {
        let mut ctrl = default_controller();
        ctrl.max_soc = Soc::new(95);
        ctrl.balance_weekday = Some(Weekday::Mon);
        assert_eq!(ctrl.effective_max_soc(Weekday::Mon), Soc::new(100));
        assert_eq!(ctrl.effective_max_soc(Weekday::Tue), Soc::new(95));
    }

    #[test]
    fn effective_max_soc_unaffected_when_disabled() {
        let mut ctrl = default_controller();
        ctrl.max_soc = Soc::new(95);
        ctrl.balance_weekday = None;
        assert_eq!(ctrl.effective_max_soc(Weekday::Mon), Soc::new(95));
    }

    #[test]
    fn balance_weekday_allows_charging_past_normal_max_soc() {
        let mut ctrl = controller_no_cooldown();
        ctrl.max_soc = Soc::new(95);
        ctrl.balance_weekday = Some(WEEKDAY);
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(-500.0), SolarPower::new(0.0), &battery(97)),
            &clock(12),
        );
        assert_eq!(decision.mode, ControlMode::Charge);
    }

    // --- Midnight rollover ---

    #[test]
    fn midnight_resets_daily_counters() {
        let mut ctrl = controller_in_mode(ControlMode::Charge, Duration::from_secs(60));
        ctrl.state.daily_transitions = 5;
        ctrl.state.daily_cooldown_suppressions = 3;

        // Same mode, so no new transition is counted — the counters show only
        // the effect of the reset.
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(-300.0), SolarPower::new(0.0), &battery(50)),
            &clock_on_day(12, DAY + 1),
        );
        assert_eq!(decision.mode, ControlMode::Charge);
        assert_eq!(ctrl.state.daily_transitions, 0);
        assert_eq!(ctrl.state.daily_cooldown_suppressions, 0);
        assert_eq!(ctrl.state.last_cycle_reset_day, DAY + 1);
    }

    #[test]
    fn same_day_does_not_reset_counters() {
        let mut ctrl = controller_in_mode(ControlMode::Charge, Duration::from_secs(60));
        ctrl.state.daily_transitions = 5;
        decide_at(
            &mut ctrl,
            &world(GridPower(-300.0), SolarPower::new(0.0), &battery(50)),
            &clock(23),
        );
        assert_eq!(ctrl.state.daily_transitions, 5);
    }

    #[test]
    fn cycle_limit_standby_is_lifted_at_midnight() {
        let mut ctrl = controller_no_cooldown();
        ctrl.cycle_warn_threshold = 1;

        // One transition hits the limit, the next decision is forced to standby.
        assert_eq!(
            decide_at(
                &mut ctrl,
                &world(GridPower(-200.0), SolarPower::new(0.0), &battery(50)),
                &clock(12)
            )
            .mode,
            ControlMode::Charge
        );
        assert_eq!(
            decide_at(
                &mut ctrl,
                &world(GridPower(-500.0), SolarPower::new(0.0), &battery(50)),
                &clock(12)
            )
            .mode,
            ControlMode::Standby
        );

        // "Standby until midnight" — so midnight must end it.
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(-500.0), SolarPower::new(0.0), &battery(50)),
            &clock_on_day(12, DAY + 1),
        );
        assert_eq!(decision.mode, ControlMode::Charge);
        assert_eq!(
            ctrl.state.daily_transitions, 1,
            "counters reset, then this change"
        );
    }

    // --- Balance weekday, on a fixed weekday ---

    #[test]
    fn balance_weekday_only_raises_max_soc_on_that_day() {
        let mut ctrl = controller_no_cooldown();
        ctrl.max_soc = Soc::new(95);
        ctrl.balance_weekday = Some(Weekday::Mon);

        let sunday = Clock {
            weekday: Weekday::Sun,
            ..clock(12)
        };
        assert_eq!(
            decide_at(
                &mut ctrl,
                &world(GridPower(-500.0), SolarPower::new(0.0), &battery(97)),
                &sunday
            )
            .mode,
            ControlMode::Idle,
            "97% is above the normal 95% cap"
        );

        let monday = Clock {
            weekday: Weekday::Mon,
            ..clock(12)
        };
        assert_eq!(
            decide_at(
                &mut ctrl,
                &world(GridPower(-500.0), SolarPower::new(0.0), &battery(97)),
                &monday
            )
            .mode,
            ControlMode::Charge,
            "balance day raises the cap to 100%"
        );
    }

    // --- Exact timing boundaries ---

    #[test]
    fn cooldown_boundary_is_exclusive() {
        // 1ms short of min_mode_duration (10s) → toggle still suppressed.
        let mut ctrl = controller_in_mode(ControlMode::Discharge, Duration::from_millis(9_999));
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(-200.0), SolarPower::new(0.0), &battery(50)),
            &clock(20),
        );
        assert_eq!(decision.mode, ControlMode::Idle);
        assert!(decision.reason.contains("Cooldown"));

        // Exactly at min_mode_duration → allowed through.
        let mut ctrl = controller_in_mode(ControlMode::Discharge, Duration::from_secs(10));
        let decision = decide_at(
            &mut ctrl,
            &world(GridPower(-200.0), SolarPower::new(0.0), &battery(50)),
            &clock(20),
        );
        assert_eq!(decision.mode, ControlMode::Charge);
    }

    #[test]
    fn idle_timeout_boundary_is_inclusive() {
        // 1ms short of the 5min default → still idle.
        let mut ctrl = controller_in_mode(ControlMode::Idle, Duration::from_millis(299_999));
        assert_eq!(
            decide_at(
                &mut ctrl,
                &world(GridPower(0.0), SolarPower::new(0.0), &battery(50)),
                &clock(12)
            )
            .mode,
            ControlMode::Idle
        );

        // Exactly at the timeout → standby.
        let mut ctrl = controller_in_mode(ControlMode::Idle, Duration::from_secs(300));
        assert_eq!(
            decide_at(
                &mut ctrl,
                &world(GridPower(0.0), SolarPower::new(0.0), &battery(50)),
                &clock(12)
            )
            .mode,
            ControlMode::Standby
        );
    }

    #[test]
    fn min_idle_before_discharge_boundary_is_inclusive() {
        // Guard only applies when the last active mode was Charge.
        let mut ctrl = controller_no_cooldown();
        ctrl.state.last_active_mode = Some(ControlMode::Charge);
        ctrl.min_idle_before_discharge = Duration::from_secs(300);

        ctrl.state.last_idle_start = Some(Timestamp::from_millis(NOW_MS - 299_999));
        assert_eq!(
            decide_at(
                &mut ctrl,
                &world(GridPower(400.0), SolarPower::new(0.0), &battery(80)),
                &clock(20)
            )
            .mode,
            ControlMode::Idle
        );

        ctrl.state.last_idle_start = Some(Timestamp::from_millis(NOW_MS - 300_000));
        assert_eq!(
            decide_at(
                &mut ctrl,
                &world(GridPower(400.0), SolarPower::new(0.0), &battery(80)),
                &clock(20)
            )
            .mode,
            ControlMode::Discharge
        );
    }

    #[test]
    fn decision_interval_boundary_is_inclusive() {
        let mut ctrl = default_controller();
        ctrl.min_decision_interval = Duration::from_secs(5);

        ctrl.state.last_decision = Timestamp::from_millis(NOW_MS - 4_999);
        assert!(
            ctrl.decide(
                &world(GridPower(-300.0), SolarPower::new(0.0), &battery(50)),
                &clock(12)
            )
            .is_none()
        );

        ctrl.state.last_decision = Timestamp::from_millis(NOW_MS - 5_000);
        assert!(
            ctrl.decide(
                &world(GridPower(-300.0), SolarPower::new(0.0), &battery(50)),
                &clock(12)
            )
            .is_some()
        );
    }
}
