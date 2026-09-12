//! Test scenarios shared across modules.
//!
//! Distinct from the `test_*` constructors that live next to their types —
//! `Clock::test_at`, `BatteryState::test_sample`, `Controller::test_default`,
//! `SessionConfig::test_default`. Each of those is *a canonical sample of type
//! T, on type T*, and belongs beside T.
//!
//! What lives here is the other thing: a scenario, about how the controller
//! behaves over time, that happens to be expressed as a list of [`Event`]s.
//! `journey` is not a sample `Event` — it is eight readings tuned to cross both
//! start thresholds with a timeout and a resume in the middle — so putting it
//! next to the definition of `Event` would have made `event.rs` a quarter test
//! scenario for two other modules, and would have claimed a precedent the
//! `test_*` convention does not set.

use crate::battery::BatteryState;
use crate::clock::Clock;
use crate::event::Event;
use crate::units::{GridPower, Soc, SolarPower};
use crate::world::{DeviceId, Measurement, MeterReading};

/// A run the engine can be folded over, for tests that need a *sequence*
/// rather than one event. Shared by `engine.rs` and the replay tests, which
/// need the same journey — a second copy would be a second thing to keep in
/// step, and the two disagreeing would make a `--verify` test meaningless.
///
/// The constants are public because a caller building the world this is folded
/// into has to agree with it on the device id and the day ordinal, or the
/// midnight reset fires on the first step.
pub mod journey {
    use super::*;

    /// A recent instant, not a round number near the epoch. `1_000_000_000` ms
    /// is January 1970, which is beyond any retention window — so a test that
    /// recorded two sessions into one journal had the second one's startup
    /// prune delete the first, and the restart it was trying to observe with
    /// it.
    pub const NOW_MS: i64 = 1_757_000_000_000;
    pub const DAY: u32 = 100;
    pub const BATTERY_ID: &str = "test-battery";

    pub fn clock_at(secs: i64) -> Clock {
        Clock {
            day_ordinal: DAY,
            ..Clock::test_at(NOW_MS + secs * 1000)
        }
    }

    /// A sequence chosen to write every field of the engine's snapshot: swings
    /// across both start thresholds (mode changes, cooldown stamps, transition
    /// counters), a device update, and a timeout/resume pair, so a snapshot
    /// taken between them has to carry `mqtt_timed_out` too.
    pub fn events() -> Vec<Event> {
        let meter_at = |secs, total| Event::Meter {
            at: clock_at(secs),
            grid: MeterReading::total_only(GridPower(total)),
            solar: SolarPower::new(0.0),
        };
        vec![
            meter_at(0, -500.0),
            meter_at(20, -800.0),
            meter_at(40, 300.0),
            // Deliberately *not* the same battery a fixture world starts with:
            // an update that changes nothing leaves a restored world
            // indistinguishable from a fresh one, and the snapshot's `world`
            // stops being under test.
            Event::DeviceUpdate {
                at: clock_at(60),
                id: DeviceId::new(BATTERY_ID),
                measurement: Measurement::Battery(BatteryState {
                    soc: Soc::new(81),
                    ..BatteryState::test_sample()
                }),
            },
            Event::MqttTimeout { at: clock_at(80) },
            meter_at(100, 250.0),
            meter_at(120, -600.0),
            meter_at(140, 400.0),
        ]
    }

    /// What `main.rs` journals at startup: the world's first battery, arriving
    /// through the fold rather than written into the world behind it.
    pub fn startup() -> Event {
        Event::DeviceUpdate {
            at: clock_at(0),
            id: DeviceId::new(BATTERY_ID),
            measurement: Measurement::Battery(BatteryState::test_sample()),
        }
    }

    /// The whole recorded stream as a session actually produces it — the
    /// startup seed, then the journey.
    pub fn session() -> Vec<Event> {
        std::iter::once(startup()).chain(events()).collect()
    }
}
