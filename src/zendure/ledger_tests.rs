use super::*;
use crate::units::Setpoint;

/// The device can enter flash mode on its own, and its own report — never a
/// write of ours — is what says so. `ensure_ram_mode` reads this to decide
/// whether a wake is owed before the next charge.
#[test]
fn a_report_is_what_moves_the_tracked_storage_mode() {
    let ledger = Ledger::new();

    ledger.observe_storage_mode(Some(0));
    assert_eq!(ledger.tracked_storage_mode(), StorageMode::Flash);

    ledger.observe_storage_mode(Some(1));
    assert_eq!(ledger.tracked_storage_mode(), StorageMode::Ram);
}

/// A report that says nothing about `smartMode` is not evidence the device
/// left the mode it was in.
#[test]
fn a_report_without_a_smart_mode_leaves_the_tracked_mode_alone() {
    let ledger = Ledger::new();
    ledger.observe_storage_mode(Some(0));

    ledger.observe_storage_mode(None);

    assert_eq!(ledger.tracked_storage_mode(), StorageMode::Flash);
}

#[test]
fn a_fresh_ledger_starts_believing_the_device_is_in_ram_mode() {
    assert!(Ledger::new().is_ram());
}

/// Idle zeroes both tracked caps and forgets the tracked `acMode`, so the
/// next charge or discharge re-sends it.
#[test]
fn record_idle_zeroes_both_caps_and_forgets_ac_mode() {
    let ledger = Ledger::new();
    ledger.record_ac_mode(AcMode::Charge);
    ledger.record_input_limit(Setpoint::new(500));

    ledger.record_idle();

    assert_eq!(
        ledger.tracked_state(),
        DeviceState {
            input_limit: Some(Setpoint::ZERO),
            output_limit: Some(Setpoint::ZERO),
        }
    );
    assert_eq!(ledger.tracked_ac_mode(), None);
}

/// Nothing is tracked until a write of ours lands: a fresh ledger reports
/// both caps and the ac mode as unknown, not zero.
#[test]
fn a_fresh_ledger_tracks_nothing() {
    let ledger = Ledger::new();

    assert_eq!(
        ledger.tracked_state(),
        DeviceState {
            input_limit: None,
            output_limit: None,
        }
    );
    assert_eq!(ledger.tracked_ac_mode(), None);
}

/// `ac_mode_pending` is a read: it doesn't record anything by itself, and
/// stays true until `record_ac_mode` is actually called.
#[test]
fn ac_mode_pending_only_changes_once_recorded() {
    let ledger = Ledger::new();
    assert!(ledger.ac_mode_pending(AcMode::Charge));
    assert!(ledger.ac_mode_pending(AcMode::Charge));

    ledger.record_ac_mode(AcMode::Charge);

    assert!(!ledger.ac_mode_pending(AcMode::Charge));
    assert!(ledger.ac_mode_pending(AcMode::Discharge));
}
