//! Projects a [`DashboardState`] into plain, already-formatted view-model
//! structs. Newtypes stop here — every Maud template downstream renders
//! strings, never a `Watts` or a `Soc`.

use crate::models::ControlMode;
use crate::units::{KiloWattHours, Percent, Timestamp};

use super::state::{DashboardState, Sparkline};

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

fn format_watts_signed(watts: f64) -> String {
    format!(
        "{}{}",
        if watts < 0.0 { "-" } else { "" },
        thousands(watts.abs().round() as i64)
    )
}

/// Whole-number thousands grouping — `std::fmt` has none built in, and this
/// crate's low-dependency style prefers ten lines here over a new crate.
fn thousands(n: i64) -> String {
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

fn sparkline_path(spark: &Sparkline) -> String {
    let samples = spark.samples();
    if samples.is_empty() {
        return String::new();
    }
    if samples.len() == 1 {
        return "M0.0,14.0 L96.0,14.0".to_string();
    }

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

fn badge_variant(mode: ControlMode) -> &'static str {
    match mode {
        ControlMode::Charge => "charge",
        ControlMode::Discharge => "discharge",
        ControlMode::Idle | ControlMode::Standby => "idle",
    }
}

fn mode_label(mode: ControlMode) -> &'static str {
    match mode {
        ControlMode::Charge => "CHARGE",
        ControlMode::Discharge => "DISCHARGE",
        ControlMode::Idle => "IDLE",
        ControlMode::Standby => "STANDBY",
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

fn stat_card(
    variant: &'static str,
    glyph: &'static str,
    label: &'static str,
    value: f64,
    unit: &'static str,
    detail: String,
    spark: &Sparkline,
) -> StatCardView {
    StatCardView {
        variant,
        glyph,
        label,
        value: format_watts_signed(value),
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

/// Everything the full page and every SSE fragment render from.
pub fn dashboard_view(state: &DashboardState, timezone: chrono_tz::Tz) -> DashboardView {
    let world = &state.engine.world;

    let solar = stat_card(
        "solar",
        "☀",
        "Solar production",
        world.solar.get(),
        "W",
        "Live solar production".to_string(),
        &state.sparklines.solar,
    );
    let home = stat_card(
        "home",
        "⌂",
        "Home usage",
        world.home_usage().as_f64(),
        "W",
        "Live home usage".to_string(),
        &state.sparklines.home_usage,
    );
    let grid = stat_card(
        "grid",
        "⇄",
        "Grid",
        world.grid.total.get(),
        "W",
        if world.grid.total.get() < 0.0 {
            "Exporting to grid".to_string()
        } else {
            "Importing from grid".to_string()
        },
        &state.sparklines.grid,
    );

    let battery = world.battery().map(|battery| {
        let mode = state
            .last_decision
            .as_ref()
            .map(|d| d.mode)
            .unwrap_or(ControlMode::Idle);
        BatteryPanelView {
            soc_percent: battery.soc.get(),
            mode_label: match mode {
                ControlMode::Charge => "Charging",
                ControlMode::Discharge => "Discharging",
                ControlMode::Idle => "Idle",
                ControlMode::Standby => "Standby",
            },
            badge_variant: badge_variant(mode),
            rate: MiniStatView {
                label: "Rate",
                value: {
                    let watts = battery.current_power.into_watts().get();
                    format!(
                        "{}{} W",
                        if watts >= 0 { "+" } else { "-" },
                        thousands(i64::from(watts.abs()))
                    )
                },
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
        .map(|(at, decision)| DecisionLogRowView {
            time: format_time(*at, timezone),
            mode_label: mode_label(decision.mode),
            badge_variant: badge_variant(decision.mode),
            reason: decision.reason.clone(),
            power: format!("{} W", thousands(decision.power_watts.get() as i64)),
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
