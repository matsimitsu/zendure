use serde::{Deserialize, Serialize};

use crate::clock::Clock;
use crate::units::{SolarPower, Timestamp};
use crate::world::{DeviceId, Measurement, MeterReading};

/// Everything the engine can react to, each stamped with the `Clock` in
/// effect when it was observed. Carrying the full clock (not just Gleam's
/// hour+day) is what keeps weekday-dependent behavior replayable once these
/// are journaled (step 7).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    Meter {
        at: Clock,
        grid: MeterReading,
        solar: SolarPower,
    },
    /// A device told us about itself. Addressed by `id` rather than implied by
    /// the variant, so a second battery — or the first charger — is a new entry
    /// in the world's device map and not a new arm in this enum.
    DeviceUpdate {
        at: Clock,
        id: DeviceId,
        measurement: Measurement,
    },
    MqttTimeout {
        at: Clock,
    },
}

impl Event {
    /// Fills the journal's indexed `ts_ms` column — the event's own observed
    /// time, not the moment it happened to be written.
    pub fn at(&self) -> Timestamp {
        match self {
            Event::Meter { at, .. } => at.now,
            Event::DeviceUpdate { at, .. } => at.now,
            Event::MqttTimeout { at } => at.now,
        }
    }

    /// Fills the journal's `kind` column. Kept in step with the serde tag by
    /// `the_serde_tag_agrees_with_kind`, since the journal uses both.
    pub fn kind(&self) -> &'static str {
        match self {
            Event::Meter { .. } => "meter",
            Event::DeviceUpdate { .. } => "device_update",
            Event::MqttTimeout { .. } => "mqtt_timeout",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::battery::BatteryState;
    use crate::units::{BatteryPower, GridPower};

    const NOW_MS: i64 = 1_757_000_000_000;

    fn clock() -> Clock {
        Clock {
            hour: 19,
            day_ordinal: 255,
            ..Clock::test_at(NOW_MS)
        }
    }

    fn meter() -> Event {
        Event::Meter {
            at: clock(),
            grid: MeterReading::new(
                GridPower(150.5),
                [GridPower(10.0), GridPower(-200.0), GridPower(340.5)],
            ),
            solar: SolarPower::new(200.0),
        }
    }

    fn device_update() -> Event {
        Event::DeviceUpdate {
            at: clock(),
            id: DeviceId::new("SN123"),
            measurement: Measurement::Battery(BatteryState {
                current_power: BatteryPower(-300),
                ..BatteryState::test_sample()
            }),
        }
    }

    fn every_variant() -> [Event; 3] {
        [meter(), device_update(), Event::MqttTimeout { at: clock() }]
    }

    /// An event that cannot be read back is not a journal entry, it's a log
    /// line. Step 8 replays these, so every variant has to survive the trip —
    /// including `DeviceUpdate`, whose payload is a whole `Measurement`.
    #[test]
    fn every_variant_round_trips_through_json() {
        for event in every_variant() {
            let json = serde_json::to_string(&event).unwrap();
            let back: Event = serde_json::from_str(&json).unwrap();
            assert_eq!(event, back, "round trip failed for {json}");
        }
    }

    /// `kind()` and the serde tag are two sources of truth for one string, and
    /// the journal uses both — `kind()` fills the indexed `kind` column, the tag
    /// lands inside `payload_json`. If they drift, a query by column silently
    /// stops agreeing with the payloads it returns.
    #[test]
    fn the_serde_tag_agrees_with_kind() {
        for event in every_variant() {
            let json = serde_json::to_value(&event).unwrap();
            assert_eq!(json["kind"], event.kind());
        }
    }

    /// The strings themselves. Renaming a variant would otherwise rewrite the
    /// journal's vocabulary without failing anything.
    #[test]
    fn kind_strings_are_pinned() {
        assert_eq!(meter().kind(), "meter");
        assert_eq!(device_update().kind(), "device_update");
        assert_eq!(Event::MqttTimeout { at: clock() }.kind(), "mqtt_timeout");
    }

    /// `at()` reads through to the clock on every variant — it is what fills the
    /// journal's indexed `ts_ms` column.
    #[test]
    fn at_reads_the_clock_for_every_variant() {
        for event in every_variant() {
            assert_eq!(event.at(), Timestamp::from_millis(NOW_MS));
        }
    }

    /// `Clock` carries a `chrono::Weekday`, which has no serde impl unless
    /// chrono's `serde` feature is on — this is the test that fails if that
    /// feature is ever dropped. `weekday` is not cosmetic: it drives the
    /// balance-day max-SOC override, so a clock that loses it replays wrong.
    #[test]
    fn clock_round_trips_including_weekday() {
        let json = serde_json::to_string(&clock()).unwrap();
        assert_eq!(
            json,
            r#"{"now":1757000000000,"hour":19,"day_ordinal":255,"weekday":"Wed"}"#
        );
        assert_eq!(clock(), serde_json::from_str(&json).unwrap());
    }
}
