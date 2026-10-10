//! The live dashboard: an Axum server, fed by a `watch` channel `run()`
//! populates after every event it folds, rendering Maud templates styled by
//! Grass-compiled, rust-embed'd CSS.

mod axis;
mod detail;
mod entity;
mod flows;
mod intervals;
mod line_chart;
mod pack_intervals;
mod past_days;
mod routes;
mod server;
mod soc_bar;
mod sse;
mod state;
mod templates;
mod view;

#[cfg(test)]
pub(crate) use intervals::IntervalHistory;
pub use intervals::seed_interval_history;
pub use past_days::PastDays;
pub use server::spawn;
pub use state::{
    DashboardState, DashboardStateSender, DashboardTelemetry, ForecastSnapshot, PolledPacks,
    PriceSnapshot, seed_actual_solar, seed_decision_log,
};
