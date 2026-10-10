//! Prices keyed by the interval they apply to: what every backend returns,
//! what the journal records, and what the dashboard and `analyze` read.

use serde::{Deserialize, Serialize};

use crate::units::{CentsPerKwh, Timestamp};

/// A wholesale price over the interval it applies to. Carries its own end so
/// hourly and quarter-hourly feeds share one type.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PricePoint {
    pub from: Timestamp,
    pub until: Timestamp,
    pub wholesale: CentsPerKwh,
}

/// Prices keyed by the start of their interval.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PriceSeries(std::collections::BTreeMap<Timestamp, PricePoint>);

impl PriceSeries {
    /// A point with the same `from` replaces the old one: a re-fetched feed
    /// carries the newer value.
    pub fn insert(&mut self, point: PricePoint) {
        self.0.insert(point.from, point);
    }

    /// In order of `from`.
    pub fn iter(&self) -> impl Iterator<Item = &PricePoint> {
        self.0.values()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Drops every point whose interval is over by `cutoff`.
    pub fn drop_ending_before(&mut self, cutoff: Timestamp) {
        self.0.retain(|_, point| point.until > cutoff);
    }

    /// The point whose interval contains `ts`, `until` being exclusive so
    /// adjacent intervals never both match. `None` in a gap.
    pub fn at(&self, ts: Timestamp) -> Option<&PricePoint> {
        self.0
            .range(..=ts)
            .next_back()
            .map(|(_, point)| point)
            .filter(|point| ts < point.until)
    }
}

#[cfg(test)]
#[path = "series_tests.rs"]
mod tests;
