//! A synthetic backend: a deterministic day-shaped curve, no network call.
//! Lets the dashboard and analysis be exercised locally without a real feed.

use super::{PriceError, PriceSource};
use crate::units::{CentsPerKwh, PricePoint, Timestamp};

const HOUR_MS: i64 = 3_600_000;
const DAY_HOURS: i64 = 24;

pub struct SimulatedPrices;

impl SimulatedPrices {
    pub fn new() -> Self {
        SimulatedPrices
    }
}

impl Default for SimulatedPrices {
    fn default() -> Self {
        Self::new()
    }
}

impl PriceSource for SimulatedPrices {
    async fn prices(
        &self,
        from: Timestamp,
        until: Timestamp,
    ) -> Result<Vec<PricePoint>, PriceError> {
        Ok(hourly_curve(from, until))
    }
}

/// Cheap overnight, dipping below zero around midday (solar surplus), peaking
/// in the evening. Keyed on the UTC hour of day alone, so the same hour always
/// prices the same and no clock or randomness is involved.
fn wholesale_for_hour(hour: i64) -> CentsPerKwh {
    CentsPerKwh(match hour {
        0..=5 => 8.0,
        6..=8 => 16.0,
        9..=10 => 9.0,
        11..=14 => -1.5,
        15..=16 => 10.0,
        17..=21 => 28.0,
        _ => 14.0,
    })
}

/// One point per whole UTC hour, from the hour containing `from` through the
/// hour containing the last instant before `until`, so the range is covered
/// even when its ends fall mid-hour.
fn hourly_curve(from: Timestamp, until: Timestamp) -> Vec<PricePoint> {
    let first = from.as_millis().div_euclid(HOUR_MS);
    let end = (until.as_millis() + HOUR_MS - 1).div_euclid(HOUR_MS);
    (first..end)
        .map(|h| PricePoint {
            from: Timestamp::from_millis(h * HOUR_MS),
            until: Timestamp::from_millis((h + 1) * HOUR_MS),
            wholesale: wholesale_for_hour(h.rem_euclid(DAY_HOURS)),
        })
        .collect()
}

#[cfg(test)]
#[path = "simulated_tests.rs"]
mod tests;
