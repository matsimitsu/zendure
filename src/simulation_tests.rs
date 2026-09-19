use tokio::time::Duration;

use super::*;
use crate::units::PowerCap;

/// Generous caps so a test can command whatever it wants to without
/// tripping the spec clamp — that clamp gets its own dedicated test
/// below instead.
fn spec() -> BatterySpec {
    BatterySpec {
        max_charge_power: PowerCap::new(5_000),
        max_discharge_power: PowerCap::new(5_000),
    }
}

fn battery(capacity_wh: f64, soc: Soc) -> VirtualBattery {
    VirtualBattery::new(
        DeviceId::new("sim"),
        spec(),
        vec![WattHours(capacity_wh)],
        soc,
        Efficiency::new(95.0),
        Efficiency::new(95.0),
    )
}

fn hours(h: f64) -> Duration {
    Duration::from_secs_f64(h * 3600.0)
}

#[tokio::test]
async fn charging_for_an_hour_at_95_percent_adds_950_wh() {
    // 1000 W for 1 h = 1000 Wh at the meter; 95% lands in the pack: 1000 * 0.95 =
    // 950 Wh.
    // Capacity is 100,000 Wh, not a rounder 10,000: at 10,000 the SOC
    // lands on an exact x.5 boundary where the tiny float error in
    // `1000.0 * 0.95` (not quite 950.0) flips `Soc::from_fraction`'s rounding — a
    // real edge case, but not the one this test is for.
    let t0 = Instant::now();
    let battery = battery(100_000.0, Soc::new(50));

    battery.apply_at(t0, &Command::SetCharge(Setpoint::new(1_000)));
    let state = battery.reading_at(t0 + hours(1.0));

    let starting_wh = 100_000.0 * 0.5;
    let expected_wh = starting_wh + 950.0;
    assert_eq!(state.soc, Soc::from_fraction(expected_wh / 100_000.0));
    // Unclamped, so the achieved flow is exactly the commanded one.
    assert_eq!(state.current_power, BatteryPower(-1_000));
}

#[tokio::test]
async fn discharging_for_an_hour_at_95_percent_removes_1052_6_wh() {
    // Discharging divides rather than multiplies: the pack must give up
    // more than it delivers. To deliver 1000 Wh at 95% efficiency the
    // pack gives up 1000 / 0.95 = 1052.631... Wh.
    let t0 = Instant::now();
    let battery = battery(100_000.0, Soc::new(50));

    battery.apply_at(t0, &Command::SetDischarge(Setpoint::new(1_000)));
    let state = battery.reading_at(t0 + hours(1.0));

    let starting_wh = 100_000.0 * 0.5;
    let expected_wh = starting_wh - (1_000.0 / 0.95);
    assert_eq!(state.soc, Soc::from_fraction(expected_wh / 100_000.0));
    assert_eq!(state.current_power, BatteryPower(1_000));
}

#[tokio::test]
async fn a_full_round_trip_returns_eta_squared_of_the_energy_put_in() {
    // Charge 1000 W for 1h: 950 Wh stored. Discharging removes `moved /
    // eta`, so `moved = 950*0.95 = 902.5` Wh must leave the meter, taking
    // `902.5/1000` h = 0.9025h = 3249s at 1000W. Energy in 1000 Wh, out
    // 902.5 Wh: ratio 0.9025 = 0.95*0.95 = eta^2 — charging loses one factor of
    // eta, discharging loses another.
    let t0 = Instant::now();
    let battery = battery(1_000_000.0, Soc::new(50)); // capacity high enough never to clamp

    battery.apply_at(t0, &Command::SetCharge(Setpoint::new(1_000)));
    let after_charge = battery.reading_at(t0 + hours(1.0));

    let t1 = t0 + hours(1.0);
    battery.apply_at(t1, &Command::SetDischarge(Setpoint::new(1_000)));
    let drain = Duration::from_secs(3_249);
    let after_discharge = battery.reading_at(t1 + drain);

    let starting_wh = 1_000_000.0 * 0.5;
    let after_charge_wh = starting_wh + 950.0;
    assert_eq!(
        after_charge.soc,
        Soc::from_fraction(after_charge_wh / 1_000_000.0)
    );

    let energy_removed: f64 = 1_000.0 * (3_249.0 / 3_600.0); // meter-side Wh delivered
    assert!((energy_removed - 902.5).abs() < 1e-6);
    let after_discharge_wh = after_charge_wh - 950.0; // returns to the pre-charge level
    assert_eq!(
        after_discharge.soc,
        Soc::from_fraction(after_discharge_wh / 1_000_000.0)
    );

    let round_trip_ratio = energy_removed / 1_000.0;
    assert!((round_trip_ratio - 0.95 * 0.95).abs() < 1e-9);
}

#[tokio::test]
async fn a_full_pack_reports_charge_power_zero() {
    let t0 = Instant::now();
    let battery = battery(10_000.0, Soc::FULL);

    battery.apply_at(t0, &Command::SetCharge(Setpoint::new(1_000)));
    let state = battery.reading_at(t0 + hours(1.0));

    assert_eq!(state.soc, Soc::FULL);
    // Not the commanded 1000 W: the pack had no room, so nothing landed
    // and nothing should be reported as still charging.
    assert_eq!(state.current_power, BatteryPower::ZERO);
}

#[tokio::test]
async fn an_empty_pack_reports_discharge_power_zero() {
    let t0 = Instant::now();
    let battery = battery(10_000.0, Soc::ZERO);

    battery.apply_at(t0, &Command::SetDischarge(Setpoint::new(1_000)));
    let state = battery.reading_at(t0 + hours(1.0));

    assert_eq!(state.soc, Soc::ZERO);
    assert_eq!(state.current_power, BatteryPower::ZERO);
}

#[tokio::test]
async fn no_elapsed_time_mints_nothing() {
    let t0 = Instant::now();
    let battery = battery(10_000.0, Soc::new(50));

    battery.apply_at(t0, &Command::SetCharge(Setpoint::new(1_000)));
    let immediately = battery.reading_at(t0);

    assert_eq!(immediately.soc, Soc::new(50));
}

#[tokio::test]
async fn a_command_change_integrates_the_old_power_up_to_the_change() {
    // Charge for 30 minutes, then switch to discharge. The stored energy
    // at the moment of the switch must reflect 30 minutes of charging —
    // not 0 (the new command backdated) and not 60 (the old command
    // extended past when it actually changed).
    let t0 = Instant::now();
    let battery = battery(1_000_000.0, Soc::new(50));

    battery.apply_at(t0, &Command::SetCharge(Setpoint::new(1_000)));
    let switch_at = t0 + hours(0.5);
    // `apply_at` advances the model to `switch_at` — integrating the old
    // charge command — before installing the discharge command.
    battery.apply_at(switch_at, &Command::SetDischarge(Setpoint::new(1_000)));
    let state = battery.reading_at(switch_at);

    let starting_wh = 1_000_000.0 * 0.5;
    // 1000 W for 0.5 h is 500 Wh at the meter; 95% of that landed.
    let expected_wh = starting_wh + 500.0 * 0.95;
    assert_eq!(state.soc, Soc::from_fraction(expected_wh / 1_000_000.0));
}

#[tokio::test]
async fn a_setpoint_above_the_spec_cap_is_clamped() {
    let battery = VirtualBattery::new(
        DeviceId::new("sim"),
        BatterySpec {
            max_charge_power: PowerCap::new(2_400),
            max_discharge_power: PowerCap::new(800),
        },
        vec![WattHours(10_000.0)],
        Soc::new(50),
        Efficiency::new(95.0),
        Efficiency::new(95.0),
    );
    let t0 = Instant::now();

    battery.apply_at(t0, &Command::SetDischarge(Setpoint::new(5_000)));
    // Two seconds is 1600 W of slew, so only the cap can hold it at 800.
    let state = battery.reading_at(t0 + Duration::from_secs(2));
    assert_eq!(state.current_power, BatteryPower(800));
}

/// The failure this catches: a simulator that installs its setpoint instantly,
/// hiding the lag the control loop has to stay stable through.
#[tokio::test]
async fn a_new_setpoint_is_approached_over_time_rather_than_stepped_to() {
    let battery = battery(1_000_000.0, Soc::new(50));
    let t0 = Instant::now();

    battery.apply_at(t0, &Command::SetCharge(Setpoint::new(2_400)));

    let midway = battery.reading_at(t0 + Duration::from_secs(1));
    assert_eq!(midway.current_power, BatteryPower(-800));

    let arrived = battery.reading_at(t0 + Duration::from_secs(3));
    assert_eq!(arrived.current_power, BatteryPower(-2_400));
}

/// The failure this catches: integrating a ramp that arrived early as one
/// trapezoid over the whole span.
#[tokio::test]
async fn a_ramp_that_arrives_mid_span_is_integrated_exactly() {
    // Empty, so the assertion's resolution is that of ~950 Wh rather than
    // of the half-megawatt-hour it would be sitting on top of.
    let battery = battery(1_000_000.0, Soc::ZERO);
    let t0 = Instant::now();

    battery.apply_at(t0, &Command::SetCharge(Setpoint::new(1_000)));
    let stored = battery.stored_at(t0 + hours(1.0));

    // 1000 W for an hour, less the half-triangle given up while ramping
    // there at 800 W/s, and 95% of that lands in the pack.
    let ramp_hours = 1_000.0 / 800.0 / 3_600.0;
    let meter_side = 1_000.0 * (1.0 - ramp_hours / 2.0);
    assert!(
        (stored.get() - meter_side * 0.95).abs() < 1e-9,
        "stored {stored} Wh disagrees with the exact integral",
    );
}

/// The failure this catches: taking the efficiency direction from a span's
/// *net* energy, which lets a symmetric reversal round-trip for free.
#[tokio::test]
async fn a_reversal_inside_one_span_still_pays_the_round_trip_loss() {
    let battery = battery(1_000_000.0, Soc::new(50));
    let t0 = Instant::now();

    battery.apply_at(t0, &Command::SetDischarge(Setpoint::new(1_000)));
    let reversal = t0 + Duration::from_secs(10);
    let before = battery.stored_at(reversal);

    battery.apply_at(reversal, &Command::SetCharge(Setpoint::new(1_000)));
    let after = battery.stored_at(reversal + Duration::from_millis(2_500));

    // +1000 W to -1000 W at 800 W/s: 1.25 s of discharge and 1.25 s of
    // charge, each a triangle of 1000 W, each paying its own efficiency.
    let half = 1_000.0 * 1.25 / 2.0 / 3_600.0;
    let expected = half * 0.95 - half / 0.95;
    assert!(
        (after.get() - before.get() - expected).abs() < 1e-9,
        "a symmetric reversal moved {} Wh, not the {expected} Wh it costs",
        after.get() - before.get(),
    );
}

/// The failure this catches: an integrator whose answer depends on how often
/// it was read. 350 ms divides neither the ramp nor the span.
#[tokio::test]
async fn energy_is_the_same_however_the_span_was_split() {
    let one_span = battery(1_000_000.0, Soc::ZERO);
    let many_spans = battery(1_000_000.0, Soc::ZERO);
    // Shared, and after both constructions, so the two runs start from the
    // same instant rather than from whenever each was built.
    let t0 = Instant::now();

    one_span.apply_at(t0, &Command::SetCharge(Setpoint::new(2_000)));
    many_spans.apply_at(t0, &Command::SetCharge(Setpoint::new(2_000)));

    let span = Duration::from_secs(60);
    let mut read_at = Duration::from_millis(350);
    while read_at < span {
        many_spans.stored_at(t0 + read_at);
        read_at += Duration::from_millis(350);
    }

    let split = many_spans.stored_at(t0 + span);
    let whole = one_span.stored_at(t0 + span);
    assert!(
        (split.get() - whole.get()).abs() < 1e-9,
        "read every 350 ms: {split} Wh; read once: {whole} Wh",
    );
}
