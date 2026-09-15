use super::*;

fn point(ms: i64, watts: f64) -> SolarForecastPoint {
    SolarForecastPoint {
        at: Timestamp::from_millis(ms),
        estimate: SolarPower::new(watts),
    }
}

#[test]
fn parses_a_realistic_response_converting_kw_to_w() {
    let body = r#"{"forecasts":[
        {"pv_estimate":1.234,"pv_estimate10":1.0,"pv_estimate90":1.5,"period_end":"2026-06-01T12:00:00.0000000Z","period":"PT30M"},
        {"pv_estimate":0.0,"pv_estimate10":0.0,"pv_estimate90":0.0,"period_end":"2026-06-01T12:30:00.0000000Z","period":"PT30M"}
    ]}"#;

    let points = parse_forecast_response(body).unwrap();

    assert_eq!(points.len(), 2);
    assert!((points[0].estimate.get() - 1234.0).abs() < 1e-6);
    assert_eq!(points[1].estimate.get(), 0.0);
}

#[test]
fn malformed_json_is_an_error_not_a_panic() {
    assert!(parse_forecast_response("not valid json").is_err());
}

#[test]
fn an_entry_with_an_unparseable_period_end_is_skipped_not_fatal() {
    let body = r#"{"forecasts":[
        {"pv_estimate":1.0,"period_end":"not a timestamp"},
        {"pv_estimate":2.0,"period_end":"2026-06-01T12:00:00Z"}
    ]}"#;

    let points = parse_forecast_response(body).unwrap();

    assert_eq!(points.len(), 1);
    assert_eq!(points[0].estimate.get(), 2000.0);
}

#[test]
fn combine_series_sums_matching_timestamps() {
    let east = vec![point(1_000, 100.0), point(2_000, 200.0)];
    let west = vec![point(1_000, 50.0)];

    let combined = combine_series(&east, &west);

    assert_eq!(combined.len(), 2);
    assert_eq!(combined[0].at, Timestamp::from_millis(1_000));
    assert_eq!(combined[0].estimate.get(), 150.0);
    assert_eq!(combined[1].at, Timestamp::from_millis(2_000));
    assert_eq!(combined[1].estimate.get(), 200.0);
}

#[test]
fn combine_series_of_two_empties_is_empty() {
    assert!(combine_series(&[], &[]).is_empty());
}

#[test]
fn combine_series_output_is_sorted_by_time() {
    let east = vec![point(3_000, 1.0), point(1_000, 2.0)];
    let west = vec![point(2_000, 3.0)];

    let combined = combine_series(&east, &west);

    let times: Vec<i64> = combined.iter().map(|p| p.at.as_millis()).collect();
    assert_eq!(times, vec![1_000, 2_000, 3_000]);
}
