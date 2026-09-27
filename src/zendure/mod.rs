//! Zendure REST API client. This file is the shell: `Wire` (the transport),
//! `ZendureClient` (the adapter `registry.rs` holds), and turning a parsed
//! report into a `BatteryReading`. `mode.rs` is the pure fold over commands
//! and reports; `ledger.rs` is the tracked state those folds run against.

use std::collections::BTreeSet;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use crate::battery::BatteryState;
use crate::command::Command;
use crate::device::{
    AC2400_PLUS, BatteryController, BatteryMonitor, BatteryReading, BatterySpec, BatteryTelemetry,
    PollError, RawCapture,
};
use crate::models::{PackData, StorageMode, ZendureReport, ZendureWriteRequest};
use crate::scan::http_client;
use crate::units::{DeciKelvin, PackTemperature, Soc, WattHours, Watts};
use crate::world::DeviceId;

mod ledger;
mod mode;

use ledger::Ledger;
use mode::{AcMode, needs_write};

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;

/// Where a Zendure REST call failed: never reached the device, reached it and
/// got a status the device itself flagged as an error, or came back 200 with
/// a body this build cannot decode. A caller — the write guards, the
/// journal — needs to tell these apart, which one bare `reqwest::Error`
/// cannot: `reqwest` alone doesn't fail a request just because the status
/// line says 500.
#[derive(Debug)]
pub enum ZendureError {
    Transport(reqwest::Error),
    Status(reqwest::Error),
    Parse(serde_json::Error),
}

impl std::fmt::Display for ZendureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ZendureError::Transport(e) => write!(f, "request failed: {e}"),
            ZendureError::Status(e) => write!(f, "device returned an error status: {e}"),
            ZendureError::Parse(e) => write!(f, "parse error: {e}"),
        }
    }
}

impl std::error::Error for ZendureError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ZendureError::Transport(e) | ZendureError::Status(e) => Some(e),
            ZendureError::Parse(e) => Some(e),
        }
    }
}

/// `reqwest::Error::is_status` is only ever `true` for an error `Response::error_for_status`
/// produced, so this is the one place that needs to know that — every `?` on a
/// `reqwest::Error` downstream gets the right variant for free.
impl From<reqwest::Error> for ZendureError {
    fn from(e: reqwest::Error) -> Self {
        if e.is_status() {
            ZendureError::Status(e)
        } else {
            ZendureError::Transport(e)
        }
    }
}

/// The transport `ZendureClient` speaks over. No trait: `registry.rs`'s
/// `Battery` exists as an enum because `BatteryController`/`BatteryMonitor`
/// aren't `dyn`-safe (RPITIT, plus a per-adapter `Error`) and it holds many
/// adapters behind one registry. `Wire` has exactly one production variant
/// and one test double behind exactly one consumer — `ZendureClient` itself —
/// so a trait would buy back nothing a `match` doesn't already give.
enum Wire {
    Http {
        http: reqwest::Client,
        base_url: String,
    },
    #[cfg(test)]
    Fake(std::sync::Arc<Fake>),
}

impl Wire {
    fn http(ip: &str, poll_interval: Duration) -> Self {
        Wire::Http {
            http: http_client(poll_interval),
            base_url: format!("http://{ip}"),
        }
    }

    /// GET `path`, rejecting a status the device itself flagged as an error
    /// before the body is ever looked at.
    async fn get(&self, path: &str) -> Result<String, ZendureError> {
        match self {
            Wire::Http { http, base_url } => {
                let response = http
                    .get(format!("{base_url}{path}"))
                    .send()
                    .await?
                    .error_for_status()?;
                Ok(response.text().await?)
            }
            #[cfg(test)]
            Wire::Fake(fake) => fake.get(),
        }
    }

    async fn post(&self, path: &str, body: &ZendureWriteRequest) -> Result<(), ZendureError> {
        match self {
            Wire::Http { http, base_url } => {
                http.post(format!("{base_url}{path}"))
                    .json(body)
                    .send()
                    .await?
                    .error_for_status()?;
                Ok(())
            }
            #[cfg(test)]
            Wire::Fake(fake) => fake.post(),
        }
    }
}

/// Canned answers for `Wire::Fake`, queued by a test before the call each one
/// answers. Errors are genuine `ZendureError`s — `mod_tests.rs`'s
/// `status_error` builds a real status error without a socket.
#[cfg(test)]
#[derive(Default)]
struct Fake {
    gets: Mutex<std::collections::VecDeque<Result<String, ZendureError>>>,
    posts: Mutex<std::collections::VecDeque<Result<(), ZendureError>>>,
}

#[cfg(test)]
impl Fake {
    fn queue_get(&self, result: Result<String, ZendureError>) {
        self.gets.lock().unwrap().push_back(result);
    }

    fn queue_post(&self, result: Result<(), ZendureError>) {
        self.posts.lock().unwrap().push_back(result);
    }

    fn get(&self) -> Result<String, ZendureError> {
        self.gets
            .lock()
            .unwrap()
            .pop_front()
            .expect("test queued no get() response")
    }

    fn post(&self) -> Result<(), ZendureError> {
        self.posts
            .lock()
            .unwrap()
            .pop_front()
            .expect("test queued no post() response")
    }
}

pub struct ZendureClient {
    wire: Wire,
    /// The serial, which is also this device's identity in the world — one
    /// field, not a `sn: String` next to a `DeviceId` that could drift from it.
    /// The write request wants it as a bare string and gets it through
    /// `Display`.
    id: DeviceId,
    /// What model is on the other end of `base_url`. The adapter is the thing
    /// that knows: it was handed an address and a serial, and every caller that
    /// needs the rated limits (startup's cap write, every `BatteryState`) can
    /// ask it instead of naming a model of its own.
    spec: BatterySpec,
    ledger: Ledger,
}

/// The shortest period this box is worth polling at: the firmware refreshes its
/// own report every 3 s, and a faster poll re-reads state it has not updated.
pub const POLL_INTERVAL_FLOOR: Duration = Duration::from_secs(3);

impl ZendureClient {
    pub fn new(ip: &str, sn: String, poll_interval: Duration) -> Self {
        Self {
            wire: Wire::http(ip, poll_interval),
            id: DeviceId::new(sn),
            // The one place the model is named. A second Zendure of a different
            // model makes this a constructor argument (and `Config` the thing
            // that says which), which is one edit here rather than one at every
            // site that builds a `BatteryState`.
            spec: AC2400_PLUS,
            ledger: Ledger::new(),
        }
    }

    /// A client wired to a canned `Fake` instead of a real device — the
    /// handle it returns alongside is how a test queues that Fake's answers.
    #[cfg(test)]
    fn fake(sn: &str) -> (Self, std::sync::Arc<Fake>) {
        let fake = std::sync::Arc::new(Fake::default());
        let client = ZendureClient {
            wire: Wire::Fake(fake.clone()),
            id: DeviceId::new(sn.to_string()),
            spec: AC2400_PLUS,
            ledger: Ledger::new(),
        };
        (client, fake)
    }

    /// This device's identity in the world — the key its `Measurement` is
    /// filed under and the address its directives carry.
    pub fn id(&self) -> &DeviceId {
        &self.id
    }

    /// The rated limits of the box on the other end, for the caller turning a
    /// device report into a `BatteryState`.
    pub fn spec(&self) -> &BatterySpec {
        &self.spec
    }

    pub async fn get_properties(&self) -> Result<ZendureReport, ZendureError> {
        let body = self.get_properties_raw().await?;
        serde_json::from_str(&body).map_err(ZendureError::Parse)
    }

    /// The same request returned as text, so the response can be captured
    /// verbatim before parsing. The device's API is undocumented and
    /// `ZendureProperties` is `Deserialize`-only, so round-tripping through our
    /// own types would discard exactly the fields worth keeping.
    pub async fn get_properties_raw(&self) -> Result<String, ZendureError> {
        self.wire.get("/properties/report").await
    }

    /// Ensure the device is in RAM mode (smartMode: 1) before sending commands.
    /// If currently in Flash mode, sends the wake command and waits 5 seconds
    /// for the device to transition.
    pub async fn ensure_ram_mode(&self) -> Result<(), ZendureError> {
        if self.ledger.is_ram() {
            return Ok(());
        }
        self.write_properties(serde_json::json!({ "smartMode": 1 }))
            .await?;
        tokio::time::sleep(Duration::from_secs(5)).await;
        self.ledger.set_storage_mode(StorageMode::Ram);
        Ok(())
    }

    /// Charge/discharge power-cap setpoints. The device can reset these to 0,
    /// stalling all power flow. Written only at startup, never mid-run — if the
    /// device zeroes a cap while running, the controller stands down rather
    /// than fighting it.
    pub async fn write_power_caps(&self) -> Result<(), ZendureError> {
        self.ensure_ram_mode().await?;
        self.write_properties(serde_json::json!({
            "chargeMaxLimit": self.spec.max_charge_power,
            "inverseMaxPower": self.spec.max_discharge_power,
        }))
        .await
    }

    /// Apply a command via the Zendure REST API. `acMode` is sent only when
    /// switching between charge and discharge, since writing it resets the
    /// inverter; SetIdle/SetStandby leave it untouched.
    ///
    /// A command the device already satisfies is dropped here rather than
    /// re-POSTed every decision interval — see [`needs_write`] for what that
    /// costs in standby.
    pub async fn apply_command(&self, command: &Command) -> Result<(), ZendureError> {
        if !needs_write(command, self.ledger.tracked_state()) {
            return Ok(());
        }
        match *command {
            Command::SetCharge(power_watts) => {
                self.ensure_ram_mode().await?;
                let mut props = serde_json::json!({
                    "inputLimit": power_watts,
                });
                let send_ac_mode = self.ledger.ac_mode_pending(AcMode::Charge);
                if send_ac_mode {
                    props["acMode"] = serde_json::json!(AcMode::Charge);
                }
                if let Err(e) = self.write_properties(props).await {
                    self.ledger.forget_input_limit();
                    if send_ac_mode {
                        self.ledger.forget_ac_mode();
                    }
                    return Err(e);
                }
                self.ledger.record_input_limit(power_watts);
                self.ledger.record_ac_mode(AcMode::Charge);
                Ok(())
            }
            Command::SetDischarge(power_watts) => {
                self.ensure_ram_mode().await?;
                let mut props = serde_json::json!({
                    "outputLimit": power_watts,
                });
                let send_ac_mode = self.ledger.ac_mode_pending(AcMode::Discharge);
                if send_ac_mode {
                    props["acMode"] = serde_json::json!(AcMode::Discharge);
                }
                if let Err(e) = self.write_properties(props).await {
                    self.ledger.forget_output_limit();
                    if send_ac_mode {
                        self.ledger.forget_ac_mode();
                    }
                    return Err(e);
                }
                self.ledger.record_output_limit(power_watts);
                self.ledger.record_ac_mode(AcMode::Discharge);
                Ok(())
            }
            // One arm, because they are one request. Standby would also write
            // `smartMode: 0`, which commits every later write to the device's
            // flash; until the wear that costs is designed for, it is not
            // commanded, and standby settles for idle's zeroed caps.
            Command::SetIdle | Command::SetStandby => {
                let written = self
                    .write_properties(serde_json::json!({
                        "inputLimit": 0,
                        "outputLimit": 0,
                    }))
                    .await;
                if let Err(e) = written {
                    self.ledger.forget_input_limit();
                    self.ledger.forget_output_limit();
                    return Err(e);
                }
                self.ledger.record_idle();
                Ok(())
            }
        }
    }

    /// Parse one raw report body into a reading, carrying the body along either
    /// way. Split out from `poll` so the property this exists for —
    /// [`PollError`] carrying the raw body on a parse failure — is testable
    /// without a live HTTP round trip.
    ///
    /// Every poll passes through here, which is why the tracked storage mode is
    /// refreshed from the device's own `smartMode` at this point: the guard in
    /// [`needs_write`] is only safe to suppress a write if the device gets to
    /// contradict it.
    fn parse_report(&self, body: String) -> Result<BatteryReading, PollError> {
        let raw = RawCapture {
            kind: "zendure_poll",
            body: body.clone(),
        };
        match serde_json::from_str::<ZendureReport>(&body) {
            Ok(report) => {
                self.ledger
                    .observe_storage_mode(report.properties.smart_mode);
                Ok(reading_from_report(&report, Some(raw), self.spec()))
            }
            Err(e) => Err(PollError {
                raw: Some(raw),
                error: format!("parse error: {e}"),
            }),
        }
    }

    pub async fn write_properties(
        &self,
        properties: serde_json::Value,
    ) -> Result<(), ZendureError> {
        let body = ZendureWriteRequest {
            sn: self.id.to_string(),
            properties,
        };
        self.wire.post("/properties/write", &body).await
    }
}

/// The adapter side of the capability trait: everything real is already in
/// `apply_command`, which the poll loop and startup path also call directly.
/// This is the seam `actuate` drives, so the control loop never names a vendor.
impl BatteryController for ZendureClient {
    type Error = ZendureError;

    fn id(&self) -> &DeviceId {
        &self.id
    }

    async fn apply(&self, command: &Command) -> Result<(), ZendureError> {
        self.apply_command(command).await
    }
}

/// The read side of the same seam: everything real is already in
/// `get_properties`/`get_properties_raw` and `write_power_caps`. This is what
/// `run.rs`'s startup handshake and poll loop drive instead of reaching for
/// those methods directly.
impl BatteryMonitor for ZendureClient {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn spec(&self) -> &BatterySpec {
        &self.spec
    }

    async fn prepare(&self) -> Result<BatteryReading, PollError> {
        // The first read. Fatal: nothing below can proceed without at least
        // one report, and there is nothing yet to fall back to.
        let initial_report = self.get_properties().await.map_err(|e| PollError {
            raw: None,
            error: e.to_string(),
        })?;

        // Sync tracked storage mode with the device's actual state: it may be in
        // Flash/standby (e.g. after an idle-timeout before a restart). Without
        // this, `ensure_ram_mode` short-circuits and never wakes the device, so
        // every write after this one commits to flash instead of staying in
        // RAM — the wear this build otherwise avoids commanding.
        let initial_storage_mode = if initial_report.properties.smart_mode == Some(1) {
            StorageMode::Ram
        } else {
            StorageMode::Flash
        };
        self.ledger.set_storage_mode(initial_storage_mode);
        tracing::info!("Device storage mode at startup: {initial_storage_mode:?}");

        // Write the charge/discharge power caps once, here at startup. The device
        // stores these as setpoints it can reset to 0; we deliberately only write
        // them at startup (never mid-run) so a device-initiated 0 stops power flow
        // until a human restarts the process, rather than being silently overwritten.
        if let Err(e) = self.write_power_caps().await {
            tracing::warn!("Failed to write power caps at startup: {e}");
        }

        // Re-read so the reading reflects the caps just written — otherwise the
        // first decision would use the pre-write (possibly 0) limits. Captured
        // raw-then-parsed like every other read, adding one extra `zendure_poll`
        // raw row per session.
        match self.get_properties_raw().await {
            Ok(body) => match self.parse_report(body) {
                Ok(reading) => Ok(reading),
                Err(e) => {
                    tracing::warn!(
                        "Failed to re-read properties after writing caps: {}",
                        e.error,
                    );
                    // The parse failed, but whatever bytes came back are
                    // still worth keeping — they ride along on the reading
                    // built from the first report, the same fallback the
                    // pre-trait handshake used.
                    Ok(reading_from_report(&initial_report, e.raw, self.spec()))
                }
            },
            Err(e) => {
                tracing::warn!("Failed to re-read properties after writing caps: {e}");
                Ok(reading_from_report(&initial_report, None, self.spec()))
            }
        }
    }

    /// Absorbs the poll arm's read: capture the body before parsing, so a
    /// payload our types cannot decode still leaves something on record.
    async fn poll(&self) -> Result<BatteryReading, PollError> {
        let body = self.get_properties_raw().await.map_err(|e| PollError {
            raw: None,
            error: e.to_string(),
        })?;
        self.parse_report(body)
    }
}

fn reading_from_report(
    report: &ZendureReport,
    raw: Option<RawCapture>,
    spec: &BatterySpec,
) -> BatteryReading {
    let state = BatteryState::from_properties(&report.properties, spec);

    let charge = Watts::from_device(report.properties.output_pack_power.unwrap_or(0));
    let discharge = Watts::from_device(report.properties.pack_input_power.unwrap_or(0));

    let pack_capacities = complete_pack_capacities(&report.pack_data, report.properties.pack_num);

    let pack_temps = report
        .pack_data
        .as_ref()
        .map(|packs| {
            packs
                .iter()
                .enumerate()
                .filter_map(|(index, p)| {
                    p.max_temp.map(|t| PackTemperature {
                        index,
                        temp: DeciKelvin(t),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    BatteryReading {
        state,
        telemetry: BatteryTelemetry {
            charge,
            discharge,
            pack_capacities,
            pack_temps,
            enclosure_temp: report.properties.hyper_tmp.map(DeciKelvin),
            min_soc: report.properties.min_soc.map(Soc::from_tenths),
        },
        raw,
    }
}

/// Nominal capacity for a `pack_type` this build recognises, `None` for one it
/// does not. Pure and total, so the table stays a table: what an unidentified
/// pack costs is [`pack_capacity`]'s decision, and it says so out loud.
fn known_pack_type_capacity(pack_type: u32) -> Option<WattHours> {
    match pack_type {
        // AC2400 Plus's own built-in pack
        500 => Some(WattHours(2400.0)),
        // AB2000 / AB2000S
        501 => Some(WattHours(1920.0)),
        _ => None,
    }
}

/// What a pack this build cannot identify is assumed to hold: the smallest in
/// the range. Understating capacity only makes `usable_kwh` and the published
/// capacity figure pessimistic, while overstating it would have them promise
/// energy the pack does not have.
const UNIDENTIFIED_PACK_CAPACITY: WattHours = WattHours(1920.0);

/// One pack's nominal capacity, naming anything this build cannot identify.
///
/// An expansion pack newer than the table — an AB3000 holds 2880 Wh — is
/// otherwise counted as [`UNIDENTIFIED_PACK_CAPACITY`], and a total that is
/// quietly ~1 kWh short reaches `RteTracker::usable_kwh`, the published
/// capacity and the dashboard alike with nothing to say it was a guess.
fn pack_capacity(pack: &PackData) -> WattHours {
    let Some(pack_type) = pack.pack_type else {
        report_unidentified_pack(None, pack.sn.as_deref());
        return UNIDENTIFIED_PACK_CAPACITY;
    };
    known_pack_type_capacity(pack_type).unwrap_or_else(|| {
        report_unidentified_pack(Some(pack_type), pack.sn.as_deref());
        UNIDENTIFIED_PACK_CAPACITY
    })
}

/// Warn once per distinct `pack_type`, not once per poll.
///
/// A pack reports on every poll, so per-report warnings are thousands of
/// identical lines a day and the one that matters scrolls away. What is being
/// reported is a gap in this build's table rather than something that
/// happened, so it needs saying once — with the serial, since closing the gap
/// means knowing which pack to go read the label off.
fn report_unidentified_pack(pack_type: Option<u32>, sn: Option<&str>) {
    static REPORTED: LazyLock<Mutex<BTreeSet<Option<u32>>>> =
        LazyLock::new(|| Mutex::new(BTreeSet::new()));

    // Taken through the poison, not past it: the only thing behind this lock is
    // a set of already-printed warnings, and a logging concern has no business
    // propagating a panic from an unrelated thread.
    let mut reported = match REPORTED.lock() {
        Ok(reported) => reported,
        Err(poisoned) => poisoned.into_inner(),
    };
    if !reported.insert(pack_type) {
        return;
    }

    let sn = sn.unwrap_or("unknown");
    let assumed_wh = UNIDENTIFIED_PACK_CAPACITY.get();
    match pack_type {
        Some(pack_type) => tracing::warn!(
            pack_type,
            sn,
            assumed_wh,
            "unrecognised packType; capacity is a guess — add it to known_pack_type_capacity"
        ),
        None => tracing::warn!(
            sn,
            assumed_wh,
            "pack reported no packType; capacity is a guess"
        ),
    }
}

/// Extract per-pack capacities from a report's `pack_data`.
fn pack_capacities(packs: &[PackData]) -> Vec<WattHours> {
    packs.iter().map(pack_capacity).collect()
}

/// Sum pack capacities, but only once `packData` looks complete. `pack_num`
/// is the device's own count of registered packs; a `packData` shorter than
/// that is a pack that hasn't reported in yet, not the true total, so it's
/// treated like `None` (the caller keeps its last known capacity) rather
/// than being published as a smaller-than-real figure. Absent `pack_num`
/// means the device didn't say how many packs to expect, so `packData` is
/// trusted as-is.
fn complete_pack_capacities(
    pack_data: &Option<Vec<PackData>>,
    pack_num: Option<u32>,
) -> Option<Vec<WattHours>> {
    let packs = pack_data.as_ref()?;
    if let Some(expected) = pack_num {
        match u32::try_from(packs.len()) {
            Ok(actual) if actual == expected => {}
            Ok(actual) => {
                tracing::warn!(
                    "packData has {actual} packs but pack_num reports {expected}; treating as incomplete"
                );
                return None;
            }
            Err(_) => return None,
        }
    }
    Some(pack_capacities(packs))
}
