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
use crate::prices::tiers::{Block, Slot, Thresholds, Tier, cheapest_block, priciest_block, tiers};
use crate::prices::{DayPrices, HourPrice, PRICE_HISTORY_DAYS, PriceSnapshot};
use crate::units::{CentsPerKwh, Timestamp};

use super::axis::{AxisDensity, AxisPosition, AxisTick};
use super::day_nav::{BadQuery, DayNavView, parse_day_param};
use super::plot::{Day, SlotSpan, YScale};
use super::state::DashboardState;
use super::view::{MISSING, MiniStatSub, MiniStatView};

/// The chart's viewBox. The height matches `--size-chart-price`, so the bars
/// are drawn at the proportions they are shown at.
pub(super) const PRICE_CHART_WIDTH: f64 = super::plot::CHART_WIDTH;
pub(super) const PRICE_CHART_HEIGHT: f64 = 160.0;

/// The bars' corner radius, in viewBox units.
pub(super) const PRICE_BAR_RADIUS: f64 = 1.0;

/// The share of a slot left empty on each side of its bar.
const BAR_INSET: f64 = 0.14;

/// Keeps a price of about zero visible as a sliver rather than nothing.
const MIN_BAR_HEIGHT: f64 = 1.0;

/// The y axis steps in whole tens of cents.
const SCALE_STEP: CentsPerKwh = CentsPerKwh(10.0);

/// The day a request asks the panel for, before today is known. `None` is
/// today.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PriceDayQuery {
    day: Option<NaiveDate>,
}

impl PriceDayQuery {
    /// `/fragments/price-panel?day=YYYY-MM-DD`.
    pub fn parse_fragment(raw: Option<&str>) -> Result<Self, BadQuery> {
        Ok(PriceDayQuery {
            day: parse_day_param(raw, "day")?,
        })
    }

    /// `/?price_day=YYYY-MM-DD`: its own key, so the flows panel's `?day=`
    /// can share the page's query.
    pub fn parse_page(raw: Option<&str>) -> Result<Self, BadQuery> {
        Ok(PriceDayQuery {
            day: parse_day_param(raw, "price_day")?,
        })
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
    /// journal, or today before its prices land: the nav stays so the user
    /// can step on.
    Unpriced(DayNavView),
    Priced(Box<PricedDayView>),
}

impl PricePanelView {
    pub fn nav(&self) -> Option<&DayNavView> {
        match self {
            PricePanelView::Empty(_) => None,
            PricePanelView::Unpriced(nav) => Some(nav),
            PricePanelView::Priced(day) => Some(&day.nav),
        }
    }

    pub fn nav_mut(&mut self) -> Option<&mut DayNavView> {
        match self {
            PricePanelView::Empty(_) => None,
            PricePanelView::Unpriced(nav) => Some(nav),
            PricePanelView::Priced(day) => Some(&mut day.nav),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmptyReason {
    NotConfigured,
    /// Nothing is priced yet, today included.
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
    pub nav: DayNavView,
    /// What the readout shows with nothing hovered.
    pub readout: PriceReadoutView,
    /// The cheapest and priciest 3-hour blocks; `None` when no full block is
    /// left to show.
    pub windows: Option<[MiniStatView; 2]>,
    pub chart: PriceChartView,
    pub legend: [PriceLegendItem; 3],
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
    /// The hour's place in the day, which a refetch filling a gap never
    /// moves, unlike the hit's position among the hits.
    pub slot: Slot,
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

/// The price a consumer is shown: the all-in import price under a dynamic
/// contract, the bare wholesale price without one.
fn shown_price(wholesale: CentsPerKwh, tariff: Option<&DynamicTariff>) -> CentsPerKwh {
    tariff.map_or(wholesale, |tariff| tariff.import_price(wholesale))
}

/// One decimal with the typographic minus, as every price on the panel reads.
fn format_cents(price: CentsPerKwh) -> String {
    price.to_string().replacen('-', "−", 1)
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
/// than seeming to end where it began. A range across the change names both
/// offsets, so a 3-hour block over the repeated hour never reads as 2 hours.
fn range_label(start: Timestamp, end: Timestamp, day_end: Timestamp, tz: Tz) -> String {
    let local = |at: Timestamp| tz.timestamp_millis_opt(at.as_millis()).single();
    let (Some(from), Some(to)) = (local(start), local(end)) else {
        return MISSING.to_string();
    };
    let from_repeated = repeated(&from, tz);
    let from_offset = from.offset().fix();
    let to_repeated = end < day_end && repeated(&to, tz);
    let to_text = if end >= day_end {
        "24:00".to_string()
    } else if to_repeated {
        to.with_timezone(&from_offset).format("%H:%M").to_string()
    } else {
        to.format("%H:%M").to_string()
    };
    let crosses = !to_repeated && to.offset().fix() != from_offset;
    let from_text = from.format("%H:%M");
    if crosses {
        format!(
            "{from_text} {}–{to_text} {}",
            from.format("%Z"),
            to.format("%Z")
        )
    } else if from_repeated || to_repeated {
        format!("{from_text}–{to_text} {}", from.format("%Z"))
    } else {
        format!("{from_text}–{to_text}")
    }
}

/// The shown day once it holds a price: what the readout, chart and windows
/// all read.
struct PricedDay<'a> {
    hours: &'a [HourPrice],
    /// The price a consumer is shown for each of `hours`, in the shape
    /// `tiers` and the block search take.
    prices: Vec<Option<CentsPerKwh>>,
    thresholds: Thresholds,
    end: Timestamp,
    /// Today only: when `now` is, and which of `hours` it falls in.
    now: Option<(Timestamp, Option<usize>)>,
    tz: Tz,
}

impl<'a> PricedDay<'a> {
    /// `None` when no hour of `day` is priced.
    fn of(
        day: &'a DayPrices,
        tariff: Option<&DynamicTariff>,
        now: Option<Timestamp>,
        tz: Tz,
    ) -> Option<Self> {
        let hours = day.hours();
        let prices: Vec<Option<CentsPerKwh>> = hours
            .iter()
            .map(|hour| hour.wholesale.map(|price| shown_price(price, tariff)))
            .collect();
        let priced: Vec<CentsPerKwh> = prices.iter().flatten().copied().collect();
        let thresholds = tiers(&priced)?;
        let now = now.map(|now| {
            let current = hours
                .iter()
                .position(|hour| hour.start <= now && now < hour.end);
            (now, current)
        });
        Some(PricedDay {
            hours,
            prices,
            thresholds,
            end: day.end,
            now,
            tz,
        })
    }

    fn is_today(&self) -> bool {
        self.now.is_some()
    }

    fn current(&self) -> Option<usize> {
        self.now.and_then(|(_, current)| current)
    }

    fn label(&self, hour: &HourPrice) -> String {
        range_label(hour.start, hour.end, self.end, self.tz)
    }

    fn readout(&self, label: String, price: Option<CentsPerKwh>) -> PriceReadoutView {
        PriceReadoutView {
            label,
            value: price.map_or_else(|| MISSING.to_string(), format_cents),
            tier: price.map(|price| Tier::of(price, self.thresholds)),
        }
    }

    /// Each priced hour with its slot and shown price.
    fn priced(&self) -> impl Iterator<Item = (Slot, &'a HourPrice, CentsPerKwh)> + '_ {
        self.hours
            .iter()
            .zip(&self.prices)
            .enumerate()
            .filter_map(|(slot, (hour, price))| price.map(|price| (Slot(slot), hour, price)))
    }
}

/// The current hour on today, the day's average on any other.
fn default_readout(day: &PricedDay) -> PriceReadoutView {
    match day.current() {
        Some(index) => day.readout(
            format!("Now · {}", day.label(&day.hours[index])),
            day.prices[index],
        ),
        None if day.is_today() => day.readout("Now".to_string(), None),
        None => day.readout(
            "Day average".to_string(),
            CentsPerKwh::mean(day.priced().map(|(_, _, price)| price)),
        ),
    }
}

fn chart(
    day: &PricedDay,
    frame: &Day,
    snapshot: &PriceSnapshot,
    tariff: Option<&DynamicTariff>,
) -> PriceChartView {
    let (top, bottom) = price_scale(snapshot, tariff);
    let scale = YScale::new(top.0, bottom.0, PRICE_CHART_HEIGHT);
    let ticks = scale_ticks(top, bottom);
    let span_of = |hour: &HourPrice| frame.span(hour.start, hour.end);

    let (bars, hits) = day
        .priced()
        .map(|(slot, hour, price)| {
            let span = span_of(hour);
            let (y, height) = scale.bar(price.0, MIN_BAR_HEIGHT);
            let bar = PriceBarView {
                span: SlotSpan {
                    x: span.x + span.width * BAR_INSET,
                    width: span.width * (1.0 - 2.0 * BAR_INSET),
                },
                y,
                height,
                tier: Tier::of(price, day.thresholds),
                negative: price < CentsPerKwh(0.0),
                past: day.now.is_some_and(|(now, _)| hour.end <= now),
            };
            let hit = PriceHitView {
                slot,
                span,
                readout: day.readout(day.label(hour), Some(price)),
            };
            (bar, hit)
        })
        .unzip();

    PriceChartView {
        bars,
        hits,
        grid_lines: ticks.iter().map(|tick| scale.y(tick.0)).collect(),
        zero_y: (bottom < CentsPerKwh(0.0)).then(|| scale.y(0.0)),
        now_x: day.now.map(|(now, _)| frame.x(now)),
        highlight: day.current().map(|index| span_of(&day.hours[index])),
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
        x_axis: frame.axis(day.tz, 6),
    }
}

fn block_range(day: &PricedDay, block: Block) -> String {
    let first = day.hours.get(block.start.0);
    let last = block
        .end()
        .0
        .checked_sub(1)
        .and_then(|at| day.hours.get(at));
    match (first, last) {
        (Some(first), Some(last)) => range_label(first.start, last.end, day.end, day.tz),
        _ => MISSING.to_string(),
    }
}

/// The cheapest and priciest blocks, from the current hour on today, or
/// `None` when no full block is left.
fn windows(day: &PricedDay) -> Option<[MiniStatView; 2]> {
    let from = Slot(day.current().unwrap_or(0));
    let cheapest = cheapest_block(&day.prices, from)?;
    let priciest = priciest_block(&day.prices, from)?;
    let stat = |label, block: Block, tone| MiniStatView {
        label,
        value: block_range(day, block),
        sub: Some(MiniStatSub {
            text: format!("{} ct avg", format_cents(block.mean)),
            tone,
        }),
    };
    let (cheap_label, pricey_label) = if day.is_today() {
        ("Cheapest 3 h ahead", "Priciest 3 h ahead")
    } else {
        ("Cheapest 3 h", "Priciest 3 h")
    };
    Some([
        stat(cheap_label, cheapest, Tier::Cheap),
        stat(pricey_label, priciest, Tier::Expensive),
    ])
}

fn legend(Thresholds { lo, hi }: Thresholds) -> [PriceLegendItem; 3] {
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

/// The panel on the day `query` asks for.
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
    price_panel_view(query, &context, state.as_of, tz)
}

/// The day `query` asks for, clamped to the days the nav reaches. Today has
/// a now: its readout defaults to the current hour, its past hours dim and
/// its windows only look ahead. Any other day defaults to its average.
pub fn price_panel_view(
    query: PriceDayQuery,
    context: &PriceContext,
    now: Timestamp,
    tz: Tz,
) -> PricePanelView {
    if !context.configured {
        return PricePanelView::Empty(EmptyReason::NotConfigured);
    }
    // A clock no calendar can place has no today to show prices for.
    let Some(today) = local_date(now, tz) else {
        return PricePanelView::Empty(EmptyReason::Waiting);
    };
    let range = NavRange::of(today, context.snapshot, tz);
    let date = query.resolve(today, range);
    let nav = DayNavView::new(date, today, Some(range.earliest), range.latest);
    let prices = context.snapshot.prices_for(date, tz);
    let day_now = nav.is_today().then_some(now);
    let Some(day) = prices
        .as_ref()
        .and_then(|prices| PricedDay::of(prices, context.tariff, day_now, tz))
    else {
        // Today keeps its nav only while some other day holds a price to
        // step to.
        return if nav.is_today() && context.snapshot.points.is_empty() {
            PricePanelView::Empty(EmptyReason::Waiting)
        } else {
            PricePanelView::Unpriced(nav)
        };
    };
    PricePanelView::Priced(Box::new(PricedDayView {
        subtitle: subtitle(context.snapshot, context.tariff, tz),
        nav,
        readout: default_readout(&day),
        windows: windows(&day),
        chart: chart(&day, &Day::of(date, tz), context.snapshot, context.tariff),
        legend: legend(day.thresholds),
    }))
}

#[cfg(test)]
#[path = "prices_tests.rs"]
mod tests;
