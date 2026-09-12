use std::sync::Mutex;
use std::time::Duration;

use crate::command::Command;
use crate::device::{AC2400_PLUS, BatteryController, BatterySpec};
use crate::models::{StorageMode, ZendureReport, ZendureWriteRequest};
use crate::sync::guard;
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

    /// Write the charge/discharge power-cap setpoints to the device.
    ///
    /// These are read/write setpoints the device can reset to 0 (which stalls
    /// all power flow). We write them **only at startup** — a deliberate,
    /// restart-controlled action — and never overwrite them mid-run, so if the
    /// device zeroes a cap while running we stand down rather than fighting it.
    pub async fn write_power_caps(&self) -> Result<(), reqwest::Error> {
        self.ensure_ram_mode().await?;
        self.write_properties(serde_json::json!({
            "chargeMaxLimit": self.spec.max_charge_power,
            "inverseMaxPower": self.spec.max_discharge_power,
        }))
        .await
    }

    /// Apply a command to the battery via the Zendure REST API.
    ///
    /// - SetCharge: wakes to RAM mode, sets acMode=1 (only on mode change) and inputLimit.
    /// - SetDischarge: wakes to RAM mode, sets acMode=2 (only on mode change) and outputLimit.
    /// - SetIdle: sets inputLimit=0, outputLimit=0 (stays in RAM mode for quick resume).
    /// - SetStandby: sets smartMode=0 (flash), inputLimit=0, outputLimit=0.
    ///
    /// acMode is only sent when switching between charge/discharge to avoid
    /// unnecessary inverter resets when just adjusting power levels.
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
