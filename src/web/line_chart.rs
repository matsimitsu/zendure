//! One series over the rolling 24 hours: the geometry and labels the
//! `line_chart` component draws, computed here so the template only places
//! them.

use chrono_tz::Tz;

use crate::units::{BatteryPower, GridPower, Soc, SolarPower, Watts};

use super::axis::{AxisDensity, AxisPosition, AxisTick};
use super::entity::Entity;
use super::intervals::{IntervalAverages, IntervalSlot};
use super::view::{SignStyle, format_kw, format_time};

/// The plot's viewBox. The SVG stretches to its box, so these are user units,
/// not pixels.
pub const LINE_CHART_WIDTH: f64 = 1000.0;
pub const LINE_CHART_HEIGHT: f64 = 180.0;

/// Fifteen-minute slots between two wide-screen time labels: three hours.
const SLOTS_PER_TIME_LABEL: usize = 12;

/// How a chart's value range is chosen and its ticks are labelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChartScale {
    /// Fitted to the data, always including zero, in kW.
    Power,
    /// The whole 0–100% range, whatever the data covers.
    Percent,
}

/// A quantity a line chart can draw. Implemented per role rather than taken
/// as `f64`, so a chart of one quantity cannot be fed another (`RUST-2`).
pub(crate) trait Charted: Copy {
    const SCALE: ChartScale;
    /// Watts for [`ChartScale::Power`], percent for [`ChartScale::Percent`].
    fn plot(self) -> f64;
    /// What a reader is shown for one bucket.
    fn readout(self) -> String;
}

impl Charted for SolarPower {
    const SCALE: ChartScale = ChartScale::Power;
    fn plot(self) -> f64 {
        self.get()
    }
    fn readout(self) -> String {
        kw_readout(self.into_watts())
    }
}

impl Charted for Watts {
    const SCALE: ChartScale = ChartScale::Power;
    fn plot(self) -> f64 {
        self.as_f64()
    }
    fn readout(self) -> String {
        kw_readout(self)
    }
}

impl Charted for GridPower {
    const SCALE: ChartScale = ChartScale::Power;
    fn plot(self) -> f64 {
        self.get()
    }
    fn readout(self) -> String {
        kw_readout(self.into_watts())
    }
}

impl Charted for BatteryPower {
    const SCALE: ChartScale = ChartScale::Power;
    fn plot(self) -> f64 {
        self.as_f64()
    }
    fn readout(self) -> String {
        kw_readout(self.into_watts())
    }
}

impl Charted for Soc {
    const SCALE: ChartScale = ChartScale::Percent;
    fn plot(self) -> f64 {
        f64::from(self.get())
    }
    fn readout(self) -> String {
        format!("{:.1}%", self.plot())
    }
}

/// Signed, so a flow always reads as a direction.
fn kw_readout(watts: Watts) -> String {
    format!("{} kW", format_kw(watts, 2, SignStyle::Explicit))
}

/// What one chart shows, before the data: its labels and any reference marks.
pub struct LineChartSpec<T> {
    pub series: Entity,
    pub title: &'static str,
    pub note: Option<String>,
    /// Drawn as dashed horizontal lines.
    pub limits: Vec<T>,
    /// Shaded horizontal zones, each between two values in either order.
    pub bands: Vec<(T, T)>,
}

impl<T> LineChartSpec<T> {
    pub fn new(series: Entity, title: &'static str) -> Self {
        Self {
            series,
            title,
            note: None,
            limits: Vec::new(),
            bands: Vec::new(),
        }
    }

    pub fn note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }
}

/// A horizontal zone, in viewBox units from the top.
pub struct BandView {
    pub y: f64,
    pub height: f64,
}

/// One bucket, kept whether or not it has data so a hover can address every
/// slot by index. The server renders no per-bucket markup: these are the
/// strings a hover readout copies.
pub struct LinePointView {
    /// `13:15–13:30`.
    pub time: String,
    /// `+1.42 kW`, or `—` for a gap.
    pub value: String,
    /// Viewbox units from the top; `None` for a gap.
    pub y: Option<f64>,
}

pub struct LineChartView {
    pub series: Entity,
    pub title: &'static str,
    pub note: Option<String>,
    pub y_ticks: Vec<AxisTick>,
    /// Viewbox `y` of each grid line, in the order of `y_ticks`.
    pub grid_lines: Vec<f64>,
    pub limit_lines: Vec<f64>,
    pub bands: Vec<BandView>,
    pub zero_y: f64,
    /// SVG path `d`: one `M` subpath per run of buckets with data.
    pub line_path: String,
    /// The same runs, each closed down to the zero line.
    pub area_path: String,
    pub x_ticks: Vec<AxisTick>,
    pub points: Vec<LinePointView>,
}

/// The value range a chart spans, with the step between its ticks.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Domain {
    low: f64,
    high: f64,
    step: f64,
}

impl Domain {
    fn fit(scale: ChartScale, values: impl Iterator<Item = f64>) -> Self {
        match scale {
            ChartScale::Percent => Domain {
                low: 0.0,
                high: 100.0,
                step: 25.0,
            },
            ChartScale::Power => {
                let (low, high) =
                    values.fold((0.0_f64, 0.0_f64), |(lo, hi), v| (lo.min(v), hi.max(v)));
                let step = nice_step(high - low);
                // `+ 0.0` keeps a floored `-0.0` from labelling as "−0".
                let low = (low / step).floor() * step + 0.0;
                let high = ((high / step).ceil() * step).max(low + step);
                Domain { low, high, step }
            }
        }
    }

    /// Viewbox `y` of `value`, clamped so a mark outside the range sits on its
    /// edge rather than off the plot.
    fn y(self, value: f64) -> f64 {
        let clamped = value.clamp(self.low, self.high);
        (self.high - clamped) / (self.high - self.low) * LINE_CHART_HEIGHT
    }

    fn ticks(self) -> impl Iterator<Item = f64> {
        let count = ((self.high - self.low) / self.step).round() as usize;
        (0..=count).map(move |i| self.low + self.step * i as f64)
    }
}

/// Coarse enough that a 180-unit-tall plot never carries more than about
/// seven labels.
fn nice_step(range: f64) -> f64 {
    if range > 6000.0 {
        2000.0
    } else if range > 3000.0 {
        1000.0
    } else {
        500.0
    }
}

fn tick_label(scale: ChartScale, value: f64) -> String {
    match scale {
        ChartScale::Percent => format!("{value:.0}%"),
        ChartScale::Power => {
            let watts = Watts::rounded(value);
            let places = if watts.get() % 1000 == 0 { 0 } else { 1 };
            format_kw(watts, places, SignStyle::Negative)
        }
    }
}

fn x_of(index: usize, count: usize) -> f64 {
    if count < 2 {
        return 0.0;
    }
    index as f64 / (count - 1) as f64 * LINE_CHART_WIDTH
}

/// One subpath per run of present values; a gap ends the run rather than
/// being interpolated across, so a missing bucket reads as missing.
fn runs(ys: &[Option<f64>]) -> Vec<Vec<(f64, f64)>> {
    let mut runs: Vec<Vec<(f64, f64)>> = Vec::new();
    let mut drawing = false;
    for (i, y) in ys.iter().enumerate() {
        match y {
            Some(y) => {
                if !drawing {
                    runs.push(Vec::new());
                    drawing = true;
                }
                if let Some(run) = runs.last_mut() {
                    run.push((x_of(i, ys.len()), *y));
                }
            }
            None => drawing = false,
        }
    }
    runs
}

fn run_path(run: &[(f64, f64)]) -> String {
    run.iter()
        .enumerate()
        .map(|(i, (x, y))| format!("{}{x:.1},{y:.1}", if i == 0 { "M" } else { "L" }))
        .collect::<Vec<_>>()
        .join(" ")
}

fn line_path(ys: &[Option<f64>]) -> String {
    runs(ys)
        .iter()
        .map(|run| run_path(run))
        .collect::<Vec<_>>()
        .join(" ")
}

fn area_path(ys: &[Option<f64>], zero_y: f64) -> String {
    runs(ys)
        .iter()
        .filter_map(|run| {
            let (first, last) = (run.first()?.0, run.last()?.0);
            Some(format!(
                "{} L{last:.1},{zero_y:.1} L{first:.1},{zero_y:.1} Z",
                run_path(run)
            ))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Every three hours on a wide screen; only the window's start, middle and
/// end on a narrow one. Each label names its slot's local start time.
fn time_axis(slots: &[IntervalSlot], tz: Tz) -> Vec<AxisTick> {
    let last = slots.len().saturating_sub(1);
    let middle = slots.len() / 2 / SLOTS_PER_TIME_LABEL * SLOTS_PER_TIME_LABEL;
    slots
        .iter()
        .enumerate()
        .filter(|(i, _)| i % SLOTS_PER_TIME_LABEL == 0 || *i == last)
        // A three-hourly label just short of the end would collide with it.
        .filter(|(i, _)| *i == last || last - i >= SLOTS_PER_TIME_LABEL / 2)
        .map(|(i, slot)| {
            let density = if i == 0 || i == middle || i == last {
                AxisDensity::Always
            } else {
                AxisDensity::WideOnly
            };
            AxisTick::new(
                AxisPosition::new(x_of(i, slots.len()) / LINE_CHART_WIDTH),
                format_time(slot.index.start(), tz),
                density,
            )
        })
        .collect()
}

fn time_range(slot: &IntervalSlot, tz: Tz) -> String {
    format!(
        "{}–{}",
        format_time(slot.index.start(), tz),
        format_time(slot.index.offset(1).start(), tz)
    )
}

impl LineChartView {
    /// The readout a chart shows when nothing is hovered: the latest bucket.
    pub fn default_value(&self) -> &str {
        self.points.last().map_or("—", |p| p.value.as_str())
    }

    /// `pick` chooses the series out of each slot's averages, so one call
    /// site names which quantity it charts.
    pub(crate) fn build<T: Charted>(
        spec: LineChartSpec<T>,
        slots: &[IntervalSlot],
        pick: impl Fn(&IntervalAverages) -> Option<T>,
        tz: Tz,
    ) -> Self {
        let values: Vec<Option<T>> = slots.iter().map(|slot| pick(&slot.averages)).collect();
        let domain = Domain::fit(
            T::SCALE,
            values
                .iter()
                .flatten()
                .map(|v| v.plot())
                .chain(spec.limits.iter().map(|v| v.plot())),
        );
        let ys: Vec<Option<f64>> = values
            .iter()
            .map(|v| v.map(|v| domain.y(v.plot())))
            .collect();
        let zero_y = domain.y(0.0);

        let tick_values: Vec<f64> = domain.ticks().collect();
        let y_ticks = tick_values
            .iter()
            .map(|&value| {
                AxisTick::new(
                    AxisPosition::new(domain.y(value) / LINE_CHART_HEIGHT),
                    tick_label(T::SCALE, value),
                    AxisDensity::Always,
                )
            })
            .collect();

        let bands = spec
            .bands
            .iter()
            .map(|(a, b)| {
                let (top, bottom) = (domain.y(a.plot()), domain.y(b.plot()));
                BandView {
                    y: top.min(bottom),
                    height: (top - bottom).abs(),
                }
            })
            .collect();

        let points = slots
            .iter()
            .zip(&values)
            .zip(&ys)
            .map(|((slot, value), y)| LinePointView {
                time: time_range(slot, tz),
                value: value.map_or_else(|| "—".to_string(), Charted::readout),
                y: *y,
            })
            .collect();

        LineChartView {
            series: spec.series,
            title: spec.title,
            note: spec.note,
            y_ticks,
            grid_lines: tick_values.iter().map(|&v| domain.y(v)).collect(),
            limit_lines: spec.limits.iter().map(|v| domain.y(v.plot())).collect(),
            bands,
            zero_y,
            line_path: line_path(&ys),
            area_path: area_path(&ys, zero_y),
            x_ticks: time_axis(slots, tz),
            points,
        }
    }
}

#[cfg(test)]
#[path = "line_chart_tests.rs"]
mod tests;
