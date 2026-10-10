//! The energy-flows chart's view model: today's intervals as grouped, signed
//! bars on one scale, at both resolutions, so switching between them is a
//! class toggle in the browser rather than a round trip.

use std::collections::BTreeMap;
use std::str::FromStr;
use std::time::Duration;

use chrono::NaiveDate;
use chrono_tz::Tz;

use crate::clock::local_date;
use crate::units::{BatteryPower, Elapsed, GridPower, SolarPower, Timestamp, Watts};

use super::axis::{AxisDensity, AxisPosition, AxisTick, power_step};
use super::day_nav::DayNavView;
use super::entity::Entity;
use super::intervals::{
    HOUR, INTERVAL, IntervalAverages, IntervalHistory, IntervalIndex, IntervalSlot, merged,
};
use super::plot::{CHART_WIDTH, Day, YScale};
use super::view::{MISSING, SignStyle, format_kw, format_time};

/// The plots' viewBox. Stretched to the panel (`preserveAspectRatio="none"`),
/// so these are proportions rather than pixels.
pub(super) const FLOWS_CHART_WIDTH: f64 = CHART_WIDTH;
pub(super) const FLOWS_CHART_HEIGHT: f64 = 260.0;

/// Keeps a non-zero flow visible on a scale sized for kilowatts.
const MIN_BAR_HEIGHT: f64 = 0.6;

/// The most steps a scale may span before the next step up is tried.
const MAX_STEPS: i32 = 6;

/// One series' flow over an interval, as the chart draws it. Grid is positive
/// importing and battery positive discharging, as their role types already
/// are, so below zero reads as export and charge.
fn flow(series: Entity, averages: &IntervalAverages) -> Option<Watts> {
    match series {
        Entity::Solar => averages.solar.map(SolarPower::into_watts),
        Entity::Home => averages.home,
        Entity::Grid => averages.grid.map(GridPower::into_watts),
        Entity::Battery => averages.battery.map(BatteryPower::into_watts),
    }
}

/// One of the two plots the panel carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
}

/// The `interval=` a request carries.
impl FromStr for FlowResolution {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        [Self::Hour, Self::Quarter]
            .into_iter()
            .find(|resolution| resolution.key() == raw)
            .ok_or_else(|| format!("unknown interval {raw:?}; expected 1h or 15m"))
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
    fn fitting(values: impl Iterator<Item = Watts>) -> Self {
        let (high, low) = values.fold((Watts::ZERO, Watts::ZERO), |(hi, lo), v| {
            (hi.max(v), lo.min(v))
        });
        // How many `step`s cover `extent`; saturating, so an absurd reading
        // costs a tall axis, not a wrap.
        let steps = |step: Watts, extent: Watts| {
            let count = extent
                .get()
                .unsigned_abs()
                .div_ceil(step.get().unsigned_abs());
            i32::try_from(count).unwrap_or(i32::MAX)
        };
        let step = power_step(|step| {
            steps(step, high).max(1).saturating_add(steps(step, -low)) <= MAX_STEPS
        });
        FlowScale {
            top: Watts(step.get().saturating_mul(steps(step, high).max(1))),
            bottom: Watts(step.get().saturating_mul(-steps(step, -low))),
            step,
        }
    }

    fn plot(&self) -> YScale {
        YScale::new(self.top.as_f64(), self.bottom.as_f64(), FLOWS_CHART_HEIGHT)
    }

    fn y(&self, watts: Watts) -> f64 {
        self.plot().y(watts.as_f64())
    }

    /// A bar's `(y, height)`: up from the zero line for a positive flow,
    /// down from it for a negative one.
    fn bar(&self, watts: Watts) -> (f64, f64) {
        self.plot().bar(watts.as_f64(), MIN_BAR_HEIGHT)
    }

    /// Every step from the top down to the bottom, inclusive.
    fn ticks(&self) -> Vec<Watts> {
        std::iter::successors(Some(self.top), |&tick| Some(tick - self.step))
            .take_while(|&tick| tick >= self.bottom)
            .collect()
    }
}

/// A y-axis label: kilowatts to one place, and a bare zero.
fn tick_label(watts: Watts) -> String {
    if watts == Watts::ZERO {
        return "0".to_string();
    }
    format_kw(watts, 1, SignStyle::Negative)
}

/// A readout value: signed kilowatts to two places, so every series reads as
/// a direction.
fn readout_value(watts: Option<Watts>) -> String {
    watts.map_or_else(
        || MISSING.to_string(),
        |watts| format_kw(watts, 2, SignStyle::Explicit),
    )
}

/// What the legend shows for one column: its time span and a value per series.
#[derive(Debug, Clone, PartialEq)]
pub struct FlowReadoutView {
    /// `13:00–14:00`, as the hit column's `data-label` carries it.
    pub label: String,
    pub values: [(Entity, String); 4],
}

impl FlowReadoutView {
    fn of(start: Timestamp, span: Duration, averages: &IntervalAverages, tz: Tz) -> Self {
        FlowReadoutView {
            label: format!(
                "{}–{}",
                format_time(start, tz),
                format_time(start + Elapsed::of(span), tz)
            ),
            values: Entity::ALL.map(|series| (series, readout_value(flow(series, averages)))),
        }
    }

    pub fn value(&self, series: Entity) -> &str {
        self.values
            .iter()
            .find(|(each, _)| *each == series)
            .map_or(MISSING, |(_, value)| value)
    }

    fn empty() -> Self {
        FlowReadoutView {
            label: MISSING.to_string(),
            values: Entity::ALL.map(|series| (series, MISSING.to_string())),
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

#[derive(Debug, Clone)]
pub struct FlowBarView {
    pub series: Entity,
    pub rect: FlowRect,
}

/// A full-height column over one slot, the hover target the readout reads.
#[derive(Debug, Clone)]
pub struct FlowHitView {
    pub x: f64,
    pub width: f64,
    pub readout: FlowReadoutView,
    /// The most recent completed slot: what the readout shows unhovered.
    pub latest: bool,
}

#[derive(Debug, Clone)]
pub struct FlowPlotView {
    pub resolution: FlowResolution,
    pub bars: Vec<FlowBarView>,
    /// One per slot of the day, completed or not, in order.
    pub hits: Vec<FlowHitView>,
    /// The start of the in-progress slot; `None` when it is outside the day.
    pub now_x: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct EnergyFlowsView {
    pub nav: DayNavView,
    /// The plot the server shows: the segmented control's pressed button,
    /// and the interval the step links carry.
    pub interval: FlowResolution,
    pub y_axis: Vec<AxisTick>,
    pub x_axis: Vec<AxisTick>,
    /// Gridline heights, one per y-axis tick other than zero.
    pub grid_lines: Vec<f64>,
    pub zero_y: f64,
    /// The hourly plot first: it is the one shown by default.
    pub plots: [FlowPlotView; 2],
    /// The default plot's most recent completed slot.
    pub readout: FlowReadoutView,
    /// The journal could not be read for this day. Drawn as an empty chart it
    /// would claim nothing was measured.
    pub unreadable: bool,
}

impl EnergyFlowsView {
    /// The same day shown at `interval`: both plots are always rendered, so
    /// the interval only picks which one is shown.
    pub fn at_interval(self, interval: FlowResolution) -> Self {
        EnergyFlowsView { interval, ..self }
    }
}

/// One slot of the day as a plot draws it: `None` until it has finished.
struct FlowSlot {
    start: Timestamp,
    averages: Option<IntervalAverages>,
}

/// Every quarter of the day, looked up by its absolute index.
fn quarter_slots(
    day: &Day,
    completed: &BTreeMap<IntervalIndex, IntervalAverages>,
) -> Vec<FlowSlot> {
    day.slot_starts(INTERVAL)
        .map(|start| FlowSlot {
            start,
            averages: completed.get(&IntervalIndex::containing(start)).copied(),
        })
        .collect()
}

/// Every hour of the day on the local clock, merged from its quarters.
/// Grouped from local midnight rather than by UTC hour, so a zone offset by
/// half an hour still reads 10:00–11:00 as its own ten o'clock.
fn hour_slots(
    day: &Day,
    completed: &BTreeMap<IntervalIndex, IntervalAverages>,
    now: Timestamp,
) -> Vec<FlowSlot> {
    day.slot_starts(HOUR)
        .map(|start| {
            let end = start + Elapsed::of(HOUR);
            let quarters: Vec<IntervalSlot> =
                std::iter::successors(Some(IntervalIndex::containing(start)), |index| {
                    Some(index.offset(1))
                })
                .take_while(|index| index.start() < end)
                .filter_map(|index| {
                    completed
                        .get(&index)
                        .map(|&averages| IntervalSlot { index, averages })
                })
                .collect();
            FlowSlot {
                start,
                averages: (end <= now && !quarters.is_empty()).then(|| merged(&quarters)),
            }
        })
        .collect()
}

fn plot(
    resolution: FlowResolution,
    slots: &[FlowSlot],
    now: Timestamp,
    day: &Day,
    scale: &FlowScale,
    tz: Tz,
) -> FlowPlotView {
    let span = resolution.span();
    let slot_width = day.width(Elapsed::of(span));
    let padding = slot_width * resolution.group_padding();
    // Each series' lane, counted in lane widths from the group's left edge.
    let lanes: Vec<(f64, Entity)> = std::iter::successors(Some(0.0), |lane| Some(lane + 1.0))
        .zip(Entity::ALL)
        .collect();
    let lane_count = lanes.last().map_or(1.0, |&(last, _)| last + 1.0);
    let lane = (slot_width - 2.0 * padding) / lane_count;
    let bar_width = lane * resolution.bar_fill();

    let latest = slots
        .iter()
        .rev()
        .find(|slot| slot.averages.is_some())
        .map(|slot| slot.start);

    let mut bars = Vec::new();
    let mut hits = Vec::new();
    for slot in slots {
        let x = day.x(slot.start);
        let Some(averages) = &slot.averages else {
            hits.push(FlowHitView {
                x,
                width: slot_width,
                readout: FlowReadoutView::of(slot.start, span, &IntervalAverages::default(), tz),
                latest: false,
            });
            continue;
        };
        for &(lane_index, series) in &lanes {
            let Some(watts) = flow(series, averages).filter(|&w| w != Watts::ZERO) else {
                continue;
            };
            let (y, height) = scale.bar(watts);
            bars.push(FlowBarView {
                series,
                rect: FlowRect {
                    x: x + padding + lane * lane_index + (lane - bar_width) / 2.0,
                    y,
                    width: bar_width,
                    height,
                },
            });
        }
        hits.push(FlowHitView {
            x,
            width: slot_width,
            readout: FlowReadoutView::of(slot.start, span, averages, tz),
            latest: Some(slot.start) == latest,
        });
    }

    FlowPlotView {
        resolution,
        bars,
        hits,
        now_x: day.slot_containing(span, now).map(|start| day.x(start)),
    }
}

/// What the panel was asked to show, already resolved against today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FlowsRequest {
    pub day: NaiveDate,
    pub interval: FlowResolution,
}

/// Today in the hourly view, as the live stream renders it. The step back
/// is always offered: the journal is not read on every tick.
pub fn energy_flows_view(history: &IntervalHistory, now: Timestamp, tz: Tz) -> EnergyFlowsView {
    let request = FlowsRequest {
        day: local_date(now, tz).unwrap_or_default(),
        interval: FlowResolution::Hour,
    };
    requested_flows_view(history, request, None, now, tz)
}

/// `request.day`'s completed intervals from `history`, laid out on a scale
/// fitted to the 15-minute extremes, which the hourly means can never exceed.
/// `earliest` is the oldest day the journal still holds.
pub fn requested_flows_view(
    history: &IntervalHistory,
    request: FlowsRequest,
    earliest: Option<NaiveDate>,
    now: Timestamp,
    tz: Tz,
) -> EnergyFlowsView {
    let today = local_date(now, tz).unwrap_or_default();
    let day = Day::of(request.day, tz);
    let completed: BTreeMap<IntervalIndex, IntervalAverages> = history
        .completed_on(request.day, tz, now)
        .into_iter()
        .map(|slot| (slot.index, slot.averages))
        .collect();

    let scale = FlowScale::fitting(completed.values().flat_map(|averages| {
        Entity::ALL
            .into_iter()
            .filter_map(|series| flow(series, averages))
    }));

    let hours = hour_slots(&day, &completed, now);
    let quarters = quarter_slots(&day, &completed);
    let hourly_plot = plot(FlowResolution::Hour, &hours, now, &day, &scale, tz);
    let quarterly_plot = plot(FlowResolution::Quarter, &quarters, now, &day, &scale, tz);
    let readout = hourly_plot
        .hits
        .iter()
        .find(|hit| hit.latest)
        .map_or_else(FlowReadoutView::empty, |hit| hit.readout.clone());

    let ticks = scale.ticks();
    EnergyFlowsView {
        nav: DayNavView::new(request.day, today, earliest, today),
        interval: request.interval,
        y_axis: ticks
            .iter()
            .map(|&tick| {
                AxisTick::new(
                    AxisPosition::new(scale.y(tick) / FLOWS_CHART_HEIGHT),
                    tick_label(tick),
                    AxisDensity::Always,
                )
            })
            .collect(),
        x_axis: day.axis(tz, 12),
        grid_lines: ticks
            .iter()
            .filter(|&&tick| tick != Watts::ZERO)
            .map(|&tick| scale.y(tick))
            .collect(),
        zero_y: scale.y(Watts::ZERO),
        plots: [hourly_plot, quarterly_plot],
        readout,
        unreadable: false,
    }
}

#[cfg(test)]
#[path = "flows_tests.rs"]
mod tests;
