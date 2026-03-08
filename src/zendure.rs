use std::sync::Mutex;
use std::time::Duration;

use crate::models::{StorageMode, ZendureReport, ZendureWriteRequest};

#[allow(dead_code)]
pub struct ZendureClient {
    http: reqwest::Client,
    base_url: String,
    sn: String,
    storage_mode: Mutex<StorageMode>,
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
