//! Projects a [`DashboardState`] into plain, already-formatted view-model
//! structs. Newtypes mostly stop here: a template renders strings, never a
//! `Watts`, and a `Soc` only where it also places something by it.

use chrono::NaiveDate;

use crate::config::DynamicTariff;
use crate::device::PackStatus;
use crate::models::ControlMode;
use crate::units::{
    CentsPerKwh, Elapsed, KiloWattHours, Percent, PricePoint, PriceSeries, Soc, SolarForecastPoint,
    SolarPower, Timestamp, Watts,
};

use super::axis::{AxisTick, day_axis};
use super::detail::detail_body;
use super::entity::Entity;
use super::flows::{EnergyFlowsView, energy_flows_view};
use super::line_chart::LineChartView;
use super::soc_bar::SocBarView;
use super::state::{
    ActualSolarHistory, DashboardState, ForecastSnapshot, Plottable, PriceSnapshot,
    SOLAR_BUCKET_MS, SOLAR_BUCKETS_PER_DAY, Sparkline,
};
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
    pub bar_heights: [f64; SOLAR_BUCKETS_PER_DAY],
    /// The actual-production line's SVG path `d`, possibly several `M`/`L`
    /// subpaths where a slot has no recorded sample.
    pub line_path: String,
    pub axis: Vec<AxisTick>,
}

/// The price panel's contents: one bar chart per local day the poller has
/// prices for (today, and tomorrow once published), on one shared scale.
pub struct PricePanelView {
    pub has_data: bool,
    /// When the prices were fetched, or the empty state's text.
    pub as_of: String,
    /// Says whether the bars are the all-in import price or wholesale.
    pub chart_label: &'static str,
    pub current: MiniStatView,
    pub today_min: MiniStatView,
    pub today_max: MiniStatView,
    pub days: Vec<PriceDayView>,
}

pub struct PriceDayView {
    pub label: &'static str,
    pub bars: Vec<PriceBarView>,
    /// The zero line's `y` within the `1000x110` viewBox: bars rise above it
    /// and negative prices hang below it.
    pub zero_y: f64,
    pub axis: Vec<AxisTick>,
}

/// One price interval, in viewBox units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PriceBarView {
    pub x: f64,
    pub width: f64,
    pub y: f64,
    pub height: f64,
    pub negative: bool,
    /// The interval `now` falls in.
    pub current: bool,
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
            axis: day_axis(),
        };
    }

    let today_start = crate::clock::local_midnight(now, timezone);
    let forecast_buckets = bucketed_forecast_watts(&forecast.points, today_start);
    let actual_buckets = actual.averages().map(|mean| mean.map(SolarPower::get));

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
        axis: day_axis(),
    }
}

// --- Price panel: a bar per price interval, today and tomorrow --------------

pub(super) const PRICE_CHART_WIDTH: f64 = 1000.0;
pub(super) const PRICE_CHART_HEIGHT: f64 = 110.0;
const PRICE_CHART_MARGIN: f64 = 4.0;
/// The gap between neighbouring bars, in viewBox units.
const PRICE_BAR_GAP: f64 = 1.0;

/// The price a consumer is shown: the all-in import price under a dynamic
/// contract, the bare wholesale price without one.
fn shown_price(point: &PricePoint, tariff: Option<&DynamicTariff>) -> CentsPerKwh {
    tariff.map_or(point.wholesale, |tariff| {
        tariff.import_price(point.wholesale)
    })
}

/// `23.4 ct/kWh`, with the typographic minus a negative price wears.
pub(super) fn format_price(price: CentsPerKwh) -> String {
    let shown = (price.0 * 10.0).round() / 10.0;
    format!(
        "{}{:.1} ct/kWh",
        SignStyle::Negative.sign(shown < 0.0),
        shown.abs()
    )
}

/// Each local day's intervals with the price shown for them, keyed by the
/// date their interval starts on. Only today and tomorrow are kept.
fn prices_by_day(
    prices: &PriceSeries,
    tariff: Option<&DynamicTariff>,
    today: NaiveDate,
    timezone: chrono_tz::Tz,
) -> [Vec<(PricePoint, CentsPerKwh)>; 2] {
    let tomorrow = today.succ_opt();
    let mut days: [Vec<_>; 2] = [Vec::new(), Vec::new()];
    for point in prices.iter() {
        let date = crate::clock::local_date(point.from, timezone);
        let slot = if date == Some(today) {
            0
        } else if date.is_some() && date == tomorrow {
            1
        } else {
            continue;
        };
        days[slot].push((*point, shown_price(point, tariff)));
    }
    days
}

/// The chart spans one 24-hour wall-clock day, like `day_axis` beneath it.
const PRICE_CHART_DAY: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// How far through its local wall-clock day `at` falls, so a bar lines up
/// with the hour tick it is labelled by. On a DST day the repeated hour's two
/// bars overlap and the skipped hour stays empty: the axis has one slot per
/// clock hour, not per elapsed hour.
fn wall_clock_fraction(at: Timestamp, timezone: chrono_tz::Tz) -> f64 {
    use chrono::Timelike;
    chrono::DateTime::from_timestamp_millis(at.as_millis()).map_or(0.0, |utc| {
        f64::from(utc.with_timezone(&timezone).num_seconds_from_midnight())
            / PRICE_CHART_DAY.as_secs_f64()
    })
}

/// The bars of one local day on a shared scale: `top` is the highest price
/// the chart has to fit (at least zero), `bottom` the lowest (at most zero).
fn price_bars(
    points: &[(PricePoint, CentsPerKwh)],
    (bottom, top): (CentsPerKwh, CentsPerKwh),
    now: Timestamp,
    timezone: chrono_tz::Tz,
) -> (Vec<PriceBarView>, f64) {
    let plot_height = PRICE_CHART_HEIGHT - 2.0 * PRICE_CHART_MARGIN;
    let per_cent = plot_height / (top.0 - bottom.0).max(f64::EPSILON);
    let zero_y = PRICE_CHART_MARGIN + top.0 * per_cent;

    let bars = points
        .iter()
        .map(|(point, price)| {
            let x = wall_clock_fraction(point.from, timezone) * PRICE_CHART_WIDTH;
            let span = (point.until - point.from).as_secs_f64() / PRICE_CHART_DAY.as_secs_f64();
            let end = (x + span * PRICE_CHART_WIDTH).min(PRICE_CHART_WIDTH);
            let height = price.0.abs() * per_cent;
            let negative = price.0 < 0.0;
            PriceBarView {
                x,
                width: (end - x - PRICE_BAR_GAP).max(PRICE_BAR_GAP),
                y: if negative { zero_y } else { zero_y - height },
                height,
                negative,
                current: point.from <= now && now < point.until,
            }
        })
        .collect();
    (bars, zero_y)
}

fn price_panel_view(
    prices: &PriceSnapshot,
    tariff: Option<&DynamicTariff>,
    configured: bool,
    now: Timestamp,
    timezone: chrono_tz::Tz,
) -> PricePanelView {
    let chart_label = if tariff.is_some() {
        "All-in import price incl. VAT, ct/kWh"
    } else {
        "Wholesale price, ct/kWh"
    };
    let days = crate::clock::local_date(now, timezone)
        .map(|today| prices_by_day(&prices.points, tariff, today, timezone));
    let Some(days) = days.filter(|days| days.iter().any(|day| !day.is_empty())) else {
        return PricePanelView {
            has_data: false,
            as_of: if configured {
                "Waiting for prices…"
            } else {
                "No prices configured — add [prices] to config.toml"
            }
            .to_string(),
            chart_label,
            current: MiniStatView {
                label: "Now",
                value: MISSING.to_string(),
            },
            today_min: MiniStatView {
                label: "Today min",
                value: MISSING.to_string(),
            },
            today_max: MiniStatView {
                label: "Today max",
                value: MISSING.to_string(),
            },
            days: Vec::new(),
        };
    };

    let shown = || days.iter().flatten().map(|(_, price)| *price);
    let top = shown().fold(CentsPerKwh(0.0), |a, b| if b > a { b } else { a });
    let bottom = shown().fold(CentsPerKwh(0.0), |a, b| if b < a { b } else { a });
    let today_prices = || days[0].iter().map(|(_, price)| *price);
    let today_min = today_prices().reduce(|a, b| if b < a { b } else { a });
    let today_max = today_prices().reduce(|a, b| if b > a { b } else { a });
    let current = prices
        .points
        .at(now)
        .map(|point| shown_price(point, tariff));
    let stat = |label, price: Option<CentsPerKwh>| MiniStatView {
        label,
        value: price.map_or_else(|| MISSING.to_string(), format_price),
    };

    let day_views = ["Today", "Tomorrow"]
        .into_iter()
        .zip(days.iter())
        .filter(|(_, points)| !points.is_empty())
        .map(|(label, points)| {
            let (bars, zero_y) = price_bars(points, (bottom, top), now, timezone);
            PriceDayView {
                label,
                bars,
                zero_y,
                axis: day_axis(),
            }
        })
        .collect();

    PricePanelView {
        has_data: true,
        as_of: prices
            .as_of
            .map(|at| format!("Prices last fetched {}", format_time(at, timezone)))
            .unwrap_or_else(|| "Price fetch pending".to_string()),
        chart_label,
        current: stat("Now", current),
        today_min: stat("Today min", today_min),
        today_max: stat("Today max", today_max),
        days: day_views,
    }
}

/// Everything the full page and every SSE fragment render from.
pub fn dashboard_view(state: &DashboardState, timezone: chrono_tz::Tz) -> DashboardView {
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
            },
            usable_energy: MiniStatView {
                label: "Usable energy",
                value: format!(
                    "{} of {}",
                    energy_string(state.usable_energy),
                    energy_string(state.usable_max())
                ),
            },
            capacity: MiniStatView {
                label: "Capacity",
                value: energy_string(state.pack_capacity),
            },
            round_trip_efficiency: MiniStatView {
                label: "Round-trip eff.",
                value: efficiency_string(state.rte_percent),
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
        forecast: forecast_panel_view(&state.forecast, &state.actual_solar, state.as_of, timezone),
        prices: price_panel_view(
            &state.prices,
            state.tariff.as_ref(),
            state.price_feed,
            state.as_of,
            timezone,
        ),
        energy_flows: energy_flows_view(&state.intervals, state.as_of, timezone),
    }
}

#[cfg(test)]
#[path = "view_tests.rs"]
mod tests;
