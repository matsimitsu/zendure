use serde::{Deserialize, Serialize};

use crate::device::BatterySpec;
use crate::models::ZendureProperties;
use crate::units::{BatteryPower, PowerCap, Soc, Watts};

/// Current battery state, used by the controller to make decisions.
/// `Serialize`/`Deserialize` because this is a journalled `Measurement` in
/// the `World`; `PartialEq` compares recorded worlds. Every field is a
/// `#[serde(transparent)]` newtype or `bool`, so the JSON is bare numbers/flags.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BatteryState {
    pub soc: Soc,
    /// Maximum discharge/inverter output power.
    pub max_discharge_power: PowerCap,
    /// Maximum charge power.
    pub max_charge_power: PowerCap,
    /// Current battery output power. Positive = discharging, negative = charging.
    pub current_power: BatteryPower,
    pub soc_calibrating: bool,
    /// True when the battery reports it has reached its SOC limit and refuses charging.
    pub soc_limit_reached: bool,
    /// True when the device reports `isError`. `faultLevel` is deliberately
    /// ignored: it also goes non-zero for benign WiFi hiccups or firmware update
    /// checks, which caused spurious idling. While faulted, the controller
    /// stays idle and never overwrites the power-cap setpoints.
    pub fault: bool,
}

impl BatteryState {
    /// A healthy mid-charge battery, for tests. Variants are struct updates:
    /// `BatteryState { soc: Soc::new(80), ..BatteryState::test_sample() }`.
    /// Shared because five test modules were duplicating the same seven
    /// fields, with arbitrary non-zero cap headroom in each.
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
            // A reported 0 means the device zeroed its own setpoint; honored
            // deliberately (caps are only written once, at startup) rather than
            // overwritten mid-run. Absent falls back to the rated cap. Not
            // clamped to the rating: `inverseMaxPower` is authoritative even when it
            // disagrees with the spec.
            max_discharge_power: props
                .inverse_max_power
                .map(PowerCap::new)
                .unwrap_or(spec.max_discharge_power),
            // Input *is* clamped to the rating. It belongs here at the
            // device boundary, not pushed up to callers.
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
