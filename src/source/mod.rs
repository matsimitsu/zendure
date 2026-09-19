//! Where a meter reading comes from, and nothing about what it means.
//!
//! A source adapter owns one meter's wire format: its JSON, its field names,
//! how many phases it has. What leaves this module is a [`MeterObservation`],
//! which carries none of that.

pub mod shelly;
pub mod synthetic;

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::config::{Config, MeterConfig};
use crate::device::{PollError, RawCapture};
use crate::journal::Journal;
use crate::mqtt::MqttEvent;
use crate::registry::{Battery, Devices};
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

/// One meter reading, pulled on request. Mirrors
/// [`crate::device::BatteryMonitor`]'s shape, and is not `dyn`-safe for the
/// same reason — hence [`Meter`].
pub trait MeterSource {
    fn sample(&self) -> impl Future<Output = Result<MeterSample, PollError>> + Send;
}

/// One meter adapter, in whichever shape it actually is.
pub enum Meter {
    Shelly(shelly::ShellyClient),
    Synthetic(synthetic::SyntheticMeter),
}

impl MeterSource for Meter {
    async fn sample(&self) -> Result<MeterSample, PollError> {
        match self {
            Meter::Shelly(client) => client.sample().await,
            Meter::Synthetic(meter) => meter.sample().await,
        }
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

/// How often the meter is read. The Shelly refreshes its own registers at
/// about this rate, so a faster pull re-reads a figure that has not moved.
const SAMPLE_PERIOD: Duration = Duration::from_secs(1);

/// Reads `meter` on [`SAMPLE_PERIOD`] and offers each observation to the
/// coordinator. A failed read is journalled, logged and skipped: the loop's
/// own failsafe is what a meter that stays down trips, and a task that
/// returned on the first timeout would never see it come back. If `tx.send`
/// fails the coordinator is gone, so there is nothing left to feed.
pub async fn run_meter_feed(meter: Meter, tx: mpsc::Sender<MqttEvent>, journal: Arc<Journal>) {
    let mut ticker = tokio::time::interval(SAMPLE_PERIOD);

    loop {
        ticker.tick().await;

        // Captured whichever way the read went: a reading that failed to
        // decode is exactly the one worth having on record.
        match meter.sample().await {
            Ok(sample) => {
                if let Some(raw) = &sample.raw {
                    journal.raw(raw.kind, &raw.body);
                }

                let obs = sample.observation;
                tracing::info!(
                    "Meter: total={:.0}W (A={:.0} B={:.0} C={:.0}), solar={:.0}W",
                    obs.grid.total,
                    obs.grid.phases[0],
                    obs.grid.phases[1],
                    obs.grid.phases[2],
                    obs.solar,
                );

                if tx.send(MqttEvent::Meter(obs)).await.is_err() {
                    tracing::info!("Meter feed: coordinator gone, stopping");
                    return;
                }
            }
            Err(e) => {
                if let Some(raw) = &e.raw {
                    journal.raw(raw.kind, &raw.body);
                }
                tracing::warn!("Failed to read the meter: {}", e.error);
            }
        }
    }
}
