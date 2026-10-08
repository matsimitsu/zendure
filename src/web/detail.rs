//! The entities `GET /detail/{entity}` can describe, and what each one's
//! panel summarises from the interval ring.

use std::str::FromStr;

use chrono_tz::Tz;

use crate::controller::SocLimits;
use crate::device::PackStatus;
use crate::units::{BatteryPower, DeciKelvin, Soc, SolarPower, WattHours, Watts};

use super::intervals::{
    Averaged, IntervalAverages, IntervalIndex, IntervalSlot, Mean, interval_energy,
};
use super::line_chart::{ChartSeries, LineChartSpec, LineChartView};
use super::pack_intervals::{Extent, PackId, PackInterval};
use super::state::{DashboardState, Plottable};
use super::view::{
    DetailBodyView, MISSING, MiniStatView, PackSummaryView, SignStyle, energy_string, format_time,
    format_watts,
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

/// The stats and charts over the rolling 24 hours for `entity`; `None` while
/// there is nothing yet to summarise.
pub fn detail_body(state: &DashboardState, entity: DetailEntity, tz: Tz) -> Option<DetailBodyView> {
    let slots = state.intervals.last_24h(state.as_of);
    let world = &state.engine.world;
    let body = match entity {
        DetailEntity::Solar => {
            let solar = |a: &IntervalAverages| a.solar;
            let solar_watts = |a: &IntervalAverages| a.solar.map(SolarPower::into_watts);
            DetailBodyView {
                packs: Vec::new(),
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
                packs: Vec::new(),
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
                packs: Vec::new(),
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
        DetailEntity::Battery => return battery_body(state, &slots, tz),
    };
    Some(body)
}

/// `None` until a battery has reported, live or anywhere in the window: the
/// panel links here before the first poll lands.
fn battery_body(state: &DashboardState, slots: &[IntervalSlot], tz: Tz) -> Option<DetailBodyView> {
    let soc_now = state.engine.world.battery().map(|battery| battery.soc);
    let packs = pack_summaries(state);
    let has_history = slots
        .iter()
        .any(|slot| slot.averages.soc.is_some() || slot.averages.battery.is_some());
    if soc_now.is_none() && !has_history && packs.is_empty() {
        return None;
    }
    let charging = |a: &IntervalAverages| a.battery.map(BatteryPower::charging);
    let discharging = |a: &IntervalAverages| a.battery.map(BatteryPower::discharging);
    Some(DetailBodyView {
        stats: vec![
            stat(
                "State of charge",
                soc_now.map_or_else(|| MISSING.to_string(), |soc| format!("{soc}%")),
            ),
            stat("Charged", kwh(energy(slots, charging))),
            stat("Discharged", kwh(energy(slots, discharging))),
            stat("24h range", soc_range(soc_extent(slots))),
        ],
        packs,
        charts: vec![
            LineChartView::build(soc_chart(state.soc_limits), slots, |a| a.soc, tz),
            LineChartView::build(
                LineChartSpec::new(ChartSeries::Battery, POWER_TITLE)
                    .note("+ discharge · − charge"),
                slots,
                |a| a.battery,
                tz,
            ),
        ],
    })
}

/// Dashed at the controller's SOC window, with the SOC it will not use
/// shaded. A limit at the scale's edge leaves no band to shade.
fn soc_chart(limits: SocLimits) -> LineChartSpec<Soc> {
    let bands = [(Soc::ZERO, limits.min), (limits.max, Soc::FULL)]
        .into_iter()
        .filter(|(from, to)| from != to)
        .collect();
    LineChartSpec {
        limits: vec![limits.min, limits.max],
        bands,
        ..LineChartSpec::new(ChartSeries::Battery, "State of charge (%)")
            .note(format!("dashed: limits {}% / {}%", limits.min, limits.max))
    }
}

fn soc_extent(slots: &[IntervalSlot]) -> Option<Extent<Soc>> {
    slots.iter().filter_map(|slot| slot.averages.soc).fold(
        None,
        |extent: Option<Extent<Soc>>, soc| {
            Some(extent.map_or(Extent { min: soc, max: soc }, |e| Extent {
                min: e.min.min(soc),
                max: e.max.max(soc),
            }))
        },
    )
}

fn soc_range(extent: Option<Extent<Soc>>) -> String {
    extent.map_or_else(|| MISSING.to_string(), |e| format!("{}–{}%", e.min, e.max))
}

fn temp_range(extent: Option<Extent<DeciKelvin>>) -> String {
    extent.map_or_else(
        || MISSING.to_string(),
        |e| format!("{:.0}–{:.0} °C", e.min.to_celsius().0, e.max.to_celsius().0),
    )
}

/// One row per pack seen in the window, its figures combined over the
/// whole 24 hours.
fn pack_summaries(state: &DashboardState) -> Vec<PackSummaryView> {
    state
        .intervals
        .packs_last_24h(state.as_of)
        .into_iter()
        .enumerate()
        .map(|(row, (key, slots))| {
            let day = PackInterval::combined(slots.into_iter().filter_map(|slot| slot.figures))
                .unwrap_or_default();
            PackSummaryView {
                name: pack_name(&key.pack, &state.packs, row),
                serial: match key.pack {
                    PackId::Serial(serial) => serial,
                    PackId::Position(_) => String::new(),
                },
                soc_range: soc_range(day.soc),
                charged: kwh(day.charged),
                discharged: kwh(day.discharged),
                temp_range: temp_range(day.temp),
            }
        })
        .collect()
}

/// Named as the battery panel names it, from the pack the latest poll
/// reported with the same identity. A pack that has since left the report
/// falls back to its row in the table.
fn pack_name(pack: &PackId, current: &[PackStatus], row: usize) -> String {
    let (position, status) = match pack {
        PackId::Serial(serial) => current
            .iter()
            .enumerate()
            .find(|(_, status)| status.serial.as_ref() == Some(serial))
            .map_or((row, None), |(position, status)| (position, Some(status))),
        PackId::Position(position) => (*position, current.get(*position)),
    };
    status
        .and_then(|status| status.model)
        .map_or_else(|| format!("Pack {}", position + 1), str::to_string)
}

#[cfg(test)]
#[path = "detail_tests.rs"]
mod tests;
