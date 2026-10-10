//! The price panel's view model: one local day's prices as an hourly bar
//! chart, tinted by tier, on a y scale shared by every day the snapshot
//! holds so stepping between days never rescales the bars. The day a request
//! asks for is parsed once at the edge (`CONTROL-1`) and clamped to the days
//! the nav reaches.

use chrono::offset::LocalResult;
use chrono::{DateTime, NaiveDate, Offset, TimeZone};
use chrono_tz::Tz;

use crate::clock::local_date;
use crate::config::DynamicTariff;
use crate::prices::tiers::{Block, Slot, Tier, cheapest_block, priciest_block, tiers};
use crate::prices::{DayPrices, PRICE_HISTORY_DAYS, PriceSnapshot};
use crate::units::{CentsPerKwh, Timestamp};

use super::axis::{AxisDensity, AxisPosition, AxisTick};
use super::plot::{Day, YScale};
use super::state::DashboardState;
use super::view::{MISSING, MiniStatSub, MiniStatView};

/// The chart's viewBox. The height matches `--size-chart-price`, so the bars
/// are drawn at the proportions they are shown at.
pub(super) const PRICE_CHART_WIDTH: f64 = super::plot::CHART_WIDTH;
pub(super) const PRICE_CHART_HEIGHT: f64 = 160.0;

/// The share of a slot left empty on each side of its bar.
const BAR_INSET: f64 = 0.14;

/// Keeps a price of about zero visible as a sliver rather than nothing.
const MIN_BAR_HEIGHT: f64 = 1.0;

/// The y axis steps in whole tens of cents.
const SCALE_STEP: CentsPerKwh = CentsPerKwh(10.0);

/// A query string naming a day the panel cannot parse; the caller answers
/// 400.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadPriceQuery(String);

impl std::fmt::Display for BadPriceQuery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The day a request asks the panel for, before today is known. `None` is
/// today.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PriceDayQuery {
    day: Option<NaiveDate>,
}

impl PriceDayQuery {
    /// `/fragments/price-panel?day=YYYY-MM-DD`.
    pub fn parse_fragment(raw: Option<&str>) -> Result<Self, BadPriceQuery> {
        Self::parse(raw, "day")
    }

    /// `/?price_day=YYYY-MM-DD`: its own key, so the flows panel's `?day=`
    /// can share the page's query.
    pub fn parse_page(raw: Option<&str>) -> Result<Self, BadPriceQuery> {
        Self::parse(raw, "price_day")
    }

    /// Other keys are ignored, and an empty value reads as absent.
    fn parse(raw: Option<&str>, key: &str) -> Result<Self, BadPriceQuery> {
        let mut query = PriceDayQuery::default();
        for pair in raw.unwrap_or_default().split('&') {
            let (each, value) = pair.split_once('=').unwrap_or((pair, ""));
            if each != key || value.is_empty() {
                continue;
            }
            let day = NaiveDate::parse_from_str(value, "%Y-%m-%d")
                .map_err(|_| BadPriceQuery(format!("{key}={value} is not a YYYY-MM-DD date")))?;
            query.day = Some(day);
        }
        Ok(query)
    }

    /// The asked-for day pulled into `range`, so a stale or hand-typed link
    /// still lands on a day the panel can show.
    fn resolve(self, today: NaiveDate, range: NavRange) -> NaiveDate {
        self.day
            .map_or(today, |day| day.clamp(range.earliest, range.latest))
    }
}

/// The days the nav reaches: `PRICE_HISTORY_DAYS` back, and forward to
/// tomorrow only once it is fully priced, so an empty tomorrow never shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NavRange {
    earliest: NaiveDate,
    latest: NaiveDate,
}

impl NavRange {
    fn of(today: NaiveDate, snapshot: &PriceSnapshot, tz: Tz) -> Self {
        let back = chrono::Days::new(u64::from(PRICE_HISTORY_DAYS.count()));
        let tomorrow = today
            .succ_opt()
            .filter(|_| snapshot.tomorrow_published(today, tz));
        NavRange {
            earliest: today.checked_sub_days(back).unwrap_or(today),
            latest: tomorrow.unwrap_or(today),
        }
    }
}

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
    /// A day the nav reaches that holds no prices, e.g. a gap in the
    /// journal: the nav stays so the user can step on.
    Unpriced(PriceNavView),
    Priced(Box<PricedDayView>),
}

impl PricePanelView {
    /// The host's `data-day` on first render.
    pub fn data_day(&self) -> String {
        match self {
            PricePanelView::Empty(_) => "today".to_string(),
            PricePanelView::Unpriced(nav) => nav.data_day(),
            PricePanelView::Priced(day) => day.nav.data_day(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmptyReason {
    NotConfigured,
    /// Today has no prices yet.
    Waiting,
}

impl EmptyReason {
    pub fn text(self) -> &'static str {
        match self {
            EmptyReason::NotConfigured => "No prices configured — add [prices] to config.toml",
            EmptyReason::Waiting => "Waiting for prices…",
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
    fn new(shown: NaiveDate, today: NaiveDate, range: NavRange) -> Self {
        let label = if shown == today {
            "Today".to_string()
        } else if today.succ_opt() == Some(shown) {
            "Tomorrow".to_string()
        } else if today.pred_opt() == Some(shown) {
            "Yesterday".to_string()
        } else {
            shown.format("%a %-d %b").to_string()
        };
        PriceNavView {
            shown,
            today,
            previous: shown.pred_opt().filter(|day| *day >= range.earliest),
            next: shown.succ_opt().filter(|day| *day <= range.latest),
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

/// One local clock hour of the shown day, at the price a consumer is shown.
struct HourSlot {
    start: Timestamp,
    end: Timestamp,
    price: Option<CentsPerKwh>,
}

/// The price a consumer is shown: the all-in import price under a dynamic
/// contract, the bare wholesale price without one.
fn shown_price(wholesale: CentsPerKwh, tariff: Option<&DynamicTariff>) -> CentsPerKwh {
    tariff.map_or(wholesale, |tariff| tariff.import_price(wholesale))
}

/// One decimal with the typographic minus, as every price on the panel reads.
fn format_cents(price: CentsPerKwh) -> String {
    price.to_string().replacen('-', "−", 1)
}

fn hour_slots(day: &DayPrices, tariff: Option<&DynamicTariff>) -> Vec<HourSlot> {
    day.hours()
        .zip(day.slots())
        .map(|((start, end), price)| HourSlot {
            start,
            end,
            price: price.map(|price| shown_price(price, tariff)),
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
fn price_scale(
    snapshot: &PriceSnapshot,
    tariff: Option<&DynamicTariff>,
) -> (CentsPerKwh, CentsPerKwh) {
    let zero = CentsPerKwh(0.0);
    let (high, low) = snapshot
        .points
        .iter()
        .map(|point| shown_price(point.wholesale, tariff))
        .fold((zero, zero), |(hi, lo), p| (hi.max(p), lo.min(p)));
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

/// Whether the local clock shows `at`'s time twice that day.
fn repeated(at: &DateTime<Tz>, tz: Tz) -> bool {
    matches!(
        tz.from_local_datetime(&at.naive_local()),
        LocalResult::Ambiguous(..)
    )
}

/// `14:00–17:00`, ending `24:00` at the day's close. Where the clock repeats
/// an hour, the end reads in the start's offset and the offset is named, so
/// the repeated hour reads `02:00–03:00 CEST` then `02:00–03:00 CET` rather
/// than seeming to end where it began.
fn range_label(start: Timestamp, end: Timestamp, day_end: Timestamp, tz: Tz) -> String {
    let local = |at: Timestamp| tz.timestamp_millis_opt(at.as_millis()).single();
    let (Some(from), Some(to)) = (local(start), local(end)) else {
        return MISSING.to_string();
    };
    let from_repeated = repeated(&from, tz);
    let from_offset = from.offset().fix();
    let to_repeated = end < day_end && repeated(&to, tz);
    let (to_text, to_offset) = if end >= day_end {
        ("24:00".to_string(), from_offset)
    } else if to_repeated {
        let to = to.with_timezone(&from_offset);
        (to.format("%H:%M").to_string(), from_offset)
    } else {
        (to.format("%H:%M").to_string(), to.offset().fix())
    };
    let from_text = from.format("%H:%M");
    if !from_repeated && !to_repeated {
        format!("{from_text}–{to_text}")
    } else if from_offset == to_offset {
        format!("{from_text}–{to_text} {}", from.format("%Z"))
    } else {
        // Only a repeated start in one offset ending past the change in the
        // other: the end's own wall time is unambiguous.
        format!("{from_text} {}–{to_text}", from.format("%Z"))
    }
}

fn readout(label: String, price: Option<CentsPerKwh>, thresholds: Thresholds) -> PriceReadoutView {
    PriceReadoutView {
        label,
        value: price.map_or_else(|| MISSING.to_string(), format_cents),
        tier: price.map(|price| Tier::of(price, thresholds)),
    }
}

type Thresholds = (CentsPerKwh, CentsPerKwh);

fn block_range(slots: &[HourSlot], block: Block, day_end: Timestamp, tz: Tz) -> String {
    let first = slots.get(block.start.0);
    let last = block.end().0.checked_sub(1).and_then(|at| slots.get(at));
    match (first, last) {
        (Some(first), Some(last)) => range_label(first.start, last.end, day_end, tz),
        _ => MISSING.to_string(),
    }
}

/// The cheapest and priciest blocks starting at or after `from`, or `None`
/// when no full block is left.
fn windows(
    slots: &[HourSlot],
    from: Slot,
    today: bool,
    day_end: Timestamp,
    tz: Tz,
) -> Option<[MiniStatView; 2]> {
    let prices: Vec<Option<CentsPerKwh>> = slots.iter().map(|slot| slot.price).collect();
    let cheapest = cheapest_block(&prices, from)?;
    let priciest = priciest_block(&prices, from)?;
    let stat = |label, block: Block, tone| MiniStatView {
        label,
        value: block_range(slots, block, day_end, tz),
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
        Some(at) => format!("{label} · fetched {}", super::view::format_time(at, tz)),
        None => format!("{label} · fetch pending"),
    }
}

/// The panel on the day `query` asks for, clamped to the days the nav
/// reaches.
pub fn requested_price_panel(
    state: &DashboardState,
    query: PriceDayQuery,
    tz: Tz,
) -> PricePanelView {
    let context = PriceContext {
        snapshot: &state.prices,
        tariff: state.tariff.as_ref(),
        configured: state.price_feed,
    };
    let today = local_date(state.as_of, tz).unwrap_or_default();
    let day = query.resolve(today, NavRange::of(today, &state.prices, tz));
    price_panel_view(day, &context, state.as_of, tz)
}

/// `day` from the snapshot. Today has a now: its readout defaults to the
/// current hour, its past hours dim and its windows only look ahead. Any
/// other day defaults to its average.
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
    let nav = PriceNavView::new(day, today, NavRange::of(today, context.snapshot, tz));
    let priced_day = context.snapshot.prices_for(day, tz).and_then(|prices| {
        let slots = hour_slots(&prices, context.tariff);
        let priced: Vec<CentsPerKwh> = slots.iter().filter_map(|slot| slot.price).collect();
        tiers(&priced).map(|thresholds| (prices, slots, priced, thresholds))
    });
    let Some((prices, slots, priced, thresholds)) = priced_day else {
        return if nav.is_today() {
            PricePanelView::Empty(EmptyReason::Waiting)
        } else {
            PricePanelView::Unpriced(nav)
        };
    };
    let is_today = nav.is_today();
    let day_end = prices.end;
    let frame = Day::of(day, tz);

    let current = is_today
        .then(|| {
            slots
                .iter()
                .position(|slot| slot.start <= now && now < slot.end)
        })
        .flatten();

    let hour_label = |slot: &HourSlot| range_label(slot.start, slot.end, day_end, tz);
    let default_readout = match current.and_then(|index| slots.get(index)) {
        Some(slot) => readout(
            format!("Now · {}", hour_label(slot)),
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

    let (top, bottom) = price_scale(context.snapshot, context.tariff);
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
            readout: readout(hour_label(slot), Some(price), thresholds),
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
        windows: windows(&slots, from, is_today, day_end, tz),
        nav,
        readout: default_readout,
        chart,
        legend: legend(thresholds),
    }))
}

#[cfg(test)]
#[path = "prices_tests.rs"]
mod tests;
