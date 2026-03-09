use std::time::Instant;

use crate::models::MeterReading;

/// Provides signed net grid power from meter data.
/// Positive = importing from grid, negative = exporting to grid.
///
/// Returns `Some(power)` when a new estimate is available (kWh tick detected),
/// `None` when no new data (caller should skip decision-making and let the
/// battery hold its current setting).
///
/// Implementations:
/// - `KwhDeltaEstimator`: Computes net power from kWh register deltas.
/// - Future: Shelly 3EM would return `Some` on every reading (signed per-phase data).
pub trait GridPowerEstimator {
    fn update(&mut self, meter: &MeterReading, solar_power: f64) -> Option<f64>;
}

/// Computes signed net grid power from the DSMR consumption/production kWh registers.
///
/// The DSMR smart meter accumulates energy across all three phases:
/// - `consumption_total_kwh` increases when net importing from grid
/// - `production_total_kwh` increases when net exporting to grid
///
/// By comparing the deltas of both registers since the last tick, we get both
/// direction and magnitude of net grid power — without needing to know individual
/// phase directions.
///
/// Between kWh ticks (0.001 kWh / 1 Wh resolution), returns `None` so the
/// controller skips decision-making and the battery holds its current setting.
/// This prevents stale data from causing cumulative drift. At typical household
/// power levels, ticks arrive every few seconds (high power) to about a minute
/// (low power near net-zero — which means we're already on target).
pub struct KwhDeltaEstimator {
    /// kWh register baseline from which we accumulate deltas.
    /// Only reset when a tick is detected, so deltas naturally accumulate
    /// until there's enough resolution to compute meaningful power.
    base_import_kwh: Option<f64>,
    base_export_kwh: Option<f64>,
    base_time: Option<Instant>,
}

impl KwhDeltaEstimator {
    pub fn new() -> Self {
        Self {
            base_import_kwh: None,
            base_export_kwh: None,
            base_time: None,
        }
    }
}

impl GridPowerEstimator for KwhDeltaEstimator {
    fn update(&mut self, meter: &MeterReading, _solar_power: f64) -> Option<f64> {
        let now = Instant::now();

        let Some(base_import) = self.base_import_kwh else {
            // First reading: establish baseline, no power estimate yet.
            self.base_import_kwh = Some(meter.consumption_total_kwh);
            self.base_export_kwh = Some(meter.production_total_kwh);
            self.base_time = Some(now);
            return None;
        };

        let base_export = self.base_export_kwh.unwrap();
        let base_time = self.base_time.unwrap();

        let import_delta = meter.consumption_total_kwh - base_import;
        let export_delta = meter.production_total_kwh - base_export;

        // Guard against meter resets or bad data.
        if import_delta < 0.0 || export_delta < 0.0 {
            self.base_import_kwh = Some(meter.consumption_total_kwh);
            self.base_export_kwh = Some(meter.production_total_kwh);
            self.base_time = Some(now);
            return None;
        }

        let elapsed_secs = base_time.elapsed().as_secs_f64();

        // 0.001 kWh = 1 Wh tick. Use half as threshold for float comparison.
        const KWH_TICK: f64 = 0.0005;

        if elapsed_secs > 0.0 && (import_delta > KWH_TICK || export_delta > KWH_TICK) {
            let elapsed_hours = elapsed_secs / 3600.0;
            // Positive = net importing, negative = net exporting.
            let net_grid_power = (import_delta - export_delta) / elapsed_hours * 1000.0;

            // Reset baseline for next measurement window.
            self.base_import_kwh = Some(meter.consumption_total_kwh);
            self.base_export_kwh = Some(meter.production_total_kwh);
            self.base_time = Some(now);

            return Some(net_grid_power);
        }

        // No tick yet — caller should skip decision-making.
        None
    }
}

#[cfg(test)]
impl KwhDeltaEstimator {
    /// Wind the baseline clock back so the next update sees elapsed time.
    pub(crate) fn wind_back(&mut self, secs: u64) {
        self.base_time = Some(Instant::now() - std::time::Duration::from_secs(secs));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meter(import_kwh: f64, export_kwh: f64) -> MeterReading {
        MeterReading {
            device_id: "test".to_string(),
            consumption_total_kwh: import_kwh,
            consumption_t1_kwh: 0.0,
            consumption_t2_kwh: import_kwh,
            production_total_kwh: export_kwh,
            production_t1_kwh: 0.0,
            production_t2_kwh: export_kwh,
            phase1_voltage: 230.0,
            phase2_voltage: 230.0,
            phase3_voltage: 230.0,
            phase1_current: 0.0,
            phase2_current: 0.0,
            phase3_current: 0.0,
            frequency: 50.0,
            phase1_pf: 1.0,
            phase2_pf: 1.0,
            phase3_pf: 1.0,
            phase1_power: 0.0,
            phase2_power: 0.0,
            phase3_power: 0.0,
            total_power: 0.0,
            timestamp: "2024-01-01T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn first_reading_returns_none() {
        let mut est = KwhDeltaEstimator::new();
        assert!(est.update(&meter(500.0, 300.0), 0.0).is_none());
    }

    #[test]
    fn import_tick_gives_positive_power() {
        let mut est = KwhDeltaEstimator::new();
        est.update(&meter(500.0, 300.0), 0.0);
        est.wind_back(10);

        // 0.001 kWh imported in 10 seconds = 360W
        let net = est.update(&meter(500.001, 300.0), 0.0).unwrap();
        assert!((net - 360.0).abs() < 1.0);
    }

    #[test]
    fn export_tick_gives_negative_power() {
        let mut est = KwhDeltaEstimator::new();
        est.update(&meter(500.0, 300.0), 0.0);
        est.wind_back(10);

        // 0.001 kWh exported in 10 seconds = -360W
        let net = est.update(&meter(500.0, 300.001), 0.0).unwrap();
        assert!((net - (-360.0)).abs() < 1.0);
    }

    #[test]
    fn both_tick_gives_net_power() {
        let mut est = KwhDeltaEstimator::new();
        est.update(&meter(500.0, 300.0), 0.0);
        est.wind_back(10);

        // 0.003 kWh imported, 0.001 kWh exported in 10s
        // Net = (0.003 - 0.001) / (10/3600) * 1000 = 720W importing
        let net = est.update(&meter(500.003, 300.001), 0.0).unwrap();
        assert!((net - 720.0).abs() < 1.0);
    }

    #[test]
    fn no_tick_returns_none() {
        let mut est = KwhDeltaEstimator::new();
        est.update(&meter(500.0, 300.0), 0.0);
        est.wind_back(10);

        // Same kWh values → no tick → None
        assert!(est.update(&meter(500.0, 300.0), 0.0).is_none());
    }

    #[test]
    fn direction_change_detected() {
        let mut est = KwhDeltaEstimator::new();
        est.update(&meter(500.0, 300.0), 0.0);
        est.wind_back(10);

        // Importing 360W
        est.update(&meter(500.001, 300.0), 0.0);
        est.wind_back(10);

        // Now exporting 360W
        let net = est.update(&meter(500.001, 300.001), 0.0).unwrap();
        assert!((net - (-360.0)).abs() < 1.0);
    }

    #[test]
    fn longer_window_gives_accurate_low_power() {
        let mut est = KwhDeltaEstimator::new();
        est.update(&meter(500.0, 300.0), 0.0);
        est.wind_back(72);

        // 0.001 kWh in 72 seconds = 50W
        let net = est.update(&meter(500.001, 300.0), 0.0).unwrap();
        assert!((net - 50.0).abs() < 1.0);
    }

    #[test]
    fn meter_reset_returns_none() {
        let mut est = KwhDeltaEstimator::new();
        est.update(&meter(500.0, 300.0), 0.0);
        est.wind_back(10);

        // Meter reset: kWh values drop → None (resets baseline)
        assert!(est.update(&meter(0.0, 0.0), 0.0).is_none());
    }

    #[test]
    fn near_zero_net_from_equal_ticks() {
        let mut est = KwhDeltaEstimator::new();
        est.update(&meter(500.0, 300.0), 0.0);
        est.wind_back(10);

        // Equal import and export → net zero
        let net = est.update(&meter(500.001, 300.001), 0.0).unwrap();
        assert!(net.abs() < 1.0);
    }

    #[test]
    fn baseline_resets_after_tick() {
        let mut est = KwhDeltaEstimator::new();
        est.update(&meter(500.0, 300.0), 0.0);
        est.wind_back(10);

        // First tick: 360W import
        est.update(&meter(500.001, 300.0), 0.0);
        est.wind_back(20);

        // Second tick from new baseline: 0.002 kWh in 20s = 360W
        let net = est.update(&meter(500.003, 300.0), 0.0).unwrap();
        assert!((net - 360.0).abs() < 1.0);
    }
}
