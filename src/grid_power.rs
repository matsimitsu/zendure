use crate::models::MeterReading;

/// Provides signed net grid power from meter data.
/// Positive = importing from grid, negative = exporting to grid.
///
/// Implementations:
/// - `KwhDeltaEstimator`: Uses unsigned meter readings + kWh register deltas for direction.
/// - Future: Shelly 3EM or DSMR P1 meter would provide signed power directly.
pub trait GridPowerEstimator {
    fn update(&mut self, meter: &MeterReading, solar_power: f64) -> f64;
}

/// Estimates signed grid power from an unsigned meter (no directional readings).
///
/// Direction detection strategy:
/// 1. If solar power >= phase 1 meter reading → phase 1 is definitely exporting
///    (solar alone exceeds what the meter sees, so current must flow grid-ward).
/// 2. Otherwise, use kWh register deltas: whichever cumulative register (import or export)
///    increased since the last reading tells us the net direction.
/// 3. If no kWh tick observed yet, default to importing.
///
/// Phase 2 & 3 are always importing (no solar or battery on those phases).
pub struct KwhDeltaEstimator {
    prev_import_kwh: Option<f64>,
    prev_export_kwh: Option<f64>,
    /// Whether phase 1 is currently exporting, as determined by kWh register deltas.
    phase1_exporting: Option<bool>,
}

impl KwhDeltaEstimator {
    pub fn new() -> Self {
        Self {
            prev_import_kwh: None,
            prev_export_kwh: None,
            phase1_exporting: None,
        }
    }

    fn update_direction(&mut self, meter: &MeterReading) {
        let Some(prev_import) = self.prev_import_kwh else {
            self.prev_import_kwh = Some(meter.consumption_total_kwh);
            self.prev_export_kwh = Some(meter.production_total_kwh);
            return;
        };
        let prev_export = self.prev_export_kwh.unwrap();

        let import_delta = meter.consumption_total_kwh - prev_import;
        let export_delta = meter.production_total_kwh - prev_export;

        // kWh registers have 0.001 kWh (1 Wh) resolution. Use a small epsilon
        // to ignore float rounding noise.
        const KWH_EPSILON: f64 = 0.0005;

        if export_delta > KWH_EPSILON && export_delta > import_delta {
            self.phase1_exporting = Some(true);
        } else if import_delta > KWH_EPSILON && import_delta > export_delta {
            self.phase1_exporting = Some(false);
        }
        // If neither ticked, keep last known direction.

        self.prev_import_kwh = Some(meter.consumption_total_kwh);
        self.prev_export_kwh = Some(meter.production_total_kwh);
    }
}

impl GridPowerEstimator for KwhDeltaEstimator {
    fn update(&mut self, meter: &MeterReading, solar_power: f64) -> f64 {
        self.update_direction(meter);

        // Phase 1 direction:
        // - Solar heuristic is authoritative when solar clearly exceeds meter reading
        // - kWh delta handles discharge periods when solar is low/zero
        // - Default to importing if no direction established yet
        let phase1_exporting =
            solar_power >= meter.phase1_power || self.phase1_exporting == Some(true);

        let net_p1 = if phase1_exporting {
            -meter.phase1_power
        } else {
            meter.phase1_power
        };

        // Phase 2 & 3 have no solar or battery — always importing
        net_p1 + meter.phase2_power + meter.phase3_power
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meter(
        phase1_power: f64,
        phase2_power: f64,
        phase3_power: f64,
        import_kwh: f64,
        export_kwh: f64,
    ) -> MeterReading {
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
            phase1_current: phase1_power / 230.0,
            phase2_current: phase2_power / 230.0,
            phase3_current: phase3_power / 230.0,
            frequency: 50.0,
            phase1_pf: 1.0,
            phase2_pf: 1.0,
            phase3_pf: 1.0,
            phase1_power,
            phase2_power,
            phase3_power,
            total_power: phase1_power + phase2_power + phase3_power,
            timestamp: "2024-01-01T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn solar_heuristic_when_solar_exceeds_phase1() {
        let mut est = KwhDeltaEstimator::new();
        let m = meter(500.0, 100.0, 50.0, 100.0, 200.0);
        // Solar 800W > phase1 500W → exporting on phase 1
        let net = est.update(&m, 800.0);
        // net = -500 + 100 + 50 = -350 (exporting 350W)
        assert!((net - (-350.0)).abs() < 0.1);
    }

    #[test]
    fn defaults_to_importing_without_kwh_data() {
        let mut est = KwhDeltaEstimator::new();
        let m = meter(200.0, 100.0, 50.0, 100.0, 200.0);
        // Solar 0W, no kWh history → default to importing
        let net = est.update(&m, 0.0);
        // net = +200 + 100 + 50 = 350 (importing 350W)
        assert!((net - 350.0).abs() < 0.1);
    }

    #[test]
    fn kwh_delta_detects_export_during_discharge() {
        let mut est = KwhDeltaEstimator::new();

        // First reading: establish baseline (no solar, battery not discharging yet)
        let m1 = meter(100.0, 80.0, 50.0, 500.0, 300.0);
        let net1 = est.update(&m1, 0.0);
        assert!((net1 - 230.0).abs() < 0.1); // All importing

        // Second reading: battery discharging, export register ticked
        // Phase 1 shows 100W (battery output - house consumption)
        let m2 = meter(100.0, 80.0, 50.0, 500.0, 300.002);
        let net2 = est.update(&m2, 0.0);
        // kWh delta detected export → phase 1 exporting
        // net = -100 + 80 + 50 = 30 (still net importing due to phase 2+3)
        assert!((net2 - 30.0).abs() < 0.1);
    }

    #[test]
    fn kwh_delta_detects_import_after_discharge_stops() {
        let mut est = KwhDeltaEstimator::new();

        // Establish baseline
        let m1 = meter(100.0, 80.0, 50.0, 500.0, 300.0);
        est.update(&m1, 0.0);

        // Export detected (battery discharging)
        let m2 = meter(100.0, 80.0, 50.0, 500.0, 300.002);
        est.update(&m2, 0.0);

        // Battery stops, import register ticks
        let m3 = meter(100.0, 80.0, 50.0, 500.002, 300.002);
        let net3 = est.update(&m3, 0.0);
        // Back to importing on phase 1
        // net = +100 + 80 + 50 = 230
        assert!((net3 - 230.0).abs() < 0.1);
    }

    #[test]
    fn direction_persists_between_kwh_ticks() {
        let mut est = KwhDeltaEstimator::new();

        // Baseline
        let m1 = meter(100.0, 80.0, 50.0, 500.0, 300.0);
        est.update(&m1, 0.0);

        // Export tick
        let m2 = meter(100.0, 80.0, 50.0, 500.0, 300.001);
        est.update(&m2, 0.0);

        // No tick — direction should persist as exporting
        let m3 = meter(120.0, 80.0, 50.0, 500.0, 300.001);
        let net3 = est.update(&m3, 0.0);
        // Still exporting on phase 1
        // net = -120 + 80 + 50 = 10
        assert!((net3 - 10.0).abs() < 0.1);
    }
}
