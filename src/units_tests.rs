//! Tests for `units.rs`, kept in their own file because the module is long.
//!
//! The bulk of these pin *wire formats*, not arithmetic: `mqtt.rs`,
//! `zendure.rs`, `command.rs` and `models.rs` have no tests of their own, so
//! the bytes that must not move are otherwise unguarded — and step 7's journal
//! is built on them.

use std::time::Duration;

use super::*;

// --- Display forwards the caller's format spec ----------------------------
//
// The failure this guards is silent: a hand-written `write!(f, "{}", self.0)`
// drops `f.precision()`, and `format!("{v:.1}")` in `publish_rte` starts
// emitting 85.23456789 instead of 85.2 with nothing to notice it.

#[test]
fn float_display_honors_precision() {
    let x = 85.23456789_f64;
    assert_eq!(format!("{:.1}", Percent(x)), format!("{x:.1}"));
    assert_eq!(format!("{:.2}", KiloWattHours(x)), format!("{x:.2}"));
    assert_eq!(format!("{:.0}", WattHours(x)), format!("{x:.0}"));
    assert_eq!(format!("{:.0}", GridPower(150.5)), format!("{:.0}", 150.5));
    assert_eq!(format!("{:.0}", SolarPower::new(x)), format!("{x:.0}"));
}

#[test]
fn integer_display_is_the_bare_number() {
    // mqtt.rs publishes `decision.power_watts.to_string()`, and command.rs
    // formats `set_charge({w}W)` into the journal.
    assert_eq!(Setpoint::new(145).to_string(), "145");
    assert_eq!(Soc::new(64).to_string(), "64");
    assert_eq!(Watts(-320).to_string(), "-320");
    assert_eq!(PowerCap::new(2400).to_string(), "2400");
    assert_eq!(BatteryPower(-500).to_string(), "-500");
}

// --- serde(transparent) keeps every wire format byte-identical ------------

#[test]
fn newtypes_serialize_as_bare_numbers() {
    // The device write path: zendure.rs builds `{"inputLimit": setpoint}` with
    // serde_json::json!, which would emit an object for a non-transparent type.
    assert_eq!(
        serde_json::to_string(&serde_json::json!({ "inputLimit": Setpoint::new(500) })).unwrap(),
        r#"{"inputLimit":500}"#
    );
    assert_eq!(serde_json::to_string(&GridPower(150.5)).unwrap(), "150.5");
    assert_eq!(serde_json::to_string(&Soc::new(64)).unwrap(), "64");
    assert_eq!(serde_json::to_string(&WattHours(1920.0)).unwrap(), "1920.0");
    assert_eq!(
        serde_json::to_string(&Timestamp(1_000_000_000)).unwrap(),
        "1000000000"
    );
}

#[test]
fn newtypes_deserialize_from_bare_numbers() {
    // The RTE state file round-trips through these.
    assert_eq!(
        serde_json::from_str::<WattHours>("1920.0").unwrap(),
        WattHours(1920.0)
    );
    assert_eq!(serde_json::from_str::<Soc>("64").unwrap(), Soc::new(64));
    assert_eq!(
        serde_json::from_str::<GridPower>("-233.4").unwrap(),
        GridPower(-233.4)
    );
}

// --- Elapsed keeps today's signed comparison semantics --------------------

#[test]
fn elapsed_is_signed_so_a_backwards_clock_reads_as_not_yet_elapsed() {
    // A backwards NTP step makes now - last negative. Today `-5 >= 0` is false,
    // so the guard holds. A Duration-returning subtraction would saturate to
    // zero and flip that to true for every Duration::ZERO threshold — which
    // both `test_default` and `controller_no_cooldown` use.
    let now = Timestamp(1_000);
    let later_recorded = Timestamp(1_005);
    let span = now - later_recorded;

    assert_eq!(span.as_millis(), -5);

    // Bind the comparison the guards actually make (`>=`), rather than
    // asserting its negation inline: it is the operator under test, and
    // `apply_guards` reading `true` here would fire a mode change early.
    let guard_says_elapsed = span >= Duration::ZERO;
    assert!(!guard_says_elapsed);
    assert!(span < Duration::from_secs(10));
}

#[test]
fn elapsed_compares_against_durations_at_the_exact_boundary() {
    let span = Timestamp(10_000) - Timestamp(5_000);
    assert!(span >= Duration::from_secs(5));

    let one_ms_past = span >= Duration::from_secs(5) + Duration::from_millis(1);
    assert!(!one_ms_past);
    assert_eq!(span.as_secs_f64(), 5.0);
}

#[test]
fn timestamp_arithmetic_round_trips() {
    let now = Timestamp(1_000_000_000);
    let back = now - Elapsed::of(Duration::from_secs(60));
    assert_eq!(back.as_millis(), 1_000_000_000 - 60_000);
    assert_eq!(now - back, Elapsed::of(Duration::from_secs(60)));
    assert_eq!(back + Elapsed::of(Duration::from_secs(60)), now);
}

// --- Roles: the conversions that used to be casts -------------------------

#[test]
fn grid_power_splits_into_import_and_export() {
    assert_eq!(GridPower(500.0).importing(), Watts(500));
    assert_eq!(GridPower(500.0).exporting(), Watts(-500));
    assert_eq!(GridPower(-500.0).exporting(), Watts(500));
    // Truncates toward zero, as `(-grid_power) as i32` did.
    assert_eq!(GridPower(-500.9).exporting(), Watts(500));
    assert_eq!(GridPower(500.9).importing(), Watts(500));
}

#[test]
fn battery_flow_is_signed_and_splits_by_direction() {
    let charging = BatteryPower::from_flows(Watts::from_device(0), Watts::from_device(800));
    assert_eq!(charging, BatteryPower(-800));
    assert_eq!(charging.charging(), Watts(800));
    assert_eq!(charging.discharging(), Watts::ZERO);

    let discharging = BatteryPower::from_flows(Watts::from_device(600), Watts::from_device(0));
    assert_eq!(discharging, BatteryPower(600));
    assert_eq!(discharging.discharging(), Watts(600));
    assert_eq!(discharging.charging(), Watts::ZERO);
}

#[test]
fn underlying_grid_adds_back_the_batterys_own_effect() {
    // Discharging 400 W while the meter reads 0 means the house is drawing 400.
    assert_eq!(GridPower::ZERO + BatteryPower(400), GridPower(400.0));
    // Charging 400 W while the meter reads 0 means we're exporting 400.
    assert_eq!(GridPower::ZERO + BatteryPower(-400), GridPower(-400.0));
}

#[test]
fn setpoint_is_never_negative_and_respects_the_cap() {
    let cap = PowerCap::new(2400);
    assert_eq!(Setpoint::clamped(Watts(-500), cap), Setpoint::ZERO);
    assert_eq!(Setpoint::clamped(Watts(9000), cap), Setpoint::new(2400));
    assert_eq!(Setpoint::clamped(Watts(740), cap), Setpoint::new(740));
    assert_eq!(Setpoint::new(-1), Setpoint::ZERO);
}

#[test]
fn a_huge_device_cap_cannot_make_clamping_panic() {
    // battery.rs used to do `u32 as i32`, which wraps negative above i32::MAX
    // and then reaches clamp(0, negative) — a panic in the decision path.
    let absurd = PowerCap::new(u32::MAX);
    assert!(absurd.watts().get() > 0);
    assert_eq!(Setpoint::clamped(Watts(1000), absurd), Setpoint::new(1000));
}

#[test]
fn a_zeroed_cap_stops_everything() {
    // The device zeroing its own setpoint is honored, not overwritten.
    assert_eq!(
        Setpoint::clamped(Watts(1000), PowerCap::ZERO),
        Setpoint::ZERO
    );
}

#[test]
fn ramp_truncates_toward_zero() {
    // Was `(power as f64 * RAMP_FACTOR) as i32`.
    assert_eq!(Setpoint::new(1000).ramped(0.75), Setpoint::new(750));
    assert_eq!(Setpoint::new(145).ramped(0.75), Setpoint::new(108)); // 108.75
    assert_eq!(Setpoint::ZERO.ramped(0.75), Setpoint::ZERO);
}

#[test]
fn solar_power_clamps_at_zero() {
    assert_eq!(SolarPower::new(-50.0), SolarPower::ZERO);
    // Production is the export on the configured phase.
    assert_eq!(
        SolarPower::from_phase_export(GridPower(-1500.0)).get(),
        1500.0
    );
    // An importing phase means no production to read.
    assert_eq!(
        SolarPower::from_phase_export(GridPower(300.0)),
        SolarPower::ZERO
    );
}

// --- Soc: validation in the constructor -----------------------------------

#[test]
fn soc_clamps_once_so_call_sites_never_re_check() {
    assert_eq!(Soc::new(150), Soc::FULL);
    assert_eq!(Soc::new(100), Soc::FULL);
    assert_eq!(Soc::new(0), Soc::ZERO);
}

#[test]
fn soc_from_tenths_names_the_10x_conversion() {
    // The device reports socSet/minSoc in tenths: 1000 is 100.0%, 100 is 10.0%.
    assert_eq!(Soc::from_tenths(1000), Soc::FULL);
    assert_eq!(Soc::from_tenths(100), Soc::new(10));
    assert_eq!(Soc::from_tenths(0), Soc::ZERO);
}

#[test]
fn fraction_above_saturates_below_the_floor() {
    assert_eq!(Soc::new(80).fraction_above(Soc::new(10)), 0.70);
    // rte.rs's `(soc - min_soc) as f64` was an unchecked u32 subtraction, safe
    // only because of an early return that a future edit could drop.
    assert_eq!(Soc::new(5).fraction_above(Soc::new(10)), 0.0);
}

// --- Energy ---------------------------------------------------------------

#[test]
fn watt_hours_integrate_power_over_time() {
    // 1000 W held for one hour is 1000 Wh.
    let wh = WattHours::integrate(Watts(1000), Watts(1000), Duration::from_secs(3600));
    assert!((wh.get() - 1000.0).abs() < 1e-9);

    // Trapezoidal: ramping 0 -> 1000 W over an hour averages 500 W.
    let ramp = WattHours::integrate(Watts::ZERO, Watts(1000), Duration::from_secs(3600));
    assert!((ramp.get() - 500.0).abs() < 1e-9);
}

#[test]
fn watt_hours_sum_and_convert_to_kwh() {
    let total: WattHours = [WattHours(1920.0), WattHours(1920.0)].into_iter().sum();
    assert_eq!(total, WattHours(3840.0));
    assert_eq!(total.to_kwh(), KiloWattHours(3.84));
}

#[test]
fn percent_converts_to_a_fraction() {
    assert_eq!(Percent(85.0).fraction(), 0.85);
}

// --- Watts arithmetic saturates rather than panicking ---------------------

#[test]
fn watts_arithmetic_saturates_at_the_extremes() {
    assert_eq!(Watts(i32::MAX) + Watts(1), Watts(i32::MAX));
    assert_eq!(Watts(i32::MIN) - Watts(1), Watts(i32::MIN));
    // `-battery.current_power` on i32::MIN used to overflow-panic in debug.
    assert_eq!(-Watts(i32::MIN), Watts(i32::MAX));
    assert_eq!(BatteryPower(i32::MIN).charging(), Watts(i32::MAX));
}

#[test]
fn watts_from_device_saturates_on_an_absurd_payload() {
    assert_eq!(Watts::from_device(2400), Watts(2400));
    assert_eq!(Watts::from_device(u32::MAX), Watts(i32::MAX));
}

#[test]
fn battery_power_addition_saturates_at_the_extremes() {
    // A corrupt reading must not panic a debug build in the decision path,
    // which is the whole reason this is `saturating_add` and not `+`.
    assert_eq!(
        BatteryPower(i32::MAX) + BatteryPower(1),
        BatteryPower(i32::MAX),
    );
    assert_eq!(
        BatteryPower(i32::MIN) + BatteryPower(-1),
        BatteryPower(i32::MIN),
    );
}

#[test]
fn battery_power_sums_over_a_fleet() {
    // The world's meter correction is this sum, so an empty fleet has to read
    // as no correction rather than as a missing value.
    let none: BatteryPower = [].into_iter().sum();
    assert_eq!(none, BatteryPower::ZERO);

    let one: BatteryPower = [BatteryPower(-800)].into_iter().sum();
    assert_eq!(one, BatteryPower(-800));

    // Signs are meaningful and mixed in a real fleet: charging is negative,
    // discharging positive, so the total is a net flow and not a magnitude.
    let mixed: BatteryPower = [BatteryPower(-800), BatteryPower(600), BatteryPower(-100)]
        .into_iter()
        .sum();
    assert_eq!(mixed, BatteryPower(-300));

    // Folding through `Add` keeps the saturation the single-step case has.
    let extreme: BatteryPower = [BatteryPower(i32::MAX), BatteryPower(i32::MAX)]
        .into_iter()
        .sum();
    assert_eq!(extreme, BatteryPower(i32::MAX));
}
