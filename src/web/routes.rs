use axum::Router;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::sse::{KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use chrono_tz::Tz;
use rust_embed::Embed;

use super::sse::fragment_stream;
use super::state::DashboardStateReceiver;
use super::templates::layout;
use super::view::dashboard_view;

/// The Grass-compiled CSS, written to `OUT_DIR` by `build.rs` — see its own
/// doc comment for why compilation happens at build time rather than here.
#[derive(Embed)]
#[folder = "$OUT_DIR"]
struct Assets;

#[derive(Clone)]
pub struct AppState {
    pub dashboard: DashboardStateReceiver,
    pub timezone: Tz,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/events", get(events))
        .route("/assets/{*path}", get(asset))
        .with_state(state)
}

async fn index(State(state): State<AppState>) -> impl IntoResponse {
    let current = state.dashboard.borrow().clone();
    let view = dashboard_view(&current, state.timezone);
    layout::page(&view)
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
