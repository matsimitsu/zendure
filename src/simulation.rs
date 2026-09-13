//! A simulated battery with a real integrating energy model, so the
//! controller can eventually run against no hardware at all.
//!
//! Not a stub that echoes back what it was told: a controller exercised
//! against one of those would "work" no matter how wrong its decisions were.
//! `VirtualBattery` keeps its own store of [`crate::units::WattHours`] and
//! integrates real power over real time, so a setpoint that would overcharge
//! the pack hits a ceiling and an empty battery really does stop discharging.
//!
//! `registry::from_config` is what selects it: a `[[device]] kind = "virtual"`
//! entry builds one of these instead of a `ZendureClient`, and from that point
//! on `run.rs` reaches it only through [`BatteryController`] and
//! [`BatteryMonitor`], the same as any real device.

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

/// A simulated battery: a rated spec (what it will accept), a set of pack
/// capacities (how much it can hold), and an integrating model of what it
/// currently holds.
///
/// The model lives behind a `Mutex` rather than being a plain field because
/// [`BatteryController::apply`] takes `&self` — the trait's own doc comment
/// explains why, and `ZendureClient` and the test-only `RecordingBattery`
/// hold their mutable state behind the same kind of lock for the same reason.
pub struct VirtualBattery {
    id: DeviceId,
    spec: BatterySpec,
    /// Capacities of the connected packs; their sum is the usable capacity.
    /// Kept as a `Vec` rather than collapsing it to one `WattHours` at
    /// construction because a heterogeneous fleet (a smaller expansion pack
    /// alongside the base unit) is the reason this is a list and not a single
    /// number in the first place — a future change to per-pack behaviour
    /// (different efficiencies, different degradation) has somewhere to hang
    /// without changing this field's shape.
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
    /// never falls behind what was last reported or commanded.
    ///
    /// In order:
    /// 1. The elapsed time. Nothing to do if none has passed.
    /// 2. The energy the last commanded flow moved over that span.
    /// 3. That energy applied to `stored`, with efficiency in the physically
    ///    correct direction.
    /// 4. `stored` clamped into `[0, capacity]`.
    /// 5. The *achieved* flow recomputed from what the clamp actually let
    ///    happen, replacing the commanded flow `model.power` held on entry.
    /// 6. `model.last` moved to `now`.
    fn advance_to(&self, model: &mut Model, now: Instant) {
        // Step 1.
        let dt = now.saturating_duration_since(model.last);
        if dt.is_zero() {
            return;
        }

        // Step 2. Both arguments to `integrate` are the same value —
        // `model.power`, the flow commanded (or last achieved) since
        // `model.last`. That is only a legitimate use of a *trapezoidal*
        // integrator, which is built to handle a changing rate, because the
        // rate here provably did not change across this span: `apply` (below)
        // integrates the old power up to the moment a new command replaces
        // it, and this method is the only other writer of `model.power` —
        // and it runs at the *end* of the span it is integrating, never in
        // the middle. So between the last `model.last` and `now`, `power` was
        // piecewise-constant, and a trapezoid over a constant is exact; it is
        // just a very flat one. A reviewer seeing a trapezoidal call with
        // identical endpoints should read that as "this is a constant", not
        // as a mistake — the constancy is the whole reason it's licensed.
        let moved = WattHours::integrate(model.power.into_watts(), model.power.into_watts(), dt);

        let before = model.stored;
        let capacity = self.capacity();

        // Step 3. Charging adds `moved * eta_charge` to the pack — some of
        // what was drawn is lost to heat before it lands. Discharging removes
        // `moved / eta_discharge` — the pack must give up *more* than it
        // delivers. Getting this backwards is the classic simulated-battery
        // bug: it makes a round trip *gain* energy instead of losing it.
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
        // `stored`, not from the setpoint that was commanded. This is the
        // subtle step and it is not optional: a pack that just clamped to
        // full absorbed less than it was told to, and it has to *report*
        // less — a full pack that keeps claiming its commanded 1200 W of
        // charge tells every future consumer of `flow()` that it is still
        // taking power it has, in fact, stopped accepting. A feedback loop
        // closed around that figure would never converge.
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
    /// [`crate::device::BatteryController::apply`]: everything here is
    /// deterministic given an explicit instant, so tests drive it directly
    /// instead of racing a real clock. `tokio::time::Instant` rather than
    /// `std::time::Instant` for the same reason `rte.rs`'s `Instant` choice
    /// would if it needed replaying: production behaviour is identical, but a
    /// test can run under a paused Tokio clock and integrate a simulated hour
    /// in microseconds instead of actually waiting one.
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
            // are real-hardware conditions this model doesn't produce. A
            // later commit can inject them deliberately once something
            // exercises the controller's handling of them; inventing the
            // knob before that exists would be speculative API with no call
            // site, which is exactly what `units.rs`'s own header warns
            // against.
            soc_calibrating: false,
            soc_limit_reached: soc >= Soc::FULL,
            fault: false,
        }
    }

    /// The flow the battery is currently reporting — the achieved figure
    /// `advance_to` last computed, not necessarily the commanded one. A later
    /// commit closes a feedback loop through this: the objective reads it
    /// back to decide the next setpoint, which is the entire reason a full
    /// pack has to report zero here instead of its stale commanded power.
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
    /// write to fail halfway through — `apply` only ever mutates an in-process
    /// `Mutex`. `Infallible` states that plainly, rather than reaching for a
    /// `String` (as the test-only `RecordingBattery` does, because it *can*
    /// fail on purpose) for an error that can never actually occur: the
    /// compiler can see an `Ok` is the only possible outcome, and so can every
    /// caller of `actuate`.
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
    /// Builds the [`BatteryReading`] `prepare`/`poll` hand back: the model's
    /// state plus telemetry that is honest about what a simulated pack does
    /// not have.
    ///
    /// `pack_capacities` is always `Some` — every tick, not only the first —
    /// because unlike a real device this model never fails to report them;
    /// `run.rs`'s "keep the last known set" fallback exists for a report that
    /// *can* omit them, which this one never does. No temperatures (a
    /// simulated pack generates none) and no `min_soc` (nothing here ever
    /// floors it below zero), so both read as the caller's own defaults
    /// rather than a fabricated number.
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
        // 1000 W held for 1 h is 1000 Wh at the meter (`WattHours::integrate`
        // of a constant, pinned by units_tests.rs's own
        // `watt_hours_integrate_power_over_time`). 95% of that lands in the
        // pack: 1000 * 0.95 = 950 Wh.
        //
        // Capacity is 100,000 Wh rather than a rounder 10,000: at 10,000 the
        // resulting SOC lands on an exact x.5 boundary, where the tiny
        // floating-point error `1000.0 * 0.95` actually carries (it is not
        // quite 950.0) flips which way `Soc::from_fraction`'s rounding goes —
        // a real edge case, but not the one this test is for.
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
        // Charge 1000 W for 1 h: 1000 Wh drawn, 950 Wh actually stored (the
        // previous test's arithmetic). Then discharge long enough to remove
        // exactly those 950 Wh again: discharging removes `moved / eta`, so
        // `moved = 950 * 0.95 = 902.5` Wh must leave at the meter, which at
        // 1000 W takes `902.5 / 1000` h = 0.9025 h = 3249 s.
        //
        // Energy in was 1000 Wh; energy out was 902.5 Wh. The ratio,
        // 902.5 / 1000 = 0.9025, is exactly 0.95 * 0.95 = eta^2 — charging
        // loses one factor of eta, discharging loses another.
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
