//! A simulated battery with a real integrating energy model, so the
//! controller can run against no hardware at all. Not a stub that echoes
//! back what it was told: it keeps its own store of [`crate::units::WattHours`]
//! and integrates real power over real time, so an overcharging setpoint hits
//! a ceiling and an empty battery really does stop discharging.
//!
//! `[[device]] kind = "virtual"` selects it; `run.rs` reaches it only through
//! [`BatteryController`] and [`BatteryMonitor`], like any device.

use std::sync::Mutex;

use tokio::time::Instant;

use crate::battery::BatteryState;
use crate::command::Command;
use crate::device::{
    BatteryController, BatteryMonitor, BatteryReading, BatterySpec, BatteryTelemetry, PollError,
};
use crate::sync::guard;
use crate::units::{BatteryPower, Efficiency, Setpoint, Soc, WattHours, Watts};
use crate::world::DeviceId;

/// A simulated battery: a rated spec (what it accepts), pack capacities (how
/// much it can hold), and an integrating model of what it currently holds.
/// The model lives behind a `Mutex`, not a plain field, because
/// [`BatteryController::apply`] takes `&self` — see that trait's own doc comment.
pub struct VirtualBattery {
    id: DeviceId,
    spec: BatterySpec,
    /// Capacities of the connected packs; their sum is the usable capacity.
    /// A `Vec`, not one collapsed `WattHours`, since a heterogeneous fleet
    /// (a smaller expansion pack alongside the base unit) needs per-pack values —
    /// leaving room for future per-pack behaviour without changing this field's shape.
    packs: Vec<WattHours>,
    charge_efficiency: Efficiency,
    discharge_efficiency: Efficiency,
    model: Mutex<Model>,
}

/// The part of a `VirtualBattery` that changes on every command and every
/// tick — everything else on the struct is fixed at construction.
struct Model {
    stored: WattHours,
    /// The last commanded flow, signed as `BatteryPower` always is: positive
    /// discharging, negative charging. This is what `advance_to` integrates
    /// forward, and what it overwrites with the *achieved* flow once a clamp
    /// has been applied — see `advance_to`'s step 5.
    power: BatteryPower,
    /// The instant the model was last advanced to. Every `apply` and every
    /// read moves this forward; nothing else does.
    last: Instant,
    standby: bool,
}

impl VirtualBattery {
    pub fn new(
        id: DeviceId,
        spec: BatterySpec,
        packs: Vec<WattHours>,
        soc: Soc,
        charge_efficiency: Efficiency,
        discharge_efficiency: Efficiency,
    ) -> Self {
        let capacity: WattHours = packs.iter().copied().sum();
        // `fraction_above(Soc::ZERO)` rather than a hand-rolled `soc.get() as
        // f64 / 100.0`: the fraction-of-the-pack arithmetic already exists on
        // `Soc` for the controller's own floor calculations, and reusing it
        // here means there is one place that says what an SOC fraction is.
        let stored = WattHours(capacity.get() * soc.fraction_above(Soc::ZERO));
        VirtualBattery {
            id,
            spec,
            packs,
            charge_efficiency,
            discharge_efficiency,
            model: Mutex::new(Model {
                stored,
                power: BatteryPower::ZERO,
                last: Instant::now(),
                standby: false,
            }),
        }
    }

    fn capacity(&self) -> WattHours {
        self.packs.iter().copied().sum()
    }

    /// Integrates the model forward from `model.last` to `now`, in place.
    /// Every public read or write goes through this first, so `model.last`
    /// never falls behind. In order: elapsed time; energy moved over that
    /// span; applied to `stored` with efficiency in the correct direction; clamped into
    /// `[0, capacity]`; achieved flow recomputed from what the clamp let happen;
    /// `model.last` advanced to `now`.
    fn advance_to(&self, model: &mut Model, now: Instant) {
        // Step 1.
        let dt = now.saturating_duration_since(model.last);
        if dt.is_zero() {
            return;
        }

        // Step 2. Both arguments to `integrate` are `model.power`: legitimate
        // for a trapezoidal integrator only because the rate provably didn't
        // change across this span — `apply` integrates the old power up to a
        // new command, and this is the only other writer, at the *end* of the span. A
        // trapezoid over a constant is exact; identical endpoints mean "constant", not
        // a mistake.
        let moved = WattHours::integrate(model.power.into_watts(), model.power.into_watts(), dt);

        let before = model.stored;
        let capacity = self.capacity();

        // Step 3. Charging adds `moved * eta_charge` (heat lost before it
        // lands); discharging removes `moved / eta_discharge` (the pack gives up more
        // than it delivers). Backwards, a round trip would *gain* energy instead of
        // losing it.
        let after = if model.power.charging() > Watts::ZERO {
            WattHours(before.get() + moved.get().abs() * self.charge_efficiency.fraction())
        } else if model.power.discharging() > Watts::ZERO {
            WattHours(before.get() - moved.get().abs() / self.discharge_efficiency.fraction())
        } else {
            before
        };

        // Step 4.
        let clamped = WattHours(after.get().clamp(0.0, capacity.get()));

        // Step 5. Recompute the achieved flow from what actually happened to
        // `stored`, not the commanded setpoint: a pack clamped to full
        // absorbed less than commanded and must *report* less — claiming its
        // stale 1200W setpoint would tell every `flow()` consumer it's still charging,
        // and a feedback loop closed on that figure would never converge.
        let achieved = clamped.get() - before.get();
        model.power = if model.power.charging() > Watts::ZERO {
            // Invert step 3: the meter-side energy that would have produced
            // only `achieved` worth of stored gain.
            let meter_side = WattHours(achieved.abs() / self.charge_efficiency.fraction());
            BatteryPower(-meter_side.over(dt).get())
        } else if model.power.discharging() > Watts::ZERO {
            let meter_side = WattHours(achieved.abs() * self.discharge_efficiency.fraction());
            BatteryPower(meter_side.over(dt).get())
        } else {
            BatteryPower::ZERO
        };

        model.stored = clamped;

        // Step 6.
        model.last = now;
    }

    /// The test seam behind [`VirtualBattery::flow`] and
    /// [`crate::device::BatteryController::apply`]: deterministic given an
    /// explicit instant, so tests drive it directly instead of racing a real
    /// clock. `tokio::time::Instant`, not `std::time::Instant`, lets a test run under a
    /// paused Tokio clock and integrate a simulated hour in microseconds.
    pub(crate) fn apply_at(&self, now: Instant, command: &Command) {
        let mut model = guard(&self.model);

        // Integrates the *old* command up to this moment before installing
        // the new one — the reason step 2 above is allowed to treat `power`
        // as constant since the last change.
        self.advance_to(&mut model, now);

        model.standby = matches!(command, Command::SetStandby);
        model.power = match command {
            // The commanded setpoint is clamped to the spec's cap here, not
            // trusted from the caller: `Setpoint` itself only guarantees
            // non-negative, and this is the boundary where "what was asked
            // for" becomes "what this box can actually do".
            Command::SetCharge(setpoint) => {
                let watts = Setpoint::clamped(Watts(setpoint.get()), self.spec.max_charge_power);
                BatteryPower(-watts.get())
            }
            Command::SetDischarge(setpoint) => {
                let watts = Setpoint::clamped(Watts(setpoint.get()), self.spec.max_discharge_power);
                BatteryPower(watts.get())
            }
            Command::SetIdle | Command::SetStandby => BatteryPower::ZERO,
        };
    }

    /// The test seam behind [`VirtualBattery::reading`]. See `apply_at` for
    /// why this takes an explicit instant rather than reading the clock.
    pub(crate) fn reading_at(&self, now: Instant) -> BatteryState {
        let mut model = guard(&self.model);
        self.advance_to(&mut model, now);

        // NaN-safe: an empty `packs` (capacity zero) makes this `0.0 / 0.0`,
        // and `Soc::from_fraction` maps that to `Soc::ZERO` rather than
        // propagating.
        let soc = Soc::from_fraction(model.stored.get() / self.capacity().get());

        BatteryState {
            soc,
            max_discharge_power: self.spec.max_discharge_power,
            max_charge_power: self.spec.max_charge_power,
            current_power: model.power,
            // A simulated pack never recalibrates and never faults — those
            // are real-hardware conditions this model doesn't produce, so both read as
            // always-false rather than a fabricated value.
            soc_calibrating: false,
            soc_limit_reached: soc >= Soc::FULL,
            fault: false,
        }
    }

    /// The flow the battery is currently reporting — the achieved figure
    /// `advance_to` last computed, not necessarily the commanded one. The
    /// objective reads this back to decide the next setpoint, which is why a full pack
    /// must report zero here instead of its stale commanded power.
    pub fn flow(&self) -> BatteryPower {
        let mut model = guard(&self.model);
        self.advance_to(&mut model, Instant::now());
        model.power
    }

    pub fn reading(&self) -> BatteryState {
        self.reading_at(Instant::now())
    }
}

impl BatteryController for VirtualBattery {
    /// A simulated pack has no network to be unreachable over and no partial
    /// write to fail halfway through — `apply` only mutates an in-process
    /// `Mutex`. `Infallible` states that plainly (unlike the test-only
    /// `RecordingBattery`'s `String`, which fails on purpose): the compiler sees `Ok`
    /// is the only outcome, and so does every caller of `actuate`.
    type Error = std::convert::Infallible;

    fn id(&self) -> &DeviceId {
        &self.id
    }

    async fn apply(&self, command: &Command) -> Result<(), Self::Error> {
        self.apply_at(Instant::now(), command);
        Ok(())
    }
}

/// The read side. `prepare` and `poll` are the same call here — there is no
/// wake-into-RAM-mode handshake to run once at startup, because there is no
/// device to wake; a simulated pack is ready to report from the moment it is
/// constructed.
impl BatteryMonitor for VirtualBattery {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn spec(&self) -> &BatterySpec {
        &self.spec
    }

    async fn prepare(&self) -> Result<BatteryReading, PollError> {
        Ok(self.reading_as_battery_reading())
    }

    async fn poll(&self) -> Result<BatteryReading, PollError> {
        Ok(self.reading_as_battery_reading())
    }
}

impl VirtualBattery {
    /// Builds the [`BatteryReading`] `prepare`/`poll` hand back, honest about
    /// what a simulated pack doesn't have. `pack_capacities` is always
    /// `Some` (this model never fails to report them, so `run.rs`'s "keep the last
    /// known set" fallback never engages); no temperatures or `min_soc`, so both read
    /// as the caller's defaults.
    fn reading_as_battery_reading(&self) -> BatteryReading {
        let state = self.reading();
        BatteryReading {
            telemetry: BatteryTelemetry {
                charge: state.current_power.charging(),
                discharge: state.current_power.discharging(),
                pack_capacities: Some(self.packs.clone()),
                pack_temps: Vec::new(),
                enclosure_temp: None,
                min_soc: None,
            },
            state,
            raw: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::time::Duration;

    use super::*;
    use crate::units::PowerCap;

    /// Generous caps so a test can command whatever it wants to without
    /// tripping the spec clamp — that clamp gets its own dedicated test
    /// below instead.
    fn spec() -> BatterySpec {
        BatterySpec {
            max_charge_power: PowerCap::new(5_000),
            max_discharge_power: PowerCap::new(5_000),
        }
    }

    fn battery(capacity_wh: f64, soc: Soc) -> VirtualBattery {
        VirtualBattery::new(
            DeviceId::new("sim"),
            spec(),
            vec![WattHours(capacity_wh)],
            soc,
            Efficiency::new(95.0),
            Efficiency::new(95.0),
        )
    }

    fn hours(h: f64) -> Duration {
        Duration::from_secs_f64(h * 3600.0)
    }

    #[tokio::test]
    async fn charging_for_an_hour_at_95_percent_adds_950_wh() {
        // 1000 W for 1 h = 1000 Wh at the meter; 95% lands in the pack: 1000 * 0.95 =
        // 950 Wh.
        // Capacity is 100,000 Wh, not a rounder 10,000: at 10,000 the SOC
        // lands on an exact x.5 boundary where the tiny float error in
        // `1000.0 * 0.95` (not quite 950.0) flips `Soc::from_fraction`'s rounding — a
        // real edge case, but not the one this test is for.
        let t0 = Instant::now();
        let battery = battery(100_000.0, Soc::new(50));

        battery.apply_at(t0, &Command::SetCharge(Setpoint::new(1_000)));
        let state = battery.reading_at(t0 + hours(1.0));

        let starting_wh = 100_000.0 * 0.5;
        let expected_wh = starting_wh + 950.0;
        assert_eq!(state.soc, Soc::from_fraction(expected_wh / 100_000.0));
        // Unclamped, so the achieved flow is exactly the commanded one.
        assert_eq!(state.current_power, BatteryPower(-1_000));
    }

    #[tokio::test]
    async fn discharging_for_an_hour_at_95_percent_removes_1052_6_wh() {
        // Discharging divides rather than multiplies: the pack must give up
        // more than it delivers. To deliver 1000 Wh at 95% efficiency the
        // pack gives up 1000 / 0.95 = 1052.631... Wh.
        let t0 = Instant::now();
        let battery = battery(100_000.0, Soc::new(50));

        battery.apply_at(t0, &Command::SetDischarge(Setpoint::new(1_000)));
        let state = battery.reading_at(t0 + hours(1.0));

        let starting_wh = 100_000.0 * 0.5;
        let expected_wh = starting_wh - (1_000.0 / 0.95);
        assert_eq!(state.soc, Soc::from_fraction(expected_wh / 100_000.0));
        assert_eq!(state.current_power, BatteryPower(1_000));
    }

    #[tokio::test]
    async fn a_full_round_trip_returns_eta_squared_of_the_energy_put_in() {
        // Charge 1000 W for 1h: 950 Wh stored. Discharging removes `moved /
        // eta`, so `moved = 950*0.95 = 902.5` Wh must leave the meter, taking
        // `902.5/1000` h = 0.9025h = 3249s at 1000W. Energy in 1000 Wh, out
        // 902.5 Wh: ratio 0.9025 = 0.95*0.95 = eta^2 — charging loses one factor of
        // eta, discharging loses another.
        let t0 = Instant::now();
        let battery = battery(1_000_000.0, Soc::new(50)); // capacity high enough never to clamp

        battery.apply_at(t0, &Command::SetCharge(Setpoint::new(1_000)));
        let after_charge = battery.reading_at(t0 + hours(1.0));

        let t1 = t0 + hours(1.0);
        battery.apply_at(t1, &Command::SetDischarge(Setpoint::new(1_000)));
        let drain = Duration::from_secs(3_249);
        let after_discharge = battery.reading_at(t1 + drain);

        let starting_wh = 1_000_000.0 * 0.5;
        let after_charge_wh = starting_wh + 950.0;
        assert_eq!(
            after_charge.soc,
            Soc::from_fraction(after_charge_wh / 1_000_000.0)
        );

        let energy_removed: f64 = 1_000.0 * (3_249.0 / 3_600.0); // meter-side Wh delivered
        assert!((energy_removed - 902.5).abs() < 1e-6);
        let after_discharge_wh = after_charge_wh - 950.0; // returns to the pre-charge level
        assert_eq!(
            after_discharge.soc,
            Soc::from_fraction(after_discharge_wh / 1_000_000.0)
        );

        let round_trip_ratio = energy_removed / 1_000.0;
        assert!((round_trip_ratio - 0.95 * 0.95).abs() < 1e-9);
    }

    #[tokio::test]
    async fn a_full_pack_reports_charge_power_zero() {
        let t0 = Instant::now();
        let battery = battery(10_000.0, Soc::FULL);

        battery.apply_at(t0, &Command::SetCharge(Setpoint::new(1_000)));
        let state = battery.reading_at(t0 + hours(1.0));

        assert_eq!(state.soc, Soc::FULL);
        // Not the commanded 1000 W: the pack had no room, so nothing landed
        // and nothing should be reported as still charging.
        assert_eq!(state.current_power, BatteryPower::ZERO);
    }

    #[tokio::test]
    async fn an_empty_pack_reports_discharge_power_zero() {
        let t0 = Instant::now();
        let battery = battery(10_000.0, Soc::ZERO);

        battery.apply_at(t0, &Command::SetDischarge(Setpoint::new(1_000)));
        let state = battery.reading_at(t0 + hours(1.0));

        assert_eq!(state.soc, Soc::ZERO);
        assert_eq!(state.current_power, BatteryPower::ZERO);
    }

    #[tokio::test]
    async fn no_elapsed_time_mints_nothing() {
        let t0 = Instant::now();
        let battery = battery(10_000.0, Soc::new(50));

        battery.apply_at(t0, &Command::SetCharge(Setpoint::new(1_000)));
        let immediately = battery.reading_at(t0);

        assert_eq!(immediately.soc, Soc::new(50));
    }

    #[tokio::test]
    async fn a_command_change_integrates_the_old_power_up_to_the_change() {
        // Charge for 30 minutes, then switch to discharge. The stored energy
        // at the moment of the switch must reflect 30 minutes of charging —
        // not 0 (the new command backdated) and not 60 (the old command
        // extended past when it actually changed).
        let t0 = Instant::now();
        let battery = battery(1_000_000.0, Soc::new(50));

        battery.apply_at(t0, &Command::SetCharge(Setpoint::new(1_000)));
        let switch_at = t0 + hours(0.5);
        // `apply_at` advances the model to `switch_at` — integrating the old
        // charge command — before installing the discharge command.
        battery.apply_at(switch_at, &Command::SetDischarge(Setpoint::new(1_000)));
        let state = battery.reading_at(switch_at);

        let starting_wh = 1_000_000.0 * 0.5;
        // 1000 W for 0.5 h is 500 Wh at the meter; 95% of that landed.
        let expected_wh = starting_wh + 500.0 * 0.95;
        assert_eq!(state.soc, Soc::from_fraction(expected_wh / 1_000_000.0));
    }

    #[tokio::test]
    async fn a_setpoint_above_the_spec_cap_is_clamped() {
        let t0 = Instant::now();
        let battery = VirtualBattery::new(
            DeviceId::new("sim"),
            BatterySpec {
                max_charge_power: PowerCap::new(2_400),
                max_discharge_power: PowerCap::new(800),
            },
            vec![WattHours(10_000.0)],
            Soc::new(50),
            Efficiency::new(95.0),
            Efficiency::new(95.0),
        );

        battery.apply_at(t0, &Command::SetDischarge(Setpoint::new(5_000)));
        // Read back immediately: the achieved flow before any clamping-to-
        // capacity can kick in should already be the spec's 800 W cap, not
        // the requested 5000 W.
        let state = battery.reading_at(t0);
        assert_eq!(state.current_power, BatteryPower(800));
    }
}
