//! The forecast panel's view model: one local day of half-hour solar, the
//! forecast as bars and the measured production as a line over it, on a y
//! scale shared by today and tomorrow so stepping between them never
//! rescales the bars. A day is 46, 48 or 50 slots long (DST days), so nothing
//! here assumes 48.

use chrono::NaiveDate;
use chrono_tz::Tz;

use crate::clock::local_date;
use crate::units::{Elapsed, KiloWattHours, SolarPower, Timestamp, Watts};

use super::axis::{AxisDensity, AxisPosition, AxisTick};
use super::day_nav::DayNavView;
use super::intervals::IntervalHistory;
use super::plot::{DAY_CHART_HEIGHT, Day, SlotSpan, YScale};
use super::prices::range_label;
use super::solar_day::{SOLAR_SLOT, SolarDay, SolarSlot, solar_for, tomorrow_complete};
use super::state::{DashboardState, ForecastSnapshot};
use super::view::{MISSING, MiniStatSub, MiniStatView, SignStyle, format_time, format_watts};

/// The chart's viewBox width; the height is the shared [`DAY_CHART_HEIGHT`].
pub(super) const FORECAST_CHART_WIDTH: f64 = super::plot::CHART_WIDTH;

/// The bars' corner radius, in viewBox units.
pub(super) const FORECAST_BAR_RADIUS: f64 = 1.0;

/// The share of a slot left empty on each side of its bar.
const BAR_INSET: f64 = 0.14;

/// The y axis steps, and its grid lines fall, every half kilowatt.
const SCALE_STEP: Watts = Watts(500);

/// A slot's power is its mean over the slot, so it is held for this long.
const SLOT_HOURS: f64 = 0.5;

/// Energy over the slots: each slot's mean power held for half an hour.
pub(super) fn kwh(slots: &[SolarPower]) -> KiloWattHours {
    // A fold from +0.0, as `sum` starts at −0.0 and an empty day would read `-0.0`.
    let watts = slots.iter().fold(0.0, |total, p| total + p.get());
    KiloWattHours(watts * SLOT_HOURS / 1000.0)
}

/// Actual against forecast as a whole percent. `None` when the forecast is
/// zero, where a ratio says nothing.
pub(super) fn delta_pct(actual: KiloWattHours, forecast: KiloWattHours) -> Option<i32> {
    if forecast.get() == 0.0 {
        return None;
    }
    Some(((actual.get() - forecast.get()) / forecast.get() * 100.0).round() as i32)
}

/// `+N%`, `−N%` or `±0%`. The minus is U+2212 so it lines up with the plus in
/// the mono readout.
pub(super) fn format_delta(pct: i32) -> String {
    match pct {
        0 => "±0%".to_owned(),
        p if p > 0 => format!("+{p}%"),
        p => format!("\u{2212}{}%", p.unsigned_abs()),
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

pub enum ForecastPanelView {
    Empty(EmptyReason),
    Forecast(Box<ForecastDayView>),
}

impl ForecastPanelView {
    pub fn nav(&self) -> Option<&DayNavView> {
        match self {
            ForecastPanelView::Empty(_) => None,
            ForecastPanelView::Forecast(day) => Some(&day.nav),
        }
    }

    /// The day shown, which the host's `data-day` mirrors; `None` with no
    /// nav, which only ever stands for today.
    pub fn data_day(&self) -> Option<NaiveDate> {
        self.nav().map(|nav| nav.shown)
    }

    /// Whether the stream, which always renders today, may replace the panel.
    pub fn data_live(&self) -> bool {
        self.nav().is_none_or(DayNavView::is_today)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmptyReason {
    NotConfigured,
    /// Configured, but no forecast has landed yet.
    Waiting,
}

impl EmptyReason {
    pub fn text(self) -> &'static str {
        match self {
            EmptyReason::NotConfigured => {
                "No solar forecast configured — add [prediction] to config.toml"
            }
            EmptyReason::Waiting => "Waiting for the solar forecast…",
        }
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
            value: format_kwh(energy),
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
    pub bars: Vec<ForecastBarView>,
    /// The actual line's `d`: a new subpath after every slot with no
    /// measurement, so a lost slot reads as a gap rather than a guess.
    pub actual_path: String,
    pub hits: Vec<ForecastHitView>,
    /// One per y-axis tick.
    pub grid_lines: Vec<f64>,
    pub now_x: Option<f64>,
    /// Where the highlight sits unhovered: the current slot, today only.
    pub highlight: Option<SlotSpan>,
    pub y_axis: Vec<AxisTick>,
    pub x_axis: Vec<AxisTick>,
}

/// kWh to one decimal.
fn format_kwh(energy: KiloWattHours) -> String {
    format!("{:.1}", energy.get())
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

/// The shown day with what the readout, chart and stats all read.
struct ShownDay<'a> {
    day: &'a SolarDay,
    frame: Day,
    /// Today only: when `now` is, and which slot it falls in.
    now: Option<(Timestamp, Option<usize>)>,
    tz: Tz,
}

impl ShownDay<'_> {
    fn current(&self) -> Option<usize> {
        self.now.and_then(|(_, current)| current)
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
        self.now.is_some_and(|(now, _)| Self::end_of(slot) <= now)
    }
}

/// The current slot on today, the day's forecast total on any other.
fn default_readout(shown: &ShownDay) -> SolarReadoutView {
    match (shown.now, shown.current()) {
        (Some(_), Some(index)) => {
            let slot = &shown.day.slots[index];
            SolarReadoutView {
                label: format!("Now · {}", shown.label(slot)),
                forecast: SolarFigure::watts(slot.forecast),
                actual: Some(SolarFigure::watts(slot.actual)),
            }
        }
        (Some(_), None) => SolarReadoutView {
            label: "Now".to_string(),
            forecast: SolarFigure::watts(None),
            actual: Some(SolarFigure::watts(None)),
        },
        (None, _) => SolarReadoutView {
            label: "Day total".to_string(),
            forecast: SolarFigure::kwh(kwh(&powers(shown.day.forecast()))),
            actual: None,
        },
    }
}

fn stats(shown: &ShownDay) -> Vec<MiniStatView> {
    match shown.now {
        Some(_) => today_stats(shown, shown.current().unwrap_or(0)),
        None => vec![peak_stat(shown)],
    }
}

/// So far against the forecast for the same slots, and what is still to come.
fn today_stats(shown: &ShownDay, current: usize) -> Vec<MiniStatView> {
    let slots = &shown.day.slots;
    // Only slots with a measurement, so a gap in the record never reads as
    // underproduction.
    let (actual, forecast): (Vec<SolarPower>, Vec<SolarPower>) = shown
        .day
        .actual()
        .zip(shown.day.forecast())
        .take(current)
        .filter_map(|(actual, forecast)| Some((actual?, forecast.unwrap_or(SolarPower::ZERO))))
        .unzip();
    let (actual, forecast) = (kwh(&actual), kwh(&forecast));
    let so_far = MiniStatView {
        label: "So far",
        value: format!("{} kWh", format_kwh(actual)),
        sub: delta_pct(actual, forecast).map(|pct| MiniStatSub {
            text: format!("{} vs forecast", format_delta(pct)),
            tone: None,
        }),
    };

    let ahead = powers(slots[current..].iter().map(|slot| slot.forecast));
    let until = last_sun_slot(&ahead).map(|last| &slots[current + last]);
    let still_expected = MiniStatView {
        label: "Still expected",
        value: format!("{} kWh", format_kwh(kwh(&ahead))),
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
                span: SlotSpan {
                    x: span.x + span.width * BAR_INSET,
                    width: span.width * (1.0 - 2.0 * BAR_INSET),
                },
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
        bars,
        actual_path: actual_path(shown, &scale),
        hits,
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
    }
}

fn subtitle(forecast: &ForecastSnapshot, tz: Tz) -> String {
    let label = "Forecast vs. actual production, per 30 min";
    match forecast.as_of {
        Some(at) => format!("{label} · fetched {}", format_time(at, tz)),
        None => format!("{label} · fetch pending"),
    }
}

/// Today's panel, as the page and the stream render it.
pub fn todays_forecast_panel(state: &DashboardState, tz: Tz) -> ForecastPanelView {
    let context = SolarContext {
        forecast: &state.forecast,
        intervals: &state.intervals,
        configured: state.forecast_feed,
    };
    forecast_panel_view(None, &context, state.as_of, tz)
}

/// `day`, or today when `None`, clamped to today and tomorrow, the latter
/// only once its forecast covers every slot. Today has a now: its readout
/// defaults to the current slot and its line stops at the last whole one.
/// Tomorrow defaults to its forecast total.
pub fn forecast_panel_view(
    day: Option<NaiveDate>,
    context: &SolarContext,
    now: Timestamp,
    tz: Tz,
) -> ForecastPanelView {
    if !context.configured {
        return ForecastPanelView::Empty(EmptyReason::NotConfigured);
    }
    let forecast = context.forecast;
    // A clock no calendar can place has no today to forecast.
    let Some(today) = local_date(now, tz).filter(|_| !forecast.points.is_empty()) else {
        return ForecastPanelView::Empty(EmptyReason::Waiting);
    };
    let tomorrow = today
        .succ_opt()
        .filter(|_| tomorrow_complete(forecast, tz, now));
    let latest = tomorrow.unwrap_or(today);
    let date = day.map_or(today, |day| day.clamp(today, latest));
    let days: Vec<SolarDay> = [Some(today), tomorrow]
        .into_iter()
        .flatten()
        .filter_map(|day| solar_for(day, forecast, context.intervals, tz, now))
        .collect();
    let Some(day) = days.iter().find(|day| day.date == date) else {
        return ForecastPanelView::Empty(EmptyReason::Waiting);
    };

    let nav = DayNavView::new(date, today, Some(today), latest);
    let frame = Day::of(date, tz);
    let now = nav.is_today().then(|| {
        let current = day
            .slots
            .iter()
            .position(|slot| slot.start <= now && now < ShownDay::end_of(slot));
        (now, current)
    });
    let shown = ShownDay {
        day,
        frame,
        now,
        tz,
    };
    ForecastPanelView::Forecast(Box::new(ForecastDayView {
        subtitle: subtitle(forecast, tz),
        readout: default_readout(&shown),
        stats: stats(&shown),
        chart: chart(&shown, scale_top(&days)),
        nav,
    }))
}

#[cfg(test)]
#[path = "solar_tests.rs"]
mod tests;
