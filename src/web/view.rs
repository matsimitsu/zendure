//! Projects a [`DashboardState`] into plain, already-formatted view-model
//! structs. Newtypes stop here — every Maud template downstream renders
//! strings, never a `Watts` or a `Soc`.

use crate::models::ControlMode;
use crate::units::{Elapsed, KiloWattHours, Percent, SolarForecastPoint, Timestamp, Watts};

use super::state::{
    ActualSolarHistory, DashboardState, ForecastSnapshot, Plottable, SOLAR_BUCKET_MS,
    SOLAR_BUCKETS_PER_DAY, Sparkline,
};

pub struct StatCardView {
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

pub struct MiniStatView {
    pub label: &'static str,
    pub value: String,
}

pub struct BatteryPanelView {
    pub soc_percent: u32,
    pub mode_label: &'static str,
    /// BEM modifier: "charge" | "discharge" | "idle".
    pub badge_variant: &'static str,
    pub rate: MiniStatView,
    pub usable_energy: MiniStatView,
    pub capacity: MiniStatView,
    pub round_trip_efficiency: MiniStatView,
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
    pub bar_heights: [f64; SOLAR_BUCKETS_PER_DAY],
    /// The actual-production line's SVG path `d`, possibly several `M`/`L`
    /// subpaths where a slot has no recorded sample.
    pub line_path: String,
    pub hour_labels: [String; SOLAR_BUCKETS_PER_DAY],
}

pub struct DecisionLogRowView {
    pub time: String,
    pub mode_label: &'static str,
    pub badge_variant: &'static str,
    pub reason: String,
    pub power: String,
}

pub struct TopBarView {
    pub operational: bool,
}

pub struct PageHeaderView {
    pub last_updated: String,
}

pub struct DashboardView {
    pub top_bar: TopBarView,
    pub page_header: PageHeaderView,
    pub stat_cards: [StatCardView; 4],
    pub battery: Option<BatteryPanelView>,
    pub decision_log: Vec<DecisionLogRowView>,
    pub forecast: ForecastPanelView,
}

/// How a formatted watt figure wears its sign.
#[derive(Clone, Copy)]
enum SignStyle {
    /// `-1,234` when negative, `1,234` otherwise.
    Negative,
    /// `-1,234` or `+1,234`, so a flow always reads as a direction.
    Explicit,
    /// `1,234` either way, for a quantity whose direction is stated elsewhere.
    Magnitude,
}

fn format_watts(watts: Watts, style: SignStyle) -> String {
    let negative = watts.get() < 0;
    let sign = match style {
        SignStyle::Negative if negative => "-",
        SignStyle::Explicit if negative => "-",
        SignStyle::Explicit => "+",
        _ => "",
    };
    format!("{sign}{}", thousands(watts.get().unsigned_abs().into()))
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

/// A zero-parameter placeholder: the EV card has no real integration, and a
/// function that cannot read `DashboardState` cannot accidentally regress
/// into pretending it does.
pub fn ev_stat_card_placeholder() -> StatCardView {
    StatCardView {
        variant: "ev",
        glyph: "⛽",
        label: "EV (sample data)",
        value: "42".to_string(),
        unit: "%",
        detail: "Sample data — no vehicle integration".to_string(),
        sparkline_path: String::new(),
    }
}

fn stat_card<T: Plottable>(
    variant: &'static str,
    glyph: &'static str,
    label: &'static str,
    value: Watts,
    unit: &'static str,
    detail: String,
    spark: &Sparkline<T>,
) -> StatCardView {
    StatCardView {
        variant,
        glyph,
        label,
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

fn energy_string(energy: KiloWattHours) -> String {
    format!("{:.1} kWh", energy.get())
}

fn format_time(at: Timestamp, timezone: chrono_tz::Tz) -> String {
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

/// The panel's fixed `1000x110` viewBox geometry — kept as named constants
/// rather than literals scattered through the functions below, since the bar
/// and line builders both have to agree on it.
const FORECAST_CHART_WIDTH: f64 = 1000.0;
const FORECAST_CHART_BASELINE: f64 = 108.0;
const FORECAST_CHART_TOP_MARGIN: f64 = 4.0;

/// Buckets a forecast series into today's 48 local half-hours (Solcast's own
/// resolution, so this is normally a 1:1 mapping — the averaging only
/// matters if two samples ever land in the same slot). A forecast is always
/// forward-looking from the last fetch, so an elapsed slot simply has no
/// bar — nothing here needs to know that on purpose.
fn bucketed_forecast_watts(
    points: &[SolarForecastPoint],
    today_start: Timestamp,
) -> [Option<f64>; SOLAR_BUCKETS_PER_DAY] {
    let mut sum = [0.0_f64; SOLAR_BUCKETS_PER_DAY];
    let mut count = [0u32; SOLAR_BUCKETS_PER_DAY];
    let day_end = today_start + Elapsed::of(std::time::Duration::from_secs(24 * 3600));

    for point in points {
        if point.at < today_start || point.at >= day_end {
            continue;
        }
        let bucket = ((point.at - today_start).as_millis() / SOLAR_BUCKET_MS) as usize;
        if let (Some(s), Some(c)) = (sum.get_mut(bucket), count.get_mut(bucket)) {
            *s += point.estimate.get();
            *c += 1;
        }
    }

    std::array::from_fn(|h| (count[h] > 0).then(|| sum[h] / f64::from(count[h])))
}

/// The actual-production line's `d` attribute: one or more `M`/`L` subpaths,
/// starting a new subpath at every slot with no recorded sample rather than
/// interpolating across the gap — a restart that lost a slot must read as a
/// gap, not a smoothed-over guess.
fn actual_line_path(
    buckets: &[Option<f64>; SOLAR_BUCKETS_PER_DAY],
    scale: f64,
    plot_height: f64,
) -> String {
    let bar_width = FORECAST_CHART_WIDTH / SOLAR_BUCKETS_PER_DAY as f64;
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

/// The top of every hour labelled, the half-hour slot blank — the same
/// density the panel drew when it had one bar per hour.
fn forecast_hour_labels() -> [String; SOLAR_BUCKETS_PER_DAY] {
    std::array::from_fn(|h| {
        if h % 2 == 0 {
            format!("{:02}", h / 2)
        } else {
            String::new()
        }
    })
}

fn forecast_panel_view(
    forecast: &ForecastSnapshot,
    actual: &ActualSolarHistory,
    now: Timestamp,
    timezone: chrono_tz::Tz,
) -> ForecastPanelView {
    if forecast.points.is_empty() {
        return ForecastPanelView {
            has_data: false,
            as_of: "No solar forecast configured — add [prediction] to config.toml".to_string(),
            bar_heights: [0.0; SOLAR_BUCKETS_PER_DAY],
            line_path: String::new(),
            hour_labels: forecast_hour_labels(),
        };
    }

    let today_start = crate::clock::local_midnight(now, timezone);
    let forecast_buckets = bucketed_forecast_watts(&forecast.points, today_start);
    let actual_buckets = actual.averages();

    let scale = forecast_buckets
        .iter()
        .chain(actual_buckets.iter())
        .filter_map(|v| *v)
        .fold(0.0_f64, f64::max)
        .max(1.0);
    let plot_height = FORECAST_CHART_BASELINE - FORECAST_CHART_TOP_MARGIN;

    let bar_heights = forecast_buckets.map(|v| v.map_or(0.0, |w| (w / scale) * plot_height));
    let line_path = actual_line_path(&actual_buckets, scale, plot_height);

    ForecastPanelView {
        has_data: true,
        as_of: forecast
            .as_of
            .map(|at| format!("Forecast last fetched {}", format_time(at, timezone)))
            .unwrap_or_else(|| "Forecast fetch pending".to_string()),
        bar_heights,
        line_path,
        hour_labels: forecast_hour_labels(),
    }
}

/// Everything the full page and every SSE fragment render from.
pub fn dashboard_view(state: &DashboardState, timezone: chrono_tz::Tz) -> DashboardView {
    let world = &state.engine.world;

    let solar = stat_card(
        "solar",
        "☀",
        "Solar production",
        world.solar.into_watts(),
        "W",
        "Live solar production".to_string(),
        &state.sparklines.solar,
    );
    let home = stat_card(
        "home",
        "⌂",
        "Home usage",
        world.home_usage(),
        "W",
        "Live home usage".to_string(),
        &state.sparklines.home_usage,
    );
    let importing = world.grid.total.importing();
    let grid = stat_card(
        "grid",
        "⇄",
        "Grid",
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
        BatteryPanelView {
            soc_percent: battery.soc.get(),
            mode_label,
            badge_variant,
            rate: MiniStatView {
                label: "Rate",
                value: format!(
                    "{} W",
                    format_watts(battery.current_power.into_watts(), SignStyle::Explicit)
                ),
            },
            usable_energy: MiniStatView {
                label: "Usable energy",
                value: energy_string(state.usable_energy),
            },
            capacity: MiniStatView {
                label: "Capacity",
                value: energy_string(state.pack_capacity),
            },
            round_trip_efficiency: MiniStatView {
                label: "Round-trip eff.",
                value: efficiency_string(state.rte_percent),
            },
        }
    });

    let decision_log = state
        .recent_decisions
        .iter()
        .rev()
        .map(|(at, decision)| {
            let badge = badge(decision.mode);
            DecisionLogRowView {
                time: format_log_time(*at, state.as_of, timezone),
                mode_label: badge.log_label,
                badge_variant: badge.variant,
                reason: decision.reason.clone(),
                power: format!(
                    "{} W",
                    format_watts(Watts(decision.power_watts.get()), SignStyle::Magnitude)
                ),
            }
        })
        .collect();

    DashboardView {
        top_bar: TopBarView {
            operational: !state.engine.mqtt_timed_out,
        },
        page_header: PageHeaderView {
            last_updated: format!(
                "As of {} · battery, solar and grid readings update every second.",
                format_time(state.as_of, timezone)
            ),
        },
        stat_cards: [solar, home, grid, ev_stat_card_placeholder()],
        battery,
        decision_log,
        forecast: forecast_panel_view(&state.forecast, &state.actual_solar, state.as_of, timezone),
    }
}

#[cfg(test)]
#[path = "view_tests.rs"]
mod tests;
