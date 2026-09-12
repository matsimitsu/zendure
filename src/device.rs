//! What a battery model *is*, as opposed to what one is currently doing.
//!
//! The rated limits used to live as consts in `models.rs` next to the wire
//! types, which put a fact about the hardware in the same file as the JSON it
//! happens to be written with. They belong here: one place to name a model, so
//! a second one (a different Zendure, or another vendor entirely) is a new
//! `BatterySpec` rather than another pair of consts to keep in sync.

use crate::units::PowerCap;

/// A battery model's rated limits. What the hardware can do, as distinct from
/// what it currently reports it will accept — the second is a `BatteryState`
/// measurement, the first is a fact about the box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatterySpec {
    pub max_charge_power: PowerCap,
    pub max_discharge_power: PowerCap,
}

/// Zendure solarFlow2400AC+. 800 W out is Germany's feed-in limit, stored on
/// the device as the read/write `inverseMaxPower` setpoint; 2400 W in is the
/// `chargeMaxLimit` setpoint. Both can be reset to 0 by the device, which is
/// why the controller writes them back at startup and falls back to them when
/// the device omits the field.
pub const AC2400_PLUS: BatterySpec = BatterySpec {
    max_charge_power: PowerCap::new(2400),
    max_discharge_power: PowerCap::new(800),
};
