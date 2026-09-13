//! What the dashboard renders from: a snapshot of the engine plus the bits of
//! telemetry the engine itself never carries (RTE, usable energy, pack
//! capacity — see `run.rs`'s `PollTelemetry`), refreshed after every event the
//! coordinator loop folds and broadcast to every connected browser tab.

use std::collections::VecDeque;

use crate::engine::EngineState;
use crate::journal::read::read_recent_decisions;
use crate::models::ControlDecision;
use crate::units::{GridPower, KiloWattHours, Percent, SolarPower, Timestamp, Watts};

/// How many meter readings each sparkline keeps. Only a meter tick appends
/// one, so at the meter's ~1/s cadence this is ~96 seconds of history —
/// enough to show a recent trend, not a history chart.
const SPARKLINE_CAPACITY: usize = 96;

/// How many rows the decision log shows: seeded from the journal at startup
/// and capped at this size from then on as new decisions arrive.
pub const DECISION_LOG_CAPACITY: usize = 20;

/// A quantity a sparkline can plot: the bare scalar it normalises against.
/// Implemented per role type rather than taken as `f64`, so a buffer of one
/// quantity cannot be fed another (`RUST-2`).
pub trait Plottable: Copy {
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
/// quantity its card reads. EV has no real source and carries no history.

#[derive(Debug, Clone, Default)]
pub struct SparklineHistory {
    pub solar: Sparkline<SolarPower>,
    pub home_usage: Sparkline<Watts>,
    pub grid: Sparkline<GridPower>,
}

/// The three figures a device poll produces for the dashboard, which
/// [`EngineState`] never carries: that type is journalled and replayed
/// byte-for-byte, and none of this is a decision input.
///
/// Named rather than a tuple because two of the three are `KiloWattHours`
/// and adjacent, so a swap would be invisible (`RUST-2`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DashboardTelemetry {
    pub rte: Option<Percent>,
    pub usable: KiloWattHours,
    pub capacity: KiloWattHours,
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
    /// Seeded from the journal once at startup; every later decision pushes
    /// onto the end and drops the oldest row once full — the same rows a
    /// page load and every SSE fragment render from, so the two can never
    /// disagree about what the log currently shows.
    pub recent_decisions: VecDeque<(Timestamp, ControlDecision)>,
    pub rte_percent: Option<Percent>,
    pub usable_energy: KiloWattHours,
    pub pack_capacity: KiloWattHours,
    pub sparklines: SparklineHistory,
    pub as_of: Timestamp,
}

impl DashboardState {
    /// The channel's initial value: the just-folded startup state, seeded
    /// with whatever decision history the journal already has.
    ///
    /// `history` is journalled rows, bounded by neither session nor age, so
    /// it cannot speak for what the battery is doing now — `last_decision`
    /// stays `None` until this process decides.
    pub fn seed(
        engine: &EngineState,
        history: Vec<(Timestamp, ControlDecision)>,
        as_of: Timestamp,
    ) -> Self {
        DashboardState {
            engine: engine.clone(),
            last_decision: None,
            recent_decisions: history.into(),
            rte_percent: None,
            usable_energy: KiloWattHours::ZERO,
            pack_capacity: KiloWattHours::ZERO,
            sparklines: SparklineHistory::default(),
            as_of,
        }
    }

    /// Seeds the telemetry panel from the startup poll, so the first page
    /// load shows the pack it actually has rather than a confident zero.
    pub fn with_telemetry(mut self, telemetry: DashboardTelemetry) -> Self {
        self.apply_telemetry(telemetry);
        self
    }

    /// A meter reading: the only tick that extends the sparklines, which is
    /// what keeps them on the meter's cadence.
    pub fn meter_tick(
        &mut self,
        engine: &EngineState,
        decision: Option<(&ControlDecision, Timestamp)>,
        as_of: Timestamp,
    ) {
        self.sparklines.solar.push(engine.world.solar);
        self.sparklines.grid.push(engine.world.grid.total);
        self.sparklines.home_usage.push(engine.world.home_usage());
        self.refresh(engine, decision, as_of);
    }

    /// A device poll: SOC, RTE and pack figures move here and nowhere else.
    pub fn poll_tick(
        &mut self,
        engine: &EngineState,
        telemetry: DashboardTelemetry,
        as_of: Timestamp,
    ) {
        self.apply_telemetry(telemetry);
        self.refresh(engine, None, as_of);
    }

    /// The MQTT failsafe firing, and whatever idle decision it forced.
    pub fn failsafe_tick(
        &mut self,
        engine: &EngineState,
        decision: Option<(&ControlDecision, Timestamp)>,
        as_of: Timestamp,
    ) {
        self.refresh(engine, decision, as_of);
    }

    fn apply_telemetry(&mut self, telemetry: DashboardTelemetry) {
        self.rte_percent = telemetry.rte;
        self.usable_energy = telemetry.usable;
        self.pack_capacity = telemetry.capacity;
    }

    fn refresh(
        &mut self,
        engine: &EngineState,
        decision: Option<(&ControlDecision, Timestamp)>,
        as_of: Timestamp,
    ) {
        self.engine = engine.clone();
        if let Some((decision, at)) = decision {
            if self.recent_decisions.len() == DECISION_LOG_CAPACITY {
                self.recent_decisions.pop_front();
            }
            self.recent_decisions.push_back((at, decision.clone()));
            self.last_decision = Some(decision.clone());
        }
        self.as_of = as_of;
    }
}

/// Seed a fresh channel value's decision log from the journal — called once,
/// at startup, from `run()`. Read failures degrade to an empty log rather
/// than failing startup, the same "a logging concern must never become a
/// control failure" rule the journal itself follows.
pub fn seed_decision_log(journal_path: &std::path::Path) -> Vec<(Timestamp, ControlDecision)> {
    match read_recent_decisions(journal_path, DECISION_LOG_CAPACITY) {
        Ok(rows) => rows.into_iter().map(|row| (row.at, row.decision)).collect(),
        Err(e) => {
            tracing::warn!("Dashboard: cannot seed decision log from journal: {e}");
            Vec::new()
        }
    }
}

pub type DashboardStateSender = tokio::sync::watch::Sender<DashboardState>;
pub type DashboardStateReceiver = tokio::sync::watch::Receiver<DashboardState>;

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;
