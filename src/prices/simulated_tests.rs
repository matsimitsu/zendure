use super::*;

const DAY_START: i64 = 1_800_000_000_000 / (24 * HOUR_MS) * (24 * HOUR_MS);

fn at_hour(h: i64) -> Timestamp {
    Timestamp::from_millis(DAY_START + h * HOUR_MS)
}

#[test]
fn is_deterministic() {
    assert_eq!(
        hourly_curve(at_hour(0), at_hour(48)),
        hourly_curve(at_hour(0), at_hour(48))
    );
}

#[test]
fn points_are_whole_utc_hours() {
    let points = hourly_curve(Timestamp::from_millis(DAY_START + 1_234_567), at_hour(30));
    for p in &points {
        assert_eq!(p.from.as_millis() % HOUR_MS, 0);
        assert_eq!(p.until.as_millis() - p.from.as_millis(), HOUR_MS);
    }
    for pair in points.windows(2) {
        assert_eq!(pair[0].until, pair[1].from);
    }
}

#[test]
fn covers_a_range_with_mid_hour_ends() {
    let from = Timestamp::from_millis(DAY_START + 90 * 60_000);
    let until = Timestamp::from_millis(DAY_START + 5 * HOUR_MS + 10 * 60_000);
    let points = hourly_curve(from, until);
    assert!(points.first().unwrap().from <= from);
    assert!(points.last().unwrap().until >= until);
}

#[test]
fn empty_range_yields_nothing() {
    assert!(hourly_curve(at_hour(3), at_hour(3)).is_empty());
}

#[test]
fn evening_costs_more_than_midday_and_midday_dips_below_night() {
    let day = hourly_curve(at_hour(0), at_hour(24));
    let price = |h: usize| day[h].wholesale.0;
    assert!(price(19) > price(12));
    assert!(price(12) < price(3));
}

#[tokio::test]
async fn source_returns_the_curve() {
    let points = SimulatedPrices::new()
        .prices(at_hour(0), at_hour(24))
        .await
        .unwrap();
    assert_eq!(points.len(), 24);
}
