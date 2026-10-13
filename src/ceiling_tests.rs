//! Tests for `ceiling.rs`.

use chrono::Weekday;

use super::*;
use crate::clock::Clock;
use crate::config::{Config, SessionConfig};
use crate::controller::Controller;
use crate::device::RecordingBattery;
use crate::registry::{self, Battery};

const NOW_MS: i64 = 1_757_000_000_000;

/// Production's tuning: kept at 95 for longevity, full on Mondays.
fn controller() -> Controller {
    let session = SessionConfig {
        max_soc: Soc::new(95),
        balance_weekday: Some(Weekday::Mon),
        ..SessionConfig::test_default()
    };
    Controller::from_session(&session, &Clock::test_at(NOW_MS))
}

fn target(weekday: Weekday) -> Soc {
    controller().soc_limits(weekday, Soc::ZERO).max
}

fn recording(ids: &[&str]) -> Devices {
    Devices::new(
        ids.iter()
            .map(|id| Battery::Recording(RecordingBattery::new(id))),
    )
}

fn ceilings_of(devices: &Devices, id: &str) -> Vec<Soc> {
    match devices.battery(&DeviceId::new(id)) {
        Some(Battery::Recording(battery)) => battery.ceilings(),
        _ => panic!("{id} must be a registered recording battery"),
    }
}

fn at(offset_ms: i64) -> Timestamp {
    Timestamp::from_millis(NOW_MS + offset_ms)
}

/// The device stops at the `socSet` it holds and reports `socLimit: 1`, which
/// the controller honours, so the balance day's 100% is reached by raising
/// `socSet` on every pack — a restart on the balance day included.
#[tokio::test]
async fn a_balance_day_restart_raises_every_packs_ceiling_to_full() {
    let devices = recording(&["pack-a", "pack-b"]);
    let mut ceilings = CeilingSync::new(devices.ceiling_holders());

    sync(&devices, &mut ceilings, target(Weekday::Mon), at(0)).await;

    assert_eq!(ceilings_of(&devices, "pack-a"), vec![Soc::FULL]);
    assert_eq!(ceilings_of(&devices, "pack-b"), vec![Soc::FULL]);
}

#[tokio::test]
async fn the_ceiling_is_written_once_per_target_change_not_every_tick() {
    let devices = recording(&["pack-a"]);
    let mut ceilings = CeilingSync::new(devices.ceiling_holders());

    let days = [Weekday::Sun, Weekday::Mon, Weekday::Tue];
    for (day, weekday) in days.into_iter().enumerate() {
        for tick in 0..5 {
            let now = at(day as i64 * 86_400_000 + tick * 5_000);
            sync(&devices, &mut ceilings, target(weekday), now).await;
        }
    }

    assert_eq!(
        ceilings_of(&devices, "pack-a"),
        vec![Soc::new(95), Soc::FULL, Soc::new(95)],
    );
}

#[test]
fn a_failed_write_is_retried_after_a_pause_not_every_tick() {
    let pack = DeviceId::new("pack-a");
    let mut ceilings = CeilingSync::new([pack.clone()]);

    assert_eq!(ceilings.due(Soc::FULL, at(0)), vec![pack.clone()]);
    ceilings.record(&pack, Soc::FULL, at(0), false);

    assert!(ceilings.due(Soc::FULL, at(5_000)).is_empty());
    assert_eq!(ceilings.due(Soc::FULL, at(60_000)), vec![pack.clone()]);

    ceilings.record(&pack, Soc::FULL, at(60_000), true);
    assert!(ceilings.due(Soc::FULL, at(600_000)).is_empty());
}

/// A new target is a different write, not a retry of the one that failed.
#[test]
fn a_target_change_after_a_failure_is_written_at_once() {
    let pack = DeviceId::new("pack-a");
    let mut ceilings = CeilingSync::new([pack.clone()]);
    ceilings.record(&pack, Soc::FULL, at(0), false);

    assert_eq!(ceilings.due(Soc::new(95), at(5_000)), vec![pack]);
}

#[test]
fn a_virtual_battery_has_no_ceiling_to_keep() {
    let toml = r#"
        [[device]]
        kind = "virtual"
        id = "sim"
        packs = [10000.0]
        soc = 50
        charge_efficiency = 95.0
        discharge_efficiency = 95.0

        [meter]
        kind = "synthetic"
        base_load = 500
        solar_peak = 3000
    "#;
    let (config, _) = Config::from_toml_str(toml).unwrap();
    let devices = registry::from_config(&config);

    assert_eq!(devices.ceiling_holders().count(), 0);
}
