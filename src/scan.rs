//! The scan cycle's own arithmetic, shared by every adapter that reads a
//! device on a period.
//!
//! What a round costs the requests inside it is a property of the cycle, not
//! of one vendor's wire format, so the meter and the battery derive their
//! HTTP timeouts from the same place.

use std::time::Duration;

/// What [`request_timeout`] derives within: the ceiling bounds how long one
/// unresponsive device can hold the loop, and the floor stays under the
/// shortest period configuration allows, so a request is abandoned before the
/// next round is due.
pub(crate) const MIN_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
pub(crate) const MAX_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Half the period the caller scans at, bounded: a request still in flight
/// when the next round goes out overlaps it, which the Zendure firmware
/// answers with delayed replies and `ECONNRESET`.
pub(crate) fn request_timeout(scan_period: Duration) -> Duration {
    (scan_period / 2).clamp(MIN_REQUEST_TIMEOUT, MAX_REQUEST_TIMEOUT)
}

#[cfg(test)]
#[path = "scan_tests.rs"]
mod tests;
