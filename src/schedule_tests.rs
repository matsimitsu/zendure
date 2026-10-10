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
