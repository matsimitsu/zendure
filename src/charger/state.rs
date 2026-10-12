//! What an EV charger reports about itself, as journalled in `world_json`.
//!
//! Vendor-neutral: an adapter maps its own registers onto these, and every
//! status a firmware might add that this build cannot name survives as
//! `Unknown(n)` rather than failing the poll.

use serde::{Deserialize, Serialize};

use crate::units::{ChargerDraw, MilliAmps, SessionDuration, Volts, WattHours};

/// The charge point's own state machine, in OCPP's vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChargePointStatus {
    Available,
    Preparing,
    Charging,
    SuspendedEvse,
    /// Plugged in, but the car has stopped asking for current — usually full.
    SuspendedEv,
    Finishing,
    Reserved,
    Unavailable,
    Faulted,
    Unknown(u16),
}

impl ChargePointStatus {
    /// What Home Assistant's `enum` sensor shows; every status this build
    /// cannot name collapses to `"unknown"`, so the option list stays closed.
    pub const LABELS: &'static [&'static str] = &[
        "available",
        "preparing",
        "charging",
        "suspended_evse",
        "suspended_ev",
        "finishing",
        "reserved",
        "unavailable",
        "faulted",
        "unknown",
    ];

    pub fn label(self) -> &'static str {
        match self {
            ChargePointStatus::Available => "available",
            ChargePointStatus::Preparing => "preparing",
            ChargePointStatus::Charging => "charging",
            ChargePointStatus::SuspendedEvse => "suspended_evse",
            ChargePointStatus::SuspendedEv => "suspended_ev",
            ChargePointStatus::Finishing => "finishing",
            ChargePointStatus::Reserved => "reserved",
            ChargePointStatus::Unavailable => "unavailable",
            ChargePointStatus::Faulted => "faulted",
            ChargePointStatus::Unknown(_) => "unknown",
        }
    }
}

/// What is plugged into what.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CableState {
    NoCable,
    /// The cable is in the charger, but no car is on the other end.
    ChargerOnly,
    VehicleConnected,
    VehicleLocked,
    Unknown(u16),
}

impl CableState {
    /// As [`ChargePointStatus::LABELS`].
    pub const LABELS: &'static [&'static str] = &[
        "no_cable",
        "charger_only",
        "vehicle_connected",
        "vehicle_locked",
        "unknown",
    ];

    pub fn label(self) -> &'static str {
        match self {
            CableState::NoCable => "no_cable",
            CableState::ChargerOnly => "charger_only",
            CableState::VehicleConnected => "vehicle_connected",
            CableState::VehicleLocked => "vehicle_locked",
            CableState::Unknown(_) => "unknown",
        }
    }
}

/// How many phases the charger is drawing on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhaseMode {
    One,
    Three,
}

impl PhaseMode {
    pub fn count(self) -> usize {
        match self {
            PhaseMode::One => 1,
            PhaseMode::Three => 3,
        }
    }
}

/// `phases = 1` or `phases = 3` in a config file; any other count is not a
/// wiring that exists.
impl TryFrom<u8> for PhaseMode {
    type Error = String;

    fn try_from(phases: u8) -> Result<Self, String> {
        match phases {
            1 => Ok(PhaseMode::One),
            3 => Ok(PhaseMode::Three),
            other => Err(format!("phases must be 1 or 3, found {other}")),
        }
    }
}

/// One charger reading. `power` is the charger's own figure, never re-summed
/// from the phases, for the reason `MeterReading::total` gives.
///
/// Only `power` gates a decision, so the display-only fields a firmware may
/// not answer are optional rather than a reason to lose the reading; a
/// missing one journals as `null`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChargerState {
    pub status: ChargePointStatus,
    pub cable: CableState,
    pub power: ChargerDraw,
    pub currents: [MilliAmps; 3],
    pub voltages: [Volts; 3],
    pub phases: Option<PhaseMode>,
    pub session_energy: Option<WattHours>,
    pub session_duration: Option<SessionDuration>,
}

#[cfg(test)]
impl ChargerState {
    /// A car charging on one phase, every field distinct from its default
    /// and from its neighbours, so a round trip that swapped or dropped one
    /// fails rather than matching by coincidence.
    pub(crate) fn test_sample() -> Self {
        ChargerState {
            status: ChargePointStatus::Charging,
            cable: CableState::VehicleLocked,
            power: ChargerDraw::new(3_650),
            currents: [MilliAmps(15_900), MilliAmps(20), MilliAmps(10)],
            voltages: [Volts(229), Volts(231), Volts(233)],
            phases: Some(PhaseMode::One),
            session_energy: Some(WattHours(4_200.5)),
            session_duration: Some(SessionDuration::new(std::time::Duration::from_secs(4_140))),
        }
    }
}
