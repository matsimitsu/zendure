use crate::models::{ControlDecision, ControlMode, ZendureProperties};
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
}

impl BatteryState {
    pub fn from_properties(props: &ZendureProperties) -> Self {
        let discharge = props.pack_input_power.unwrap_or(0) as i32;
        let charge = props.output_pack_power.unwrap_or(0) as i32;
        Self {
            soc: props.electric_level.unwrap_or(0),
            max_discharge_power: props.inverse_max_power.unwrap_or(800) as i32,
            max_charge_power: props.charge_max_limit.unwrap_or(2400) as i32,
            current_power: discharge - charge,
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
        let props = match decision.mode {
            ControlMode::Charge => serde_json::json!({
                "acMode": 1,
                "inputLimit": decision.power_watts,
            }),
            ControlMode::Discharge => serde_json::json!({
                "acMode": 2,
                "outputLimit": decision.power_watts,
            }),
            ControlMode::Idle => serde_json::json!({
                "acMode": 1,
                "inputLimit": 0,
            }),
            ControlMode::Standby => serde_json::json!({
                "acMode": 1,
                "inputLimit": 0,
                "smartMode": 0,
            }),
        };
        self.write_properties(props).await?;
        Ok(())
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
