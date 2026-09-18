//! What the dashboard renders from: a snapshot of the engine plus the bits of
//! telemetry the engine itself never carries (RTE, usable energy, pack
//! capacity — see `run.rs`'s `PollTelemetry`), refreshed after every event the
//! coordinator loop folds and broadcast to every connected browser tab.

use std::collections::VecDeque;

use chrono_tz::Tz;

use crate::clock::Clock;
use crate::command::Command;
use crate::engine::EngineState;
use crate::journal::read::read_recent_decisions;
use crate::models::ControlDecision;
use crate::units::{
    GridPower, KiloWattHours, Percent, SolarForecastPoint, SolarPower, Timestamp, Watts,
};

/// Solcast's own resolution (see `SolcastEntry`'s doc comment in
/// `prediction/solcast.rs`) — the forecast panel's bars and
/// [`ActualSolarHistory`]'s buckets both use this, so the two series share
/// one axis. `forecast-panel__axis`'s column count has to match.
pub(super) const SOLAR_BUCKETS_PER_DAY: usize = 48;

/// One bucket's width: the half-hour [`SOLAR_BUCKETS_PER_DAY`] divides the
/// day into.
pub(super) const SOLAR_BUCKET_MS: i64 = 30 * 60 * 1000;

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

/// Today's *actual* measured solar production, bucketed into the same 48
/// local half-hours the forecast panel's bars use, so the two share one
/// x-axis. Reset at local midnight, keyed on the `day_ordinal` a meter tick
/// already carries via its `Clock`.
#[derive(Debug, Clone)]
pub struct ActualSolarHistory {
    day_ordinal: Option<u32>,
    sum_by_bucket: [f64; SOLAR_BUCKETS_PER_DAY],
    count_by_bucket: [u32; SOLAR_BUCKETS_PER_DAY],
}

// `#[derive(Default)]` only covers arrays up to length 32 (a pre-const-generics
// limitation std still carries), and `SOLAR_BUCKETS_PER_DAY` is 48.
impl Default for ActualSolarHistory {
    fn default() -> Self {
        ActualSolarHistory {
            day_ordinal: None,
            sum_by_bucket: [0.0; SOLAR_BUCKETS_PER_DAY],
            count_by_bucket: [0; SOLAR_BUCKETS_PER_DAY],
        }
    }
}

impl ActualSolarHistory {
    /// Folds one meter reading into its local half-hour's running average,
    /// resetting every bucket first if `day_ordinal` has moved on from
    /// whatever this last saw. The bucket comes from `now`/`timezone` rather
    /// than `Clock`, which deliberately resolves no finer than the hour (see
    /// `prediction::LocalNow`'s doc comment) — extending it would touch every
    /// journalled `Event`.
    pub fn record(&mut self, now: Timestamp, timezone: Tz, day_ordinal: u32, solar: SolarPower) {
        if self.day_ordinal != Some(day_ordinal) {
            *self = ActualSolarHistory {
                day_ordinal: Some(day_ordinal),
                ..ActualSolarHistory::default()
            };
        }
        let today_start = crate::clock::local_midnight(now, timezone);
        let elapsed_ms = (now - today_start).as_millis().max(0);
        let bucket = ((elapsed_ms / SOLAR_BUCKET_MS) as usize).min(SOLAR_BUCKETS_PER_DAY - 1);
        self.sum_by_bucket[bucket] += solar.get();
        self.count_by_bucket[bucket] += 1;
    }

    /// One average watts figure per local half-hour, `None` where nothing has
    /// been recorded yet today (every slot from now on, and any slot lost to
    /// downtime before this process's first meter tick or its startup seed).
    pub fn averages(&self) -> [Option<f64>; SOLAR_BUCKETS_PER_DAY] {
        std::array::from_fn(|h| {
            (self.count_by_bucket[h] > 0)
                .then(|| self.sum_by_bucket[h] / f64::from(self.count_by_bucket[h]))
        })
    }
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
    pub sparklines: SparklineHistory,
    /// Today's actual solar production, seeded from the journal at startup
    /// and extended by every `meter_tick` thereafter.
    pub actual_solar: ActualSolarHistory,
    /// The forecast poller's latest cached series — see `crate::prediction`.
    /// Updated only by `forecast_tick`, on that poller's own schedule.
    pub forecast: ForecastSnapshot,
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
    /// load and a long-running process show the same runs. `actual_solar` is
    /// likewise seeded from the journal (see
    /// `crate::journal::read::read_meter_solar_since`) so a restart doesn't
    /// blank today's actual-production line.
    pub fn seed(
        engine: &EngineState,
        history: Vec<(Timestamp, ControlDecision)>,
        actual_solar: ActualSolarHistory,
        as_of: Timestamp,
    ) -> Self {
        let mut state = DashboardState {
            engine: engine.clone(),
            last_decision: None,
            recent_decisions: VecDeque::new(),
            rte_percent: None,
            usable_energy: KiloWattHours::ZERO,
            pack_capacity: KiloWattHours::ZERO,
            sparklines: SparklineHistory::default(),
            actual_solar,
            forecast: ForecastSnapshot::default(),
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

    /// A meter reading: the only tick that extends the sparklines, which is
    /// what keeps them on the meter's cadence. Takes the whole `Clock`
    /// (rather than a bare `Timestamp`, as the other ticks do) because
    /// `actual_solar` needs the day ordinal it already carries; `timezone` is
    /// separate because `Clock` deliberately resolves no finer than the hour.
    pub fn meter_tick(
        &mut self,
        engine: &EngineState,
        decision: Option<(&ControlDecision, Timestamp)>,
        clock: &Clock,
        timezone: Tz,
    ) {
        self.sparklines.solar.push(engine.world.solar);
        self.sparklines.grid.push(engine.world.grid.total);
        self.sparklines.home_usage.push(engine.world.home_usage());
        self.actual_solar
            .record(clock.now, timezone, clock.day_ordinal, engine.world.solar);
        self.refresh(engine, decision, clock.now);
    }

    /// The forecast poller's own tick — not folded through `refresh` like the
    /// other three, since it carries no engine snapshot or decision; it
    /// updates on its own schedule (see `crate::prediction::run_forecast_poller`),
    /// independent of every event the engine folds.
    pub fn forecast_tick(&mut self, forecast: ForecastSnapshot) {
        self.forecast = forecast;
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
