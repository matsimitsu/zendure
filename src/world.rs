//! What the controller knows right now, as a projection of the event log.
//!
//! Every decision is a function of a `World` and the controller's own state.
//! Splitting the two is what makes replay sound: fold a recorded event stream
//! into a `World` and you have exactly the inputs a past decision saw, with no
//! configured knob or running timer smuggled in alongside them.
//!
//! Nothing here is a setting and nothing here is a timer — a threshold belongs
//! to `Controller`, a cooldown belongs to whoever counts it down. This file is
//! measurements only, which is what is journalled as `world_json`.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::battery::BatteryState;
use crate::units::{BatteryPower, GridPower, SolarPower, Watts, forward_display};

/// One meter observation: the signed net total plus each phase. `total` is the
/// meter's own `total_act_power`, never re-summed from the phases — every
/// threshold was tuned against that number, and the device is not required to
/// make them agree.
///
/// Per-phase is carried and journaled but never decided on. "One battery per
/// phase" later reads `phases[i]`; the data is on record from today.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MeterReading {
    pub total: GridPower,
    pub phases: [GridPower; 3],
}

impl MeterReading {
    pub const ZERO: MeterReading = MeterReading {
        total: GridPower::ZERO,
        phases: [GridPower::ZERO; 3],
    };

    pub fn new(total: GridPower, phases: [GridPower; 3]) -> Self {
        MeterReading { total, phases }
    }

    /// A reading with no per-phase breakdown, built by `controller.rs`'s and
    /// `engine.rs`'s test modules where a test only cares about the net total.
    /// The live adapters all report three phases, so this has no production
    /// caller — `#[cfg(test)]` rather than `#[allow(dead_code)]` makes that
    /// enforced instead of merely claimed: a production caller would fail to
    /// compile, and promoting it out of test-only is then a deliberate edit.
    #[cfg(test)]
    pub fn total_only(total: GridPower) -> Self {
        MeterReading {
            total,
            phases: [GridPower::ZERO; 3],
        }
    }
}

/// Hand-written rather than derived: `GridPower` has no `Default`, deliberately
/// — a signed flow with no reading behind it is not obviously zero. Here it is:
/// a meter we have not heard from reads nothing, which is what every threshold
/// comparison already treats an absent import or export as.
impl Default for MeterReading {
    fn default() -> Self {
        MeterReading::ZERO
    }
}

/// A device's stable identity — the Zendure's serial today, a charger's own
/// later. `String` rather than `&'static str` because it comes from
/// `ZENDURE_SN` at runtime; not an enum because that would make the device set
/// a compile-time constant, which is exactly what "a second battery is config
/// plus a device entry" has to avoid. It is also a journal key — the
/// `decisions.device` column — so it must be human-readable and stable across
/// restarts.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DeviceId(String);

impl DeviceId {
    pub fn new(id: impl Into<String>) -> Self {
        DeviceId(id.into())
    }
}

forward_display!(DeviceId, str);

/// What a device reports about itself. One variant per device class: a
/// charger's measurement (CP state, plugged, session energy) has nothing in
/// common with a battery's, and a flat struct of `Option`s would let the
/// objective ask a battery whether a car is plugged in.
///
/// Internally tagged so a new class is additive on the wire and
/// `world_json` stays readable: `{"class":"battery","soc":50,...}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "class", rename_all = "snake_case")]
pub enum Measurement {
    Battery(BatteryState),
}

/// A projection of the event log: what the controller knows right now.
/// Measurements only — every knob lives on `Controller`, every timer with its
/// owner. That scope is what makes replay sound, and what is journalled as
/// `world_json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct World {
    pub grid: MeterReading,
    pub solar: SolarPower,
    /// Private, and the accessors below are the only way in, so the controller
    /// never learns there is a map here at all.
    ///
    /// With one device a map makes "no battery" representable, which a bare
    /// `BatteryState` field did not. That is the point rather than a cost:
    /// `battery()` returns `Option`, and the controller turns `None` into the
    /// *existing* "no decision this tick" path instead of a panic.
    ///
    /// `BTreeMap` rather than `Vec` because `Event::DeviceUpdate` addresses a
    /// device by id, so the fold is one `insert` with no scan and no duplicate
    /// entry to reconcile — and because sorted, stable key order is what makes
    /// two recorded worlds diffable.
    devices: BTreeMap<DeviceId, Measurement>,
}

/// Hand-written for the same reason as `MeterReading`'s: `SolarPower` has no
/// `Default` either. Zero is right for both — an unobserved world imports
/// nothing and produces nothing, so a controller that somehow decided against
/// one would idle rather than act on a number nobody measured.
impl Default for World {
    fn default() -> Self {
        World {
            grid: MeterReading::ZERO,
            solar: SolarPower::ZERO,
            devices: BTreeMap::new(),
        }
    }
}

impl World {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn observe_meter(&mut self, grid: MeterReading, solar: SolarPower) {
        self.grid = grid;
        self.solar = solar;
    }

    pub fn observe_device(&mut self, id: DeviceId, measurement: Measurement) {
        self.devices.insert(id, measurement);
    }

    /// Every device that is a battery, in id order.
    ///
    /// The `match` is exhaustive today because every device is a battery, and
    /// that is deliberately left as the tripwire: the day a charger variant is
    /// added this stops compiling here, at the one place that would otherwise
    /// have quietly handed the objective a car charger to discharge.
    pub fn batteries(&self) -> impl Iterator<Item = (&DeviceId, &BatteryState)> {
        self.devices
            .iter()
            .map(|(id, measurement)| match measurement {
                Measurement::Battery(state) => (id, state),
            })
    }

    /// The single battery this objective is written for. `None` when none is
    /// registered; the lowest id when — impossibly today — there are two.
    pub fn battery(&self) -> Option<&BatteryState> {
        self.batteries().next().map(|(_, state)| state)
    }

    /// Combined battery flow. Positive = discharging into the house.
    pub fn battery_flow(&self) -> BatteryPower {
        self.batteries().map(|(_, state)| state.current_power).sum()
    }

    /// What the meter would read if every battery were idle — the figure the
    /// objective actually decides on. Storage only: a charger's draw is genuine
    /// demand and must not be subtracted out.
    pub fn underlying_grid(&self) -> GridPower {
        self.grid.total + self.battery_flow()
    }

    /// What the house itself is drawing. Not a decision input.
    ///
    /// The meter nets both solar and the battery out of its total
    /// (`grid.total = load - solar - battery_flow`), so the load is both of
    /// them added back; [`underlying_grid`](Self::underlying_grid) is already
    /// the battery half.
    pub fn home_usage(&self) -> Watts {
        self.underlying_grid().importing() + self.solar.into_watts()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::battery::BatteryState;
    use crate::units::{BatteryPower, PowerCap, Soc};

    /// Every field distinct from its default and from every other field of
    /// the same type, so a round-trip that silently dropped, swapped or
    /// defaulted one would show up as an equality failure rather than hide
    /// behind a coincidental match — three different phase readings, a phase
    /// total that agrees with none of them, non-zero solar, and a battery
    /// whose flags disagree with each other.
    fn sample_world() -> World {
        let mut world = World::new();
        world.observe_meter(
            MeterReading::new(
                GridPower(150.5),
                [GridPower(10.0), GridPower(-200.0), GridPower(340.5)],
            ),
            SolarPower::new(200.0),
        );
        world.observe_device(
            DeviceId::new("SN123"),
            Measurement::Battery(BatteryState {
                soc: Soc::new(50),
                max_discharge_power: PowerCap::new(800),
                max_charge_power: PowerCap::new(2400),
                current_power: BatteryPower(-300),
                soc_calibrating: true,
                soc_limit_reached: false,
                fault: true,
            }),
        );
        world
    }

    /// Pins the exact wire shape, the same way `command_tests.rs` pins
    /// `Command`'s `Display` and `ControlDecision`'s JSON: `Measurement` is
    /// internally tagged (`"class":"battery"`, not a wrapper object) and
    /// `DeviceId` is a bare string key rather than `{"0":"SN123"}`. Both are
    /// what keeps the journal's `world_json` readable. Do not "fix" this if it
    /// starts failing — a diff here means the format actually moved, which is
    /// exactly what this test exists to catch.
    ///
    /// `a_restored_engine_resumes_the_fold_exactly` proves a `World` survives a
    /// round trip, which says nothing about what the bytes look like. The
    /// journal is append-only, so the shape is a compatibility
    /// contract with rows already written, and round-trip equality would hold
    /// just as well after a rename that orphaned every one of them. Its
    /// companion round-trip test genuinely was superseded, and is gone.
    #[test]
    fn serializes_to_the_exact_pinned_shape() {
        let world = sample_world();
        assert_eq!(
            serde_json::to_string(&world).unwrap(),
            r#"{"grid":{"total":150.5,"phases":[10.0,-200.0,340.5]},"solar":200.0,"devices":{"SN123":{"class":"battery","soc":50,"max_discharge_power":800,"max_charge_power":2400,"current_power":-300,"soc_calibrating":true,"soc_limit_reached":false,"fault":true}}}"#
        );
    }

    /// A battery otherwise identical to `sample_world`'s, distinguished only by
    /// `current_power` — the one field `battery_flow` and `underlying_grid`
    /// read.
    fn battery_with_power(current_power: BatteryPower) -> BatteryState {
        BatteryState {
            soc: Soc::new(50),
            max_discharge_power: PowerCap::new(800),
            max_charge_power: PowerCap::new(2400),
            current_power,
            soc_calibrating: false,
            soc_limit_reached: false,
            fault: false,
        }
    }

    /// Zero batteries net to zero, not a panic or a default from summing an
    /// empty iterator over some other type.
    #[test]
    fn battery_flow_with_no_batteries_is_zero() {
        let world = World::new();
        assert_eq!(world.battery_flow(), BatteryPower::ZERO);
    }

    #[test]
    fn battery_flow_with_one_battery_is_its_own_power() {
        let mut world = World::new();
        world.observe_device(
            DeviceId::new("SN1"),
            Measurement::Battery(battery_with_power(BatteryPower(-450))),
        );
        assert_eq!(world.battery_flow(), BatteryPower(-450));
    }

    /// Mixed signs: two discharging (positive), one charging (negative). A
    /// naive count or an accidental `abs` would both pass a same-sign fixture;
    /// this one only passes if the signs are actually summed.
    #[test]
    fn battery_flow_sums_mixed_signs_across_several_batteries() {
        let mut world = World::new();
        world.observe_device(
            DeviceId::new("SN1"),
            Measurement::Battery(battery_with_power(BatteryPower(-300))),
        );
        world.observe_device(
            DeviceId::new("SN2"),
            Measurement::Battery(battery_with_power(BatteryPower(500))),
        );
        world.observe_device(
            DeviceId::new("SN3"),
            Measurement::Battery(battery_with_power(BatteryPower(-150))),
        );
        assert_eq!(world.battery_flow(), BatteryPower(50));
    }

    /// `underlying_grid` has to be the meter total plus *every* battery's
    /// flow, not just the first one it happens to iterate. The first battery
    /// by id ("SN1") is charging at -300 W; if `underlying_grid` used only
    /// that battery it would read 1000 + (-300) = 700 W. The other two
    /// batteries add another 350 W of net flow, so the right answer, 1050 W,
    /// is one a first-battery-only implementation cannot produce.
    #[test]
    fn underlying_grid_sums_meter_and_every_batterys_flow() {
        let mut world = World::new();
        world.observe_meter(
            MeterReading::total_only(GridPower(1000.0)),
            SolarPower::ZERO,
        );
        world.observe_device(
            DeviceId::new("SN1"),
            Measurement::Battery(battery_with_power(BatteryPower(-300))),
        );
        world.observe_device(
            DeviceId::new("SN2"),
            Measurement::Battery(battery_with_power(BatteryPower(200))),
        );
        world.observe_device(
            DeviceId::new("SN3"),
            Measurement::Battery(battery_with_power(BatteryPower(150))),
        );

        assert_eq!(world.underlying_grid(), GridPower(1050.0));
    }

    /// 1000 W still imported + 200 W of solar + 300 W out of the pack = a
    /// house drawing 1500 W.
    #[test]
    fn home_usage_adds_solar_and_battery_flow_back_onto_the_meter() {
        let mut world = World::new();
        world.observe_meter(
            MeterReading::total_only(GridPower(1000.0)),
            SolarPower::new(200.0),
        );
        world.observe_device(
            DeviceId::new("SN1"),
            Measurement::Battery(battery_with_power(BatteryPower(300))),
        );
        assert_eq!(world.home_usage(), Watts(1500));
    }

    /// The pack is covering the whole house, so the meter reads zero and every
    /// watt drawn comes from the battery — the case a flipped sign turns
    /// negative.
    #[test]
    fn home_usage_is_positive_when_the_battery_covers_the_whole_house() {
        let mut world = World::new();
        world.observe_meter(MeterReading::total_only(GridPower::ZERO), SolarPower::ZERO);
        world.observe_device(
            DeviceId::new("SN1"),
            Measurement::Battery(battery_with_power(BatteryPower(800))),
        );
        assert_eq!(world.home_usage(), Watts(800));
    }

    /// Charging is demand like any other: 1500 W imported with 1000 W of it
    /// going into the pack leaves 500 W for the house.
    #[test]
    fn home_usage_excludes_what_the_battery_is_charging() {
        let mut world = World::new();
        world.observe_meter(
            MeterReading::total_only(GridPower(1500.0)),
            SolarPower::ZERO,
        );
        world.observe_device(
            DeviceId::new("SN1"),
            Measurement::Battery(battery_with_power(BatteryPower(-1000))),
        );
        assert_eq!(world.home_usage(), Watts(500));
    }

    /// Exporting: 2000 W of solar, 1200 W pushed to the grid, battery idle —
    /// the house is drawing the 800 W difference.
    #[test]
    fn home_usage_while_exporting_is_solar_minus_the_export() {
        let mut world = World::new();
        world.observe_meter(
            MeterReading::total_only(GridPower(-1200.0)),
            SolarPower::new(2000.0),
        );
        world.observe_device(
            DeviceId::new("SN1"),
            Measurement::Battery(battery_with_power(BatteryPower::ZERO)),
        );
        assert_eq!(world.home_usage(), Watts(800));
    }

    /// The round trip against `source::synthetic`'s model
    /// (`grid.total = load - solar - battery_flow`): whatever load goes in
    /// comes back out, for every combination of sign.
    #[test]
    fn home_usage_inverts_the_meter_model_for_every_sign() {
        for load in [0, 400, 3000] {
            for solar in [0.0, 250.0, 4000.0] {
                for flow in [-2400, 0, 1800] {
                    let total = f64::from(load) - solar - f64::from(flow);
                    let mut world = World::new();
                    world.observe_meter(
                        MeterReading::total_only(GridPower(total)),
                        SolarPower::new(solar),
                    );
                    world.observe_device(
                        DeviceId::new("SN1"),
                        Measurement::Battery(battery_with_power(BatteryPower(flow))),
                    );
                    assert_eq!(
                        world.home_usage(),
                        Watts(load),
                        "load={load} solar={solar} flow={flow}"
                    );
                }
            }
        }
    }

    #[test]
    fn battery_of_an_empty_world_is_none() {
        let world = World::new();
        assert!(world.battery().is_none());
    }

    /// Registered out of id order, so an implementation reading insertion
    /// order (a `Vec`, or the first key an unsorted map happens to yield)
    /// would return "battery-c" here instead of the lowest id.
    #[test]
    fn battery_returns_the_lowest_device_id() {
        let mut world = World::new();
        world.observe_device(
            DeviceId::new("battery-c"),
            Measurement::Battery(battery_with_power(BatteryPower(300))),
        );
        world.observe_device(
            DeviceId::new("battery-a"),
            Measurement::Battery(battery_with_power(BatteryPower(-100))),
        );
        world.observe_device(
            DeviceId::new("battery-b"),
            Measurement::Battery(battery_with_power(BatteryPower(200))),
        );

        assert_eq!(world.battery().unwrap().current_power, BatteryPower(-100));
    }
}
