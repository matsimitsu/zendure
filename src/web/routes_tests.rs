use super::*;

use crate::battery::BatteryState;
use crate::engine::EngineState;
use crate::fixtures::journey;
use crate::units::{BatteryPower, GridPower, SolarPower, Timestamp};
use crate::web::past_days::PastDays;
use crate::web::state::ActualSolarHistory;
use crate::web::state::DashboardState;
use crate::world::{DeviceId, Measurement, MeterReading, World};

fn app_state() -> AppState {
    app_state_with(|_| {})
}

fn app_state_with(edit: impl FnOnce(&mut DashboardState)) -> AppState {
    let mut world = World::new();
    world.observe_meter(
        None,
        MeterReading::total_only(GridPower(400.0)),
        SolarPower::new(750.0),
    );
    world.observe_device(
        DeviceId::new(journey::BATTERY_ID),
        Timestamp::from_millis(0),
        Measurement::Battery(BatteryState {
            current_power: BatteryPower::ZERO,
            ..BatteryState::test_sample()
        }),
    );
    let engine = EngineState {
        world,
        controller: crate::controller::Controller::test_default(journey::NOW_MS, journey::DAY)
            .state(),
        mqtt_timed_out: false,
    };
    let now = Timestamp::from_millis(journey::NOW_MS);
    let mut intervals = journey::interval_ring();
    intervals.record(&journey::meter_event(now, 400.0, 750.0));
    let mut seeded = DashboardState::seed(
        &engine,
        vec![],
        ActualSolarHistory::default(),
        intervals,
        now,
    );
    edit(&mut seeded);
    // The sender is dropped: a `watch` receiver keeps serving its last value.
    let (_, dashboard) = tokio::sync::watch::channel(seeded);
    AppState {
        dashboard,
        timezone: chrono_tz::UTC,
        // Unreadable on purpose: a past day then renders as unreadable,
        // which is all the routing tests need.
        past_days: Arc::new(PastDays::new(
            std::path::PathBuf::from("/nonexistent/journal.db"),
            [DeviceId::new(journey::BATTERY_ID)],
        )),
    }
}

/// Serves the router on an ephemeral port and fetches `path`, so the test
/// goes through routing and extraction rather than calling the handler.
async fn get(path: &str, htmx: bool) -> (StatusCode, String) {
    get_from(app_state(), path, htmx).await
}

async fn get_from(state: AppState, path: &str, htmx: bool) -> (StatusCode, String) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router(state)).await.unwrap();
    });

    let mut request = reqwest::Client::new().get(format!("http://{addr}{path}"));
    if htmx {
        request = request.header("HX-Request", "true");
    }
    let response = request.send().await.unwrap();
    let status = response.status();
    (status, response.text().await.unwrap())
}

#[tokio::test]
async fn an_unknown_entity_is_not_found() {
    let (status, _) = get("/detail/nope", false).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_car_has_no_detail() {
    let (status, _) = get("/detail/car", false).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_htmx_request_gets_the_panel_without_a_page_around_it() {
    for slug in ["solar", "home", "grid", "battery"] {
        let (status, body) = get(&format!("/detail/{slug}"), true).await;
        assert_eq!(status, StatusCode::OK);
        assert!(!body.contains("<html"), "{slug}: {body}");
        assert!(body.contains("modal__header"), "{slug}: {body}");
    }
}

#[tokio::test]
async fn every_component_script_is_loaded_deferred_and_served() {
    let (_, page) = get("/", false).await;
    assert!(
        page.contains(r#"<script type="module" src="/assets/energy-flows.js"></script>"#),
        "the page must load the energy flows element"
    );

    let (status, script) = get("/assets/energy-flows.js", false).await;
    assert_eq!(status, StatusCode::OK);
    assert!(script.contains("customElements.define(\"energy-flows\""));
}

/// `journey::NOW_MS` falls on 4 September 2025, UTC.
const TODAY: &str = "2025-09-04";

#[tokio::test]
async fn an_unparsable_day_is_a_bad_request() {
    let (status, _) = get("/fragments/energy-flows?day=someday", true).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = get("/?day=2025-02-30", false).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = get("/fragments/energy-flows?interval=5m", true).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_future_day_is_today_and_live() {
    let (status, body) = get("/fragments/energy-flows?day=2099-01-01", true).await;

    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(&format!(r#"data-day="{TODAY}""#)), "{body}");
    assert!(body.contains(r#"data-live="true""#));
    assert!(!body.contains("<html"));
}

#[tokio::test]
async fn the_page_opens_on_the_day_it_is_asked_for() {
    let (status, page) = get("/?day=2025-09-01&interval=15m", false).await;

    assert_eq!(status, StatusCode::OK);
    assert!(
        page.contains(r#"class="energy-flows energy-flows--15m" sse-swap="energy-flows" data-day="2025-09-01" data-live="false""#),
        "{page}"
    );
    assert!(page.contains("Mon 1 Sep"));
    assert!(page.contains(r#"href="/?day=2025-09-02&amp;interval=15m""#));
}

#[tokio::test]
async fn the_page_without_a_day_shows_today() {
    let (_, page) = get("/", false).await;

    assert!(page.contains(&format!(r#"data-day="{TODAY}" data-live="true""#)));
}

/// `days` whole UTC days from 29 August (today − 6), priced.
fn priced_days(days: u32) -> AppState {
    use crate::prices::{PricePoint, PriceSeries, PriceSnapshot};
    use crate::units::{CentsPerKwh, Elapsed};
    let first =
        Timestamp::from(chrono::DateTime::parse_from_rfc3339("2025-08-29T00:00:00Z").unwrap());
    let mut points = PriceSeries::default();
    for hour in 0..days * 24 {
        let from = first + Elapsed::of(std::time::Duration::from_secs(u64::from(hour) * 3600));
        points.insert(PricePoint {
            from,
            until: from + Elapsed::HOUR,
            wholesale: CentsPerKwh(10.0 + f64::from(hour % 24)),
        });
    }
    app_state_with(|state| {
        state.price_feed = true;
        state.prices = PriceSnapshot {
            points,
            as_of: Some(state.as_of),
        };
    })
}

/// Through tomorrow, 5 September.
fn tomorrow_published() -> AppState {
    priced_days(8)
}

/// Through today, 4 September.
fn today_only() -> AppState {
    priced_days(7)
}

#[tokio::test]
async fn an_unparsable_price_day_is_a_bad_request() {
    let (status, _) = get("/fragments/price-panel?day=someday", true).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = get("/?price_day=2025-02-30", false).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn the_price_fragment_steps_to_a_past_day() {
    let (status, body) = get_from(
        tomorrow_published(),
        "/fragments/price-panel?day=2025-09-01",
        true,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains("<html"));
    assert!(body.contains(r#"data-day="2025-09-01""#), "{body}");
    assert!(body.contains("Mon 1 Sep"));
    assert!(body.contains("Day average"));
    assert!(!body.contains("price-panel__now-line"));
}

#[tokio::test]
async fn the_price_fragment_clamps_to_its_range() {
    let cases = [
        (tomorrow_published(), "2099-01-01", "2025-09-05"),
        (today_only(), "2025-09-05", "today"),
        (today_only(), "2020-01-01", "2025-08-29"),
    ];
    for (state, asked, shown) in cases {
        let (status, body) =
            get_from(state, &format!("/fragments/price-panel?day={asked}"), true).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains(&format!(r#"data-day="{shown}""#)),
            "{asked}: {body}"
        );
    }
}

#[tokio::test]
async fn the_page_opens_each_panel_on_its_own_day() {
    let (status, page) = get_from(
        tomorrow_published(),
        "/?day=2025-09-01&price_day=2025-09-05",
        false,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(
        page.contains(r#"sse-swap="energy-flows" data-day="2025-09-01""#),
        "{page}"
    );
    assert!(
        page.contains(r#"sse-swap="price-panel" data-day="2025-09-05""#),
        "{page}"
    );
    assert!(page.contains("Tomorrow"));
}

#[tokio::test]
async fn the_price_host_is_today_without_a_price_day() {
    let (_, page) = get_from(tomorrow_published(), "/?day=2025-09-01", false).await;

    assert!(page.contains(r#"sse-swap="price-panel" data-day="today""#));
}

#[tokio::test]
async fn a_plain_request_gets_a_full_page_with_a_way_home() {
    let (status, body) = get("/detail/battery", false).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("<html"));
    assert!(body.contains("href=\"/\""));
    assert!(!body.contains("modal__close"), "no dialog to close");
}

#[tokio::test]
async fn the_solar_detail_charts_its_history_in_the_dialog_and_on_its_own_page() {
    for htmx in [true, false] {
        let (status, body) = get("/detail/solar", htmx).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains("<line-chart class=\"line-chart line-chart--solar\""),
            "{body}"
        );
        assert!(body.contains("class=\"line-chart__line\" d=\"M"), "{body}");
        assert!(body.contains("Produced"), "{body}");
    }
}

#[tokio::test]
async fn the_battery_detail_charts_soc_and_power_in_the_dialog_and_on_its_own_page() {
    for htmx in [true, false] {
        let (status, body) = get("/detail/battery", htmx).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.contains("<html"), !htmx, "{body}");
        assert!(body.contains("State of charge"), "{body}");
        assert_eq!(body.matches("line-chart--battery").count(), 2, "{body}");
        assert!(body.contains("+ discharge · − charge"), "{body}");
    }
}
