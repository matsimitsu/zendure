use super::*;
use crate::units::Setpoint;

fn unknown() -> DeviceState {
    DeviceState {
        input_limit: None,
        output_limit: None,
    }
}

fn zeroed() -> DeviceState {
    DeviceState {
        input_limit: Some(Setpoint::ZERO),
        output_limit: Some(Setpoint::ZERO),
    }
}

/// Idle and standby are written while the caps are unknown — until a write of
/// ours lands, "unknown" is never treated as "already zero".
#[test]
fn idle_and_standby_are_written_while_state_is_unknown() {
    assert!(needs_write(&Command::SetIdle, unknown()));
    assert!(needs_write(&Command::SetStandby, unknown()));
}

/// Once both caps are known to be zero, idle and standby have nothing left
/// to say.
#[test]
fn idle_and_standby_are_suppressed_once_state_is_zeroed() {
    assert!(!needs_write(&Command::SetIdle, zeroed()));
    assert!(!needs_write(&Command::SetStandby, zeroed()));
}

/// Zeroing the caps is the whole of a standby now, so one still holding a
/// non-zero cap has not been stood down yet.
#[test]
fn standby_is_still_written_while_one_cap_is_not_zero() {
    let state = DeviceState {
        input_limit: Some(Setpoint::new(800)),
        output_limit: Some(Setpoint::ZERO),
    };

    assert!(needs_write(&Command::SetStandby, state));
}

/// The contract this build commits to: flash mode is never commanded, so
/// standby and idle are one request and the guard cannot tell them apart.
#[test]
fn standby_asks_the_device_for_exactly_what_idle_does() {
    for state in [unknown(), zeroed()] {
        assert_eq!(
            needs_write(&Command::SetStandby, state),
            needs_write(&Command::SetIdle, state)
        );
    }
}

/// Charge and discharge carry a setpoint that genuinely changes tick to
/// tick and already write in RAM, so they are never suppressed — not even
/// the degenerate 0 W one that matches a zeroed state.
#[test]
fn a_charge_or_discharge_is_never_suppressed() {
    assert!(needs_write(&Command::SetCharge(Setpoint::ZERO), zeroed()));
    assert!(needs_write(
        &Command::SetDischarge(Setpoint::ZERO),
        zeroed()
    ));
}

#[test]
fn fold_smart_mode_reads_one_as_ram_and_zero_as_flash() {
    assert_eq!(fold_smart_mode(Some(1)), Some(StorageMode::Ram));
    assert_eq!(fold_smart_mode(Some(0)), Some(StorageMode::Flash));
}

/// A missing or unrecognised `smartMode` says nothing about the device's
/// mode, and must change nothing the ledger tracks.
#[test]
fn fold_smart_mode_is_silent_on_anything_else() {
    assert_eq!(fold_smart_mode(None), None);
    assert_eq!(fold_smart_mode(Some(7)), None);
}

#[test]
fn ac_mode_serializes_to_the_wire_numbers_the_device_expects() {
    assert_eq!(serde_json::to_string(&AcMode::Charge).unwrap(), "1");
    assert_eq!(serde_json::to_string(&AcMode::Discharge).unwrap(), "2");
}
