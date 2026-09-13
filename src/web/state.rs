//! What the dashboard renders from: a snapshot of the engine plus the bits of
//! telemetry the engine itself never carries (RTE, usable energy, pack
//! capacity — see `run.rs`'s `PollTelemetry`), refreshed after every event the
//! coordinator loop folds and broadcast to every connected browser tab.

use std::collections::VecDeque;

use crate::engine::EngineState;
use crate::models::ControlDecision;
use crate::units::{GridPower, KiloWattHours, Percent, SolarPower, Timestamp, Watts};

/// How many samples each sparkline keeps. ~96 seconds of history at the
/// meter's ~1/s cadence — enough to show a recent trend, not a history chart.
const SPARKLINE_CAPACITY: usize = 96;

/// How many rows the decision log shows: seeded from the journal at startup
/// and capped at this size from then on as new decisions arrive.
pub const DECISION_LOG_CAPACITY: usize = 20;

/// A fixed-capacity ring buffer of recent readings for one stat card's
/// sparkline.
#[derive(Debug, Clone, Default)]
pub struct Sparkline {
    samples: VecDeque<f64>,
}

impl Sparkline {
    pub fn push(&mut self, value: f64) {
        if self.samples.len() == SPARKLINE_CAPACITY {
            self.samples.pop_front();
        }
        self.samples.push_back(value);
    }

    pub fn samples(&self) -> &VecDeque<f64> {
        &self.samples
    }
}

/// One ring buffer per stat card that has real data. EV has no real source
/// (see the design handoff) and carries no history.
#[derive(Debug, Clone, Default)]
pub struct SparklineHistory {
    pub solar: Sparkline,
    pub home_usage: Sparkline,
    pub grid: Sparkline,
}

/// A snapshot of everything the dashboard renders, refreshed after every
/// event `run()` folds and broadcast over a `watch` channel — each SSE
/// connection gets its own `Receiver`, and a `GET /` gets the latest value
/// via `borrow()`.
#[derive(Debug, Clone)]
pub struct DashboardState {
    pub engine: EngineState,
    /// The most recent decision, if any — what the battery panel's mode
    /// badge reflects.
    pub last_decision: Option<ControlDecision>,
    /// The decision log, oldest first, capped at [`DECISION_LOG_CAPACITY`].
    /// Seeded from the journal once at startup; every later decision pushes
    /// onto the end and drops the oldest row once full — the same rows a
    /// page load and every SSE fragment render from, so the two can never
    /// disagree about what the log currently shows.
    pub recent_decisions: VecDeque<(Timestamp, ControlDecision)>,
    /// Dashboard-local telemetry `EngineState` never carries — see
    /// `run.rs`'s `PollTelemetry`, which already computes these.
    pub rte_percent: Option<Percent>,
    pub usable_energy: KiloWattHours,
    pub pack_capacity: KiloWattHours,
    pub sparklines: SparklineHistory,
    pub as_of: Timestamp,
}

impl DashboardState {
    /// The channel's initial value: the just-folded startup state, seeded
    /// with whatever decision history the journal already has.
    pub fn seed(
        engine: &EngineState,
        history: Vec<(Timestamp, ControlDecision)>,
        as_of: Timestamp,
    ) -> Self {
        DashboardState {
            engine: engine.clone(),
            last_decision: history.last().map(|(_, d)| d.clone()),
            recent_decisions: history.into(),
            rte_percent: None,
            usable_energy: KiloWattHours::ZERO,
            pack_capacity: KiloWattHours::ZERO,
            sparklines: SparklineHistory::default(),
            as_of,
        }
    }

    /// Fold one tick of `run()`'s loop into the next dashboard snapshot,
    /// carrying forward the sparkline history and the decision log unless a
    /// new decision is given.
    #[allow(clippy::too_many_arguments)]
    pub fn next(
        previous: &DashboardState,
        engine: &EngineState,
        grid: GridPower,
        solar: SolarPower,
        home_usage: Watts,
        new_decision: Option<(&ControlDecision, Timestamp)>,
        telemetry: Option<(Option<Percent>, KiloWattHours, KiloWattHours)>,
        as_of: Timestamp,
    ) -> Self {
        let mut sparklines = previous.sparklines.clone();
        sparklines.solar.push(solar.get());
        sparklines.grid.push(grid.get());
        sparklines.home_usage.push(home_usage.as_f64());

        let (last_decision, recent_decisions) = match new_decision {
            Some((decision, at)) => {
                let mut log = previous.recent_decisions.clone();
                if log.len() == DECISION_LOG_CAPACITY {
                    log.pop_front();
                }
                log.push_back((at, decision.clone()));
                (Some(decision.clone()), log)
            }
            None => (
                previous.last_decision.clone(),
                previous.recent_decisions.clone(),
            ),
        };

        let (rte_percent, usable_energy, pack_capacity) = telemetry.unwrap_or((
            previous.rte_percent,
            previous.usable_energy,
            previous.pack_capacity,
        ));

        DashboardState {
            engine: engine.clone(),
            last_decision,
            recent_decisions,
            rte_percent,
            usable_energy,
            pack_capacity,
            sparklines,
            as_of,
        }
    }
}

pub type DashboardStateSender = tokio::sync::watch::Sender<DashboardState>;
pub type DashboardStateReceiver = tokio::sync::watch::Receiver<DashboardState>;
