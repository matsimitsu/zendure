//! Integrating a recorded run into daily energy, offline.
//!
//! The journal records power and never energy — `rte.rs` is the only thing in
//! the crate that integrates, and it keeps a rolling 24 h window it never
//! persists. So the questions that decide hardware (how much solar reached the
//! grid while the battery had nowhere to put it; how much demand sat above the
//! inverter's ceiling) have to be re-integrated from the rows every time.
//!
//! Power in, energy out, and nothing else: no configuration, no clock, no
//! network, the same hermeticity `replay` promises. What a reading *means*
//! lives here; `journal::read` only hands over rows it could decode.

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::{DateTime, Datelike, NaiveDate};

use crate::battery::BatteryState;
use crate::clock::Clock;
use crate::event::Event;
use crate::units::{GridPower, Soc, Timestamp, WattHours, Watts};
use crate::world::{Measurement, MeterReading};

/// Longer than this between meter readings is a gap, not a measurement.
///
/// The journal drops rows when its write queue is full, deletes them when
/// retention prunes, and a restart leaves a hole the width of the outage.
/// Integrating a 1 Hz signal across an hour-long hole invents whatever the two
/// endpoints happened to be doing, so the interval is skipped and its time goes
/// uncounted instead — a total that is honestly short, with the coverage to say
/// so, beats one that is confidently wrong.
const MAX_SAMPLE_GAP: Duration = Duration::from_secs(30);

/// A meter reading together with the battery state in force when it arrived.
///
/// The two arrive on different cadences — the meter at ~1 Hz, the battery once
/// per `poll_interval_secs` — so every battery figure here is up to one poll
/// stale. That is fine for energy over a day and wrong for anything that cares
/// about a single ramp.
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
}

impl DayTotals {
    fn new(day: NaiveDate) -> Self {
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

/// Fold a range of events into one entry per local day, oldest first.
pub fn daily(events: &[Event]) -> Vec<DayTotals> {
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
                    accumulate(&mut days, previous, &sample);
                }

                // Entered on sight of a reading, not on a successful
                // integration: a day whose intervals were all gaps still
                // happened, and a row saying 0% coverage is the point. SoC goes
                // the same way, since it is observed rather than integrated.
                let totals = days.entry(day).or_insert_with(|| DayTotals::new(day));
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
/// spanning midnight is attributed whole to the day it started in: at 1 Hz that
/// misplaces at most one second of energy, and splitting it would buy precision
/// this is nowhere near accurate enough to carry.
fn accumulate(days: &mut BTreeMap<NaiveDate, DayTotals>, previous: &Sample, current: &Sample) {
    let Some(dt) = span(previous.at, current.at) else {
        return;
    };

    let day = days
        .entry(previous.day)
        .or_insert_with(|| DayTotals::new(previous.day));

    day.covered += dt;
    day.import = day.import + WattHours::integrate(previous.import(), current.import(), dt);
    day.export = day.export + WattHours::integrate(previous.export(), current.export(), dt);
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

/// The interval between two samples, or `None` when it is a gap or runs
/// backwards. Backwards is possible without anything being corrupt: `ts_ms` is
/// wall clock, so an NTP step can land one reading before the one it followed.
fn span(from: Timestamp, to: Timestamp) -> Option<Duration> {
    let millis = to.as_millis().checked_sub(from.as_millis())?;
    let dt = Duration::from_millis(u64::try_from(millis).ok()?);
    (dt <= MAX_SAMPLE_GAP).then_some(dt)
}

/// The local calendar date a clock falls in.
///
/// `Clock` records the local day-of-year but not the year, and around New Year
/// the two disagree — 00:30 on 1 January in Amsterdam is still 31 December in
/// UTC, so pairing ordinal 1 with the UTC year is off by one. Of the three
/// years the ordinal could belong to, the real one is whichever lands nearest
/// the UTC date; that needs no timezone, which is good, because `analyze` reads
/// no configuration and so has none.
fn local_date(clock: &Clock) -> Option<NaiveDate> {
    let utc = DateTime::from_timestamp_millis(clock.now.as_millis())?.date_naive();
    let year = utc.year();
    [year - 1, year, year + 1]
        .into_iter()
        .filter_map(|year| NaiveDate::from_yo_opt(year, clock.day_ordinal))
        .min_by_key(|date| (*date - utc).num_days().abs())
}

/// Both tables, ready to print. kWh throughout — the journal's watts are a
/// detail of how this was measured, not of what it says.
pub fn render(days: &[DayTotals]) -> String {
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
