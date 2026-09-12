//! Counting what had to be thrown away, without becoming the flood.

use std::sync::atomic::{AtomicU64, Ordering};

/// Records one more casualty, reporting the running total on powers of two.
///
/// The throttle is the whole point and it is not obvious, which is why it is a
/// function rather than a fourth copy of four lines: a persistent stall has to
/// be loud, but the thing that stalls is usually the thing that would flood —
/// a dead broker drops a dozen messages per poll, a wedged journal writer one
/// per meter reading. Powers of two are silent enough to live with and dense
/// enough at the start to notice.
///
/// The caller keeps its own sentence. These lines are on watch-day checklists
/// and are grepped for by name, so the message stays where its subject is and
/// only the mechanism is shared.
pub fn tally(counter: &AtomicU64, report: impl FnOnce(u64)) {
    let n = counter.fetch_add(1, Ordering::Relaxed) + 1;
    if n.is_power_of_two() {
        report(n);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_on_powers_of_two_and_stays_quiet_between() {
        let counter = AtomicU64::new(0);
        let mut reported = Vec::new();

        for _ in 0..16 {
            tally(&counter, |n| reported.push(n));
        }

        assert_eq!(reported, vec![1, 2, 4, 8, 16]);
        assert_eq!(counter.load(Ordering::Relaxed), 16);
    }
}
