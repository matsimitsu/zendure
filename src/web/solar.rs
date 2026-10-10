//! Pure figures over a day's half-hour solar series. The series is 46, 48 or 50
//! slots long (DST days), so nothing here assumes 48.
//!
//! The `dead_code` allows below go when the forecast panel, which consumes
//! these, lands in the next ticket.

use crate::units::{KiloWattHours, SolarPower};

/// A slot's power is its mean over the slot, so it is held for this long.
#[allow(dead_code)] // used by `kwh`, itself unused until the panel exists
const SLOT_HOURS: f64 = 0.5;

/// Energy over the slots: each slot's mean power held for half an hour.
#[allow(dead_code)] // consumed by the forecast panel, not yet written
pub(super) fn kwh(slots: &[SolarPower]) -> KiloWattHours {
    let watts: f64 = slots.iter().map(|p| p.get()).sum();
    KiloWattHours(watts * SLOT_HOURS / 1000.0)
}

/// Actual against forecast as a whole percent. `None` when the forecast is
/// zero, where a ratio says nothing.
#[allow(dead_code)] // consumed by the forecast panel, not yet written
pub(super) fn delta_pct(actual: KiloWattHours, forecast: KiloWattHours) -> Option<i32> {
    if forecast.get() == 0.0 {
        return None;
    }
    Some(((actual.get() - forecast.get()) / forecast.get() * 100.0).round() as i32)
}

/// `+N%`, `−N%` or `±0%`. The minus is U+2212 so it lines up with the plus in
/// the mono readout.
#[allow(dead_code)] // consumed by the forecast panel, not yet written
pub(super) fn format_delta(pct: i32) -> String {
    match pct {
        0 => "±0%".to_owned(),
        p if p > 0 => format!("+{p}%"),
        p => format!("\u{2212}{}%", p.unsigned_abs()),
    }
}

/// Index of the first maximum, so a flat-topped day peaks at its earliest slot.
/// An empty or all-zero series gives 0.
#[allow(dead_code)] // consumed by the forecast panel, not yet written
pub(super) fn peak_slot(slots: &[SolarPower]) -> usize {
    let mut best = 0;
    for (i, p) in slots.iter().enumerate() {
        if *p > slots[best] {
            best = i;
        }
    }
    best
}

/// Index of the last slot with any production; `None` on a day without sun.
#[allow(dead_code)] // consumed by the forecast panel, not yet written
pub(super) fn last_sun_slot(slots: &[SolarPower]) -> Option<usize> {
    slots.iter().rposition(|p| *p > SolarPower::ZERO)
}

#[cfg(test)]
#[path = "solar_tests.rs"]
mod tests;
