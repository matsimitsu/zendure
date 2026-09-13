//! Wire-format guards for the command path.
//!
//! `Command`'s `Display` string and `ControlDecision`'s JSON both leave the
//! process: the first into the NDJSON journal's `command` field, the second
//! into the journal's `decision` field and, stringified, onto MQTT. Neither had
//! a test before the newtype migration, which made them exactly the bytes most
//! likely to move without anyone noticing, so they are worth pinning permanently.

use super::*;
use crate::units::GridPower;

#[test]
fn display_renders_the_journaled_command_string() {
    // The journal reaches this string through `Directive::describe()`
    // (`src/device.rs`), and README's raw-capture example quotes this exact
    // text.
    assert_eq!(
        Command::SetDischarge(Setpoint::new(145)).to_string(),
        "set_discharge(145W)"
    );
    assert_eq!(
        Command::SetCharge(Setpoint::new(2400)).to_string(),
        "set_charge(2400W)"
    );
    assert_eq!(Command::SetIdle.to_string(), "set_idle");
    assert_eq!(Command::SetStandby.to_string(), "set_standby");
    // Zero is still rendered, not elided.
    assert_eq!(
        Command::SetCharge(Setpoint::ZERO).to_string(),
        "set_charge(0W)"
    );
}

#[test]
fn decision_maps_to_the_command_its_mode_implies() {
    let charge = ControlDecision {
        mode: ControlMode::Charge,
        power_watts: Setpoint::new(740),
        reason: String::new(),
        grid_power: GridPower(-800.0),
    };
    assert_eq!(
        Command::from(&charge),
        Command::SetCharge(Setpoint::new(740))
    );

    // Idle and Standby drop the power entirely rather than sending a zero
    // setpoint in the mode's own shape.
    let idle = ControlDecision {
        mode: ControlMode::Idle,
        power_watts: Setpoint::ZERO,
        reason: String::new(),
        grid_power: GridPower::ZERO,
    };
    assert_eq!(Command::from(&idle), Command::SetIdle);
}

#[test]
fn decision_serializes_to_the_exact_journal_shape() {
    // The NDJSON journal embeds this verbatim. Key names, the capitalized mode
    // tag, the bare integer power and the float grid power are all load-bearing
    // — `serde(transparent)` on Setpoint/GridPower is what keeps them bare.
    let decision = ControlDecision {
        mode: ControlMode::Discharge,
        power_watts: Setpoint::new(145),
        reason: "Grid demand: importing 150W, discharging at 145W (hour 19)".to_string(),
        grid_power: GridPower(150.5),
    };

    assert_eq!(
        serde_json::to_string(&decision).unwrap(),
        r#"{"mode":"Discharge","power_watts":145,"reason":"Grid demand: importing 150W, discharging at 145W (hour 19)","grid_power":150.5}"#
    );
}

#[test]
fn mode_serializes_capitalized_but_displays_lowercase() {
    // These two disagree deliberately and both are live: Serialize feeds the
    // journal, Display feeds MQTT's `decision_mode` topic. Do not "fix" it —
    // aligning them would silently rewrite one of the two channels.
    assert_eq!(
        serde_json::to_string(&ControlMode::Charge).unwrap(),
        r#""Charge""#
    );
    assert_eq!(ControlMode::Charge.to_string(), "charge");
    assert_eq!(ControlMode::Standby.to_string(), "standby");
}

#[test]
fn mode_deserializes_from_the_capitalized_form_already_on_disk() {
    // `Deserialize` was added for the journal's `ControllerState`, which stores
    // `last_mode`. The journal is append-only and there are already capitalized
    // modes recorded, so the round trip has to close on *those* bytes — adding a
    // `rename_all` here would read as a tidy-up and orphan every existing row.
    for mode in [
        ControlMode::Charge,
        ControlMode::Discharge,
        ControlMode::Idle,
        ControlMode::Standby,
    ] {
        let json = serde_json::to_string(&mode).unwrap();
        assert_eq!(mode, serde_json::from_str(&json).unwrap());
    }
    assert_eq!(
        ControlMode::Standby,
        serde_json::from_str::<ControlMode>(r#""Standby""#).unwrap()
    );
}
