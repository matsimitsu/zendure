use super::*;

use crate::battery::BatteryState;
use crate::engine::EngineState;
use crate::fixtures::journey;
use crate::units::{BatteryPower, GridPower, SolarPower, Timestamp};
use crate::web::state::DashboardState;
use crate::web::{ActualSolarHistory, intervals::IntervalHistory};
use crate::world::{DeviceId, Measurement, MeterReading, World};

fn app_state() -> AppState {
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
    let mut intervals = IntervalHistory::new([DeviceId::new(journey::BATTERY_ID)]);
    intervals.record(&journey::meter_event(now, 400.0, 750.0));
    let seeded = DashboardState::seed(
        &engine,
        vec![],
        ActualSolarHistory::default(),
        intervals,
        now,
    );
    // The sender is dropped: a `watch` receiver keeps serving its last value.
    let (_, dashboard) = tokio::sync::watch::channel(seeded);
    AppState {
        dashboard,
        timezone: chrono_tz::UTC,
    }
}

/// Serves the router on an ephemeral port and fetches `path`, so the test
/// goes through routing and extraction rather than calling the handler.
async fn get(path: &str, htmx: bool) -> (StatusCode, String) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router(app_state())).await.unwrap();
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
        page.contains(r#"<script src="/assets/energy-flows.js" defer></script>"#),
        "the page must load the energy flows element"
    );

    let (status, script) = get("/assets/energy-flows.js", false).await;
    assert_eq!(status, StatusCode::OK);
    assert!(script.contains("customElements.define(\"energy-flows\""));
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
