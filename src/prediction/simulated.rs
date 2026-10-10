//! A synthetic backend: a deterministic clear-sky-shaped curve, no network
//! call and no daily quota to spend. `[prediction] kind = "simulated"`
//! selects it — mirrors `simulation::VirtualBattery`'s role on the device
//! side, letting the scheduler/persistence/dashboard pipeline be exercised
//! locally without a real Solcast API key.

use chrono::{DateTime, Timelike, Utc};
use chrono_tz::Tz;

use super::{FORECAST_PERIOD, Prediction};
use crate::clock::{local_date, local_day_start};
use crate::fetch::FetchError;
use crate::units::{Elapsed, SolarForecastPoint, SolarPower, Timestamp, Watts};

/// A rooftop array's rough peak output — enough to draw a plausible curve,
/// not a claim about any real installation.
const PEAK: Watts = Watts(4000);

pub struct SimulatedForecaster {
    timezone: Tz,
}

impl SimulatedForecaster {
    pub fn new(timezone: Tz) -> Self {
        SimulatedForecaster { timezone }
    }
}

impl Prediction for SimulatedForecaster {
    async fn forecast(&self) -> Result<Vec<SolarForecastPoint>, FetchError> {
        Ok(clear_sky_curve(Utc::now(), self.timezone, PEAK))
    }
}

/// Zero outside 06:00-20:00 local, a cosine bell peaking at 13:00, sampled
/// every 30 minutes from today's local midnight to the end of tomorrow, as a
/// real 48-hour fetch would cover. Not a real sun position.
fn clear_sky_curve(now: DateTime<Utc>, tz: Tz, peak: Watts) -> Vec<SolarForecastPoint> {
    let bounds = local_date(Timestamp::from(now), tz).and_then(|today| {
        let after_tomorrow = today.succ_opt()?.succ_opt()?;
        Some((
            local_day_start(today, tz)?,
            local_day_start(after_tomorrow, tz)?,
        ))
    });
    let Some((start, end)) = bounds else {
        return Vec::new();
    };
    std::iter::successors(Some(start), |&at| Some(at + Elapsed::of(FORECAST_PERIOD)))
        .take_while(|&at| at < end)
        .filter_map(|at| {
            let local = DateTime::from_timestamp_millis(at.as_millis())?.with_timezone(&tz);
            let hour = f64::from(local.hour()) + f64::from(local.minute()) / 60.0;
            let watts = if (6.0..=20.0).contains(&hour) {
                let fraction = ((hour - 13.0) / 7.0 * std::f64::consts::FRAC_PI_2)
                    .cos()
                    .max(0.0);
                peak.as_f64() * fraction
            } else {
                0.0
            };
            Some(SolarForecastPoint {
                at,
                estimate: SolarPower::new(watts),
            })
        })
        .collect()
}

#[cfg(test)]
#[path = "simulated_tests.rs"]
mod tests;
