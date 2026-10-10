//! Integrating a recorded run into daily energy, offline.
//!
//! The journal records power and never energy — `rte.rs` is the only thing in
//! the crate that integrates, and it keeps a rolling 24 h window it never
//! persists. So the questions that decide hardware (how much solar reached the
//! grid while the battery had nowhere to put it; how much demand sat above the
//! inverter's ceiling) have to be re-integrated from the rows every time.
//!
//! Power in, energy out — and, when the caller hands over prices and both
//! tariffs, what that energy would have cost under each contract. No clock, no
//! network, the same hermeticity `replay` promises: prices come from the
//! journal's own rows and the tariffs from the caller, never from here. What a
//! reading *means* lives here; `journal::read` only hands over rows it could
//! decode.

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::{DateTime, Datelike, NaiveDate};

use crate::battery::BatteryState;
use crate::clock::Clock;
use crate::config::{DynamicTariff, FixedTariff};
use crate::event::Event;
use crate::prices::PriceSeries;
use crate::units::{Cents, Cost, GridPower, Soc, Timestamp, WattHours, Watts};
use crate::world::{Measurement, MeterReading};

/// Longer than this between meter readings is a gap, not a measurement.
///
/// The journal drops rows when its write queue is full, deletes them when
/// retention prunes, and a restart leaves a hole the width of the outage.
/// A gap beyond this is an outage, not normal cadence — samples arrive every
/// scan tick — so the interval is skipped and its time goes uncounted instead.
/// A total that is honestly short, with the coverage to say so, beats one that
/// is confidently wrong.
const MAX_SAMPLE_GAP: Duration = Duration::from_secs(30);

/// A meter reading together with the battery state in force when it arrived.
///
/// The battery figure is the last one folded before the meter reading beside
/// it, so it can lag that reading by up to one battery poll. That is fine for
/// energy over a day and wrong for anything that cares about a single ramp.
#[derive(Debug, Clone)]
struct Sample {
    at: Timestamp,
    day: NaiveDate,
    grid: MeterReading,
    /// `None` until the range's first `device_update`. Left absent rather than
    /// assumed zero, which would read as "the battery was idle" and quietly
    /// fold the opening minutes of every range into the wrong column.
    battery: Option<BatteryState>,
}

impl Sample {
    fn import(&self) -> Watts {
        self.grid.total.importing().max(Watts::ZERO)
    }

    fn export(&self) -> Watts {
        self.grid.total.exporting().max(Watts::ZERO)
    }

    fn charging(&self) -> Watts {
        self.battery.as_ref().map_or(Watts::ZERO, |b| {
            (-b.current_power.into_watts()).max(Watts::ZERO)
        })
    }

    fn discharging(&self) -> Watts {
        self.battery.as_ref().map_or(Watts::ZERO, |b| {
            b.current_power.into_watts().max(Watts::ZERO)
        })
    }

    /// What the house would be drawing with the battery standing still — the
    /// same correction `World::underlying_grid` applies, and the only figure
    /// here that a bigger battery would have changed.
    fn underlying(&self) -> Option<GridPower> {
        self.battery
            .as_ref()
            .map(|b| self.grid.total + b.current_power)
    }

    /// Export the battery did not take. Zero while it is charging, so an
    /// interval that starts mid-charge and ends idle gets blended by the
    /// trapezoid rather than counted whole or dropped whole.
    ///
    /// Deliberately "while not charging" rather than "while at max SoC": a full
    /// battery is only one of the reasons surplus goes to the grid, and the
    /// question this answers — how much more could a bigger pack have held —
    /// does not care which reason applied.
    fn unstored_export(&self) -> Watts {
        // An absent battery counts as neither: with no poll yet there is no
        // way to know whether it was taking the surplus, and reading "unknown"
        // as "idle" would inflate the one figure this exists to report.
        if self.battery.is_none() || self.charging() > Watts::ZERO {
            Watts::ZERO
        } else {
            self.export()
        }
    }

    /// Demand above the inverter's own ceiling: what a second *inverter*, as
    /// opposed to a second pack, would have to buy to be worth anything.
    /// Reads the cap the device reported rather than the model's rating, so a
    /// box configured to a different feed-in limit is judged against its own.
    fn above_cap(&self) -> Watts {
        let (Some(battery), Some(underlying)) = (self.battery.as_ref(), self.underlying()) else {
            return Watts::ZERO;
        };
        (underlying.importing() - Watts::from_device(battery.max_discharge_power.get()))
            .max(Watts::ZERO)
    }
}

/// Import and export on one phase.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PhaseTotals {
    pub import: WattHours,
    pub export: WattHours,
}

/// One local day's energy. Every field is integrated from power except
/// `soc_min`/`soc_max`, which are observed.
#[derive(Debug, Clone, PartialEq)]
pub struct DayTotals {
    pub day: NaiveDate,
    /// How much of the day the samples actually span. Anything short of 24 h
    /// means rows were missing or too far apart to integrate across, and every
    /// total below is short by whatever happened in the hole.
    pub covered: Duration,
    pub import: WattHours,
    pub export: WattHours,
    pub unstored_export: WattHours,
    pub charged: WattHours,
    pub discharged: WattHours,
    pub above_cap: WattHours,
    pub soc_min: Option<Soc>,
    pub soc_max: Option<Soc>,
    pub phases: [PhaseTotals; 3],
    /// `None` when the run was not priced at all, so a day with no cost table
    /// cannot be mistaken for one that cost nothing.
    pub costs: Option<DayCosts>,
}

impl DayTotals {
    fn new(day: NaiveDate, pricing: &Pricing) -> Self {
        DayTotals {
            day,
            covered: Duration::ZERO,
            import: WattHours::ZERO,
            export: WattHours::ZERO,
            unstored_export: WattHours::ZERO,
            charged: WattHours::ZERO,
            discharged: WattHours::ZERO,
            above_cap: WattHours::ZERO,
            soc_min: None,
            soc_max: None,
            phases: [PhaseTotals::default(); 3],
            costs: pricing.tariffs().map(|_| DayCosts::default()),
        }
    }

    fn observe_soc(&mut self, soc: Soc) {
        self.soc_min = Some(self.soc_min.map_or(soc, |m| m.min(soc)));
        self.soc_max = Some(self.soc_max.map_or(soc, |m| m.max(soc)));
    }

    /// Coverage as a fraction of the day, capped at 1.0 — a day can report
    /// slightly over 24 h when a boundary interval is attributed to it whole.
    pub fn coverage(&self) -> f64 {
        (self.covered.as_secs_f64() / 86_400.0).min(1.0)
    }
}

/// Both contracts being compared. Only both together make a comparison; one
/// alone would be a price list, which the supplier already publishes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tariffs {
    pub dynamic: DynamicTariff,
    pub fixed: FixedTariff,
}

/// Whether a run gets a cost table, and if not, the one line that says why.
#[derive(Debug, Clone, PartialEq)]
pub enum Pricing {
    Priced {
        series: PriceSeries,
        tariffs: Tariffs,
    },
    Skipped(String),
}

impl Pricing {
    fn tariffs(&self) -> Option<&Tariffs> {
        match self {
            Pricing::Priced { tariffs, .. } => Some(tariffs),
            Pricing::Skipped(_) => None,
        }
    }
}

/// One local day's energy priced under both contracts. Kept as fractional
/// [`Cost`]s and rounded to [`Cents`] only when read, once per day — one
/// interval is worth a sliver of a cent, and rounding each would floor the
/// day to nothing.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct DayCosts {
    /// Integrated time that had a price. Only this time is costed, under
    /// either contract, so the two columns always cover the same energy.
    pub priced: Duration,
    /// Integrated time with no price in force — a feed outage, or a range
    /// older than the backfill reached.
    pub unpriced: Duration,
    pub dynamic: ContractCost<Cost>,
    pub fixed: ContractCost<Cost>,
}

impl DayCosts {
    /// Share of the integrated time that had a price, 0.0 when none was
    /// integrated at all.
    pub fn priced_share(&self) -> f64 {
        share(self.priced, self.priced + self.unpriced)
    }

    pub fn rounded(&self) -> RoundedCosts {
        RoundedCosts {
            dynamic: self.dynamic.map(Cost::total),
            fixed: self.fixed.map(Cost::total),
        }
    }
}

/// What one contract charges for energy taken and credits for energy given
/// back. Generic over the money representation so a day's fractional
/// [`Cost`]s and its settled [`Cents`] share one shape.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ContractCost<T> {
    pub import: T,
    pub export: T,
}

impl<T> ContractCost<T> {
    fn map<U>(self, f: impl Fn(T) -> U) -> ContractCost<U> {
        ContractCost {
            import: f(self.import),
            export: f(self.export),
        }
    }
}

impl ContractCost<Cents> {
    /// Import paid less export credited: what the household is out of pocket.
    pub fn net(&self) -> Cents {
        self.import - self.export
    }
}

impl std::ops::Add for ContractCost<Cents> {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        ContractCost {
            import: self.import + other.import,
            export: self.export + other.export,
        }
    }
}

/// A day's costs as settled money. Summed across days for the period row, so
/// the total is exactly the sum of the rows above it.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct RoundedCosts {
    pub dynamic: ContractCost<Cents>,
    pub fixed: ContractCost<Cents>,
}

impl RoundedCosts {
    /// Negative when the dynamic contract would have been cheaper.
    pub fn delta(&self) -> Cents {
        self.dynamic.net() - self.fixed.net()
    }
}

impl std::ops::Add for RoundedCosts {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        RoundedCosts {
            dynamic: self.dynamic + other.dynamic,
            fixed: self.fixed + other.fixed,
        }
    }
}

/// Fold a range of events into one entry per local day, oldest first.
pub fn daily(events: &[Event], pricing: &Pricing) -> Vec<DayTotals> {
    let mut days: BTreeMap<NaiveDate, DayTotals> = BTreeMap::new();
    let mut battery: Option<BatteryState> = None;
    let mut previous: Option<Sample> = None;

    for event in events {
        match event {
            // Held rather than integrated: a poll says what the battery is
            // doing from now until the next one, and the meter ticks that
            // follow are where that gets turned into energy.
            Event::DeviceUpdate {
                measurement: Measurement::Battery(state),
                ..
            } => battery = Some(state.clone()),

            Event::Meter { at, grid, .. } => {
                let Some(day) = local_date(at) else { continue };
                let sample = Sample {
                    at: at.now,
                    day,
                    grid: *grid,
                    battery: battery.clone(),
                };

                if let Some(previous) = previous.as_ref() {
                    accumulate(&mut days, pricing, previous, &sample);
                }

                // Entered on sight of a reading, not on a successful
                // integration: a day whose intervals were all gaps still
                // happened, and a row saying 0% coverage is the point. SoC goes
                // the same way, since it is observed rather than integrated.
                let totals = days
                    .entry(day)
                    .or_insert_with(|| DayTotals::new(day, pricing));
                if let Some(state) = sample.battery.as_ref() {
                    totals.observe_soc(state.soc);
                }

                previous = Some(sample);
            }

            Event::MqttTimeout { .. } => {}
        }
    }

    days.into_values().collect()
}

/// Integrate one interval into the day its *earlier* end falls in. An interval
/// spanning midnight is attributed whole to the day it started in: that
/// misplaces at most one scan tick of energy, and splitting it would buy precision
/// this is nowhere near accurate enough to carry.
fn accumulate(
    days: &mut BTreeMap<NaiveDate, DayTotals>,
    pricing: &Pricing,
    previous: &Sample,
    current: &Sample,
) {
    let Some(dt) = previous.at.span_within(current.at, MAX_SAMPLE_GAP) else {
        return;
    };

    let day = days
        .entry(previous.day)
        .or_insert_with(|| DayTotals::new(previous.day, pricing));

    let import = WattHours::integrate(previous.import(), current.import(), dt);
    let export = WattHours::integrate(previous.export(), current.export(), dt);

    day.covered += dt;
    day.import = day.import + import;
    day.export = day.export + export;

    // Priced at the interval's start, the same end the day attribution uses: an
    // interval straddling a price boundary is one scan tick at the old price,
    // which is noise against a day's bill.
    if let (Pricing::Priced { series, tariffs }, Some(costs)) = (pricing, day.costs.as_mut()) {
        match series.at(previous.at) {
            Some(point) => {
                costs.priced += dt;
                costs
                    .dynamic
                    .import
                    .add(import, tariffs.dynamic.import_price(point.wholesale));
                costs
                    .dynamic
                    .export
                    .add(export, tariffs.dynamic.export_price(point.wholesale));
                costs.fixed.import.add(import, tariffs.fixed.import);
                costs.fixed.export.add(export, tariffs.fixed.export);
            }
            None => costs.unpriced += dt,
        }
    }
    day.unstored_export = day.unstored_export
        + WattHours::integrate(previous.unstored_export(), current.unstored_export(), dt);
    // Trapezoidal over polled data, so a poll that changes direction is blended
    // across the interval it was seen in rather than attributed whole to either
    // end. `rte.rs` integrates the same telemetry the same way; a second rule
    // here would make two energy figures for one battery disagree.
    day.charged = day.charged + WattHours::integrate(previous.charging(), current.charging(), dt);
    day.discharged =
        day.discharged + WattHours::integrate(previous.discharging(), current.discharging(), dt);
    day.above_cap =
        day.above_cap + WattHours::integrate(previous.above_cap(), current.above_cap(), dt);

    for (phase, totals) in day.phases.iter_mut().enumerate() {
        let (was, is) = (previous.grid.phases[phase], current.grid.phases[phase]);
        totals.import = totals.import
            + WattHours::integrate(
                was.importing().max(Watts::ZERO),
                is.importing().max(Watts::ZERO),
                dt,
            );
        totals.export = totals.export
            + WattHours::integrate(
                was.exporting().max(Watts::ZERO),
                is.exporting().max(Watts::ZERO),
                dt,
            );
    }
}

/// The local calendar date a clock falls in.
///
/// `Clock` records the local day-of-year but not the year, and around New Year
/// the two disagree — 00:30 on 1 January in Amsterdam is still 31 December in
/// UTC, so pairing ordinal 1 with the UTC year is off by one. Of the three
/// years the ordinal could belong to, the real one is whichever lands nearest
/// the UTC date; that needs no timezone, which is good, because `analyze` runs
/// without a configuration as often as with one.
fn local_date(clock: &Clock) -> Option<NaiveDate> {
    let utc = DateTime::from_timestamp_millis(clock.now.as_millis())?.date_naive();
    let year = utc.year();
    [year - 1, year, year + 1]
        .into_iter()
        .filter_map(|year| NaiveDate::from_yo_opt(year, clock.day_ordinal))
        .min_by_key(|date| (*date - utc).num_days().abs())
}

/// The tables, ready to print. kWh throughout — the journal's watts are a
/// detail of how this was measured, not of what it says — and euros for the
/// cost table, which `pricing` either earns or explains the absence of.
pub fn render(days: &[DayTotals], pricing: &Pricing) -> String {
    if days.is_empty() {
        return "no meter readings in that range\n".to_string();
    }

    let mut out = String::new();

    out.push_str(
        "day           cover   import   export  unstored  charged  discharged   >cap      soc\n",
    );
    for day in days {
        out.push_str(&format!(
            "{}   {:>4.0}%  {:>7.2}  {:>7.2}  {:>8.2}  {:>7.2}  {:>10.2}  {:>5.2}  {:>7}\n",
            day.day,
            day.coverage() * 100.0,
            kwh(day.import),
            kwh(day.export),
            kwh(day.unstored_export),
            kwh(day.charged),
            kwh(day.discharged),
            kwh(day.above_cap),
            soc_range(day),
        ));
    }

    out.push_str("\nday            A imp    A exp    B imp    B exp    C imp    C exp\n");
    for day in days {
        out.push_str(&format!("{}", day.day));
        for phase in &day.phases {
            out.push_str(&format!(
                "  {:>7.2}  {:>7.2}",
                kwh(phase.import),
                kwh(phase.export)
            ));
        }
        out.push('\n');
    }

    out.push_str(
        "\nkWh. `unstored` is export while the battery was not charging — what more \
         capacity could have held.\n`>cap` is demand above the inverter's reported \
         discharge limit — what a second inverter would buy.\n",
    );

    match pricing {
        Pricing::Priced { .. } => render_costs(&mut out, days),
        Pricing::Skipped(reason) => out.push_str(&format!("\nno cost table: {reason}\n")),
    }

    let thin: Vec<&DayTotals> = days.iter().filter(|d| d.coverage() < 0.95).collect();
    if !thin.is_empty() {
        let names: Vec<String> = thin.iter().map(|d| d.day.to_string()).collect();
        out.push_str(&format!(
            "\nwarning: under 95% coverage on {} — those totals are short by whatever \
             happened in the gaps.\n",
            names.join(", ")
        ));
    }

    out
}

/// Dynamic against fixed, per day and for the whole period. The period row
/// sums the already-rounded days, so it agrees with the column above it to the
/// cent.
fn render_costs(out: &mut String, days: &[DayTotals]) {
    out.push_str(
        "\nday          priced  dyn.import  dyn.export   dyn.net  fix.import  fix.export   fix.net         Δ\n",
    );

    let mut total = RoundedCosts::default();
    let mut priced = Duration::ZERO;
    let mut integrated = Duration::ZERO;
    for day in days {
        let Some(costs) = day.costs.as_ref() else {
            continue;
        };
        let rounded = costs.rounded();
        out.push_str(&cost_row(
            &day.day.to_string(),
            costs.priced_share(),
            &rounded,
        ));
        total = total + rounded;
        priced += costs.priced;
        integrated += costs.priced + costs.unpriced;
    }
    out.push_str(&cost_row("total     ", share(priced, integrated), &total));

    out.push_str(
        "\n`priced` is the share of measured time with a known price; only that time is \
         costed, under both contracts.\nnet is import paid less export credited. Δ is \
         dyn.net − fix.net: negative means dynamic was cheaper.\nIgnores net metering \
         (salderingsregeling, ends 2027-01-01), so for 2026 this understates the fixed \
         contract.\n",
    );
}

fn cost_row(label: &str, priced: f64, costs: &RoundedCosts) -> String {
    format!(
        "{label}   {:>4.0}%  {:>10}  {:>10}  {:>8}  {:>10}  {:>10}  {:>8}  {:>8}\n",
        priced * 100.0,
        costs.dynamic.import.to_string(),
        costs.dynamic.export.to_string(),
        costs.dynamic.net().to_string(),
        costs.fixed.import.to_string(),
        costs.fixed.export.to_string(),
        costs.fixed.net().to_string(),
        costs.delta().to_string(),
    )
}

/// `part` as a fraction of `whole`, 0.0 rather than NaN when `whole` is empty.
fn share(part: Duration, whole: Duration) -> f64 {
    if whole.is_zero() {
        0.0
    } else {
        part.as_secs_f64() / whole.as_secs_f64()
    }
}

fn kwh(wh: WattHours) -> f64 {
    wh.to_kwh().get()
}

fn soc_range(day: &DayTotals) -> String {
    match (day.soc_min, day.soc_max) {
        (Some(min), Some(max)) => format!("{}-{}%", min.get(), max.get()),
        _ => "—".to_string(),
    }
}

#[cfg(test)]
#[path = "analyze_tests.rs"]
mod tests;
