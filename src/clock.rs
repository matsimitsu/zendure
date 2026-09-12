use chrono::{Datelike, Timelike, Utc, Weekday};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

use crate::units::Timestamp;

/// Time context captured at the edge and handed to the controller, so the
/// decision logic never reads a clock itself. Everything the controller needs to
/// know about "when" lives here.
///
/// `now` is wall-clock unix milliseconds rather than a monotonic `Instant`
/// because a recorded event has to replay identically later, which a
/// process-relative counter can't do. The cost is NTP sensitivity: a backwards
/// step makes an elapsed comparison read as "not yet elapsed", delaying a mode
/// change until time catches up. A forward step permits one slightly early.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Clock {
    /// Wall-clock instant.
    pub now: Timestamp,
    /// Hour of day (0–23) in the configured timezone. Reported in decision
    /// reasons; no longer used to gate any behavior.
    pub hour: u32,
    /// Day of year (1–366) in the configured timezone. Only ever compared for
    /// change, to reset the daily counters at midnight.
    pub day_ordinal: u32,
    /// Weekday in the configured timezone; drives the balance-day max SOC override.
    pub weekday: Weekday,
}

impl Clock {
    /// A clock fixed at `now_ms`, for tests.
    ///
    /// `pub(crate)` and living next to the type for the same reason
    /// `Controller::test_default` does: four test modules were each writing this
    /// literal out, and two of them byte-identically. Tests that care about a
    /// particular hour or weekday say so with struct update syntax —
    /// `Clock { hour: 19, ..Clock::test_at(ms) }` — which also makes it obvious
    /// which field a given test is actually about.
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
            now: Timestamp::from_millis(utc.timestamp_millis()),
            hour: local.hour(),
            day_ordinal: local.ordinal(),
            weekday: local.weekday(),
        }
    }
}
