use std::sync::Mutex;
use std::time::Duration;

use crate::battery::BatteryState;
use crate::command::Command;
use crate::device::{
    AC2400_PLUS, BatteryController, BatteryMonitor, BatteryReading, BatterySpec, BatteryTelemetry,
    PollError, RawCapture,
};
use crate::models::{PackData, StorageMode, ZendureReport, ZendureWriteRequest};
use crate::sync::guard;
use crate::units::{DeciKelvin, PackTemperature, Soc, WattHours, Watts};
use crate::world::DeviceId;

#[allow(dead_code)]
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
    #[allow(dead_code)]
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
    #[allow(dead_code)]
    pub fn set_storage_mode(&self, mode: StorageMode) {
        *guard(&self.storage_mode) = mode;
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
    pub async fn apply_command(&self, command: &Command) -> Result<(), reqwest::Error> {
        match *command {
            Command::SetCharge(power_watts) => {
                self.ensure_ram_mode().await?;
                let mut props = serde_json::json!({
                    "inputLimit": power_watts,
                });
                if self.set_ac_mode(1) {
                    props["acMode"] = serde_json::json!(1);
                }
                self.write_properties(props).await
            }
            Command::SetDischarge(power_watts) => {
                self.ensure_ram_mode().await?;
                let mut props = serde_json::json!({
                    "outputLimit": power_watts,
                });
                if self.set_ac_mode(2) {
                    props["acMode"] = serde_json::json!(2);
                }
                self.write_properties(props).await
            }
            Command::SetIdle => {
                *guard(&self.last_ac_mode) = None;
                self.write_properties(serde_json::json!({
                    "inputLimit": 0,
                    "outputLimit": 0,
                }))
                .await
            }
            Command::SetStandby => {
                *guard(&self.last_ac_mode) = None;
                self.set_storage_mode(StorageMode::Flash);
                self.write_properties(serde_json::json!({
                    "smartMode": 0,
                    "inputLimit": 0,
                    "outputLimit": 0,
                }))
                .await
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

    #[allow(dead_code)]
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
            Ok(body) => match parse_report(body, self.spec()) {
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
        parse_report(body, self.spec())
    }
}

/// Parse one raw report body into a reading, carrying the body along either
/// way. Split out from `poll` so the property this exists for —
/// [`PollError`] carrying the raw body on a parse failure — is testable
/// without a live HTTP round trip.
fn parse_report(body: String, spec: &BatterySpec) -> Result<BatteryReading, PollError> {
    let raw = RawCapture {
        kind: "zendure_poll",
        body: body.clone(),
    };
    match serde_json::from_str::<ZendureReport>(&body) {
        Ok(report) => Ok(reading_from_report(&report, Some(raw), spec)),
        Err(e) => Err(PollError {
            raw: Some(raw),
            error: format!("parse error: {e}"),
        }),
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

    let pack_capacities = report
        .pack_data
        .is_some()
        .then(|| pack_capacities(&report.pack_data));

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

/// Map a Zendure `pack_type` to its nominal capacity in Wh.
fn pack_type_capacity_wh(pack_type: u32) -> WattHours {
    match pack_type {
        // AB1000 / AB1000S
        500 => WattHours(960.0),
        // AB2000 / AB2000S
        501 => WattHours(1920.0),
        // Unknown — assume AB2000 as conservative default
        _ => {
            tracing::warn!("Unknown pack_type {pack_type}, assuming 1920 Wh");
            WattHours(1920.0)
        }
    }
}

/// Extract per-pack capacities from a report's `pack_data`.
fn pack_capacities(pack_data: &Option<Vec<PackData>>) -> Vec<WattHours> {
    match pack_data {
        Some(packs) => packs
            .iter()
            .map(|p| pack_type_capacity_wh(p.pack_type.unwrap_or(501)))
            .collect(),
        None => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pack_type_capacity() {
        assert_eq!(pack_type_capacity_wh(500).get(), 960.0);
        assert_eq!(pack_type_capacity_wh(501).get(), 1920.0);
    }

    /// The property `PollError` exists for: a response we failed to decode is
    /// the one most worth having on record, so it must carry the exact bytes
    /// that failed to parse rather than discarding them alongside the error.
    #[test]
    fn a_parse_failure_carries_the_raw_body() {
        let body = "not valid json".to_string();

        let err = parse_report(body.clone(), &AC2400_PLUS).expect_err("not valid JSON");

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
}
