//! Where a meter reading comes from, and nothing about what it means.
//!
//! A source adapter owns one meter's wire format: its JSON, its field names,
//! its idea of how many phases there are. What leaves this module is a
//! [`MeterObservation`], which carries none of that.
//!
//! What that has actually bought so far: `main.rs` no longer matches on
//! `SolarPhase` to pick a field out of a Shelly DTO, `ShellyReading` moved out
//! of `models.rs`, and `MeterObservation` is a real normalized boundary
//! between wire format and decision. There is no `trait Source` and no
//! dispatch yet, so a second meter is not "one new file": it would still touch
//! `mqtt.rs` (which hardcodes `shelly::parse(...)` in the subscriber and logs
//! `"Shelly: …"` from otherwise meter-agnostic plumbing), `run_subscriber`
//! (which takes a `SolarPhase`, a Shelly concept, as a parameter), `Config`
//! (which holds one), and `main.rs` (which threads it through). That is the
//! honest map for whoever adds a P1 meter next, not a promise that it is free.

pub mod shelly;
pub mod synthetic;

use crate::units::SolarPower;
use crate::world::MeterReading;

/// What any meter source produces, whatever its wire format. Solar belongs
/// here because it is read from this same meter — the export on the phase the
/// inverter feeds into, since the meter's total nets that against loads
/// elsewhere. A P1 meter that reports production separately fills the same
/// struct from a different field; nothing downstream changes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeterObservation {
    pub grid: MeterReading,
    pub solar: SolarPower,
}
