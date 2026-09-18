use std::collections::{BTreeSet, VecDeque};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

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
    flash_budget: Mutex<FlashWriteBudget>,
}

/// What the device is believed to be holding: the tracked state the write
/// guards run on. A snapshot rather than the locks themselves, so
/// [`needs_write`] stays pure and testable.
#[derive(Debug, Clone, Copy, PartialEq)]
struct DeviceState {
    storage_mode: StorageMode,
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
/// `smartMode: 0` makes the device commit every written property to its flash,
/// and high-frequency writing in that mode is reported to damage the flash
/// itself — so re-asserting standby on each decision interval is not merely
/// wasteful. Suppression is on the tracked *device state*, never on the command
/// repeating: charge and discharge carry a fresh setpoint every tick, write in
/// RAM, and always go out.
fn needs_write(command: &Command, state: DeviceState, can_enter_flash: bool) -> bool {
    match *command {
        Command::SetCharge(_) | Command::SetDischarge(_) => true,
        Command::SetIdle => !state.is_zeroed(),
        // Out of budget, standby has nothing left to say that idle has not
        // already said: the caps are zero and flash is off the table until the
        // window rolls. The tracked mode stays honestly `Ram`, so the wake path
        // still sees the device as it is.
        Command::SetStandby => {
            (can_enter_flash && state.storage_mode != StorageMode::Flash) || !state.is_zeroed()
        }
    }
}

/// How many flash-committed writes this adapter will spend on the device in any
/// rolling 24 hours.
///
/// One standby round trip costs two: the `smartMode: 0` that enters flash mode,
/// and the `smartMode: 1` that leaves it, which is issued while the device is
/// still committing every write to flash. Community-cited endurance for this
/// part is ~100,000 cycles, so 50 writes/day is 25 standby cycles/day and
/// 100_000 / 50 / 365 ≈ 5.5 years even assuming no wear-levelling whatsoever.
/// Normal operation spends well under 20 a day, so this should never bind; what
/// it is here for is a pathological oscillation, which `cycle_warn_threshold`
/// alone tolerates up to ~200 mode transitions a day of.
const FLASH_WRITE_BUDGET: usize = 50;

/// The window [`FLASH_WRITE_BUDGET`] is counted over. Rolling rather than
/// calendar: the controller's daily counters already reset at midnight, and a
/// burst either side of that boundary would spend two budgets in minutes.
const FLASH_WRITE_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);

/// A rolling-window count of the writes the device commits to its flash, so an
/// oscillation the control loop is happy to keep deciding cannot wear the part
/// out.
///
/// Every method takes `now` rather than reading a clock, which keeps the window
/// testable without sleeping. `Instant` is the right clock here because it is
/// monotonic and this is the I/O adapter — the decision path still gets its time
/// from `Clock`, and `BatteryController::apply` is unchanged.
#[derive(Debug, Default)]
struct FlashWriteBudget {
    spent: VecDeque<Instant>,
    /// Whether this binding has been reported. Cleared when the window rolls, so
    /// each exhaustion costs one log line rather than one per decision interval.
    reported: bool,
}

impl FlashWriteBudget {
    /// Drop everything that has aged out of the window ending at `now`.
    fn prune(&mut self, now: Instant) {
        while self
            .spent
            .front()
            .is_some_and(|&oldest| now.saturating_duration_since(oldest) >= FLASH_WRITE_WINDOW)
        {
            self.spent.pop_front();
        }
        if self.spent.len() < FLASH_WRITE_BUDGET {
            self.reported = false;
        }
    }

    /// Whether one more flash-committed write fits in the window ending at `now`.
    fn has_headroom(&mut self, now: Instant) -> bool {
        self.prune(now);
        self.spent.len() < FLASH_WRITE_BUDGET
    }

    /// Charge one flash-committed write to the window. Recorded even when it put
    /// the count over budget: [`ensure_ram_mode`](ZendureClient::ensure_ram_mode)
    /// is never refused, so the count must be free to exceed what it permits.
    fn spend(&mut self, now: Instant) {
        self.prune(now);
        self.spent.push_back(now);
    }
}

/// The properties a standby write carries. Without `enters_flash` — the budget
/// is spent — the caps are still zeroed, which costs nothing in RAM, and only
/// the `smartMode: 0` that wears the part is left out.
fn standby_properties(enters_flash: bool) -> serde_json::Value {
    let mut properties = serde_json::json!({
        "inputLimit": 0,
        "outputLimit": 0,
    });
    if enters_flash {
        properties["smartMode"] = serde_json::json!(0);
    }
    properties
}

/// Whether a write about to be issued will be committed to the device's flash:
/// one sent while it is already in flash mode, or the `smartMode: 0` that puts
/// it there.
fn commits_to_flash(properties: &serde_json::Value, mode: StorageMode) -> bool {
    mode == StorageMode::Flash
        || properties
            .get("smartMode")
            .and_then(serde_json::Value::as_u64)
            == Some(0)
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
            flash_budget: Mutex::new(FlashWriteBudget::default()),
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
    ///
    /// Never budget-blocked, though the wake is itself flash-committed (it is
    /// issued while the device is still in flash mode, so it is charged to
    /// [`FlashWriteBudget`] and may put it over): refusing to wake would strand
    /// the battery unable to charge or discharge at all. Only the `smartMode: 0`
    /// that *enters* standby is ever skipped.
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

    /// One consistent snapshot for the write guard to decide on.
    fn tracked_state(&self) -> DeviceState {
        DeviceState {
            storage_mode: *guard(&self.storage_mode),
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

    /// The state a landed standby write leaves the device in: idle's zeroed
    /// caps, plus the flash mode `smartMode: 0` asks for.
    fn record_standby(&self) {
        self.record_idle();
        self.set_storage_mode(StorageMode::Flash);
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
        // Asked once: `can_commit_to_flash` warns the first time it refuses, and
        // the standby arm below must not ask again and warn twice.
        let enters_flash = match *command {
            Command::SetStandby => self.can_commit_to_flash(),
            _ => false,
        };
        if !needs_write(command, self.tracked_state(), enters_flash) {
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
            Command::SetIdle => {
                self.write_properties(serde_json::json!({
                    "inputLimit": 0,
                    "outputLimit": 0,
                }))
                .await?;
                self.record_idle();
                Ok(())
            }
            Command::SetStandby => {
                // `smartMode: 0` is what actually puts some units into standby
                // — under `Ram` they idle on at ~20W — so this write must not be
                // dropped, only de-duplicated.
                //
                // Zeroing the caps is free in RAM; entering flash mode is the
                // part that wears. Out of budget the device idles at ~20W
                // instead, which is the trade on purpose: more consumption,
                // never a worn-out part. Still a success — nothing failed.
                self.write_properties(standby_properties(enters_flash))
                    .await?;
                if enters_flash {
                    self.record_standby();
                } else {
                    self.record_idle();
                }
                Ok(())
            }
        }
    }

    /// Whether the device can afford another flash-committed write right now.
    ///
    /// Warns the first time the budget binds in a window rather than on every
    /// suppressed write, the way [`report_unidentified_pack`] warns once per
    /// unknown pack: what is being reported is a standing condition, not an
    /// event that keeps happening.
    fn can_commit_to_flash(&self) -> bool {
        let mut budget = guard(&self.flash_budget);
        if budget.has_headroom(Instant::now()) {
            return true;
        }
        if !budget.reported {
            budget.reported = true;
            tracing::warn!(
                spent = budget.spent.len(),
                budget = FLASH_WRITE_BUDGET,
                window_hours = FLASH_WRITE_WINDOW.as_secs() / 3600,
                "flash write budget exhausted; standby will idle at ~20W rather than commit smartMode:0"
            );
        }
        false
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
        let flash_committed = commits_to_flash(&properties, *guard(&self.storage_mode));
        let url = format!("{}/properties/write", self.base_url);
        let body = ZendureWriteRequest {
            sn: self.id.to_string(),
            properties,
        };
        self.http.post(&url).json(&body).send().await?;
        // Charged here, at the one place a write is issued, and only once the
        // POST returned — a write that never reached the device wore nothing
        // out, the same reason the tracked limits are recorded after the call.
        if flash_committed {
            guard(&self.flash_budget).spend(Instant::now());
        }
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

    /// Spend a whole window's worth of flash writes, so the next one is refused.
    fn exhaust_flash_budget(client: &ZendureClient) {
        let mut budget = guard(&client.flash_budget);
        let now = Instant::now();
        for _ in 0..FLASH_WRITE_BUDGET {
            budget.spend(now);
        }
    }

    /// The write the guard exists to stop: standby is re-decided every 5s, and
    /// each POST of `smartMode: 0` commits to the battery's flash, so once the
    /// device is in standby the repeat has nothing to say.
    #[test]
    fn a_standby_is_written_once_and_then_suppressed() {
        let client = client();

        assert!(needs_write(
            &Command::SetStandby,
            client.tracked_state(),
            true
        ));
        client.record_standby();

        assert!(!needs_write(
            &Command::SetStandby,
            client.tracked_state(),
            true
        ));
    }

    /// The property that makes suppression safe: the device can leave standby
    /// on its own, and the next poll's report — not our own last write — is
    /// what the guard believes, so the following decision re-asserts standby.
    #[test]
    fn a_report_showing_the_device_left_standby_re_arms_the_write() {
        let client = client();
        client.record_standby();

        client
            .parse_report(r#"{"properties":{"smartMode":1}}"#.to_string())
            .expect("fixture should parse");

        assert_eq!(client.tracked_state().storage_mode, StorageMode::Ram);
        assert!(needs_write(
            &Command::SetStandby,
            client.tracked_state(),
            true
        ));
    }

    /// A report that says nothing about `smartMode` is not evidence the device
    /// left the mode it was put in.
    #[test]
    fn a_report_without_a_smart_mode_leaves_the_tracked_mode_alone() {
        let client = client();
        client.record_standby();

        client
            .parse_report(r#"{"properties":{}}"#.to_string())
            .expect("fixture should parse");

        assert_eq!(client.tracked_state().storage_mode, StorageMode::Flash);
        assert!(!needs_write(
            &Command::SetStandby,
            client.tracked_state(),
            true
        ));
    }

    /// Standby zeroes the caps as well as the mode, so a device in flash mode
    /// still holding a non-zero cap has not been put into standby yet.
    #[test]
    fn standby_is_still_written_while_a_cap_is_not_zero() {
        let client = client();
        client.record_standby();
        client.record_input_limit(Setpoint::new(800));

        assert!(needs_write(
            &Command::SetStandby,
            client.tracked_state(),
            true
        ));
    }

    /// Idle is decided every interval too, and repeats the same two zeroes.
    #[test]
    fn an_idle_is_written_once_and_then_suppressed() {
        let client = client();

        assert!(needs_write(&Command::SetIdle, client.tracked_state(), true));
        client.record_idle();

        assert!(!needs_write(
            &Command::SetIdle,
            client.tracked_state(),
            true
        ));
    }

    /// Nothing is suppressed on the strength of the command alone: until a
    /// write of ours lands, the caps are unknown, not zero.
    #[test]
    fn an_idle_is_written_while_the_caps_are_unknown() {
        let client = client();

        assert!(needs_write(&Command::SetIdle, client.tracked_state(), true));
        assert!(needs_write(&Command::SetIdle, client.tracked_state(), true));
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
            client.tracked_state(),
            true
        ));
        assert!(needs_write(
            &Command::SetDischarge(Setpoint::ZERO),
            client.tracked_state(),
            true
        ));
    }

    /// A charge out of standby still goes through `ensure_ram_mode`: the write
    /// is attempted (and fails, against a closed port) rather than skipped, and
    /// nothing about the failed attempt is recorded as having landed.
    #[tokio::test]
    async fn a_charge_after_standby_still_wakes_the_device_and_writes() {
        let client = client();
        client.record_standby();

        client
            .apply_command(&Command::SetCharge(Setpoint::new(500)))
            .await
            .expect_err("nothing is listening on port 1");

        assert_eq!(client.tracked_state().storage_mode, StorageMode::Flash);
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

        assert_eq!(client.tracked_state().storage_mode, StorageMode::Ram);
        assert!(needs_write(
            &Command::SetStandby,
            client.tracked_state(),
            true
        ));
    }

    /// Writes under budget are permitted; nothing changes for a normal day.
    #[test]
    fn a_flash_write_under_budget_is_permitted() {
        let mut budget = FlashWriteBudget::default();
        let now = Instant::now();

        for _ in 0..FLASH_WRITE_BUDGET - 1 {
            assert!(budget.has_headroom(now));
            budget.spend(now);
        }

        assert!(budget.has_headroom(now));
    }

    /// The point of the whole thing: at the limit, the budget binds.
    #[test]
    fn the_budget_binds_at_the_limit() {
        let mut budget = FlashWriteBudget::default();
        let now = Instant::now();

        for _ in 0..FLASH_WRITE_BUDGET {
            budget.spend(now);
        }

        assert!(!budget.has_headroom(now));
    }

    /// The window rolls rather than resetting: writes that have aged out stop
    /// counting, so a day of oscillation does not lock standby out forever.
    #[test]
    fn writes_older_than_the_window_stop_counting() {
        let mut budget = FlashWriteBudget::default();
        let start = Instant::now();

        for _ in 0..FLASH_WRITE_BUDGET {
            budget.spend(start);
        }
        assert!(!budget.has_headroom(start));

        assert!(budget.has_headroom(start + FLASH_WRITE_WINDOW));
    }

    /// Only what has aged out is dropped — a write one second inside the window
    /// is still spent, which is what makes it a rolling window and not a daily
    /// reset.
    #[test]
    fn a_write_still_inside_the_window_keeps_counting() {
        let mut budget = FlashWriteBudget::default();
        let start = Instant::now();

        budget.spend(start);
        for _ in 1..FLASH_WRITE_BUDGET {
            budget.spend(start + Duration::from_secs(1));
        }

        assert!(!budget.has_headroom(start + FLASH_WRITE_WINDOW - Duration::from_secs(1)));
    }

    /// The exhaustion is reported once, not once per suppressed write. The flag
    /// clears only when the window rolls enough to free capacity.
    #[test]
    fn an_exhausted_budget_is_reported_once_per_window() {
        let mut budget = FlashWriteBudget::default();
        let start = Instant::now();

        for _ in 0..FLASH_WRITE_BUDGET {
            budget.spend(start);
        }
        budget.has_headroom(start);
        budget.reported = true;
        budget.has_headroom(start);
        assert!(budget.reported);

        budget.has_headroom(start + FLASH_WRITE_WINDOW);
        assert!(!budget.reported);
    }

    /// What counts: anything issued while the device is already committing to
    /// flash, plus the `smartMode: 0` that puts it in that state.
    #[test]
    fn a_write_is_flash_committed_in_flash_mode_or_when_it_enters_flash_mode() {
        let caps = standby_properties(false);

        assert!(!commits_to_flash(&caps, StorageMode::Ram));
        assert!(commits_to_flash(&caps, StorageMode::Flash));
        assert!(commits_to_flash(
            &standby_properties(true),
            StorageMode::Ram
        ));
    }

    /// The round trip the budget is denominated in. Entering standby writes
    /// `smartMode: 0`; leaving it writes `smartMode: 1` *while the device is
    /// still in flash mode*, so both are committed and one cycle costs two.
    #[test]
    fn a_standby_round_trip_costs_two_flash_writes() {
        assert!(commits_to_flash(
            &standby_properties(true),
            StorageMode::Ram
        ));
        assert!(commits_to_flash(
            &serde_json::json!({ "smartMode": 1 }),
            StorageMode::Flash
        ));
    }

    /// Out of budget, standby still zeroes the caps — free in RAM — and omits
    /// only the write that wears the flash.
    #[test]
    fn an_exhausted_budget_stops_restating_a_standby_it_has_already_reached() {
        let client = client();
        client.record_idle();
        exhaust_flash_budget(&client);

        // Caps already zero and flash unaffordable: idle has said everything
        // standby could, so the interval writes nothing at all.
        assert!(!needs_write(
            &Command::SetStandby,
            client.tracked_state(),
            false
        ));

        // The moment the window frees up, standby is attempted again with no
        // other trigger.
        assert!(needs_write(
            &Command::SetStandby,
            client.tracked_state(),
            true
        ));
    }

    #[test]
    fn an_exhausted_budget_still_zeroes_caps_that_are_not_yet_zero() {
        let client = client();
        client.record_input_limit(Setpoint::new(800));
        exhaust_flash_budget(&client);

        assert!(needs_write(
            &Command::SetStandby,
            client.tracked_state(),
            false
        ));
    }

    #[test]
    fn an_exhausted_budget_zeroes_the_limits_without_entering_flash_mode() {
        let degraded = standby_properties(false);

        assert_eq!(degraded["inputLimit"], 0);
        assert_eq!(degraded["outputLimit"], 0);
        assert!(degraded.get("smartMode").is_none());
        assert_eq!(standby_properties(true)["smartMode"], 0);
    }

    /// Waking the device is never budget-blocked: a battery that cannot leave
    /// standby cannot charge or discharge at all, which is worse than a worn
    /// flash. Against a closed port the wake fails — but it was *attempted*,
    /// where a refusal would have returned `Ok` without touching the network.
    #[tokio::test]
    async fn an_exhausted_budget_still_lets_the_device_wake() {
        let client = client();
        client.record_standby();
        exhaust_flash_budget(&client);

        client
            .ensure_ram_mode()
            .await
            .expect_err("nothing is listening on port 1");

        assert_eq!(client.tracked_state().storage_mode, StorageMode::Flash);
        assert!(!client.can_commit_to_flash());
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
