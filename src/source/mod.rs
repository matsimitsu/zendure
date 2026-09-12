//! Where a meter reading comes from, and nothing about what it means.
//!
//! A source adapter owns one meter's wire format: its JSON, its field names,
//! its idea of how many phases there are. What leaves this module is a
//! [`MeterObservation`], which carries none of that — so the day a P1 smart
//! meter replaces the Shelly, it is a new file next to `shelly.rs` and not a
//! single edit anywhere downstream.
//!
//! The split is worth naming because of what it replaced: the coordinator loop
//! used to match on `SolarPhase` to pick a field out of a Shelly DTO, which
//! quietly made `main.rs` know what a Shelly Pro 3EM is.

pub mod shelly;

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
