use super::*;

use crate::prices::PriceSeries;
use crate::units::{Elapsed, Percent};
use crate::web::templates::price_panel;

fn tz() -> Tz {
    chrono_tz::Europe::Amsterdam
}

fn date(day: u64) -> NaiveDate {
    NaiveDate::from_ymd_opt(2025, 9, 4).unwrap() + chrono::Days::new(day)
}

/// Amsterdam wall-clock time, `day` days after 4 September 2025.
fn local(day: u64, hour: u32, minute: u32) -> Timestamp {
    use chrono::TimeZone;
    let local = date(day).and_hms_opt(hour, minute, 0).unwrap();
    Timestamp::from(tz().from_local_datetime(&local).single().unwrap())
}

fn price(day: u64, hour: u32, cents: f64) -> PricePoint {
    PricePoint {
        from: local(day, hour, 0),
        until: local(day, hour, 0) + Elapsed::HOUR,
        wholesale: CentsPerKwh(cents),
    }
}

/// Every hour of local `day`, priced `cents(hour)`.
fn whole_day(day: u64, cents: impl Fn(u32) -> f64) -> Vec<PricePoint> {
    (0..24).map(|hour| price(day, hour, cents(hour))).collect()
}

/// 1, 2, …, 24 ct: the cheap third is 00:00–07:00, the expensive 16:00 on.
fn rising(hour: u32) -> f64 {
    f64::from(hour + 1)
}

fn snapshot(points: &[PricePoint]) -> PriceSnapshot {
    let mut series = PriceSeries::default();
    for point in points {
        series.insert(*point);
    }
    PriceSnapshot {
        points: series,
        as_of: Some(local(0, 13, 2)),
    }
}

fn view_at(day: NaiveDate, snapshot: &PriceSnapshot, now: Timestamp) -> PricePanelView {
    let context = PriceContext {
        snapshot,
        tariff: None,
        configured: true,
    };
    price_panel_view(day, &context, now, tz())
}

fn priced(view: PricePanelView) -> PricedDayView {
    match view {
        PricePanelView::Priced(day) => *day,
        PricePanelView::Empty(reason) => panic!("empty: {reason:?}"),
    }
}

fn empty(view: PricePanelView) -> EmptyReason {
    match view {
        PricePanelView::Empty(reason) => reason,
        PricePanelView::Priced(_) => panic!("priced"),
    }
}

// --- Empty states -------------------------------------------------------------

#[test]
fn without_a_feed_the_panel_says_how_to_configure_prices() {
    let snapshot = snapshot(&whole_day(0, rising));
    let context = PriceContext {
        snapshot: &snapshot,
        tariff: None,
        configured: false,
    };
    let view = price_panel_view(date(0), &context, local(0, 12, 0), tz());

    assert_eq!(empty(view), EmptyReason::NotConfigured);
}

#[test]
fn a_feed_with_nothing_for_today_is_waiting() {
    let view = view_at(date(1), &snapshot(&whole_day(0, rising)), local(1, 12, 0));

    assert_eq!(empty(view), EmptyReason::Waiting);
}

#[test]
fn another_day_without_prices_is_unpriced() {
    let view = view_at(date(0), &snapshot(&whole_day(1, rising)), local(1, 12, 0));

    assert_eq!(empty(view), EmptyReason::Unpriced);
}

// --- Slots ----------------------------------------------------------------------

/// Every UTC hour from a local midnight.
fn utc_hours(first: &str, count: usize) -> Vec<PricePoint> {
    let start = Timestamp::from(chrono::DateTime::parse_from_rfc3339(first).unwrap());
    std::iter::successors(Some(start), |from| Some(*from + Elapsed::HOUR))
        .zip([5.0, 10.0, 20.0].into_iter().cycle())
        .take(count)
        .map(|(from, cents)| PricePoint {
            from,
            until: from + Elapsed::HOUR,
            wholesale: CentsPerKwh(cents),
        })
        .collect()
}

fn utc(at: &str) -> Timestamp {
    Timestamp::from(chrono::DateTime::parse_from_rfc3339(at).unwrap())
}

/// Hits tile the chart left to right without overlapping or overrunning it.
fn assert_tiled(hits: &[PriceHitView]) {
    let mut edge = 0.0;
    for hit in hits {
        assert!((hit.span.x - edge).abs() < 1e-6, "gap or overlap at {edge}");
        edge = hit.span.x + hit.span.width;
    }
    assert!((edge - PRICE_CHART_WIDTH).abs() < 1e-6);
}

#[test]
fn an_ordinary_day_has_a_slot_per_hour() {
    let view = priced(view_at(
        date(0),
        &snapshot(&whole_day(0, rising)),
        local(0, 12, 0),
    ));

    assert_eq!(view.chart.bars.len(), 24);
    assert_eq!(view.chart.hits.len(), 24);
    assert_tiled(&view.chart.hits);
    assert_eq!(view.chart.hits[13].readout.label, "13:00–14:00");
}

#[test]
fn the_long_dst_day_has_25_slots_and_repeats_two_oclock() {
    let points = utc_hours("2026-10-24T22:00:00Z", 25);
    let day = NaiveDate::from_ymd_opt(2026, 10, 25).unwrap();
    let view = priced(view_at(
        day,
        &snapshot(&points),
        utc("2026-10-25T11:00:00Z"),
    ));

    assert_eq!(view.chart.hits.len(), 25);
    assert_tiled(&view.chart.hits);
    let labels: Vec<_> = view.chart.hits[1..4]
        .iter()
        .map(|hit| hit.readout.label.as_str())
        .collect();
    assert_eq!(labels, ["01:00–02:00", "02:00–02:00", "02:00–03:00"]);
}

#[test]
fn the_short_dst_day_has_23_slots_and_skips_two_oclock() {
    let points = utc_hours("2026-03-28T23:00:00Z", 23);
    let day = NaiveDate::from_ymd_opt(2026, 3, 29).unwrap();
    let view = priced(view_at(
        day,
        &snapshot(&points),
        utc("2026-03-29T10:00:00Z"),
    ));

    assert_eq!(view.chart.hits.len(), 23);
    assert_tiled(&view.chart.hits);
    assert_eq!(view.chart.hits[2].readout.label, "03:00–04:00");
}

#[test]
fn an_unpriced_hour_has_no_bar_and_no_hit() {
    let mut points = whole_day(0, rising);
    points.remove(5);
    let view = priced(view_at(date(0), &snapshot(&points), local(0, 12, 0)));

    assert_eq!(view.chart.bars.len(), 23);
    assert_eq!(view.chart.hits.len(), 23);
}

#[test]
fn quarter_hour_prices_are_averaged_into_their_hour() {
    let quarters: Vec<PricePoint> = (0..4)
        .map(|quarter| {
            let from = local(0, 12, quarter * 15);
            PricePoint {
                from,
                until: from + Elapsed::of(Duration::from_secs(15 * 60)),
                wholesale: CentsPerKwh(f64::from(quarter) * 2.0),
            }
        })
        .collect();
    let view = priced(view_at(date(0), &snapshot(&quarters), local(0, 12, 30)));

    assert_eq!(view.chart.hits.len(), 1);
    assert_eq!(view.readout.value, "3.0");
}

// --- Scale --------------------------------------------------------------------

#[test]
fn the_scale_rounds_the_highest_price_up_to_ten_and_has_no_zero_line() {
    let view = priced(view_at(
        date(0),
        &snapshot(&whole_day(0, |_| 23.4)),
        local(0, 12, 0),
    ));

    let labels: Vec<_> = view.chart.y_axis.iter().map(|t| t.label.as_str()).collect();
    assert_eq!(labels, ["0", "10", "20", "30"]);
    assert_eq!(view.chart.grid_lines.len(), 4);
    assert_eq!(view.chart.zero_y, None, "the bottom grid line is zero");
}

#[test]
fn the_scale_spans_every_day_in_the_snapshot() {
    let mut points = whole_day(0, rising);
    points.extend(whole_day(1, |_| 45.0));
    let view = priced(view_at(date(0), &snapshot(&points), local(0, 12, 0)));

    assert_eq!(view.chart.y_axis.last().unwrap().label, "50");
}

#[test]
fn a_negative_price_extends_the_scale_below_zero_and_draws_the_zero_line() {
    let points = whole_day(0, |hour| if hour == 13 { -4.0 } else { 12.0 });
    let view = priced(view_at(date(0), &snapshot(&points), local(0, 9, 0)));

    let labels: Vec<_> = view.chart.y_axis.iter().map(|t| t.label.as_str()).collect();
    assert_eq!(labels, ["−10", "0", "10", "20"]);
    let zero = view.chart.zero_y.expect("a zero line");
    let negative = view.chart.bars[13];
    assert!(negative.negative);
    assert_eq!(negative.tier, Tier::Cheap);
    assert!((negative.y - zero).abs() < 1e-9, "hangs from the zero line");
    let positive = view.chart.bars[12];
    assert!((positive.y + positive.height - zero).abs() < 1e-9);
    assert_eq!(view.chart.hits[13].readout.value, "−4.0");
}

// --- Today ----------------------------------------------------------------------

#[test]
fn today_reads_the_current_hour_and_dims_the_hours_before_it() {
    let view = priced(view_at(
        date(0),
        &snapshot(&whole_day(0, rising)),
        local(0, 13, 20),
    ));

    assert_eq!(view.readout.label, "Now · 13:00–14:00");
    assert_eq!(view.readout.value, "14.0");
    assert_eq!(view.readout.tier, Some(Tier::Normal));
    let past: Vec<_> = view.chart.bars.iter().map(|bar| bar.past).collect();
    assert!(past[..13].iter().all(|&p| p));
    assert!(
        past[13..].iter().all(|&p| !p),
        "the current hour is not past"
    );
    assert_eq!(view.chart.highlight, Some(view.chart.hits[13].span));
    let now_x = view.chart.now_x.expect("a now line");
    assert!(now_x > view.chart.hits[13].span.x);
}

#[test]
fn today_looks_ahead_for_its_windows() {
    let view = priced(view_at(
        date(0),
        &snapshot(&whole_day(0, rising)),
        local(0, 13, 20),
    ));

    let [cheapest, priciest] = view.windows.expect("windows");
    assert_eq!(cheapest.label, "Cheapest 3 h ahead");
    assert_eq!(
        cheapest.value, "13:00–16:00",
        "not the cheaper hours gone by"
    );
    assert_eq!(cheapest.sub.unwrap().text, "15.0 ct avg");
    assert_eq!(priciest.label, "Priciest 3 h ahead");
    assert_eq!(priciest.value, "21:00–24:00");
    assert_eq!(priciest.sub.unwrap().tone, "expensive");
}

#[test]
fn the_windows_hide_once_no_full_block_is_left() {
    let view = priced(view_at(
        date(0),
        &snapshot(&whole_day(0, rising)),
        local(0, 22, 0),
    ));

    assert!(view.windows.is_none());
}

#[test]
fn an_unpriced_current_hour_reads_as_missing() {
    let view = priced(view_at(
        date(0),
        &snapshot(&[price(0, 10, 5.0)]),
        local(0, 12, 0),
    ));

    assert_eq!(view.readout.label, "Now · 12:00–13:00");
    assert_eq!(view.readout.value, "—");
    assert_eq!(view.readout.tier, None);
}

#[test]
fn the_legend_states_the_days_thresholds() {
    let view = priced(view_at(
        date(0),
        &snapshot(&whole_day(0, rising)),
        local(0, 12, 0),
    ));

    let ranges: Vec<_> = view.legend.iter().map(|item| item.range.as_str()).collect();
    assert_eq!(ranges, ["< 9.0", "9.0–17.0", "≥ 17.0"]);
}

// --- Another day ----------------------------------------------------------------

#[test]
fn another_day_reads_its_average_with_no_now() {
    let mut points = whole_day(0, rising);
    points.extend(whole_day(1, |_| 8.0));
    let view = priced(view_at(date(0), &snapshot(&points), local(1, 12, 0)));

    assert_eq!(view.readout.label, "Day average");
    assert_eq!(view.readout.value, "12.5");
    assert_eq!(view.chart.now_x, None);
    assert_eq!(view.chart.highlight, None);
    assert!(view.chart.bars.iter().all(|bar| !bar.past));
    let [cheapest, priciest] = view.windows.expect("windows");
    assert_eq!(cheapest.label, "Cheapest 3 h");
    assert_eq!(cheapest.value, "00:00–03:00", "the whole day counts");
    assert_eq!(priciest.label, "Priciest 3 h");
}

// --- Nav ------------------------------------------------------------------------

#[test]
fn today_steps_back_to_yesterday_and_not_ahead_before_tomorrow_is_published() {
    let mut points = whole_day(0, rising);
    points.push(price(1, 0, 5.0));
    let view = priced(view_at(date(0), &snapshot(&points), local(0, 12, 0)));

    assert_eq!(view.nav.label, "Today");
    assert_eq!(view.nav.previous, Some(date(0).pred_opt().unwrap()));
    assert_eq!(view.nav.next, None, "tomorrow is only partly priced");
    assert_eq!(view.nav.data_day(), "today");
}

#[test]
fn today_steps_ahead_once_tomorrow_is_fully_priced() {
    let mut points = whole_day(0, rising);
    points.extend(whole_day(1, rising));
    let view = priced(view_at(date(0), &snapshot(&points), local(0, 12, 0)));

    assert_eq!(view.nav.next, Some(date(1)));
}

#[test]
fn the_nav_stops_six_days_back_and_at_tomorrow() {
    let mut points = whole_day(0, rising);
    points.extend(whole_day(1, rising));
    let oldest = priced(view_at(date(0), &snapshot(&points), local(6, 12, 0)));
    let tomorrow = priced(view_at(date(1), &snapshot(&points), local(0, 12, 0)));

    assert_eq!(oldest.nav.previous, None);
    assert_eq!(oldest.nav.label, "Thu 4 Sep");
    assert_eq!(oldest.nav.data_day(), "2025-09-04");
    assert_eq!(tomorrow.nav.label, "Tomorrow");
    assert_eq!(tomorrow.nav.next, None);
}

// --- Labels -------------------------------------------------------------------

#[test]
fn the_subtitle_names_the_price_shown_and_when_it_was_fetched() {
    let snapshot = snapshot(&whole_day(0, rising));
    let wholesale = priced(view_at(date(0), &snapshot, local(0, 12, 0)));
    let tariff = DynamicTariff {
        markup: CentsPerKwh(2.0),
        energy_tax: CentsPerKwh(10.0),
        export_markup: CentsPerKwh(1.0),
        vat: Percent(21.0),
    };
    let context = PriceContext {
        snapshot: &snapshot,
        tariff: Some(&tariff),
        configured: true,
    };
    let all_in = priced(price_panel_view(date(0), &context, local(0, 11, 30), tz()));

    assert_eq!(wholesale.subtitle, "Wholesale price · fetched 13:02");
    assert_eq!(
        all_in.subtitle,
        "All-in import price incl. VAT · fetched 13:02"
    );
    // (12 + 2 + 10) × 1.21
    assert_eq!(all_in.readout.value, "29.0");
}

// --- Markup ---------------------------------------------------------------------

#[test]
fn the_markup_carries_what_the_readout_script_reads() {
    let view = view_at(date(0), &snapshot(&whole_day(0, rising)), local(0, 13, 20));
    let html = price_panel::render(&view).into_string();

    assert_eq!(html.matches("class=\"price-panel__hit\"").count(), 24);
    assert!(html.contains(
        "data-label=\"15:00–16:00\" data-value=\"16.0\" data-tier=\"normal\" data-tier-label=\"Normal\""
    ));
    assert!(html.contains("data-default-label=\"Now · 13:00–14:00\""));
    assert!(html.contains("data-default-tier=\"normal\""));
    assert!(html.contains("price-tier price-tier--normal"));
    assert!(html.contains("data-day=\"today\""));
    assert!(html.contains("hx-get=\"/fragments/price-panel?day=2025-09-03\""));
    assert!(html.contains("href=\"/?price_day=2025-09-03\""));
    assert!(html.contains("price-panel__bar price-panel__bar--cheap price-panel__bar--past"));
    assert!(html.contains("price-panel__now-line"));
    assert!(!html.contains("price-panel__zero"));
}
