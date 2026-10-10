//! Geometry every day chart shares: where a moment of the local day falls
//! along the x axis, and where a value falls on a linear y scale. Both are in
//! viewBox units, stretched to the panel by `preserveAspectRatio="none"`.

use std::time::Duration;

use chrono::{NaiveDate, NaiveTime, TimeZone};
use chrono_tz::Tz;

use crate::clock::{local_date, local_day_bounds};
use crate::units::{Elapsed, Timestamp};

use super::axis::{AxisDensity, AxisPosition, AxisTick, day_axis};

/// Every day chart's viewBox width.
pub(super) const CHART_WIDTH: f64 = 1000.0;

/// The viewBox height every day chart shares. It matches `--size-chart-day`, so
/// the bars are drawn at the proportions they are shown at.
pub(super) const DAY_CHART_HEIGHT: f64 = 160.0;

/// A bar's corner radius, in viewBox units.
pub(super) const BAR_RADIUS: f64 = 1.0;

/// The share of a slot left empty on each side of its bar.
const BAR_INSET: f64 = 0.14;

const HOUR: Duration = Duration::from_secs(60 * 60);

/// A rectangle's horizontal extent, in viewBox units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SlotSpan {
    pub x: f64,
    pub width: f64,
}

impl SlotSpan {
    /// The bar drawn in this slot, inset so neighbouring bars stand apart.
    pub fn bar(self) -> SlotSpan {
        SlotSpan {
            x: self.x + self.width * BAR_INSET,
            width: self.width * (1.0 - 2.0 * BAR_INSET),
        }
    }
}

/// The local day a chart spans, as an absolute window: 23 or 25 hours on a
/// DST change.
pub(super) struct Day {
    pub start: Timestamp,
    length: Elapsed,
}

impl Day {
    /// Falls back to the UTC day for a date `tz` cannot place, so the chart
    /// still has a frame to draw its gaps in.
    pub fn of(date: NaiveDate, tz: Tz) -> Self {
        match local_day_bounds(date, tz) {
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

    pub fn end(&self) -> Timestamp {
        self.start + self.length
    }

    /// The slot of `span` that `at` falls in; `None` outside the day.
    pub fn slot_containing(&self, span: Duration, at: Timestamp) -> Option<Timestamp> {
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
    /// narrow screen only the hours divisible by `narrow_every` and `23:59`
    /// remain.
    pub fn axis(&self, tz: Tz, narrow_every: u32) -> Vec<AxisTick> {
        let Some(date) = local_date(self.start, tz) else {
            return day_axis();
        };
        let hours = (0..24).step_by(3).filter_map(|hour| {
            // A local hour a DST change skips has no tick.
            let at = tz
                .from_local_datetime(&date.and_hms_opt(hour, 0, 0)?)
                .earliest()?;
            let density = if hour % narrow_every == 0 {
                AxisDensity::Always
            } else {
                AxisDensity::WideOnly
            };
            Some(AxisTick::new(
                AxisPosition::new(self.x(Timestamp::from(at)) / CHART_WIDTH),
                format!("{hour:02}:00"),
                density,
            ))
        });
        let end = AxisTick::new(AxisPosition::END, "23:59".to_string(), AxisDensity::Always);
        hours.chain(std::iter::once(end)).collect()
    }

    /// `span` as a share of the viewBox's width.
    pub fn width(&self, span: Elapsed) -> f64 {
        let length = self.length.as_secs_f64();
        if length <= 0.0 {
            return 0.0;
        }
        span.as_secs_f64() / length * CHART_WIDTH
    }

    pub fn x(&self, at: Timestamp) -> f64 {
        self.width(at - self.start)
    }

    /// `[start, end)` along the x axis.
    pub fn span(&self, start: Timestamp, end: Timestamp) -> SlotSpan {
        let x = self.x(start);
        SlotSpan {
            x,
            width: self.x(end) - x,
        }
    }

    pub fn slot_starts(&self, span: Duration) -> impl Iterator<Item = Timestamp> {
        let span = Elapsed::of(span);
        let count = self.length.as_millis() / span.as_millis().max(1);
        std::iter::successors(Some(self.start), move |&start| Some(start + span))
            .take(usize::try_from(count).unwrap_or(0))
    }
}

/// A linear y scale: `top` at the viewBox's top edge, `bottom` at `height`.
/// Callers convert their unit to `f64` once, when building it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct YScale {
    top: f64,
    bottom: f64,
    height: f64,
}

impl YScale {
    pub fn new(top: f64, bottom: f64, height: f64) -> Self {
        YScale {
            top,
            bottom,
            height,
        }
    }

    pub fn y(&self, value: f64) -> f64 {
        let range = self.top - self.bottom;
        if range <= 0.0 {
            return self.height;
        }
        (self.top - value) / range * self.height
    }

    /// A bar's `(y, height)`: up from the zero line for a positive value,
    /// down from it otherwise, and never thinner than `min_height`.
    pub fn bar(&self, value: f64, min_height: f64) -> (f64, f64) {
        let zero = self.y(0.0);
        let height = (zero - self.y(value)).abs().max(min_height);
        if value > 0.0 {
            (zero - height, height)
        } else {
            (zero, height)
        }
    }
}

#[cfg(test)]
pub(super) mod testing {
    use super::{CHART_WIDTH, SlotSpan};

    /// Hit targets tile the chart left to right without overlapping or
    /// overrunning it.
    pub fn assert_tiled(spans: impl IntoIterator<Item = SlotSpan>) {
        let mut edge = 0.0;
        for span in spans {
            assert!((span.x - edge).abs() < 1e-6, "gap or overlap at {edge}");
            edge = span.x + span.width;
        }
        assert!((edge - CHART_WIDTH).abs() < 1e-6);
    }
}
