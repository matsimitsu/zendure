//! What each battery pack did per fifteen minutes: the SOC and temperature it
//! ranged over and the energy that went in and out of it.
//!
//! Folded from the packs a poll reported, live from the reading and on a
//! restart from the journalled `zendure_poll` captures re-parsed, so both
//! paths go through [`PackTrace::observe`].

use std::collections::BTreeMap;
use std::time::Duration;

use crate::device::PackStatus;
use crate::units::{BatteryPower, DeciKelvin, Soc, Timestamp, WattHours};
use crate::world::DeviceId;

/// Which pack within its box. The serial where the pack reports one; its
/// position in the report otherwise, which is stable only while nothing is
/// added or removed.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PackId {
    Serial(String),
    Position(usize),
}

/// One pack, across every box this process drives.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PackKey {
    pub device: DeviceId,
    pub pack: PackId,
}

/// The lowest and highest of a level seen over a span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent<T> {
    pub min: T,
    pub max: T,
}

impl<T: Ord + Copy> Extent<T> {
    fn of(value: T) -> Self {
        Extent {
            min: value,
            max: value,
        }
    }

    fn widen(extent: Option<Self>, other: Option<Self>) -> Option<Self> {
        match (extent, other) {
            (Some(a), Some(b)) => Some(Extent {
                min: a.min.min(b.min),
                max: a.max.max(b.max),
            }),
            (a, b) => a.or(b),
        }
    }
}

/// One pack over one span. A level is `None` where the pack never reported
/// it; energy is zero where no two consecutive flow readings fell close
/// enough together to integrate.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PackInterval {
    pub soc: Option<Extent<Soc>>,
    pub temp: Option<Extent<DeciKelvin>>,
    pub charged: WattHours,
    pub discharged: WattHours,
}

impl PackInterval {
    /// Several spans as one, for a figure over a longer window than a bucket.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn combined(spans: impl IntoIterator<Item = PackInterval>) -> Option<PackInterval> {
        spans.into_iter().reduce(|a, b| PackInterval {
            soc: Extent::widen(a.soc, b.soc),
            temp: Extent::widen(a.temp, b.temp),
            charged: a.charged + b.charged,
            discharged: a.discharged + b.discharged,
        })
    }
}

/// The per-pack figures of one bucket.
pub(crate) type PackIntervals = BTreeMap<PackKey, PackInterval>;

/// A gap wider than this between two flow readings is an outage or a
/// restart: integrating across it would invent hours of constant flow.
const MAX_FLOW_GAP: Duration = Duration::from_secs(15 * 60);

/// What one reading changes in a pack's bucket.
pub(crate) struct PackSample {
    key: PackKey,
    soc: Option<Soc>,
    temp: Option<DeciKelvin>,
    charged: WattHours,
    discharged: WattHours,
}

impl PackSample {
    pub(crate) fn key(&self) -> &PackKey {
        &self.key
    }

    pub(crate) fn apply(&self, interval: &mut PackInterval) {
        interval.soc = Extent::widen(interval.soc, self.soc.map(Extent::of));
        interval.temp = Extent::widen(interval.temp, self.temp.map(Extent::of));
        interval.charged = interval.charged + self.charged;
        interval.discharged = interval.discharged + self.discharged;
    }
}

/// Each pack's last flow reading, which the next one integrates against.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct PackTrace {
    last_flow: BTreeMap<PackKey, (Timestamp, BatteryPower)>,
}

impl PackTrace {
    /// The samples one poll's packs contribute, with the energy since each
    /// pack's previous flow reading. A pack reporting nothing contributes
    /// nothing, so a simulated box's capacity-only packs leave no rows.
    pub(crate) fn observe(
        &mut self,
        device: &DeviceId,
        at: Timestamp,
        packs: &[PackStatus],
    ) -> Vec<PackSample> {
        packs
            .iter()
            .enumerate()
            .filter(|(_, pack)| pack.soc.is_some() || pack.temp.is_some() || pack.power.is_some())
            .map(|(position, pack)| {
                let key = PackKey {
                    device: device.clone(),
                    pack: pack
                        .serial
                        .clone()
                        .map_or(PackId::Position(position), PackId::Serial),
                };
                let (charged, discharged) = match pack.power {
                    Some(power) => self.integrate(&key, at, power),
                    None => (WattHours::ZERO, WattHours::ZERO),
                };
                PackSample {
                    key,
                    soc: pack.soc,
                    temp: pack.temp,
                    charged,
                    discharged,
                }
            })
            .collect()
    }

    fn integrate(
        &mut self,
        key: &PackKey,
        at: Timestamp,
        power: BatteryPower,
    ) -> (WattHours, WattHours) {
        let previous = self.last_flow.insert(key.clone(), (at, power));
        let span = previous.and_then(|(then, before)| {
            // Negative after a backwards clock step: no span to integrate.
            let dt = Duration::from_millis(u64::try_from((at - then).as_millis()).ok()?);
            (dt <= MAX_FLOW_GAP).then_some((dt, before))
        });
        match span {
            Some((dt, before)) => (
                WattHours::integrate(before.charging(), power.charging(), dt),
                WattHours::integrate(before.discharging(), power.discharging(), dt),
            ),
            None => (WattHours::ZERO, WattHours::ZERO),
        }
    }
}
