use crate::battery::BatteryState;
use crate::clock::Clock;
use crate::units::{GridPower, SolarPower, Timestamp};

/// Everything the engine can react to, each stamped with the `Clock` in
/// effect when it was observed. Carrying the full clock (not just Gleam's
/// hour+day) is what keeps weekday-dependent behavior replayable once these
/// are journaled (step 5).
#[derive(Debug, Clone)]
pub enum Event {
    GridPower {
        at: Clock,
        total: GridPower,
        solar: SolarPower,
    },
    BatteryUpdate {
        at: Clock,
        state: BatteryState,
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
            Event::GridPower { at, .. } => at.now,
            Event::BatteryUpdate { at, .. } => at.now,
            Event::MqttTimeout { at } => at.now,
        }
    }

    #[allow(dead_code)]
    pub fn kind(&self) -> &'static str {
        match self {
            Event::GridPower { .. } => "grid_power",
            Event::BatteryUpdate { .. } => "battery_update",
            Event::MqttTimeout { .. } => "mqtt_timeout",
        }
    }
}
