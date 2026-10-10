//! The car's (a VW ID.3, via the VW/Cupra EU Data Act portal) battery state
//! of charge, for the dashboard's EV card and a Home Assistant sensor.
//!
//! `CarBatterySource` is the capability, mirroring `crate::prediction::Prediction`:
//! one trait, several backends, dispatched through an enum because the
//! trait's `impl Future + Send` return isn't `dyn`-safe. `[car_battery] kind
//! = "vw_portal"` selects the real backend (`vw_portal.rs`), `"simulated"` a
//! synthetic one (`simulated.rs`) for local dev and testing with no VW
//! account needed.
//!
//! Display-only, like `prediction`: nothing here feeds `crate::controller`,
//! and the reading never touches `World`/`Event`/`Engine` — it lands
//! straight in `DashboardState` (`car_soc_tick`) and, separately, is
//! announced/published to Home Assistant under its own device.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use crate::announce::Announcer;
use crate::config::CarBatteryConfig;
use crate::publish::Publisher;
use crate::units::{Soc, Timestamp};
use crate::web::DashboardStateSender;

pub mod simulated;
pub mod vw_portal;

/// What can go wrong fetching the car's SoC. Mirrors `crate::fetch::FetchError`'s rule:
/// a response this build cannot decode is the one most worth keeping.
#[derive(Debug)]
pub enum CarBatteryError {
    Request(String),
    Auth(String),
    Parse(String),
}

impl std::fmt::Display for CarBatteryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CarBatteryError::Request(e) => write!(f, "request failed: {e}"),
            CarBatteryError::Auth(e) => write!(f, "authentication failed: {e}"),
            CarBatteryError::Parse(e) => write!(f, "parse error: {e}"),
        }
    }
}

/// `&mut self`, unlike `Prediction::forecast`'s `&self`: the VW backend holds
/// session cookies that a login mutates, and re-authenticating mid-poll is
/// part of a normal poll, not a special case.
pub trait CarBatterySource {
    fn poll(&mut self) -> impl Future<Output = Result<Soc, CarBatteryError>> + Send;
}

/// Whichever backend `[car_battery]` selected. Mirrors `prediction::Forecaster`:
/// `CarBatterySource::poll`'s `impl Future` return isn't `dyn`-safe, so a
/// real seam needs an enum, not a trait object.
pub enum CarBattery {
    VwPortal(vw_portal::VwPortalClient),
    Simulated(simulated::SimulatedCarBattery),
}

impl CarBatterySource for CarBattery {
    async fn poll(&mut self) -> Result<Soc, CarBatteryError> {
        match self {
            CarBattery::VwPortal(c) => c.poll().await,
            CarBattery::Simulated(c) => c.poll().await,
        }
    }
}

/// The only place a [`CarBatteryConfig`] becomes a live backend — `run.rs`
/// never matches on it directly, the same rule `prediction::from_config`
/// follows.
pub fn from_config(config: &CarBatteryConfig) -> CarBattery {
    match config {
        CarBatteryConfig::VwPortal {
            email,
            password,
            vin,
            country,
            language,
            ..
        } => CarBattery::VwPortal(vw_portal::VwPortalClient::new(
            email.clone(),
            password.clone(),
            vin.clone(),
            country.clone(),
            language.clone(),
        )),
        CarBatteryConfig::Simulated { .. } => {
            CarBattery::Simulated(simulated::SimulatedCarBattery::new())
        }
    }
}

/// The poller task: independent of `Event`/`Engine::step` and the source
/// tasks, on its own timer — the same posture as
/// `prediction::run_forecast_poller`. Gated only on `[car_battery]` being
/// configured, *not* also on a dashboard existing: publishing to Home
/// Assistant is useful with `[web]` absent, unlike the forecast poller which
/// is dashboard-only, so `dashboard_tx` is optional here.
///
/// A failed poll logs and leaves the dashboard's last-known reading in
/// place — its age is what tells a viewer it's stale, the same "stop
/// updating, never show a wrong number" rule every other poller here
/// follows.
pub async fn run_car_battery_poller(
    mut source: CarBattery,
    poll_interval: Duration,
    dashboard_tx: Option<DashboardStateSender>,
    publisher: Arc<dyn Publisher>,
    announcer: Arc<Announcer>,
    ha_publish_prefix: String,
    mut shutdown: tokio::sync::oneshot::Receiver<()>,
) {
    let mut ticker = tokio::time::interval(poll_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            _ = ticker.tick() => {
                match source.poll().await {
                    Ok(soc) => {
                        tracing::info!("Car battery: {soc}%");
                        let now = Timestamp::from(chrono::Utc::now());
                        if let Some(tx) = &dashboard_tx {
                            tx.send_modify(|s| s.car_soc_tick(soc, now));
                        }
                        crate::mqtt::publish_car_battery_soc(
                            &*publisher,
                            &announcer,
                            &ha_publish_prefix,
                            soc,
                        );
                    }
                    Err(e) => {
                        tracing::warn!("Car battery poll failed: {e}");
                    }
                }
            }
        }
    }
}
