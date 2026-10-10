//! The price panel's view model: one local day's prices as an hourly bar
//! chart, tinted by tier, on a y scale shared by every day the snapshot
//! holds so stepping between days never rescales the bars.

use std::time::Duration;

use chrono::NaiveDate;
use chrono_tz::Tz;

use crate::clock::local_date;
use crate::config::DynamicTariff;
use crate::prices::tiers::{Block, Slot, Tier, cheapest_block, priciest_block, tiers};
use crate::prices::{PricePoint, PriceSnapshot};
use crate::units::{CentsPerKwh, Timestamp};

use super::axis::{AxisDensity, AxisPosition, AxisTick};
use super::plot::{Day, YScale};
use super::view::{MISSING, MiniStatSub, MiniStatView, format_time};

/// The chart's viewBox. The height matches `--size-chart-price`, so the bars
/// are drawn at the proportions they are shown at.
pub(super) const PRICE_CHART_WIDTH: f64 = super::plot::CHART_WIDTH;
pub(super) const PRICE_CHART_HEIGHT: f64 = 160.0;

const HOUR: Duration = Duration::from_secs(60 * 60);

/// The share of a slot left empty on each side of its bar.
const BAR_INSET: f64 = 0.14;

/// Keeps a price of about zero visible as a sliver rather than nothing.
const MIN_BAR_HEIGHT: f64 = 1.0;

/// The y axis steps in whole tens of cents.
const SCALE_STEP: CentsPerKwh = CentsPerKwh(10.0);

/// The oldest day the nav steps back to, counted from today.
const HISTORY_DAYS: u64 = 6;

/// What the panel renders from besides the day and the clock.
pub struct PriceContext<'a> {
    pub snapshot: &'a PriceSnapshot,
    /// `[prices.dynamic]`: bars show the all-in import price through it, and
    /// the bare wholesale price without it.
    pub tariff: Option<&'a DynamicTariff>,
    /// Whether `[prices]` is configured at all.
    pub configured: bool,
}

pub enum PricePanelView {
    Empty(EmptyReason),
    Priced(Box<PricedDayView>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmptyReason {
    NotConfigured,
    /// Today has no prices yet.
    Waiting,
    /// Another day the snapshot holds no prices for.
    Unpriced,
}

impl EmptyReason {
    pub fn text(self) -> &'static str {
        match self {
            EmptyReason::NotConfigured => "No prices configured — add [prices] to config.toml",
            EmptyReason::Waiting => "Waiting for prices…",
            EmptyReason::Unpriced => "No prices for this day",
        }
    }
}

pub struct PricedDayView {
    /// `All-in import price incl. VAT · fetched 13:00`.
    pub subtitle: String,
    pub nav: PriceNavView,
    /// What the readout shows with nothing hovered.
    pub readout: PriceReadoutView,
    /// The cheapest and priciest 3-hour blocks; `None` when no full block is
    /// left to show.
    pub windows: Option<[MiniStatView; 2]>,
    pub chart: PriceChartView,
    pub legend: [PriceLegendItem; 3],
}

/// Which day the panel shows, and which days its steps reach.
#[derive(Debug, Clone, PartialEq)]
pub struct PriceNavView {
    pub shown: NaiveDate,
    pub today: NaiveDate,
    pub previous: Option<NaiveDate>,
    /// `None` on tomorrow, and on today until tomorrow is fully priced.
    pub next: Option<NaiveDate>,
    /// "Today", "Tomorrow", "Yesterday" or "Wed 7 Oct".
    pub label: String,
    /// "7 Oct".
    pub date: String,
}

impl PriceNavView {
    fn new(shown: NaiveDate, today: NaiveDate, tomorrow_priced: bool) -> Self {
        let tomorrow = today.succ_opt();
        let earliest = today.checked_sub_days(chrono::Days::new(HISTORY_DAYS));
        let label = if shown == today {
            "Today".to_string()
        } else if Some(shown) == tomorrow {
            "Tomorrow".to_string()
        } else if today.pred_opt() == Some(shown) {
            "Yesterday".to_string()
        } else {
            shown.format("%a %-d %b").to_string()
        };
        PriceNavView {
            shown,
            today,
            previous: shown
                .pred_opt()
                .filter(|day| earliest.is_none_or(|earliest| *day >= earliest)),
            next: shown
                .succ_opt()
                .filter(|day| *day <= today || (Some(*day) == tomorrow && tomorrow_priced)),
            label,
            date: shown.format("%-d %b").to_string(),
        }
    }

    /// Only today has a now, and only today takes the live stream.
    pub fn is_today(&self) -> bool {
        self.shown == self.today
    }

    /// The `data-day` marker the host mirrors: `today` or the date.
    pub fn data_day(&self) -> String {
        if self.is_today() {
            "today".to_string()
        } else {
            self.shown.to_string()
        }
    }
}

/// One readout state, formatted: the default, or a hit's.
#[derive(Debug, Clone, PartialEq)]
pub struct PriceReadoutView {
    /// `Now · 13:00–14:00`, `15:00–16:00` or `Day average`.
    pub label: String,
    /// `13.5`, or `—` for an unpriced current hour.
    pub value: String,
    pub tier: Option<Tier>,
}

/// A rectangle's horizontal extent, in viewBox units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SlotSpan {
    pub x: f64,
    pub width: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PriceBarView {
    pub span: SlotSpan,
    pub y: f64,
    pub height: f64,
    pub tier: Tier,
    pub negative: bool,
    /// An hour of today that is already over.
    pub past: bool,
}

/// A full-height hover target over one priced hour.
#[derive(Debug, Clone, PartialEq)]
pub struct PriceHitView {
    pub span: SlotSpan,
    pub readout: PriceReadoutView,
}

pub struct PriceChartView {
    pub bars: Vec<PriceBarView>,
    pub hits: Vec<PriceHitView>,
    /// One per y-axis tick.
    pub grid_lines: Vec<f64>,
    /// Drawn only when a negative price pulls the scale below zero; otherwise
    /// the bottom grid line is zero.
    pub zero_y: Option<f64>,
    pub now_x: Option<f64>,
    /// Where the highlight sits unhovered: the current hour, today only.
    pub highlight: Option<SlotSpan>,
    pub y_axis: Vec<AxisTick>,
    pub x_axis: Vec<AxisTick>,
}

pub struct PriceLegendItem {
    pub tier: Tier,
    /// `< 8.2`, `8.2–14.0` or `≥ 14.0`.
    pub range: String,
}

/// One local clock hour of the shown day.
struct HourSlot {
    start: Timestamp,
    end: Timestamp,
    price: Option<CentsPerKwh>,
}

/// The price a consumer is shown: the all-in import price under a dynamic
/// contract, the bare wholesale price without one.
fn shown_price(point: &PricePoint, tariff: Option<&DynamicTariff>) -> CentsPerKwh {
    tariff.map_or(point.wholesale, |tariff| {
        tariff.import_price(point.wholesale)
    })
}

/// One decimal with the typographic minus, as every price on the panel reads.
fn format_cents(price: CentsPerKwh) -> String {
    price.to_string().replacen('-', "−", 1)
}

/// Each clock hour of `day` with the mean of the prices starting in it. A
/// coarser point that started earlier still prices the hours it covers.
fn hour_slots(day: &Day, points: &[(PricePoint, CentsPerKwh)]) -> Vec<HourSlot> {
    let day_end = day.end();
    day.slot_starts(HOUR)
        .map(|start| {
            let end = (start + crate::units::Elapsed::of(HOUR)).min(day_end);
            let starting = points
                .iter()
                .filter(|(point, _)| point.from >= start && point.from < end)
                .map(|(_, price)| *price);
            let covering = || {
                points
                    .iter()
                    .find(|(point, _)| point.from <= start && start < point.until)
                    .map(|(_, price)| *price)
            };
            HourSlot {
                start,
                end,
                price: CentsPerKwh::mean(starting).or_else(covering),
            }
        })
        .collect()
}

/// `value` rounded outward to a whole step: up when above zero, down below.
fn round_out(value: CentsPerKwh) -> CentsPerKwh {
    let steps = value.0 / SCALE_STEP.0;
    let rounded = if value.0 >= 0.0 {
        steps.ceil()
    } else {
        steps.floor()
    };
    CentsPerKwh(rounded * SCALE_STEP.0)
}

/// The y scale over every price the snapshot holds: zero up to the highest,
/// rounded up to a whole step, and down past zero only for a negative price.
fn price_scale(points: &[(PricePoint, CentsPerKwh)]) -> (CentsPerKwh, CentsPerKwh) {
    let zero = CentsPerKwh(0.0);
    let (high, low) = points
        .iter()
        .fold((zero, zero), |(hi, lo), (_, p)| (hi.max(*p), lo.min(*p)));
    (round_out(high).max(SCALE_STEP), round_out(low))
}

/// Every step from `bottom` up to `top`, inclusive.
fn scale_ticks(top: CentsPerKwh, bottom: CentsPerKwh) -> Vec<CentsPerKwh> {
    // Half a step of slack, so float drift never drops the top tick.
    let last = top + CentsPerKwh(SCALE_STEP.0 / 2.0);
    std::iter::successors(Some(bottom), |tick| Some(*tick + SCALE_STEP))
        .take_while(|tick| *tick < last)
        .collect()
}

/// A y-axis label: whole cents, with the typographic minus.
fn tick_label(tick: CentsPerKwh) -> String {
    let sign = if tick < CentsPerKwh(0.0) { "−" } else { "" };
    format!("{sign}{:.0}", tick.abs().0)
}

fn hour_range(slot: &HourSlot, tz: Tz) -> String {
    format!(
        "{}–{}",
        format_time(slot.start, tz),
        format_time(slot.end, tz)
    )
}

fn readout(label: String, price: Option<CentsPerKwh>, thresholds: Thresholds) -> PriceReadoutView {
    PriceReadoutView {
        label,
        value: price.map_or_else(|| MISSING.to_string(), format_cents),
        tier: price.map(|price| Tier::of(price, thresholds)),
    }
}

type Thresholds = (CentsPerKwh, CentsPerKwh);

/// `14:00–17:00`; a block ending at midnight reads `24:00` rather than
/// wrapping to the start of the day.
fn block_range(slots: &[HourSlot], block: Block, tz: Tz) -> String {
    let start = slots
        .get(block.start.0)
        .map_or_else(|| MISSING.to_string(), |slot| format_time(slot.start, tz));
    let end = slots
        .get(block.end().0)
        .map_or_else(|| "24:00".to_string(), |slot| format_time(slot.start, tz));
    format!("{start}–{end}")
}

/// The cheapest and priciest blocks starting at or after `from`, or `None`
/// when no full block is left.
fn windows(slots: &[HourSlot], from: Slot, today: bool, tz: Tz) -> Option<[MiniStatView; 2]> {
    let prices: Vec<Option<CentsPerKwh>> = slots.iter().map(|slot| slot.price).collect();
    let cheapest = cheapest_block(&prices, from)?;
    let priciest = priciest_block(&prices, from)?;
    let stat = |label, block: Block, tone| MiniStatView {
        label,
        value: block_range(slots, block, tz),
        sub: Some(MiniStatSub {
            text: format!("{} ct avg", format_cents(block.mean)),
            tone,
        }),
    };
    let (cheap_label, pricey_label) = if today {
        ("Cheapest 3 h ahead", "Priciest 3 h ahead")
    } else {
        ("Cheapest 3 h", "Priciest 3 h")
    };
    Some([
        stat(cheap_label, cheapest, "cheap"),
        stat(pricey_label, priciest, "expensive"),
    ])
}

fn legend((lo, hi): Thresholds) -> [PriceLegendItem; 3] {
    let (lo, hi) = (format_cents(lo), format_cents(hi));
    [
        PriceLegendItem {
            tier: Tier::Cheap,
            range: format!("< {lo}"),
        },
        PriceLegendItem {
            tier: Tier::Normal,
            range: format!("{lo}–{hi}"),
        },
        PriceLegendItem {
            tier: Tier::Expensive,
            range: format!("≥ {hi}"),
        },
    ]
}

fn subtitle(snapshot: &PriceSnapshot, tariff: Option<&DynamicTariff>, tz: Tz) -> String {
    let label = if tariff.is_some() {
        "All-in import price incl. VAT"
    } else {
        "Wholesale price"
    };
    match snapshot.as_of {
        Some(at) => format!("{label} · fetched {}", format_time(at, tz)),
        None => format!("{label} · fetch pending"),
    }
}

/// `day` from the snapshot, for any day the nav can reach. Today has a now:
/// its readout defaults to the current hour, its past hours dim and its
/// windows only look ahead. Any other day defaults to its average.
pub fn price_panel_view(
    day: NaiveDate,
    context: &PriceContext,
    now: Timestamp,
    tz: Tz,
) -> PricePanelView {
    if !context.configured {
        return PricePanelView::Empty(EmptyReason::NotConfigured);
    }
    let today = local_date(now, tz).unwrap_or_default();
    let points: Vec<(PricePoint, CentsPerKwh)> = context
        .snapshot
        .points
        .iter()
        .map(|point| (*point, shown_price(point, context.tariff)))
        .collect();

    let frame = Day::of(day, tz);
    let slots = hour_slots(&frame, &points);
    let priced: Vec<CentsPerKwh> = slots.iter().filter_map(|slot| slot.price).collect();
    let Some(thresholds) = tiers(&priced) else {
        return PricePanelView::Empty(if day == today {
            EmptyReason::Waiting
        } else {
            EmptyReason::Unpriced
        });
    };

    let tomorrow_priced = today.succ_opt().is_some_and(|tomorrow| {
        let slots = hour_slots(&Day::of(tomorrow, tz), &points);
        !slots.is_empty() && slots.iter().all(|slot| slot.price.is_some())
    });
    let nav = PriceNavView::new(day, today, tomorrow_priced);
    let is_today = nav.is_today();

    let current = is_today
        .then(|| {
            slots
                .iter()
                .position(|slot| slot.start <= now && now < slot.end)
        })
        .flatten();

    let default_readout = match current.and_then(|index| slots.get(index)) {
        Some(slot) => readout(
            format!("Now · {}", hour_range(slot, tz)),
            slot.price,
            thresholds,
        ),
        None if is_today => readout("Now".to_string(), None, thresholds),
        None => readout(
            "Day average".to_string(),
            CentsPerKwh::mean(priced.iter().copied()),
            thresholds,
        ),
    };

    let (top, bottom) = price_scale(&points);
    let scale = YScale::new(top.0, bottom.0, PRICE_CHART_HEIGHT);
    let ticks = scale_ticks(top, bottom);
    let span_of = |slot: &HourSlot| {
        let x = frame.x(slot.start);
        SlotSpan {
            x,
            width: frame.x(slot.end) - x,
        }
    };

    let mut bars = Vec::new();
    let mut hits = Vec::new();
    for slot in &slots {
        let Some(price) = slot.price else {
            continue;
        };
        let span = span_of(slot);
        let tier = Tier::of(price, thresholds);
        let (y, height) = scale.bar(price.0, MIN_BAR_HEIGHT);
        bars.push(PriceBarView {
            span: SlotSpan {
                x: span.x + span.width * BAR_INSET,
                width: span.width * (1.0 - 2.0 * BAR_INSET),
            },
            y,
            height,
            tier,
            negative: price < CentsPerKwh(0.0),
            past: is_today && slot.end <= now,
        });
        hits.push(PriceHitView {
            span,
            readout: readout(hour_range(slot, tz), Some(price), thresholds),
        });
    }

    let chart = PriceChartView {
        bars,
        hits,
        grid_lines: ticks.iter().map(|tick| scale.y(tick.0)).collect(),
        zero_y: (bottom < CentsPerKwh(0.0)).then(|| scale.y(0.0)),
        now_x: is_today.then(|| frame.x(now)),
        highlight: current.and_then(|index| slots.get(index)).map(span_of),
        y_axis: ticks
            .iter()
            .map(|tick| {
                AxisTick::new(
                    AxisPosition::new(scale.y(tick.0) / PRICE_CHART_HEIGHT),
                    tick_label(*tick),
                    AxisDensity::Always,
                )
            })
            .collect(),
        x_axis: frame.axis(tz, 6),
    };

    let from = current.map_or(Slot(0), Slot);
    PricePanelView::Priced(Box::new(PricedDayView {
        subtitle: subtitle(context.snapshot, context.tariff, tz),
        nav,
        readout: default_readout,
        windows: windows(&slots, from, is_today, tz),
        chart,
        legend: legend(thresholds),
    }))
}

#[cfg(test)]
#[path = "prices_tests.rs"]
mod tests;
