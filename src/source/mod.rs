//! Where a meter reading comes from, and nothing about what it means.
//!
//! A source adapter owns one meter's wire format: its JSON, its field names,
//! how many phases it has. What leaves this module is a [`MeterObservation`],
//! which carries none of that.

pub mod shelly;
pub mod synthetic;

use crate::config::{Config, MeterConfig};
use crate::device::{PollError, RawCapture};
use crate::registry::{Battery, Devices};
use crate::scan::{Captured, Sampler};
use crate::units::SolarPower;
use crate::world::MeterReading;

/// What any meter source produces, whatever its wire format. Solar belongs
/// here because it's read from the same meter — export on the phase the
/// inverter feeds into, since the meter's total nets that against loads
/// elsewhere. A P1 meter reporting production separately fills the same struct from a
/// different field; nothing downstream changes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeterObservation {
    pub grid: MeterReading,
    pub solar: SolarPower,
}

/// What a read produced, plus the bytes it was parsed from. Separate from
/// [`MeterObservation`] so the observation stays `Copy` and free of anything
/// about how it arrived.
pub struct MeterSample {
    pub observation: MeterObservation,
    /// `None` from a source with no wire format to capture.
    pub raw: Option<RawCapture>,
}

impl Captured for MeterSample {
    fn raw(&self) -> Option<&RawCapture> {
        self.raw.as_ref()
    }
}

/// One meter adapter, in whichever shape it actually is. Not `dyn`-safe, for
/// the reason [`crate::registry::Battery`]'s own doc gives: the futures are
/// `impl Future + Send`.
pub enum Meter {
    Shelly(shelly::ShellyClient),
    Synthetic(synthetic::SyntheticMeter),
}

/// The meter's half of the scan cycle. The per-reading line lives here
/// rather than in the loop: the loop folds a `MeterObservation` and knows
/// nothing about phases.
impl Sampler for Meter {
    type Reading = MeterSample;

    fn id(&self) -> &str {
        match self {
            Meter::Shelly(client) => client.id(),
            Meter::Synthetic(meter) => meter.id(),
        }
    }

    async fn sample(&self) -> Result<MeterSample, PollError> {
        let sample = match self {
            Meter::Shelly(client) => client.sample().await?,
            Meter::Synthetic(meter) => meter.sample().await?,
        };

        let obs = sample.observation;
        tracing::info!(
            "Meter: total={:.0}W (A={:.0} B={:.0} C={:.0}), solar={:.0}W",
            obs.grid.total,
            obs.grid.phases[0],
            obs.grid.phases[1],
            obs.grid.phases[2],
            obs.solar,
        );

        Ok(sample)
    }
}

/// Builds the meter [`run`](crate::run) reads. The only place a
/// [`MeterConfig`] becomes a live adapter, mirroring
/// [`crate::registry::from_config`] on the device side. The synthetic arm
/// reaches back into the registry for the battery it folds into its readings,
/// which is why it takes `devices`; `Config::from_toml_str`'s coherence
/// checks are what make the other arms unreachable.
pub fn from_config(config: &Config, devices: &Devices) -> Meter {
    match &config.meter {
        MeterConfig::Shelly => {
            let Some(shelly) = &config.shelly else {
                unreachable!("Config::from_toml_str requires [shelly] when the meter is Shelly")
            };
            Meter::Shelly(shelly::ShellyClient::new(
                &shelly.ip,
                shelly.solar_phase,
                config.device.poll_interval(),
            ))
        }
        MeterConfig::Synthetic {
            base_load,
            solar_peak,
        } => {
            let battery = match devices.primary() {
                Some((_, Battery::Virtual(battery))) => battery.clone(),
                _ => unreachable!(
                    "Config::from_toml_str requires a virtual device when the meter is synthetic"
                ),
            };
            Meter::Synthetic(synthetic::SyntheticMeter::new(
                synthetic::HouseProfile::new(*base_load, *solar_peak, config.timezone),
                battery,
            ))
        }
    }
}
