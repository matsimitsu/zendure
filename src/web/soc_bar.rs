//! The SOC bar: the fill, the window the controller keeps the battery in,
//! and the labels that name that window.

use crate::units::Soc;
use crate::web::axis::AxisAnchor;

/// The SOC window the bar marks with limit ticks.
#[derive(Clone, Copy)]
pub struct SocLimitsView {
    pub min: Soc,
    pub max: Soc,
    pub balance_day: bool,
}

/// Below this many points between the limits, two labels on one line would
/// collide at phone width, so the max label drops to a second line.
const STACK_BELOW_POINTS: u32 = 40;
/// Past these points a label anchored outward would cross the bar's edge.
const MIN_LABEL_EDGE_POINTS: u32 = 15;
const MAX_LABEL_EDGE_POINTS: u32 = 85;

pub struct SocBarLabel {
    pub anchor: AxisAnchor,
    pub long: String,
    /// The phone-width form; equal to `long` where nothing needs shortening.
    pub short: String,
}

pub struct SocBarLabels {
    pub min: SocBarLabel,
    pub max: SocBarLabel,
    pub stacked: bool,
}

pub struct SocBarView {
    pub fill: Soc,
    pub limits: SocLimitsView,
    /// `None` is the compact bar: ticks only, no stripes and no labels.
    pub labels: Option<SocBarLabels>,
}

impl SocBarView {
    /// Ticks only: a pack row has no room for stripes or labels, and the
    /// battery panel above it already names the limits.
    pub fn compact(fill: Soc, limits: SocLimitsView) -> Self {
        Self {
            fill,
            limits,
            labels: None,
        }
    }

    pub fn labelled(fill: Soc, limits: SocLimitsView) -> Self {
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
    // limits are close; that case stacks them. A label that would cross the
    // bar's edge anchors inward instead.
    fn for_limits(limits: &SocLimitsView) -> Self {
        let min_anchor = if limits.min.get() < MIN_LABEL_EDGE_POINTS {
            AxisAnchor::Start
        } else {
            AxisAnchor::End
        };
        let max_anchor = if limits.max.get() > MAX_LABEL_EDGE_POINTS {
            AxisAnchor::End
        } else {
            AxisAnchor::Start
        };
        let min = format!("min {}%", limits.min);
        let max = format!("max {}%", limits.max);
        let (max_long, max_short) = if limits.balance_day {
            (format!("{max} · balance day"), format!("{max} ⚖"))
        } else {
            (max.clone(), max)
        };
        Self {
            min: SocBarLabel {
                anchor: min_anchor,
                short: min.clone(),
                long: min,
            },
            max: SocBarLabel {
                anchor: max_anchor,
                long: max_long,
                short: max_short,
            },
            stacked: limits.max.get().saturating_sub(limits.min.get()) < STACK_BELOW_POINTS,
        }
    }
}
