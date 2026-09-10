use crate::models::{DEVICE_MAX_CHARGE_POWER, DEVICE_MAX_DISCHARGE_POWER, ZendureProperties};

/// Current battery state, used by the controller to make decisions.
#[derive(Debug, Clone)]
pub struct BatteryState {
    /// State of charge (%), 0–100.
    pub soc: u32,
    /// Maximum discharge/inverter output power (W).
    pub max_discharge_power: i32,
    /// Maximum charge power (W).
    pub max_charge_power: i32,
    /// Current battery output power (W). Positive = discharging, negative = charging.
    pub current_power: i32,
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
    pub fn from_properties(props: &ZendureProperties) -> Self {
        let discharge = props.pack_input_power.unwrap_or(0) as i32;
        let charge = props.output_pack_power.unwrap_or(0) as i32;
        Self {
            soc: props.electric_level.unwrap_or(0),
            // Honor the device's reported caps verbatim. A reported 0 means the
            // device zeroed its own power-cap setpoint — we deliberately let that
            // stop charging/discharging rather than overwriting it mid-run (the
            // caps are only written once, at startup). An *absent* field falls
            // back to the rated cap.
            max_discharge_power: props
                .inverse_max_power
                .unwrap_or(DEVICE_MAX_DISCHARGE_POWER) as i32,
            max_charge_power: props.charge_max_limit.unwrap_or(DEVICE_MAX_CHARGE_POWER) as i32,
            current_power: discharge - charge,
            soc_calibrating: props.soc_status == Some(1),
            soc_limit_reached: props.soc_limit == Some(1),
            fault: props.is_error.unwrap_or(0) != 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ZendureProperties;

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
        let state = BatteryState::from_properties(&props);
        assert_eq!(state.max_charge_power, 0);
        assert_eq!(state.max_discharge_power, 0);
    }

    #[test]
    fn error_flag_sets_fault_but_fault_level_is_ignored() {
        assert!(!BatteryState::from_properties(&ZendureProperties::default()).fault);

        // faultLevel also goes non-zero for benign conditions (WiFi hiccups,
        // firmware update checks), so it must NOT set fault.
        let benign = ZendureProperties {
            fault_level: Some(2),
            ..Default::default()
        };
        assert!(!BatteryState::from_properties(&benign).fault);

        let errored = ZendureProperties {
            is_error: Some(1),
            ..Default::default()
        };
        assert!(BatteryState::from_properties(&errored).fault);
    }

    #[test]
    fn absent_limits_fall_back_to_default() {
        let props = ZendureProperties::default();
        let state = BatteryState::from_properties(&props);
        assert_eq!(state.max_charge_power, DEVICE_MAX_CHARGE_POWER as i32);
        assert_eq!(state.max_discharge_power, DEVICE_MAX_DISCHARGE_POWER as i32);
    }

    #[test]
    fn reported_nonzero_limits_are_respected() {
        let props = ZendureProperties {
            charge_max_limit: Some(1200),
            inverse_max_power: Some(600),
            ..Default::default()
        };
        let state = BatteryState::from_properties(&props);
        assert_eq!(state.max_charge_power, 1200);
        assert_eq!(state.max_discharge_power, 600);
    }
}
