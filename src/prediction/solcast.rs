//! The real backend: two Solcast rooftop sites (east/west-facing panels on
//! one physical array), fetched concurrently and summed into one series.
//! Mirrors `zendure.rs`'s rule: capture the response body as text first, then
//! parse, so an undecodable payload is still logged rather than silently
//! lost.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Deserialize;

use super::{ForecastError, Prediction};
use crate::units::{SolarForecastPoint, SolarPower, Timestamp};

/// Deserialize-only, like `ZendureReport` — this crate never writes to
/// Solcast, only reads its forecast.
#[derive(Debug, Deserialize)]
struct SolcastResponse {
    forecasts: Vec<SolcastEntry>,
}

#[derive(Debug, Deserialize)]
struct SolcastEntry {
    /// Kilowatts. Converted to watts at the parse boundary below, the one
    /// named place a unit crossing a boundary is allowed to change (per
    /// `CLAUDE.md`'s rule on casts).
    pv_estimate: f64,
    /// ISO 8601 / RFC 3339.
    period_end: String,
    // `pv_estimate10`/`pv_estimate90` (the uncertainty band) are in Solcast's
    // response but not carried further for v1 — see the plan's open items.
}

pub struct SolcastForecaster {
    http: reqwest::Client,
    api_key: String,
    site_east: String,
    site_west: String,
}

impl SolcastForecaster {
    pub fn new(api_key: String, site_east: String, site_west: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .expect("failed to create HTTP client");
        SolcastForecaster {
            http,
            api_key,
            site_east,
            site_west,
        }
    }

    /// The response body, captured as text before anything tries to parse
    /// it — same rule `zendure.rs`'s `get_properties_raw` follows.
    async fn fetch_site_raw(&self, site_id: &str) -> Result<String, ForecastError> {
        let url = format!(
            "https://api.solcast.com.au/rooftop_sites/{site_id}/forecasts?format=json&api_key={}",
            self.api_key,
        );
        // Never log `url` — it carries the API key. Only the site id and
        // anchor name (logged by the poller) are worth a line.
        self.http
            .get(&url)
            .send()
            .await
            .map_err(|e| ForecastError::Request(e.to_string()))?
            .text()
            .await
            .map_err(|e| ForecastError::Request(e.to_string()))
    }

    async fn fetch_site(&self, site_id: &str) -> Result<Vec<SolarForecastPoint>, ForecastError> {
        let body = self.fetch_site_raw(site_id).await?;
        parse_forecast_response(&body).map_err(|error| ForecastError::Parse { body, error })
    }
}

impl Prediction for SolcastForecaster {
    async fn forecast(&self) -> Result<Vec<SolarForecastPoint>, ForecastError> {
        let (east, west) = tokio::try_join!(
            self.fetch_site(&self.site_east),
            self.fetch_site(&self.site_west),
        )?;
        Ok(combine_series(&east, &west))
    }
}

/// Parses a Solcast forecast response body into points, kW converted to W.
/// An entry whose `period_end` fails to parse is skipped (warned) rather than
/// failing the whole batch — free-standing so it's unit-testable with no
/// network round trip, the same reason `zendure::parse_report` is split out.
fn parse_forecast_response(body: &str) -> Result<Vec<SolarForecastPoint>, String> {
    let response: SolcastResponse = serde_json::from_str(body).map_err(|e| e.to_string())?;

    Ok(response
        .forecasts
        .into_iter()
        .filter_map(
            |entry| match chrono::DateTime::parse_from_rfc3339(&entry.period_end) {
                Ok(at) => Some(SolarForecastPoint {
                    at: Timestamp::from(at),
                    estimate: SolarPower::new(entry.pv_estimate * 1000.0),
                }),
                Err(e) => {
                    tracing::warn!(
                        "Solcast: skipping a forecast entry with unparseable period_end {:?}: {e}",
                        entry.period_end,
                    );
                    None
                }
            },
        )
        .collect())
}

/// Sums two sites' series by matching timestamp — one physical array split
/// across two Solcast site ids by orientation, so their point estimates add
/// up to the array's total predicted production. A timestamp present in only
/// one series is still included, not dropped, in case the two sites' forecast
/// runs are ever slightly misaligned. Output is sorted by time as a
/// side-effect of accumulating through a `BTreeMap`.
fn combine_series(a: &[SolarForecastPoint], b: &[SolarForecastPoint]) -> Vec<SolarForecastPoint> {
    let mut totals: BTreeMap<Timestamp, f64> = BTreeMap::new();
    for point in a.iter().chain(b.iter()) {
        *totals.entry(point.at).or_insert(0.0) += point.estimate.get();
    }
    totals
        .into_iter()
        .map(|(at, watts)| SolarForecastPoint {
            at,
            estimate: SolarPower::new(watts),
        })
        .collect()
}

#[cfg(test)]
#[path = "solcast_tests.rs"]
mod tests;
