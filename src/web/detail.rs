//! The entities `GET /detail/{entity}` can describe, and what each one's
//! panel summarises from the interval ring.

use std::str::FromStr;

use chrono_tz::Tz;

use crate::units::{SolarPower, WattHours, Watts};

use super::intervals::{
    Averaged, IntervalAverages, IntervalIndex, IntervalSlot, Mean, interval_energy,
};
use super::line_chart::{ChartSeries, LineChartSpec, LineChartView};
use super::state::{DashboardState, Plottable};
use super::view::{
    DetailBodyView, MISSING, MiniStatView, SignStyle, energy_string, format_time, format_watts,
};

/// Parsed from the URL once, at the route, so everything past it holds a
/// valid entity. There is no car variant: its SOC is not journalled, so there
/// is no history to show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DetailEntity {
    Solar,
    Home,
    Grid,
    Battery,
}

impl DetailEntity {
    pub const fn slug(self) -> &'static str {
        match self {
            DetailEntity::Solar => "solar",
            DetailEntity::Home => "home",
            DetailEntity::Grid => "grid",
            DetailEntity::Battery => "battery",
        }
    }

    pub fn path(self) -> String {
        format!("/detail/{}", self.slug())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct UnknownEntity;

impl FromStr for DetailEntity {
    type Err = UnknownEntity;

    fn from_str(slug: &str) -> Result<Self, UnknownEntity> {
        [Self::Solar, Self::Home, Self::Grid, Self::Battery]
            .into_iter()
            .find(|entity| entity.slug() == slug)
            .ok_or(UnknownEntity)
    }
}

// --- What each panel shows ---------------------------------------------------

/// A peak reading and the interval it was averaged over.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Peak<T> {
    pub value: T,
    pub at: IntervalIndex,
}

/// The highest average among the slots that have one; the earliest wins a tie.
pub(crate) fn peak<T: Plottable>(
    slots: &[IntervalSlot],
    pick: impl Fn(&IntervalAverages) -> Option<T>,
) -> Option<Peak<T>> {
    slots
        .iter()
        .filter_map(|slot| {
            pick(&slot.averages).map(|value| Peak {
                value,
                at: slot.index,
            })
        })
        .fold(None, |best: Option<Peak<T>>, next| match best {
            Some(best) if best.value.plot_value() >= next.value.plot_value() => Some(best),
            _ => Some(next),
        })
}

/// Energy across the slots that have a reading; a gap adds nothing rather
/// than being guessed at.
pub(crate) fn energy(
    slots: &[IntervalSlot],
    pick: impl Fn(&IntervalAverages) -> Option<Watts>,
) -> WattHours {
    slots
        .iter()
        .filter_map(|slot| pick(&slot.averages))
        .map(interval_energy)
        .sum()
}

/// Only the importing buckets count; an exporting one adds nothing rather
/// than cancelling an import.
fn imported(slots: &[IntervalSlot]) -> WattHours {
    energy(slots, |a| a.grid.map(|g| g.importing().max(Watts::ZERO)))
}

fn exported(slots: &[IntervalSlot]) -> WattHours {
    energy(slots, |a| a.grid.map(|g| g.exporting().max(Watts::ZERO)))
}

pub(crate) fn average<T: Averaged>(
    slots: &[IntervalSlot],
    pick: impl Fn(&IntervalAverages) -> Option<T>,
) -> Option<T> {
    Mean::of(slots.iter().map(|slot| pick(&slot.averages)))
}

fn stat(label: &'static str, value: String) -> MiniStatView {
    MiniStatView { label, value }
}

fn watts_or_missing(watts: Option<Watts>) -> String {
    watts.map_or_else(
        || MISSING.to_string(),
        |w| format!("{} W", format_watts(w, SignStyle::Negative)),
    )
}

fn peak_string(peak: Option<Peak<Watts>>, tz: Tz) -> String {
    peak.map_or_else(
        || MISSING.to_string(),
        |peak| {
            format!(
                "{} W · {}",
                format_watts(peak.value, SignStyle::Negative),
                format_time(peak.at.start(), tz)
            )
        },
    )
}

fn kwh(energy: WattHours) -> String {
    energy_string(energy.to_kwh())
}

const POWER_TITLE: &str = "Power (kW)";

/// The stats and chart over the rolling 24 hours for `entity`. The battery
/// has no view here yet.
pub fn detail_body(state: &DashboardState, entity: DetailEntity, tz: Tz) -> Option<DetailBodyView> {
    let slots = state.intervals.last_24h(state.as_of);
    let world = &state.engine.world;
    let body = match entity {
        DetailEntity::Solar => {
            let solar = |a: &IntervalAverages| a.solar;
            let solar_watts = |a: &IntervalAverages| a.solar.map(SolarPower::into_watts);
            DetailBodyView {
                stats: vec![
                    stat("Now", watts_or_missing(Some(world.solar.into_watts()))),
                    stat("Peak", peak_string(peak(&slots, solar_watts), tz)),
                    stat("Produced", kwh(energy(&slots, solar_watts))),
                    stat(
                        "Average",
                        watts_or_missing(average(&slots, solar).map(SolarPower::into_watts)),
                    ),
                ],
                charts: vec![LineChartView::build(
                    LineChartSpec::new(ChartSeries::Solar, POWER_TITLE),
                    &slots,
                    solar,
                    tz,
                )],
            }
        }
        DetailEntity::Home => {
            let home = |a: &IntervalAverages| a.home;
            DetailBodyView {
                stats: vec![
                    stat("Now", watts_or_missing(Some(world.home_usage()))),
                    stat("Peak", peak_string(peak(&slots, home), tz)),
                    stat("Consumed", kwh(energy(&slots, home))),
                    stat("Average", watts_or_missing(average(&slots, home))),
                ],
                charts: vec![LineChartView::build(
                    LineChartSpec::new(ChartSeries::Home, POWER_TITLE),
                    &slots,
                    home,
                    tz,
                )],
            }
        }
        DetailEntity::Grid => {
            let (imported, exported) = (imported(&slots), exported(&slots));
            DetailBodyView {
                stats: vec![
                    stat(
                        "Now",
                        format!(
                            "{} W",
                            format_watts(world.grid.total.importing(), SignStyle::Explicit)
                        ),
                    ),
                    stat("Imported", kwh(imported)),
                    stat("Exported", kwh(exported)),
                    stat("Net", kwh(imported - exported)),
                ],
                charts: vec![LineChartView::build(
                    LineChartSpec::new(ChartSeries::Grid, POWER_TITLE).note("+ import · − export"),
                    &slots,
                    |a| a.grid,
                    tz,
                )],
            }
        }
        DetailEntity::Battery => return None,
    };
    Some(body)
}

#[cfg(test)]
#[path = "detail_tests.rs"]
mod tests;
