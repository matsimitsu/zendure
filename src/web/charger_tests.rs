//! The charger's two faces on the dashboard: the EV card's detail line and
//! the panel, including the discharge-hold callout.

use std::time::Duration;

use super::*;

use crate::charger::state::{CableState, ChargePointStatus};
use crate::controller::ChargerGuard;
use crate::engine::EngineState;
use crate::fixtures::journey;
use crate::units::{ChargerDraw, GridPower, SolarPower};
use crate::web::templates::{charger_panel, layout};
use crate::web::view::{PanelDays, dashboard_view};
use crate::world::{DeviceId, Measurement, MeterReading, World};

fn tz() -> chrono_tz::Tz {
    chrono_tz::Europe::Amsterdam
}

fn at(secs: i64) -> Timestamp {
    Timestamp::from_millis(journey::NOW_MS + secs * 1000)
}

fn guard() -> ChargerGuard {
    ChargerGuard {
        threshold: ChargerDraw::new(500),
        max_age: Duration::from_secs(60),
    }
}

/// A dashboard with a charger configured, reading `charger` taken at `sampled`
/// and viewed at `as_of`.
fn state(charger: Option<ChargerState>, sampled: i64, as_of: i64) -> DashboardState {
    let mut world = World::new();
    world.observe_meter(
        None,
        MeterReading::total_only(GridPower(400.0)),
        SolarPower::new(750.0),
    );
    if let Some(charger) = charger {
        world.observe_device(
            DeviceId::new("charger"),
            at(sampled),
            Measurement::Charger(charger),
        );
    }
    let engine = EngineState {
        world,
        controller: crate::controller::Controller::test_default(journey::NOW_MS, journey::DAY)
            .state(),
        mqtt_timed_out: false,
    };
    DashboardState::seed(&engine, vec![], journey::interval_ring(), at(as_of))
        .with_charger(Some(guard()))
}

fn reading(status: ChargePointStatus, cable: CableState, watts: u32) -> ChargerState {
    ChargerState {
        status,
        cable,
        power: ChargerDraw::new(watts),
        ..ChargerState::test_sample()
    }
}

fn ev_detail(state: &DashboardState) -> String {
    dashboard_view(state, PanelDays::default(), tz())
        .stat_cards
        .ev
        .detail
}

fn panel_html(state: &DashboardState) -> String {
    charger_panel::contents(&dashboard_view(state, PanelDays::default(), tz()).charger)
        .into_string()
}

#[test]
fn the_ev_card_says_how_hard_the_car_is_charging() {
    let s = state(
        Some(reading(
            ChargePointStatus::Charging,
            CableState::VehicleLocked,
            7_200,
        )),
        0,
        5,
    );
    assert_eq!(ev_detail(&s), "Charging · 7.2 kW");
}

#[test]
fn the_ev_card_says_plugged_in_when_the_car_is_not_drawing() {
    let s = state(
        Some(reading(
            ChargePointStatus::SuspendedEv,
            CableState::VehicleConnected,
            0,
        )),
        0,
        5,
    );
    assert_eq!(ev_detail(&s), "Plugged in");
}

#[test]
fn the_ev_card_says_no_car_with_nothing_on_the_cable() {
    let s = state(
        Some(reading(
            ChargePointStatus::Available,
            CableState::NoCable,
            0,
        )),
        0,
        5,
    );
    assert_eq!(ev_detail(&s), "No car");
}

#[test]
fn the_ev_card_counts_the_minutes_a_silent_charger_has_been_gone() {
    let s = state(
        Some(reading(
            ChargePointStatus::Charging,
            CableState::VehicleLocked,
            7_200,
        )),
        0,
        12 * 60 + 30,
    );
    assert_eq!(ev_detail(&s), "Charger offline (12 min)");
}

#[test]
fn the_ev_card_keeps_its_own_line_until_the_charger_has_read() {
    assert_eq!(ev_detail(&state(None, 0, 5)), "No vehicle configured");
}

#[test]
fn the_panel_renders_a_reading() {
    let s = state(
        Some(reading(
            ChargePointStatus::Charging,
            CableState::VehicleLocked,
            3_650,
        )),
        0,
        5,
    );
    let html = panel_html(&s);

    assert!(html.contains("charger-panel__badge--charging"), "{html}");
    assert!(html.contains(">3.65 kW<"), "{html}");
    assert!(html.contains(">15.9 A<"), "{html}");
    assert!(html.contains(">229 V<"), "{html}");
    assert!(html.contains(">1 phase<"), "{html}");
    assert!(html.contains(">4.2 kWh<"), "{html}");
    assert!(html.contains(">1 h 09 min<"), "{html}");
    assert!(html.contains("Last seen"), "{html}");
}

#[test]
fn a_three_phase_panel_lists_each_phase() {
    let three = ChargerState {
        phases: PhaseMode::Three,
        ..reading(
            ChargePointStatus::Charging,
            CableState::VehicleLocked,
            3_650,
        )
    };
    let html = panel_html(&state(Some(three), 0, 5));

    assert!(html.contains(">15.9 · 0.0 · 0.0 A<"), "{html}");
    assert!(html.contains(">229 · 231 · 233 V<"), "{html}");
}

#[test]
fn the_panel_says_when_no_charger_is_configured() {
    let mut s = state(None, 0, 5);
    s.charger = None;
    assert!(panel_html(&s).contains("No charger configured"));
}

#[test]
fn the_panel_says_when_a_configured_charger_has_not_read_yet() {
    assert!(panel_html(&state(None, 0, 5)).contains("No charger data yet"));
}

#[test]
fn the_callout_shows_while_the_guard_holds_discharge() {
    let s = state(
        Some(reading(
            ChargePointStatus::Charging,
            CableState::VehicleLocked,
            7_200,
        )),
        0,
        5,
    );
    let html = panel_html(&s);
    assert!(
        html.contains("Battery discharge held — car charging at 7.2 kW"),
        "{html}"
    );
}

#[test]
fn no_callout_below_the_threshold() {
    let s = state(
        Some(reading(
            ChargePointStatus::Charging,
            CableState::VehicleLocked,
            300,
        )),
        0,
        5,
    );
    assert!(!panel_html(&s).contains("discharge held"));
}

#[test]
fn a_stale_reading_reads_offline_and_holds_nothing() {
    let s = state(
        Some(reading(
            ChargePointStatus::Charging,
            CableState::VehicleLocked,
            7_200,
        )),
        0,
        600,
    );
    let html = panel_html(&s);

    assert!(html.contains(">Offline<"), "{html}");
    assert!(!html.contains("discharge held"), "{html}");
}

#[test]
fn the_page_carries_the_panel_in_the_charger_fragment() {
    let s = state(
        Some(reading(
            ChargePointStatus::Charging,
            CableState::VehicleLocked,
            7_200,
        )),
        0,
        5,
    );
    let view = dashboard_view(&s, PanelDays::default(), tz());

    assert!(
        layout::page(&view)
            .into_string()
            .contains("sse-swap=\"charger-panel\"")
    );
}
