use std::sync::Mutex;
use std::time::Duration;

use crate::models::{
    ControlDecision, ControlMode, StorageMode, ZendureReport, ZendureWriteRequest,
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
        let url = format!("{}/properties/report", self.base_url);
        self.http.get(&url).send().await?.json().await
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

    /// Apply a control decision to the battery via the Zendure REST API.
    ///
    /// - Charge: wakes to RAM mode, sets acMode=1 (only on mode change) and inputLimit.
    /// - Discharge: wakes to RAM mode, sets acMode=2 (only on mode change) and outputLimit.
    /// - Idle: sets inputLimit=0, outputLimit=0 (stays in RAM mode for quick resume).
    /// - Standby: sets smartMode=0 (flash), inputLimit=0, outputLimit=0.
    ///
    /// acMode is only sent when switching between charge/discharge to avoid
    /// unnecessary inverter resets when just adjusting power levels.
    pub async fn apply_decision(&self, decision: &ControlDecision) -> Result<(), reqwest::Error> {
        match decision.mode {
            ControlMode::Charge => {
                self.ensure_ram_mode().await?;
                let mut props = serde_json::json!({
                    "inputLimit": decision.power_watts,
                });
                if self.set_ac_mode(1) {
                    props["acMode"] = serde_json::json!(1);
                }
                self.write_properties(props).await
            }
            ControlMode::Discharge => {
                self.ensure_ram_mode().await?;
                let mut props = serde_json::json!({
                    "outputLimit": decision.power_watts,
                });
                if self.set_ac_mode(2) {
                    props["acMode"] = serde_json::json!(2);
                }
                self.write_properties(props).await
            }
            ControlMode::Idle => {
                *self.last_ac_mode.lock().unwrap() = None;
                self.write_properties(serde_json::json!({
                    "inputLimit": 0,
                    "outputLimit": 0,
                }))
                .await
            }
            ControlMode::Standby => {
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
