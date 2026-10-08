//! Which tick moves what. All of it is pure: a snapshot goes in, a snapshot
//! comes out, so none of this needs a channel or a server.

use super::*;

use crate::battery::BatteryState;
use crate::fixtures::{journey, utc};
use crate::units::{BatteryPower, GridPower, SolarPower, Timestamp};
use crate::world::{DeviceId, Measurement, MeterReading, World};

fn at(secs: i64) -> Timestamp {
    Timestamp::from_millis(journey::NOW_MS + secs * 1000)
}

fn clock(secs: i64) -> Clock {
    journey::clock_at(secs)
}

fn tz() -> chrono_tz::Tz {
    chrono_tz::UTC
}

fn engine_state(grid: GridPower, solar: SolarPower) -> EngineState {
    let mut world = World::new();
    world.observe_meter(None, MeterReading::total_only(grid), solar);
    world.observe_device(
        DeviceId::new(journey::BATTERY_ID),
        Timestamp::from_millis(0),
        Measurement::Battery(BatteryState::test_sample()),
    );

    EngineState {
        world,
        controller: crate::controller::Controller::test_default(journey::NOW_MS, journey::DAY)
            .state(),
        mqtt_timed_out: false,
    }
}

fn engine() -> EngineState {
    engine_state(GridPower(400.0), SolarPower::new(750.0))
}

fn intervals() -> IntervalHistory {
    IntervalHistory::new([DeviceId::new(journey::BATTERY_ID)])
}

fn seeded() -> DashboardState {
    DashboardState::seed(
        &engine(),
        vec![],
        ActualSolarHistory::default(),
        intervals(),
        at(0),
    )
}

fn telemetry() -> DashboardTelemetry {
    DashboardTelemetry {
        rte: Some(Percent(91.4)),
        usable: KiloWattHours(1.8),
        capacity: KiloWattHours(3.84),
        packs: Vec::new(),
        soc_limits: SocLimits::default(),
    }
}

/// A meter tick whose event carries nothing the test is about: the
/// interval ring has tests of its own.
fn meter_tick(
    state: &mut DashboardState,
    engine: &EngineState,
    decision: Option<(&ControlDecision, Timestamp)>,
    clock: &Clock,
    timezone: chrono_tz::Tz,
) {
    let event = journey::meter_event(clock.now, 0.0, 0.0);
    state.meter_tick(
        engine,
        &event,
        decision,
        clock,
        timezone,
        SocLimits::default(),
    );
}

fn poll_tick(
    state: &mut DashboardState,
    engine: &EngineState,
    telemetry: DashboardTelemetry,
    as_of: Timestamp,
) {
    let event = journey::battery_event(as_of, BatteryPower::ZERO, Soc::new(50));
    state.poll_tick(engine, &event, None, telemetry, as_of);
}

fn decision() -> ControlDecision {
    ControlDecision::test_sample()
}

/// A decision whose *command* differs from every other `watts`, while its
/// reason and grid reading stay put — the axis the log collapses on.
fn at_watts(watts: i32) -> ControlDecision {
    ControlDecision {
        power_watts: crate::units::Setpoint::new(watts),
        ..ControlDecision::test_sample()
    }
}

/// The same command as [`decision`], but with the live figures a quiet period
/// moves under it: this is what must *not* push a new row.
fn same_command_different_reason(reason: &str, grid: f64) -> ControlDecision {
    ControlDecision {
        reason: reason.to_string(),
        grid_power: GridPower(grid),
        ..ControlDecision::test_sample()
    }
}

// --- Sparklines sample the meter, and only the meter ---------------------

/// The poll timer and the MQTT failsafe both fire on their own schedules. A
/// sample from either would re-push the last meter reading, flattening the
/// series and making the ~1/s window the capacity is sized for a fiction.
#[test]
fn only_a_meter_tick_extends_the_sparklines() {
    let mut state = seeded();

    poll_tick(&mut state, &engine(), telemetry(), at(1));
    state.failsafe_tick(&engine(), None, at(2), SocLimits::default());
    assert!(state.sparklines.grid.is_empty());
    assert!(state.sparklines.solar.is_empty());
    assert!(state.sparklines.home_usage.is_empty());

    meter_tick(
        &mut state,
        &engine_state(GridPower(100.0), SolarPower::new(10.0)),
        None,
        &clock(3),
        tz(),
    );
    poll_tick(&mut state, &engine(), telemetry(), at(4));
    meter_tick(
        &mut state,
        &engine_state(GridPower(200.0), SolarPower::new(20.0)),
        None,
        &clock(5),
        tz(),
    );

    assert_eq!(
        state.sparklines.grid.values().collect::<Vec<_>>(),
        vec![100.0, 200.0],
    );
    assert_eq!(
        state.sparklines.solar.values().collect::<Vec<_>>(),
        vec![10.0, 20.0],
    );
    assert_eq!(state.sparklines.home_usage.len(), 2);
}

#[test]
fn a_sparkline_holds_its_capacity_and_no_more() {
    let mut state = seeded();
    for i in 0..SPARKLINE_CAPACITY + 10 {
        meter_tick(
            &mut state,
            &engine_state(GridPower(i as f64), SolarPower::ZERO),
            None,
            &clock(i as i64),
            tz(),
        );
    }

    let samples: Vec<f64> = state.sparklines.grid.values().collect();
    assert_eq!(samples.len(), SPARKLINE_CAPACITY);
    assert_eq!(samples.first().copied(), Some(10.0));
    assert_eq!(
        samples.last().copied(),
        Some((SPARKLINE_CAPACITY + 9) as f64)
    );
}

// --- Telemetry -----------------------------------------------------------

/// The first page load lands ~10s before the first poll, so the seed has to
/// carry the startup reading rather than a confident zero.
#[test]
fn a_seed_carries_the_startup_polls_own_figures() {
    let state = seeded().with_telemetry(telemetry());

    assert_eq!(state.pack_capacity, KiloWattHours(3.84));
    assert_eq!(state.usable_energy, KiloWattHours(1.8));
    assert_eq!(state.rte_percent, Some(Percent(91.4)));
}

/// Telemetry moves on a poll and is carried by every tick in between, so the
/// battery panel does not blink back to zero on each meter reading.
#[test]
fn telemetry_survives_the_ticks_that_do_not_carry_it() {
    let mut state = seeded();
    poll_tick(&mut state, &engine(), telemetry(), at(1));

    meter_tick(&mut state, &engine(), None, &clock(2), tz());
    state.failsafe_tick(&engine(), None, at(3), SocLimits::default());

    assert_eq!(state.pack_capacity, KiloWattHours(3.84));
    assert_eq!(state.usable_energy, KiloWattHours(1.8));
    assert_eq!(state.rte_percent, Some(Percent(91.4)));
    assert_eq!(state.as_of, at(3));
}

// --- The decision log ----------------------------------------------------

/// A tick with no decision must not duplicate the last row, and the log is
/// bounded — it is a panel, not a history.
#[test]
fn the_decision_log_appends_only_real_decisions_and_stays_bounded() {
    let mut state = seeded();
    assert!(state.last_decision.is_none());

    meter_tick(&mut state, &engine(), None, &clock(1), tz());
    assert!(state.recent_decisions.is_empty());
    assert!(state.last_decision.is_none());

    for i in 0..DECISION_LOG_CAPACITY + 5 {
        let decision = at_watts(i as i32);
        meter_tick(
            &mut state,
            &engine(),
            Some((&decision, at(i as i64))),
            &clock(i as i64),
            tz(),
        );
    }

    assert_eq!(state.recent_decisions.len(), DECISION_LOG_CAPACITY);
    assert_eq!(
        state.recent_decisions.front().map(|entry| entry.first_at),
        Some(at(5))
    );
    assert!(state.last_decision.is_some());
}

/// A quiet period decides the same thing every 5 seconds. Those repeats must
/// fold into one row rather than flushing everything interesting out of a
/// 20-row log — and they fold on the *command*, even though the reason and
/// the grid reading move under it.
#[test]
fn a_repeated_command_extends_the_newest_row_instead_of_pushing_another() {
    let mut state = seeded();

    state.failsafe_tick(
        &engine(),
        Some((&same_command_different_reason("grid: 150W", 150.5), at(1))),
        at(1),
        SocLimits::default(),
    );
    state.failsafe_tick(
        &engine(),
        Some((&same_command_different_reason("grid: 162W", 162.25), at(6))),
        at(6),
        SocLimits::default(),
    );
    state.failsafe_tick(
        &engine(),
        Some((&same_command_different_reason("grid: 171W", 171.75), at(11))),
        at(11),
        SocLimits::default(),
    );

    assert_eq!(state.recent_decisions.len(), 1);
    let row = state.recent_decisions.back().unwrap();
    assert_eq!(row.repeats, 3);
    assert_eq!(row.first_at, at(1));
    assert_eq!(row.last_at, at(11));
    // The row speaks for the moment it claims to: the newest reason, not the
    // one the run opened with.
    assert_eq!(row.decision.reason, "grid: 171W");
}

/// A row collapses only what it commands. A different setpoint is a different
/// thing to have done, so it earns its own row.
#[test]
fn a_different_command_pushes_a_new_row() {
    let mut state = seeded();

    state.failsafe_tick(
        &engine(),
        Some((&at_watts(145), at(1))),
        at(1),
        SocLimits::default(),
    );
    state.failsafe_tick(
        &engine(),
        Some((&at_watts(145), at(6))),
        at(6),
        SocLimits::default(),
    );
    state.failsafe_tick(
        &engine(),
        Some((&at_watts(900), at(11))),
        at(11),
        SocLimits::default(),
    );

    assert_eq!(state.recent_decisions.len(), 2);
    assert_eq!(state.recent_decisions.front().unwrap().repeats, 2);
    let newest = state.recent_decisions.back().unwrap();
    assert_eq!(newest.repeats, 1);
    assert_eq!(newest.first_at, at(11));
    assert_eq!(newest.last_at, at(11));
}

/// The badge renders from `last_decision`, so a collapsed decision still has
/// to reach it — otherwise a quiet stretch would leave the badge showing the
/// figures of whenever the run started.
#[test]
fn a_collapsed_decision_still_updates_the_badge() {
    let mut state = seeded();

    state.failsafe_tick(
        &engine(),
        Some((&same_command_different_reason("first", 150.5), at(1))),
        at(1),
        SocLimits::default(),
    );
    state.failsafe_tick(
        &engine(),
        Some((&same_command_different_reason("newest", 162.25), at(6))),
        at(6),
        SocLimits::default(),
    );

    assert_eq!(state.recent_decisions.len(), 1);
    assert_eq!(
        state.last_decision.as_ref().map(|d| d.reason.as_str()),
        Some("newest")
    );
    assert_eq!(
        state.last_decision.as_ref().map(|d| d.grid_power),
        Some(GridPower(162.25))
    );
}

/// The seeded rows arrive from the journal uncollapsed, so they must go
/// through the same fold — otherwise a page load right after a restart would
/// disagree with the same process ten minutes later.
#[test]
fn the_seeded_log_collapses_the_same_way_a_running_one_does() {
    let history = vec![
        (at(1), at_watts(145)),
        (at(6), at_watts(145)),
        (at(11), at_watts(145)),
        (at(16), at_watts(900)),
    ];
    let state = DashboardState::seed(
        &engine(),
        history,
        ActualSolarHistory::default(),
        intervals(),
        at(16),
    );

    assert_eq!(state.recent_decisions.len(), 2);
    let first = state.recent_decisions.front().unwrap();
    assert_eq!(first.repeats, 3);
    assert_eq!(first.first_at, at(1));
    assert_eq!(first.last_at, at(11));
    // Journalled rows still cannot speak for what this process is doing.
    assert!(state.last_decision.is_none());
}

/// Collapsing must not be a way around the cap: a run of distinct commands
/// longer than the log still drops its oldest rows.
#[test]
fn collapsed_runs_still_obey_the_capacity_cap() {
    let mut state = seeded();

    for i in 0..DECISION_LOG_CAPACITY + 5 {
        // Two decisions per command, so every row is a collapsed run.
        for repeat in 0..2 {
            let decision = at_watts(i as i32);
            let at = at((i * 2 + repeat) as i64);
            state.failsafe_tick(&engine(), Some((&decision, at)), at, SocLimits::default());
        }
    }

    assert_eq!(state.recent_decisions.len(), DECISION_LOG_CAPACITY);
    assert!(
        state
            .recent_decisions
            .iter()
            .all(|entry| entry.repeats == 2)
    );
    assert_eq!(
        state.recent_decisions.front().map(|entry| entry.first_at),
        Some(at(10))
    );
}

/// The failsafe's forced idle is a decision this process made, so the badge
/// has to follow it.
#[test]
fn a_failsafe_decision_reaches_the_badge() {
    let mut state = seeded();
    let forced = decision();
    state.failsafe_tick(
        &engine(),
        Some((&forced, at(1))),
        at(1),
        SocLimits::default(),
    );

    assert_eq!(state.recent_decisions.len(), 1);
    assert_eq!(
        state.last_decision.as_ref().map(|d| d.mode),
        Some(forced.mode)
    );
}

/// Each buffer takes only its own quantity, so a grid reading cannot reach the
/// solar card — `history.solar.push(GridPower(..))` does not compile.
#[test]
fn each_sparkline_carries_its_own_quantity() {
    let mut history = SparklineHistory::default();
    history.solar.push(SolarPower::new(1200.0));
    history.grid.push(GridPower(-450.0));
    history.home_usage.push(Watts(750));

    assert_eq!(history.solar.values().collect::<Vec<_>>(), vec![1200.0]);
    assert_eq!(history.grid.values().collect::<Vec<_>>(), vec![-450.0]);
    assert_eq!(history.home_usage.values().collect::<Vec<_>>(), vec![750.0]);
}

// --- Actual solar history --------------------------------------------------

#[test]
fn actual_solar_history_buckets_by_half_hour_and_averages() {
    let mut history = ActualSolarHistory::default();
    history.record(utc(1, 6, 0), tz(), 100, SolarPower::new(1000.0));
    history.record(utc(1, 6, 10), tz(), 100, SolarPower::new(2000.0));
    history.record(utc(1, 7, 0), tz(), 100, SolarPower::new(500.0));

    let averages = history.averages();
    assert_eq!(
        averages[12],
        Some(SolarPower::new(1500.0)),
        "06:00-06:30 is bucket 12"
    );
    assert_eq!(
        averages[14],
        Some(SolarPower::new(500.0)),
        "07:00-07:30 is bucket 14"
    );
    assert_eq!(
        averages[16], None,
        "a slot with no samples reads as unknown, not zero"
    );
}

/// A new day ordinal must not let yesterday's samples for the same slot
/// leak into today's average.
#[test]
fn actual_solar_history_resets_on_a_new_day() {
    let mut history = ActualSolarHistory::default();
    history.record(utc(1, 10, 0), tz(), 100, SolarPower::new(5000.0));
    history.record(utc(1, 10, 0), tz(), 101, SolarPower::new(1000.0));

    assert_eq!(
        history.averages()[20],
        Some(SolarPower::new(1000.0)),
        "yesterday's sample must not survive the rollover"
    );
}

#[test]
fn a_meter_tick_records_into_the_actual_solar_history() {
    let mut state = seeded();
    let clock = Clock {
        now: utc(1, 14, 0),
        day_ordinal: 200,
        ..Clock::test_at(journey::NOW_MS)
    };
    meter_tick(
        &mut state,
        &engine_state(GridPower(0.0), SolarPower::new(3000.0)),
        None,
        &clock,
        tz(),
    );

    assert_eq!(
        state.actual_solar.averages()[28],
        Some(SolarPower::new(3000.0))
    );
}

/// A restart mid-day shows the line the process it replaced was showing,
/// and nothing from before midnight.
#[tokio::test]
async fn the_actual_solar_seed_equals_todays_live_fold() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    let yesterday = journey::meter_event(utc(1, 23, 0), 0.0, 4000.0);
    let today = [
        journey::meter_event(utc(2, 6, 0), 0.0, 1000.0),
        journey::meter_event(utc(2, 6, 10), 0.0, 2000.0),
        journey::meter_event(utc(2, 7, 0), 0.0, 500.0),
    ];
    let events: Vec<Event> = std::iter::once(yesterday).chain(today.clone()).collect();
    crate::journal::testing::record(&path, &events).await;

    let mut live = ActualSolarHistory::default();
    for event in &today {
        if let Event::Meter { at, solar, .. } = event {
            live.record(at.now, tz(), at.day_ordinal, *solar);
        }
    }
    let seeded = seed_actual_solar(&path, utc(2, 8, 0), tz());

    assert_eq!(seeded.averages(), live.averages());
    assert_eq!(seeded.averages()[46], None, "yesterday's 23:00 stays out");
}

#[test]
fn an_unreadable_journal_seeds_an_empty_actual_solar_history() {
    let dir = tempfile::tempdir().unwrap();
    let seeded = seed_actual_solar(&dir.path().join("missing.db"), utc(2, 8, 0), tz());
    assert!(seeded.averages().iter().all(Option::is_none));
}

// --- Forecast tick ----------------------------------------------------------

/// The forecast poller's tick has no engine snapshot or decision behind it —
/// it must not disturb anything the engine-driven ticks own.
#[test]
fn forecast_tick_changes_only_the_forecast_field() {
    let mut state = seeded();
    let engine_before = state.engine.clone();
    let as_of_before = state.as_of;

    let forecast = ForecastSnapshot {
        points: vec![],
        as_of: Some(at(5)),
    };
    state.forecast_tick(forecast.clone());

    assert_eq!(state.forecast, forecast);
    assert_eq!(state.engine, engine_before);
    assert_eq!(state.as_of, as_of_before);
    assert!(state.recent_decisions.is_empty());
    assert!(state.last_decision.is_none());
}

#[test]
fn a_full_bar_is_worth_what_usable_energy_reads_at_max() {
    let mut state = seeded();
    let limits = SocLimits {
        min: Soc::new(20),
        max: Soc::new(70),
        balance_day: false,
    };
    poll_tick(
        &mut state,
        &engine(),
        DashboardTelemetry {
            soc_limits: limits,
            capacity: KiloWattHours(4.0),
            ..telemetry()
        },
        at(1),
    );

    assert_eq!(state.soc_limits, limits);
    // 50% of 4 kWh, discounted by the 91.4% round trip `telemetry` carries.
    assert!((state.usable_max().get() - 1.828).abs() < 1e-9);

    let dir = tempfile::tempdir().unwrap();
    let rte = crate::rte::RteTracker::new(dir.path().join("rte.json"));
    let at_max = rte.usable_kwh(limits.max, limits.min, crate::units::WattHours(4000.0));
    let no_history = DashboardState {
        rte_percent: None,
        ..state
    };
    assert!((no_history.usable_max().get() - at_max.get()).abs() < 1e-9);
}

#[test]
fn a_meter_tick_refreshes_the_limits() {
    let mut state = seeded();
    let balance = SocLimits {
        min: Soc::new(10),
        max: Soc::FULL,
        balance_day: true,
    };
    state.meter_tick(
        &engine(),
        &journey::meter_event(at(1), 0.0, 0.0),
        None,
        &clock(1),
        tz(),
        balance,
    );

    assert_eq!(state.soc_limits, balance);
}
