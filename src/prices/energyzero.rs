//! The real backend: EnergyZero's public wholesale feed, which answers without
//! an API key. The body goes through `crate::fetch`, so an undecodable payload
//! rides along on the error.

use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::Deserialize;

use super::{PricePoint, PriceSource};
use crate::fetch::{FetchError, fetch_parsed};
use crate::units::{CentsPerKwh, Elapsed, Timestamp};

const BASE_URL: &str = "https://api.energyzero.nl/v1/energyprices";

/// The feed's resolution with `interval=4`.
const INTERVAL: Elapsed = Elapsed::HOUR;

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
    ) -> Result<Vec<PricePoint>, FetchError> {
        let points = fetch_parsed(
            self.http.get(request_url(from, until)),
            parse_prices_response,
        )
        .await?;
        Ok(within(points, from, until))
    }
}

/// `tillDate` is inclusive: asking up to `until` itself returns the hour
/// starting there too.
fn request_url(from: Timestamp, until: Timestamp) -> String {
    format!(
        "{BASE_URL}?fromDate={}&tillDate={}&interval=4&usageType=1&inclBtw=false",
        iso_millis(from),
        iso_millis(until - Elapsed::MILLISECOND),
    )
}

fn iso_millis(ts: Timestamp) -> String {
    DateTime::<Utc>::from_timestamp_millis(ts.as_millis())
        .unwrap_or_default()
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// The points starting in `[from, until)`, whatever the feed sent back.
fn within(points: Vec<PricePoint>, from: Timestamp, until: Timestamp) -> Vec<PricePoint> {
    points
        .into_iter()
        .filter(|point| from <= point.from && point.from < until)
        .collect()
}

/// Pure over the body so it is testable without a network round trip. Every
/// point lasts one `INTERVAL`, so an hour missing from the feed stays a gap
/// rather than stretching its predecessor's price over it.
fn parse_prices_response(body: &str) -> Result<Vec<PricePoint>, String> {
    let response: EnergyZeroResponse = serde_json::from_str(body).map_err(|e| e.to_string())?;
    Ok(response
        .prices
        .iter()
        .map(|entry| {
            let from = Timestamp::from(entry.reading_date);
            PricePoint {
                from,
                until: from + INTERVAL,
                wholesale: CentsPerKwh(entry.price * 100.0),
            }
        })
        .collect())
}

#[cfg(test)]
#[path = "energyzero_tests.rs"]
mod tests;
