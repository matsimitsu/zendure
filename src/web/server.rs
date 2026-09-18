use std::future::Future;
use std::net::SocketAddr;

use chrono_tz::Tz;
use tokio::net::TcpListener;

use crate::config::WebConfig;

use super::routes::{self, AppState};
use super::state::DashboardStateReceiver;

pub(crate) type ServerHandle = tokio::task::JoinHandle<()>;

/// Binds and serves the dashboard, or logs a warning and returns `None` if
/// the port cannot be bound — a missing dashboard degrades the same way a
/// missing `[mqtt]` broker does, not a fatal `run()` error.
pub async fn spawn(
    config: &WebConfig,
    dashboard: DashboardStateReceiver,
    timezone: Tz,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Option<ServerHandle> {
    let addr = SocketAddr::new(config.bind_address, config.port);
    let listener = match TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(e) => {
            tracing::warn!("Dashboard disabled: cannot bind {addr}: {e}");
            return None;
        }
    };

    // The bound address, not the requested one: `port = 0` asks the OS to
    // pick, and this line is the only place the answer surfaces.
    let bound = listener.local_addr().unwrap_or(addr);
    tracing::info!("Web interface listening on: {bound}");
    let app = routes::router(AppState {
        dashboard,
        timezone,
    });

    Some(tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app)
            .with_graceful_shutdown(shutdown)
            .await
        {
            tracing::warn!("Dashboard server ended: {e}");
        }
    }))
}
