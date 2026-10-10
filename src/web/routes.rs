use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::sse::{KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use chrono_tz::Tz;
use rust_embed::Embed;

use super::day_nav::{BadQuery, kept_query};
use super::entity::Entity;
use super::flows::{EnergyFlowsView, requested_flows_view};
use super::past_days::{FlowsQuery, PastDays};
use super::prices::{PriceDayQuery, requested_price_panel};
use super::sse::fragment_stream;
use super::state::{DashboardState, DashboardStateReceiver};
use super::templates::{energy_flows, layout, price_panel};
use super::view::{dashboard_view, detail_view};
use crate::clock::local_date;

/// The Grass-compiled CSS, written to `OUT_DIR` by `build.rs` — see its own
/// doc comment for why compilation happens at build time rather than here.
#[derive(Embed)]
#[folder = "$OUT_DIR"]
struct Assets;

#[derive(Clone)]
pub struct AppState {
    pub dashboard: DashboardStateReceiver,
    pub timezone: Tz,
    pub past_days: Arc<PastDays>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/events", get(events))
        .route("/fragments/energy-flows", get(energy_flows))
        .route("/fragments/price-panel", get(price_panel))
        .route("/detail/{entity}", get(detail))
        .route("/assets/{*path}", get(asset))
        .with_state(state)
}

/// `?day=` and `?interval=` choose the flows panel's day and `?price_day=`
/// the price panel's, so a step link still works in a browser without htmx.
async fn index(
    State(state): State<AppState>,
    RawQuery(raw): RawQuery,
) -> Result<Response, Refused> {
    let query = FlowsQuery::parse(raw.as_deref())?;
    let price_day = PriceDayQuery::parse_page(raw.as_deref())?;
    let current = state.dashboard.borrow().clone();
    let mut view = dashboard_view(&current, price_day, state.timezone);
    view.energy_flows = flows_view(&state, &current, query).await?;
    // Each panel's full-page links keep the other's day, for a browser
    // without htmx.
    view.energy_flows.nav.keep = kept_query(raw.as_deref(), &["price_day"]);
    if let Some(nav) = view.prices.nav_mut() {
        nav.keep = kept_query(raw.as_deref(), &["day", "interval"]);
    }
    Ok(layout::page(&view).into_response())
}

/// The flows panel's contents for one day, which the step links swap into
/// its host.
async fn energy_flows(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
) -> Result<Response, Refused> {
    let query = FlowsQuery::parse(query.as_deref())?;
    let current = state.dashboard.borrow().clone();
    let view = flows_view(&state, &current, query).await?;
    Ok(energy_flows::render(&view).into_response())
}

/// The price panel's contents for one day, which the step links swap into
/// its host. Every day it reaches is already in the snapshot.
async fn price_panel(
    State(state): State<AppState>,
    RawQuery(raw): RawQuery,
) -> Result<Response, BadQuery> {
    let query = PriceDayQuery::parse_fragment(raw.as_deref())?;
    let current = state.dashboard.borrow().clone();
    let view = requested_price_panel(&current, query, state.timezone);
    Ok(price_panel::render(&view).into_response())
}

/// Today from the live ring, which the stream keeps current; any earlier day
/// from the journal, off the async runtime since SQLite blocks.
async fn flows_view(
    state: &AppState,
    current: &DashboardState,
    query: FlowsQuery,
) -> Result<EnergyFlowsView, PastDayFailed> {
    let (now, tz) = (current.as_of, state.timezone);
    let today = local_date(now, tz).unwrap_or_default();
    let request = query.resolve(today);
    if request.day == today {
        return Ok(requested_flows_view(
            &current.intervals,
            request,
            None,
            now,
            tz,
        ));
    }
    let past_days = Arc::clone(&state.past_days);
    tokio::task::spawn_blocking(move || past_days.view(request, now, tz))
        .await
        .map_err(|e| {
            tracing::warn!("Dashboard: reading a past day failed: {e}");
            PastDayFailed
        })
}

/// The task reading a past day panicked. Answered 500: the live ring under a
/// past day's label would be a lie.
struct PastDayFailed;

impl IntoResponse for PastDayFailed {
    fn into_response(self) -> Response {
        (StatusCode::INTERNAL_SERVER_ERROR, "reading that day failed").into_response()
    }
}

impl IntoResponse for BadQuery {
    fn into_response(self) -> Response {
        (StatusCode::BAD_REQUEST, self.to_string()).into_response()
    }
}

/// Why a panel request was not answered with the panel.
enum Refused {
    BadQuery(BadQuery),
    PastDayFailed(PastDayFailed),
}

impl From<BadQuery> for Refused {
    fn from(bad: BadQuery) -> Self {
        Refused::BadQuery(bad)
    }
}

impl From<PastDayFailed> for Refused {
    fn from(failed: PastDayFailed) -> Self {
        Refused::PastDayFailed(failed)
    }
}

impl IntoResponse for Refused {
    fn into_response(self) -> Response {
        match self {
            Refused::BadQuery(bad) => bad.into_response(),
            Refused::PastDayFailed(failed) => failed.into_response(),
        }
    }
}

/// The panel alone for htmx, which swaps it into the dialog; a whole page for
/// a browser that followed the card's link itself. The two share a URL, so
/// the response must say it varies by who is asking.
async fn detail(
    State(state): State<AppState>,
    Path(entity): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Ok(entity) = entity.parse::<Entity>() else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let current = state.dashboard.borrow().clone();
    let detail = detail_view(&current, entity, state.timezone);
    let vary = [(header::VARY, HeaderValue::from_static("HX-Request"))];

    if headers.contains_key(HeaderName::from_static("hx-request")) {
        return (vary, layout::detail_fragment(&detail)).into_response();
    }
    let dashboard = dashboard_view(&current, PriceDayQuery::default(), state.timezone);
    (vary, layout::detail_page(&dashboard, &detail)).into_response()
}

async fn events(State(state): State<AppState>) -> impl IntoResponse {
    let stream = fragment_stream(state.dashboard.clone(), state.timezone);
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn asset(Path(path): Path<String>) -> Response {
    match Assets::get(&path) {
        Some(file) => {
            let mime = mime_guess::from_path(&path).first_or_octet_stream();
            (
                [(header::CONTENT_TYPE, mime.as_ref().to_string())],
                file.data.into_owned(),
            )
                .into_response()
        }
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

#[cfg(test)]
#[path = "routes_tests.rs"]
mod tests;
