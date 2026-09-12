//! The Shelly Pro 3EM adapter: its JSON, its three phases, and the arithmetic
//! that turns them into a [`MeterObservation`].
//!
//! Everything Shelly-shaped lives here — the DTO, the phase selector, the
//! `SOLAR_PHASE` parse — so that "a P1 meter is one new file" is a fact about
//! the tree rather than an aspiration. The DTO in particular used to sit in
//! `models.rs` next to the Zendure wire types, which is where a second meter's
//! fields would have landed too.

use serde::Deserialize;

use super::MeterObservation;
use crate::units::{GridPower, SolarPower};
use crate::world::MeterReading;

/// Shelly Pro 3EM energy meter reading, received via MQTT on the status/em:0 topic.
/// Provides signed per-phase and total active power every second.
/// Positive = importing from grid, negative = exporting to grid.
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct ShellyReading {
    /// Phase A active power (W), signed
    pub a_act_power: f64,
    /// Phase B active power (W), signed
    pub b_act_power: f64,
    /// Phase C active power (W), signed
    pub c_act_power: f64,
    /// Total active power across all phases (W), signed
    pub total_act_power: f64,
    /// Phase A voltage (V)
    #[serde(default)]
    pub a_voltage: f64,
    /// Phase B voltage (V)
    #[serde(default)]
    pub b_voltage: f64,
    /// Phase C voltage (V)
    #[serde(default)]
    pub c_voltage: f64,
    /// Phase A current (A)
    #[serde(default)]
    pub a_current: f64,
    /// Phase B current (A)
    #[serde(default)]
    pub b_current: f64,
    /// Phase C current (A)
    #[serde(default)]
    pub c_current: f64,
}

/// Which Shelly Pro 3EM phase the solar inverter (e.g. Huawei Sun2000) feeds
/// into. Solar production is read as the export on that single phase, since the
/// meter's total nets solar export against loads on the other phases.
///
/// It lives with the adapter rather than with `Config` because A/B/C is a fact
/// about this meter, not about the controller: a single-phase P1 meter has no
/// such knob to configure.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SolarPhase {
    A,
    B,
    C,
}

impl SolarPhase {
    pub(crate) fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_uppercase().as_str() {
            "A" => Ok(SolarPhase::A),
            "B" => Ok(SolarPhase::B),
            "C" => Ok(SolarPhase::C),
            _ => Err("SOLAR_PHASE must be one of A, B, or C".to_string()),
        }
    }
}

/// Parse a Shelly Pro 3EM `status/em:0` payload into a normalized observation.
pub fn parse(
    payload: &[u8],
    solar_phase: SolarPhase,
) -> Result<MeterObservation, serde_json::Error> {
    let reading: ShellyReading = serde_json::from_slice(payload)?;

    let phases = [
        GridPower(reading.a_act_power),
        GridPower(reading.b_act_power),
        GridPower(reading.c_act_power),
    ];

    // Solar production = export (negative power) on the phase the inverter
    // feeds into. The meter total nets this against loads on other phases, so
    // read the single phase directly.
    let solar_phase_power = match solar_phase {
        SolarPhase::A => phases[0],
        SolarPhase::B => phases[1],
        SolarPhase::C => phases[2],
    };
    let solar = SolarPower::from_phase_export(solar_phase_power);

    // The total is the meter's own `total_act_power`, never re-summed from the
    // phases: every threshold in the controller was tuned against that number,
    // and the device is not required to make the two agree.
    Ok(MeterObservation {
        grid: MeterReading::new(GridPower(reading.total_act_power), phases),
        solar,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A realistic `status/em:0` frame, voltages and currents included so the
    /// `#[serde(default)]` fields are exercised rather than defaulted. Callers
    /// pick per-phase values that are far enough apart that selecting the wrong
    /// one cannot be mistaken for a rounding difference.
    fn payload(a: f64, b: f64, c: f64, total: f64) -> String {
        format!(
            r#"{{"id":0,"a_act_power":{a},"b_act_power":{b},"c_act_power":{c},
                 "total_act_power":{total},
                 "a_voltage":231.4,"b_voltage":230.1,"c_voltage":232.0,
                 "a_current":1.9,"b_current":10.4,"c_current":0.3}}"#
        )
    }

    #[test]
    fn phase_a_selects_phase_a() {
        let obs = parse(
            payload(-1200.0, 250.0, 70.0, -880.0).as_bytes(),
            SolarPhase::A,
        )
        .unwrap();
        assert_eq!(obs.solar, SolarPower::new(1200.0));
    }

    #[test]
    fn phase_b_selects_phase_b() {
        let obs = parse(
            payload(250.0, -1200.0, 70.0, -880.0).as_bytes(),
            SolarPhase::B,
        )
        .unwrap();
        assert_eq!(obs.solar, SolarPower::new(1200.0));
    }

    #[test]
    fn phase_c_selects_phase_c() {
        let obs = parse(
            payload(250.0, 70.0, -1200.0, -880.0).as_bytes(),
            SolarPhase::C,
        )
        .unwrap();
        assert_eq!(obs.solar, SolarPower::new(1200.0));
        // The other two phases are still carried verbatim, sign and all.
        assert_eq!(
            obs.grid.phases,
            [GridPower(250.0), GridPower(70.0), GridPower(-1200.0)]
        );
    }

    #[test]
    fn an_importing_solar_phase_reads_as_no_production() {
        // Production is read as *export*. A phase drawing 250 W is a load, not
        // an inverter running backwards.
        let obs = parse(
            payload(250.0, -1200.0, 70.0, -880.0).as_bytes(),
            SolarPhase::A,
        )
        .unwrap();
        assert_eq!(obs.solar, SolarPower::ZERO);
    }

    #[test]
    fn negative_zero_on_the_solar_phase_reads_as_zero() {
        // Negating `-0.0` gives `0.0`, which `SolarPower::new`'s `> 0.0` guard
        // rejects — so the published figure is a plain zero rather than a
        // `-0` that would render as "-0W" downstream.
        let obs = parse(payload(-0.0, 100.0, 100.0, 200.0).as_bytes(), SolarPhase::A).unwrap();
        assert_eq!(obs.solar, SolarPower::ZERO);
        assert!(obs.solar.get().is_sign_positive());
    }

    #[test]
    fn total_comes_from_the_meter_not_from_the_phases() {
        // The phases here sum to 300 W; the meter says 1234 W. The meter wins:
        // it is the number the thresholds were tuned against, and a real 3EM
        // does not promise the two agree.
        let obs = parse(
            payload(100.0, 100.0, 100.0, 1234.0).as_bytes(),
            SolarPhase::A,
        )
        .unwrap();
        assert_eq!(obs.grid.total, GridPower(1234.0));
    }

    #[test]
    fn a_malformed_payload_is_an_error_not_a_panic() {
        // The subscriber logs and drops this; it must never take the process
        // down, because a truncated MQTT frame is a routine event.
        assert!(parse(b"{\"a_act_power\":", SolarPhase::A).is_err());
        assert!(parse(b"not json at all", SolarPhase::A).is_err());
    }

    #[test]
    fn solar_phase_parses_case_insensitively_and_rejects_the_rest() {
        assert_eq!(SolarPhase::parse(" a ").unwrap(), SolarPhase::A);
        assert_eq!(SolarPhase::parse("B").unwrap(), SolarPhase::B);
        assert_eq!(SolarPhase::parse("c").unwrap(), SolarPhase::C);
        assert_eq!(
            SolarPhase::parse("D").unwrap_err(),
            "SOLAR_PHASE must be one of A, B, or C"
        );
    }
}
