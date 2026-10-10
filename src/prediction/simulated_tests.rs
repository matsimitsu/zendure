use super::*;
use chrono::TimeZone;

fn utc() -> Tz {
    chrono_tz::UTC
}

fn midnight(y: i32, m: u32, d: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, m, d, 0, 0, 0).unwrap()
}

#[test]
fn covers_today_and_tomorrow_in_half_hours() {
    let afternoon = midnight(2026, 6, 1) + chrono::Duration::hours(15);
    let points = clear_sky_curve(afternoon, utc(), PEAK);

    assert_eq!(points.len(), 96);
    assert_eq!(points[0].at, Timestamp::from(midnight(2026, 6, 1)));
    assert_eq!(points[95].at, Timestamp::from(midnight(2026, 6, 3) - SLOT));
}

/// Amsterdam falls back on 25 October 2026, so tomorrow has 50 half-hours.
#[test]
fn follows_the_local_day_across_a_dst_change() {
    let tz = chrono_tz::Europe::Amsterdam;
    let now = tz.with_ymd_and_hms(2026, 10, 24, 15, 0, 0).unwrap();
    let points = clear_sky_curve(now.with_timezone(&Utc), tz, PEAK);

    assert_eq!(points.len(), 48 + 50);
    let today = tz.with_ymd_and_hms(2026, 10, 24, 0, 0, 0).unwrap();
    assert_eq!(points[0].at, Timestamp::from(today));
}

#[test]
fn is_zero_outside_daylight_hours() {
    let points = clear_sky_curve(midnight(2026, 6, 1), utc(), PEAK);
    // Index 0 is 00:00, index 10 is 05:00 — both before the 06:00 start.
    assert_eq!(points[0].estimate.get(), 0.0);
    assert_eq!(points[10].estimate.get(), 0.0);
    // Index 41 is 20:30, after the 20:00 end.
    assert_eq!(points[41].estimate.get(), 0.0);
}

#[test]
fn peaks_at_solar_noon() {
    let points = clear_sky_curve(midnight(2026, 6, 1), utc(), PEAK);
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

/// Any fetch during the same local day returns the same series.
#[test]
fn every_fetch_on_the_same_day_produces_the_same_curve() {
    let morning = clear_sky_curve(midnight(2026, 6, 1), utc(), PEAK);
    let evening = clear_sky_curve(
        midnight(2026, 6, 1) + chrono::Duration::hours(21),
        utc(),
        PEAK,
    );
    assert_eq!(morning, evening);
}
