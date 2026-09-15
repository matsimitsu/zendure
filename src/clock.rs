use chrono::{Datelike, TimeZone, Timelike, Utc, Weekday};
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

/// The start of `now`'s calendar day in `tz` — what the dashboard's forecast
/// panel and its actual-solar history both bucket against, so a forecast's
/// bars and today's measured production share one x-axis. Falls back to
/// `now` itself if the conversion is ever ambiguous (a DST transition) or
/// fails outright: a mislabelled axis for one render is a display bug, not
/// one worth failing over.
pub fn local_midnight(now: Timestamp, tz: Tz) -> Timestamp {
    let Some(local) = tz.timestamp_millis_opt(now.as_millis()).single() else {
        return now;
    };
    let Some(midnight_naive) = local.date_naive().and_hms_opt(0, 0, 0) else {
        return now;
    };
    let midnight = tz
        .from_local_datetime(&midnight_naive)
        .single()
        .or_else(|| tz.from_local_datetime(&midnight_naive).earliest())
        .unwrap_or(local);
    Timestamp::from(midnight)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_midnight_zeroes_the_time_in_the_configured_timezone() {
        use chrono::TimeZone;
        let tz = chrono_tz::Europe::Amsterdam;
        let afternoon = tz.with_ymd_and_hms(2026, 6, 15, 14, 30, 0).unwrap();
        let now = Timestamp::from(afternoon);

        let midnight = local_midnight(now, tz);

        let local = tz
            .timestamp_millis_opt(midnight.as_millis())
            .single()
            .unwrap();
        assert_eq!(local.date_naive(), afternoon.date_naive());
        assert_eq!((local.hour(), local.minute(), local.second()), (0, 0, 0));
    }

    /// A different timezone must not just shift the wall-clock hour — it has
    /// to move which *day* midnight falls on when the two dates disagree, as
    /// they do here: 01:00 in Tokyo is still the previous day in UTC.
    #[test]
    fn local_midnight_can_fall_on_a_different_calendar_day_than_utc() {
        use chrono::TimeZone;
        let tz = chrono_tz::Asia::Tokyo;
        let early_morning = tz.with_ymd_and_hms(2026, 1, 2, 1, 0, 0).unwrap();
        let now = Timestamp::from(early_morning);
        assert_eq!(
            early_morning.with_timezone(&chrono::Utc).date_naive(),
            chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap(),
            "sanity: this instant must actually straddle midnight UTC"
        );

        let midnight = local_midnight(now, tz);
        let local = tz
            .timestamp_millis_opt(midnight.as_millis())
            .single()
            .unwrap();

        assert_eq!(local.date_naive(), early_morning.date_naive());
        assert_eq!((local.hour(), local.minute()), (0, 0));
    }
}
