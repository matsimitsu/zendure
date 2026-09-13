use chrono::{Datelike, Timelike, Utc, Weekday};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

use crate::units::Timestamp;

/// Time context captured at the edge and handed to the controller, so
/// decision logic never reads a clock itself. `now` is wall-clock unix
/// milliseconds, not a monotonic `Instant`, so a recorded event replays
/// identically later; the cost is NTP sensitivity — a backwards step delays a mode
/// change, a forward step permits one slightly early.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Clock {
    /// Wall-clock instant.
    pub now: Timestamp,
    /// Hour of day (0–23) in the configured timezone. Reported in decision
    /// reasons.
    pub hour: u32,
    /// Day of year (1–366) in the configured timezone. Only ever compared for
    /// change, to reset the daily counters at midnight.
    pub day_ordinal: u32,
    /// Weekday in the configured timezone; drives the balance-day max SOC override.
    pub weekday: Weekday,
}

impl Clock {
    /// A clock fixed at `now_ms`, for tests. Tests that care about a
    /// particular hour or weekday say so with struct update syntax —
    /// `Clock { hour: 19, ..Clock::test_at(ms) }` — which makes clear which field a
    /// given test is actually about.
    #[cfg(test)]
    pub(crate) fn test_at(now_ms: i64) -> Self {
        Self {
            now: Timestamp::from_millis(now_ms),
            hour: 12,
            day_ordinal: 100,
            weekday: chrono::Weekday::Wed,
        }
    }

    /// Read the real clock. Called only at the edges — the MQTT handler and the
    /// failsafe timeout — never below them.
    pub fn now(tz: Tz) -> Self {
        let utc = Utc::now();
        let local = utc.with_timezone(&tz);
        Self {
            now: utc.into(),
            hour: local.hour(),
            day_ordinal: local.ordinal(),
            weekday: local.weekday(),
        }
    }
}
