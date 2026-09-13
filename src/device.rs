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

/// Zendure solarFlow2400AC+: 800 W out is Germany's feed-in limit, the
/// read/write `inverseMaxPower` setpoint; 2400 W in is `chargeMaxLimit`. Both
/// can reset to 0 on-device, so the controller writes them at startup and
/// falls back to these when the device omits the field.
pub const AC2400_PLUS: BatterySpec = BatterySpec {
    max_charge_power: PowerCap::new(2400),
    max_discharge_power: PowerCap::new(800),
};

/// `&self`: `ZendureClient` keeps its acMode/storage-mode caches behind a
/// `Mutex`, so the actuation loop borrows it immutably inside
/// `tokio::select!`. `-> impl Future + Send` fixes the `Send` bound a bare
/// `async fn` in a trait would leave unspecified. `Error` is per-adapter, bounded only
/// by `Display`.
pub trait BatteryController {
    type Error: std::fmt::Display;

    /// Which device this adapter writes to. `Devices::new` keys the registry by
    /// it, so a directive reaches this adapter through a real map lookup rather
    /// than trusting the registry's construction — an `Outcome` naming a device
    /// means the write actually went there.
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

/// Everything a poll produces that no decision reads: round-trip efficiency,
/// pack/enclosure temperatures and the device's minimum SOC are derived from
/// these and published for graphing, but the engine never consults them.
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

/// A failure that still carries whatever bytes arrived: a response that
/// failed to decode is the one most worth keeping for a human to inspect. A
/// failure with nothing to show — the request never came back — carries
/// `None` instead.
#[derive(Debug)]
pub struct PollError {
    pub raw: Option<RawCapture>,
    pub error: String,
}

/// Separate from [`BatteryController`] because `registry::actuate` drives
/// devices through `apply` alone, so a write-only test double needn't fake
/// being readable. Same `&self` / `-> impl Future + Send` shape. No
/// associated `Error`: every adapter reports failure as [`PollError`], whose raw bytes
/// are journalled.
pub trait BatteryMonitor {
    /// Which device this adapter reads from. Mirrors
    /// [`BatteryController::id`] — the same identity answers for both halves
    /// of one physical box.
    fn id(&self) -> &DeviceId;

    /// The rated limits of the box, for turning a raw report into a
    /// `BatteryState`. `prepare`/`poll` build that themselves, so nothing
    /// outside an adapter calls this yet — it stays on the trait so a caller
    /// holding only a reading has somewhere to ask what the box is rated for.
    #[allow(dead_code)]
    fn spec(&self) -> &BatterySpec;

    /// The startup handshake, then the first reading: the Zendure must be woken
    /// into a writable mode and have its power caps rewritten before its first
    /// report can be trusted. Runs once at process start — folding it into
    /// `poll` would repeat the handshake every tick.
    fn prepare(&self) -> impl Future<Output = Result<BatteryReading, PollError>> + Send;

    /// One reading, on the interval the coordinator polls at.
    fn poll(&self) -> impl Future<Output = Result<BatteryReading, PollError>> + Send;
}

/// Whether a command landed. As a bare `&'static str` compared at call
/// sites, a typo like `== "eror"` compiled and silently reported
/// `operational` through an outage. `snake_case` serializes as the `"ok"` /
/// `"error"` the journal carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Applied {
    Ok,
    Error,
}

impl Applied {
    /// The journal's `outcome` column, as a plain `&'static str` rather than
    /// JSON-serializing and stripping quotes — which allocated, could fail into
    /// a `None` read as "commanded nothing", and would mangle a variant whose
    /// rename contained a quote. `applied_str_matches_serde` pins this against
    /// `rename_all` above.
    pub fn as_str(self) -> &'static str {
        match self {
            Applied::Ok => "ok",
            Applied::Error => "error",
        }
    }
}

/// Which path drove the device: the objective's own decision, or the
/// failsafe standing everything down. One type for four values that move
/// together: the log line, two MQTT status strings, and the journal's
/// `kind` column.
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

/// Records commands and can fail on the nth call. `pub(crate)` at module
/// scope so journal tests route through the real `actuate` path instead of
/// hand-building `Outcome`s. Tracks call order, can fail mid-sequence to
/// prove the loop continues, and answers for one device id only.
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
