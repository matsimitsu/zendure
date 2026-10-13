//! The device's own charge ceiling (`socSet`), kept in step with the
//! controller's max SOC for the day.
//!
//! A Zendure pack stops charging at whatever `socSet` it holds and reports
//! `socLimit: 1`, which the controller honours. So a ceiling the controller
//! raises (the balance day's 100%) is only reachable once the device has been
//! told. Which device needs a write, and when, is decided here without I/O;
//! [`sync`] is the shell that sends it.

use std::collections::BTreeMap;

use crate::registry::Devices;
use crate::units::{Elapsed, Soc, Timestamp};
use crate::world::DeviceId;

/// How long a failed write waits before it is tried again: soon enough that
/// the ceiling is not stale for the day, not so soon that an unreachable box
/// gets a request per tick (CONTROL-2).
const RETRY_AFTER: Elapsed = Elapsed::MINUTE;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Held {
    /// Nothing has landed since startup: the device may hold anything.
    Unknown,
    Written(Soc),
    Failed {
        target: Soc,
        at: Timestamp,
    },
}

/// What each device's ceiling was last set to, for the devices that have one.
pub struct CeilingSync {
    held: BTreeMap<DeviceId, Held>,
}

impl CeilingSync {
    pub fn new(devices: impl IntoIterator<Item = DeviceId>) -> Self {
        CeilingSync {
            held: devices.into_iter().map(|id| (id, Held::Unknown)).collect(),
        }
    }

    /// The devices whose ceiling should be written to `target` now.
    pub fn due(&self, target: Soc, now: Timestamp) -> Vec<DeviceId> {
        self.held
            .iter()
            .filter(|(_, held)| match **held {
                Held::Unknown => true,
                Held::Written(soc) => soc != target,
                Held::Failed { target: tried, at } => tried != target || now - at >= RETRY_AFTER,
            })
            .map(|(id, _)| id.clone())
            .collect()
    }

    pub fn record(&mut self, id: &DeviceId, target: Soc, now: Timestamp, landed: bool) {
        if let Some(held) = self.held.get_mut(id) {
            *held = if landed {
                Held::Written(target)
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
