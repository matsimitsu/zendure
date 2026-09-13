//! Projects a [`DashboardState`] into plain, already-formatted view-model
//! structs. Newtypes stop here — every Maud template downstream renders
//! strings, never a `Watts` or a `Soc`.

use crate::models::ControlMode;
use crate::units::{KiloWattHours, Percent, Timestamp, Watts};

use super::state::{DashboardState, Plottable, Sparkline};

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
    }
}

#[cfg(test)]
#[path = "view_tests.rs"]
mod tests;
