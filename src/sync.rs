//! Locking that cannot take the controller down with it.

/// Takes a poisoned lock rather than panicking through it.
///
/// Every critical section behind this is a small map or cache update with no
/// panic in it, so poisoning should be unreachable. But these locks sit on the
/// decision path — between the controller and the hardware, or between a
/// decision and the broker — and nothing on that path is permitted to be the
/// thing that kills an unattended controller. A poisoned mutex means some other
/// thread panicked; it is not a reason for this one to stop commanding a
/// battery.
pub fn guard<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
