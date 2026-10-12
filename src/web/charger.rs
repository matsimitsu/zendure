//! The charger as the dashboard shows it: the EV card's detail line and the
//! charger panel, both read from the engine snapshot's `World::charger()` and
//! the engine's verdict on it.

use crate::charger::ChargerState;
use crate::charger::state::{CableState, ChargePointStatus, PhaseMode};
use crate::controller::ChargerStanding;
use crate::units::{Elapsed, MilliAmps, SessionDuration, Timestamp, Volts};

use super::state::DashboardState;
use super::view::{MiniStatView, SignStyle, energy_string, format_kw, format_log_time};

pub enum ChargerPanelView {
    /// No `[charger]` table.
    NotConfigured,
    /// Configured, nothing read yet.
    Waiting,
    Reading(Box<ChargerReadingView>),
}

pub struct ChargerReadingView {
    pub status_label: String,
    /// BEM modifier: "charging" | "idle" | "fault".
    pub badge_variant: &'static str,
    pub power: MiniStatView,
    pub currents: MiniStatView,
    pub voltages: MiniStatView,
    pub phases: MiniStatView,
    pub session_energy: MiniStatView,
    pub session_duration: MiniStatView,
    pub last_seen: MiniStatView,
    /// The callout's text while the controller is holding battery discharge
    /// for the car.
    pub discharge_hold: Option<String>,
}

/// One charger reading and how old it is against the dashboard's clock.
struct Seen<'a> {
    state: &'a ChargerState,
    sampled_at: Timestamp,
    age: Elapsed,
    standing: ChargerStanding,
}

impl Seen<'_> {
    fn offline(&self) -> bool {
        !self.standing.fresh
    }
}

fn seen(state: &DashboardState) -> Option<Seen<'_>> {
    if !state.charger_feed {
        return None;
    }
    let standing = state.charger?;
    let (_, charger, sampled_at) = state.engine.world.charger()?;
    Some(Seen {
        state: charger,
        sampled_at,
        age: state.as_of - sampled_at,
        standing,
    })
}

/// The EV card's line under the car's state of charge; `None` leaves the card
/// to its own reading when there is no charger reading to speak of.
pub(super) fn ev_card_detail(state: &DashboardState) -> Option<String> {
    let seen = seen(state)?;
    Some(if seen.offline() {
        format!("Charger offline ({} min)", minutes(seen.age))
    } else if seen.state.status == ChargePointStatus::Charging {
        format!(
            "Charging · {} kW",
            format_kw(seen.state.power.into_watts(), 1, SignStyle::Magnitude)
        )
    } else if plugged_in(seen.state.cable) {
        "Plugged in".to_string()
    } else {
        "No car".to_string()
    })
}

pub(super) fn charger_panel_view(
    state: &DashboardState,
    timezone: chrono_tz::Tz,
) -> ChargerPanelView {
    if !state.charger_feed {
        return ChargerPanelView::NotConfigured;
    }
    let Some(seen) = seen(state) else {
        return ChargerPanelView::Waiting;
    };
    let charger = seen.state;
    let phases = charger.phases.count();

    let discharge_hold = seen.standing.hold.map(|draw| {
        format!(
            "Battery discharge held — car charging at {} kW",
            format_kw(draw.into_watts(), 1, SignStyle::Magnitude)
        )
    });

    let (status_label, badge_variant) = if seen.offline() {
        ("Offline".to_string(), "fault")
    } else {
        status_badge(charger.status)
    };

    ChargerPanelView::Reading(Box::new(ChargerReadingView {
        status_label,
        badge_variant,
        power: stat(
            "Power",
            format!(
                "{} kW",
                format_kw(charger.power.into_watts(), 2, SignStyle::Magnitude)
            ),
        ),
        currents: stat(
            "Current",
            format!(
                "{} A",
                joined(charger.currents[..phases].iter().map(|&ma| amps(ma)))
            ),
        ),
        voltages: stat(
            "Voltage",
            format!(
                "{} V",
                joined(charger.voltages[..phases].iter().map(|&v| volts(v)))
            ),
        ),
        phases: stat(
            "Phases",
            match charger.phases {
                PhaseMode::One => "1 phase",
                PhaseMode::Three => "3 phases",
            }
            .to_string(),
        ),
        session_energy: stat(
            "Session energy",
            energy_string(charger.session_energy.to_kwh()),
        ),
        session_duration: stat("Session time", duration(charger.session_duration)),
        last_seen: stat(
            "Last seen",
            format_log_time(seen.sampled_at, state.as_of, timezone),
        ),
        discharge_hold,
    }))
}

fn stat(label: &'static str, value: String) -> MiniStatView {
    MiniStatView {
        label,
        value,
        sub: None,
    }
}

fn plugged_in(cable: CableState) -> bool {
    matches!(
        cable,
        CableState::VehicleConnected | CableState::VehicleLocked
    )
}

fn status_badge(status: ChargePointStatus) -> (String, &'static str) {
    let (label, variant) = match status {
        ChargePointStatus::Available => ("Available", "idle"),
        ChargePointStatus::Preparing => ("Preparing", "idle"),
        ChargePointStatus::Charging => ("Charging", "charging"),
        ChargePointStatus::SuspendedEvse => ("Paused by charger", "idle"),
        ChargePointStatus::SuspendedEv => ("Paused by car", "idle"),
        ChargePointStatus::Finishing => ("Finishing", "idle"),
        ChargePointStatus::Reserved => ("Reserved", "idle"),
        ChargePointStatus::Unavailable => ("Unavailable", "fault"),
        ChargePointStatus::Faulted => ("Faulted", "fault"),
        ChargePointStatus::Unknown(code) => return (format!("Status {code}"), "idle"),
    };
    (label.to_string(), variant)
}

fn joined(figures: impl Iterator<Item = String>) -> String {
    figures.collect::<Vec<_>>().join(" · ")
}

fn amps(current: MilliAmps) -> String {
    format!("{:.1}", f64::from(current.0) / 1000.0)
}

fn volts(voltage: Volts) -> String {
    voltage.to_string()
}

fn minutes(age: Elapsed) -> i64 {
    age.as_millis().max(0) / 60_000
}

fn duration(session: SessionDuration) -> String {
    let minutes = session.as_secs() / 60;
    match (minutes / 60, minutes % 60) {
        (0, minutes) => format!("{minutes} min"),
        (hours, minutes) => format!("{hours} h {minutes:02} min"),
    }
}

#[cfg(test)]
#[path = "charger_tests.rs"]
mod tests;
