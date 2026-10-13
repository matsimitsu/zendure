//! The device's own charge ceiling (`socSet`), kept in step with the
//! controller's max SOC for the day.
//!
//! A Zendure pack stops charging at whatever `socSet` it holds and reports
//! `socLimit: 1`, which the controller honours. So a ceiling the controller
//! raises (the balance day's 100%) is only reachable once the device has been
//! told, and told again if it comes back holding something else, as a pack
//! that restarts onto its flash value does. Which device needs a write, and
//! when, is decided here without I/O; [`sync`] is the shell that sends it.

use std::collections::BTreeMap;

use crate::registry::Devices;
use crate::units::{Elapsed, Soc, Timestamp};
use crate::world::DeviceId;

/// How long a failed write, or one the device doesn't report back, waits
/// before it is tried again: soon enough that the ceiling is not stale for the
/// day, not so soon that an unreachable box, or one that clamps the value,
/// gets a request per tick (CONTROL-2).
const RETRY_AFTER: Elapsed = Elapsed::MINUTE;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Held {
    /// Nothing has landed since startup: the device may hold anything.
    Unknown,
    Written {
        target: Soc,
        at: Timestamp,
    },
    Failed {
        target: Soc,
        at: Timestamp,
    },
}

#[derive(Debug, Clone, Copy)]
struct Ceiling {
    held: Held,
    /// What the device last said it holds; `None` until a poll reports it.
    reported: Option<Soc>,
}

impl Ceiling {
    fn due(self, target: Soc, now: Timestamp) -> bool {
        match self.held {
            Held::Unknown => true,
            // The device can undo a write that landed, so its report outranks
            // the record. A poll sampled before the write disagrees too, hence
            // the pause rather than a write on the next tick.
            Held::Written { target: wrote, at } => {
                wrote != target
                    || (self.reported.is_some_and(|soc| soc != target) && now - at >= RETRY_AFTER)
            }
            Held::Failed { target: tried, at } => tried != target || now - at >= RETRY_AFTER,
        }
    }
}

/// What each device's ceiling was last set to and last reported as, for the
/// devices that have one.
pub struct CeilingSync {
    ceilings: BTreeMap<DeviceId, Ceiling>,
}

impl CeilingSync {
    pub fn new(devices: impl IntoIterator<Item = DeviceId>) -> Self {
        let unknown = Ceiling {
            held: Held::Unknown,
            reported: None,
        };
        CeilingSync {
            ceilings: devices.into_iter().map(|id| (id, unknown)).collect(),
        }
    }

    /// The devices whose ceiling should be written to `target` now.
    pub fn due(&self, target: Soc, now: Timestamp) -> Vec<DeviceId> {
        self.ceilings
            .iter()
            .filter(|(_, ceiling)| ceiling.due(target, now))
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Folds in the ceiling a poll found the device holding.
    pub fn observe(&mut self, id: &DeviceId, reported: Soc) {
        if let Some(ceiling) = self.ceilings.get_mut(id) {
            ceiling.reported = Some(reported);
        }
    }

    pub fn record(&mut self, id: &DeviceId, target: Soc, now: Timestamp, landed: bool) {
        if let Some(ceiling) = self.ceilings.get_mut(id) {
            ceiling.held = if landed {
                Held::Written { target, at: now }
            } else {
                Held::Failed { target, at: now }
            };
        }
    }
}

/// Writes `target` to every device [`CeilingSync::due`] names. Best-effort: a
/// failure is logged and left for the retry, never fatal to the tick.
pub async fn sync(devices: &Devices, ceilings: &mut CeilingSync, target: Soc, now: Timestamp) {
    for id in ceilings.due(target, now) {
        let Some(battery) = devices.battery(&id) else {
            continue;
        };
        let landed = match battery.write_charge_ceiling(target).await {
            Ok(()) => {
                tracing::info!("Set {id}'s charge ceiling (socSet) to {target}%");
                true
            }
            Err(e) => {
                tracing::warn!("Failed to set {id}'s charge ceiling (socSet) to {target}%: {e}");
                false
            }
        };
        ceilings.record(&id, target, now, landed);
    }
}

#[cfg(test)]
#[path = "ceiling_tests.rs"]
mod tests;
