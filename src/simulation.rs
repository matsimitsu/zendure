//! A simulated battery with a real integrating energy model, so the
//! controller can run against no hardware at all. Not a stub that echoes
//! back what it was told: it keeps its own store of [`crate::units::WattHours`]
//! and integrates real power over real time, so an overcharging setpoint hits
//! a ceiling and an empty battery really does stop discharging.
//!
//! `[[device]] kind = "virtual"` selects it; `run.rs` reaches it only through
//! [`BatteryController`] and [`BatteryMonitor`], like any device.

use std::sync::Mutex;

use tokio::time::{Duration, Instant};

use crate::battery::BatteryState;
use crate::command::Command;
use crate::device::{
    BatteryController, BatteryMonitor, BatteryReading, BatterySpec, BatteryTelemetry, PollError,
};
use crate::sync::guard;
use crate::units::{BatteryPower, Efficiency, Setpoint, Soc, WattHours, Watts};
use crate::world::DeviceId;

/// How fast power is physically able to change, in watts per second. Neither
/// a power nor a duration, so neither `Watts` nor `Duration`.
#[derive(Debug, Clone, Copy)]
struct PowerSlewRate(u32);

impl PowerSlewRate {
    /// `const fn`, so a caller's `const` rejects a zero rate — a setpoint that
    /// could never be reached — at compile time rather than stalling at run time.
    const fn watts_per_second(watts: u32) -> Self {
        assert!(watts > 0, "a slew rate of zero never reaches any setpoint");
        PowerSlewRate(watts)
    }

    fn as_f64(self) -> f64 {
        f64::from(self.0)
    }
}

/// The AC2400's full 2400 W range in the ~3 s Zendure publishes as its response time.
const SLEW_RATE: PowerSlewRate = PowerSlewRate::watts_per_second(800);

/// A signed flow, positive discharging and negative charging like
/// [`BatteryPower`], but unrounded: whole watts in the model's own state would
/// make what it says depend on how often it was read.
#[derive(Debug, Clone, Copy)]
struct Flow(f64);

impl Flow {
    const ZERO: Flow = Flow(0.0);

    fn reported(self) -> BatteryPower {
        BatteryPower(self.0.round() as i32)
    }
}

/// A span's energy split by direction, as magnitudes. A span that reverses has
/// both, and each half meets its own efficiency.
#[derive(Debug, Clone, Copy, Default)]
struct MovedEnergy {
    charged: WattHours,
    discharged: WattHours,
}

/// Where the flow set off from, when, and what it is closing on at
/// [`SLEW_RATE`] — a function of absolute time rather than an accumulator, so
/// no read cadence can change the answer.
#[derive(Debug, Clone, Copy)]
struct Ramp {
    origin: Flow,
    started: Instant,
    target: Flow,
}

impl Ramp {
    fn held(flow: Flow, at: Instant) -> Self {
        Ramp {
            origin: flow,
            started: at,
            target: flow,
        }
    }

    /// The flow at an instant: the origin closed toward the target, and held
    /// there once it arrives.
    fn at(&self, instant: Instant) -> Flow {
        let travelled = SLEW_RATE.as_f64()
            * instant
                .saturating_duration_since(self.started)
                .as_secs_f64();
        let gap = self.target.0 - self.origin.0;
        Flow(self.origin.0 + gap.clamp(-travelled, travelled))
    }

    fn arrival(&self) -> Instant {
        let seconds = (self.target.0 - self.origin.0).abs() / SLEW_RATE.as_f64();
        self.started + Duration::from_secs_f64(seconds)
    }

    /// When the flow passes through zero, if it does so before arriving.
    fn zero_crossing(&self) -> Option<Instant> {
        let gap = self.target.0 - self.origin.0;
        let seconds = -self.origin.0 / (SLEW_RATE.as_f64() * gap.signum());
        (seconds > 0.0 && seconds < gap.abs() / SLEW_RATE.as_f64())
            .then(|| self.started + Duration::from_secs_f64(seconds))
    }

    /// The energy moved between two instants, charge and discharge sides kept
    /// apart. Cut where the flow arrives and where it crosses zero: a
    /// trapezoid is exact only over a straight line that keeps its sign.
    fn moved(&self, from: Instant, to: Instant) -> MovedEnergy {
        let mut cuts: Vec<Instant> = [Some(self.arrival()), self.zero_crossing()]
            .into_iter()
            .flatten()
            .filter(|cut| *cut > from && *cut < to)
            .collect();
        cuts.sort_unstable();

        let (mut charged, mut discharged) = (0.0, 0.0);
        let mut start = from;
        for end in cuts.into_iter().chain(std::iter::once(to)) {
            let energy = self.straight_line(start, end);
            if energy < 0.0 {
                charged -= energy;
            } else {
                discharged += energy;
            }
            start = end;
        }

        MovedEnergy {
            charged: WattHours(charged),
            discharged: WattHours(discharged),
        }
    }

    /// Watt-hours under a straight segment. In `f64` rather than through
    /// [`WattHours::integrate`], whose whole-watt endpoints would put the
    /// cadence dependence back.
    fn straight_line(&self, from: Instant, to: Instant) -> f64 {
        let hours = to.saturating_duration_since(from).as_secs_f64() / 3600.0;
        (self.at(from).0 + self.at(to).0) / 2.0 * hours
    }
}

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
    /// The flow last reported: what the ramp had reached, or what a clamp let
    /// happen instead.
    power: BatteryPower,
    ramp: Ramp,
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
        let start = Instant::now();
        VirtualBattery {
            id,
            spec,
            packs,
            charge_efficiency,
            discharge_efficiency,
            model: Mutex::new(Model {
                stored,
                power: BatteryPower::ZERO,
                ramp: Ramp::held(Flow::ZERO, start),
                last: start,
                standby: false,
            }),
        }
    }

    fn capacity(&self) -> WattHours {
        self.packs.iter().copied().sum()
    }

    /// Integrates the model forward from `model.last` to `now`, in place.
    /// Every public read or write goes through this first, so `model.last`
    /// never falls behind.
    fn advance_to(&self, model: &mut Model, now: Instant) {
        let dt = now.saturating_duration_since(model.last);
        if dt.is_zero() {
            return;
        }

        let before = model.stored;
        let capacity = self.capacity();
        let after = self.settle(before, model.ramp.moved(model.last, now));
        let clamped = WattHours(after.get().clamp(0.0, capacity.get()));

        model.power = if (0.0..=capacity.get()).contains(&after.get()) {
            model.ramp.at(now).reported()
        } else {
            self.achieved_flow(WattHours(clamped.get() - before.get()), dt)
        };
        model.stored = clamped;
        model.last = now;
    }

    /// What a span's energy does to the pack: charging loses heat before it
    /// lands, discharging gives up more than it delivers. Backwards, a round
    /// trip would *gain* energy instead of losing it.
    fn settle(&self, before: WattHours, moved: MovedEnergy) -> WattHours {
        WattHours(
            before.get() + moved.charged.get() * self.charge_efficiency.fraction()
                - moved.discharged.get() / self.discharge_efficiency.fraction(),
        )
    }

    /// Inverts [`VirtualBattery::settle`]: the meter-side flow that would have
    /// produced only the energy that actually landed. A pack the clamp bit
    /// reporting its commanded setpoint instead would tell every `flow()`
    /// consumer a full pack is still charging, and a loop closed on that
    /// figure would never converge.
    fn achieved_flow(&self, landed: WattHours, dt: Duration) -> BatteryPower {
        let meter_side = if landed.get() > 0.0 {
            WattHours(-landed.get() / self.charge_efficiency.fraction())
        } else {
            WattHours(-landed.get() * self.discharge_efficiency.fraction())
        };
        BatteryPower(meter_side.over(dt).get())
    }

    /// The commanded setpoint clamped to the spec's cap, not trusted from the
    /// caller: `Setpoint` itself only guarantees non-negative, and this is the
    /// boundary where "what was asked for" becomes "what this box can do".
    fn commanded_flow(&self, command: &Command) -> Flow {
        match command {
            Command::SetCharge(setpoint) => {
                let watts = Setpoint::clamped(Watts(setpoint.get()), self.spec.max_charge_power);
                Flow(-f64::from(watts.get()))
            }
            Command::SetDischarge(setpoint) => {
                let watts = Setpoint::clamped(Watts(setpoint.get()), self.spec.max_discharge_power);
                Flow(f64::from(watts.get()))
            }
            Command::SetIdle | Command::SetStandby => Flow::ZERO,
        }
    }

    /// The test seam behind [`VirtualBattery::flow`] and
    /// [`crate::device::BatteryController::apply`]: deterministic given an
    /// explicit instant, so tests drive it directly instead of racing a real
    /// clock. `tokio::time::Instant`, not `std::time::Instant`, lets a test run under a
    /// paused Tokio clock and integrate a simulated hour in microseconds.
    pub(crate) fn apply_at(&self, now: Instant, command: &Command) {
        let mut model = guard(&self.model);

        // The span just ended belongs to the target that was in force for it.
        self.advance_to(&mut model, now);

        model.standby = matches!(command, Command::SetStandby);
        model.ramp = Ramp {
            origin: model.ramp.at(now),
            started: now,
            target: self.commanded_flow(command),
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

    /// The test seam behind [`VirtualBattery::flow`]. See `apply_at` for why
    /// this takes an explicit instant rather than reading the clock.
    pub(crate) fn flow_at(&self, now: Instant) -> BatteryPower {
        let mut model = guard(&self.model);
        self.advance_to(&mut model, now);
        model.power
    }

    /// The flow the battery is currently reporting — what `advance_to` last
    /// computed, which both a ramp and a clamp can hold short of the commanded
    /// setpoint. The objective reads this back to decide the next setpoint, which is
    /// why a full pack must report zero here instead of its stale commanded power.
    pub fn flow(&self) -> BatteryPower {
        self.flow_at(Instant::now())
    }

    pub fn reading(&self) -> BatteryState {
        self.reading_at(Instant::now())
    }

    /// The pack's energy content, for tests asserting on the integrator at a
    /// finer resolution than whole-percent [`Soc`].
    #[cfg(test)]
    pub(crate) fn stored_at(&self, now: Instant) -> WattHours {
        let mut model = guard(&self.model);
        self.advance_to(&mut model, now);
        model.stored
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
#[path = "simulation_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "scenario_tests.rs"]
mod scenario_tests;
