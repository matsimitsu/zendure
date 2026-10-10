use super::*;

fn series(len: usize, f: impl Fn(usize) -> f64) -> Vec<SolarPower> {
    (0..len).map(|i| SolarPower::new(f(i))).collect()
}

#[test]
fn an_all_zero_day_has_no_energy_peak_at_zero_and_no_last_sun() {
    let day = series(48, |_| 0.0);
    assert_eq!(kwh(&day), KiloWattHours(0.0));
    assert_eq!(peak_slot(&day), 0);
    assert_eq!(last_sun_slot(&day), None);
}

#[test]
fn kwh_is_half_an_hour_per_slot() {
    let day = series(48, |i| if i == 20 || i == 21 { 1000.0 } else { 0.0 });
    assert_eq!(kwh(&day), KiloWattHours(1.0));
}

#[test]
fn dst_days_are_not_assumed_to_be_48_slots() {
    for len in [46, 50] {
        let day = series(len, |i| if i == len - 3 { 400.0 } else { 0.0 });
        assert_eq!(kwh(&day), KiloWattHours(0.2));
        assert_eq!(peak_slot(&day), len - 3);
        assert_eq!(last_sun_slot(&day), Some(len - 3));
    }
}

#[test]
fn delta_is_none_for_a_zero_forecast() {
    assert_eq!(delta_pct(KiloWattHours(3.0), KiloWattHours(0.0)), None);
    assert_eq!(delta_pct(KiloWattHours(0.0), KiloWattHours(0.0)), None);
}

#[test]
fn delta_rounds_to_the_nearest_percent() {
    assert_eq!(delta_pct(KiloWattHours(1.04), KiloWattHours(1.0)), Some(4));
    assert_eq!(delta_pct(KiloWattHours(1.046), KiloWattHours(1.0)), Some(5));
    assert_eq!(delta_pct(KiloWattHours(0.5), KiloWattHours(1.0)), Some(-50));
    assert_eq!(delta_pct(KiloWattHours(1.004), KiloWattHours(1.0)), Some(0));
}

#[test]
fn delta_formats_with_a_real_minus_sign() {
    assert_eq!(format_delta(4), "+4%");
    assert_eq!(format_delta(-12), "\u{2212}12%");
    assert_eq!(format_delta(0), "±0%");
}

#[test]
fn peak_ties_go_to_the_first_slot() {
    let day = series(48, |i| if i == 22 || i == 24 { 900.0 } else { 100.0 });
    assert_eq!(peak_slot(&day), 22);
}

#[test]
fn last_sun_is_the_last_nonzero_slot() {
    let day = series(48, |i| if (10..=38).contains(&i) { 50.0 } else { 0.0 });
    assert_eq!(last_sun_slot(&day), Some(38));
}
