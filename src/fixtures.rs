//! Test scenarios shared across modules.

use crate::battery::BatteryState;
use crate::clock::Clock;
use crate::event::Event;
use crate::units::{BatteryPower, GridPower, Soc, SolarPower, Timestamp};
use crate::world::{DeviceId, Measurement, MeterReading};

/// A run the engine can be folded over, for tests needing a *sequence* rather
/// than one event. Shared by `engine.rs` and the replay tests so a `--verify`
/// test can't disagree with itself. Constants are public because a caller's
/// world must agree on the device id and day ordinal, or the midnight reset fires on
/// the first step.
pub mod journey {
    use super::*;

    /// A recent instant, not a round number near the epoch: `1_000_000_000` ms
    /// is January 1970, outside any retention window, so a second session's
    /// startup prune would delete the first — and the restart it was meant to observe.
    pub const NOW_MS: i64 = 1_757_000_000_000;
    pub const DAY: u32 = 100;
    pub const BATTERY_ID: &str = "test-battery";

    pub fn clock_at(secs: i64) -> Clock {
        Clock {
            day_ordinal: DAY,
            ..Clock::test_at(NOW_MS + secs * 1000)
        }
    }

    /// The journey's clock, moved to `now` without moving its day.
    pub fn clock_on(now: Timestamp) -> Clock {
        Clock { now, ..clock_at(0) }
    }

    pub fn meter(at: Clock, sampled_at: Option<Timestamp>, grid: f64, solar: f64) -> Event {
        Event::Meter {
            at,
            sampled_at,
            grid: MeterReading::total_only(GridPower(grid)),
            solar: SolarPower::new(solar),
        }
    }

    pub fn device_update(at: Clock, id: &str, state: BatteryState) -> Event {
        Event::DeviceUpdate {
            at,
            id: DeviceId::new(id),
            measurement: Measurement::Battery(state),
        }
    }

    /// A meter reading sampled at `now`, as the live loop records one.
    pub fn meter_event(now: Timestamp, grid: f64, solar: f64) -> Event {
        meter(clock_on(now), Some(now), grid, solar)
    }

    /// The journey's battery reporting `power` and `soc` at `now`.
    pub fn battery_event(now: Timestamp, power: BatteryPower, soc: Soc) -> Event {
        device_update(
            clock_on(now),
            BATTERY_ID,
            BatteryState {
                current_power: power,
                soc,
                ..BatteryState::test_sample()
            },
        )
    }

    /// A sequence chosen to write every field of the engine's snapshot: swings
    /// across both start thresholds (mode changes, cooldown stamps, transition
    /// counters), a device update, and a timeout/resume pair, so a snapshot
    /// taken between them has to carry `mqtt_timed_out` too.
    pub fn events() -> Vec<Event> {
        let meter_at = |secs, total| meter(clock_at(secs), None, total, 0.0);
        vec![
            meter_at(0, -500.0),
            meter_at(20, -800.0),
            meter_at(40, 300.0),
            // Deliberately *not* the same battery a fixture world starts with:
            // an update that changes nothing leaves a restored world
            // indistinguishable from a fresh one, and the snapshot's `world`
            // stops being under test.
            device_update(
                clock_at(60),
                BATTERY_ID,
                BatteryState {
                    soc: Soc::new(81),
                    ..BatteryState::test_sample()
                },
            ),
            Event::MqttTimeout { at: clock_at(80) },
            meter_at(100, 250.0),
            meter_at(120, -600.0),
            meter_at(140, 400.0),
        ]
    }

    /// What `main.rs` journals at startup: the world's first battery, arriving
    /// through the fold rather than written into the world behind it.
    pub fn startup() -> Event {
        device_update(clock_at(0), BATTERY_ID, BatteryState::test_sample())
    }

    /// The whole recorded stream as a session actually produces it — the
    /// startup seed, then the journey.
    pub fn session() -> Vec<Event> {
        std::iter::once(startup()).chain(events()).collect()
    }

    /// An empty dashboard interval ring that counts the journey's battery.
    pub fn interval_ring() -> crate::web::IntervalHistory {
        crate::web::IntervalHistory::new([DeviceId::new(BATTERY_ID)])
    }

    /// A poll report as the device journals it: two packs, the first charging
    /// and the second discharging at `power`.
    pub fn poll_body(device: &str, soc: u32, power: i32, temp: u32) -> String {
        format!(
            r#"{{"sn":"{device}","properties":{{"packNum":2}},"packData":[
            {{"sn":"P1","packType":500,"socLevel":{soc},"state":1,"power":{power},"maxTemp":{temp}}},
            {{"sn":"P2","packType":501,"socLevel":{soc},"state":2,"power":{power},"maxTemp":{temp}}}
        ]}}"#
        )
    }
}

/// An instant in October 2026, UTC: a month that holds Europe's autumn DST
/// change, so one helper serves both plain and DST-crossing tests.
pub fn utc(day: u32, hour: u32, minute: u32) -> Timestamp {
    use chrono::TimeZone;
    Timestamp::from(
        chrono::Utc
            .with_ymd_and_hms(2026, 10, day, hour, minute, 0)
            .single()
            .expect("a valid UTC instant"),
    )
}

const YEAR: i32 = 2026;

/// The dashboard tests' zone: one with DST, so a day can be 23, 24 or 25
/// hours long.
pub fn amsterdam() -> chrono_tz::Tz {
    chrono_tz::Europe::Amsterdam
}

/// A date in 2026, the year every local-day helper here shares.
pub fn date(month: u32, day: u32) -> chrono::NaiveDate {
    chrono::NaiveDate::from_ymd_opt(YEAR, month, day).expect("a valid date")
}

/// Amsterdam wall-clock time on `month`/`day` 2026.
pub fn local(month: u32, day: u32, hour: u32, minute: u32) -> Timestamp {
    use chrono::TimeZone;
    Timestamp::from(
        amsterdam()
            .with_ymd_and_hms(YEAR, month, day, hour, minute, 0)
            .single()
            .expect("a wall-clock time that occurs once"),
    )
}

/// An anchor-driven poller's clock read at local `hour:minute` on `y-m-d`.
pub fn local_now(y: i32, m: u32, d: u32, hour: u32, minute: u32) -> crate::schedule::LocalNow {
    crate::schedule::LocalNow {
        date: chrono::NaiveDate::from_ymd_opt(y, m, d).expect("a valid date"),
        time: crate::schedule::TimeOfDay::new(hour, minute).expect("a valid time of day"),
    }
}
