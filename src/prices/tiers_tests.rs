use super::*;

fn c(v: f64) -> CentsPerKwh {
    CentsPerKwh(v)
}

fn day(prices: &[f64]) -> Vec<Option<CentsPerKwh>> {
    prices.iter().map(|&p| Some(c(p))).collect()
}

#[test]
fn thresholds_are_the_thirds_ranks_of_24_hours() {
    let prices: Vec<_> = (0..24).map(|h| c(f64::from(h))).collect();
    assert_eq!(tiers(&prices), Some((c(8.0), c(16.0))));
}

#[test]
fn thresholds_work_on_dst_slot_counts() {
    let p23: Vec<_> = (0..23).map(|h| c(f64::from(h))).collect();
    let p25: Vec<_> = (0..25).map(|h| c(f64::from(h))).collect();
    assert_eq!(tiers(&p23), Some((c(7.0), c(15.0))));
    assert_eq!(tiers(&p25), Some((c(8.0), c(16.0))));
}

#[test]
fn no_prices_means_no_thresholds() {
    assert_eq!(tiers(&[]), None);
}

#[test]
fn tier_boundaries_and_ties() {
    let t = (c(8.0), c(16.0));
    assert_eq!(Tier::of(c(7.9), t), Tier::Cheap);
    assert_eq!(Tier::of(c(8.0), t), Tier::Normal);
    assert_eq!(Tier::of(c(15.9), t), Tier::Normal);
    assert_eq!(Tier::of(c(16.0), t), Tier::Expensive);
    // A flat day has nothing cheap, everything expensive.
    let flat = tiers(&[c(5.0); 24]).unwrap();
    assert_eq!(Tier::of(c(5.0), flat), Tier::Expensive);
}

#[test]
fn negative_prices_are_cheap() {
    let mut prices: Vec<_> = (10..34).map(|h| c(f64::from(h))).collect();
    prices[3] = c(-4.0);
    let t = tiers(&prices).unwrap();
    assert_eq!(Tier::of(c(-4.0), t), Tier::Cheap);
}

#[test]
fn tier_names() {
    assert_eq!(Tier::Cheap.class_suffix(), "cheap");
    assert_eq!(Tier::Expensive.label(), "Expensive");
}

#[test]
fn finds_cheapest_and_priciest_block() {
    let d = day(&[5.0, 5.0, 1.0, 1.0, 1.0, 9.0, 9.0, 9.0, 5.0]);
    let cheap = cheapest_block(&d, Slot(0)).unwrap();
    assert_eq!(
        (cheap.start, cheap.end(), cheap.mean),
        (Slot(2), Slot(5), c(1.0))
    );
    assert_eq!(priciest_block(&d, Slot(0)).unwrap().start, Slot(5));
}

#[test]
fn ties_go_to_the_earliest_block() {
    let d = day(&[2.0; 6]);
    assert_eq!(cheapest_block(&d, Slot(0)).unwrap().start, Slot(0));
    assert_eq!(priciest_block(&d, Slot(0)).unwrap().start, Slot(0));
}

#[test]
fn only_blocks_starting_at_or_after_from_count() {
    let d = day(&[0.0, 0.0, 0.0, 5.0, 5.0, 5.0, 9.0, 9.0]);
    assert_eq!(cheapest_block(&d, Slot(1)).unwrap().start, Slot(1));
    assert_eq!(cheapest_block(&d, Slot(3)).unwrap().start, Slot(3));
}

#[test]
fn fewer_than_three_hours_left_is_none() {
    let d = day(&[1.0; 24]);
    assert!(cheapest_block(&d, Slot(21)).is_some());
    assert_eq!(cheapest_block(&d, Slot(22)), None);
    assert_eq!(priciest_block(&d, Slot(24)), None);
}

#[test]
fn blocks_never_cross_the_end_of_a_dst_day() {
    for len in [23usize, 25] {
        let mut prices = vec![5.0; len];
        prices[len - 1] = -50.0;
        let b = cheapest_block(&day(&prices), Slot(0)).unwrap();
        assert_eq!(b.start, Slot(len - 3));
        assert_eq!(b.end(), Slot(len));
    }
}

#[test]
fn negative_prices_win_cheapest_and_lose_priciest() {
    let d = day(&[3.0, -1.0, -2.0, -3.0, 3.0, 3.0]);
    assert_eq!(cheapest_block(&d, Slot(0)).unwrap().start, Slot(1));
    assert_eq!(priciest_block(&d, Slot(0)).unwrap().start, Slot(3));
}

#[test]
fn a_block_with_an_unpriced_hour_is_skipped() {
    let mut d = day(&[1.0, 1.0, 1.0, 5.0, 5.0, 5.0]);
    d[1] = None;
    // Blocks 0..3 and 1..4 touch the gap; 2..5 is the cheapest whole one.
    assert_eq!(cheapest_block(&d, Slot(0)).unwrap().start, Slot(2));
    let mut gappy = day(&[1.0; 6]);
    gappy[2] = None;
    gappy[3] = None;
    assert_eq!(cheapest_block(&gappy, Slot(0)), None);
}
