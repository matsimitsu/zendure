use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

/// Persisted energy sample: (unix timestamp, charge_wh, discharge_wh)
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedSample {
    ts: f64,
    charge_wh: f64,
    discharge_wh: f64,
}

/// On-disk format for the RTE state file.
#[derive(Debug, Serialize, Deserialize)]
struct PersistedState {
    samples: Vec<PersistedSample>,
    last_charge_power: f64,
    last_discharge_power: f64,
    last_sample_ts: Option<f64>,
}

/// In-memory energy sample with monotonic timestamp for window pruning.
struct Sample {
    instant: Instant,
    unix_ts: f64,
    charge_wh: f64,
    discharge_wh: f64,
}

/// Tracks battery round-trip efficiency over a rolling 24-hour window
/// by integrating charge and discharge power over time.
pub struct RteTracker {
    samples: VecDeque<Sample>,
    last_sample_time: Option<Instant>,
    last_sample_unix: Option<f64>,
    last_charge_power: f64,
    last_discharge_power: f64,
    window: Duration,
    state_path: PathBuf,
}

impl RteTracker {
    pub fn new(state_path: PathBuf) -> Self {
        let mut tracker = Self {
            samples: VecDeque::new(),
            last_sample_time: None,
            last_sample_unix: None,
            last_charge_power: 0.0,
            last_discharge_power: 0.0,
            window: Duration::from_secs(24 * 3600),
            state_path,
        };
        tracker.load();
        tracker
    }

    /// Record a power sample. Call this on every battery poll.
    /// `charge_w` = power flowing into battery (W), `discharge_w` = power flowing out (W).
    pub fn record(&mut self, charge_w: f64, discharge_w: f64) {
        self.record_at(Instant::now(), charge_w, discharge_w);
    }

    fn record_at(&mut self, now: Instant, charge_w: f64, discharge_w: f64) {
        let unix_now = unix_now();

        if let Some(last_time) = self.last_sample_time {
            let dt_hours = now.duration_since(last_time).as_secs_f64() / 3600.0;
            // Trapezoidal integration
            let charge_wh = (self.last_charge_power + charge_w) / 2.0 * dt_hours;
            let discharge_wh = (self.last_discharge_power + discharge_w) / 2.0 * dt_hours;
            self.samples.push_back(Sample {
                instant: now,
                unix_ts: unix_now,
                charge_wh,
                discharge_wh,
            });
        }

        self.last_sample_time = Some(now);
        self.last_sample_unix = Some(unix_now);
        self.last_charge_power = charge_w;
        self.last_discharge_power = discharge_w;

        self.prune(now);
    }

    /// Remove samples older than the rolling window.
    fn prune(&mut self, now: Instant) {
        let cutoff = now - self.window;
        while self.samples.front().is_some_and(|s| s.instant < cutoff) {
            self.samples.pop_front();
        }
    }

    /// Total energy charged in the rolling window (Wh).
    pub fn total_charge_wh(&self) -> f64 {
        self.samples.iter().map(|s| s.charge_wh).sum()
    }

    /// Total energy discharged in the rolling window (Wh).
    pub fn total_discharge_wh(&self) -> f64 {
        self.samples.iter().map(|s| s.discharge_wh).sum()
    }

    /// Round-trip efficiency percentage (0–100), or None if insufficient data.
    pub fn rte_percent(&self) -> Option<f64> {
        let charged = self.total_charge_wh();
        if charged < 1.0 {
            return None; // Need at least 1 Wh of charge data
        }
        let discharged = self.total_discharge_wh();
        let rte = (discharged / charged) * 100.0;

        // When RTE drops below 70%, use geometric mean fallback to smooth out
        // poor efficiency readings (per Zendure-HA-zenSDK approach).
        if rte < 70.0 {
            Some((rte / 100.0).sqrt() * 100.0)
        } else {
            Some(rte)
        }
    }

    /// Estimate usable energy (kWh) that can be recovered from the battery.
    ///
    /// - `soc`: current state of charge (0–100%)
    /// - `min_soc`: minimum allowed SOC (0–100%)
    /// - `pack_capacities_wh`: capacity of each connected pack in Wh
    pub fn usable_kwh(&self, soc: u32, min_soc: u32, pack_capacities_wh: &[f64]) -> f64 {
        let total_capacity_wh: f64 = pack_capacities_wh.iter().sum();
        if total_capacity_wh <= 0.0 || soc <= min_soc {
            return 0.0;
        }

        let usable_soc_fraction = (soc - min_soc) as f64 / 100.0;
        let rte_factor = self.rte_percent().unwrap_or(85.0) / 100.0;

        total_capacity_wh * usable_soc_fraction * rte_factor / 1000.0
    }

    /// Persist current state to disk.
    pub fn save(&self) {
        let state = PersistedState {
            samples: self
                .samples
                .iter()
                .map(|s| PersistedSample {
                    ts: s.unix_ts,
                    charge_wh: s.charge_wh,
                    discharge_wh: s.discharge_wh,
                })
                .collect(),
            last_charge_power: self.last_charge_power,
            last_discharge_power: self.last_discharge_power,
            last_sample_ts: self.last_sample_unix,
        };

        match serde_json::to_string(&state) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&self.state_path, json) {
                    tracing::warn!("Failed to persist RTE state: {e}");
                }
            }
            Err(e) => tracing::warn!("Failed to serialize RTE state: {e}"),
        }
    }

    /// Load persisted state from disk, discarding samples outside the 24h window.
    fn load(&mut self) {
        let data = match std::fs::read_to_string(&self.state_path) {
            Ok(d) => d,
            Err(_) => return, // No state file yet
        };

        let state: PersistedState = match serde_json::from_str(&data) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("Failed to parse RTE state file: {e}");
                return;
            }
        };

        let now_unix = unix_now();
        let now_instant = Instant::now();
        let window_secs = self.window.as_secs_f64();

        for s in state.samples {
            let age_secs = now_unix - s.ts;
            if age_secs < 0.0 || age_secs > window_secs {
                continue; // Skip samples outside the window
            }
            self.samples.push_back(Sample {
                instant: now_instant - Duration::from_secs_f64(age_secs),
                unix_ts: s.ts,
                charge_wh: s.charge_wh,
                discharge_wh: s.discharge_wh,
            });
        }

        self.last_charge_power = state.last_charge_power;
        self.last_discharge_power = state.last_discharge_power;

        // Restore last_sample_time relative to now, but only if it's recent enough
        if let Some(last_ts) = state.last_sample_ts {
            let age = now_unix - last_ts;
            if age >= 0.0 && age < window_secs {
                self.last_sample_time = Some(now_instant - Duration::from_secs_f64(age));
                self.last_sample_unix = Some(last_ts);
            }
        }

        let count = self.samples.len();
        if count > 0 {
            tracing::info!(
                "Restored {count} RTE samples, charge={:.0}Wh discharge={:.0}Wh",
                self.total_charge_wh(),
                self.total_discharge_wh(),
            );
        }
    }
}

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

/// Map a Zendure `pack_type` to its nominal capacity in Wh.
pub fn pack_type_capacity_wh(pack_type: u32) -> f64 {
    match pack_type {
        // AB1000 / AB1000S
        500 => 960.0,
        // AB2000 / AB2000S
        501 => 1920.0,
        // Unknown — assume AB2000 as conservative default
        _ => {
            tracing::warn!("Unknown pack_type {pack_type}, assuming 1920 Wh");
            1920.0
        }
    }
}

/// Extract per-pack capacities from ZendureReport pack_data.
pub fn pack_capacities(pack_data: &Option<Vec<crate::models::PackData>>) -> Vec<f64> {
    match pack_data {
        Some(packs) => packs
            .iter()
            .map(|p| pack_type_capacity_wh(p.pack_type.unwrap_or(501)))
            .collect(),
        None => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn temp_path() -> PathBuf {
        let f = NamedTempFile::new().unwrap();
        f.path().to_path_buf()
    }

    #[test]
    fn test_no_data_returns_none() {
        let tracker = RteTracker::new(temp_path());
        assert!(tracker.rte_percent().is_none());
    }

    #[test]
    fn test_rte_calculation() {
        let path = temp_path();
        let mut tracker = RteTracker::new(path);
        let t0 = Instant::now();

        // Simulate 1 hour of charging at 1000W
        tracker.record_at(t0, 1000.0, 0.0);
        tracker.record_at(t0 + Duration::from_secs(3600), 1000.0, 0.0);

        // Simulate 1 hour of discharging at 850W (85% efficiency)
        tracker.record_at(t0 + Duration::from_secs(3600), 0.0, 850.0);
        tracker.record_at(t0 + Duration::from_secs(7200), 0.0, 850.0);

        let rte = tracker.rte_percent().unwrap();
        assert!((rte - 85.0).abs() < 1.0, "Expected ~85% RTE, got {rte}");
    }

    #[test]
    fn test_rte_geometric_mean_fallback() {
        let path = temp_path();
        let mut tracker = RteTracker::new(path);
        let t0 = Instant::now();

        // Simulate 1 hour of charging at 1000W
        tracker.record_at(t0, 1000.0, 0.0);
        tracker.record_at(t0 + Duration::from_secs(3600), 1000.0, 0.0);

        // Simulate 1 hour of discharging at 500W (50% raw efficiency → below 70%)
        tracker.record_at(t0 + Duration::from_secs(3600), 0.0, 500.0);
        tracker.record_at(t0 + Duration::from_secs(7200), 0.0, 500.0);

        let rte = tracker.rte_percent().unwrap();
        // Raw 50% → sqrt(0.5) * 100 ≈ 70.7
        assert!(
            (rte - 70.7).abs() < 1.0,
            "Expected ~70.7% RTE with fallback, got {rte}"
        );
    }

    #[test]
    fn test_usable_kwh() {
        let path = temp_path();
        let tracker = RteTracker::new(path);
        // No RTE data → uses 85% default
        // 2 packs of 1920 Wh = 3840 Wh, SOC=80%, min=10% → 70% usable
        // 3840 * 0.70 * 0.85 / 1000 = 2.2848
        let usable = tracker.usable_kwh(80, 10, &[1920.0, 1920.0]);
        assert!(
            (usable - 2.285).abs() < 0.1,
            "Expected ~2.28 kWh, got {usable}"
        );
    }

    #[test]
    fn test_usable_kwh_at_min_soc() {
        let tracker = RteTracker::new(temp_path());
        assert_eq!(tracker.usable_kwh(10, 10, &[1920.0]), 0.0);
    }

    #[test]
    fn test_persistence_roundtrip() {
        let path = temp_path();
        let t0 = Instant::now();

        // Create tracker, add data, save
        {
            let mut tracker = RteTracker::new(path.clone());
            tracker.record_at(t0, 1000.0, 0.0);
            tracker.record_at(t0 + Duration::from_secs(3600), 0.0, 850.0);
            tracker.save();
        }

        // Load into new tracker — samples should be restored
        let tracker2 = RteTracker::new(path);
        assert!(tracker2.total_charge_wh() > 0.0 || tracker2.total_discharge_wh() > 0.0);
    }

    #[test]
    fn test_corrupt_state_file_handled() {
        let path = temp_path();
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(b"not valid json").unwrap();

        // Should not panic, just log a warning and start fresh
        let tracker = RteTracker::new(path);
        assert!(tracker.rte_percent().is_none());
    }

    #[test]
    fn test_pack_type_capacity() {
        assert_eq!(pack_type_capacity_wh(500), 960.0);
        assert_eq!(pack_type_capacity_wh(501), 1920.0);
    }
}
