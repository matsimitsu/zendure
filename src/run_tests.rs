use super::*;
use crate::config::{DeviceConfig, MeterConfig, SessionConfig, ShellyConfig, WebConfig};
use crate::fixtures;
use crate::journal;
use crate::registry::Battery;
use crate::simulation::VirtualBattery;
use crate::source::shelly::SolarPhase;
use crate::units::{Efficiency, GridPower, PowerMargin, RetentionDays, SolarPower, Watts};

/// The two signal arms became one, so the wording an operator greps for is
/// now produced by `Display`. Only the name is pinned here: retyping the
/// whole log line would assert against a copy this test made, which keeps
/// passing when the real one changes.
#[test]
fn a_stop_reason_renders_the_signal_name() {
    assert_eq!(StopReason::Sigterm.to_string(), "SIGTERM");
    assert_eq!(StopReason::Sigint.to_string(), "SIGINT");
}

/// `shut_down` must finish without taking the journal down with it. A dead broker
/// parks the
/// delivery task on rumqttc's channel, forcing the MQTT drain to hit its deadline —
/// mutation-
/// checked, since a bare `await` in place of the timeout makes this fail. The source
/// task holds
/// an `Arc<Journal>` clone like a real sampler, so this also covers the writer ending
/// only once every sender is gone (the journal drain's own deadline is
/// belt-and-braces and not distinguished here).
#[tokio::test]
async fn a_shutdown_finishes_when_the_broker_never_drains() {
    let dir = tempfile::TempDir::new().unwrap();
    let (journal, writer, path) = journal::testing::open(&dir, &SessionConfig::test_default());
    let journal = std::sync::Arc::new(journal);

    let events = fixtures::journey::events();
    for event in &events {
        journal.event(event);
    }

    // A publisher whose broker is not there, filled until its task parks.
    let opts = rumqttc::MqttOptions::new("zendure-shutdown-test", "127.0.0.1", 1);
    let (client, _eventloop) = rumqttc::AsyncClient::new(opts, 50);
    let (publisher, mut publisher_task) = MqttPublisher::open(client);
    for i in 0..300 {
        publisher.publish(crate::publish::Message::telemetry(
            "zendure/decision_power".to_string(),
            i.to_string(),
        ));
        tokio::task::yield_now().await;
    }
    assert!(publisher.queued() > 0, "the delivery task is parked");

    // Holds a journal sender and never returns, like a real sampler.
    let sampler_journal = journal.clone();
    let mut sources = tokio::task::JoinSet::new();
    sources.spawn(async move {
        let _held = sampler_journal;
        std::future::pending::<()>().await;
    });

    // Generously past both deadlines: this asserts that it is bounded at
    // all, not what the bound is.
    tokio::time::timeout(
        DRAIN_DEADLINE * 4,
        shut_down(
            Some((&publisher, &mut publisher_task)),
            sources,
            journal,
            Some(writer),
        ),
    )
    .await
    .expect("a parked delivery task must not stall the shutdown");

    let conn = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        journal::testing::count(&conn, "SELECT COUNT(*) FROM events"),
        events.len() as i64,
        "every row handed to the journal survived a drain that could not finish",
    );
}

/// Builds the `Config` the wiring tests below share: a virtual battery, a null
/// publisher (no
/// `[mqtt]`) and a synthetic meter — no network, no broker, no hardware. `capacity`
/// is a
/// parameter because the wiring test wants it small enough to see the SOC move,
/// while the round-trip test doesn't care and uses whatever is convenient. The
/// device's period is the control tick, and is short enough here that a run
/// measured in wall-clock seconds still takes several.
fn virtual_config(dir: &tempfile::TempDir, capacity: WattHours) -> Config {
    Config {
        mqtt: None,
        device: DeviceConfig::Virtual {
            id: "sim".to_string(),
            packs: vec![capacity],
            soc: Soc::new(50),
            charge_efficiency: Efficiency::new(95.0),
            discharge_efficiency: Efficiency::new(95.0),
            poll_interval: Duration::from_millis(250),
        },
        shelly: None,
        // A constant 2 kW load and no solar: the grid reading always
        // imports solidly, so the objective has something unambiguous to
        // discharge against instead of hovering near a threshold.
        meter: MeterConfig::Synthetic {
            base_load: Watts(2000),
            solar_peak: Watts(0),
        },
        web: None,
        prediction: None,
        ha_publish_prefix: "test".to_string(),
        charge_margin: PowerMargin::new(50),
        discharge_margin: PowerMargin::new(5),
        charge_start_threshold: GridPower(-100.0),
        discharge_start_threshold: GridPower(0.0),
        // No cooldowns: the tuning knobs a real deployment leans on to
        // avoid chattering, turned down here so the test does not have
        // to wait out a cooldown window to see a second decision within
        // its short run.
        min_mode_duration: Duration::from_secs(0),
        min_decision_interval: Duration::from_secs(0),
        idle_timeout: Duration::from_secs(300),
        cycle_warn_threshold: 200,
        min_soc: Soc::new(10),
        max_soc: Soc::new(100),
        balance_weekday: None,
        solar_discharge_block_threshold: SolarPower::ZERO,
        min_idle_before_discharge: Duration::from_secs(0),
        timezone: chrono_tz::Tz::UTC,
        mqtt_timeout: Duration::from_secs(60),
        journal_path: dir.path().join("journal.db"),
        journal_retention_days: RetentionDays::new(90).unwrap(),
        rte_state_path: dir.path().join("rte_state.json"),
        log_filter: "zendure=off".to_string(),
    }
}

/// The control tick, which a virtual device carries as its own scan period.
fn with_tick(config: &mut Config, tick: Duration) {
    let DeviceConfig::Virtual { poll_interval, .. } = &mut config.device else {
        panic!("virtual_config must always build a DeviceConfig::Virtual")
    };
    *poll_interval = tick;
}

async fn stop_after(run_for: Duration) -> StopReason {
    tokio::time::sleep(run_for).await;
    StopReason::Sigterm
}

fn count_events(conn: &rusqlite::Connection, kind: &str) -> i64 {
    journal::testing::count(
        conn,
        &format!("SELECT COUNT(*) FROM events WHERE kind = '{kind}'"),
    )
}

/// Builds the registry `virtual_config`'s device describes, and hands
/// back the same `Arc<VirtualBattery>` the registry holds — the seam
/// `run`'s own doc comment describes, exercised directly rather than
/// through `main`.
fn virtual_devices(config: &Config) -> (Devices, std::sync::Arc<VirtualBattery>) {
    let devices = registry::from_config(config);
    let battery = match devices.primary() {
        Some((_, Battery::Virtual(battery))) => battery.clone(),
        _ => panic!("virtual_config must always build a DeviceConfig::Virtual"),
    };
    (devices, battery)
}

/// The end-to-end wiring test: the real `run()` with a virtual battery, a null
/// publisher and a synthetic meter — no network, no broker, no hardware. Asserted
/// via this test's own `Arc<VirtualBattery>` clone rather than through the journal.
/// `journal_path` points somewhere `Journal::open` cannot create, disabling the
/// journal deliberately rather than as a workaround: under `start_paused`, a single
/// outstanding `spawn_blocking` task — and the journal's writer is one — is by
/// itself enough to stop `tokio::time::sleep` from ever resolving.
/// `a_run_against_the_simulator_replays_byte_identically` keeps a real journal.
/// Expected capacity: discharge is capped at 800W and a 2kW load keeps the
/// objective pinned there, so at 95% efficiency the pack gives up 842.1Wh to
/// deliver 800Wh; over 300 simulated seconds a 1,000Wh pack loses about 70.2Wh —
/// roughly 7 points of SOC from a 50% start.
#[tokio::test(start_paused = true)]
async fn run_drives_real_decisions_against_a_virtual_battery() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut config = virtual_config(&dir, WattHours(1_000.0));
    // Deliberately unopenable: `blocker` is a plain file, so `Journal::open`'s
    // `create_dir_all(parent)` fails and it falls back to its documented disabled
    // state (no
    // channel, writer thread, or SQLite) — the right choice here per this test's
    // own doc comment, not a workaround.
    let blocker = dir.path().join("blocker");
    std::fs::write(&blocker, b"unwritable as a directory").unwrap();
    config.journal_path = blocker.join("journal.db");
    let (devices, battery) = virtual_devices(&config);

    let soc_before = battery.reading().soc;

    let stop = stop_after(Duration::from_secs(300));

    run(config, devices, stop)
        .await
        .expect("run must exit cleanly");

    assert!(
        battery.flow().discharging() > Watts::ZERO,
        "a constant importing load with no solar must leave the battery discharging"
    );

    let soc_after = battery.reading().soc;
    assert!(
        soc_after < soc_before,
        "SOC must have moved downward over 300 simulated seconds of discharge: \
             before={soc_before}%, after={soc_after}%",
    );
}

/// Asks the OS for a free port and hands the number back: `WebConfig`
/// takes a port, not a listener, so the bind itself has to happen inside
/// `run`.
fn a_free_port() -> u16 {
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.local_addr().unwrap().port()
}

async fn connect_to_dashboard(port: u16) -> tokio::net::TcpStream {
    for _ in 0..200 {
        if let Ok(stream) =
            tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).await
        {
            return stream;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the dashboard never came up on port {port}");
}

/// A browser tab left open must not hold shutdown to the drain deadline. Two
/// independent
/// faults show up as the same two seconds: a dashboard's own signal handler never
/// hears a
/// non-signal stop like this test's, and a live `WatchStream` keeps every SSE body
/// (and
/// `with_graceful_shutdown`) open until the sender drops. Timed, not inspected,
/// since the deadline is the only observable either has; real time, not paused,
/// since auto-advance would skip past the wall-clock deadline being asserted.
#[tokio::test]
async fn an_open_dashboard_stream_does_not_hold_shutdown_to_the_deadline() {
    const RUN_FOR: Duration = Duration::from_millis(300);

    let dir = tempfile::TempDir::new().unwrap();
    let mut config = virtual_config(&dir, WattHours(1_000.0));
    // Short enough that this window holds several ticks: a run that never
    // ticks would time its own shutdown and prove nothing.
    with_tick(&mut config, Duration::from_millis(50));
    let port = a_free_port();
    config.web = Some(WebConfig {
        bind_address: std::net::Ipv4Addr::LOCALHOST.into(),
        port,
    });
    let (devices, _battery) = virtual_devices(&config);

    // The tab: `WatchStream` yields the current value on subscribe, so
    // this is a live body from the moment it connects.
    let tab = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = connect_to_dashboard(port).await;
        stream
            .write_all(b"GET /events HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut body = Vec::new();
        stream.read_to_end(&mut body).await.unwrap();
        body
    });

    let stop = stop_after(RUN_FOR);

    let started = std::time::Instant::now();
    run(config, devices, stop)
        .await
        .expect("run must exit cleanly");
    let elapsed = started.elapsed();

    let body = tokio::time::timeout(DRAIN_DEADLINE, tab)
        .await
        .expect("the SSE body must end once the dashboard channel closes")
        .unwrap();
    assert!(
        String::from_utf8_lossy(&body).contains("event: battery-panel"),
        "the tab was never served a live fragment"
    );

    assert!(
        elapsed < RUN_FOR + DRAIN_DEADLINE / 2,
        "shutdown burned the dashboard's drain deadline: {elapsed:?}",
    );
}

/// Nothing re-arms the failsafe any more, so the only thing that can stand the
/// fleet down is a source that stops answering — and the tick that notices has
/// to be an otherwise healthy one. A Shelly pointed at a closed port refuses
/// every round instantly (a failure, not a hang), while the virtual battery
/// keeps answering, so the meter alone is what trips it. Mutation check: a
/// liveness deadline re-armed from the tick would leave no `mqtt_timeout` row
/// at all.
#[tokio::test]
async fn the_failsafe_fires_while_the_tick_is_healthy() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut config = virtual_config(&dir, WattHours(1_000.0));
    with_tick(&mut config, Duration::from_millis(50));
    config.mqtt_timeout = Duration::from_millis(300);
    config.meter = MeterConfig::Shelly;
    config.shelly = Some(ShellyConfig {
        // Bound and released, so the port is closed and the connection is
        // refused rather than hanging until the request timeout.
        ip: format!("127.0.0.1:{}", a_free_port()),
        solar_phase: SolarPhase::A,
    });
    let journal_path = config.journal_path.clone();
    let (devices, _battery) = virtual_devices(&config);

    run(config, devices, stop_after(Duration::from_millis(800)))
        .await
        .expect("run must exit cleanly");

    let conn = rusqlite::Connection::open(&journal_path).unwrap();
    assert!(
        count_events(&conn, "mqtt_timeout") > 0,
        "a meter that never answers must stand the fleet down"
    );
    assert_eq!(
        count_events(&conn, "meter"),
        0,
        "no reading ever arrived, so nothing may have been decided on one"
    );
    assert!(
        journal::testing::count(
            &conn,
            "SELECT COUNT(*) FROM decisions WHERE kind = 'failsafe'"
        ) > 0,
        "the stand-down has to reach the battery, not just the journal"
    );
}

/// Stops the test above from passing because the failsafe fires on every run:
/// the same shape with a meter that answers must never arm it.
#[tokio::test]
async fn a_live_meter_never_arms_the_failsafe() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut config = virtual_config(&dir, WattHours(1_000.0));
    with_tick(&mut config, Duration::from_millis(50));
    config.mqtt_timeout = Duration::from_millis(300);
    let journal_path = config.journal_path.clone();
    let (devices, _battery) = virtual_devices(&config);

    run(config, devices, stop_after(Duration::from_millis(600)))
        .await
        .expect("run must exit cleanly");

    let conn = rusqlite::Connection::open(&journal_path).unwrap();
    assert!(count_events(&conn, "meter") > 0, "the meter was answering");
    assert_eq!(count_events(&conn, "mqtt_timeout"), 0);
}

/// The matched-age invariant as the journal records it: a decision has to land
/// on a world already refreshed by this tick's device samples, so every
/// `meter` row is immediately preceded by the `device_update` from the same
/// tick.
#[tokio::test]
async fn a_tick_emits_its_device_updates_before_its_meter() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut config = virtual_config(&dir, WattHours(1_000.0));
    with_tick(&mut config, Duration::from_millis(50));
    let journal_path = config.journal_path.clone();
    let (devices, _battery) = virtual_devices(&config);

    run(config, devices, stop_after(Duration::from_millis(600)))
        .await
        .expect("run must exit cleanly");

    let conn = rusqlite::Connection::open(&journal_path).unwrap();
    let mut statement = conn
        .prepare("SELECT kind FROM events WHERE kind IN ('meter', 'device_update') ORDER BY seq")
        .unwrap();
    let kinds: Vec<String> = statement
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();

    assert_eq!(
        kinds.first().map(String::as_str),
        Some("device_update"),
        "the first foldable event of a session is the startup poll"
    );
    for (i, kind) in kinds.iter().enumerate() {
        if kind == "meter" {
            assert_eq!(
                i.checked_sub(1)
                    .and_then(|p| kinds.get(p))
                    .map(String::as_str),
                Some("device_update"),
                "a decision landed on a world this tick had not refreshed: {kinds:?}"
            );
        }
    }
}

/// The guard for `journal::read::pair`'s sequencing rule, which is invisible
/// to a reader of the tick body: decisions are attributed to the event whose
/// `seq` precedes them, so journalling a whole tick up front would credit
/// every command to the last event and make `--verify` pass against a wrong
/// recording.
#[tokio::test]
async fn every_commanded_frame_is_a_meter_or_a_timeout() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut config = virtual_config(&dir, WattHours(1_000.0));
    with_tick(&mut config, Duration::from_millis(50));
    let journal_path = config.journal_path.clone();
    let (devices, _battery) = virtual_devices(&config);

    run(config, devices, stop_after(Duration::from_millis(600)))
        .await
        .expect("run must exit cleanly");

    let recording = crate::journal::read::read_range(
        &journal_path,
        Timestamp::from_millis(0),
        Timestamp::from_millis(i64::MAX),
    )
    .expect("a run that recorded decisions must produce a readable range");
    let (fixture, _warnings) =
        crate::replay::from_recording(recording).expect("a populated journal must yield a fixture");

    let commanded = fixture
        .events
        .iter()
        .zip(&fixture.expected)
        .filter(|(_, expected)| !expected.ends_with(crate::replay::NOTHING))
        .count();
    assert!(commanded > 0, "the run must have commanded something");
    assert!(
        fixture
            .events
            .iter()
            .any(|event| matches!(event, Event::DeviceUpdate { .. })),
        "the recording must hold device updates for this to be about them"
    );

    for (event, expected) in fixture.events.iter().zip(&fixture.expected) {
        if matches!(event, Event::DeviceUpdate { .. }) {
            assert!(
                expected.ends_with(crate::replay::NOTHING),
                "a device update carried commands: {expected}"
            );
        }
    }
}

/// A real `run()` loop populates the journal in a shape `from_recording` can
/// consume, and
/// replaying it produces the same commands end to end (meter → channel → engine →
/// journal →
/// `read_range` → `replay::verify`) — `replay_tests.rs` only proves the fold
/// deterministic on a
/// canned event vector, taking on faith that a real loop's journal reads back. This
/// test's subject is the journal, so it keeps a real one, ruling out `start_paused`
/// (`Journal::open` spawns a `spawn_blocking` task, and an outstanding one stalls a
/// paused clock's auto-advance) — real time, capped at two seconds: several ticks
/// at `virtual_config`'s period, short enough not to dominate the suite.
#[tokio::test]
async fn a_run_against_the_simulator_replays_byte_identically() {
    let dir = tempfile::TempDir::new().unwrap();
    let config = virtual_config(&dir, WattHours(1_000.0));
    let journal_path = config.journal_path.clone();
    let (devices, _battery) = virtual_devices(&config);

    // A few real ticks is enough for more than one recorded decision; this
    // test is not about how much history replays, only that what was
    // recorded replays identically.
    let stop = stop_after(Duration::from_secs(2));
    // From the epoch: this test wants everything the session ever wrote, not a
    // particular
    // anchor. `read_range` handles a `from` before the first decision by falling
    // back to the
    // range's own start with a warning, which this test allows for rather than
    // asserts away.
    let started_at = Timestamp::from_millis(0);

    run(config, devices, stop)
        .await
        .expect("run must exit cleanly");

    let recording = crate::journal::read::read_range(
        &journal_path,
        started_at,
        Timestamp::from_millis(i64::MAX),
    )
    .expect("a run that recorded decisions must produce a readable range");
    let (fixture, _warnings) =
        crate::replay::from_recording(recording).expect("a populated journal must yield a fixture");

    let frames = crate::replay::run(&fixture, &[]).expect("the fixture's own format must replay");
    crate::replay::verify(&fixture, &frames)
        .expect("replaying a real run's own journal must match what it recorded");

    // Not trivially true: the run does command something, so `expected`
    // is not a list of nothing-but-dashes that `verify` would agree with
    // for free.
    assert!(
        fixture
            .expected
            .iter()
            .any(|line| !line.ends_with(crate::replay::NOTHING)),
        "{:?}",
        fixture.expected
    );
}
