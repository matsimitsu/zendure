//! Locking that cannot take the controller down with it.

/// Takes a poisoned lock rather than panicking through it: these locks sit on
/// the decision path (controller-to-hardware, decision-to-broker), and a
/// panic elsewhere is not a reason for this thread to stop commanding a battery.
pub fn guard<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
