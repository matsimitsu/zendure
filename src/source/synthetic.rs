//! A synthetic house meter, so a brokerless run exercises the controller
//! instead of proving nothing.
//!
//! With no MQTT broker there is no Shelly, so nothing ever produces a
//! [`MeterObservation`] and the engine only ever sees `MqttTimeout`. A
//! controller that only ever fires its failsafe has not been tested — it has
//! just been left alone. This module makes a fake house: a load, a solar
//! curve, and a battery whose flow feeds back into what the "meter" reports,
//! and pushes readings onto the exact same [`MqttEvent`] channel the real
//! Shelly subscriber uses. The coordinator loop cannot tell the two apart,
//! which is the whole point of [`super`]'s missing `trait Source` — this is
//! the second producer that module's doc comment says would still cost
//! several files to add for real, and here it costs none, because it never
//! goes near `mqtt.rs`, `run_subscriber` or `Config` at all. It is a second
//! *feeder* of the same channel, not a second implementation behind a trait.
//!
//! **The feedback term is the entire point of this file.** A naive version
//! would compute `grid = load - solar` once per tick and never look at the
//! battery again. That version runs, and it lies: the meter reports a fixed
//! surplus forever, the controller charges harder in response, the meter
//! keeps reporting the same surplus because charging was never subtracted
//! from it, and the controller ramps straight to its cap and pins there. It
//! looks alive — numbers are moving, MQTT events are flowing — and it
//! demonstrates nothing, because the one thing a real meter would do (show
//! the battery's own draw) never happens. Subtracting `battery.flow()` from
//! the grid total closes that loop: a charging battery (negative flow) makes
//! the subtraction *add* to the grid figure, pushing it toward import, and
//! the controller sees that and backs off. A discharging battery pushes the
//! other way. That closed loop, not the bell curve or the phase split, is
//! what makes a run against this file worth anything.
//!
//! `run.rs` spawns [`run_synthetic_meter`] instead of the real
//! `mqtt::run_subscriber` when `[meter] kind = "synthetic"` is configured —
//! see `config::MeterConfig` and `registry::from_config`.

use std::sync::Arc;
use std::time::Duration;

use chrono_tz::Tz;
use tokio::sync::mpsc;

use crate::clock::Clock;
use crate::mqtt::MqttEvent;
use crate::simulation::VirtualBattery;
use crate::units::{BatteryPower, GridPower, SolarPower, Watts};
use crate::world::MeterReading;

use super::MeterObservation;

/// The house side of the simulation: a constant base load and a solar array
/// with a rated peak, neither of which knows the battery exists. The battery's
/// contribution is folded in afterwards, in [`observation`] — keeping it out
/// of this struct is what lets `at` be tested with nothing but a `Clock`.
pub struct HouseProfile {
    base_load: Watts,
    solar_peak: Watts,
}

impl HouseProfile {
    pub fn new(base_load: Watts, solar_peak: Watts) -> Self {
        HouseProfile {
            base_load,
            solar_peak,
        }
    }

    /// The house's own load and its solar production at this hour, before the
    /// battery is accounted for.
    ///
    /// Solar follows a cosine centred on noon and clamped at zero, rather than
    /// a gaussian: a gaussian only *approaches* zero at the edges of the day,
    /// and a test asserting "zero at 3am" would be asserting an approximation.
    /// `cos(pi * (hour - 12) / 12)` is negative for every hour more than six
    /// hours from noon — that is, before roughly 06:00 and after roughly
    /// 18:00 — and `.max(0.0)` turns that negative stretch into an exact,
    /// reproducible zero rather than a small positive number that happens to
    /// round away in a demo. At noon the cosine is exactly `1.0`, so
    /// production is exactly the rated peak, not merely close to it.
    ///
    /// The load is returned unchanged: a constant base load is enough to make
    /// the feedback loop in [`observation`] observable, and giving it its own
    /// time-of-day shape would be a second bell curve to get right for a case
    /// nothing here needs yet.
    pub fn at(&self, clock: &Clock) -> (Watts, SolarPower) {
        const PEAK_HOUR: f64 = 12.0;
        const HALF_DAY: f64 = 12.0;

        let hours_from_noon = f64::from(clock.hour) - PEAK_HOUR;
        let fraction = (std::f64::consts::PI * hours_from_noon / HALF_DAY)
            .cos()
            .max(0.0);

        let solar = SolarPower::new(self.solar_peak.as_f64() * fraction);
        (self.base_load, solar)
    }
}

/// Which synthetic phase carries the solar export, mirroring the real Pro 3EM
/// installation (see `source/shelly.rs`'s doc comment): one phase nets the
/// inverter's production against its own load, the other two carry load only.
/// There is no configuration knob for it here — unlike `SolarPhase`, this is
/// not describing a real wiring choice a person made, just picking one of the
/// three synthetic phases to be "the" solar phase so all three are not
/// identical.
const SOLAR_PHASE: usize = 0;

/// How the load (net of the battery) is split across three synthetic phases
/// before solar is netted out of [`SOLAR_PHASE`]. Unequal and summing to
/// `1.0` so the three phases are plausible values rather than three identical
/// thirds — a real house's phases are never balanced that evenly, and
/// `MeterReading.phases` is journaled, so three identical numbers would be a
/// small standing lie about a real installation to whoever reads it back.
const PHASE_WEIGHTS: [f64; 3] = [0.4, 0.35, 0.25];

/// Folds the battery's own flow into a [`HouseProfile`] reading to produce the
/// normalized observation a real meter would report: `grid.total = load -
/// solar - battery.flow()`. See this module's doc comment for why the
/// `battery.flow()` term cannot be dropped.
///
/// A free function taking the flow as a plain [`BatteryPower`], rather than a
/// method that reaches into a battery itself, so the arithmetic is testable
/// against a chosen flow without constructing (or ticking) a real
/// [`VirtualBattery`].
fn observation(profile: &HouseProfile, clock: &Clock, flow: BatteryPower) -> MeterObservation {
    let (load, solar) = profile.at(clock);

    // The load net of the battery, before solar is netted out of one phase.
    // Subtracting `flow` here (rather than after splitting into phases) is
    // what makes the phase split sum back to the total below: both start from
    // this same figure.
    let net_of_battery = load.as_f64() - flow.as_f64();

    let mut phases = [
        GridPower(net_of_battery * PHASE_WEIGHTS[0]),
        GridPower(net_of_battery * PHASE_WEIGHTS[1]),
        GridPower(net_of_battery * PHASE_WEIGHTS[2]),
    ];
    phases[SOLAR_PHASE] = GridPower(phases[SOLAR_PHASE].get() - solar.get());

    let total = GridPower(net_of_battery - solar.get());

    MeterObservation {
        grid: MeterReading::new(total, phases),
        solar,
    }
}

/// Feeds synthetic [`MeterObservation`]s onto the coordinator's own
/// `MqttEvent` channel, once a second — the Shelly's own rate, so a
/// brokerless run sees the same cadence a real one does, not an
/// artificially fast or slow substitute.
///
/// Takes `battery` as an `Arc` rather than a reference: the device registry
/// holds its own clone so the same battery can be actuated by the controller
/// and read by this loop at once. There is no cycle in that sharing — the
/// battery is read here, never written to; only [`super::super::device`]'s
/// `BatteryController::apply` writes it, through its own clone of the same
/// `Arc`.
///
/// If `tx.send` fails, the receiving end — the coordinator loop — is gone, so
/// there is nothing left to feed. Logging and returning is correct; retrying
/// or spinning would just burn CPU narrating a shutdown that has already
/// happened.
pub async fn run_synthetic_meter(
    profile: HouseProfile,
    battery: Arc<VirtualBattery>,
    timezone: Tz,
    tx: mpsc::Sender<MqttEvent>,
) {
    let mut ticker = tokio::time::interval(Duration::from_secs(1));

    loop {
        ticker.tick().await;

        let clock = Clock::now(timezone);
        let flow = battery.flow();
        let obs = observation(&profile, &clock, flow);

        if tx.send(MqttEvent::Meter(obs)).await.is_err() {
            tracing::info!("Synthetic meter: coordinator gone, stopping");
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::Command;
    use crate::device::BatterySpec;
    use crate::units::{Efficiency, PowerCap, Setpoint, Soc, WattHours};
    use crate::world::DeviceId;

    fn profile() -> HouseProfile {
        HouseProfile::new(Watts(500), Watts(3_000))
    }

    fn clock_at(hour: u32) -> Clock {
        Clock {
            hour,
            ..Clock::test_at(0)
        }
    }

    #[test]
    fn zero_solar_at_3am() {
        let (_, solar) = profile().at(&clock_at(3));
        assert_eq!(solar, SolarPower::ZERO);
    }

    #[test]
    fn near_peak_solar_at_noon() {
        let (_, solar) = profile().at(&clock_at(12));
        // Exactly the rated peak: `cos(0) == 1.0` precisely, not merely close
        // to it, so this asserts equality rather than a tolerance.
        assert_eq!(solar, SolarPower::new(3_000.0));
    }

    #[test]
    fn the_profile_is_deterministic() {
        let clock = clock_at(9);
        assert_eq!(profile().at(&clock), profile().at(&clock));
    }

    #[test]
    fn phases_sum_to_the_total() {
        let obs = observation(&profile(), &clock_at(9), BatteryPower::ZERO);
        let summed: f64 = obs.grid.phases.iter().map(|p| p.get()).sum();
        assert!(
            (summed - obs.grid.total.get()).abs() < 1e-9,
            "phases {:?} did not sum to total {}",
            obs.grid.phases,
            obs.grid.total
        );
    }

    #[test]
    fn solar_lands_on_the_designated_phase_only() {
        // At noon, with a battery held idle, only the solar phase should
        // differ from the other two's even split of the (unchanged) load.
        let obs = observation(&profile(), &clock_at(12), BatteryPower::ZERO);
        let untouched: Vec<f64> = obs
            .grid
            .phases
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != SOLAR_PHASE)
            .map(|(_, p)| p.get())
            .collect();
        for p in untouched {
            assert!(p > 0.0, "a non-solar phase went negative: {p}");
        }
        assert!(
            obs.grid.phases[SOLAR_PHASE].get() < 0.0,
            "the solar phase should show net export at noon, got {}",
            obs.grid.phases[SOLAR_PHASE]
        );
    }

    fn battery(soc: Soc) -> VirtualBattery {
        VirtualBattery::new(
            DeviceId::new("synthetic"),
            BatterySpec {
                max_charge_power: PowerCap::new(2_400),
                max_discharge_power: PowerCap::new(800),
            },
            vec![WattHours(10_000.0)],
            soc,
            Efficiency::new(95.0),
            Efficiency::new(95.0),
        )
    }

    /// The test the sign error in the arithmetic would have failed: a
    /// discharging battery is positive `BatteryPower`, and subtracting a
    /// positive number must *reduce* the grid figure (push it toward export),
    /// while a charging battery's negative flow must *increase* it (push it
    /// toward import). Getting the sign backwards would make a discharging
    /// battery look like it was importing more, which is exactly backwards
    /// from what a real meter would show.
    #[tokio::test]
    async fn battery_flow_pushes_the_grid_reading_in_the_correct_direction() {
        let clock = clock_at(9); // some solar, but not the whole story here
        let house = profile();

        let idle = battery(Soc::new(50));
        idle.apply_at(tokio::time::Instant::now(), &Command::SetIdle);
        let idle_reading = observation(&house, &clock, idle.flow());

        let charging = battery(Soc::new(50));
        charging.apply_at(
            tokio::time::Instant::now(),
            &Command::SetCharge(Setpoint::new(1_000)),
        );
        let charging_reading = observation(&house, &clock, charging.flow());

        let discharging = battery(Soc::new(50));
        discharging.apply_at(
            tokio::time::Instant::now(),
            &Command::SetDischarge(Setpoint::new(1_000)),
        );
        let discharging_reading = observation(&house, &clock, discharging.flow());

        assert!(
            charging_reading.grid.total.get() > idle_reading.grid.total.get(),
            "a charging battery ({} W) must push the grid reading toward \
             import relative to idle ({} W)",
            charging_reading.grid.total,
            idle_reading.grid.total,
        );
        assert!(
            discharging_reading.grid.total.get() < idle_reading.grid.total.get(),
            "a discharging battery ({} W) must push the grid reading toward \
             export relative to idle ({} W)",
            discharging_reading.grid.total,
            idle_reading.grid.total,
        );
    }
}
