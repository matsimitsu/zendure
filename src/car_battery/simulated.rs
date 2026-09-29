//! A synthetic backend: a fixed SoC with a slow, bounded drift, no network
//! call and no VW account needed. `[car_battery] kind = "simulated"` selects
//! it — mirrors `prediction::simulated`'s role, letting the poller/dashboard
//! pipeline be exercised locally with no credentials.

use super::{CarBatteryError, CarBatterySource};
use crate::units::Soc;

pub struct SimulatedCarBattery {
    percent: u32,
    rising: bool,
}

impl SimulatedCarBattery {
    pub fn new() -> Self {
        SimulatedCarBattery {
            percent: 62,
            rising: true,
        }
    }
}

impl Default for SimulatedCarBattery {
    fn default() -> Self {
        Self::new()
    }
}

impl CarBatterySource for SimulatedCarBattery {
    async fn poll(&mut self) -> Result<Soc, CarBatteryError> {
        // Drifts one point per poll between 55% and 85%, reversing at each
        // end — enough movement to see the dashboard tile actually update,
        // not a claim about any real charge/discharge rate.
        if self.rising {
            self.percent += 1;
            if self.percent >= 85 {
                self.rising = false;
            }
        } else {
            self.percent -= 1;
            if self.percent <= 55 {
                self.rising = true;
            }
        }
        Ok(Soc::new(self.percent))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn it_drifts_between_bounds_and_reverses() {
        let mut battery = SimulatedCarBattery::new();
        let mut values = Vec::new();
        for _ in 0..60 {
            values.push(battery.poll().await.unwrap().get());
        }
        assert!(values.iter().all(|v| (55..=85).contains(v)));
        assert!(values.contains(&85), "never reached the top bound");
        assert!(values.contains(&55), "never reached the bottom bound");
    }
}
