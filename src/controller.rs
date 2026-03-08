use chrono::{Local, Timelike};

use crate::models::{ControlDecision, ControlMode, ZendureProperties};

const MAX_CHARGE_POWER: i32 = 2400;
const MIN_POWER_THRESHOLD: i32 = 50;
const DISCHARGE_START_HOUR: u32 = 17;
const DISCHARGE_END_HOUR: u32 = 7;

/// Battery limits read from the device at startup.
pub struct BatteryLimits {
    /// Maximum discharge/inverter output power (W).
    /// From `inverseMaxPower` on the device (e.g. 800).
    pub max_discharge_power: i32,
}

impl BatteryLimits {
    pub fn from_properties(props: &ZendureProperties) -> Self {
        Self {
            max_discharge_power: props.inverse_max_power.unwrap_or(800) as i32,
        }
    }
}

/// Pure function: given current grid and solar readings, decide what the battery should do.
///
/// `grid_power` is the net grid power (W).
///   Positive = importing from grid, negative = exporting to grid.
/// `solar_power` is the current solar production (W), always >= 0.
/// `limits` contains the battery's actual power limits from the device.
pub fn decide(grid_power: f64, solar_power: f64, limits: &BatteryLimits) -> ControlDecision {
    // If we're exporting to the grid, we have excess solar — charge the battery
    if grid_power < -(MIN_POWER_THRESHOLD as f64) {
        let excess = (-grid_power) as i32;
        let charge_power = excess.min(MAX_CHARGE_POWER);
        return ControlDecision {
            mode: ControlMode::Charge,
            power_watts: charge_power,
            reason: format!("Solar excess: exporting {excess}W to grid"),
            grid_power,
            solar_power,
        };
    }

    // During evening/night/morning: discharge to cover home demand
    let hour = Local::now().hour();
    let is_discharge_period = !(DISCHARGE_END_HOUR..DISCHARGE_START_HOUR).contains(&hour);

    if is_discharge_period && grid_power > MIN_POWER_THRESHOLD as f64 {
        let demand = grid_power as i32;
        let discharge_power = demand.min(limits.max_discharge_power);
        return ControlDecision {
            mode: ControlMode::Discharge,
            power_watts: discharge_power,
            reason: format!("Discharge period (hour {hour}): grid demand {demand}W"),
            grid_power,
            solar_power,
        };
    }

    ControlDecision {
        mode: ControlMode::Idle,
        power_watts: 0,
        reason: format!(
            "No action needed (grid: {grid_power:.0}W, solar: {solar_power:.0}W, hour: {hour})"
        ),
        grid_power,
        solar_power,
    }
}
