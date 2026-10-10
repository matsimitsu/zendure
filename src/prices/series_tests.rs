use super::*;

fn point(from: i64, until: i64, ct: f64) -> PricePoint {
    PricePoint {
        from: Timestamp::from_millis(from),
        until: Timestamp::from_millis(until),
        wholesale: CentsPerKwh(ct),
    }
}

#[test]
fn price_series_at_boundaries_and_gaps() {
    let mut series = PriceSeries::default();
    series.insert(point(100, 200, 1.0));
    series.insert(point(200, 300, 2.0));
    series.insert(point(400, 500, 3.0));
    let ct = |ts| series.at(Timestamp::from_millis(ts)).map(|p| p.wholesale);
    assert_eq!(ct(100), Some(CentsPerKwh(1.0)));
    assert_eq!(ct(199), Some(CentsPerKwh(1.0)));
    assert_eq!(ct(200), Some(CentsPerKwh(2.0)));
    assert_eq!(ct(99), None);
    assert_eq!(ct(300), None);
    assert_eq!(ct(399), None);
    assert_eq!(ct(500), None);
}

#[test]
fn price_series_insert_replaces_same_from() {
    let mut series = PriceSeries::default();
    series.insert(point(100, 200, 1.0));
    series.insert(point(100, 160, 9.0));
    assert_eq!(
        series.at(Timestamp::from_millis(150)).map(|p| p.wholesale),
        Some(CentsPerKwh(9.0))
    );
    assert!(series.at(Timestamp::from_millis(170)).is_none());
}
