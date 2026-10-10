use super::*;
use crate::config::{PriceKind, PricesConfig};
use crate::prices::PriceFeed;

/// A response of `count` hourly points from `start_ms`, priced `base + i/100`
/// €/kWh. Built rather than pasted so the DST days' point counts are the thing
/// the test states.
fn body(start: &str, count: usize, base: f64) -> String {
    let start = DateTime::parse_from_rfc3339(start).unwrap().timestamp();
    let entries: Vec<String> = (0..count)
        .map(|i| {
            let at = DateTime::<Utc>::from_timestamp(start + i as i64 * 3600, 0).unwrap();
            format!(
                r#"{{"readingDate":"{}","price":{}}}"#,
                at.to_rfc3339_opts(SecondsFormat::Secs, true),
                base + i as f64 / 100.0
            )
        })
        .collect();
    format!(r#"{{"Prices":[{}]}}"#, entries.join(","))
}

fn assert_contiguous(points: &[PricePoint]) {
    for pair in points.windows(2) {
        assert_eq!(pair[0].until, pair[1].from);
    }
    assert_eq!(
        points.last().unwrap().until.as_millis() - points.last().unwrap().from.as_millis(),
        HOUR_MS
    );
}

#[test]
fn normal_day_has_24_contiguous_points() {
    let points = parse_prices_response(&body("2026-10-09T22:00:00Z", 24, 0.02)).unwrap();
    assert_eq!(points.len(), 24);
    assert_contiguous(&points);
    assert_eq!(
        points[0].from.as_millis(),
        DateTime::parse_from_rfc3339("2026-10-09T22:00:00Z")
            .unwrap()
            .timestamp_millis()
    );
}

#[test]
fn euros_become_cents() {
    let points = parse_prices_response(
        r#"{"Prices":[{"readingDate":"2026-10-09T22:00:00Z","price":0.02}]}"#,
    )
    .unwrap();
    assert!((points[0].wholesale.0 - 2.0).abs() < 1e-9);
}

#[test]
fn spring_forward_day_has_23_points() {
    let points = parse_prices_response(&body("2026-03-28T23:00:00Z", 23, 0.0)).unwrap();
    assert_eq!(points.len(), 23);
    assert_contiguous(&points);
}

#[test]
fn fall_back_day_has_25_points() {
    let points = parse_prices_response(&body("2026-10-24T22:00:00Z", 25, 0.0)).unwrap();
    assert_eq!(points.len(), 25);
    assert_contiguous(&points);
}

#[test]
fn negative_prices_stay_negative() {
    let points = parse_prices_response(&body("2026-10-09T22:00:00Z", 3, -0.05)).unwrap();
    assert!((points[0].wholesale.0 - -5.0).abs() < 1e-9);
    assert!(points.iter().take(3).all(|p| p.wholesale.0 < 0.0));
}

#[test]
fn malformed_body_is_an_error() {
    assert!(parse_prices_response("<html>rate limited</html>").is_err());
    assert!(parse_prices_response(r#"{"Prices":[{"readingDate":"nope","price":1}]}"#).is_err());
}

#[test]
fn empty_prices_array_is_no_points() {
    assert!(
        parse_prices_response(r#"{"Prices":[]}"#)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn iso_millis_is_utc_with_millis() {
    let ts = Timestamp::from_millis(1_791_583_200_000);
    assert_eq!(iso_millis(ts), "2026-10-09T22:00:00.000Z");
}

fn config(kind: PriceKind) -> PricesConfig {
    PricesConfig {
        kind,
        poll_times: vec![],
        backfill_days: 1,
        dynamic: None,
        fixed: None,
    }
}

#[test]
fn from_config_picks_the_backend() {
    assert!(matches!(
        PriceFeed::from_config(&config(PriceKind::EnergyZero)),
        PriceFeed::EnergyZero(_)
    ));
    assert!(matches!(
        PriceFeed::from_config(&config(PriceKind::Simulated)),
        PriceFeed::Simulated(_)
    ));
}
