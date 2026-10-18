use super::*;
use crate::config::default_price_poll_times;
use crate::fixtures::local_now;

#[test]
fn a_restart_late_in_the_day_fetches_once_not_per_missed_anchor() {
    let mut schedule = AnchorSchedule::new(default_price_poll_times());
    let evening = local_now(2026, 3, 1, 18, 0);
    let due = schedule.next_due(&evening).unwrap();
    assert_eq!(due, TimeOfDay::new(17, 0).unwrap());
    schedule.mark_used(&evening, due);
    assert_eq!(schedule.next_due(&evening), None);
}

#[test]
fn the_anchors_reset_on_a_new_local_day() {
    let mut schedule = AnchorSchedule::new(default_price_poll_times());
    let evening = local_now(2026, 3, 1, 18, 0);
    schedule.mark_used(&evening, TimeOfDay::new(17, 0).unwrap());
    assert_eq!(
        schedule.next_due(&local_now(2026, 3, 2, 0, 5)),
        Some(TimeOfDay::new(0, 5).unwrap())
    );
}

const POLICY: RetryPolicy = RetryPolicy {
    delay: Duration::from_secs(10 * 60),
    attempts: 3,
};

fn t(hour: u32, minute: u32) -> TimeOfDay {
    TimeOfDay::new(hour, minute).unwrap()
}

fn key(day: u32, slot: TimeOfDay) -> AnchorKey {
    AnchorKey {
        date: local_now(2026, 3, day, 0, 0).date,
        slot,
    }
}

/// A schedule whose 15:00 anchor fetched at 15:00 and failed.
fn failed_at_15() -> AnchorSchedule {
    let mut schedule = AnchorSchedule::new(default_price_poll_times());
    let now = local_now(2026, 3, 1, 15, 0);
    schedule.mark_used(&local_now(2026, 3, 1, 0, 5), t(0, 5));
    assert_eq!(schedule.decide(&now), Fetch::Anchor(key(1, t(15, 0))));
    assert_eq!(
        schedule.mark_failed(&now, t(15, 0), POLICY),
        Failure::WillRetry
    );
    schedule
}

#[test]
fn nothing_retries_before_any_failure() {
    let schedule = AnchorSchedule::new(default_price_poll_times());
    assert_eq!(
        schedule.decide(&local_now(2026, 3, 1, 0, 0)),
        Fetch::Nothing
    );
    assert_eq!(
        schedule.decide(&local_now(2026, 3, 1, 0, 5)),
        Fetch::Anchor(key(1, t(0, 5)))
    );
}

#[test]
fn a_failure_arms_a_retry_due_after_its_delay() {
    let schedule = failed_at_15();
    assert_eq!(
        schedule.decide(&local_now(2026, 3, 1, 15, 9)),
        Fetch::Nothing
    );
    assert_eq!(
        schedule.decide(&local_now(2026, 3, 1, 15, 10)),
        Fetch::Retry {
            anchor: key(1, t(15, 0)),
            attempt: 1,
            of: 3
        }
    );
}

#[test]
fn attempts_count_up_and_the_last_failure_settles_the_anchor() {
    let mut schedule = failed_at_15();
    for (minute, attempt) in [(10, 1), (20, 2), (30, 3)] {
        let now = local_now(2026, 3, 1, 15, minute);
        assert_eq!(
            schedule.decide(&now),
            Fetch::Retry {
                anchor: key(1, t(15, 0)),
                attempt,
                of: 3
            }
        );
        let outcome = schedule.mark_failed(&now, t(15, 0), POLICY);
        assert_eq!(
            outcome,
            if attempt == 3 {
                Failure::GaveUp
            } else {
                Failure::WillRetry
            }
        );
    }
    assert_eq!(
        schedule.decide(&local_now(2026, 3, 1, 15, 59)),
        Fetch::Nothing
    );
    assert!(schedule.fired().any(|a| a == t(15, 0)));
}

#[test]
fn success_clears_a_pending_retry() {
    let mut schedule = failed_at_15();
    let now = local_now(2026, 3, 1, 15, 10);
    schedule.mark_used(&now, t(15, 0));
    assert_eq!(schedule.decide(&now), Fetch::Nothing);
}

#[test]
fn a_newly_due_anchor_replaces_a_pending_retry() {
    let schedule = failed_at_15();
    assert_eq!(
        schedule.decide(&local_now(2026, 3, 1, 17, 0)),
        Fetch::Anchor(key(1, t(17, 0)))
    );
}

#[test]
fn a_date_rollover_drops_a_pending_retry() {
    let schedule = failed_at_15();
    assert_eq!(
        schedule.decide(&local_now(2026, 3, 2, 0, 0)),
        Fetch::Nothing
    );
}

#[test]
fn a_failure_just_before_midnight_does_not_retry_on_the_next_day() {
    let mut schedule = AnchorSchedule::new(vec![t(23, 55)]);
    let now = local_now(2026, 3, 1, 23, 55);
    schedule.mark_failed(&now, t(23, 55), POLICY);
    assert_eq!(
        schedule.decide(&local_now(2026, 3, 2, 0, 5)),
        Fetch::Nothing
    );
}

#[test]
fn catch_up_after_downtime_still_fetches_once_for_the_latest_anchor() {
    let mut schedule = AnchorSchedule::new(default_price_poll_times());
    let evening = local_now(2026, 3, 1, 18, 0);
    assert_eq!(schedule.decide(&evening), Fetch::Anchor(key(1, t(17, 0))));
    schedule.mark_used(&evening, t(17, 0));
    assert_eq!(schedule.decide(&evening), Fetch::Nothing);
}
