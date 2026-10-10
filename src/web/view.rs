//! Projects a [`DashboardState`] into plain, already-formatted view-model
//! structs. Newtypes mostly stop here: a template renders strings, never a
//! `Watts`, and a `Soc` only where it also places something by it.

use crate::device::PackStatus;
use crate::models::ControlMode;
use crate::prices::tiers::Tier;
use crate::units::{KiloWattHours, Percent, Soc, SolarPower, Timestamp, Watts};

use super::axis::{AxisTick, day_axis};
use super::detail::detail_body;
use super::entity::Entity;
use super::flows::{EnergyFlowsView, energy_flows_view};
use super::intervals::IntervalHistory;
use super::line_chart::LineChartView;
use super::prices::{PriceDayQuery, PricePanelView, requested_price_panel};
use super::soc_bar::SocBarView;
use super::solar_day::solar_for;
use super::state::{DashboardState, ForecastSnapshot, Plottable, Sparkline};
use crate::controller::SocLimits;

pub struct StatCardView {
    /// Where the card opens its detail; `None` leaves it a plain card.
    pub detail_entity: Option<Entity>,
    /// BEM modifier selecting the semantic color: "solar" | "home" | "grid" | "ev".
    pub variant: &'static str,
    pub glyph: &'static str,
    pub label: &'static str,
    pub value: String,
    pub unit: &'static str,
    pub detail: String,
    /// An SVG `path` `d` attribute, already normalized to a 96x28 viewBox.
    pub sparkline_path: String,
}

/// Everything one `/detail/{entity}` panel renders from.
pub struct DetailView {
    /// What the header names, and how its icon is tinted.
    pub entity: Entity,
    /// `None` while the entity has reported nothing to summarise.
    pub body: Option<DetailBodyView>,
}

/// A row of mini stats over the window, then a chart per quantity. Only the
/// battery has packs; every other entity leaves them empty.
pub struct DetailBodyView {
    pub stats: Vec<MiniStatView>,
    pub packs: Vec<PackSummaryView>,
    pub charts: Vec<LineChartView>,
}

/// One pack over the rolling 24 hours, as the battery detail tabulates it.
pub struct PackSummaryView {
    pub name: String,
    /// Empty where the pack is keyed by position rather than serial.
    pub serial: String,
    pub soc_range: String,
    pub charged: String,
    pub discharged: String,
    pub temp_range: String,
}

pub fn detail_view(state: &DashboardState, entity: Entity, timezone: chrono_tz::Tz) -> DetailView {
    DetailView {
        entity,
        body: detail_body(state, entity, timezone),
    }
}

pub struct MiniStatView {
    pub label: &'static str,
    pub value: String,
    pub sub: Option<MiniStatSub>,
}

/// A third line under a mini stat's value, tinted by what it qualifies.
pub struct MiniStatSub {
    pub text: String,
    /// BEM modifier: "cheap" | "expensive".
    pub tone: Tier,
}

pub struct BatteryPanelView {
    pub soc_percent: u32,
    pub bar: SocBarView,
    pub mode_label: &'static str,
    /// BEM modifier: "charge" | "discharge" | "idle".
    pub badge_variant: &'static str,
    pub rate: MiniStatView,
    pub usable_energy: MiniStatView,
    pub capacity: MiniStatView,
    pub round_trip_efficiency: MiniStatView,
    /// One row per pack, in the device's order; empty until a complete
    /// report has listed them.
    pub packs: Vec<PackRowView>,
}

pub struct PackRowView {
    /// The model, or the pack's position when this build cannot identify it.
    pub name: String,
    pub serial: String,
    /// Compact bar at the pack's SOC (empty when unreported) between the
    /// system's limits.
    pub bar: SocBarView,
    pub soc: String,
    pub power: String,
    pub temperature: String,
    pub capacity: String,
}

/// The forecast panel's contents: a shared-scale bar chart (predicted
/// production, one bar per local half-hour of today — Solcast's own
/// resolution) with the actual measured production drawn over it as a line,
/// for whichever slots have already elapsed.
pub struct ForecastPanelView {
    pub has_data: bool,
    pub as_of: String,
    /// Pixel heights within the panel's `1000x110` viewBox, one per local
    /// half-hour of today; `0.0` where no forecast sample fell in that slot.
    pub bar_heights: Vec<f64>,
    /// The actual-production line's SVG path `d`, possibly several `M`/`L`
    /// subpaths where a slot has no recorded sample.
    pub line_path: String,
    pub axis: Vec<AxisTick>,
}

pub struct DecisionLogRowView {
    pub time: String,
    pub mode_label: &'static str,
    pub badge_variant: &'static str,
    pub reason: String,
    pub power: String,
    /// Present only when this row collapses a run of identical commands —
    /// otherwise the row is one decision and there is nothing to mark.
    pub repeat: Option<DecisionRepeatView>,
}

/// The marker a collapsed run wears, so time passing under an unchanged
/// command is visible rather than silently hidden.
pub struct DecisionRepeatView {
    /// What the row shows: `×12`.
    pub label: String,
    /// The `title` behind it: how many decisions, and when the run started.
    pub span: String,
}

pub struct TopBarView {
    pub operational: bool,
}

pub struct PageHeaderView {
    pub last_updated: String,
}

/// The stat row, by name rather than position: each card is its own live
/// fragment, so the stream has to reach one card without counting to it.
pub struct StatCardsView {
    pub solar: StatCardView,
    pub home: StatCardView,
    pub grid: StatCardView,
    pub ev: StatCardView,
}

pub struct DashboardView {
    pub top_bar: TopBarView,
    pub page_header: PageHeaderView,
    pub stat_cards: StatCardsView,
    pub battery: Option<BatteryPanelView>,
    pub decision_log: Vec<DecisionLogRowView>,
    pub forecast: ForecastPanelView,
    pub prices: PricePanelView,
    pub energy_flows: EnergyFlowsView,
}

/// How a formatted power figure wears its sign. The minus is always the
/// typographic `−`, as the design sets it.
#[derive(Clone, Copy)]
pub(super) enum SignStyle {
    /// `−1,234` when negative, `1,234` otherwise.
    Negative,
    /// `−1,234` or `+1,234`, so a flow always reads as a direction.
    Explicit,
    /// `1,234` either way, for a quantity whose direction is stated elsewhere.
    Magnitude,
}

impl SignStyle {
    fn sign(self, negative: bool) -> &'static str {
        match self {
            SignStyle::Negative | SignStyle::Explicit if negative => "−",
            SignStyle::Explicit => "+",
            _ => "",
        }
    }
}

pub(super) fn format_watts(watts: Watts, style: SignStyle) -> String {
    format!(
        "{}{}",
        style.sign(watts < Watts::ZERO),
        thousands(watts.get().unsigned_abs().into())
    )
}

/// Kilowatts to `places` decimals. The sign follows the figure as shown, so
/// a flow that rounds to zero reads `+0.00` rather than `−0.00`.
pub(super) fn format_kw(watts: Watts, places: u8, style: SignStyle) -> String {
    let scale = 10_f64.powi(i32::from(places));
    let kw = (watts.as_f64() / 1000.0 * scale).round() / scale;
    format!(
        "{}{:.places$}",
        style.sign(kw < 0.0),
        kw.abs(),
        places = usize::from(places)
    )
}

/// Whole-number thousands grouping, which `std::fmt` has none of built in.
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn sparkline_path<T: Plottable>(spark: &Sparkline<T>) -> String {
    if spark.is_empty() {
        return String::new();
    }
    if spark.len() == 1 {
        return "M0.0,14.0 L96.0,14.0".to_string();
    }

    let samples: Vec<f64> = spark.values().collect();
    let max = samples.iter().cloned().fold(f64::MIN, f64::max);
    let min = samples.iter().cloned().fold(f64::MAX, f64::min);
    let range = if (max - min).abs() < f64::EPSILON {
        1.0
    } else {
        max - min
    };

    let w = 96.0;
    let h = 28.0;
    let last = samples.len() - 1;
    samples
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let x = (i as f64 / last as f64) * w;
            let y = h - ((v - min) / range) * (h - 4.0) - 2.0;
            format!("{}{x:.1},{y:.1}", if i == 0 { "M" } else { "L" })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

struct ModeBadge {
    /// BEM modifier: "charge" | "discharge" | "idle".
    variant: &'static str,
    /// The decision log's label.
    log_label: &'static str,
    /// The battery panel's label.
    panel_label: &'static str,
}

fn badge(mode: ControlMode) -> ModeBadge {
    let (variant, log_label, panel_label) = match mode {
        ControlMode::Charge => ("charge", "CHARGE", "Charging"),
        ControlMode::Discharge => ("discharge", "DISCHARGE", "Discharging"),
        ControlMode::Idle => ("idle", "IDLE", "Idle"),
        ControlMode::Standby => ("idle", "STANDBY", "Standby"),
    };
    ModeBadge {
        variant,
        log_label,
        panel_label,
    }
}

/// The battery panel's badge as `(label, variant)`. "Idle" would assert one of
/// the three states this badge exists to distinguish, so no decision yet has to
/// render as its own thing.
fn panel_badge(mode: Option<ControlMode>) -> (&'static str, &'static str) {
    match mode {
        None => ("Awaiting decision", "idle"),
        Some(mode) => {
            let badge = badge(mode);
            (badge.panel_label, badge.variant)
        }
    }
}

/// The EV card: the car's last-known state of charge (`crate::car_battery`),
/// or the placeholder text `[car_battery]` not being configured, or no
/// successful poll yet, look identical from here — both are `car_soc: None`.
/// No sparkline: one poll every ~15 minutes is too sparse to plot, per
/// `SparklineHistory`'s own doc comment.
fn ev_stat_card_view(
    car_soc: Option<(Soc, Timestamp)>,
    now: Timestamp,
    timezone: chrono_tz::Tz,
) -> StatCardView {
    let (value, detail) = match car_soc {
        Some((soc, at)) => (
            soc.to_string(),
            format!("Updated {}", format_log_time(at, now, timezone)),
        ),
        None => ("--".to_string(), "No vehicle configured".to_string()),
    };
    StatCardView {
        detail_entity: None,
        variant: "ev",
        glyph: "⛽",
        label: "Car Battery",
        value,
        unit: "%",
        detail,
        sparkline_path: String::new(),
    }
}

fn stat_card<T: Plottable>(
    entity: Entity,
    value: Watts,
    unit: &'static str,
    detail: String,
    spark: &Sparkline<T>,
) -> StatCardView {
    StatCardView {
        detail_entity: Some(entity),
        variant: entity.slug(),
        glyph: entity.glyph(),
        label: entity.title(),
        value: format_watts(value, SignStyle::Negative),
        unit,
        detail,
        sparkline_path: sparkline_path(spark),
    }
}

fn efficiency_string(rte: Option<Percent>) -> String {
    match rte {
        Some(rte) => format!("{:.0}%", rte.get()),
        None => "—".to_string(),
    }
}

pub(super) const MISSING: &str = "—";

fn pack_row(index: usize, pack: &PackStatus, limits: SocLimits) -> PackRowView {
    PackRowView {
        name: pack
            .model
            .map_or_else(|| format!("Pack {}", index + 1), str::to_string),
        serial: pack.serial.clone().unwrap_or_default(),
        bar: SocBarView::compact(pack.soc.unwrap_or(Soc::new(0)), limits),
        soc: pack
            .soc
            .map_or_else(|| MISSING.to_string(), |soc| format!("{soc}%")),
        power: pack.power.map_or_else(
            || MISSING.to_string(),
            |power| {
                format!(
                    "{} W",
                    format_watts(power.into_watts(), SignStyle::Explicit)
                )
            },
        ),
        temperature: pack.temp.map_or_else(
            || MISSING.to_string(),
            |temp| format!("{:.1} °C", temp.to_celsius().0),
        ),
        capacity: energy_string(pack.capacity.to_kwh()),
    }
}

/// To one decimal, signed like [`format_kw`]: by the figure as shown, so a
/// net that rounds to zero reads `0.0 kWh`, never `-0.0 kWh`.
pub(super) fn energy_string(energy: KiloWattHours) -> String {
    let shown = (energy.get() * 10.0).round() / 10.0;
    format!(
        "{}{:.1} kWh",
        SignStyle::Negative.sign(shown < 0.0),
        shown.abs()
    )
}

pub(super) fn format_time(at: Timestamp, timezone: chrono_tz::Tz) -> String {
    use chrono::TimeZone;
    timezone
        .timestamp_millis_opt(at.as_millis())
        .single()
        .map(|dt| dt.format("%H:%M").to_string())
        .unwrap_or_else(|| "--:--".to_string())
}

/// A decision log row's timestamp, dated once it is no longer today's. The
/// log is seeded from a journal bounded by neither session nor age, so a row
/// can be days old.
fn format_log_time(at: Timestamp, now: Timestamp, timezone: chrono_tz::Tz) -> String {
    use chrono::TimeZone;
    let (Some(at), Some(now)) = (
        timezone.timestamp_millis_opt(at.as_millis()).single(),
        timezone.timestamp_millis_opt(now.as_millis()).single(),
    ) else {
        return "--:--".to_string();
    };

    if at.date_naive() == now.date_naive() {
        at.format("%H:%M").to_string()
    } else {
        at.format("%-d %b %H:%M").to_string()
    }
}

// --- Forecast panel: a shared-scale bar+line chart --------------------------

/// The panel's viewBox geometry, shared so the bar and line builders agree.
pub(super) const FORECAST_CHART_WIDTH: f64 = 1000.0;
pub(super) const FORECAST_CHART_BASELINE: f64 = 108.0;
const FORECAST_CHART_TOP_MARGIN: f64 = 4.0;

/// The actual-production line's `d` attribute: one or more `M`/`L` subpaths,
/// starting a new subpath at every slot with no recorded sample rather than
/// interpolating across the gap — a restart that lost a slot must read as a
/// gap, not a smoothed-over guess.
fn actual_line_path(buckets: &[Option<f64>], scale: f64, plot_height: f64) -> String {
    let bar_width = FORECAST_CHART_WIDTH / buckets.len() as f64;
    let mut path = String::new();
    let mut drawing = false;

    for (h, value) in buckets.iter().enumerate() {
        let Some(watts) = value else {
            drawing = false;
            continue;
        };
        let x = (h as f64 + 0.5) * bar_width;
        let y = FORECAST_CHART_BASELINE - (watts / scale) * plot_height;
        if drawing {
            path.push_str(&format!(" L{x:.1},{y:.1}"));
        } else {
            if !path.is_empty() {
                path.push(' ');
            }
            path.push_str(&format!("M{x:.1},{y:.1}"));
            drawing = true;
        }
    }
    path
}

fn forecast_panel_view(
    forecast: &ForecastSnapshot,
    intervals: &IntervalHistory,
    now: Timestamp,
    timezone: chrono_tz::Tz,
) -> ForecastPanelView {
    let today = crate::clock::local_date(now, timezone)
        .and_then(|today| solar_for(today, forecast, intervals, timezone, now));
    let Some(today) = today.filter(|_| !forecast.points.is_empty()) else {
        return ForecastPanelView {
            has_data: false,
            as_of: "No solar forecast configured — add [prediction] to config.toml".to_string(),
            bar_heights: Vec::new(),
            line_path: String::new(),
            axis: day_axis(),
        };
    };

    let watts = |power: Option<SolarPower>| power.map(SolarPower::get);
    let forecast_buckets: Vec<Option<f64>> = today.forecast().map(watts).collect();
    let actual_buckets: Vec<Option<f64>> = today.actual().map(watts).collect();

    let scale = forecast_buckets
        .iter()
        .chain(actual_buckets.iter())
        .filter_map(|v| *v)
        .fold(0.0_f64, f64::max)
        .max(1.0);
    let plot_height = FORECAST_CHART_BASELINE - FORECAST_CHART_TOP_MARGIN;

    let bar_heights = forecast_buckets
        .iter()
        .map(|v| v.map_or(0.0, |w| (w / scale) * plot_height))
        .collect();
    let line_path = actual_line_path(&actual_buckets, scale, plot_height);

    ForecastPanelView {
        has_data: true,
        as_of: forecast
            .as_of
            .map(|at| format!("Forecast last fetched {}", format_time(at, timezone)))
            .unwrap_or_else(|| "Forecast fetch pending".to_string()),
        bar_heights,
        line_path,
        axis: day_axis(),
    }
}

/// Everything the full page and every SSE fragment render from.
pub fn dashboard_view(
    state: &DashboardState,
    price_day: PriceDayQuery,
    timezone: chrono_tz::Tz,
) -> DashboardView {
    let world = &state.engine.world;

    let solar = stat_card(
        Entity::Solar,
        world.solar.into_watts(),
        "W",
        "Live solar production".to_string(),
        &state.sparklines.solar,
    );
    let home = stat_card(
        Entity::Home,
        world.home_usage(),
        "W",
        "Live home usage".to_string(),
        &state.sparklines.home_usage,
    );
    let importing = world.grid.total.importing();
    let grid = stat_card(
        Entity::Grid,
        importing,
        "W",
        if importing < Watts::ZERO {
            "Exporting to grid".to_string()
        } else {
            "Importing from grid".to_string()
        },
        &state.sparklines.grid,
    );

    let battery = world.battery().map(|battery| {
        let (mode_label, badge_variant) = panel_badge(state.last_decision.as_ref().map(|d| d.mode));
        let limits = state.soc_limits;
        BatteryPanelView {
            bar: SocBarView::labelled(battery.soc, limits),
            soc_percent: battery.soc.get(),
            mode_label,
            badge_variant,
            rate: MiniStatView {
                label: "Rate",
                value: format!(
                    "{} W",
                    format_watts(battery.current_power.into_watts(), SignStyle::Explicit)
                ),
                sub: None,
            },
            usable_energy: MiniStatView {
                label: "Usable energy",
                value: format!(
                    "{} of {}",
                    energy_string(state.usable_energy),
                    energy_string(state.usable_max())
                ),
                sub: None,
            },
            capacity: MiniStatView {
                label: "Capacity",
                value: energy_string(state.pack_capacity),
                sub: None,
            },
            round_trip_efficiency: MiniStatView {
                label: "Round-trip eff.",
                value: efficiency_string(state.rte_percent),
                sub: None,
            },
            packs: state
                .packs
                .iter()
                .enumerate()
                .map(|(index, pack)| pack_row(index, pack, limits))
                .collect(),
        }
    });

    let decision_log = state
        .recent_decisions
        .iter()
        .rev()
        .map(|entry| {
            let badge = badge(entry.decision.mode);
            DecisionLogRowView {
                time: format_log_time(entry.last_at, state.as_of, timezone),
                mode_label: badge.log_label,
                badge_variant: badge.variant,
                reason: entry.decision.reason.clone(),
                power: format!(
                    "{} W",
                    format_watts(
                        Watts(entry.decision.power_watts.get()),
                        SignStyle::Magnitude
                    )
                ),
                repeat: (entry.repeats > 1).then(|| DecisionRepeatView {
                    label: format!("×{}", entry.repeats),
                    span: format!(
                        "{} identical decisions since {}",
                        entry.repeats,
                        format_log_time(entry.first_at, state.as_of, timezone)
                    ),
                }),
            }
        })
        .collect();

    DashboardView {
        top_bar: TopBarView {
            operational: !state.engine.mqtt_timed_out,
        },
        page_header: PageHeaderView {
            last_updated: format!(
                "As of {} · battery, solar and grid readings update every scan tick.",
                format_time(state.as_of, timezone)
            ),
        },
        stat_cards: StatCardsView {
            solar,
            home,
            grid,
            ev: ev_stat_card_view(state.car_soc, state.as_of, timezone),
        },
        battery,
        decision_log,
        forecast: forecast_panel_view(&state.forecast, &state.intervals, state.as_of, timezone),
        prices: requested_price_panel(state, price_day, timezone),
        energy_flows: energy_flows_view(&state.intervals, state.as_of, timezone),
    }
}

#[cfg(test)]
#[path = "view_tests.rs"]
mod tests;
