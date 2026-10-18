//! When an anchor-driven poller should fetch: the configured local
//! times of day, the poller's own clock read, and which anchors have fired
//! today. Shared by `crate::prediction` and `crate::prices`.

use std::collections::BTreeSet;
use std::time::Duration;

use chrono::{NaiveDate, NaiveDateTime, NaiveTime, TimeDelta, Timelike};
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

    pub fn as_naive_time(self) -> NaiveTime {
        NaiveTime::from_hms_opt(self.hour, self.minute, 0)
            .expect("the constructor bounds hour and minute")
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

impl LocalNow {
    fn naive(&self) -> NaiveDateTime {
        self.date.and_time(self.time.as_naive_time())
    }
}

/// How a poller retries a failed anchor: `attempts` retries, each `delay`
/// after the previous failure. The first fetch of an anchor is not a retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub delay: Duration,
    pub attempts: u8,
}

/// One anchor on one local day: what a fetch is made for, and what a retry
/// has to repeat exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnchorKey {
    pub date: NaiveDate,
    pub slot: TimeOfDay,
}

/// What an anchor-driven poller does on this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fetch {
    Anchor(AnchorKey),
    Retry {
        anchor: AnchorKey,
        attempt: u8,
        of: u8,
    },
    Nothing,
}

/// What a failed fetch left behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// A retry is pending for the anchor.
    WillRetry,
    /// The retries ran out and the anchor is settled without a result, so
    /// the caller can warn.
    GaveUp,
}

#[derive(Debug, Clone, Copy)]
struct PendingRetry {
    anchor: AnchorKey,
    attempt: u8,
    of: u8,
    due: NaiveDateTime,
}

/// Which `poll_times` anchors have settled on the current local day, and the
/// retry pending for a failed one. Shared by every anchor-driven poller; any
/// date mismatch with `now` means the day rolled over and nothing has settled
/// yet.
#[derive(Debug, Clone)]
pub struct AnchorSchedule {
    date: NaiveDate,
    fired: BTreeSet<TimeOfDay>,
    poll_times: Vec<TimeOfDay>,
    // Not persisted: a restart fetches any anchor that isn't settled anyway.
    pending: Option<PendingRetry>,
}

impl AnchorSchedule {
    pub fn new(poll_times: Vec<TimeOfDay>) -> Self {
        AnchorSchedule {
            // Never compared equal to a real `now`, so it reads as "fresh".
            date: NaiveDate::from_ymd_opt(1970, 1, 1).expect("1970-01-01 is a valid date"),
            fired: BTreeSet::new(),
            poll_times,
            pending: None,
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

    /// What to fetch on this tick: a newly due anchor, which supersedes any
    /// pending retry, else a retry whose delay has elapsed.
    pub fn decide(&self, now: &LocalNow) -> Fetch {
        // A failed anchor stays unsettled, so `next_due` keeps naming it; only
        // its retry timer may fetch it again.
        let newly_due = self
            .next_due(now)
            .map(|slot| AnchorKey {
                date: now.date,
                slot,
            })
            .filter(|key| self.pending.is_none_or(|p| p.anchor != *key));
        if let Some(anchor) = newly_due {
            return Fetch::Anchor(anchor);
        }
        match self.pending {
            Some(p) if p.anchor.date == now.date && now.naive() >= p.due => Fetch::Retry {
                anchor: p.anchor,
                attempt: p.attempt,
                of: p.of,
            },
            _ => Fetch::Nothing,
        }
    }

    /// Records that the fetch for `anchor` failed. Arms the next retry, or
    /// settles the anchor once `policy.attempts` retries have failed. A retry
    /// that would fall on the next local day gives up instead, because the
    /// rollover would drop it without a word.
    pub fn mark_failed(
        &mut self,
        now: &LocalNow,
        anchor: AnchorKey,
        policy: RetryPolicy,
    ) -> Failure {
        if anchor.date != now.date {
            // The day rolled over mid-fetch: settling it would wipe today's
            // anchors, and today's own schedule already supersedes it.
            self.pending = None;
            return Failure::GaveUp;
        }
        let attempt = match self.pending {
            Some(p) if p.anchor == anchor => p.attempt.saturating_add(1),
            _ => 1,
        };
        let delay = TimeDelta::from_std(policy.delay).expect("a retry delay fits a TimeDelta");
        let due = now
            .naive()
            .checked_add_signed(delay)
            .filter(|due| due.date() == anchor.date);
        let Some(due) = due.filter(|_| attempt <= policy.attempts) else {
            self.mark_used(now, anchor.slot);
            return Failure::GaveUp;
        };
        self.pending = Some(PendingRetry {
            anchor,
            attempt,
            of: policy.attempts,
            due,
        });
        Failure::WillRetry
    }

    /// Settles `slot` and every earlier anchor today, matching `next_due`'s
    /// catch-up collapse, and clears any pending retry. A poller that retries
    /// calls this only once a fetch has its answer, and `mark_failed` before.
    pub fn mark_used(&mut self, now: &LocalNow, slot: TimeOfDay) {
        self.pending = None;
        if self.date != now.date {
            self.date = now.date;
            self.fired.clear();
        }
        for anchor in self.poll_times.iter().copied().filter(|a| *a <= slot) {
            self.fired.insert(anchor);
        }
    }

    /// The local day the settled anchors belong to.
    pub fn date(&self) -> NaiveDate {
        self.date
    }

    /// The anchors settled on [`AnchorSchedule::date`], in order.
    pub fn fired(&self) -> impl Iterator<Item = TimeOfDay> + '_ {
        self.fired.iter().copied()
    }

    /// Picks up where a previous process left off on `date`.
    pub fn resume(&mut self, date: NaiveDate, fired: impl IntoIterator<Item = TimeOfDay>) {
        self.date = date;
        self.fired = fired.into_iter().collect();
    }
}
