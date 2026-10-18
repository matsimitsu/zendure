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

#[test]
fn a_point_is_stamped_with_the_start_of_its_period() {
    // The 12:00–12:30 estimate, which Solcast labels by its end.
    let body = r#"{"forecasts":[
        {"pv_estimate":1.0,"period_end":"2026-06-01T12:30:00Z","period":"PT30M"}
    ]}"#;

    let points = parse_forecast_response(body).unwrap();

    let start = chrono::DateTime::parse_from_rfc3339("2026-06-01T12:00:00Z").unwrap();
    assert_eq!(points[0].at, Timestamp::from(start));
}

mod per_site {
    use std::sync::Mutex;

    use chrono::NaiveDate;

    use super::*;
    use crate::schedule::TimeOfDay;

    fn anchor(hour: u32) -> AnchorKey {
        AnchorKey {
            date: NaiveDate::from_ymd_opt(2026, 6, 1).unwrap(),
            slot: TimeOfDay::new(hour, 0).unwrap(),
        }
    }

    fn forecaster() -> SolcastForecaster {
        SolcastForecaster::new("key".into(), "east".into(), "west".into())
    }

    /// Records every site asked for; a site in `failing` answers with an error.
    struct Stub {
        failing: Mutex<Vec<Site>>,
        asked: Mutex<Vec<Site>>,
    }

    impl Stub {
        fn new(failing: &[Site]) -> Self {
            Stub {
                failing: Mutex::new(failing.to_vec()),
                asked: Mutex::new(Vec::new()),
            }
        }

        fn heal(&self) {
            self.failing.lock().unwrap().clear();
        }

        fn asked_sorted(&self) -> Vec<Site> {
            let mut asked = std::mem::take(&mut *self.asked.lock().unwrap());
            asked.sort_by_key(|site| *site == Site::West);
            asked
        }

        async fn fetch(&self, site: Site) -> Result<Series, FetchError> {
            self.asked.lock().unwrap().push(site);
            if self.failing.lock().unwrap().contains(&site) {
                return Err(FetchError::Status(reqwest::StatusCode::BAD_GATEWAY));
            }
            let watts = match site {
                Site::East => 100.0,
                Site::West => 50.0,
            };
            Ok(vec![point(1_000, watts)])
        }
    }

    #[tokio::test]
    async fn both_sites_succeeding_costs_one_request_each() {
        let f = forecaster();
        let stub = Stub::new(&[]);

        let series = f.forecast_with(anchor(6), |s| stub.fetch(s)).await.unwrap();

        assert_eq!(series[0].estimate.get(), 150.0);
        assert_eq!(stub.asked_sorted(), vec![Site::East, Site::West]);
    }

    #[tokio::test]
    async fn a_repeat_of_the_anchor_fetches_only_the_failed_site() {
        let f = forecaster();
        let stub = Stub::new(&[Site::West]);

        let first = f.forecast_with(anchor(6), |s| stub.fetch(s)).await;
        assert!(first.unwrap_err().is_transient());
        stub.asked_sorted();
        stub.heal();

        let series = f.forecast_with(anchor(6), |s| stub.fetch(s)).await.unwrap();

        assert_eq!(stub.asked_sorted(), vec![Site::West]);
        assert_eq!(series[0].estimate.get(), 150.0);
    }

    #[tokio::test]
    async fn a_new_anchor_drops_a_half_and_fetches_both() {
        let f = forecaster();
        let stub = Stub::new(&[Site::West]);
        assert!(f.forecast_with(anchor(6), |s| stub.fetch(s)).await.is_err());
        stub.asked_sorted();
        stub.heal();

        f.forecast_with(anchor(9), |s| stub.fetch(s)).await.unwrap();

        assert_eq!(stub.asked_sorted(), vec![Site::East, Site::West]);
    }

    #[tokio::test]
    async fn a_refusing_site_outranks_an_outage_on_the_other() {
        let f = forecaster();
        let error = f
            .forecast_with(anchor(6), |site| async move {
                Err(FetchError::Status(match site {
                    Site::East => reqwest::StatusCode::BAD_GATEWAY,
                    Site::West => reqwest::StatusCode::TOO_MANY_REQUESTS,
                }))
            })
            .await
            .unwrap_err();

        assert!(!error.is_transient());
    }
}
