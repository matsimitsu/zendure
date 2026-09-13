//! What a device *is*, and how a command reaches one.
//!
//! Rated limits and the capability traits a device adapter implements.

use std::future::Future;

use serde::{Deserialize, Serialize};

use crate::battery::BatteryState;
use crate::command::Command;
use crate::units::{DeciKelvin, PackTemperature, PowerCap, Soc, WattHours, Watts};
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
/// spawned task, which "never block the control loop" requires.
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

/// Bytes exactly as they arrived, before anything parsed them.
///
/// Carried *out* of the adapter rather than journalled from inside it.
/// Capture before parse, so a payload that fails to decode is still on record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawCapture {
    pub kind: &'static str,
    pub body: String,
}

/// Everything a poll produces that no decision reads.
///
/// Round-trip efficiency, pack temperatures, the enclosure and pack
/// temperatures and the device's own minimum SOC are all derived from these
/// and published for graphing; the engine never consults them. Naming the
/// group is what lets a caller take one `BatteryReading` instead of a
/// vendor-shaped report and reaching into its fields itself.
#[derive(Debug)]
pub struct BatteryTelemetry {
    pub charge: Watts,
    pub discharge: Watts,
    /// `None` when this report carried none — the caller keeps its last known
    /// set rather than publishing a capacity of zero.
    pub pack_capacities: Option<Vec<WattHours>>,
    pub pack_temps: Vec<PackTemperature>,
    pub enclosure_temp: Option<DeciKelvin>,
    pub min_soc: Option<Soc>,
}

/// One reading: what the controller decides on, plus everything else a poll
/// or the startup handshake produced.
#[derive(Debug)]
pub struct BatteryReading {
    pub state: BatteryState,
    pub telemetry: BatteryTelemetry,
    pub raw: Option<RawCapture>,
}

/// A failure that still carries whatever bytes arrived.
///
/// `RawCapture` exists as its own type, rather than living only on the
/// success path, for exactly this: a response the process failed to decode is
/// the one most worth having on record, since it is the one a person will
/// want to look at by hand. A failure with nothing to show for it — the
/// request itself never came back — carries `None` instead.
#[derive(Debug)]
pub struct PollError {
    pub raw: Option<RawCapture>,
    pub error: String,
}

/// Reading a battery's state.
///
/// A separate trait from [`BatteryController`] because the call sites differ:
/// `registry::actuate` drives devices through `apply` alone, so its write-only
/// test double would otherwise have to fake being readable.
///
/// Same shape as `BatteryController`'s: `&self`, since `ZendureClient` keeps
/// its mutable state behind a `Mutex` so the poll loop can hold it immutably
/// inside `tokio::select!`; and `-> impl Future + Send` rather than `async fn`,
/// so a generic caller sees the future's `Send`-ness spelled out rather than
/// inferred, which matters once polling moves onto a spawned task.
///
/// No associated `Error` type: every adapter reports failure as [`PollError`],
/// because the raw bytes a failure carries are what the caller journals.
pub trait BatteryMonitor {
    /// Which device this adapter reads from. Mirrors
    /// [`BatteryController::id`] — the same identity answers for both halves
    /// of one physical box.
    fn id(&self) -> &DeviceId;

    /// The rated limits of the box on the other end, for turning a raw report
    /// into a `BatteryState`.
    ///
    /// Every current caller gets a `BatteryState` already built — `prepare`
    /// and `poll` do that conversion themselves, with the adapter's own
    /// `spec` — so nothing outside an adapter calls this yet, the same way
    /// `Devices::batteries()` sat unused for one commit before `allocate`
    /// existed. Part of the trait's surface regardless: a caller that only
    /// has a reading and wants to know what the box is *rated* for, as
    /// distinct from what it just reported, has nowhere else to ask.
    #[allow(dead_code)]
    fn spec(&self) -> &BatterySpec;

    /// The startup handshake, then the first reading.
    ///
    /// Not merely "the first poll": a device like the Zendure has to be woken
    /// into a writable mode and have its power caps (re)written before its
    /// first report can be trusted, and that sequence runs once, at process
    /// start, never again. Folding it into `poll` would either repeat the
    /// wake-and-write handshake on every tick — exactly what "only at
    /// startup" forbids — or push the caller back into knowing which call is
    /// the special one, which is the coupling this split exists to remove.
    fn prepare(&self) -> impl Future<Output = Result<BatteryReading, PollError>> + Send;

    /// One reading, on the interval the coordinator polls at.
    fn poll(&self) -> impl Future<Output = Result<BatteryReading, PollError>> + Send;
}

/// Whether a command landed. A two-state role, so it gets a type: as a
/// `&'static str` compared at the call sites, `== "eror"` compiled and quietly
/// reported `operational` straight through an outage. The variants are the only
/// two values that exist, and the compiler checks the comparison.
///
/// `snake_case` so it serializes as the `"ok"` / `"error"` the journal carries.
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
/// logged, the two MQTT status strings, and the journal's `kind` column.
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
