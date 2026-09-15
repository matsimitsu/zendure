use super::*;
use chrono::TimeZone;

fn midnight(y: i32, m: u32, d: u32) -> chrono::DateTime<chrono::Utc> {
    chrono::Utc.with_ymd_and_hms(y, m, d, 0, 0, 0).unwrap()
}

#[test]
fn produces_forty_eight_half_hourly_points() {
    let points = clear_sky_curve(midnight(2026, 6, 1), PEAK);
    assert_eq!(points.len(), 48);
}

#[test]
fn is_zero_outside_daylight_hours() {
    let points = clear_sky_curve(midnight(2026, 6, 1), PEAK);
    // Index 0 is 00:00, index 10 is 05:00 — both before the 06:00 start.
    assert_eq!(points[0].estimate.get(), 0.0);
    assert_eq!(points[10].estimate.get(), 0.0);
    // Index 41 is 20:30, after the 20:00 end.
    assert_eq!(points[41].estimate.get(), 0.0);
}

#[test]
fn peaks_at_solar_noon() {
    let points = clear_sky_curve(midnight(2026, 6, 1), PEAK);
    // Index 26 is 13:00.
    let noon = points[26].estimate.get();
    assert!(
        (noon - PEAK.as_f64()).abs() < 1.0,
        "expected ~peak at 13:00, got {noon}"
    );

    // Every other daylight hour is no brighter than noon.
    for p in &points {
        assert!(p.estimate.get() <= PEAK.as_f64() + 1e-6);
    }
}

#[test]
fn the_same_starting_instant_always_produces_the_same_curve() {
    let a = clear_sky_curve(midnight(2026, 6, 1), PEAK);
    let b = clear_sky_curve(midnight(2026, 6, 1), PEAK);
    assert_eq!(
        a.iter().map(|p| p.estimate.get()).collect::<Vec<_>>(),
        b.iter().map(|p| p.estimate.get()).collect::<Vec<_>>(),
    );
}
