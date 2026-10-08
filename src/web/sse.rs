//! The `/events` stream: one named fragment per section of the dashboard
//! that can independently change, re-rendered through the same component
//! functions the full page uses.
//!
//! Every section rendering from [`DashboardState`](super::state::DashboardState)
//! must appear in [`FRAGMENTS`]. One left off still recomputes each tick and
//! then renders the value it had at page load, with nothing failing.

use std::collections::HashMap;
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

/// Every live section of the page: `layout::page` renders each name on the
/// element whose contents it replaces, and [`fragment_stream`] emits an event
/// per entry.
/// `page_is_live_everywhere_it_claims_to_be` checks the two agree.
pub(super) const FRAGMENTS: [Fragment; 10] = [
    ("top-bar", layout::top_bar_inner),
    ("page-header", layout::page_header_inner),
    ("stat-card-solar", layout::solar_card_inner),
    ("stat-card-home", layout::home_card_inner),
    ("stat-card-grid", layout::grid_card_inner),
    ("stat-card-ev", layout::ev_card_inner),
    ("battery-panel", layout::battery_panel_inner),
    ("energy-flows", layout::energy_flows_inner),
    ("decision-log", layout::decision_log_inner),
    ("forecast-panel", layout::forecast_panel_inner),
];

/// The last payload sent per fragment on one connection. Per connection, so
/// a reconnecting browser starts empty and is sent every fragment again.
#[derive(Default)]
pub(super) struct SentFragments(HashMap<&'static str, String>);

impl SentFragments {
    /// Renders every fragment and keeps only those differing from what this
    /// connection was last sent.
    pub(super) fn changed(&mut self, view: &DashboardView) -> Vec<(&'static str, String)> {
        FRAGMENTS
            .iter()
            .filter_map(|(name, render)| {
                let payload = render(view).into_string();
                if self.0.get(name) == Some(&payload) {
                    return None;
                }
                self.0.insert(name, payload.clone());
                Some((*name, payload))
            })
            .collect()
    }
}

/// `WatchStream` yields the current value immediately on subscribe, then one
/// item per subsequent change — several updates landing between polls
/// collapse into the latest, which is what a live dashboard wants rather
/// than a guaranteed-delivery log.
pub fn fragment_stream(
    rx: DashboardStateReceiver,
    timezone: Tz,
) -> impl Stream<Item = Result<Event, Infallible>> {
    let mut sent = SentFragments::default();
    WatchStream::new(rx).flat_map(move |state| {
        let view = dashboard_view(&state, timezone);
        let events: Vec<Event> = sent
            .changed(&view)
            .into_iter()
            .map(|(name, data)| Event::default().event(name).data(data))
            .collect();
        tokio_stream::iter(events.into_iter().map(Ok))
    })
}
