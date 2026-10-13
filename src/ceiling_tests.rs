//! Tests for `ceiling.rs`.

use chrono::Weekday;

use super::*;
use crate::clock::Clock;
use crate::config::{Config, SessionConfig};
use crate::controller::Controller;
use crate::device::RecordingBattery;
use crate::fixtures::journey::{NOW_MS, at};
use crate::registry::{self, Battery};

const DAY_SECS: i64 = 86_400;

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
    for (day, weekday) in (0..).zip(days) {
        for tick in 0..5 {
            let now = at(day * DAY_SECS + tick * 5);
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

    assert!(ceilings.due(Soc::FULL, at(5)).is_empty());
    assert_eq!(ceilings.due(Soc::FULL, at(60)), vec![pack.clone()]);

    ceilings.record(&pack, Soc::FULL, at(60), true);
    assert!(ceilings.due(Soc::FULL, at(600)).is_empty());
}

/// The same pause, through the adapter's own `Err`.
#[tokio::test]
async fn a_write_the_device_refuses_is_sent_again_after_the_pause() {
    let devices = Devices::new([Battery::Recording(RecordingBattery::failing_ceiling_at(
        "pack-a", 0,
    ))]);
    let mut ceilings = CeilingSync::new(devices.ceiling_holders());

    for secs in (0..=600).step_by(5) {
        sync(&devices, &mut ceilings, Soc::FULL, at(secs)).await;
    }

    assert_eq!(ceilings_of(&devices, "pack-a"), vec![Soc::FULL, Soc::FULL]);
}

/// A new target is a different write, not a retry of the one that failed.
#[test]
fn a_target_change_after_a_failure_is_written_at_once() {
    let pack = DeviceId::new("pack-a");
    let mut ceilings = CeilingSync::new([pack.clone()]);
    ceilings.record(&pack, Soc::FULL, at(0), false);

    assert_eq!(ceilings.due(Soc::new(95), at(5)), vec![pack]);
}

/// A pack that restarts onto its flash value no longer holds what landed, and
/// only its own report says so. One that clamps the value keeps disagreeing,
/// so the rewrite waits out the pause rather than following every poll.
#[tokio::test]
async fn a_ceiling_the_device_reverted_is_rewritten_after_the_pause() {
    let devices = recording(&["pack-a"]);
    let pack = DeviceId::new("pack-a");
    let mut ceilings = CeilingSync::new(devices.ceiling_holders());
    sync(&devices, &mut ceilings, Soc::FULL, at(0)).await;

    for secs in (5..=125).step_by(5) {
        ceilings.observe(&pack, Soc::from_tenths(950));
        sync(&devices, &mut ceilings, Soc::FULL, at(secs)).await;
    }

    assert_eq!(ceilings_of(&devices, "pack-a"), vec![Soc::FULL; 3]);
}

#[tokio::test]
async fn a_device_reporting_the_target_is_left_alone() {
    let devices = recording(&["pack-a"]);
    let pack = DeviceId::new("pack-a");
    let mut ceilings = CeilingSync::new(devices.ceiling_holders());
    sync(&devices, &mut ceilings, Soc::FULL, at(0)).await;

    for secs in (5..=600).step_by(5) {
        ceilings.observe(&pack, Soc::from_tenths(1000));
        sync(&devices, &mut ceilings, Soc::FULL, at(secs)).await;
    }

    assert_eq!(ceilings_of(&devices, "pack-a"), vec![Soc::FULL]);
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
