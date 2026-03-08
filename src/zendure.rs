use std::time::Duration;

use crate::models::{ZendureReport, ZendureWriteRequest};

#[allow(dead_code)]
pub struct ZendureClient {
    http: reqwest::Client,
    base_url: String,
    sn: String,
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
        }
    }

    pub async fn get_properties(&self) -> Result<ZendureReport, reqwest::Error> {
        let url = format!("{}/properties/report", self.base_url);
        self.http.get(&url).send().await?.json().await
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
