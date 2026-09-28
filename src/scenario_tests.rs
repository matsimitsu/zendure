//! The closed loop at production tuning: the real engine deciding against a
//! virtual battery and a synthetic house, on the scan cycle's 3 s tick, for
//! half an hour of simulated time.
//!
//! Driven by hand rather than through `run()`: every timestamp the controller
//! decides on comes from `Clock::now`, which is wall time, so a paused Tokio
//! runtime freezes the battery's physics but not the controller's cooldowns.
//! Here both run on one explicit timeline. The round order is `run`'s own —
//! fold each device's sample, then the meter's, then apply, then sample for
//! the next round — which `a_tick_emits_its_device_updates_before_its_meter`
//! pins on the real loop.

use chrono_tz::Tz;
use tokio::time::{Duration, Instant};

use super::VirtualBattery;
use crate::allocate::Directive;
use crate::battery::BatteryState;
use crate::clock::Clock;
use crate::command::Command;
use crate::config::{Config, SessionConfig};
use crate::controller::Controller;
use crate::device::AC2400_PLUS;
use crate::engine::Engine;
use crate::event::Event;
use crate::models::ControlMode;
use crate::source::MeterObservation;
use crate::source::synthetic::{HouseProfile, observation};
use crate::units::{Efficiency, Soc, WattHours, Watts};
use crate::world::{DeviceId, Measurement, World};

/// The scan period `config.example.toml` ships.
const TICK: Duration = Duration::from_secs(3);

/// 2025-09-04, in UTC so the synthetic sun's local noon is the instant below.
const NOON_MS: i64 = 1_756_987_200_000;
const MIDNIGHT_MS: i64 = NOON_MS - 12 * 3_600_000;

/// What the box runs with, read from the file it is deployed from rather than
/// restated here.
fn production_tuning() -> SessionConfig {
    let (config, _) = Config::from_toml_str(include_str!("../config.example.toml")).unwrap();
    config.session()
}

/// How many rounds old each source's sample is when a tick decides on it.
/// `run` reads the round before, for both: `Lag { meter: 1, device: 1 }`.
#[derive(Clone, Copy)]
struct Lag {
    meter: usize,
    device: usize,
}

const MATCHED: Lag = Lag {
    meter: 1,
    device: 1,
};

/// One tick's decision, signed the way the meter sees the battery:
/// discharging positive, charging negative, idle zero.
#[derive(Debug, Clone, Copy)]
struct Tick {
    mode: ControlMode,
    setpoint: i32,
}

struct Scenario {
    profile: HouseProfile,
    start_ms: i64,
    soc: Soc,
    lag: Lag,
    duration: Duration,
}

impl Scenario {
    fn run(&self) -> Vec<Tick> {
        let session = production_tuning();
        let clock =
            |round: usize| Clock::test_at(self.start_ms + round as i64 * TICK.as_millis() as i64);
        let battery = VirtualBattery::new(
            DeviceId::new("sim"),
            AC2400_PLUS,
            vec![WattHours(2_400.0)],
            self.soc,
            Efficiency::new(95.0),
            Efficiency::new(95.0),
        );
        let id = DeviceId::new("sim");
        let t0 = Instant::now();
        let instant = |round: usize| t0 + TICK * round as u32;

        let mut engine = Engine::new(
            Controller::from_session(&session, &clock(0)),
            World::new(),
            Duration::from_secs(session.mqtt_timeout_secs),
        );

        let sample = |round: usize| -> (BatteryState, MeterObservation) {
            let at = instant(round);
            let reading = battery.reading_at(at);
            let meter = observation(&self.profile, &clock(round), battery.flow_at(at));
            (reading, meter)
        };

        // The startup poll `main` seeds the world with.
        let mut samples = vec![sample(0)];
        engine.step(&Event::DeviceUpdate {
            at: clock(0),
            id: id.clone(),
            measurement: Measurement::Battery(samples[0].0.clone()),
        });

        let rounds = (self.duration.as_secs() / TICK.as_secs()) as usize;
        let mut ticks = Vec::with_capacity(rounds);
        for round in 1..=rounds {
            let device = round.saturating_sub(self.lag.device);
            let meter = round.saturating_sub(self.lag.meter);

            engine.step(&Event::DeviceUpdate {
                at: clock(device),
                id: id.clone(),
                measurement: Measurement::Battery(samples[device].0.clone()),
            });
            let observed = &samples[meter].1;
            let step = engine.step(&Event::Meter {
                at: clock(round),
                sampled_at: Some(clock(meter).now),
                grid: observed.grid,
                solar: observed.solar,
            });

            for Directive::Battery { command, .. } in &step.directives {
                battery.apply_at(instant(round), command);
            }
            if let Some(decision) = step.decision {
                let setpoint = match Command::from(&decision) {
                    Command::SetCharge(w) => -w.get(),
                    Command::SetDischarge(w) => w.get(),
                    Command::SetIdle | Command::SetStandby => 0,
                };
                ticks.push(Tick {
                    mode: decision.mode,
                    setpoint,
                });
            }

            samples.push(sample(round));
        }
        ticks
    }
}

/// The figures A0 took from the box's journal, computed the same way over
/// consecutive decisions in one mode.
#[derive(Debug)]
struct Dynamics {
    /// Of consecutive setpoint changes, the share that reverse the one before.
    /// A random walk sits at 0.5.
    reversal_rate: f64,
    mean_step: f64,
    largest_step: i32,
}

fn dynamics(ticks: &[Tick], mode: ControlMode) -> Dynamics {
    let setpoints: Vec<i32> = ticks
        .iter()
        .filter(|t| t.mode == mode)
        .map(|t| t.setpoint)
        .collect();
    let steps: Vec<i32> = setpoints.windows(2).map(|w| w[1] - w[0]).collect();
    let moves: Vec<i32> = steps.iter().copied().filter(|s| *s != 0).collect();
    let reversals = moves
        .windows(2)
        .filter(|w| w[0].signum() != w[1].signum())
        .count();

    Dynamics {
        reversal_rate: if moves.len() < 2 {
            0.0
        } else {
            reversals as f64 / (moves.len() - 1) as f64
        },
        mean_step: steps.iter().map(|s| f64::from(s.abs())).sum::<f64>()
            / steps.len().max(1) as f64,
        largest_step: steps.iter().map(|s| s.abs()).max().unwrap_or(0),
    }
}

fn sunny_noon(lag: Lag) -> Scenario {
    Scenario {
        profile: HouseProfile::new(Watts(300), Watts(2_000), Tz::UTC),
        start_ms: NOON_MS - 15 * 60_000,
        soc: Soc::new(30),
        lag,
        duration: Duration::from_secs(30 * 60),
    }
}

fn quiet_night(lag: Lag) -> Scenario {
    Scenario {
        profile: HouseProfile::new(Watts(400), Watts(0), Tz::UTC),
        start_ms: MIDNIGHT_MS,
        soc: Soc::new(80),
        lag,
        duration: Duration::from_secs(30 * 60),
    }
}

/// Past the ramp a mode change starts with, so the window measures the loop
/// holding a setpoint rather than reaching for it.
const SETTLED_AFTER: usize = 10;

fn settled(ticks: &[Tick], mode: ControlMode) -> Dynamics {
    dynamics(&ticks[SETTLED_AFTER..], mode)
}

/// Solar well above the house: the loop has to hold a charge setpoint
/// against a moving sun. A0's baseline on the box, before the scan cycle,
/// was 59.1 % reversals and a 146 W mean step.
#[test]
fn matched_ages_settle_a_charge() {
    let ticks = sunny_noon(MATCHED).run();
    let charge = settled(&ticks, ControlMode::Charge);

    assert!(charge.reversal_rate < 0.5, "{charge:?}");
    assert!(charge.mean_step < 5.0, "{charge:?}");
    assert!(charge.largest_step < 50, "{charge:?}");
    assert!(
        ticks[SETTLED_AFTER..]
            .iter()
            .all(|t| t.mode == ControlMode::Charge),
        "a steady surplus must never leave Charge"
    );
}

/// A steady load and no sun: the loop has to hold a discharge setpoint at the
/// load less the margin, and never flip to charging on an overshoot.
#[test]
fn matched_ages_settle_a_discharge() {
    let ticks = quiet_night(MATCHED).run();
    let discharge = settled(&ticks, ControlMode::Discharge);

    assert!(discharge.reversal_rate < 0.5, "{discharge:?}");
    assert!(discharge.largest_step < 50, "{discharge:?}");
    // 400 W of load less the 5 W margin is the ideal target, but the
    // velocity-form law's own deadband means it settles once the residual
    // error is small enough to be dropped, not at a bit-exact zero — unlike
    // the old absolute-form law, which happened to land exactly there only
    // because it re-read the device's own (externally, physically
    // converging) report every tick rather than tracking its own commanded
    // history. Within the deadband's width of the ideal is the honest bar.
    let last = ticks
        .last()
        .map(|t| t.setpoint)
        .expect("30 minutes of ticks");
    assert!((last - 395).abs() < 25, "{last} W, {discharge:?}");
}

/// What made the two tests above mean anything under the old absolute-form
/// law: the same scenario with the device's report one round staler than the
/// meter used to hunt, so a harness that passed them anyway couldn't see the
/// defect. The velocity-form law tracks its own commanded history instead of
/// a report that might be this stale, and is insensitive to it — inverted
/// from the pre-A3 assertions, per A1's own note that this needs inverting
/// once the law goes velocity-form.
#[test]
fn a_report_one_round_staler_than_the_meter_no_longer_hunts() {
    let stale = Lag {
        meter: 1,
        device: 2,
    };
    let charge = dynamics(&sunny_noon(stale).run(), ControlMode::Charge);
    let discharge = dynamics(&quiet_night(stale).run(), ControlMode::Discharge);

    assert!(charge.reversal_rate < 0.5, "{charge:?}");
    assert!(discharge.reversal_rate < 0.5, "{discharge:?}");
}
