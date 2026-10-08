//! Axis ticks, shared by every chart that labels an axis: where each label
//! sits along its track, and which ones survive a narrow screen.

/// How far along its track a tick sits, as a fraction of the track.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AxisPosition(f64);

impl AxisPosition {
    pub const START: Self = Self(0.0);
    pub const END: Self = Self(1.0);

    pub fn new(fraction: f64) -> Self {
        Self(fraction.clamp(0.0, 1.0))
    }

    pub fn of_day_hour(hour: u32) -> Self {
        Self::new(f64::from(hour) / 24.0)
    }

    pub fn percent(self) -> f64 {
        self.0 * 100.0
    }

    fn anchor(self) -> AxisAnchor {
        if self == Self::START {
            AxisAnchor::Start
        } else if self == Self::END {
            AxisAnchor::End
        } else {
            AxisAnchor::Middle
        }
    }
}

/// Which point of the label sits on its position. The end ticks lean inward
/// so their text never crosses the panel edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AxisAnchor {
    Start,
    Middle,
    End,
}

impl AxisAnchor {
    pub fn modifier(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Middle => "middle",
            Self::End => "end",
        }
    }
}

/// Whether a tick survives the narrow layout, where only a few labels fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AxisDensity {
    Always,
    WideOnly,
}

impl AxisDensity {
    pub fn modifier(self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::WideOnly => "wide-only",
        }
    }
}

pub struct AxisTick {
    pub position: AxisPosition,
    pub anchor: AxisAnchor,
    pub density: AxisDensity,
    pub label: String,
}

impl AxisTick {
    pub fn new(position: AxisPosition, label: String, density: AxisDensity) -> Self {
        Self {
            anchor: position.anchor(),
            position,
            density,
            label,
        }
    }
}

/// Every three hours on a wide screen; only `00:00 · 12:00 · 23:59` on a
/// narrow one. The last tick is `23:59` so it names the day's final minute
/// rather than the next midnight.
pub fn day_axis() -> Vec<AxisTick> {
    let hourly = (0..24).step_by(3).map(|hour| {
        let density = if hour % 12 == 0 {
            AxisDensity::Always
        } else {
            AxisDensity::WideOnly
        };
        AxisTick::new(
            AxisPosition::of_day_hour(hour),
            format!("{hour:02}:00"),
            density,
        )
    });
    let end = AxisTick::new(AxisPosition::END, "23:59".to_string(), AxisDensity::Always);
    hourly.chain(std::iter::once(end)).collect()
}

#[cfg(test)]
#[path = "axis_tests.rs"]
mod tests;
