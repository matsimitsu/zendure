//! Electricity prices for the dashboard and offline analysis.
//!
//! `PriceSource` is the capability, shaped like `crate::prediction::Prediction`:
//! one trait, several backends, dispatched through an enum because the trait's
//! `impl Future + Send` return isn't `dyn`-safe. Everything past this seam only
//! sees `Vec<PricePoint>`, never which backend produced them.
//!
//! Display and analysis only: nothing here feeds `crate::controller`.

#![allow(dead_code)]

use std::future::Future;

use crate::units::{PricePoint, Timestamp};

pub mod simulated;

/// What can go wrong fetching prices. A response this build cannot decode is
/// the one most worth keeping, so the raw body rides along on a parse failure.
#[derive(Debug)]
pub enum PriceError {
    Request(String),
    Parse { body: String, error: String },
}

impl std::fmt::Display for PriceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PriceError::Request(e) => write!(f, "request failed: {e}"),
            PriceError::Parse { error, .. } => write!(f, "parse error: {error}"),
        }
    }
}

/// Wholesale prices for the half-open range `[from, until)`. A backend may
/// return points reaching outside the range; callers key them by interval in a
/// `PriceSeries`, which tolerates that.
pub trait PriceSource {
    fn prices(
        &self,
        from: Timestamp,
        until: Timestamp,
    ) -> impl Future<Output = Result<Vec<PricePoint>, PriceError>> + Send;
}

/// Whichever backend `[prices]` selected.
pub enum PriceFeed {
    Simulated(simulated::SimulatedPrices),
}

impl PriceSource for PriceFeed {
    async fn prices(
        &self,
        from: Timestamp,
        until: Timestamp,
    ) -> Result<Vec<PricePoint>, PriceError> {
        match self {
            PriceFeed::Simulated(s) => s.prices(from, until).await,
        }
    }
}
