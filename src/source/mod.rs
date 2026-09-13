//! Where a meter reading comes from, and nothing about what it means.
//!
//! A source adapter owns one meter's wire format: its JSON, its field names,
//! how many phases it has. What leaves this module is a [`MeterObservation`],
//! which carries none of that.
//!
//! No `trait Source` or dispatch yet, so a second meter still touches
//! `mqtt.rs` (hardcodes `shelly::parse`), `run_subscriber` (takes a `SolarPhase`),
//! `Config`, and `main.rs` — not "one new file".

pub mod shelly;
pub mod synthetic;

use crate::units::SolarPower;
use crate::world::MeterReading;

/// What any meter source produces, whatever its wire format. Solar belongs
/// here because it's read from the same meter — export on the phase the
/// inverter feeds into, since the meter's total nets that against loads
/// elsewhere. A P1 meter reporting production separately fills the same struct from a
/// different field; nothing downstream changes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeterObservation {
    pub grid: MeterReading,
    pub solar: SolarPower,
}
