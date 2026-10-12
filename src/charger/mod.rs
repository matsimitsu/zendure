//! An EV charger, read each scan round like a battery and folded into
//! `World` as a [`ChargerState`].
//!
//! Read side only: [`ChargerMonitor`] is what an adapter implements, the way
//! [`crate::device::BatteryMonitor`] is for a battery. `[charger] kind`
//! selects the adapter, and [`from_config`] is the only place that happens.

pub mod simulated;
pub mod state;
pub mod vestel;

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use chrono_tz::Tz;

use crate::config::{ChargerConfig, ChargerKind};
use crate::device::{PollError, RawCapture};
use crate::scan::{Captured, Sampler};
use crate::units::WattHours;
use crate::world::DeviceId;

pub use state::ChargerState;

/// Everything a poll produces that no decision reads.
#[derive(Debug, Default)]
pub struct ChargerTelemetry {
    /// The charger's lifetime meter, `None` from an adapter without one.
    pub total_energy: Option<WattHours>,
}

/// One reading, mirroring [`crate::device::BatteryReading`]: only `state`
/// is journalled as an event; `raw` is captured before anything parses it.
#[derive(Debug)]
pub struct ChargerReading {
    pub state: ChargerState,
    pub telemetry: ChargerTelemetry,
    pub raw: Option<RawCapture>,
}

impl Captured for ChargerReading {
    fn raw(&self) -> Option<&RawCapture> {
        self.raw.as_ref()
    }
}

/// The read half of a charger adapter. Same `&self` / `-> impl Future + Send`
/// shape as [`crate::device::BatteryMonitor`], and every failure is a
/// [`PollError`] so its raw bytes are journalled.
pub trait ChargerMonitor {
    /// The charger's identity in the world and the journal.
    fn id(&self) -> &DeviceId;

    fn poll(&self) -> impl Future<Output = Result<ChargerReading, PollError>> + Send;
}

/// One charger adapter, in whichever shape it actually is. An enum for the
/// reason [`crate::registry::Battery`]'s module doc gives: RPITIT futures
/// cannot sit behind a `dyn`. `Simulated` holds an `Arc` so the synthetic
/// meter can share the same car, as `Battery::Virtual` does.
pub enum Charger {
    Simulated(Arc<simulated::SimulatedCharger>),
    Vestel(vestel::VestelClient),
    /// A charger that never answers, for the outage tests.
    #[cfg(test)]
    Unreachable(DeviceId),
    /// A charger whose adapter panics mid-poll, ending its sampler task.
    #[cfg(test)]
    Panicking(DeviceId),
}

impl ChargerMonitor for Charger {
    fn id(&self) -> &DeviceId {
        match self {
            Charger::Simulated(charger) => charger.id(),
            Charger::Vestel(charger) => charger.id(),
            #[cfg(test)]
            Charger::Unreachable(id) | Charger::Panicking(id) => id,
        }
    }

    async fn poll(&self) -> Result<ChargerReading, PollError> {
        match self {
            Charger::Simulated(charger) => charger.poll().await,
            Charger::Vestel(charger) => charger.poll().await,
            #[cfg(test)]
            Charger::Unreachable(id) => Err(PollError {
                raw: None,
                error: format!("{id} is unreachable"),
            }),
            #[cfg(test)]
            Charger::Panicking(id) => panic!("{id} panicked mid-poll"),
        }
    }
}

/// The charger's half of the scan cycle.
impl Sampler for Charger {
    type Reading = ChargerReading;

    fn id(&self) -> &str {
        ChargerMonitor::id(self).as_str()
    }

    async fn sample(&self) -> Result<ChargerReading, PollError> {
        let reading = ChargerMonitor::poll(self).await?;

        let state = &reading.state;
        tracing::debug!(
            "Charger: {:?}, cable {:?}, {}W on {:?}, session {:?}, lifetime {:?}",
            state.status,
            state.cable,
            state.power,
            state.phases,
            state.session_energy,
            reading.telemetry.total_energy,
        );

        Ok(reading)
    }
}

/// Builds the charger `run` samples. `Config::from_toml_str`'s coherence
/// checks are what make every combination reaching here a valid one.
/// `scan_period` is the round a real adapter's request timeout derives from.
pub fn from_config(config: &ChargerConfig, timezone: Tz, scan_period: Duration) -> Charger {
    match &config.kind {
        ChargerKind::Vestel {
            host,
            port,
            unit_id,
        } => Charger::Vestel(vestel::VestelClient::new(
            config.id.clone(),
            host.clone(),
            *port,
            *unit_id,
            scan_period,
        )),
        ChargerKind::Simulated {
            plug_in,
            unplug,
            current,
            phases,
            session_target,
        } => Charger::Simulated(Arc::new(simulated::SimulatedCharger::new(
            config.id.clone(),
            simulated::ChargeProfile::new(
                *plug_in,
                *unplug,
                *current,
                *phases,
                *session_target,
                timezone,
            ),
        ))),
    }
}
