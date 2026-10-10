//! The view model every day panel shares: nothing to show, a day the nav
//! reaches with nothing on it, or a day charted over the shared plot frame.
//! Each panel supplies what it charts on that day as `T`.

use chrono::NaiveDate;

use super::axis::AxisTick;
use super::day_nav::DayNavView;
use super::plot::SlotSpan;

pub enum DayPanel<T> {
    Empty(EmptyReason),
    /// A day the nav reaches that holds nothing to chart, e.g. a gap in the
    /// journal: the nav stays so the user can step on.
    Blank(DayNavView),
    Shown(Box<T>),
}

/// A day a panel charts, which always carries its nav.
pub trait ChartedDay {
    fn nav(&self) -> &DayNavView;
    fn nav_mut(&mut self) -> &mut DayNavView;
}

impl<T: ChartedDay> DayPanel<T> {
    pub fn nav(&self) -> Option<&DayNavView> {
        match self {
            DayPanel::Empty(_) => None,
            DayPanel::Blank(nav) => Some(nav),
            DayPanel::Shown(day) => Some(day.nav()),
        }
    }

    pub fn nav_mut(&mut self) -> Option<&mut DayNavView> {
        match self {
            DayPanel::Empty(_) => None,
            DayPanel::Blank(nav) => Some(nav),
            DayPanel::Shown(day) => Some(day.nav_mut()),
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

/// Why a panel has no day to show. Each panel words it for its own feed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmptyReason {
    NotConfigured,
    /// Configured, but nothing has landed yet.
    Waiting,
}

/// What every day chart draws besides its series: the scale, the time axis,
/// and where the current slot is.
#[derive(Debug, Clone)]
pub struct DayPlotView {
    /// One per y-axis tick.
    pub grid_lines: Vec<f64>,
    pub now_x: Option<f64>,
    /// Where the highlight sits unhovered: the current slot, today only.
    pub highlight: Option<SlotSpan>,
    pub y_axis: Vec<AxisTick>,
    pub x_axis: Vec<AxisTick>,
}
