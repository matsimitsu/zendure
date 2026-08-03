use crate::models::{
    ControlDecision, ControlMode, DEVICE_MAX_CHARGE_POWER, DEVICE_MAX_DISCHARGE_POWER, StorageMode,
    ZendureProperties,
};
use crate::zendure::ZendureClient;

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
}

impl BatteryState {
    pub fn from_properties(props: &ZendureProperties) -> Self {
        let discharge = props.pack_input_power.unwrap_or(0) as i32;
        let charge = props.output_pack_power.unwrap_or(0) as i32;
        Self {
            soc: props.electric_level.unwrap_or(0),
            // A reported 0 means the device's power-cap setpoint was reset (not
            // that it genuinely can't charge/discharge), so fall back to the
            // rated cap — same as an absent field.
            max_discharge_power: props
                .inverse_max_power
                .filter(|&v| v > 0)
                .unwrap_or(DEVICE_MAX_DISCHARGE_POWER) as i32,
            max_charge_power: props
                .charge_max_limit
                .filter(|&v| v > 0)
                .unwrap_or(DEVICE_MAX_CHARGE_POWER) as i32,
            current_power: discharge - charge,
            soc_calibrating: props.soc_status == Some(1),
            soc_limit_reached: props.soc_limit == Some(1),
        }
    }
}

/// Errors that can occur when interacting with a battery.
#[derive(Debug)]
#[allow(dead_code)]
pub enum BatteryError {
    /// HTTP / network error communicating with the device.
    Http(reqwest::Error),
    /// Any other error.
    Other(String),
}

impl std::fmt::Display for BatteryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BatteryError::Http(e) => write!(f, "HTTP error: {e}"),
            BatteryError::Other(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for BatteryError {}

impl From<reqwest::Error> for BatteryError {
    fn from(e: reqwest::Error) -> Self {
        BatteryError::Http(e)
    }
}

/// Abstraction over a battery system (real Zendure device or mock).
#[allow(dead_code)]
pub trait Battery {
    /// Read current battery state (SoC, limits).
    fn get_state(
        &self,
    ) -> impl std::future::Future<Output = Result<BatteryState, BatteryError>> + Send;

    /// Apply a control decision to the battery.
    #[allow(dead_code)]
    fn apply(
        &self,
        decision: &ControlDecision,
    ) -> impl std::future::Future<Output = Result<(), BatteryError>> + Send;
}

impl Battery for ZendureClient {
    async fn get_state(&self) -> Result<BatteryState, BatteryError> {
        let report = self.get_properties().await?;
        Ok(BatteryState::from_properties(&report.properties))
    }

    async fn apply(&self, decision: &ControlDecision) -> Result<(), BatteryError> {
        match decision.mode {
            ControlMode::Charge => {
                self.ensure_ram_mode().await?;
                self.write_properties(serde_json::json!({
                    "acMode": 1,
                    "inputLimit": decision.power_watts,
                }))
                .await?;
            }
            ControlMode::Discharge => {
                self.ensure_ram_mode().await?;
                self.write_properties(serde_json::json!({
                    "acMode": 2,
                    "outputLimit": decision.power_watts,
                }))
                .await?;
            }
            ControlMode::Idle => {
                self.write_properties(serde_json::json!({
                    "acMode": 1,
                    "inputLimit": 0,
                }))
                .await?;
            }
            ControlMode::Standby => {
                self.write_properties(serde_json::json!({
                    "acMode": 1,
                    "inputLimit": 0,
                    "smartMode": 0,
                }))
                .await?;
                self.set_storage_mode(StorageMode::Flash);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ZendureProperties;

    #[test]
    fn reported_zero_charge_limit_falls_back_to_default() {
        // Device reset chargeMaxLimit to 0 — treat as the rated cap, not "can't charge".
        let props = ZendureProperties {
            electric_level: Some(34),
            charge_max_limit: Some(0),
            inverse_max_power: Some(0),
            ..Default::default()
        };
        let state = BatteryState::from_properties(&props);
        assert_eq!(state.max_charge_power, DEVICE_MAX_CHARGE_POWER as i32);
        assert_eq!(state.max_discharge_power, DEVICE_MAX_DISCHARGE_POWER as i32);
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

#[cfg(test)]
pub mod mock {
    use super::*;
    use std::sync::Mutex;

    /// A mock battery for testing. State is configurable and all applied
    /// decisions are recorded for assertions.
    pub struct MockBattery {
        state: Mutex<BatteryState>,
        pub applied: Mutex<Vec<ControlDecision>>,
    }

    impl MockBattery {
        pub fn new(state: BatteryState) -> Self {
            Self {
                state: Mutex::new(state),
                applied: Mutex::new(Vec::new()),
            }
        }

        /// Update the battery state (e.g. change SoC mid-test).
        pub fn set_state(&self, state: BatteryState) {
            *self.state.lock().unwrap() = state;
        }
    }

    impl Battery for MockBattery {
        async fn get_state(&self) -> Result<BatteryState, BatteryError> {
            Ok(self.state.lock().unwrap().clone())
        }

        async fn apply(&self, decision: &ControlDecision) -> Result<(), BatteryError> {
            self.applied.lock().unwrap().push(decision.clone());
            Ok(())
        }
    }
}
