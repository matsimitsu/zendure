//! The SOC bar: the fill, the window the controller keeps the battery in,
//! and the labels that name that window.

use crate::controller::SocLimits;
use crate::units::Soc;
use crate::web::axis::AxisAnchor;

/// Below this many points between the limits, two labels on one line would
/// collide at phone width, so the max label drops to a second line.
const STACK_BELOW_POINTS: u32 = 40;

/// How many points of the bar one character of label text covers where the
/// bar is narrowest for that form: 11px mono text is ~6.6px a character, on
/// a ~780px bar just above the narrow breakpoint and a ~310px one at 375px.
const WIDE_CHAR_POINTS: f64 = 0.85;
const NARROW_CHAR_POINTS: f64 = 2.13;

pub struct SocBarLabel {
    pub anchor: AxisAnchor,
    pub long: String,
    /// The phone-width form; equal to `long` where nothing needs shortening.
    pub short: String,
}

/// Which way a label would rather run from its limit: away from the other
/// label.
#[derive(Clone, Copy)]
enum Outward {
    TowardEmpty,
    TowardFull,
}

impl SocBarLabel {
    /// Runs outward where both forms fit between the limit and the bar's
    /// edge on that side, and back over the bar where either would cross it.
    fn placed(at: Soc, outward: Outward, long: String, short: String) -> Self {
        let room = match outward {
            Outward::TowardEmpty => at.get(),
            Outward::TowardFull => Soc::FULL.get().saturating_sub(at.get()),
        };
        let needed = points(&long, WIDE_CHAR_POINTS).max(points(&short, NARROW_CHAR_POINTS));
        let anchor = match (outward, needed <= f64::from(room)) {
            (Outward::TowardEmpty, true) | (Outward::TowardFull, false) => AxisAnchor::End,
            (Outward::TowardFull, true) | (Outward::TowardEmpty, false) => AxisAnchor::Start,
        };
        Self {
            anchor,
            long,
            short,
        }
    }
}

/// How many points of the bar `text` covers.
fn points(text: &str, per_char: f64) -> f64 {
    text.chars().map(|_| per_char).sum()
}

pub struct SocBarLabels {
    pub min: SocBarLabel,
    pub max: SocBarLabel,
    pub stacked: bool,
}

pub struct SocBarView {
    pub fill: Soc,
    pub limits: SocLimits,
    /// `None` is the compact bar: ticks only, no stripes and no labels.
    pub labels: Option<SocBarLabels>,
}

impl SocBarView {
    /// Ticks only: a pack row has no room for stripes or labels, and the
    /// battery panel above it already names the limits.
    pub fn compact(fill: Soc, limits: SocLimits) -> Self {
        Self {
            fill,
            limits,
            labels: None,
        }
    }

    pub fn labelled(fill: Soc, limits: SocLimits) -> Self {
        let labels = SocBarLabels::for_limits(&limits);
        Self {
            fill,
            limits,
            labels: Some(labels),
        }
    }
}

impl SocBarLabels {
    // Each label extends away from the other one, so they only meet when the
    // limits are close; that case stacks them.
    fn for_limits(limits: &SocLimits) -> Self {
        let min = format!("min {}%", limits.min);
        let max = format!("max {}%", limits.max);
        let (max_long, max_short) = if limits.balance_day {
            (format!("{max} · balance day"), format!("{max} ⚖"))
        } else {
            (max.clone(), max)
        };
        Self {
            min: SocBarLabel::placed(limits.min, Outward::TowardEmpty, min.clone(), min),
            max: SocBarLabel::placed(limits.max, Outward::TowardFull, max_long, max_short),
            stacked: limits.max.get().saturating_sub(limits.min.get()) < STACK_BELOW_POINTS,
        }
    }
}
