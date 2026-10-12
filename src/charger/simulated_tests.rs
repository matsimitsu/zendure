use chrono_tz::Europe::Amsterdam;

use super::*;

const AMPS_16: MilliAmps = MilliAmps(16_000);

fn time(hour: u32, minute: u32) -> TimeOfDay {
    TimeOfDay::new(hour, minute).unwrap()
}

/// An evening-to-morning window, so most of these cross midnight.
fn overnight(timezone: Tz, phases: PhaseMode, target: WattHours) -> ChargeProfile {
    ChargeProfile::new(time(18, 0), time(7, 0), AMPS_16, phases, target, timezone)
}

/// A target no test reaches unless it means to.
const UNREACHED: WattHours = WattHours(1_000_000.0);

fn clock_in(timezone: Tz, (y, mo, d): (i32, u32, u32), hour: u32, minute: u32) -> Clock {
    let local = timezone
        .with_ymd_and_hms(y, mo, d, hour, minute, 0)
        .single()
        .expect("an unambiguous local time");
    Clock {
        now: Timestamp::from(local),
        ..Clock::test_at(0)
    }
}

fn utc(hour: u32, minute: u32) -> Clock {
    clock_in(chrono_tz::UTC, (2026, 6, 15), hour, minute)
}

#[test]
fn before_the_window_no_car_is_plugged_in() {
    let state = overnight(chrono_tz::UTC, PhaseMode::Three, UNREACHED).at(&utc(17, 59));

    assert_eq!(state.status, ChargePointStatus::Available);
    assert_eq!(state.cable, CableState::NoCable);
    assert_eq!(state.power, ChargerDraw::ZERO);
    assert_eq!(state.currents, [MilliAmps::ZERO; 3]);
    assert_eq!(state.session_energy, Some(WattHours::ZERO));
    assert_eq!(state.session_duration, Some(SessionDuration::ZERO));
}

/// 3 × 16 A × 230 V, half an hour in.
#[test]
fn during_the_window_three_phases_draw_their_current_on_every_phase() {
    let state = overnight(chrono_tz::UTC, PhaseMode::Three, UNREACHED).at(&utc(18, 30));

    assert_eq!(state.status, ChargePointStatus::Charging);
    assert_eq!(state.cable, CableState::VehicleLocked);
    assert_eq!(state.currents, [AMPS_16; 3]);
    assert_eq!(state.voltages, [Volts(230); 3]);
    assert_eq!(state.power, ChargerDraw::new(11_040));
    assert_eq!(state.phases, Some(PhaseMode::Three));
    assert_eq!(state.session_energy, Some(WattHours(5_520.0)));
    assert_eq!(
        state.session_duration,
        Some(SessionDuration::new(Duration::from_secs(30 * 60)))
    );
}

#[test]
fn one_phase_draws_on_the_first_phase_only() {
    let state = overnight(chrono_tz::UTC, PhaseMode::One, UNREACHED).at(&utc(19, 0));

    assert_eq!(state.currents, [AMPS_16, MilliAmps::ZERO, MilliAmps::ZERO]);
    assert_eq!(state.power, ChargerDraw::new(3_680));
    assert_eq!(state.session_energy, Some(WattHours(3_680.0)));
}

/// 11,040 W reaches 11,040 Wh after one hour exactly, and the car then stops
/// drawing while staying plugged in.
#[test]
fn once_the_target_is_reached_the_car_suspends() {
    let profile = overnight(chrono_tz::UTC, PhaseMode::Three, WattHours(11_040.0));

    assert_eq!(profile.at(&utc(18, 59)).status, ChargePointStatus::Charging);

    let state = profile.at(&utc(21, 0));
    assert_eq!(state.status, ChargePointStatus::SuspendedEv);
    assert_eq!(state.cable, CableState::VehicleLocked);
    assert_eq!(state.power, ChargerDraw::ZERO);
    assert_eq!(state.currents, [MilliAmps::ZERO; 3]);
    assert_eq!(state.session_energy, Some(WattHours(11_040.0)));
    assert_eq!(
        state.session_duration,
        Some(SessionDuration::new(Duration::from_secs(3 * 3600)))
    );
}

/// The session seen at 02:00 began at 18:00 the evening before, not at 18:00
/// on a day that has not happened yet.
#[test]
fn a_window_that_wraps_midnight_carries_the_session_across_it() {
    let profile = overnight(chrono_tz::UTC, PhaseMode::One, UNREACHED);

    let state = profile.at(&utc(2, 0));
    assert_eq!(state.status, ChargePointStatus::Charging);
    assert_eq!(
        state.session_duration,
        Some(SessionDuration::new(Duration::from_secs(8 * 3600)))
    );

    assert_eq!(profile.at(&utc(6, 59)).cable, CableState::VehicleLocked);
    let unplugged = profile.at(&utc(7, 0));
    assert_eq!(unplugged.status, ChargePointStatus::Available);
    assert_eq!(unplugged.cable, CableState::NoCable);
}

#[test]
fn a_daytime_window_is_open_from_plug_in_until_unplug() {
    let profile = ChargeProfile::new(
        time(9, 0),
        time(17, 0),
        AMPS_16,
        PhaseMode::One,
        UNREACHED,
        chrono_tz::UTC,
    );

    assert_eq!(profile.at(&utc(8, 59)).cable, CableState::NoCable);
    assert_eq!(profile.at(&utc(9, 0)).cable, CableState::VehicleLocked);
    assert_eq!(profile.at(&utc(16, 59)).cable, CableState::VehicleLocked);
    assert_eq!(profile.at(&utc(17, 0)).cable, CableState::NoCable);
    assert_eq!(profile.at(&utc(23, 0)).cable, CableState::NoCable);
}

/// The night the clocks go back has an extra hour: 18:00 to 06:00 local is
/// thirteen hours of charging, not twelve.
#[test]
fn the_night_the_clocks_go_back_charges_for_the_extra_hour() {
    let profile = overnight(Amsterdam, PhaseMode::One, UNREACHED);

    let state = profile.at(&clock_in(Amsterdam, (2026, 10, 25), 6, 0));
    assert_eq!(
        state.session_duration,
        Some(SessionDuration::new(Duration::from_secs(13 * 3600)))
    );
    assert_eq!(state.session_energy, Some(WattHours(3_680.0 * 13.0)));
}

/// And the night they go forward loses one: eleven hours.
#[test]
fn the_night_the_clocks_go_forward_charges_an_hour_less() {
    let profile = overnight(Amsterdam, PhaseMode::One, UNREACHED);

    let state = profile.at(&clock_in(Amsterdam, (2026, 3, 29), 6, 0));
    assert_eq!(
        state.session_duration,
        Some(SessionDuration::new(Duration::from_secs(11 * 3600)))
    );
}

/// 02:30 never shows on the clock that night; the car plugs in when the
/// clock jumps past it, so ten minutes after the jump is ten minutes in.
#[test]
fn a_plug_in_time_skipped_by_the_clocks_going_forward_starts_at_the_jump() {
    let profile = ChargeProfile::new(
        time(2, 30),
        time(7, 0),
        AMPS_16,
        PhaseMode::One,
        UNREACHED,
        Amsterdam,
    );

    let state = profile.at(&clock_in(Amsterdam, (2026, 3, 29), 3, 10));
    assert_eq!(state.status, ChargePointStatus::Charging);
    assert_eq!(
        state.session_duration,
        Some(SessionDuration::new(Duration::from_secs(10 * 60)))
    );
}
