use std::sync::Mutex;
use std::time::Duration;

use crate::command::Command;
use crate::models::{
    DEVICE_MAX_CHARGE_POWER, DEVICE_MAX_DISCHARGE_POWER, StorageMode, ZendureReport,
    ZendureWriteRequest,
};

#[allow(dead_code)]
pub struct ZendureClient {
    http: reqwest::Client,
    base_url: String,
    sn: String,
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
            sn,
            storage_mode: Mutex::new(StorageMode::Ram),
            last_ac_mode: Mutex::new(None),
        }
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
            let mode = self.storage_mode.lock().unwrap();
            if *mode == StorageMode::Ram {
                return Ok(());
            }
        }
        self.write_properties(serde_json::json!({ "smartMode": 1 }))
            .await?;
        tokio::time::sleep(Duration::from_secs(5)).await;
        *self.storage_mode.lock().unwrap() = StorageMode::Ram;
        Ok(())
    }

    /// Update the tracked storage mode after an external write.
    #[allow(dead_code)]
    pub fn set_storage_mode(&self, mode: StorageMode) {
        *self.storage_mode.lock().unwrap() = mode;
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
            "chargeMaxLimit": DEVICE_MAX_CHARGE_POWER,
            "inverseMaxPower": DEVICE_MAX_DISCHARGE_POWER,
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
                *self.last_ac_mode.lock().unwrap() = None;
                self.write_properties(serde_json::json!({
                    "inputLimit": 0,
                    "outputLimit": 0,
                }))
                .await
            }
            Command::SetStandby => {
                *self.last_ac_mode.lock().unwrap() = None;
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
        let mut last = self.last_ac_mode.lock().unwrap();
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
            sn: self.sn.clone(),
            properties,
        };
        self.http.post(&url).json(&body).send().await?;
        Ok(())
    }
}
