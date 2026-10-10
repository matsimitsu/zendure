//! What the dashboard renders from: a snapshot of the engine plus the bits of
//! telemetry the engine itself never carries (RTE, usable energy, pack
//! capacity — see `run.rs`'s `PollTelemetry`), refreshed after every event the
//! coordinator loop folds and broadcast to every connected browser tab.

use std::collections::VecDeque;
use std::path::Path;

use crate::command::Command;
use crate::config::{DynamicTariff, PricesConfig};
use crate::controller::SocLimits;
use crate::device::PackStatus;
use crate::engine::EngineState;
use crate::event::Event;
use crate::journal::read::{ReadError, read_recent_decisions};
use crate::models::ControlDecision;
use crate::prices::PriceSnapshot;
use crate::rte;
use crate::units::{
    BatteryPower, GridPower, KiloWattHours, Percent, Soc, SolarForecastPoint, SolarPower,
    Timestamp, Watts,
};
use crate::world::DeviceId;

use super::intervals::IntervalHistory;

/// How many meter readings each sparkline keeps. Only a meter tick appends
/// one, so at the meter's ~1/s cadence this is ~96 seconds of history —
/// enough to show a recent trend, not a history chart.
const SPARKLINE_CAPACITY: usize = 96;

/// How many rows the decision log shows: seeded from the journal at startup
/// and capped at this size from then on as new decisions arrive.
const DECISION_LOG_CAPACITY: usize = 20;

/// One row of the decision log: a run of consecutive decisions that all
/// commanded the same thing, collapsed into a single entry.
///
/// Named rather than a tuple because `first_at` and `last_at` are adjacent
/// `Timestamp`s and a swap would be invisible (`RUST-2`).
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionLogEntry {
    /// When the run's first decision landed — the start of the span this row
    /// covers.
    pub first_at: Timestamp,
    /// When the run's newest decision landed.
    pub last_at: Timestamp,
    /// How many decisions the run holds. `1` for a row that has not collapsed
    /// anything.
    pub repeats: u32,
    /// The run's newest decision. Its command is the one every decision in the
    /// run shares; its `reason` and `grid_power` are the freshest of them.
    pub decision: ControlDecision,
}

/// A quantity a sparkline can plot: the bare scalar it normalises against.
/// Implemented per role type rather than taken as `f64`, so a buffer of one
/// quantity cannot be fed another (`RUST-2`).
pub(crate) trait Plottable: Copy {
    fn plot_value(self) -> f64;
}

impl Plottable for SolarPower {
    fn plot_value(self) -> f64 {
        self.get()
    }
}

impl Plottable for GridPower {
    fn plot_value(self) -> f64 {
        self.get()
    }
}

impl Plottable for Watts {
    fn plot_value(self) -> f64 {
        self.as_f64()
    }
}

impl Plottable for BatteryPower {
    fn plot_value(self) -> f64 {
        self.as_f64()
    }
}

/// A fixed-capacity ring buffer of recent readings for one stat card's
/// sparkline.
#[derive(Debug, Clone)]
pub struct Sparkline<T> {
    samples: VecDeque<T>,
}

/// Hand-written: the derive would demand `T: Default`, and an empty buffer
/// needs nothing of its element type.
impl<T> Default for Sparkline<T> {
    fn default() -> Self {
        Sparkline {
            samples: VecDeque::new(),
        }
    }
}

impl<T: Plottable> Sparkline<T> {
    pub fn push(&mut self, value: T) {
        if self.samples.len() == SPARKLINE_CAPACITY {
            self.samples.pop_front();
        }
        self.samples.push_back(value);
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// The samples as the scalars a plot normalises against, oldest first.
    pub fn values(&self) -> impl ExactSizeIterator<Item = f64> + '_ {
        self.samples.iter().map(|v| v.plot_value())
    }
}

/// One ring buffer per stat card that has real data, each carrying the
/// quantity its card reads. The EV card's reading (`DashboardState::car_soc`)
/// carries no history — one poll every ~15 minutes is too sparse a
/// sparkline to say anything, unlike the meter-cadence readings below.

#[derive(Debug, Clone, Default)]
pub struct SparklineHistory {
    pub solar: Sparkline<SolarPower>,
    pub home_usage: Sparkline<Watts>,
    pub grid: Sparkline<GridPower>,
}

/// What a device poll produces for the dashboard, which [`EngineState`]
/// never carries: that type is journalled and replayed byte-for-byte, and
/// none of this is a decision input.
///
/// Named rather than a tuple because two of the figures are `KiloWattHours`
/// and adjacent, so a swap would be invisible (`RUST-2`).
#[derive(Debug, Clone, PartialEq)]
pub struct DashboardTelemetry {
    pub rte: Option<Percent>,
    pub usable: KiloWattHours,
    pub capacity: KiloWattHours,
    pub packs: Vec<PackStatus>,
    pub soc_limits: SocLimits,
}

/// The packs one poll reported, and which box reported them when.
#[derive(Debug, Clone, Copy)]
pub struct PolledPacks<'a> {
    pub device: &'a DeviceId,
    pub sampled_at: Timestamp,
    pub packs: &'a [PackStatus],
}

/// The dashboard's view of the forecast poller's cached series — see
/// `crate::prediction`. Empty and `as_of: None` until `[prediction]` is
/// configured and its first fetch lands.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ForecastSnapshot {
    pub points: Vec<SolarForecastPoint>,
    pub as_of: Option<Timestamp>,
}

/// A snapshot of everything the dashboard renders, refreshed after every
/// event `run()` folds and broadcast over a `watch` channel — each SSE
/// connection gets its own `Receiver`, and a `GET /` gets the latest value
/// via `borrow()`.
#[derive(Debug, Clone)]
pub struct DashboardState {
    pub engine: EngineState,
    /// The most recent decision *this process has made* — what the battery
    /// panel's mode badge reflects. `None` until the first one, even when
    /// [`recent_decisions`](Self::recent_decisions) was seeded from the
    /// journal.
    pub last_decision: Option<ControlDecision>,
    /// The decision log, oldest first, capped at [`DECISION_LOG_CAPACITY`].
    /// Seeded from the journal once at startup; every later decision either
    /// collapses into the newest row or pushes onto the end, dropping the
    /// oldest row once full — the same rows a page load and every SSE
    /// fragment render from, so the two can never disagree about what the log
    /// currently shows.
    pub recent_decisions: VecDeque<DecisionLogEntry>,
    pub rte_percent: Option<Percent>,
    pub usable_energy: KiloWattHours,
    pub pack_capacity: KiloWattHours,
    /// Each pack as the last complete report listed it, in the device's order.
    pub packs: Vec<PackStatus>,
    pub soc_limits: SocLimits,
    pub sparklines: SparklineHistory,
    /// Seeded from the journal at startup, then extended by every meter and
    /// device event the loop folds.
    pub intervals: IntervalHistory,
    /// The forecast poller's latest cached series — see `crate::prediction`.
    /// Updated only by `forecast_tick`, on that poller's own schedule.
    pub forecast: ForecastSnapshot,
    /// The price poller's latest series — see `crate::prices`. Updated only
    /// by `prices_tick`, on that poller's own schedule.
    pub prices: PriceSnapshot,
    /// `[prices.dynamic]`, when configured: the price panel shows the all-in
    /// import price through it, and the bare wholesale price without it.
    pub tariff: Option<DynamicTariff>,
    /// Whether `[prices]` is configured, so an empty price panel can tell
    /// "not set up" from "not fetched yet".
    pub price_feed: bool,
    /// Whether `[prediction]` is configured, so an empty forecast panel can
    /// tell "not set up" from "not fetched yet".
    pub forecast_feed: bool,
    /// The car's last-known state of charge and when it was read — see
    /// `crate::car_battery`. `None` until `[car_battery]` is configured and
    /// its first successful poll lands; updated only by `car_soc_tick`, on
    /// that poller's own schedule. A failed poll leaves this as it was, so
    /// its age (not a fallback value) is what tells a viewer it's stale.
    pub car_soc: Option<(Soc, Timestamp)>,
    pub as_of: Timestamp,
}

impl DashboardState {
    /// The channel's initial value: the just-folded startup state, seeded
    /// with whatever decision history the journal already has.
    ///
    /// `history` is journalled rows, bounded by neither session nor age, so
    /// it cannot speak for what the battery is doing now — `last_decision`
    /// stays `None` until this process decides. It arrives uncollapsed and is
    /// folded through [`record_decision`](Self::record_decision), so a page
    /// load and a long-running process show the same runs. `intervals` is
    /// likewise seeded from the journal, so a restart blanks neither the flows
    /// chart nor today's actual solar.
    pub fn seed(
        engine: &EngineState,
        history: Vec<(Timestamp, ControlDecision)>,
        intervals: IntervalHistory,
        as_of: Timestamp,
    ) -> Self {
        let mut state = DashboardState {
            engine: engine.clone(),
            last_decision: None,
            recent_decisions: VecDeque::new(),
            rte_percent: None,
            usable_energy: KiloWattHours::ZERO,
            pack_capacity: KiloWattHours::ZERO,
            packs: Vec::new(),
            soc_limits: SocLimits::default(),
            sparklines: SparklineHistory::default(),
            intervals,
            forecast: ForecastSnapshot::default(),
            prices: PriceSnapshot::default(),
            tariff: None,
            price_feed: false,
            forecast_feed: false,
            car_soc: None,
            as_of,
        };
        for (at, decision) in history {
            state.record_decision(&decision, at);
        }
        state
    }

    /// Seeds the telemetry panel from the startup poll, so the first page
    /// load shows the pack it actually has rather than a confident zero.
    pub fn with_telemetry(mut self, telemetry: DashboardTelemetry) -> Self {
        self.apply_telemetry(telemetry);
        self
    }

    pub fn with_prices(mut self, prices: Option<&PricesConfig>) -> Self {
        self.price_feed = prices.is_some();
        self.tariff = prices.and_then(|prices| prices.dynamic);
        self
    }

    pub fn with_forecast(mut self, configured: bool) -> Self {
        self.forecast_feed = configured;
        self
    }

    /// A meter reading: the only tick that extends the sparklines, which is
    /// what keeps them on the meter's cadence. `limits` arrives on every tick
    /// but a poll's, so a weekday change shows without waiting for a device
    /// to answer.
    pub fn meter_tick(
        &mut self,
        engine: &EngineState,
        event: &Event,
        decision: Option<(&ControlDecision, Timestamp)>,
        as_of: Timestamp,
        limits: SocLimits,
    ) {
        self.intervals.record(event);
        self.soc_limits = limits;
        self.sparklines.solar.push(engine.world.solar);
        self.sparklines.grid.push(engine.world.grid.total);
        self.sparklines.home_usage.push(engine.world.home_usage());
        self.refresh(engine, decision, as_of);
    }

    /// The forecast poller's own tick — not folded through `refresh` like the
    /// other three, since it carries no engine snapshot or decision; it
    /// updates on its own schedule (see `crate::prediction::run_forecast_poller`),
    /// independent of every event the engine folds.
    pub fn forecast_tick(&mut self, forecast: ForecastSnapshot) {
        self.forecast = forecast;
    }

    /// The price poller's own tick — same posture as `forecast_tick` (see
    /// `crate::prices::run_price_poller`).
    pub fn prices_tick(&mut self, prices: PriceSnapshot) {
        self.prices = prices;
    }

    /// The car-battery poller's own tick — same posture as `forecast_tick`:
    /// not folded through `refresh`, updates on its own schedule
    /// (see `crate::car_battery::run_car_battery_poller`), independent of
    /// every event the engine folds.
    pub fn car_soc_tick(&mut self, soc: Soc, at: Timestamp) {
        self.car_soc = Some((soc, at));
    }

    /// A device poll: SOC, RTE and pack figures move here and nowhere else.
    /// `polled` is what this poll itself reported, not the sticky set in
    /// `telemetry`, so a report without packs adds nothing to the history.
    pub fn poll_tick(
        &mut self,
        engine: &EngineState,
        event: &Event,
        polled: Option<PolledPacks<'_>>,
        telemetry: DashboardTelemetry,
        as_of: Timestamp,
    ) {
        self.intervals.record(event);
        if let Some(polled) = polled {
            self.intervals
                .record_packs(polled.device, polled.sampled_at, polled.packs);
        }
        self.apply_telemetry(telemetry);
        self.refresh(engine, None, as_of);
    }

    /// The MQTT failsafe firing, and whatever idle decision it forced.
    /// `limits` arrives here too, so an outage cannot hold a weekday's
    /// limits past midnight.
    pub fn failsafe_tick(
        &mut self,
        engine: &EngineState,
        decision: Option<(&ControlDecision, Timestamp)>,
        as_of: Timestamp,
        limits: SocLimits,
    ) {
        self.soc_limits = limits;
        self.refresh(engine, decision, as_of);
    }

    /// What a full bar is worth: the energy between the limits, discounted
    /// by the same round trip as [`usable_energy`](Self::usable_energy), so a
    /// bar at `max` reads "X of X kWh".
    pub fn usable_max(&self) -> KiloWattHours {
        let window = self.soc_limits.max.fraction_above(self.soc_limits.min);
        self.pack_capacity
            .scale(window * rte::recovered_share(self.rte_percent))
    }

    fn apply_telemetry(&mut self, telemetry: DashboardTelemetry) {
        self.rte_percent = telemetry.rte;
        self.usable_energy = telemetry.usable;
        self.pack_capacity = telemetry.capacity;
        self.packs = telemetry.packs;
        self.soc_limits = telemetry.soc_limits;
    }

    fn refresh(
        &mut self,
        engine: &EngineState,
        decision: Option<(&ControlDecision, Timestamp)>,
        as_of: Timestamp,
    ) {
        self.engine = engine.clone();
        if let Some((decision, at)) = decision {
            self.record_decision(decision, at);
            self.last_decision = Some(decision.clone());
        }
        self.as_of = as_of;
    }

    /// Folds one decision into the log, extending the newest row when it
    /// commands the same thing and pushing a new row otherwise.
    ///
    /// Equality is the [`Command`] — mode and setpoint — and not the whole
    /// decision, because `reason` interpolates the live grid reading and
    /// `grid_power` is that reading: both move nearly every tick, so
    /// whole-struct equality would never fire and a quiet Idle stretch would
    /// still flush every interesting row out of a 20-row log.
    ///
    /// The row keeps the *newest* decision of the run, so its reason and grid
    /// figure describe the moment the log claims to show.
    fn record_decision(&mut self, decision: &ControlDecision, at: Timestamp) {
        if let Some(newest) = self.recent_decisions.back_mut()
            && Command::from(&newest.decision) == Command::from(decision)
        {
            newest.last_at = at;
            newest.repeats += 1;
            newest.decision = decision.clone();
            return;
        }

        if self.recent_decisions.len() == DECISION_LOG_CAPACITY {
            self.recent_decisions.pop_front();
        }
        self.recent_decisions.push_back(DecisionLogEntry {
            first_at: at,
            last_at: at,
            repeats: 1,
            decision: decision.clone(),
        });
    }
}

/// Folds whatever the journal read into a history being seeded. A failed
/// read leaves the history as it was, with a warning: a dashboard short of
/// its history must never become a startup failure.
pub(super) fn seed_from<T>(read: Result<Vec<T>, ReadError>, what: &str, fold: impl FnMut(T)) {
    match read {
        Ok(rows) => rows.into_iter().for_each(fold),
        Err(e) => tracing::warn!("Dashboard: cannot seed {what} from journal: {e}"),
    }
}

/// Seed a fresh channel value's decision log from the journal — called once,
/// at startup, from `run()`.
pub fn seed_decision_log(journal_path: &Path) -> Vec<(Timestamp, ControlDecision)> {
    let mut log = Vec::new();
    seed_from(
        read_recent_decisions(journal_path, DECISION_LOG_CAPACITY),
        "decision log",
        |row| log.push((row.at, row.decision)),
    );
    log
}

pub type DashboardStateSender = tokio::sync::watch::Sender<DashboardState>;
pub type DashboardStateReceiver = tokio::sync::watch::Receiver<DashboardState>;

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;
