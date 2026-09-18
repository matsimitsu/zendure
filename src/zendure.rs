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
use crate::sync::guard;
use crate::units::{DeciKelvin, PackTemperature, Setpoint, Soc, WattHours, Watts};
use crate::world::DeviceId;

pub struct ZendureClient {
    http: reqwest::Client,
    base_url: String,
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
    storage_mode: Mutex<StorageMode>,
    last_ac_mode: Mutex<Option<u32>>,
    /// The limits believed to be on the device, `None` until a write of ours
    /// lands. What the idle and standby guards compare a command against, so
    /// one that would change nothing costs no write.
    last_input_limit: Mutex<Option<Setpoint>>,
    last_output_limit: Mutex<Option<Setpoint>>,
}

/// What the device is believed to be holding: the tracked state the write
/// guards run on. A snapshot rather than the locks themselves, so
/// [`needs_write`] stays pure and testable.
#[derive(Debug, Clone, Copy, PartialEq)]
struct DeviceState {
    input_limit: Option<Setpoint>,
    output_limit: Option<Setpoint>,
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
fn needs_write(command: &Command, state: DeviceState) -> bool {
    match *command {
        Command::SetCharge(_) | Command::SetDischarge(_) => true,
        // Standby asks the device for what idle does — see `apply_command`.
        Command::SetIdle | Command::SetStandby => !state.is_zeroed(),
    }
}

impl ZendureClient {
    pub fn new(ip: &str, sn: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("failed to create HTTP client");

        Self {
            http,
            base_url: format!("http://{ip}"),
            id: DeviceId::new(sn),
            // The one place the model is named. A second Zendure of a different
            // model makes this a constructor argument (and `Config` the thing
            // that says which), which is one edit here rather than one at every
            // site that builds a `BatteryState`.
            spec: AC2400_PLUS,
            storage_mode: Mutex::new(StorageMode::Ram),
            last_ac_mode: Mutex::new(None),
            last_input_limit: Mutex::new(None),
            last_output_limit: Mutex::new(None),
        }
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

    pub async fn get_properties(&self) -> Result<ZendureReport, reqwest::Error> {
        self.http.get(self.report_url()).send().await?.json().await
    }

    /// The same request returned as text, so the response can be captured
    /// verbatim before parsing. The device's API is undocumented and
    /// `ZendureProperties` is `Deserialize`-only, so round-tripping through our
    /// own types would discard exactly the fields worth keeping.
    pub async fn get_properties_raw(&self) -> Result<String, reqwest::Error> {
        self.http.get(self.report_url()).send().await?.text().await
    }

    fn report_url(&self) -> String {
        format!("{}/properties/report", self.base_url)
    }

    /// Ensure the device is in RAM mode (smartMode: 1) before sending commands.
    /// If currently in Flash mode, sends the wake command and waits 5 seconds
    /// for the device to transition.
    pub async fn ensure_ram_mode(&self) -> Result<(), reqwest::Error> {
        {
            let mode = guard(&self.storage_mode);
            if *mode == StorageMode::Ram {
                return Ok(());
            }
        }
        self.write_properties(serde_json::json!({ "smartMode": 1 }))
            .await?;
        tokio::time::sleep(Duration::from_secs(5)).await;
        *guard(&self.storage_mode) = StorageMode::Ram;
        Ok(())
    }

    /// Update the tracked storage mode after an external write.
    pub fn set_storage_mode(&self, mode: StorageMode) {
        *guard(&self.storage_mode) = mode;
    }

    /// Fold what a report says about `smartMode` into the tracked mode.
    ///
    /// The firmware can fall out of RAM mode on its own, and a guard running on
    /// nothing but our own last write would then believe it is writing to RAM
    /// while the device commits every one to flash. Trusting each report keeps
    /// the guard self-healing; a report that omits the field says nothing, so
    /// it changes nothing.
    fn observe_storage_mode(&self, smart_mode: Option<u32>) {
        match smart_mode {
            Some(1) => self.set_storage_mode(StorageMode::Ram),
            Some(0) => self.set_storage_mode(StorageMode::Flash),
            _ => {}
        }
    }

    /// What the device's own reports say its storage mode is. Only
    /// `ensure_ram_mode` consults it in production.
    #[cfg(test)]
    fn tracked_storage_mode(&self) -> StorageMode {
        *guard(&self.storage_mode)
    }

    /// One consistent snapshot for the write guard to decide on.
    fn tracked_state(&self) -> DeviceState {
        DeviceState {
            input_limit: *guard(&self.last_input_limit),
            output_limit: *guard(&self.last_output_limit),
        }
    }

    /// Record what a write actually put on the device. Called after the POST
    /// returns, never before: a write that failed left the device as it was,
    /// and tracked state claiming otherwise would suppress the retry.
    fn record_input_limit(&self, limit: Setpoint) {
        *guard(&self.last_input_limit) = Some(limit);
    }

    fn record_output_limit(&self, limit: Setpoint) {
        *guard(&self.last_output_limit) = Some(limit);
    }

    /// The state a landed idle write leaves the device in. `acMode` is
    /// forgotten so the next charge or discharge re-sends it.
    fn record_idle(&self) {
        *guard(&self.last_ac_mode) = None;
        self.record_input_limit(Setpoint::ZERO);
        self.record_output_limit(Setpoint::ZERO);
    }

    /// Charge/discharge power-cap setpoints. The device can reset these to 0,
    /// stalling all power flow. Written only at startup, never mid-run — if the
    /// device zeroes a cap while running, the controller stands down rather
    /// than fighting it.
    pub async fn write_power_caps(&self) -> Result<(), reqwest::Error> {
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
    pub async fn apply_command(&self, command: &Command) -> Result<(), reqwest::Error> {
        if !needs_write(command, self.tracked_state()) {
            return Ok(());
        }
        match *command {
            Command::SetCharge(power_watts) => {
                self.ensure_ram_mode().await?;
                let mut props = serde_json::json!({
                    "inputLimit": power_watts,
                });
                if self.set_ac_mode(1) {
                    props["acMode"] = serde_json::json!(1);
                }
                self.write_properties(props).await?;
                self.record_input_limit(power_watts);
                Ok(())
            }
            Command::SetDischarge(power_watts) => {
                self.ensure_ram_mode().await?;
                let mut props = serde_json::json!({
                    "outputLimit": power_watts,
                });
                if self.set_ac_mode(2) {
                    props["acMode"] = serde_json::json!(2);
                }
                self.write_properties(props).await?;
                self.record_output_limit(power_watts);
                Ok(())
            }
            // One arm, because they are one request. Standby would also write
            // `smartMode: 0`, which commits every later write to the device's
            // flash; until the wear that costs is designed for, it is not
            // commanded, and standby settles for idle's zeroed caps.
            Command::SetIdle | Command::SetStandby => {
                self.write_properties(serde_json::json!({
                    "inputLimit": 0,
                    "outputLimit": 0,
                }))
                .await?;
                self.record_idle();
                Ok(())
            }
        }
    }

    /// Updates the tracked acMode, returns true if it changed (and should be sent).
    fn set_ac_mode(&self, mode: u32) -> bool {
        let mut last = guard(&self.last_ac_mode);
        let changed = *last != Some(mode);
        *last = Some(mode);
        changed
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
                self.observe_storage_mode(report.properties.smart_mode);
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
    ) -> Result<(), reqwest::Error> {
        let url = format!("{}/properties/write", self.base_url);
        let body = ZendureWriteRequest {
            sn: self.id.to_string(),
            properties,
        };
        self.http.post(&url).json(&body).send().await?;
        Ok(())
    }
}

/// The adapter side of the capability trait: everything real is already in
/// `apply_command`, which the poll loop and startup path still call directly.
/// This is the seam `actuate` drives, so the control loop never names a vendor.
impl BatteryController for ZendureClient {
    type Error = reqwest::Error;

    fn id(&self) -> &DeviceId {
        &self.id
    }

    async fn apply(&self, command: &Command) -> Result<(), reqwest::Error> {
        self.apply_command(command).await
    }
}

/// The read side of the same seam: everything real is already in
/// `get_properties`/`get_properties_raw`, `write_power_caps` and
/// `set_storage_mode`. This is what `run.rs`'s startup handshake and poll loop
/// now drive instead of reaching for those methods directly.
impl BatteryMonitor for ZendureClient {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn spec(&self) -> &BatterySpec {
        &self.spec
    }

    /// Absorbs, verbatim in ordering and failure policy, the handshake that
    /// used to open `run::run`: one fatal read, a storage-mode sync, a
    /// best-effort cap write, and a re-read that falls back to the first
    /// report if it fails.
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
        // it keeps reporting chargeMaxLimit=0 / inverseMaxPower=0 and every command
        // clamps to 0W.
        let initial_storage_mode = if initial_report.properties.smart_mode == Some(1) {
            StorageMode::Ram
        } else {
            StorageMode::Flash
        };
        self.set_storage_mode(initial_storage_mode);
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
            error: format!("request failed: {e}"),
        })?;
        self.parse_report(body)
    }
}

/// Build a reading from a parsed report — the vendor-shaped fields, unpacked
/// once here rather than by every caller that used to reach into a
/// `ZendureReport` itself (`run.rs`'s `PollTelemetry` chief among them).
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

#[cfg(test)]
mod tests {
    use super::*;

    fn pack(json: &str) -> PackData {
        serde_json::from_str(json).expect("pack fixture should parse")
    }

    /// A client pointed at a port nothing listens on, so any write it does
    /// attempt fails fast instead of reaching a device.
    fn client() -> ZendureClient {
        ZendureClient::new("127.0.0.1:1", "TESTSN".to_string())
    }

    /// Standby is re-decided every 5s and repeats the same two zeroes, so once
    /// they have landed the repeat has nothing to say.
    #[test]
    fn a_standby_is_written_once_and_then_suppressed() {
        let client = client();

        assert!(needs_write(&Command::SetStandby, client.tracked_state()));
        client.record_idle();

        assert!(!needs_write(&Command::SetStandby, client.tracked_state()));
    }

    /// The contract this build commits to: flash mode is never commanded, so
    /// standby and idle are one request and the guard cannot tell them apart.
    #[test]
    fn standby_asks_the_device_for_exactly_what_idle_does() {
        let client = client();
        for _ in 0..2 {
            let state = client.tracked_state();
            assert_eq!(
                needs_write(&Command::SetStandby, state),
                needs_write(&Command::SetIdle, state)
            );
            client.record_idle();
        }
    }

    /// The device can enter flash mode on its own, and its own report — never a
    /// write of ours — is what says so. `ensure_ram_mode` reads this to decide
    /// whether a wake is owed before the next charge.
    #[test]
    fn a_report_is_what_moves_the_tracked_storage_mode() {
        let client = client();

        client
            .parse_report(r#"{"properties":{"smartMode":0}}"#.to_string())
            .expect("fixture should parse");
        assert_eq!(client.tracked_storage_mode(), StorageMode::Flash);

        client
            .parse_report(r#"{"properties":{"smartMode":1}}"#.to_string())
            .expect("fixture should parse");
        assert_eq!(client.tracked_storage_mode(), StorageMode::Ram);
    }

    /// A report that says nothing about `smartMode` is not evidence the device
    /// left the mode it was in.
    #[test]
    fn a_report_without_a_smart_mode_leaves_the_tracked_mode_alone() {
        let client = client();
        client
            .parse_report(r#"{"properties":{"smartMode":0}}"#.to_string())
            .expect("fixture should parse");

        client
            .parse_report(r#"{"properties":{}}"#.to_string())
            .expect("fixture should parse");

        assert_eq!(client.tracked_storage_mode(), StorageMode::Flash);
    }

    /// Zeroing the caps is the whole of a standby now, so one still holding a
    /// non-zero cap has not been stood down yet.
    #[test]
    fn standby_is_still_written_while_a_cap_is_not_zero() {
        let client = client();
        client.record_idle();
        client.record_input_limit(Setpoint::new(800));

        assert!(needs_write(&Command::SetStandby, client.tracked_state()));
    }

    /// Idle is decided every interval too, and repeats the same two zeroes.
    #[test]
    fn an_idle_is_written_once_and_then_suppressed() {
        let client = client();

        assert!(needs_write(&Command::SetIdle, client.tracked_state()));
        client.record_idle();

        assert!(!needs_write(&Command::SetIdle, client.tracked_state()));
    }

    /// Nothing is suppressed on the strength of the command alone: until a
    /// write of ours lands, the caps are unknown, not zero.
    #[test]
    fn an_idle_is_written_while_the_caps_are_unknown() {
        let client = client();

        assert!(needs_write(&Command::SetIdle, client.tracked_state()));
        assert!(needs_write(&Command::SetIdle, client.tracked_state()));
    }

    /// Charge and discharge carry a setpoint that genuinely changes tick to
    /// tick and already write in RAM, so they are never suppressed — not even
    /// the degenerate 0 W one that matches the tracked caps.
    #[test]
    fn a_charge_or_discharge_is_never_suppressed() {
        let client = client();
        client.record_idle();

        assert!(needs_write(
            &Command::SetCharge(Setpoint::ZERO),
            client.tracked_state()
        ));
        assert!(needs_write(
            &Command::SetDischarge(Setpoint::ZERO),
            client.tracked_state()
        ));
    }

    /// A charge out of standby still goes through `ensure_ram_mode`: the write
    /// is attempted (and fails, against a closed port) rather than skipped, and
    /// nothing about the failed attempt is recorded as having landed.
    #[tokio::test]
    async fn a_charge_after_standby_still_wakes_the_device_and_writes() {
        let client = client();
        client.record_idle();
        client
            .parse_report(r#"{"properties":{"smartMode":0}}"#.to_string())
            .expect("fixture should parse");

        client
            .apply_command(&Command::SetCharge(Setpoint::new(500)))
            .await
            .expect_err("nothing is listening on port 1");

        assert_eq!(client.tracked_storage_mode(), StorageMode::Flash);
        assert_eq!(client.tracked_state().input_limit, Some(Setpoint::ZERO));
    }

    /// Tracked state is what landed, not what was attempted: a standby whose
    /// POST failed left the device where it was, and claiming otherwise would
    /// suppress the retry forever.
    #[tokio::test]
    async fn a_failed_standby_write_does_not_claim_the_device_is_in_standby() {
        let client = client();

        client
            .apply_command(&Command::SetStandby)
            .await
            .expect_err("nothing is listening on port 1");

        assert_eq!(client.tracked_storage_mode(), StorageMode::Ram);
        assert!(needs_write(&Command::SetStandby, client.tracked_state()));
    }

    #[test]
    fn the_pack_type_table_knows_the_packs_this_build_ships_with() {
        assert_eq!(known_pack_type_capacity(500), Some(WattHours(2400.0)));
        assert_eq!(known_pack_type_capacity(501), Some(WattHours(1920.0)));
    }

    /// The case this exists for: an expansion pack newer than the table. An
    /// AB3000 holds 2880 Wh, so until its `packType` is added here the total is
    /// a guess — one the controller keeps running on, but never silently.
    #[test]
    fn an_unrecognised_pack_type_is_not_in_the_table() {
        assert_eq!(known_pack_type_capacity(999), None);
    }

    /// Loud and running beats silent and stopped: an unidentified pack still
    /// yields a capacity, so the dashboard and `usable_kwh` keep working
    /// (pessimistically) rather than going blank.
    #[test]
    fn an_unidentified_pack_still_reports_a_capacity() {
        assert_eq!(
            pack_capacity(&pack(r#"{"packType":999}"#)),
            UNIDENTIFIED_PACK_CAPACITY
        );
    }

    /// A pack that reports no `packType` at all used to take the 1920 Wh
    /// default with nothing said about it — the same guess as an unrecognised
    /// type, and it deserves the same warning.
    #[test]
    fn a_pack_with_no_pack_type_is_also_unidentified() {
        assert_eq!(
            pack_capacity(&pack(r#"{"sn":"JO4AENCN4900105"}"#)),
            UNIDENTIFIED_PACK_CAPACITY
        );
    }

    /// The shape the AB3000 arrives in: a second pack alongside the built-in
    /// one, each mapped on its own type rather than the first pack's standing
    /// for both.
    #[test]
    fn each_pack_is_mapped_on_its_own_type() {
        let packs = [pack(r#"{"packType":500}"#), pack(r#"{"packType":501}"#)];

        assert_eq!(
            pack_capacities(&packs),
            vec![WattHours(2400.0), WattHours(1920.0)]
        );
    }

    /// The property `PollError` exists for: a response we failed to decode is
    /// the one most worth having on record, so it must carry the exact bytes
    /// that failed to parse rather than discarding them alongside the error.
    #[test]
    fn a_parse_failure_carries_the_raw_body() {
        let body = "not valid json".to_string();

        let err = client()
            .parse_report(body.clone())
            .expect_err("not valid JSON");

        assert_eq!(
            err.raw,
            Some(RawCapture {
                kind: "zendure_poll",
                body,
            }),
        );
        assert!(err.error.contains("parse error"));
    }

    /// A report with no `packData` at all leaves `pack_capacities` as `None`
    /// rather than `Some(vec![])` — the caller's cue to keep its last known
    /// set instead of publishing a capacity of zero.
    #[test]
    fn a_report_with_no_pack_data_reports_no_pack_capacities() {
        let report: ZendureReport = serde_json::from_str(r#"{"properties":{}}"#).unwrap();

        let reading = reading_from_report(&report, None, &AC2400_PLUS);

        assert_eq!(reading.telemetry.pack_capacities, None);
        assert!(reading.telemetry.pack_temps.is_empty());
    }

    /// `packData` matching the device's own `packNum` count is trusted and
    /// summed in full.
    #[test]
    fn pack_data_matching_pack_num_reports_full_capacity() {
        let report: ZendureReport = serde_json::from_str(
            r#"{"properties":{"packNum":2},"packData":[{"packType":500},{"packType":501}]}"#,
        )
        .unwrap();

        let reading = reading_from_report(&report, None, &AC2400_PLUS);

        assert_eq!(
            reading.telemetry.pack_capacities,
            Some(vec![WattHours(2400.0), WattHours(1920.0)])
        );
    }

    /// `packData` shorter than the device's own `packNum` count is a pack
    /// that hasn't reported in yet, not the true total — treated the same as
    /// no pack data so the caller keeps its last known capacity instead of
    /// publishing an undersized figure.
    #[test]
    fn pack_data_short_of_pack_num_reports_no_pack_capacities() {
        let report: ZendureReport =
            serde_json::from_str(r#"{"properties":{"packNum":2},"packData":[{"packType":500}]}"#)
                .unwrap();

        let reading = reading_from_report(&report, None, &AC2400_PLUS);

        assert_eq!(reading.telemetry.pack_capacities, None);
    }

    /// When the device omits `packNum` entirely, `packData` is trusted as-is
    /// rather than rejected for lack of a count to check it against.
    #[test]
    fn pack_data_without_pack_num_is_trusted_as_is() {
        let report: ZendureReport =
            serde_json::from_str(r#"{"properties":{},"packData":[{"packType":500}]}"#).unwrap();

        let reading = reading_from_report(&report, None, &AC2400_PLUS);

        assert_eq!(
            reading.telemetry.pack_capacities,
            Some(vec![WattHours(2400.0)])
        );
    }
}
