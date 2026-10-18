//! The real backend: two Solcast rooftop sites (east/west-facing panels on
//! one physical array), fetched concurrently and summed into one series.
//! The body goes through `crate::fetch`, so an undecodable payload is still
//! logged rather than silently lost.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Mutex;
use std::time::Duration;

use serde::Deserialize;

use super::{FORECAST_PERIOD, Prediction};
use crate::fetch::{self, Connection, FetchError, fetch_parsed};
use crate::schedule::AnchorKey;
use crate::sync::guard;
use crate::units::{Elapsed, SolarForecastPoint, SolarPower, Timestamp};

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
    /// ISO 8601 / RFC 3339. The end of the averaging period: Solcast labels
    /// the 13:00–13:30 estimate 13:30.
    period_end: String,
    // `pv_estimate10`/`pv_estimate90` (the uncertainty band) are in Solcast's
    // response but not carried further for v1 — see the plan's open items.
}

/// Covers the rest of today and all of tomorrow from any fetch; the
/// default is a week the dashboard never shows.
const FORECAST_HOURS: u32 = 48;

/// Requested explicitly rather than left to Solcast's default, because
/// `period_end` only names a period once its length is known.
/// Must name [`FORECAST_PERIOD`].
const PERIOD: &str = "PT30M";

/// A fresh connection per fetch: requests come hours apart, so a pooled
/// connection would be long dead by the next one.
const CONNECTION: Connection = Connection::PerRequest;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

pub struct SolcastForecaster {
    http: reqwest::Client,
    api_key: String,
    site_east: String,
    site_west: String,
    kept: Mutex<Kept>,
}

type Series = Vec<SolarForecastPoint>;

/// The sites that already answered for one anchor. A repeat of that anchor
/// asks only for the rest, so a request that succeeded is never paid for twice
/// against Solcast's daily cap.
#[derive(Default)]
struct Kept {
    anchor: Option<AnchorKey>,
    east: Option<Series>,
    west: Option<Series>,
}

impl SolcastForecaster {
    pub fn new(api_key: String, site_east: String, site_west: String) -> Self {
        let http = fetch::client(CONNECTION, REQUEST_TIMEOUT)
            .build()
            .expect("failed to create HTTP client");
        SolcastForecaster {
            http,
            api_key,
            site_east,
            site_west,
            kept: Mutex::new(Kept::default()),
        }
    }

    async fn fetch_site(&self, site_id: &str) -> Result<Vec<SolarForecastPoint>, FetchError> {
        let url = format!(
            "https://api.solcast.com.au/rooftop_sites/{site_id}/forecasts?format=json&hours={FORECAST_HOURS}&period={PERIOD}&api_key={}",
            self.api_key,
        );
        // Never log `url` — it carries the API key. Only the site id and
        // anchor name (logged by the poller) are worth a line.
        fetch_parsed(self.http.get(&url), parse_forecast_response).await
    }
}

impl SolcastForecaster {
    /// `fetch` is the per-site request, so the keep-and-retry logic is
    /// testable without a network.
    async fn forecast_with<F, Fut>(&self, anchor: AnchorKey, fetch: F) -> Result<Series, FetchError>
    where
        F: Fn(Site) -> Fut,
        Fut: Future<Output = Result<Series, FetchError>>,
    {
        let (need_east, need_west) = {
            let mut kept = guard(&self.kept);
            if kept.anchor != Some(anchor) {
                *kept = Kept {
                    anchor: Some(anchor),
                    ..Kept::default()
                };
            }
            (kept.east.is_none(), kept.west.is_none())
        };

        // `join!`, not `try_join!`: one site failing must not drop the
        // other's in-flight request, whose answer is already paid for.
        let (east, west) = tokio::join!(
            fetch_if(need_east, Site::East, &fetch),
            fetch_if(need_west, Site::West, &fetch),
        );

        let mut guarded = guard(&self.kept);
        let kept = &mut *guarded;
        let mut failure: Option<FetchError> = None;
        for (slot, result) in [(&mut kept.east, east), (&mut kept.west, west)] {
            match result {
                Some(Ok(series)) => *slot = Some(series),
                // A site that will refuse again outranks one that might
                // answer next time, so a retry never re-asks a 4xx.
                Some(Err(e)) => {
                    failure = match failure {
                        Some(kept) if !kept.is_transient() || e.is_transient() => Some(kept),
                        _ => Some(e),
                    }
                }
                None => {}
            }
        }
        match failure {
            Some(e) => Err(e),
            None => Ok(combine_series(
                kept.east.as_deref().unwrap_or_default(),
                kept.west.as_deref().unwrap_or_default(),
            )),
        }
    }
}

async fn fetch_if<F, Fut>(needed: bool, site: Site, fetch: &F) -> Option<Result<Series, FetchError>>
where
    F: Fn(Site) -> Fut,
    Fut: Future<Output = Result<Series, FetchError>>,
{
    if needed {
        Some(fetch(site).await)
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Site {
    East,
    West,
}

impl Prediction for SolcastForecaster {
    async fn forecast(&self, anchor: AnchorKey) -> Result<Vec<SolarForecastPoint>, FetchError> {
        self.forecast_with(anchor, |site| {
            let id = match site {
                Site::East => &self.site_east,
                Site::West => &self.site_west,
            };
            self.fetch_site(id)
        })
        .await
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
                // Shifted to the period start, which is how every consumer
                // buckets a point; left as the end, each estimate lands in
                // the slot after the one it describes.
                Ok(end) => Some(SolarForecastPoint {
                    at: Timestamp::from(end) - Elapsed::of(FORECAST_PERIOD),
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
