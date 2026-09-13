use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

use crate::units::{KiloWattHours, Percent, Soc, WattHours, Watts};

/// Persisted energy sample: (unix timestamp, charge_wh, discharge_wh)
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedSample {
    ts: f64,
    charge_wh: WattHours,
    discharge_wh: WattHours,
}

/// On-disk format for the RTE state file.
#[derive(Debug, Serialize, Deserialize)]
struct PersistedState {
    samples: Vec<PersistedSample>,
    // Bare `f64` rather than `Watts`: `Watts` is `i32`-backed, so serializing
    // it would write `100` where the existing file on disk has `100.0`, and
    // parsing that same `100.0` back into an `i32` newtype fails outright.
    // Converted at the boundary instead — see `save`/`load` below.
    last_charge_power: f64,
    // See `last_charge_power` above — same reasoning applies here.
    last_discharge_power: f64,
    last_sample_ts: Option<f64>,
}

/// In-memory energy sample with monotonic timestamp for window pruning.
struct Sample {
    instant: Instant,
    unix_ts: f64,
    charge_wh: WattHours,
    discharge_wh: WattHours,
}

/// Tracks battery round-trip efficiency over a rolling 24-hour window
/// by integrating charge and discharge power over time.
pub struct RteTracker {
    samples: VecDeque<Sample>,
    last_sample_time: Option<Instant>,
    last_sample_unix: Option<f64>,
    last_charge_power: Watts,
    last_discharge_power: Watts,
    window: Duration,
    state_path: PathBuf,
    /// `save` runs on every poll, so a persistently unwritable path would log
    /// once per poll forever. Warn on the first failure, stay quiet after.
    save_failed: AtomicBool,
}

impl RteTracker {
    pub fn new(state_path: PathBuf) -> Self {
        let mut tracker = Self {
            samples: VecDeque::new(),
            last_sample_time: None,
            last_sample_unix: None,
            last_charge_power: Watts::ZERO,
            last_discharge_power: Watts::ZERO,
            window: Duration::from_secs(24 * 3600),
            state_path,
            save_failed: AtomicBool::new(false),
        };
        // The default lives under /var/lib, which the service user owns via
        // systemd's StateDirectory but which won't exist in a dev checkout.
        if let Some(parent) = tracker.state_path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            tracing::warn!("RTE state directory {} unusable: {e}", parent.display());
        }
        tracker.load();
        tracker
    }

    /// Record a power sample. Call this on every battery poll.
    /// `charge` = power flowing into battery, `discharge` = power flowing out.
    pub fn record(&mut self, charge: Watts, discharge: Watts) {
        self.record_at(Instant::now(), charge, discharge);
    }

    fn record_at(&mut self, now: Instant, charge: Watts, discharge: Watts) {
        let unix_now = unix_now();

        if let Some(last_time) = self.last_sample_time {
            let dt = now.duration_since(last_time);
            let charge_wh = WattHours::integrate(self.last_charge_power, charge, dt);
            let discharge_wh = WattHours::integrate(self.last_discharge_power, discharge, dt);
            self.samples.push_back(Sample {
                instant: now,
                unix_ts: unix_now,
                charge_wh,
                discharge_wh,
            });
        }

        self.last_sample_time = Some(now);
        self.last_sample_unix = Some(unix_now);
        self.last_charge_power = charge;
        self.last_discharge_power = discharge;

        self.prune(now);
    }

    /// Remove samples older than the rolling window.
    fn prune(&mut self, now: Instant) {
        let cutoff = now - self.window;
        while self.samples.front().is_some_and(|s| s.instant < cutoff) {
            self.samples.pop_front();
        }
    }

    /// Total energy charged in the rolling window.
    pub fn total_charge_wh(&self) -> WattHours {
        self.samples.iter().map(|s| s.charge_wh).sum()
    }

    /// Total energy discharged in the rolling window.
    pub fn total_discharge_wh(&self) -> WattHours {
        self.samples.iter().map(|s| s.discharge_wh).sum()
    }

    /// Round-trip efficiency percentage (0–100), or None if insufficient data.
    pub fn rte_percent(&self) -> Option<Percent> {
        let charged = self.total_charge_wh();
        if charged.get() < 1.0 {
            return None; // Need at least 1 Wh of charge data
        }
        let discharged = self.total_discharge_wh();
        let rte = (discharged.get() / charged.get()) * 100.0;

        // When RTE drops below 70%, use geometric mean fallback to smooth out
        // poor efficiency readings (per Zendure-HA-zenSDK approach).
        if rte < 70.0 {
            Some(Percent((rte / 100.0).sqrt() * 100.0))
        } else {
            Some(Percent(rte))
        }
    }

    /// Estimate usable energy (kWh) that can be recovered from the battery.
    pub fn usable_kwh(
        &self,
        soc: Soc,
        min_soc: Soc,
        pack_capacities: &[WattHours],
    ) -> KiloWattHours {
        let total_capacity_wh: WattHours = pack_capacities.iter().copied().sum();
        if total_capacity_wh.get() <= 0.0 || soc <= min_soc {
            return KiloWattHours::ZERO;
        }

        let usable_soc_fraction = soc.fraction_above(min_soc);
        let rte_factor = self.rte_percent().map_or(0.85, Percent::fraction);

        WattHours(total_capacity_wh.get() * usable_soc_fraction * rte_factor).to_kwh()
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
            last_charge_power: f64::from(self.last_charge_power.get()),
            last_discharge_power: f64::from(self.last_discharge_power.get()),
            last_sample_ts: self.last_sample_unix,
        };

        match serde_json::to_string(&state) {
            Ok(json) => match std::fs::write(&self.state_path, json) {
                Ok(()) => self.save_failed.store(false, Ordering::Relaxed),
                Err(e) => {
                    if !self.save_failed.swap(true, Ordering::Relaxed) {
                        tracing::warn!(
                            "Failed to persist RTE state to {}: {e} (further failures logged at debug)",
                            self.state_path.display(),
                        );
                    } else {
                        tracing::debug!("Failed to persist RTE state: {e}");
                    }
                }
            },
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

        self.last_charge_power = Watts(state.last_charge_power.round() as i32);
        self.last_discharge_power = Watts(state.last_discharge_power.round() as i32);

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
        tracker.record_at(t0, Watts(1000), Watts::ZERO);
        tracker.record_at(t0 + Duration::from_secs(3600), Watts(1000), Watts::ZERO);

        // Simulate 1 hour of discharging at 850W (85% efficiency)
        tracker.record_at(t0 + Duration::from_secs(3600), Watts::ZERO, Watts(850));
        tracker.record_at(t0 + Duration::from_secs(7200), Watts::ZERO, Watts(850));

        let rte = tracker.rte_percent().unwrap().get();
        assert!((rte - 85.0).abs() < 1.0, "Expected ~85% RTE, got {rte}");
    }

    #[test]
    fn test_rte_geometric_mean_fallback() {
        let path = temp_path();
        let mut tracker = RteTracker::new(path);
        let t0 = Instant::now();

        // Simulate 1 hour of charging at 1000W
        tracker.record_at(t0, Watts(1000), Watts::ZERO);
        tracker.record_at(t0 + Duration::from_secs(3600), Watts(1000), Watts::ZERO);

        // Simulate 1 hour of discharging at 500W (50% raw efficiency → below 70%)
        tracker.record_at(t0 + Duration::from_secs(3600), Watts::ZERO, Watts(500));
        tracker.record_at(t0 + Duration::from_secs(7200), Watts::ZERO, Watts(500));

        let rte = tracker.rte_percent().unwrap().get();
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
        let usable = tracker
            .usable_kwh(
                Soc::new(80),
                Soc::new(10),
                &[WattHours(1920.0), WattHours(1920.0)],
            )
            .get();
        assert!(
            (usable - 2.285).abs() < 0.1,
            "Expected ~2.28 kWh, got {usable}"
        );
    }

    #[test]
    fn test_usable_kwh_at_min_soc() {
        let tracker = RteTracker::new(temp_path());
        assert_eq!(
            tracker
                .usable_kwh(Soc::new(10), Soc::new(10), &[WattHours(1920.0)])
                .get(),
            0.0
        );
    }

    #[test]
    fn test_persistence_roundtrip() {
        let path = temp_path();
        let t0 = Instant::now();

        // Create tracker, add data, save
        {
            let mut tracker = RteTracker::new(path.clone());
            tracker.record_at(t0, Watts(1000), Watts::ZERO);
            tracker.record_at(t0 + Duration::from_secs(3600), Watts::ZERO, Watts(850));
            tracker.save();
        }

        // Load into new tracker — samples should be restored
        let tracker2 = RteTracker::new(path);
        assert!(
            tracker2.total_charge_wh().get() > 0.0 || tracker2.total_discharge_wh().get() > 0.0
        );
    }

    #[test]
    fn test_creates_missing_state_directory() {
        // The default path lives under /var/lib/zendure, which systemd's
        // StateDirectory owns but which won't exist on a fresh box until the
        // service has started once. Persisting must not depend on that.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/state/rte.json");
        let t0 = Instant::now();

        let mut tracker = RteTracker::new(path.clone());
        tracker.record_at(t0, Watts(1000), Watts::ZERO);
        tracker.record_at(t0 + Duration::from_secs(3600), Watts::ZERO, Watts(850));
        tracker.save();

        assert!(
            path.exists(),
            "state file should be written into a created directory"
        );
        let restored = RteTracker::new(path);
        assert!(
            restored.total_charge_wh().get() > 0.0 || restored.total_discharge_wh().get() > 0.0
        );
    }

    #[test]
    fn test_unwritable_path_does_not_panic() {
        // A misconfigured path must degrade to "no persistence", never take the
        // controller down — save() runs on every poll.
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("iam-a-file");
        std::fs::write(&blocker, b"x").unwrap();

        let mut tracker = RteTracker::new(blocker.join("state.json"));
        tracker.record_at(Instant::now(), Watts(1000), Watts::ZERO);
        tracker.save();
        tracker.save(); // second failure takes the quiet path
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

    /// The old-format JSON is a live production file holding a 24h window;
    /// with no `#[serde(default)]` anywhere, any shape change fails to parse
    /// and `load` swallows it with a warning, silently losing the window.
    /// Pins the wire format: parses the exact old string and asserts re-serializing it
    /// is byte-identical.
    #[test]
    fn test_persisted_state_wire_format_unchanged() {
        let json = r#"{"samples":[{"ts":1.0,"charge_wh":2.0,"discharge_wh":3.0}],"last_charge_power":100.0,"last_discharge_power":0.0,"last_sample_ts":1.0}"#;

        let state: PersistedState = serde_json::from_str(json).unwrap();

        assert_eq!(state.samples.len(), 1);
        assert_eq!(state.samples[0].ts, 1.0);
        assert_eq!(state.samples[0].charge_wh.get(), 2.0);
        assert_eq!(state.samples[0].discharge_wh.get(), 3.0);
        assert_eq!(state.last_charge_power, 100.0);
        assert_eq!(state.last_discharge_power, 0.0);
        assert_eq!(state.last_sample_ts, Some(1.0));

        let round_tripped = serde_json::to_string(&state).unwrap();
        assert_eq!(round_tripped, json);
    }
}
