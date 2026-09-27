//! What the device is currently believed to hold: bookkeeping behind
//! mutexes, updated only once a write has actually landed or a report has
//! actually said so. The decisions this feeds are `mode.rs`'s; this file
//! holds no decisions of its own.

use std::sync::Mutex;

use crate::models::StorageMode;
use crate::sync::guard;
use crate::units::Setpoint;

use super::mode::{self, AcMode, DeviceState};

/// The write guards' memory of what the device holds, and `ensure_ram_mode`'s
/// memory of which mode it's in. One per `ZendureClient`.
pub struct Ledger {
    storage_mode: Mutex<StorageMode>,
    last_ac_mode: Mutex<Option<AcMode>>,
    /// The limits believed to be on the device, `None` until a write of ours
    /// lands. What the idle and standby guards compare a command against, so
    /// one that would change nothing costs no write.
    last_input_limit: Mutex<Option<Setpoint>>,
    last_output_limit: Mutex<Option<Setpoint>>,
}

impl Default for Ledger {
    fn default() -> Self {
        Self::new()
    }
}

impl Ledger {
    pub fn new() -> Self {
        Ledger {
            storage_mode: Mutex::new(StorageMode::Ram),
            last_ac_mode: Mutex::new(None),
            last_input_limit: Mutex::new(None),
            last_output_limit: Mutex::new(None),
        }
    }

    pub fn is_ram(&self) -> bool {
        *guard(&self.storage_mode) == StorageMode::Ram
    }

    /// Update the tracked storage mode after an external write.
    pub fn set_storage_mode(&self, mode: StorageMode) {
        *guard(&self.storage_mode) = mode;
    }

    /// Fold what a report says about `smartMode` into the tracked mode.
    ///
    /// A second writer — the Zendure app or cloud — can flip the device out of
    /// RAM mode independently of this process, and a guard running on nothing
    /// but our own last write would then believe it is writing to RAM while
    /// the device commits every one to flash. Trusting each report keeps the
    /// guard self-healing; a report that omits the field says nothing, so it
    /// changes nothing.
    pub fn observe_storage_mode(&self, smart_mode: Option<u32>) {
        if let Some(mode) = mode::fold_smart_mode(smart_mode) {
            self.set_storage_mode(mode);
        }
    }

    /// What the device's own reports say its storage mode is. Only
    /// `ensure_ram_mode` consults it in production.
    #[cfg(test)]
    pub fn tracked_storage_mode(&self) -> StorageMode {
        *guard(&self.storage_mode)
    }

    /// What the last landed write set `acMode` to. Only `apply_command`
    /// consults it in production.
    #[cfg(test)]
    pub fn tracked_ac_mode(&self) -> Option<AcMode> {
        *guard(&self.last_ac_mode)
    }

    /// One consistent snapshot for the write guard to decide on.
    pub fn tracked_state(&self) -> DeviceState {
        DeviceState {
            input_limit: *guard(&self.last_input_limit),
            output_limit: *guard(&self.last_output_limit),
        }
    }

    /// Record what a write actually put on the device. Called after the POST
    /// returns, never before: a write that failed left the device as it was,
    /// and tracked state claiming otherwise would suppress the retry.
    pub fn record_input_limit(&self, limit: Setpoint) {
        *guard(&self.last_input_limit) = Some(limit);
    }

    pub fn record_output_limit(&self, limit: Setpoint) {
        *guard(&self.last_output_limit) = Some(limit);
    }

    /// The state a landed idle write leaves the device in. `acMode` is
    /// forgotten so the next charge or discharge re-sends it.
    pub fn record_idle(&self) {
        *guard(&self.last_ac_mode) = None;
        self.record_input_limit(Setpoint::ZERO);
        self.record_output_limit(Setpoint::ZERO);
    }

    /// Whether commanding `mode` would tell the device something it wasn't
    /// already told, and so must go out in this write at all. Read-only:
    /// recording waits for the write to land, see `record_ac_mode`.
    pub fn ac_mode_pending(&self, mode: AcMode) -> bool {
        *guard(&self.last_ac_mode) != Some(mode)
    }

    /// Record what a write actually set `acMode` to. Called after the POST
    /// returns, never before — mirroring `record_input_limit`: a write that
    /// failed left the device as it was, and recording the mode it was
    /// attempting would suppress the retry that's needed.
    pub fn record_ac_mode(&self, mode: AcMode) {
        *guard(&self.last_ac_mode) = Some(mode);
    }
}

#[cfg(test)]
#[path = "ledger_tests.rs"]
mod tests;
