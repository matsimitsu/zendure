//! The `/events` stream: one named fragment per section of the dashboard
//! that can independently change, re-rendered through the same component
//! functions the full page uses.
//!
//! Every section rendering from [`DashboardState`](super::state::DashboardState)
//! must appear in [`FRAGMENTS`]. One left off still recomputes each tick and
//! then renders the value it had at page load, with nothing failing.

use std::convert::Infallible;

use axum::response::sse::Event;
use chrono_tz::Tz;
use futures_util::{Stream, StreamExt};
use tokio_stream::wrappers::WatchStream;

use maud::Markup;

use super::state::DashboardStateReceiver;
use super::templates::layout;
use super::view::{DashboardView, dashboard_view};

/// One live section of the page: the `sse-swap` name its wrapper carries, and
/// the renderer that produces that wrapper's contents.
pub(super) type Fragment = (&'static str, fn(&DashboardView) -> Markup);

/// Every live section of the page: `layout::page` wraps each name in the
/// `div` carrying it, and [`fragment_stream`] emits an event per entry.
/// `page_is_live_everywhere_it_claims_to_be` checks the two agree.
pub(super) const FRAGMENTS: [Fragment; 6] = [
    ("top-bar", layout::top_bar_inner),
    ("page-header", layout::page_header_inner),
    ("stat-cards", layout::stat_cards_inner),
    ("battery-panel", layout::battery_panel_inner),
    ("decision-log", layout::decision_log_inner),
    ("forecast-panel", layout::forecast_panel_inner),
];

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
        let events: Vec<Event> = FRAGMENTS
            .iter()
            .map(|(name, render)| {
                Event::default()
                    .event(*name)
                    .data(render(&view).into_string())
            })
            .collect();
        tokio_stream::iter(events.into_iter().map(Ok))
    })
}
