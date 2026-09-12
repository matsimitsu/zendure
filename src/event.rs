use crate::clock::Clock;
use crate::units::{SolarPower, Timestamp};
use crate::world::{DeviceId, Measurement, MeterReading};

/// Everything the engine can react to, each stamped with the `Clock` in
/// effect when it was observed. Carrying the full clock (not just Gleam's
/// hour+day) is what keeps weekday-dependent behavior replayable once these
/// are journaled (step 5).
#[derive(Debug, Clone)]
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
    /// Unused until the replay/journal work (steps 4-5); kept alongside the
    /// enum now so the fixture format doesn't need revisiting later.
    #[allow(dead_code)]
    pub fn at(&self) -> Timestamp {
        match self {
            Event::Meter { at, .. } => at.now,
            Event::DeviceUpdate { at, .. } => at.now,
            Event::MqttTimeout { at } => at.now,
        }
    }

    #[allow(dead_code)]
    pub fn kind(&self) -> &'static str {
        match self {
            Event::Meter { .. } => "meter",
            Event::DeviceUpdate { .. } => "device_update",
            Event::MqttTimeout { .. } => "mqtt_timeout",
        }
    }
}
