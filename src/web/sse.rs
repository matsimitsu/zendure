//! The `/events` stream: one named fragment per section of the dashboard
//! that can independently change, re-rendered through the same component
//! functions the full page uses.

use std::convert::Infallible;

use axum::response::sse::Event;
use chrono_tz::Tz;
use futures_util::{Stream, StreamExt};
use tokio_stream::wrappers::WatchStream;

use super::state::DashboardStateReceiver;
use super::templates::layout;
use super::view::dashboard_view;

/// `WatchStream` yields the current value immediately on subscribe, then one
/// item per subsequent change — several updates landing between polls
/// collapse into the latest, which is what a live dashboard wants rather
/// than a guaranteed-delivery log.
pub fn fragment_stream(
    rx: DashboardStateReceiver,
    timezone: Tz,
) -> impl Stream<Item = Result<Event, Infallible>> {
    WatchStream::new(rx).flat_map(move |state| {
        let view = dashboard_view(&state, timezone);
        let events = vec![
            Event::default()
                .event("stat-cards")
                .data(layout::stat_cards_inner(&view).into_string()),
            Event::default()
                .event("battery-panel")
                .data(layout::battery_panel_inner(&view).into_string()),
            Event::default()
                .event("decision-log")
                .data(layout::decision_log_inner(&view).into_string()),
        ];
        tokio_stream::iter(events.into_iter().map(Ok))
    })
}
