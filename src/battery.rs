use serde::{Deserialize, Serialize};

use crate::device::BatterySpec;
use crate::models::ZendureProperties;
use crate::units::{BatteryPower, PowerCap, Soc, Watts};

/// Current battery state, used by the controller to make decisions.
///
/// `Serialize`/`Deserialize` because this is a `Measurement` in the `World`,
/// and the world is what step 7 records per decision; `PartialEq` so two
/// recorded worlds can be compared. Every field is already a `#[serde(transparent)]`
/// newtype or a `bool`, so the JSON is the bare numbers and flags.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BatteryState {
    /// State of charge, 0–100%.
    pub soc: Soc,
    /// Maximum discharge/inverter output power.
    pub max_discharge_power: PowerCap,
    /// Maximum charge power.
    pub max_charge_power: PowerCap,
    /// Current battery output power. Positive = discharging, negative = charging.
    pub current_power: BatteryPower,
    /// True when the battery is recalibrating its SOC reading.
    pub soc_calibrating: bool,
    /// True when the battery reports it has reached its SOC limit and refuses charging.
    pub soc_limit_reached: bool,
    /// True when the device reports an error (isError). We deliberately ignore
    /// faultLevel: it also goes non-zero for benign conditions like WiFi
    /// hiccups or firmware update checks, which caused spurious idling.
    /// While faulted, the controller stays idle and does not command power or
    /// overwrite the device's power-cap setpoints.
    pub fault: bool,
}

impl BatteryState {
    /// A healthy mid-charge battery, for tests. Variants are struct updates:
    /// `BatteryState { soc: Soc::new(80), ..BatteryState::test_sample() }`.
    ///
    /// Shared rather than restated per module — five test modules were writing
    /// the same seven fields, and the caps in particular were arbitrary
    /// non-zero headroom in every one of them.
    #[cfg(test)]
    pub(crate) fn test_sample() -> Self {
        Self {
            soc: Soc::new(50),
            max_discharge_power: PowerCap::new(800),
            max_charge_power: PowerCap::new(2400),
            current_power: BatteryPower::ZERO,
            soc_calibrating: false,
            soc_limit_reached: false,
            fault: false,
        }
    }

    pub fn from_properties(props: &ZendureProperties, spec: &BatterySpec) -> Self {
        let discharge = Watts::from_device(props.pack_input_power.unwrap_or(0));
        let charge = Watts::from_device(props.output_pack_power.unwrap_or(0));
        Self {
            soc: Soc::new(props.electric_level.unwrap_or(0)),
            // Honor the device's reported caps verbatim. A reported 0 means the
            // device zeroed its own power-cap setpoint — we deliberately let that
            // stop charging/discharging rather than overwriting it mid-run (the
            // caps are only written once, at startup). An *absent* field falls
            // back to the model's rated cap.
            //
            // Output is not clamped to the rating: `inverseMaxPower` is the
            // feed-in limit the device itself enforces, so it is authoritative
            // even when it disagrees with the spec.
            max_discharge_power: props
                .inverse_max_power
                .map(PowerCap::new)
                .unwrap_or(spec.max_discharge_power),
            // Input *is* clamped to the rating. That clamp used to live in
            // `controller.rs` as `MAX_CHARGE_POWER.min(battery.max_charge_power)`,
            // which meant the objective had to know the hardware's ceiling in
            // order to read a reported number safely. It is device knowledge,
            // so it belongs here at the device boundary; the objective now just
            // reads a capability number and trusts it.
            max_charge_power: spec.max_charge_power.min(
                props
                    .charge_max_limit
                    .map(PowerCap::new)
                    .unwrap_or(spec.max_charge_power),
            ),
            current_power: BatteryPower::from_flows(discharge, charge),
            soc_calibrating: props.soc_status == Some(1),
            soc_limit_reached: props.soc_limit == Some(1),
            fault: props.is_error.unwrap_or(0) != 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::AC2400_PLUS;
    use crate::models::ZendureProperties;
    use crate::units::PowerCap;

    #[test]
    fn reported_zero_limit_is_honored() {
        // Device zeroed its own caps mid-run — honor it (stop), don't overwrite.
        // Caps are only (re)written at startup, so this stalls until a restart.
        let props = ZendureProperties {
            electric_level: Some(34),
            charge_max_limit: Some(0),
            inverse_max_power: Some(0),
            ..Default::default()
        };
        let state = BatteryState::from_properties(&props, &AC2400_PLUS);
        assert_eq!(state.max_charge_power, PowerCap::ZERO);
        assert_eq!(state.max_discharge_power, PowerCap::ZERO);
    }

    #[test]
    fn error_flag_sets_fault_but_fault_level_is_ignored() {
        assert!(!BatteryState::from_properties(&ZendureProperties::default(), &AC2400_PLUS).fault);

        // faultLevel also goes non-zero for benign conditions (WiFi hiccups,
        // firmware update checks), so it must NOT set fault.
        let benign = ZendureProperties {
            fault_level: Some(2),
            ..Default::default()
        };
        assert!(!BatteryState::from_properties(&benign, &AC2400_PLUS).fault);

        let errored = ZendureProperties {
            is_error: Some(1),
            ..Default::default()
        };
        assert!(BatteryState::from_properties(&errored, &AC2400_PLUS).fault);
    }

    #[test]
    fn absent_limits_fall_back_to_default() {
        let props = ZendureProperties::default();
        let state = BatteryState::from_properties(&props, &AC2400_PLUS);
        assert_eq!(state.max_charge_power, AC2400_PLUS.max_charge_power);
        assert_eq!(state.max_discharge_power, AC2400_PLUS.max_discharge_power);
    }

    #[test]
    fn reported_nonzero_limits_are_respected() {
        let props = ZendureProperties {
            charge_max_limit: Some(1200),
            inverse_max_power: Some(600),
            ..Default::default()
        };
        let state = BatteryState::from_properties(&props, &AC2400_PLUS);
        assert_eq!(state.max_charge_power, PowerCap::new(1200));
        assert_eq!(state.max_discharge_power, PowerCap::new(600));
    }

    #[test]
    fn reported_cap_above_the_rating_is_clamped() {
        // A device reporting more than the model is rated for — a bad write, a
        // firmware quirk — must not become a charge target the hardware can't
        // meet. The rating wins, so the controller only ever sees a number the
        // box can actually deliver.
        let props = ZendureProperties {
            charge_max_limit: Some(5000),
            ..Default::default()
        };
        let state = BatteryState::from_properties(&props, &AC2400_PLUS);
        assert_eq!(state.max_charge_power, PowerCap::new(2400));
    }
}
