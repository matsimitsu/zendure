//! The forecast panel's view model: one local day of half-hour solar, the
//! forecast as bars and the measured production as a line over it, on a y
//! scale shared by today and tomorrow so stepping between them never
//! rescales the bars. A day is 46, 48 or 50 slots long (DST days), so nothing
//! here assumes 48.

use std::time::Duration;

use chrono_tz::Tz;

use crate::clock::local_date;
use crate::units::{Elapsed, KiloWattHours, Percent, SolarPower, Timestamp, WattHours, Watts};

use super::axis::{AxisDensity, AxisPosition, AxisTick};
use super::day_nav::{DayNavView, DayQuery};
use super::day_panel::{ChartedDay, DayPanel, DayPlotView, EmptyReason};
use super::intervals::IntervalHistory;
use super::plot::{DAY_CHART_HEIGHT, Day, SlotSpan, YScale};
use super::prices::range_label;
use super::solar_day::{SOLAR_SLOT, SolarDay, SolarSlot, solar_days};
use super::state::{DashboardState, ForecastSnapshot};
use super::view::{
    MISSING, MiniStatSub, MiniStatView, SignStyle, energy_figure, energy_string, format_time,
    format_watts,
};

/// The y axis steps, and its grid lines fall, every half kilowatt.
const SCALE_STEP: Watts = Watts(500);

/// A slot's mean power held for `over`.
fn energy(power: SolarPower, over: Duration) -> WattHours {
    let watts = power.into_watts();
    WattHours::integrate(watts, watts, over)
}

/// Energy over whole slots.
pub(super) fn kwh(slots: &[SolarPower]) -> KiloWattHours {
    slots
        .iter()
        .map(|&power| energy(power, SOLAR_SLOT))
        .sum::<WattHours>()
        .to_kwh()
}

/// Actual against forecast, rounded to a whole percent. `None` when the
/// forecast is zero, where a ratio says nothing.
pub(super) fn delta_pct(actual: KiloWattHours, forecast: KiloWattHours) -> Option<Percent> {
    if forecast.get() == 0.0 {
        return None;
    }
    Some(Percent(
        ((actual.get() - forecast.get()) / forecast.get() * 100.0).round(),
    ))
}

/// `+N%`, `−N%` or `±0%`. The minus is U+2212 so it lines up with the plus in
/// the mono readout.
pub(super) fn format_delta(delta: Percent) -> String {
    let pct = delta.get();
    if pct == 0.0 {
        "±0%".to_owned()
    } else if pct > 0.0 {
        format!("+{pct:.0}%")
    } else {
        format!("\u{2212}{:.0}%", pct.abs())
    }
}

/// Index of the first maximum, so a flat-topped day peaks at its earliest slot.
/// An empty or all-zero series gives 0.
pub(super) fn peak_slot(slots: &[SolarPower]) -> usize {
    let mut best = 0;
    for (i, p) in slots.iter().enumerate() {
        if *p > slots[best] {
            best = i;
        }
    }
    best
}

/// Index of the last slot with any production; `None` on a day without sun.
pub(super) fn last_sun_slot(slots: &[SolarPower]) -> Option<usize> {
    slots.iter().rposition(|p| *p > SolarPower::ZERO)
}

// --- Panel ------------------------------------------------------------------

/// What the panel renders from besides the day and the clock.
pub struct SolarContext<'a> {
    pub forecast: &'a ForecastSnapshot,
    pub intervals: &'a IntervalHistory,
    /// Whether `[prediction]` is configured at all.
    pub configured: bool,
}

pub type ForecastPanelView = DayPanel<ForecastDayView>;

pub fn empty_text(reason: EmptyReason) -> &'static str {
    match reason {
        EmptyReason::NotConfigured => {
            "No solar forecast configured — add [prediction] to config.toml"
        }
        EmptyReason::Waiting => "Waiting for the solar forecast…",
    }
}

pub struct ForecastDayView {
    /// `Forecast vs. actual production, per 30 min · fetched 13:00`.
    pub subtitle: String,
    pub nav: DayNavView,
    /// What the readout shows with nothing hovered.
    pub readout: SolarReadoutView,
    /// So far and Still expected on today, the Peak on tomorrow.
    pub stats: Vec<MiniStatView>,
    pub chart: ForecastChartView,
}

impl ChartedDay for ForecastDayView {
    fn nav(&self) -> &DayNavView {
        &self.nav
    }

    fn nav_mut(&mut self) -> &mut DayNavView {
        &mut self.nav
    }
}

/// A figure and its unit, formatted. The unit is empty beside `—`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SolarFigure {
    pub value: String,
    pub unit: &'static str,
}

impl SolarFigure {
    fn watts(power: Option<SolarPower>) -> Self {
        match power {
            Some(power) => SolarFigure {
                value: format_watts(power.into_watts(), SignStyle::Magnitude),
                unit: "W",
            },
            None => SolarFigure {
                value: MISSING.to_string(),
                unit: "",
            },
        }
    }

    fn kwh(energy: KiloWattHours) -> Self {
        SolarFigure {
            value: energy_figure(energy),
            unit: "kWh",
        }
    }
}

/// One readout state, formatted: the default, or a hit's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SolarReadoutView {
    /// `Now · 13:30–14:00`, `15:00–15:30` or `Day total`.
    pub label: String,
    pub forecast: SolarFigure,
    /// `None` on a day with nothing measured yet, which drops the item.
    pub actual: Option<SolarFigure>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ForecastBarView {
    pub span: SlotSpan,
    pub y: f64,
    pub height: f64,
}

/// A full-height hover target over one half-hour.
#[derive(Debug, Clone, PartialEq)]
pub struct ForecastHitView {
    /// The slot's place in the day, which the script remembers the hover by.
    pub slot: usize,
    pub span: SlotSpan,
    pub label: String,
    pub forecast: SolarFigure,
    pub actual: SolarFigure,
}

pub struct ForecastChartView {
    pub plot: DayPlotView,
    pub bars: Vec<ForecastBarView>,
    /// The actual line's `d`: a new subpath after every slot with no
    /// measurement, so a lost slot reads as a gap rather than a guess.
    pub actual_path: String,
    pub hits: Vec<ForecastHitView>,
}

/// The slots' powers, a missing one counted as nothing.
fn powers(slots: impl Iterator<Item = Option<SolarPower>>) -> Vec<SolarPower> {
    slots
        .map(|power| power.unwrap_or(SolarPower::ZERO))
        .collect()
}

/// The top of the y scale over every day the nav reaches: the highest
/// forecast or actual, rounded up to a whole step, and never below one.
fn scale_top<'a>(days: impl IntoIterator<Item = &'a SolarDay>) -> Watts {
    let high = days
        .into_iter()
        .flat_map(|day| day.slots.iter())
        .flat_map(|slot| [slot.forecast, slot.actual])
        .flatten()
        .fold(
            SolarPower::ZERO,
            |high, power| {
                if power > high { power } else { high }
            },
        );
    let steps = (high.get() / f64::from(SCALE_STEP.get())).ceil().max(1.0);
    Watts::rounded(steps * f64::from(SCALE_STEP.get()))
}

/// Where the shown day stands against the clock.
#[derive(Debug, Clone, Copy)]
enum Moment {
    /// `current` is the slot `now` falls in.
    Today {
        now: Timestamp,
        current: usize,
    },
    Other,
}

/// The shown day with what the readout, chart and stats all read.
struct ShownDay<'a> {
    day: &'a SolarDay,
    frame: Day,
    moment: Moment,
    tz: Tz,
}

impl ShownDay<'_> {
    fn current(&self) -> Option<usize> {
        match self.moment {
            Moment::Today { current, .. } => Some(current),
            Moment::Other => None,
        }
    }

    fn end_of(slot: &SolarSlot) -> Timestamp {
        slot.start + Elapsed::of(SOLAR_SLOT)
    }

    fn span(&self, slot: &SolarSlot) -> SlotSpan {
        self.frame.span(slot.start, Self::end_of(slot))
    }

    fn label(&self, slot: &SolarSlot) -> String {
        range_label(slot.start, Self::end_of(slot), self.frame.end(), self.tz)
    }

    /// Whether the slot is over, so its measurement is whole.
    fn completed(&self, slot: &SolarSlot) -> bool {
        matches!(self.moment, Moment::Today { now, .. } if Self::end_of(slot) <= now)
    }
}

/// The current slot on today, the day's forecast total on any other.
fn default_readout(shown: &ShownDay) -> SolarReadoutView {
    match shown.moment {
        Moment::Today { current, .. } => {
            let slot = &shown.day.slots[current];
            SolarReadoutView {
                label: format!("Now · {}", shown.label(slot)),
                forecast: SolarFigure::watts(slot.forecast),
                actual: Some(SolarFigure::watts(slot.actual)),
            }
        }
        Moment::Other => SolarReadoutView {
            label: "Day total".to_string(),
            forecast: SolarFigure::kwh(kwh(&powers(shown.day.forecast()))),
            actual: None,
        },
    }
}

fn stats(shown: &ShownDay) -> Vec<MiniStatView> {
    match shown.moment {
        Moment::Today { now, current } => today_stats(shown, now, current),
        Moment::Other => vec![peak_stat(shown)],
    }
}

/// So far against the forecast for the same span, and what is still to come.
fn today_stats(shown: &ShownDay, now: Timestamp, current: usize) -> Vec<MiniStatView> {
    let slots = &shown.day.slots;
    // The two totals split at `now`, not at a slot edge: the slot in progress
    // counts its elapsed part in So far and the rest in Still expected, so
    // what it has already produced is in exactly one of them.
    let elapsed = slots[current]
        .start
        .span_within(now, SOLAR_SLOT)
        .unwrap_or(Duration::ZERO);
    let remaining = SOLAR_SLOT.saturating_sub(elapsed);
    let spans = std::iter::repeat_n(SOLAR_SLOT, current).chain([elapsed]);
    // Only spans with a measurement, so a gap in the record never reads as
    // underproduction.
    let (actual, forecast) = slots
        .iter()
        .zip(spans)
        .filter_map(|(slot, span)| {
            let forecast = slot.forecast.unwrap_or(SolarPower::ZERO);
            Some((energy(slot.actual?, span), energy(forecast, span)))
        })
        .fold(
            (WattHours::ZERO, WattHours::ZERO),
            |(actual, forecast), (a, f)| (actual + a, forecast + f),
        );
    let (actual, forecast) = (actual.to_kwh(), forecast.to_kwh());
    let so_far = MiniStatView {
        label: "So far",
        value: energy_string(actual),
        sub: delta_pct(actual, forecast).map(|pct| MiniStatSub {
            text: format!("{} vs forecast", format_delta(pct)),
            tone: None,
        }),
    };

    let ahead = powers(slots[current..].iter().map(|slot| slot.forecast));
    let until = last_sun_slot(&ahead).map(|last| &slots[current + last]);
    let expected = ahead[1..]
        .iter()
        .map(|&power| energy(power, SOLAR_SLOT))
        .fold(energy(ahead[0], remaining), |total, e| total + e);
    let still_expected = MiniStatView {
        label: "Still expected",
        value: energy_string(expected.to_kwh()),
        sub: until.map(|slot| MiniStatSub {
            text: format!("until {}", format_time(ShownDay::end_of(slot), shown.tz)),
            tone: None,
        }),
    };
    vec![so_far, still_expected]
}

/// The forecast's busiest half-hour, or `—` on a day without sun.
fn peak_stat(shown: &ShownDay) -> MiniStatView {
    let forecast = powers(shown.day.forecast());
    let peak = peak_slot(&forecast);
    let sunny = forecast.get(peak).is_some_and(|&p| p > SolarPower::ZERO);
    MiniStatView {
        label: "Peak",
        value: if sunny {
            shown.label(&shown.day.slots[peak])
        } else {
            MISSING.to_string()
        },
        sub: sunny.then(|| MiniStatSub {
            text: format!(
                "{} W",
                format_watts(forecast[peak].into_watts(), SignStyle::Magnitude)
            ),
            tone: None,
        }),
    }
}

/// Each slot's centre for every completed slot with a measurement, broken
/// where one is missing.
fn actual_path(shown: &ShownDay, scale: &YScale) -> String {
    let mut path = String::new();
    let mut drawing = false;
    for slot in &shown.day.slots {
        let point = slot.actual.filter(|_| shown.completed(slot)).map(|actual| {
            let span = shown.span(slot);
            (span.x + span.width / 2.0, scale.y(actual.get()))
        });
        let Some((x, y)) = point else {
            drawing = false;
            continue;
        };
        let command = if drawing { "L" } else { "M" };
        if !path.is_empty() {
            path.push(' ');
        }
        path.push_str(&format!("{command}{x:.1},{y:.1}"));
        drawing = true;
    }
    path
}

fn chart(shown: &ShownDay, top: Watts) -> ForecastChartView {
    let scale = YScale::new(f64::from(top.get()), 0.0, DAY_CHART_HEIGHT);
    let ticks: Vec<Watts> = (0..=top.get() / SCALE_STEP.get())
        .map(|step| Watts(step * SCALE_STEP.get()))
        .collect();

    let bars = shown
        .day
        .slots
        .iter()
        .filter_map(|slot| {
            let forecast = slot.forecast.filter(|&p| p > SolarPower::ZERO)?;
            let span = shown.span(slot);
            let (y, height) = scale.bar(forecast.get(), 0.0);
            Some(ForecastBarView {
                span: span.bar(),
                y,
                height,
            })
        })
        .collect();

    let hits = shown
        .day
        .slots
        .iter()
        .enumerate()
        .map(|(index, slot)| ForecastHitView {
            slot: index,
            span: shown.span(slot),
            label: shown.label(slot),
            forecast: SolarFigure::watts(slot.forecast),
            actual: SolarFigure::watts(slot.actual),
        })
        .collect();

    let current = shown
        .current()
        .map(|index| shown.span(&shown.day.slots[index]));

    ForecastChartView {
        plot: DayPlotView {
            grid_lines: ticks
                .iter()
                .map(|tick| scale.y(f64::from(tick.get())))
                .collect(),
            now_x: current.map(|span| span.x + span.width / 2.0),
            highlight: current,
            y_axis: ticks
                .iter()
                .map(|tick| {
                    AxisTick::new(
                        AxisPosition::new(scale.y(f64::from(tick.get())) / DAY_CHART_HEIGHT),
                        tick.get().to_string(),
                        AxisDensity::Always,
                    )
                })
                .collect(),
            x_axis: shown.frame.axis(shown.tz, 6),
        },
        bars,
        actual_path: actual_path(shown, &scale),
        hits,
    }
}

/// The fetch time, dated once it is no longer today's so a stale forecast
/// says so.
fn subtitle(forecast: &ForecastSnapshot, now: Timestamp, tz: Tz) -> String {
    let label = "Forecast vs. actual production, per 30 min";
    match forecast.as_of {
        Some(at) if local_date(at, tz) == local_date(now, tz) => {
            format!("{label} · fetched {}", format_time(at, tz))
        }
        Some(at) => format!("{label} · fetched {}", format_day_time(at, tz)),
        None => format!("{label} · fetch pending"),
    }
}

/// `Thu 8 Oct 18:30`.
fn format_day_time(at: Timestamp, tz: Tz) -> String {
    use chrono::TimeZone;
    tz.timestamp_millis_opt(at.as_millis())
        .single()
        .map_or_else(
            || format_time(at, tz),
            |dt| dt.format("%a %-d %b %H:%M").to_string(),
        )
}

/// The panel on the day `query` asks for.
pub fn requested_forecast_panel(
    state: &DashboardState,
    query: DayQuery,
    tz: Tz,
) -> ForecastPanelView {
    let context = SolarContext {
        forecast: &state.forecast,
        intervals: &state.intervals,
        configured: state.forecast_feed,
    };
    forecast_panel_view(query, &context, state.as_of, tz)
}

/// The day `query` asks for, clamped to today and tomorrow, the latter only
/// once its forecast covers every slot. Today has a now: its readout
/// defaults to the current slot and its line stops at the last whole one.
/// Tomorrow defaults to its forecast total. A forecast with nothing for today
/// (none fetched, or only older days cached) waits for the next fetch.
pub fn forecast_panel_view(
    query: DayQuery,
    context: &SolarContext,
    now: Timestamp,
    tz: Tz,
) -> ForecastPanelView {
    if !context.configured {
        return DayPanel::Empty(EmptyReason::NotConfigured);
    }
    let forecast = context.forecast;
    let Some(days) = solar_days(forecast, context.intervals, tz, now)
        .filter(|days| days.today.forecast().any(|slot| slot.is_some()))
    else {
        return DayPanel::Empty(EmptyReason::Waiting);
    };
    let today = days.today.date;
    let latest = days.tomorrow.as_ref().map_or(today, |day| day.date);
    let date = query.resolve(today, today, latest);
    let Some(day) = days.get(date) else {
        return DayPanel::Empty(EmptyReason::Waiting);
    };

    let nav = DayNavView::new(date, today, Some(today), latest);
    let current = day.slots.iter().rposition(|slot| slot.start <= now);
    let moment = match current {
        Some(current) if nav.is_today() => Moment::Today { now, current },
        _ => Moment::Other,
    };
    let shown = ShownDay {
        day,
        frame: Day::of(date, tz),
        moment,
        tz,
    };
    DayPanel::Shown(Box::new(ForecastDayView {
        subtitle: subtitle(forecast, now, tz),
        readout: default_readout(&shown),
        stats: stats(&shown),
        chart: chart(&shown, scale_top(days.iter())),
        nav,
    }))
}

#[cfg(test)]
#[path = "solar_tests.rs"]
mod tests;
