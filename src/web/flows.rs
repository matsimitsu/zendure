//! The energy-flows chart's view model: today's intervals as grouped, signed
//! bars on one scale, at both resolutions, so switching between them is a
//! class toggle in the browser rather than a round trip.

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::NaiveDate;
use chrono_tz::Tz;

use crate::units::{Elapsed, Timestamp, Watts};

use super::axis::{AxisDensity, AxisPosition, AxisTick, day_axis};
use super::intervals::{
    HOUR, INTERVAL, IntervalAverages, IntervalHistory, IntervalIndex, IntervalSlot, hourly,
};
use super::state::Plottable;
use super::view::{MISSING, format_time};

/// The plots' viewBox. Stretched to the panel (`preserveAspectRatio="none"`),
/// so these are proportions rather than pixels.
pub(super) const FLOWS_CHART_WIDTH: f64 = 1000.0;
pub(super) const FLOWS_CHART_HEIGHT: f64 = 260.0;

/// Keeps a non-zero flow visible on a scale sized for kilowatts.
const MIN_BAR_HEIGHT: f64 = 0.6;

const NICE_STEPS: [Watts; 3] = [Watts(500), Watts(1000), Watts(2000)];

/// The most steps a scale may span before the next nice step up is tried.
const MAX_STEPS: i32 = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowSeries {
    Solar,
    Home,
    Grid,
    Battery,
}

impl FlowSeries {
    pub const ALL: [Self; 4] = [Self::Solar, Self::Home, Self::Grid, Self::Battery];

    /// The BEM modifier, and the `data-*` name its readout travels under.
    pub fn key(self) -> &'static str {
        match self {
            Self::Solar => "solar",
            Self::Home => "home",
            Self::Grid => "grid",
            Self::Battery => "battery",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Solar => "Solar",
            Self::Home => "Home",
            Self::Grid => "Grid",
            Self::Battery => "Battery",
        }
    }

    /// Grid is positive importing and battery positive discharging, as their
    /// role types already are, so below zero reads as export and charge.
    fn watts(self, averages: &IntervalAverages) -> Option<f64> {
        match self {
            Self::Solar => averages.solar.map(Plottable::plot_value),
            Self::Home => averages.home.map(Plottable::plot_value),
            Self::Grid => averages.grid.map(Plottable::plot_value),
            Self::Battery => averages.battery.map(Plottable::plot_value),
        }
    }
}

/// One of the two plots the panel carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowResolution {
    Hour,
    Quarter,
}

impl FlowResolution {
    /// The BEM modifier and the segmented control's `data-interval`.
    pub fn key(self) -> &'static str {
        match self {
            Self::Hour => "1h",
            Self::Quarter => "15m",
        }
    }

    fn span(self) -> Duration {
        match self {
            Self::Hour => HOUR,
            Self::Quarter => INTERVAL,
        }
    }

    /// The share of a slot left empty on each side of its group of bars.
    fn group_padding(self) -> f64 {
        match self {
            Self::Hour => 0.14,
            Self::Quarter => 0.08,
        }
    }

    /// The share of each bar's lane the bar itself fills.
    fn bar_fill(self) -> f64 {
        match self {
            Self::Hour => 0.82,
            Self::Quarter => 1.0,
        }
    }

    /// The first slot `now` has not finished filling.
    fn in_progress(self, now: Timestamp) -> IntervalIndex {
        let quarter = IntervalIndex::containing(now);
        match self {
            Self::Hour => quarter.hour_start(),
            Self::Quarter => quarter,
        }
    }
}

/// The local day the chart spans, as an absolute window: 23 or 25 hours on
/// a DST change.
struct Day {
    start: Timestamp,
    length: Elapsed,
}

impl Day {
    fn of(date: NaiveDate, tz: Tz) -> Option<Self> {
        let start = crate::clock::local_day_start(date, tz)?;
        let end = crate::clock::local_day_start(date.succ_opt()?, tz)?;
        Some(Day {
            start,
            length: end - start,
        })
    }

    /// A plain 24 hours from local midnight, for a date `tz` cannot place.
    fn fallback(now: Timestamp, tz: Tz) -> Self {
        Day {
            start: crate::clock::local_midnight(now, tz),
            length: Elapsed::of(24 * HOUR),
        }
    }

    /// `span` as a share of the viewBox's width.
    fn width(&self, span: Elapsed) -> f64 {
        span.as_millis() as f64 / self.length.as_millis().max(1) as f64 * FLOWS_CHART_WIDTH
    }

    fn x(&self, at: Timestamp) -> f64 {
        self.width(at - self.start)
    }

    fn slot_starts(&self, span: Duration) -> impl Iterator<Item = Timestamp> {
        let span = Elapsed::of(span);
        let count = self.length.as_millis() / span.as_millis().max(1);
        std::iter::successors(Some(self.start), move |&start| Some(start + span))
            .take(usize::try_from(count).unwrap_or(0))
    }
}

/// One signed y-scale, shared by both plots so switching resolution never
/// rescales, stepped in round watts so its gridlines read as whole kW.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct FlowScale {
    pub top: Watts,
    pub bottom: Watts,
    pub step: Watts,
}

impl FlowScale {
    fn fitting(values: impl Iterator<Item = f64>) -> Self {
        let (high, low) = values.fold((0.0_f64, 0.0_f64), |(hi, lo), v| (hi.max(v), lo.min(v)));
        // `as` saturates, so an absurd reading costs a tall axis, not a wrap.
        let steps = |step: Watts, extent: f64| (extent / step.as_f64()).ceil() as i32;
        let largest = NICE_STEPS[NICE_STEPS.len() - 1];
        let step = NICE_STEPS
            .into_iter()
            .find(|&step| steps(step, high).max(1) + steps(step, -low) <= MAX_STEPS)
            .unwrap_or(largest);
        FlowScale {
            top: Watts(step.get().saturating_mul(steps(step, high).max(1))),
            bottom: Watts(step.get().saturating_mul(-steps(step, -low))),
            step,
        }
    }

    fn range(&self) -> f64 {
        (self.top - self.bottom).as_f64()
    }

    fn y(&self, watts: f64) -> f64 {
        (self.top.as_f64() - watts) / self.range() * FLOWS_CHART_HEIGHT
    }

    /// A bar's `(y, height)`: up from the zero line for a positive flow,
    /// down from it for a negative one.
    fn bar(&self, watts: f64) -> (f64, f64) {
        let zero = self.y(0.0);
        let height = (zero - self.y(watts)).abs().max(MIN_BAR_HEIGHT);
        if watts > 0.0 {
            (zero - height, height)
        } else {
            (zero, height)
        }
    }

    /// Every step from the top down to the bottom, inclusive.
    fn ticks(&self) -> Vec<Watts> {
        std::iter::successors(Some(self.top), |&tick| Some(tick - self.step))
            .take_while(|&tick| tick >= self.bottom)
            .collect()
    }
}

/// A y-axis label: kilowatts to one place, with a typographic minus.
fn tick_label(watts: Watts) -> String {
    if watts == Watts::ZERO {
        return "0".to_string();
    }
    let kw = format!("{:.1}", watts.as_f64().abs() / 1000.0);
    if watts < Watts::ZERO {
        format!("−{kw}")
    } else {
        kw
    }
}

/// A readout value: signed kilowatts to two places, so every series reads as
/// a direction, with a zero that rounded from either side reading `+0.00`.
fn readout_value(watts: Option<f64>) -> String {
    let Some(watts) = watts else {
        return MISSING.to_string();
    };
    let kw = format!("{:.2}", watts.abs() / 1000.0);
    if watts < 0.0 && kw != "0.00" {
        format!("−{kw}")
    } else {
        format!("+{kw}")
    }
}

/// What the legend shows for one column: its time span and a value per series.
#[derive(Debug, Clone, PartialEq)]
pub struct FlowReadoutView {
    /// `13:00–14:00`, as the hit column's `data-label` carries it.
    pub label: String,
    pub values: [(FlowSeries, String); 4],
}

impl FlowReadoutView {
    fn of(start: Timestamp, span: Duration, averages: &IntervalAverages, tz: Tz) -> Self {
        FlowReadoutView {
            label: format!(
                "{}–{}",
                format_time(start, tz),
                format_time(start + Elapsed::of(span), tz)
            ),
            values: FlowSeries::ALL.map(|series| (series, readout_value(series.watts(averages)))),
        }
    }

    pub fn value(&self, series: FlowSeries) -> &str {
        self.values
            .iter()
            .find(|(each, _)| *each == series)
            .map_or(MISSING, |(_, value)| value)
    }

    fn empty() -> Self {
        FlowReadoutView {
            label: MISSING.to_string(),
            values: FlowSeries::ALL.map(|series| (series, MISSING.to_string())),
        }
    }
}

/// A rectangle in the plot's viewBox units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FlowRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

pub struct FlowBarView {
    pub series: FlowSeries,
    pub rect: FlowRect,
}

/// A full-height column over one slot, the hover target the readout reads.
pub struct FlowHitView {
    pub x: f64,
    pub width: f64,
    pub readout: FlowReadoutView,
    /// The most recent completed slot: what the readout shows unhovered.
    pub latest: bool,
}

pub struct FlowPlotView {
    pub resolution: FlowResolution,
    pub bars: Vec<FlowBarView>,
    /// One per slot of the day, completed or not, in order.
    pub hits: Vec<FlowHitView>,
    /// The start of the in-progress slot; `None` when it is outside the day.
    pub now_x: Option<f64>,
}

pub struct EnergyFlowsView {
    pub y_axis: Vec<AxisTick>,
    pub x_axis: Vec<AxisTick>,
    /// Gridline heights, one per y-axis tick other than zero.
    pub grid_lines: Vec<f64>,
    pub zero_y: f64,
    /// The hourly plot first: it is the one shown by default.
    pub plots: [FlowPlotView; 2],
    /// The default plot's most recent completed slot.
    pub readout: FlowReadoutView,
}

fn plot(
    resolution: FlowResolution,
    completed: &[IntervalSlot],
    now: Timestamp,
    day: &Day,
    scale: &FlowScale,
    tz: Tz,
) -> FlowPlotView {
    let span = resolution.span();
    let slot_width = day.width(Elapsed::of(span));
    let padding = slot_width * resolution.group_padding();
    let lane = (slot_width - 2.0 * padding) / FlowSeries::ALL.len() as f64;
    let bar_width = lane * resolution.bar_fill();

    let by_start: BTreeMap<Timestamp, &IntervalAverages> = completed
        .iter()
        .map(|slot| (slot.index.start(), &slot.averages))
        .collect();
    let latest = by_start.keys().next_back().copied();

    let mut bars = Vec::new();
    let mut hits = Vec::new();
    for start in day.slot_starts(span) {
        let x = day.x(start);
        let Some(averages) = by_start.get(&start) else {
            hits.push(FlowHitView {
                x,
                width: slot_width,
                readout: FlowReadoutView::of(start, span, &IntervalAverages::default(), tz),
                latest: false,
            });
            continue;
        };
        for (lane_index, series) in FlowSeries::ALL.into_iter().enumerate() {
            let Some(watts) = series.watts(averages).filter(|&w| w != 0.0) else {
                continue;
            };
            let (y, height) = scale.bar(watts);
            bars.push(FlowBarView {
                series,
                rect: FlowRect {
                    x: x + padding + lane * lane_index as f64 + (lane - bar_width) / 2.0,
                    y,
                    width: bar_width,
                    height,
                },
            });
        }
        hits.push(FlowHitView {
            x,
            width: slot_width,
            readout: FlowReadoutView::of(start, span, averages, tz),
            latest: Some(start) == latest,
        });
    }

    let in_progress = resolution.in_progress(now).start();
    let now_x = (in_progress >= day.start && in_progress < day.start + day.length)
        .then(|| day.x(in_progress));

    FlowPlotView {
        resolution,
        bars,
        hits,
        now_x,
    }
}

fn local_date(now: Timestamp, tz: Tz) -> Option<NaiveDate> {
    chrono::DateTime::from_timestamp_millis(now.as_millis())
        .map(|utc| utc.with_timezone(&tz).date_naive())
}

/// Today's completed intervals from `history`, laid out on a scale fitted to
/// the 15-minute extremes, which the hourly means can never exceed.
pub fn energy_flows_view(history: &IntervalHistory, now: Timestamp, tz: Tz) -> EnergyFlowsView {
    let today = local_date(now, tz);
    let day = today
        .and_then(|date| Day::of(date, tz))
        .unwrap_or_else(|| Day::fallback(now, tz));
    let quarters = today
        .map(|date| history.completed_on(date, tz, now))
        .unwrap_or_default();
    let current_hour = FlowResolution::Hour.in_progress(now);
    let hours: Vec<IntervalSlot> = hourly(&quarters)
        .into_iter()
        .filter(|hour| hour.index < current_hour)
        .collect();

    let scale = FlowScale::fitting(quarters.iter().flat_map(|slot| {
        FlowSeries::ALL
            .into_iter()
            .filter_map(|series| series.watts(&slot.averages))
    }));

    let hourly_plot = plot(FlowResolution::Hour, &hours, now, &day, &scale, tz);
    let quarterly_plot = plot(FlowResolution::Quarter, &quarters, now, &day, &scale, tz);
    let readout = hourly_plot
        .hits
        .iter()
        .find(|hit| hit.latest)
        .map_or_else(FlowReadoutView::empty, |hit| hit.readout.clone());

    let ticks = scale.ticks();
    EnergyFlowsView {
        y_axis: ticks
            .iter()
            .map(|&tick| {
                AxisTick::new(
                    AxisPosition::new(scale.y(tick.as_f64()) / FLOWS_CHART_HEIGHT),
                    tick_label(tick),
                    AxisDensity::Always,
                )
            })
            .collect(),
        x_axis: day_axis(),
        grid_lines: ticks
            .iter()
            .filter(|&&tick| tick != Watts::ZERO)
            .map(|tick| scale.y(tick.as_f64()))
            .collect(),
        zero_y: scale.y(0.0),
        plots: [hourly_plot, quarterly_plot],
        readout,
    }
}

#[cfg(test)]
#[path = "flows_tests.rs"]
mod tests;
