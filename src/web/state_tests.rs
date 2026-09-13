//! Which tick moves what. All of it is pure: a snapshot goes in, a snapshot
//! comes out, so none of this needs a channel or a server.

use super::*;

use crate::battery::BatteryState;
use crate::fixtures::journey;
use crate::units::{GridPower, SolarPower};
use crate::world::{DeviceId, Measurement, MeterReading, World};

fn at(secs: i64) -> Timestamp {
    Timestamp::from_millis(journey::NOW_MS + secs * 1000)
}

fn engine_state(grid: GridPower, solar: SolarPower) -> EngineState {
    let mut world = World::new();
    world.observe_meter(MeterReading::total_only(grid), solar);
    world.observe_device(
        DeviceId::new(journey::BATTERY_ID),
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

fn seeded() -> DashboardState {
    DashboardState::seed(&engine(), vec![], at(0))
}

fn telemetry() -> DashboardTelemetry {
    DashboardTelemetry {
        rte: Some(Percent(91.4)),
        usable: KiloWattHours(1.8),
        capacity: KiloWattHours(3.84),
    }
}

fn decision() -> ControlDecision {
    ControlDecision::test_sample()
}

// --- Sparklines sample the meter, and only the meter ---------------------

/// The poll timer and the MQTT failsafe both fire on their own schedules. A
/// sample from either would re-push the last meter reading, flattening the
/// series and making the ~1/s window the capacity is sized for a fiction.
#[test]
fn only_a_meter_tick_extends_the_sparklines() {
    let mut state = seeded();

    state.poll_tick(&engine(), telemetry(), at(1));
    state.failsafe_tick(&engine(), None, at(2));
    assert!(state.sparklines.grid.is_empty());
    assert!(state.sparklines.solar.is_empty());
    assert!(state.sparklines.home_usage.is_empty());

    state.meter_tick(
        &engine_state(GridPower(100.0), SolarPower::new(10.0)),
        None,
        at(3),
    );
    state.poll_tick(&engine(), telemetry(), at(4));
    state.meter_tick(
        &engine_state(GridPower(200.0), SolarPower::new(20.0)),
        None,
        at(5),
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
        state.meter_tick(
            &engine_state(GridPower(i as f64), SolarPower::ZERO),
            None,
            at(i as i64),
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
    state.poll_tick(&engine(), telemetry(), at(1));

    state.meter_tick(&engine(), None, at(2));
    state.failsafe_tick(&engine(), None, at(3));

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

    state.meter_tick(&engine(), None, at(1));
    assert!(state.recent_decisions.is_empty());
    assert!(state.last_decision.is_none());

    for i in 0..DECISION_LOG_CAPACITY + 5 {
        let decision = decision();
        state.meter_tick(&engine(), Some((&decision, at(i as i64))), at(i as i64));
    }

    assert_eq!(state.recent_decisions.len(), DECISION_LOG_CAPACITY);
    assert_eq!(
        state.recent_decisions.front().map(|(at, _)| *at),
        Some(at(5))
    );
    assert!(state.last_decision.is_some());
}

/// The failsafe's forced idle is a decision this process made, so the badge
/// has to follow it.
#[test]
fn a_failsafe_decision_reaches_the_badge() {
    let mut state = seeded();
    let forced = decision();
    state.failsafe_tick(&engine(), Some((&forced, at(1))), at(1));

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
