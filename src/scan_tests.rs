//! Tests for `scan.rs`.

use super::*;
use crate::config::Config;

/// Catches a timeout that would leave a request in flight when the next round
/// starts, and one that would hold the loop for longer than the fixed ceiling
/// however long the period grows.
#[test]
fn a_request_gives_up_before_the_next_poll_and_within_a_fixed_bound() {
    for secs in [3, 4, 5, 9, 10, 11, 60, 150, 3_600, 86_400] {
        let period = Duration::from_secs(secs);
        let timeout = request_timeout(period);

        assert!(timeout < period, "{period:?} derived {timeout:?}");
        assert!(
            timeout <= MAX_REQUEST_TIMEOUT,
            "{period:?} derived {timeout:?}"
        );
    }
}

/// Catches a change to the timeout the deployed period derives, which is the
/// one that reaches the box.
#[test]
fn the_shipped_poll_period_derives_a_five_second_timeout() {
    let (config, _) = Config::from_toml_str(include_str!("../config.example.toml"))
        .expect("the example config parses");

    assert_eq!(
        request_timeout(config.device.poll_interval()),
        Duration::from_secs(5)
    );
}
