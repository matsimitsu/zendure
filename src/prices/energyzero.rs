//! The real backend: EnergyZero's public wholesale feed, which answers without
//! an API key. Like `prediction::solcast`, the body is captured as text before
//! anything parses it, so an undecodable payload rides along on the error.

use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::Deserialize;

use super::{PriceError, PriceSource};
use crate::units::{CentsPerKwh, PricePoint, Timestamp};

const BASE_URL: &str = "https://api.energyzero.nl/v1/energyprices";
const HOUR_MS: i64 = 3_600_000;

/// Deserialize-only: this crate never writes to EnergyZero.
#[derive(Debug, Deserialize)]
struct EnergyZeroResponse {
    #[serde(rename = "Prices")]
    prices: Vec<EnergyZeroEntry>,
}

#[derive(Debug, Deserialize)]
struct EnergyZeroEntry {
    #[serde(rename = "readingDate")]
    reading_date: DateTime<Utc>,
    /// Euros per kWh, excluding VAT. Converted to cents at the parse boundary
    /// below, the one place the unit is allowed to change.
    price: f64,
}

pub struct EnergyZeroPrices {
    http: reqwest::Client,
}

impl EnergyZeroPrices {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .expect("failed to create HTTP client");
        EnergyZeroPrices { http }
    }
}

impl Default for EnergyZeroPrices {
    fn default() -> Self {
        Self::new()
    }
}

impl PriceSource for EnergyZeroPrices {
    async fn prices(
        &self,
        from: Timestamp,
        until: Timestamp,
    ) -> Result<Vec<PricePoint>, PriceError> {
        let url = format!(
            "{BASE_URL}?fromDate={}&tillDate={}&interval=4&usageType=1&inclBtw=false",
            iso_millis(from),
            iso_millis(until),
        );
        let body = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| PriceError::Request(e.to_string()))?
            .text()
            .await
            .map_err(|e| PriceError::Request(e.to_string()))?;
        parse_prices_response(&body).map_err(|error| PriceError::Parse { body, error })
    }
}

fn iso_millis(ts: Timestamp) -> String {
    DateTime::<Utc>::from_timestamp_millis(ts.as_millis())
        .unwrap_or_default()
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Pure over the body so it is testable without a network round trip. Each
/// point runs until the next one starts, which keeps DST days (23 or 25
/// hourly points) gap-free; the last has no successor and is assumed hourly.
fn parse_prices_response(body: &str) -> Result<Vec<PricePoint>, String> {
    let response: EnergyZeroResponse = serde_json::from_str(body).map_err(|e| e.to_string())?;
    let starts: Vec<Timestamp> = response
        .prices
        .iter()
        .map(|e| Timestamp::from(e.reading_date))
        .collect();
    Ok(response
        .prices
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let from = starts[i];
            PricePoint {
                from,
                until: starts
                    .get(i + 1)
                    .copied()
                    .unwrap_or_else(|| Timestamp::from_millis(from.as_millis() + HOUR_MS)),
                wholesale: CentsPerKwh(entry.price * 100.0),
            }
        })
        .collect())
}

#[cfg(test)]
#[path = "energyzero_tests.rs"]
mod tests;
