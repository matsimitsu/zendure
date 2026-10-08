//! The live dashboard: an Axum server, fed by a `watch` channel `run()`
//! populates after every event it folds, rendering Maud templates styled by
//! Grass-compiled, rust-embed'd CSS.

mod axis;
mod intervals;
mod routes;
mod server;
mod sse;
mod state;
mod templates;
mod view;

pub use intervals::seed_interval_history;
pub use server::spawn;
pub use state::{
    ActualSolarHistory, DashboardState, DashboardStateSender, DashboardTelemetry, ForecastSnapshot,
    seed_decision_log,
};
