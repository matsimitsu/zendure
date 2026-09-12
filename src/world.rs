//! What the controller knows right now, as a projection of the event log.
//!
//! Every decision is a function of a `World` and the controller's own state.
//! Splitting the two is what makes replay sound: fold a recorded event stream
//! into a `World` and you have exactly the inputs a past decision saw, with no
//! configured knob or running timer smuggled in alongside them.
//!
//! Nothing here is a setting and nothing here is a timer — a threshold belongs
//! to `Controller`, a cooldown belongs to whoever counts it down. This file is
//! measurements only, which is also why it is the thing step 7 records as
//! `world_json`.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::battery::BatteryState;
use crate::units::{BatteryPower, GridPower, SolarPower};

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

    /// A reading with no per-phase breakdown, for sources that report only a
    /// net total and for fixtures that predate per-phase capture. Only the test
    /// modules call it today — the live adapters all report three phases — but
    /// it is the constructor a total-only meter would arrive through.
    #[allow(dead_code)]
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
/// plus a device entry" has to avoid. It is also a journal key — step 7's
/// `decisions.device` column — so it must be human-readable and stable across
/// restarts.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DeviceId(String);

impl DeviceId {
    pub fn new(id: impl Into<String>) -> Self {
        DeviceId(id.into())
    }

    /// The id as a plain string, for callers that need one without going
    /// through `Display`. Exercised by the test modules; the decision path and
    /// the journal both reach an id through `Display` or `Serialize` instead.
    #[allow(dead_code)]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        <str as fmt::Display>::fmt(&self.0, f)
    }
}

/// What a device reports about itself. One variant per device class: a
/// charger's measurement (CP state, plugged, session energy) has nothing in
/// common with a battery's, and a flat struct of `Option`s would let the
/// objective ask a battery whether a car is plugged in.
///
/// Internally tagged so a new class is additive on the wire and step 7's
/// `world_json` stays readable: `{"class":"battery","soc":50,...}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "class", rename_all = "snake_case")]
pub enum Measurement {
    Battery(BatteryState),
}

/// A projection of the event log: what the controller knows right now.
/// Measurements only — every knob lives on `Controller`, every timer with its
/// owner. That scope is what makes replay sound, and it is why this is the
/// thing step 7 records as `world_json`.
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
    /// two recorded worlds diffable in step 7.
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
}
