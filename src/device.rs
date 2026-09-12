//! What a device *is*, and how a command reaches one.
//!
//! Two halves that belong together. The rated limits used to live as consts in
//! `models.rs` next to the wire types, which put a fact about the hardware in
//! the same file as the JSON it happens to be written with. They belong here:
//! one place to name a model, so a second one (a different Zendure, or another
//! vendor entirely) is a new `BatterySpec` rather than another pair of consts
//! to keep in sync.
//!
//! The other half is the capability trait a device adapter implements.
//! `main.rs` used to reach straight for `ZendureClient` and apply
//! `commands.first()`, silently dropping the rest — harmless with one device,
//! wrong the moment a step means "stop charging the car, start charging the
//! battery". The loop that drives every adapter from a list, `actuate`, lives
//! in [`crate::registry`] alongside the map it dispatches through — this
//! module only names the capability, not how many boxes implement it.

use std::future::Future;

use serde::{Deserialize, Serialize};

use crate::command::Command;
use crate::units::PowerCap;
use crate::world::DeviceId;

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

/// Writing a command to a battery.
///
/// `&self`, not `&mut self`: `ZendureClient` keeps its acMode and storage-mode
/// caches behind a `Mutex`, which is what lets the actuation loop borrow it
/// immutably from inside `tokio::select!`.
///
/// `-> impl Future` rather than `async fn` so the `Send` bound is written out.
/// A bare `async fn` in a trait leaves the future's auto-traits unspecified for
/// generic callers, which would bite the first time actuation moves onto a
/// spawned task — which step 7's "never block the control loop" requires.
///
/// `Error` is an associated type bounded only by `Display`, so an adapter
/// keeps its own error (`reqwest::Error` here) and `actuate` stringifies it at
/// the edge rather than every adapter converting into a shared error enum
/// nobody matches on.
pub trait BatteryController {
    type Error: std::fmt::Display;

    /// Which device this adapter writes to. `Devices::new` keys the registry
    /// by it, so a directive's address reaches this adapter by a real map
    /// lookup rather than by trusting whoever constructed the registry to
    /// have put it in the right slot — an `Outcome` that names a device must
    /// mean the write actually went there.
    fn id(&self) -> &DeviceId;

    fn apply(&self, command: &Command) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// Whether a command landed. A two-state role, so it gets a type: as a
/// `&'static str` compared at the call sites, `== "eror"` compiled and quietly
/// reported `operational` straight through an outage. The variants are the only
/// two values that exist, and the compiler checks the comparison.
///
/// `snake_case` so it serializes as the `"ok"` / `"error"` the journal already
/// carries — step 7's `decisions.outcome` column and the README example read
/// the same bytes as before.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Applied {
    Ok,
    Error,
}

impl Applied {
    /// The journal's `outcome` column. A plain `&'static str`, like
    /// `ControlPath`'s and `ControlMode`'s `Display`, rather than serializing to
    /// JSON and stripping the quotes back off — which allocated, could fail
    /// into a `None` that read as "commanded nothing", and would have silently
    /// mangled any future variant whose rename contained a quote.
    ///
    /// `applied_str_matches_serde` pins these against the `rename_all` above,
    /// since the two now have to agree.
    pub fn as_str(self) -> &'static str {
        match self {
            Applied::Ok => "ok",
            Applied::Error => "error",
        }
    }
}

/// Which path is driving the device: the objective's own decision, or the
/// failsafe standing everything down.
///
/// One type for four values that have to move together — how the actuation is
/// logged, the two MQTT status strings, and the journal's `kind` column. They
/// used to be an `Actuation` and a separate `DecisionKind` passed fourteen
/// lines apart in the same branch, plus two bare string literals at the call
/// site, with nothing checking they agreed. The charger this anticipates adds one
/// variant here instead of four coordinated edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlPath {
    Objective,
    Failsafe,
}

impl ControlPath {
    /// The journal's `kind` column. Deliberately not `Display`: the log says
    /// "failsafe idle" and the column says "failsafe", and both are pinned.
    pub fn journal_kind(self) -> &'static str {
        match self {
            ControlPath::Objective => "decision",
            ControlPath::Failsafe => "failsafe",
        }
    }

    /// Published to HA when every device took its command.
    pub fn ok_status(self) -> &'static str {
        match self {
            ControlPath::Objective => "operational",
            ControlPath::Failsafe => "mqtt_timeout",
        }
    }

    /// Published instead when any device refused it.
    pub fn err_status(self) -> &'static str {
        match self {
            ControlPath::Objective => "zendure_api_error",
            ControlPath::Failsafe => "mqtt_timeout_api_error",
        }
    }
}

/// Renders exactly the two literals the `tracing::error!` lines interpolated
/// before, so an operator's existing grep over the logs still matches.
impl std::fmt::Display for ControlPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ControlPath::Objective => "decision",
            ControlPath::Failsafe => "failsafe idle",
        })
    }
}

/// One command's fate, as recorded. `command` is the `Display` string, which is
/// the format `command_tests.rs` pins and the raw log already quotes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Outcome {
    pub device: DeviceId,
    pub command: String,
    /// The field is `applied` because `outcome.outcome` stutters; the JSON key
    /// stays `outcome` because the journal is append-only and lines already on
    /// disk have to keep reading the same way as the ones written tomorrow.
    #[serde(rename = "outcome")]
    pub applied: Applied,
    pub error: Option<String>,
}

/// A battery that records what it was asked to do, and can be told to fail
/// on the nth call.
///
/// At module scope and `pub(crate)` because the journal's tests need a device
/// to actuate against: routing their recorded `command` column through
/// `actuate` rather than hand-building `Outcome`s is what stops a
/// replay-versus-recording comparison from being two expressions of one local
/// variable.
///
/// Three halves: the order the commands arrived in is the property under test,
/// a failure has to be injectable at a position other than the last one to
/// prove the loop keeps going, and it answers for one device id so a directive
/// addressed elsewhere has somewhere to not go.
#[cfg(test)]
pub(crate) struct RecordingBattery {
    id: DeviceId,
    applied: std::sync::Mutex<Vec<Command>>,
    fails_at: Option<usize>,
}

#[cfg(test)]
impl RecordingBattery {
    pub(crate) fn new(id: &str) -> Self {
        RecordingBattery {
            id: DeviceId::new(id),
            applied: std::sync::Mutex::new(Vec::new()),
            fails_at: None,
        }
    }

    pub(crate) fn failing_at(id: &str, index: usize) -> Self {
        RecordingBattery {
            id: DeviceId::new(id),
            applied: std::sync::Mutex::new(Vec::new()),
            fails_at: Some(index),
        }
    }

    pub(crate) fn applied(&self) -> Vec<Command> {
        self.applied.lock().unwrap().clone()
    }
}

#[cfg(test)]
impl BatteryController for RecordingBattery {
    type Error = String;

    fn id(&self) -> &DeviceId {
        &self.id
    }

    async fn apply(&self, command: &Command) -> Result<(), String> {
        // The guard is scoped and dropped before the function's implicit
        // await point for the same reason `ZendureClient::apply_command`
        // scopes its: a `MutexGuard` alive across an await would cost the
        // future its `Send`, and the trait demands it.
        let index = {
            let mut applied = self.applied.lock().unwrap();
            applied.push(*command);
            applied.len() - 1
        };

        if self.fails_at == Some(index) {
            return Err("device unreachable".to_string());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `as_str` and the serde rename are two spellings of one string, and the
    /// journal reaches for both — `as_str` fills the indexed `outcome` column,
    /// `Serialize` is what the raw capture wrote before it. Same class of drift
    /// as `event.rs`'s `the_serde_tag_agrees_with_kind`, same guard.
    #[test]
    fn applied_str_matches_serde() {
        for applied in [Applied::Ok, Applied::Error] {
            let serialized = serde_json::to_string(&applied).unwrap();
            assert_eq!(serialized, format!(r#""{}""#, applied.as_str()));
        }
        assert_eq!(Applied::Ok.as_str(), "ok");
        assert_eq!(Applied::Error.as_str(), "error");
    }
}
