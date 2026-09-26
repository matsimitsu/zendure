//! A synthetic house meter, so a brokerless run exercises the controller
//! instead of proving nothing. It makes a fake house (load, solar curve,
//! battery) and answers a [`MeterSample`] read from it, so the coordinator
//! loop cannot tell it apart from a real meter.
//!
//! **The feedback term is the entire point.** `grid = load - solar` alone
//! lies: the meter reports a fixed surplus forever, the controller charges
//! harder, and pins at its cap while looking alive. Subtracting
//! `battery.flow()` closes the loop — a charging battery pushes the grid figure toward
//! import, and the controller backs off.

use std::sync::Arc;

use chrono::{TimeZone, Timelike};
use chrono_tz::Tz;

use crate::clock::Clock;
use crate::device::PollError;
use crate::simulation::VirtualBattery;
use crate::units::{BatteryPower, GridPower, SolarPower, Watts};
use crate::world::MeterReading;

use super::{MeterObservation, MeterSample};

/// The house side of the simulation: a constant base load and a solar array
/// with a rated peak, neither of which knows the battery exists. The battery's
/// contribution is folded in afterwards, in [`observation`] — keeping it out
/// of this struct is what lets `at` be tested with nothing but a `Clock`.
pub struct HouseProfile {
    base_load: Watts,
    solar_peak: Watts,
    timezone: Tz,
}

impl HouseProfile {
    pub fn new(base_load: Watts, solar_peak: Watts, timezone: Tz) -> Self {
        HouseProfile {
            base_load,
            solar_peak,
            timezone,
        }
    }

    /// The house's own load and its solar production at this instant, before
    /// the battery. Solar follows a cosine centred on noon, clamped at zero (not
    /// a gaussian, which only approaches zero at the day's edges): `cos(pi *
    /// (hour-12)/12)` is negative before ~06:00 and after ~18:00, and exactly `1.0` at
    /// noon — an exact, reproducible zero/peak rather than an approximation. The load
    /// is returned unchanged; a second bell curve isn't needed yet.
    pub fn at(&self, clock: &Clock) -> (Watts, SolarPower) {
        const PEAK_HOUR: f64 = 12.0;
        const HALF_DAY: f64 = 12.0;

        let fraction = (std::f64::consts::PI * (self.local_hour(clock) - PEAK_HOUR) / HALF_DAY)
            .cos()
            .max(0.0);

        let solar = SolarPower::new(self.solar_peak.as_f64() * fraction);
        (self.base_load, solar)
    }

    /// The hour of day including its minutes, both in the configured zone: a
    /// half-hour offset would otherwise step the curve backwards twice an hour.
    fn local_hour(&self, clock: &Clock) -> f64 {
        let Some(local) = self
            .timezone
            .timestamp_millis_opt(clock.now.as_millis())
            .single()
        else {
            return f64::from(clock.hour);
        };
        f64::from(local.hour()) + f64::from(local.minute()) / 60.0
    }
}

/// Which synthetic phase carries the solar export, mirroring a real Pro 3EM
/// installation: one phase nets production against its own load, the other
/// two carry load only. No config knob, unlike `SolarPhase` — this just picks one of
/// three synthetic phases to be "the" solar phase so all three aren't identical.
const SOLAR_PHASE: usize = 0;

/// How the load (net of battery) splits across three synthetic phases before
/// solar is netted out of [`SOLAR_PHASE`]. Unequal, summing to `1.0`: a real
/// house's phases are never balanced evenly, and since `MeterReading.phases`
/// is journaled, three identical thirds would be a standing lie to whoever reads it
/// back.
const PHASE_WEIGHTS: [f64; 3] = [0.4, 0.35, 0.25];

/// Folds the battery's own flow into a [`HouseProfile`] reading: `grid.total =
/// load - solar - battery.flow()` (see the module doc for why that term can't
/// be dropped). A free function taking flow as a plain [`BatteryPower`], not
/// a method on the battery, so the arithmetic is testable against a chosen flow without
/// constructing or ticking a real [`VirtualBattery`].
pub(crate) fn observation(
    profile: &HouseProfile,
    clock: &Clock,
    flow: BatteryPower,
) -> MeterObservation {
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

/// A [`HouseProfile`] wired to the battery whose flow it nets out. `battery`
/// is the device registry's own `Arc`: read-only here, only
/// [`crate::device`]'s `BatteryController::apply` writes it, so a read sees
/// whatever the loop last commanded.
pub struct SyntheticMeter {
    profile: HouseProfile,
    battery: Arc<VirtualBattery>,
}

impl SyntheticMeter {
    pub fn new(profile: HouseProfile, battery: Arc<VirtualBattery>) -> Self {
        SyntheticMeter { profile, battery }
    }

    pub fn id(&self) -> &str {
        "synthetic meter"
    }

    /// Infallible, and with nothing to capture: there is no wire format
    /// between the house and the reader.
    pub async fn sample(&self) -> Result<MeterSample, PollError> {
        let clock = Clock::now(self.profile.timezone);

        Ok(MeterSample {
            observation: observation(&self.profile, &clock, self.battery.flow()),
            raw: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::command::Command;
    use crate::device::BatterySpec;
    use crate::units::{Efficiency, PowerCap, Setpoint, Soc, Timestamp, WattHours};
    use crate::world::DeviceId;

    fn profile() -> HouseProfile {
        profile_in(chrono_tz::UTC)
    }

    fn profile_in(timezone: Tz) -> HouseProfile {
        HouseProfile::new(Watts(500), Watts(3_000), timezone)
    }

    fn clock_at(hour: u32) -> Clock {
        clock_at_minute(hour, 0)
    }

    fn clock_at_minute(hour: u32, minute: u32) -> Clock {
        clock_in(chrono_tz::UTC, hour, minute)
    }

    /// `at` reads the timestamp, in the profile's zone; `hour` only agrees with it.
    fn clock_in(timezone: Tz, hour: u32, minute: u32) -> Clock {
        let local = timezone
            .with_ymd_and_hms(2026, 6, 15, hour, minute, 0)
            .unwrap();
        Clock {
            hour,
            now: Timestamp::from(local),
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

    /// The failure this catches: solar keyed off the integer hour, a staircase
    /// no smooth ramp can be simulated against.
    #[test]
    fn solar_climbs_within_the_hour_rather_than_stepping_at_it() {
        let house = profile();
        let readings: Vec<f64> = [0, 15, 30, 45]
            .iter()
            .map(|m| house.at(&clock_at_minute(9, *m)).1.get())
            .collect();

        for pair in readings.windows(2) {
            assert!(
                pair[1] > pair[0],
                "solar stalled across a quarter-hour: {readings:?}",
            );
        }
        assert!(
            *readings.last().unwrap() < house.at(&clock_at(10)).1.get(),
            "the last quarter of 09:00 overshot 10:00: {readings:?}",
        );
    }

    /// The failure this catches: a curve that approaches its peak and its
    /// zeroes rather than landing on them exactly.
    #[test]
    fn the_curve_keeps_its_exact_peak_and_its_exact_zeroes() {
        let house = profile();

        assert_eq!(house.at(&clock_at(12)).1, SolarPower::new(3_000.0));
        assert!(house.at(&clock_at_minute(11, 59)).1 < SolarPower::new(3_000.0));
        assert!(house.at(&clock_at_minute(12, 1)).1 < SolarPower::new(3_000.0));

        assert_eq!(house.at(&clock_at_minute(5, 59)).1, SolarPower::ZERO);
        assert_eq!(house.at(&clock_at_minute(18, 1)).1, SolarPower::ZERO);
    }

    /// The failure this catches: a local hour mixed with a UTC sub-hour
    /// fraction, which in a half-hour-offset zone misses local noon.
    #[test]
    fn a_half_hour_offset_zone_peaks_at_its_own_noon() {
        let tz = chrono_tz::Asia::Kolkata;
        let house = profile_in(tz);

        assert_eq!(house.at(&clock_in(tz, 12, 0)).1, SolarPower::new(3_000.0));
        let readings: Vec<f64> = (0..60)
            .map(|m| house.at(&clock_in(tz, 9, m)).1.get())
            .collect();
        for pair in readings.windows(2) {
            assert!(pair[1] > pair[0], "solar stepped backwards: {readings:?}");
        }
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

    /// The test a sign error in the arithmetic would fail: discharging is
    /// positive `BatteryPower`, so subtracting it must *reduce* the grid
    /// figure (toward export); charging's negative flow must *increase* it (toward
    /// import) — backwards would show a discharging battery as importing more.
    #[tokio::test]
    async fn battery_flow_pushes_the_grid_reading_in_the_correct_direction() {
        let clock = clock_at(9); // some solar, but not the whole story here
        let house = profile();

        // The pack slews, so each flow is read once it has arrived.
        let t0 = tokio::time::Instant::now();
        let arrived = t0 + Duration::from_secs(3);

        let idle = battery(Soc::new(50));
        idle.apply_at(t0, &Command::SetIdle);
        let idle_reading = observation(&house, &clock, idle.flow_at(arrived));

        let charging = battery(Soc::new(50));
        charging.apply_at(t0, &Command::SetCharge(Setpoint::new(1_000)));
        let charging_reading = observation(&house, &clock, charging.flow_at(arrived));

        let discharging = battery(Soc::new(50));
        discharging.apply_at(t0, &Command::SetDischarge(Setpoint::new(1_000)));
        let discharging_reading = observation(&house, &clock, discharging.flow_at(arrived));

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
