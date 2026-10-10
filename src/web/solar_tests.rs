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
    let delta = |actual| delta_pct(KiloWattHours(actual), KiloWattHours(1.0)).map(format_delta);
    assert_eq!(delta(1.04).as_deref(), Some("+4%"));
    assert_eq!(delta(1.046).as_deref(), Some("+5%"));
    assert_eq!(delta(0.5).as_deref(), Some("\u{2212}50%"));
    assert_eq!(delta(1.004).as_deref(), Some("±0%"));
    assert_eq!(delta(0.996).as_deref(), Some("±0%"));
}

#[test]
fn delta_formats_with_a_real_minus_sign() {
    assert_eq!(format_delta(Percent(4.0)), "+4%");
    assert_eq!(format_delta(Percent(-12.0)), "\u{2212}12%");
    assert_eq!(format_delta(Percent(0.0)), "±0%");
    assert_eq!(format_delta(Percent(-0.0)), "±0%");
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

// --- Panel ------------------------------------------------------------------

use chrono::{NaiveDate, TimeZone};

use crate::fixtures::{amsterdam, date, journey, local};
use crate::units::SolarForecastPoint;
use crate::web::plot::{CHART_WIDTH, testing};
use crate::web::templates::forecast_panel;

/// Every half-hour slot start of local `day`.
fn slot_starts(day: NaiveDate) -> impl Iterator<Item = Timestamp> {
    Day::of(day, amsterdam()).slot_starts(SOLAR_SLOT)
}

/// A bell peaking at 2,000 W in slot 26 (13:00 on a 48-slot day), with sun
/// from slot 17 to slot 35.
fn bell(slot: usize) -> f64 {
    (2000.0 - slot.abs_diff(26) as f64 * 200.0).max(0.0)
}

/// One forecast point per slot of each of `days`, `watts(day, slot)`.
fn forecast(days: &[NaiveDate], watts: impl Fn(NaiveDate, usize) -> f64) -> ForecastSnapshot {
    let points = days
        .iter()
        .flat_map(|&day| {
            slot_starts(day)
                .enumerate()
                .map(move |(slot, at)| (day, slot, at))
        })
        .map(|(day, slot, at)| SolarForecastPoint {
            at,
            estimate: SolarPower::new(watts(day, slot)),
        })
        .collect();
    ForecastSnapshot {
        points,
        as_of: Some(local(6, 15, 6, 0)),
    }
}

/// One meter reading at the start of each of `day`'s first `slots` slots.
fn measured(day: NaiveDate, slots: usize, watts: impl Fn(usize) -> f64) -> IntervalHistory {
    let mut intervals = journey::interval_ring();
    for (slot, at) in slot_starts(day).take(slots).enumerate() {
        intervals.record(&journey::meter_event(at, 0.0, watts(slot)));
    }
    intervals
}

fn view(
    day: Option<NaiveDate>,
    forecast: &ForecastSnapshot,
    intervals: &IntervalHistory,
    now: Timestamp,
) -> ForecastPanelView {
    let context = SolarContext {
        forecast,
        intervals,
        configured: true,
    };
    let query = day.map_or_else(DayQuery::default, DayQuery::on);
    forecast_panel_view(query, &context, now, amsterdam())
}

fn shown(view: ForecastPanelView) -> ForecastDayView {
    match view {
        DayPanel::Shown(day) => *day,
        DayPanel::Empty(reason) => panic!("empty: {reason:?}"),
        DayPanel::Blank(nav) => panic!("blank: {nav:?}"),
    }
}

fn empty(view: ForecastPanelView) -> EmptyReason {
    match view {
        DayPanel::Empty(reason) => reason,
        _ => panic!("not empty"),
    }
}

fn render(view: ForecastDayView) -> String {
    forecast_panel::render(&DayPanel::Shown(Box::new(view))).into_string()
}

fn assert_tiled(hits: &[ForecastHitView]) {
    testing::assert_tiled(hits.iter().map(|hit| hit.span));
}

#[test]
fn without_a_feed_the_panel_says_how_to_configure_one() {
    let forecast = forecast(&[date(6, 15)], |_, slot| bell(slot));
    let context = SolarContext {
        forecast: &forecast,
        intervals: &journey::interval_ring(),
        configured: false,
    };
    let view = forecast_panel_view(
        DayQuery::default(),
        &context,
        local(6, 15, 12, 0),
        amsterdam(),
    );

    assert_eq!(empty(view), EmptyReason::NotConfigured);
}

#[test]
fn a_feed_with_nothing_fetched_waits_and_stays_live_without_a_nav() {
    let view = view(
        None,
        &ForecastSnapshot::default(),
        &journey::interval_ring(),
        local(6, 15, 12, 0),
    );
    assert_eq!(view.data_day(), None);
    assert!(view.data_live());

    let html = forecast_panel::render(&view).into_string();
    assert!(html.contains("Waiting for the solar forecast…"));
    assert!(html.contains(r#"data-live="true""#));
    assert!(!html.contains("day-nav"));
    assert_eq!(empty(view), EmptyReason::Waiting);
}

#[test]
fn a_dst_day_has_a_slot_per_half_hour_of_its_real_length() {
    for (day, slots, three_oclock) in [
        (date(3, 29), 46, 2.0 / 23.0),
        (date(6, 15), 48, 3.0 / 24.0),
        (date(10, 25), 50, 4.0 / 25.0),
    ] {
        let forecast = forecast(&[day], |_, slot| bell(slot));
        let noon = Timestamp::from(
            amsterdam()
                .from_local_datetime(&day.and_hms_opt(12, 0, 0).unwrap())
                .unwrap(),
        );
        let view = shown(view(None, &forecast, &journey::interval_ring(), noon));

        assert_eq!(view.chart.hits.len(), slots, "{day}");
        assert_tiled(&view.chart.hits);
        let tick = view
            .chart
            .plot
            .x_axis
            .iter()
            .find(|tick| tick.label == "03:00")
            .unwrap();
        assert!(
            (tick.position.percent() - three_oclock * 100.0).abs() < 1e-6,
            "{day}: 03:00 at {}%",
            tick.position.percent()
        );
    }
}

#[test]
fn todays_readout_defaults_to_the_current_slot() {
    let today = date(6, 15);
    let forecast = forecast(&[today], |_, slot| bell(slot) + 50.0);
    let intervals = measured(today, 24, |_| 1410.0);
    let view = shown(view(None, &forecast, &intervals, local(6, 15, 12, 10)));

    assert_eq!(
        view.readout,
        SolarReadoutView {
            label: "Now · 12:00–12:30".to_string(),
            forecast: SolarFigure {
                value: "1,650".to_string(),
                unit: "W",
            },
            actual: Some(SolarFigure {
                value: "—".to_string(),
                unit: "",
            }),
        }
    );
    let html = render(view);
    assert!(html.contains(r#"data-default-label="Now · 12:00–12:30""#));
    assert!(html.contains(r#"data-default-forecast="1,650" data-default-forecast-unit="W""#));
    assert!(html.contains(r#"data-default-actual="—" data-default-actual-unit="""#));
    assert!(html.contains(
        r#"data-label="11:30–12:00" data-forecast="1,450" data-forecast-unit="W" data-actual="1,410" data-actual-unit="W""#
    ));
}

#[test]
fn the_actual_line_stops_at_the_last_completed_slot() {
    let today = date(6, 15);
    let forecast = forecast(&[today], |_, slot| bell(slot));
    // Slot 24, 12:00–12:30, is in progress and already measured.
    let intervals = measured(today, 25, bell);
    let view = shown(view(None, &forecast, &intervals, local(6, 15, 12, 10)));

    let path = &view.chart.actual_path;
    assert_eq!(path.matches('M').count(), 1, "{path}");
    assert_eq!(path.matches('L').count(), 23, "{path}");
    let slot = CHART_WIDTH / 48.0;
    // Slot 23 forecasts 1,400 W on a 2,000 W scale.
    let y = DAY_CHART_HEIGHT * 0.3;
    assert!(
        path.ends_with(&format!("L{:.1},{y:.1}", 23.5 * slot)),
        "{path}"
    );
    assert!((view.chart.plot.now_x.unwrap() - 24.5 * slot).abs() < 1e-6);
    assert!((view.chart.plot.highlight.unwrap().x - 24.0 * slot).abs() < 1e-6);
}

#[test]
fn a_gap_in_the_record_breaks_the_line() {
    let today = date(6, 15);
    let forecast = forecast(&[today], |_, slot| bell(slot));
    let mut intervals = journey::interval_ring();
    for (slot, at) in slot_starts(today).enumerate().take(24) {
        if slot != 20 {
            intervals.record(&journey::meter_event(at, 0.0, bell(slot)));
        }
    }
    let view = shown(view(None, &forecast, &intervals, local(6, 15, 12, 10)));

    assert_eq!(view.chart.actual_path.matches('M').count(), 2);
}

#[test]
fn so_far_compares_with_the_forecast_for_the_same_slots() {
    let today = date(6, 15);
    let forecast = forecast(&[today], |_, slot| bell(slot));
    let intervals = measured(today, 24, |slot| bell(slot) * 1.04);
    let view = shown(view(None, &forecast, &intervals, local(6, 15, 12, 10)));

    let [so_far, still_expected] = &view.stats[..] else {
        panic!("today has two stats");
    };
    assert_eq!(so_far.label, "So far");
    // Slots 17–23 forecast 5,600 W between them: 2.8 kWh.
    assert_eq!(so_far.value, "2.9 kWh");
    assert_eq!(so_far.sub.as_ref().unwrap().text, "+4% vs forecast");
    assert_eq!(still_expected.label, "Still expected");
    // Slots 24–35 forecast 7.2 kWh, less the 10 minutes of slot 24 gone by.
    assert_eq!(still_expected.value, "6.9 kWh");
    assert_eq!(still_expected.sub.as_ref().unwrap().text, "until 18:00");
}

#[test]
fn before_sunrise_so_far_has_no_forecast_to_compare_with() {
    let today = date(6, 15);
    let forecast = forecast(&[today], |_, slot| bell(slot));
    let intervals = measured(today, 10, |_| 0.0);
    let view = shown(view(None, &forecast, &intervals, local(6, 15, 5, 10)));

    assert_eq!(view.stats[0].value, "0.0 kWh");
    assert!(view.stats[0].sub.is_none());
    assert_eq!(energy_figure(kwh(&[])), "0.0");
    let html = render(view);
    assert_eq!(html.matches("mini-stat__sub").count(), 1, "{html}");
}

#[test]
fn after_sunset_nothing_is_still_expected_until_anything() {
    let today = date(6, 15);
    let forecast = forecast(&[today], |_, slot| bell(slot));
    let view = shown(view(
        None,
        &forecast,
        &journey::interval_ring(),
        local(6, 15, 21, 0),
    ));

    assert_eq!(view.stats[1].value, "0.0 kWh");
    assert!(view.stats[1].sub.is_none());
}

#[test]
fn tomorrow_shows_its_total_and_peak_without_actuals_or_a_now() {
    let (today, tomorrow) = (date(6, 15), date(6, 16));
    let forecast = forecast(&[today, tomorrow], |_, slot| bell(slot));
    let intervals = measured(today, 24, bell);
    let panel = view(Some(tomorrow), &forecast, &intervals, local(6, 15, 12, 10));
    assert_eq!(panel.data_day(), Some(tomorrow));
    assert!(!panel.data_live());
    let view = shown(panel);

    assert_eq!(view.nav.label, "Tomorrow");
    assert_eq!(view.nav.previous, Some(today));
    assert_eq!(view.nav.next, None);
    assert_eq!(
        view.readout,
        SolarReadoutView {
            label: "Day total".to_string(),
            forecast: SolarFigure {
                value: "10.0".to_string(),
                unit: "kWh",
            },
            actual: None,
        }
    );
    let [peak] = &view.stats[..] else {
        panic!("tomorrow has one stat");
    };
    assert_eq!(peak.label, "Peak");
    assert_eq!(peak.value, "13:00–13:30");
    assert_eq!(peak.sub.as_ref().unwrap().text, "2,000 W");
    assert!(view.chart.actual_path.is_empty());
    assert_eq!(view.chart.plot.now_x, None);
    assert_eq!(view.chart.plot.highlight, None);

    let html = render(view);
    assert!(!html.contains("forecast-panel__read--actual"));
    assert!(!html.contains("data-default-actual"));
    assert!(html.contains(r#"href="/?solar_day=2026-06-15""#));
    assert!(html.contains(r#"hx-get="/fragments/forecast-panel?day=2026-06-15""#));
    assert!(html.contains(r##"hx-target="#forecast-panel""##));
    assert!(html.contains(r#"data-day="2026-06-16" data-live="false""#));
    assert!(!html.contains("day-nav__today"));
}

#[test]
fn an_incomplete_tomorrow_is_out_of_reach() {
    let (today, tomorrow) = (date(6, 15), date(6, 16));
    let mut forecast = forecast(&[today, tomorrow], |_, slot| bell(slot));
    let cutoff = local(6, 16, 12, 0);
    forecast.points.retain(|point| point.at < cutoff);
    let view = shown(view(
        Some(tomorrow),
        &forecast,
        &journey::interval_ring(),
        local(6, 15, 12, 10),
    ));

    assert!(view.nav.is_today());
    assert_eq!(view.nav.next, None);
    assert_eq!(view.nav.previous, None);
}

#[test]
fn today_and_tomorrow_share_one_y_scale() {
    let (today, tomorrow) = (date(6, 15), date(6, 16));
    let forecast = forecast(&[today, tomorrow], |day, slot| {
        if day == tomorrow {
            bell(slot) * 1.15
        } else {
            bell(slot) * 0.6
        }
    });
    let now = local(6, 15, 12, 10);
    let intervals = journey::interval_ring();

    for day in [today, tomorrow] {
        let view = shown(view(Some(day), &forecast, &intervals, now));
        let labels: Vec<&str> = view
            .chart
            .plot
            .y_axis
            .iter()
            .map(|t| t.label.as_str())
            .collect();
        assert_eq!(
            labels,
            ["0", "500", "1000", "1500", "2000", "2500"],
            "{day}"
        );
        assert_eq!(view.chart.plot.grid_lines.len(), 6);
    }
}

#[test]
fn only_sunny_slots_have_a_bar_inset_from_its_slot() {
    let today = date(6, 15);
    let forecast = forecast(&[today], |_, slot| bell(slot));
    let view = shown(view(
        None,
        &forecast,
        &journey::interval_ring(),
        local(6, 15, 12, 10),
    ));

    let slot = CHART_WIDTH / 48.0;
    assert_eq!(view.chart.bars.len(), 19);
    let peak = view.chart.bars[26 - 17];
    let inset = SlotSpan {
        x: 26.0 * slot,
        width: slot,
    }
    .bar();
    assert!((peak.span.x - inset.x).abs() < 1e-6);
    assert!((peak.span.width - inset.width).abs() < 1e-6);
    assert!(peak.y.abs() < 1e-6, "the peak fills the 2,000 W scale");
}

/// At 12:10 slot 24 is a third gone: that third of its measurement is in So
/// far, compared with a third of its forecast, and the other two thirds of
/// its forecast are still expected.
#[test]
fn the_slot_in_progress_splits_at_now_between_the_two_totals() {
    let today = date(6, 15);
    let forecast = forecast(&[today], |_, slot| bell(slot));
    let intervals = measured(today, 25, |slot| bell(slot) * 1.04);
    let view = shown(view(None, &forecast, &intervals, local(6, 15, 12, 10)));

    let [so_far, still_expected] = &view.stats[..] else {
        panic!("today has two stats");
    };
    // 2.8 kWh by 12:00 and a third of 1,600 W for half an hour: 3.07 kWh
    // forecast, 4% more measured.
    assert_eq!(so_far.value, "3.2 kWh");
    assert_eq!(so_far.sub.as_ref().unwrap().text, "+4% vs forecast");
    assert_eq!(still_expected.value, "6.9 kWh");
}

/// Exactly on a slot edge nothing of the new slot has gone by.
#[test]
fn on_a_slot_edge_the_new_slot_is_wholly_still_expected() {
    let today = date(6, 15);
    let forecast = forecast(&[today], |_, slot| bell(slot));
    let intervals = measured(today, 24, bell);
    let view = shown(view(None, &forecast, &intervals, local(6, 15, 12, 0)));

    assert_eq!(view.stats[0].value, "2.8 kWh");
    assert_eq!(view.stats[1].value, "7.2 kWh");
}

#[test]
fn a_fetch_from_an_earlier_day_is_dated() {
    let (yesterday, today) = (date(6, 14), date(6, 15));
    let mut forecast = forecast(&[yesterday, today], |_, slot| bell(slot));
    forecast.as_of = Some(local(6, 14, 18, 30));
    let view = shown(view(
        None,
        &forecast,
        &journey::interval_ring(),
        local(6, 15, 12, 10),
    ));

    assert_eq!(
        view.subtitle,
        "Forecast vs. actual production, per 30 min · fetched Sun 14 Jun 18:30"
    );
}

#[test]
fn a_fetch_from_today_shows_only_its_time() {
    let forecast = forecast(&[date(6, 15)], |_, slot| bell(slot));
    let view = shown(view(
        None,
        &forecast,
        &journey::interval_ring(),
        local(6, 15, 12, 10),
    ));

    assert_eq!(
        view.subtitle,
        "Forecast vs. actual production, per 30 min · fetched 06:00"
    );
}

/// A cache holding only older days has nothing to draw today, so the panel
/// waits for the next fetch rather than showing bare axes.
#[test]
fn a_forecast_with_nothing_for_today_waits() {
    let mut forecast = forecast(&[date(6, 13), date(6, 14)], |_, slot| bell(slot));
    forecast.as_of = Some(local(6, 13, 18, 30));
    let view = view(
        Some(date(6, 16)),
        &forecast,
        &journey::interval_ring(),
        local(6, 15, 12, 10),
    );

    assert_eq!(empty(view), EmptyReason::Waiting);
}
