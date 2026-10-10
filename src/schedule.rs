//! When an anchor-driven poller should fetch: the configured local
//! times of day, the poller's own clock read, and which anchors have fired
//! today. Shared by `crate::prediction` and `crate::prices`.

use std::collections::BTreeSet;

use chrono::{NaiveDate, Timelike};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

#[cfg(test)]
#[path = "schedule_tests.rs"]
mod tests;

/// A local time-of-day anchor (hour, minute) — an entry in `poll_times`, and
/// what `fired` remembers having used today. Hand-written `"HH:MM"`
/// `Serialize`/`Deserialize` so the TOML config and the persisted state's
/// `fired` list share one representation. `Ord`/`Eq` derived so the scheduler
/// can compare anchors and a `BTreeSet` can hold them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TimeOfDay {
    hour: u32,
    minute: u32,
}

impl TimeOfDay {
    pub fn new(hour: u32, minute: u32) -> Result<Self, String> {
        if hour >= 24 {
            return Err(format!("hour must be 0-23, found {hour}"));
        }
        if minute >= 60 {
            return Err(format!("minute must be 0-59, found {minute}"));
        }
        Ok(TimeOfDay { hour, minute })
    }
}

impl std::fmt::Display for TimeOfDay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:02}:{:02}", self.hour, self.minute)
    }
}

impl std::str::FromStr for TimeOfDay {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let (h, m) = s
            .split_once(':')
            .ok_or_else(|| format!("must be \"HH:MM\", found {s:?}"))?;
        let hour = h
            .parse::<u32>()
            .map_err(|_| format!("must be \"HH:MM\", found {s:?}"))?;
        let minute = m
            .parse::<u32>()
            .map_err(|_| format!("must be \"HH:MM\", found {s:?}"))?;
        TimeOfDay::new(hour, minute)
    }
}

impl Serialize for TimeOfDay {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for TimeOfDay {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// A poller's own clock read — deliberately not `crate::clock::Clock`,
/// which only resolves the hour (0-23) and is journalled/replayed
/// byte-for-byte; extending it with minutes would touch every journalled
/// `Event`. Read once per check, at the poller's own edge; every scheduling
/// decision takes this as a plain argument, so it stays pure and testable
/// with no wall clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalNow {
    pub date: NaiveDate,
    pub time: TimeOfDay,
}

impl LocalNow {
    pub fn now(tz: Tz) -> Self {
        let local = chrono::Utc::now().with_timezone(&tz);
        LocalNow {
            date: local.date_naive(),
            time: TimeOfDay::new(local.hour(), local.minute())
                .expect("chrono's own hour()/minute() are always in range"),
        }
    }
}

/// Which `poll_times` anchors have fired on the current local day. Shared by
/// every anchor-driven poller; any date mismatch with `now` means the day
/// rolled over and nothing has fired yet.
#[derive(Debug, Clone)]
pub struct AnchorSchedule {
    date: NaiveDate,
    fired: BTreeSet<TimeOfDay>,
    poll_times: Vec<TimeOfDay>,
}

impl AnchorSchedule {
    pub fn new(poll_times: Vec<TimeOfDay>) -> Self {
        AnchorSchedule {
            // Never compared equal to a real `now`, so it reads as "fresh".
            date: NaiveDate::from_ymd_opt(1970, 1, 1).expect("1970-01-01 is a valid date"),
            fired: BTreeSet::new(),
            poll_times,
        }
    }

    /// The latest anchor at-or-before `now.time` that hasn't fired today.
    /// Returning the *latest* rather than the earliest collapses a catch-up
    /// after downtime into a single fetch.
    pub fn next_due(&self, now: &LocalNow) -> Option<TimeOfDay> {
        let fresh_day = self.date != now.date;
        self.poll_times
            .iter()
            .rev()
            .find(|slot| **slot <= now.time && (fresh_day || !self.fired.contains(slot)))
            .copied()
    }

    /// Records `slot` and every earlier anchor as used today, matching
    /// `next_due`'s catch-up collapse. Called after a failed fetch too.
    pub fn mark_used(&mut self, now: &LocalNow, slot: TimeOfDay) {
        if self.date != now.date {
            self.date = now.date;
            self.fired.clear();
        }
        for anchor in self.poll_times.iter().copied().filter(|a| *a <= slot) {
            self.fired.insert(anchor);
        }
    }

    /// The local day the fired anchors belong to.
    pub fn date(&self) -> NaiveDate {
        self.date
    }

    /// The anchors used on [`AnchorSchedule::date`], in order.
    pub fn fired(&self) -> impl Iterator<Item = TimeOfDay> + '_ {
        self.fired.iter().copied()
    }

    /// Picks up where a previous process left off on `date`.
    pub fn resume(&mut self, date: NaiveDate, fired: impl IntoIterator<Item = TimeOfDay>) {
        self.date = date;
        self.fired = fired.into_iter().collect();
    }
}
