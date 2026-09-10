use std::fmt;

use crate::models::{ControlDecision, ControlMode};

/// What the controller wants sent to the device, separate from the decision
/// that produced it (which also carries the HA-published `reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)]
pub enum Command {
    SetCharge(i32),
    SetDischarge(i32),
    SetIdle,
    SetStandby,
}

impl From<&ControlDecision> for Command {
    fn from(decision: &ControlDecision) -> Self {
        match decision.mode {
            ControlMode::Charge => Command::SetCharge(decision.power_watts),
            ControlMode::Discharge => Command::SetDischarge(decision.power_watts),
            ControlMode::Idle => Command::SetIdle,
            ControlMode::Standby => Command::SetStandby,
        }
    }
}

impl fmt::Display for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Command::SetCharge(w) => write!(f, "set_charge({w}W)"),
            Command::SetDischarge(w) => write!(f, "set_discharge({w}W)"),
            Command::SetIdle => write!(f, "set_idle"),
            Command::SetStandby => write!(f, "set_standby"),
        }
    }
}
