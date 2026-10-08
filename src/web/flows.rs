//! The energy-flows chart's view model: today's intervals as grouped, signed
//! bars on one scale, at both resolutions, so switching between them is a
//! class toggle in the browser rather than a round trip.

use std::collections::BTreeMap;
use std::str::FromStr;
use std::time::Duration;

use chrono::{NaiveDate, NaiveTime, TimeZone};
use chrono_tz::Tz;

use crate::units::{Elapsed, Timestamp, Watts};

use super::axis::{AxisDensity, AxisPosition, AxisTick, day_axis};
use super::intervals::{
    HOUR, INTERVAL, IntervalAverages, IntervalHistory, IntervalIndex, IntervalSlot, merged,
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

/// The local day the chart spans, as an absolute window: 23 or 25 hours on
/// a DST change.
struct Day {
    start: Timestamp,
    length: Elapsed,
}

impl Day {
    /// Falls back to the UTC day for a date `tz` cannot place, so the chart
    /// still has a frame to draw its gaps in.
    fn of(date: NaiveDate, tz: Tz) -> Self {
        let bounds = date.succ_opt().and_then(|next| {
            Some((
                crate::clock::local_day_start(date, tz)?,
                crate::clock::local_day_start(next, tz)?,
            ))
        });
        match bounds {
            Some((start, end)) => Day {
                start,
                length: end - start,
            },
            None => Day {
                start: Timestamp::from(date.and_time(NaiveTime::MIN).and_utc()),
                length: Elapsed::of(24 * HOUR),
            },
        }
    }

    fn end(&self) -> Timestamp {
        self.start + self.length
    }

    /// The slot of `span` that `at` falls in; `None` outside the day.
    fn slot_containing(&self, span: Duration, at: Timestamp) -> Option<Timestamp> {
        (at >= self.start && at < self.end())
            .then(|| {
                self.slot_starts(span)
                    .take_while(|&start| start <= at)
                    .last()
            })
            .flatten()
    }

    /// A tick every three local hours, each placed where that hour really
    /// falls, so a 23- or 25-hour day's labels stay over their bars. On a
    /// narrow screen only `00:00 · 12:00 · 23:59` remain.
    fn axis(&self, tz: Tz) -> Vec<AxisTick> {
        let Some(date) = local_date(self.start, tz) else {
            return day_axis();
        };
        let hours = (0..24).step_by(3).filter_map(|hour| {
            // A local hour a DST change skips has no tick.
            let at = tz
                .from_local_datetime(&date.and_hms_opt(hour, 0, 0)?)
                .earliest()?;
            let density = if hour % 12 == 0 {
                AxisDensity::Always
            } else {
                AxisDensity::WideOnly
            };
            Some(AxisTick::new(
                AxisPosition::new(self.x(Timestamp::from(at)) / FLOWS_CHART_WIDTH),
                format!("{hour:02}:00"),
                density,
            ))
        });
        let end = AxisTick::new(AxisPosition::END, "23:59".to_string(), AxisDensity::Always);
        hours.chain(std::iter::once(end)).collect()
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

#[derive(Debug, Clone)]
pub struct FlowBarView {
    pub series: FlowSeries,
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

/// Which day the panel shows, and where its step controls lead.
#[derive(Debug, Clone, PartialEq)]
pub struct DayNavView {
    pub shown: NaiveDate,
    pub today: NaiveDate,
    /// `None` on the oldest day the journal still holds.
    pub previous: Option<NaiveDate>,
    /// `None` on today: there is nothing to show after it.
    pub next: Option<NaiveDate>,
    /// "Today", "Yesterday" or "Mon 5 Oct".
    pub label: String,
    /// "5 Oct".
    pub date: String,
}

impl DayNavView {
    fn new(shown: NaiveDate, today: NaiveDate, earliest: Option<NaiveDate>) -> Self {
        let label = if shown == today {
            "Today".to_string()
        } else if today.pred_opt() == Some(shown) {
            "Yesterday".to_string()
        } else {
            shown.format("%a %-d %b").to_string()
        };
        DayNavView {
            shown,
            today,
            previous: shown
                .pred_opt()
                .filter(|_| earliest.is_none_or(|earliest| shown > earliest)),
            next: shown.succ_opt().filter(|_| shown < today),
            label,
            date: shown.format("%-d %b").to_string(),
        }
    }

    /// Only today takes the live stream; a past day never changes.
    pub fn live(&self) -> bool {
        self.shown == self.today
    }
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
    let lane = (slot_width - 2.0 * padding) / FlowSeries::ALL.len() as f64;
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

/// The calendar date `at` falls on in `tz`.
pub fn local_date(at: Timestamp, tz: Tz) -> Option<NaiveDate> {
    chrono::DateTime::from_timestamp_millis(at.as_millis())
        .map(|utc| utc.with_timezone(&tz).date_naive())
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
        FlowSeries::ALL
            .into_iter()
            .filter_map(|series| series.watts(averages))
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
        nav: DayNavView::new(request.day, today, earliest),
        interval: request.interval,
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
        x_axis: day.axis(tz),
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
