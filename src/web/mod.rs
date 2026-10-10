//! The live dashboard: an Axum server, fed by a `watch` channel `run()`
//! populates after every event it folds, rendering Maud templates styled by
//! Grass-compiled, rust-embed'd CSS.

mod axis;
mod day_nav;
mod day_panel;
mod detail;
mod entity;
mod flows;
mod intervals;
mod line_chart;
mod pack_intervals;
mod past_days;
mod plot;
mod prices;
mod routes;
mod server;
mod soc_bar;
mod solar;
mod solar_day;
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
    seed_decision_log,
};
