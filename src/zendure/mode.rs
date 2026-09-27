//! The pure fold: what a command still has to say to a device already in a
//! known state, and what a report's `smartMode` field means for the tracked
//! storage mode. No I/O, no locks — `ledger.rs` holds the state these read.

use serde::Serialize;

use crate::command::Command;
use crate::models::StorageMode;
use crate::units::Setpoint;

/// The device's `acMode` property. `1`/`2` as bare literals let a Charge write
/// silently carry a Discharge's mode number (or vice versa) past the compiler;
/// spelled out, the only way to send the wrong one is to call the wrong
/// variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcMode {
    Charge,
    Discharge,
}

impl AcMode {
    fn wire(self) -> u32 {
        match self {
            AcMode::Charge => 1,
            AcMode::Discharge => 2,
        }
    }
}

/// The wire format is a bare `u32`, matching what `ZendureProperties::ac_mode`
/// already expects on read. `serde(into)` would need `AcMode: Clone +
/// Into<u32>` on the type itself, so this is spelled out instead.
impl Serialize for AcMode {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u32(self.wire())
    }
}

/// What the device is believed to be holding: the tracked state the write
/// guards run on. A snapshot rather than the locks themselves, so
/// [`needs_write`] stays pure and testable.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DeviceState {
    pub input_limit: Option<Setpoint>,
    pub output_limit: Option<Setpoint>,
}

impl DeviceState {
    /// Both caps known to be zero — not merely unknown, which is what `None`
    /// means and why it never suppresses a write.
    fn is_zeroed(self) -> bool {
        self.input_limit == Some(Setpoint::ZERO) && self.output_limit == Some(Setpoint::ZERO)
    }
}

/// Whether `command` still has anything to say to a device already in `state`.
///
/// Suppression is on the tracked *device state*, never on the command
/// repeating: charge and discharge carry a fresh setpoint every tick and always
/// go out.
pub fn needs_write(command: &Command, state: DeviceState) -> bool {
    match *command {
        Command::SetCharge(_) | Command::SetDischarge(_) => true,
        // Standby asks the device for what idle does — see `apply_command`.
        Command::SetIdle | Command::SetStandby => !state.is_zeroed(),
    }
}

/// What a report's `smartMode` field says the device's storage mode now is.
/// `None` when the field said nothing at all, which is not evidence of
/// anything and must change nothing.
pub fn fold_smart_mode(smart_mode: Option<u32>) -> Option<StorageMode> {
    match smart_mode {
        Some(1) => Some(StorageMode::Ram),
        Some(0) => Some(StorageMode::Flash),
        _ => None,
    }
}

#[cfg(test)]
#[path = "mode_tests.rs"]
mod tests;
