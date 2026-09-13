//! Tests for `units.rs`, kept in their own file because the module is long.
//!
//! The bulk of these pin *wire formats*, not arithmetic: `mqtt.rs`,
//! `zendure.rs`, `command.rs` and `models.rs` have no tests of their own, so
//! the bytes that must not move are otherwise unguarded.

use std::time::Duration;

use super::*;

// --- Display forwards the caller's format spec ----------------------------
// A hand-written `write!(f, "{}", self.0)` drops `f.precision()`, so
// `format!("{v:.1}")` in `publish_rte` would silently emit 85.23456789
// instead of 85.2.

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

// --- Roles: the conversions -----------------------------------------------

#[test]
fn grid_power_splits_into_import_and_export() {
    assert_eq!(GridPower(500.0).importing(), Watts(500));
    assert_eq!(GridPower(500.0).exporting(), Watts(-500));
    assert_eq!(GridPower(-500.0).exporting(), Watts(500));
    // Truncates toward zero.
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
fn battery_power_converts_to_the_plain_signed_watts_it_wraps() {
    // `simulation.rs` integrates this with `WattHours::integrate`, which has
    // no notion of charging or discharging — only a signed rate.
    assert_eq!(BatteryPower(-800).into_watts(), Watts(-800));
    assert_eq!(BatteryPower(600).into_watts(), Watts(600));
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
    // Above i32::MAX a `u32 as i32` cast wraps negative and reaches clamp(0, negative)
    // — a panic in the decision path.
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
    assert_eq!(Soc::new(5).fraction_above(Soc::new(10)), 0.0);
}

#[test]
fn soc_from_fraction_rounds_to_the_nearest_whole_percent() {
    assert_eq!(Soc::from_fraction(0.5), Soc::new(50));
    // Rounds, rather than truncating: 0.505 is closer to 51% of the pack than
    // to 50%.
    assert_eq!(Soc::from_fraction(0.505), Soc::new(51));
    assert_eq!(Soc::from_fraction(0.0), Soc::ZERO);
    assert_eq!(Soc::from_fraction(1.0), Soc::FULL);
}

#[test]
fn soc_from_fraction_clamps_out_of_range_values_through_new() {
    // A rounding blip past full, or stored energy that has drifted a hair
    // below zero from floating-point error, must land in range rather than
    // wrap or panic.
    assert_eq!(Soc::from_fraction(1.2), Soc::FULL);
    assert_eq!(Soc::from_fraction(-0.05), Soc::ZERO);
}

#[test]
fn soc_from_fraction_maps_nan_to_zero() {
    // A zero-capacity pack computes `0.0 / 0.0`. `NaN as u32` is a defined but
    // meaningless 0 in Rust; this asserts the type states that explicitly
    // rather than depending on the cast's incidental behavior.
    assert_eq!(Soc::from_fraction(f64::NAN), Soc::ZERO);
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

#[test]
fn watt_hours_over_recovers_the_average_power() {
    // The inverse of `integrate`: 500 Wh spread over an hour is 500 W.
    assert_eq!(WattHours(500.0).over(Duration::from_secs(3600)), Watts(500));
    // Half an hour: the same energy is twice the rate.
    assert_eq!(
        WattHours(500.0).over(Duration::from_secs(1800)),
        Watts(1000)
    );
}

#[test]
fn watt_hours_over_a_zero_interval_mints_nothing() {
    // Dividing by zero seconds would otherwise produce an infinite wattage.
    assert_eq!(WattHours(500.0).over(Duration::ZERO), Watts::ZERO);
}

// --- Efficiency: clamped so discharge can never divide by zero ------------

#[test]
fn efficiency_clamps_to_one_through_a_hundred_percent() {
    assert_eq!(Efficiency::new(150.0).get(), 100.0);
    // Not zero: discharge divides by this, and a zero would mint infinite
    // energy out of a battery that gave up nothing.
    assert_eq!(Efficiency::new(0.0).get(), 1.0);
    assert_eq!(Efficiency::new(-10.0).get(), 1.0);
    assert_eq!(Efficiency::new(95.0).get(), 95.0);
}

#[test]
fn efficiency_maps_nan_to_the_worst_defined_value_rather_than_propagating() {
    assert_eq!(Efficiency::new(f64::NAN).get(), 1.0);
}

#[test]
fn efficiency_converts_to_a_fraction() {
    assert_eq!(Efficiency::new(95.0).fraction(), 0.95);
}

// --- Watts arithmetic saturates rather than panicking ---------------------

#[test]
fn watts_arithmetic_saturates_at_the_extremes() {
    assert_eq!(Watts(i32::MAX) + Watts(1), Watts(i32::MAX));
    assert_eq!(Watts(i32::MIN) - Watts(1), Watts(i32::MIN));
    // Negating i32::MIN saturates to i32::MAX rather than panicking.
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

/// Zero and negative are rejected, not clamped: this is the bug the type
/// exists for. `prune` deletes rows older than `now - days`; a negative count
/// puts the cutoff in the *future*, so "prune" deletes the entire journal and logs it
/// as a success.
#[test]
fn retention_rejects_windows_that_would_delete_the_present() {
    for bad in [0, -1, -5, i64::MIN] {
        assert!(
            RetentionDays::new(bad).is_err(),
            "{bad} days must not be accepted"
        );
    }
    assert_eq!(RetentionDays::new(1).unwrap().days(), 1);
    assert_eq!(RetentionDays::new(90).unwrap().days(), 90);
}

/// An absurd value is clamped rather than rejected — the intent is
/// unambiguous — and clamping is what keeps `cutoff` away from the range where
/// `chrono::Duration::days` panics.
#[test]
fn retention_clamps_absurd_windows_instead_of_panicking() {
    assert_eq!(
        RetentionDays::new(i64::MAX).unwrap().days(),
        RetentionDays::MAX_DAYS
    );
    // The point of the clamp: this must not panic.
    let now = chrono::Utc::now();
    let cutoff = RetentionDays::new(i64::MAX).unwrap().cutoff(now);
    assert!(cutoff < Timestamp::from_millis(now.timestamp_millis()));
}

/// The cutoff is in the past by exactly the window, which is what makes
/// "older than N days" mean N days.
#[test]
fn retention_cutoff_is_the_window_behind_now() {
    let now = chrono::Utc::now();
    let cutoff = RetentionDays::new(30).unwrap().cutoff(now);
    let expected = now - chrono::Duration::days(30);
    assert_eq!(cutoff, Timestamp::from_millis(expected.timestamp_millis()));
}

/// Reading a value back must enforce the same invariant constructing it does:
/// a derived `Deserialize` writes the field directly, bypassing every clamp
/// for external sources like `replay --set` or hand-edited fixtures. E.g.
/// `min_soc=1000` (the device's format) becomes `Soc(1000)`, against which `soc >
/// min_soc` is never true.
#[test]
fn clamping_newtypes_clamp_on_the_way_in_too() {
    assert_eq!(serde_json::from_str::<Soc>("1000").unwrap(), Soc::new(1000));
    assert_eq!(serde_json::from_str::<Soc>("1000").unwrap(), Soc::FULL);

    // The one that inverts a guard rather than merely saturating: the README
    // documents a negative `SOLAR_DISCHARGE_BLOCK_THRESHOLD` as "disables the
    // guard", which is only true because the clamp turns it into the `0`
    // sentinel. Unclamped, it wires the guard permanently on.
    let off = serde_json::from_str::<SolarPower>("-500").unwrap();
    assert_eq!(off, SolarPower::ZERO);
    assert_eq!(off, SolarPower::new(-500.0));

    assert_eq!(
        serde_json::from_str::<Setpoint>("-42").unwrap(),
        Setpoint::ZERO
    );
}

/// And a valid value still round-trips byte-for-byte, so the journal, MQTT and
/// every fixture already written read back unchanged.
#[test]
fn validating_deserialize_leaves_the_wire_format_alone() {
    for json in ["0", "55", "100"] {
        let soc: Soc = serde_json::from_str(json).unwrap();
        assert_eq!(serde_json::to_string(&soc).unwrap(), json);
    }
    let solar: SolarPower = serde_json::from_str("212.5").unwrap();
    assert_eq!(serde_json::to_string(&solar).unwrap(), "212.5");
    let setpoint: Setpoint = serde_json::from_str("145").unwrap();
    assert_eq!(serde_json::to_string(&setpoint).unwrap(), "145");
}

/// Validation on deserialization prevents invalid retention values.
///
/// `RetentionDays` routes `Deserialize` through `new` instead of deriving it,
/// so a `0` read off a wire is rejected rather than bypassing validation.
#[test]
fn retention_days_refuses_through_serde_what_its_constructor_refuses() {
    for bad in ["0", "-5"] {
        let err = serde_json::from_str::<RetentionDays>(bad)
            .expect_err("a retention that deletes everything is not a retention");
        assert!(
            err.to_string()
                .contains("must be a positive number of days"),
            "the constructor's own message should reach the caller, got: {err}",
        );
    }

    assert_eq!(
        serde_json::from_str::<RetentionDays>("30").unwrap(),
        RetentionDays::new(30).unwrap(),
    );
    // An absurd upper value is still clamped rather than refused: the intent
    // there is unambiguous.
    assert_eq!(
        serde_json::from_str::<RetentionDays>("99999")
            .unwrap()
            .days(),
        RetentionDays::MAX_DAYS,
    );
}

/// Serialization is untouched, so the journal's own round trip still works.
#[test]
fn retention_days_still_serializes_as_a_bare_number() {
    assert_eq!(
        serde_json::to_string(&RetentionDays::new(90).unwrap()).unwrap(),
        "90",
    );
}

/// The vendor encoding every Zendure temperature arrives in, and the unit
/// anyone actually reads.
#[test]
fn deci_kelvin_converts_to_celsius_at_one_decimal() {
    // The values the wire format is pinned on.
    assert_eq!(format!("{:.1}", DeciKelvin(3001).to_celsius()), "27.0");
    assert_eq!(format!("{:.1}", DeciKelvin(2981).to_celsius()), "25.0");
    assert_eq!(format!("{:.1}", DeciKelvin(2995).to_celsius()), "26.4");
    // Just below freezing. Renders as "-0.0" due to f64 rounding — pinned to
    // detect a future switch to a decimal type.
    assert_eq!(format!("{:.1}", DeciKelvin(2731).to_celsius()), "-0.0");
}

/// `Display` forwards the formatter rather than rendering through `{}`, so a
/// precision at the call site is not silently dropped.
#[test]
fn celsius_keeps_the_precision_it_is_given() {
    assert_eq!(format!("{:.1}", Celsius(1.2345)), "1.2");
    assert_eq!(format!("{}", Celsius(1.2345)), "1.2345");
}
