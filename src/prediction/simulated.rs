//! A synthetic backend: a deterministic clear-sky-shaped curve, no network
//! call and no daily quota to spend. `[prediction] kind = "simulated"`
//! selects it — mirrors `simulation::VirtualBattery`'s role on the device
//! side, letting the scheduler/persistence/dashboard pipeline be exercised
//! locally without a real Solcast API key.

use super::{ForecastError, Prediction};
use crate::units::{SolarForecastPoint, SolarPower, Timestamp, Watts};

/// A rooftop array's rough peak output — enough to draw a plausible curve,
/// not a claim about any real installation.
const PEAK: Watts = Watts(4000);

pub struct SimulatedForecaster;

impl SimulatedForecaster {
    pub fn new() -> Self {
        SimulatedForecaster
    }
}

impl Default for SimulatedForecaster {
    fn default() -> Self {
        Self::new()
    }
}

impl Prediction for SimulatedForecaster {
    async fn forecast(&self) -> Result<Vec<SolarForecastPoint>, ForecastError> {
        Ok(clear_sky_curve(chrono::Utc::now(), PEAK))
    }
}

/// Zero outside 06:00-20:00, a cosine bell peaking at 13:00, sampled every 30
/// minutes across the 24 hours starting at `from`. Not tied to the configured
/// timezone or a real sun position — this backend exists to exercise the
/// scheduler/dashboard pipeline, not to model a particular installation.
fn clear_sky_curve(from: chrono::DateTime<chrono::Utc>, peak: Watts) -> Vec<SolarForecastPoint> {
    use chrono::Timelike;

    (0..48)
        .map(|i| {
            let at = from + chrono::Duration::minutes(i * 30);
            let hour = at.hour() as f64 + at.minute() as f64 / 60.0;
            let watts = if (6.0..=20.0).contains(&hour) {
                let fraction = ((hour - 13.0) / 7.0 * std::f64::consts::FRAC_PI_2)
                    .cos()
                    .max(0.0);
                peak.as_f64() * fraction
            } else {
                0.0
            };
            SolarForecastPoint {
                at: Timestamp::from(at),
                estimate: SolarPower::new(watts),
            }
        })
        .collect()
}

#[cfg(test)]
#[path = "simulated_tests.rs"]
mod tests;
