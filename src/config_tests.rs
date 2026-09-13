//! Tests for `config.rs`, kept in their own file because the module covers
//! both `from_toml_str` and the `SessionConfig` projection out of it.

use std::collections::BTreeSet;
use std::time::Duration;

use super::*;

// --- `SessionConfig` carries no connection settings -------------------------

/// A config with every connection setting set to something unmistakable, so
/// a leak into `SessionConfig` is visible rather than plausible.
fn config() -> Config {
    Config {
        mqtt: Some(MqttConfig {
            host: "SECRET-HOST".to_string(),
            port: 1883,
            username: Some("SECRET-USER".to_string()),
            password: Some("SECRET-PASSWORD".to_string()),
            client_id: "SECRET-CLIENT".to_string(),
        }),
        device: DeviceConfig::Zendure {
            ip: "SECRET-IP".to_string(),
            sn: "SECRET-SERIAL".to_string(),
            poll_interval: Duration::from_secs(30),
        },
        shelly: Some(ShellyConfig {
            topic: "SECRET-TOPIC".to_string(),
            solar_phase: SolarPhase::A,
        }),
        meter: MeterConfig::Shelly,
        web: None,
        ha_publish_prefix: "SECRET-PREFIX".to_string(),
        charge_margin: PowerMargin::new(50),
        discharge_margin: PowerMargin::new(5),
        charge_start_threshold: GridPower(-100.0),
        discharge_start_threshold: GridPower(0.0),
        min_mode_duration: Duration::from_secs(10),
        min_decision_interval: Duration::from_secs(5),
        idle_timeout: Duration::from_secs(300),
        cycle_warn_threshold: 200,
        min_soc: Soc::new(10),
        max_soc: Soc::new(100),
        rte_state_path: PathBuf::from("/SECRET/rte_state.json"),
        balance_weekday: Some(Weekday::Mon),
        solar_discharge_block_threshold: SolarPower::new(0.0),
        min_idle_before_discharge: Duration::from_secs(300),
        timezone: Tz::UTC,
        mqtt_timeout: Duration::from_secs(60),
        journal_path: PathBuf::from("/SECRET/journal.db"),
        journal_retention_days: RetentionDays::new(90).unwrap(),
        log_filter: "zendure=info".to_string(),
    }
}

/// **A fixture has to be hermetic.** `config_json` is written into an
/// append-only journal and is meant to be handed around — into a replay, a
/// bug report, a test case. Anything describing how to reach a device makes
/// that unsafe, and no amount of care at the call site fixes a leak once the
/// rows are written.
#[test]
fn the_session_config_carries_no_connection_settings() {
    let json = serde_json::to_string(&config().session()).unwrap();
    for secret in [
        "SECRET-HOST",
        "SECRET-USER",
        "SECRET-PASSWORD",
        "SECRET-CLIENT",
        "SECRET-IP",
        "SECRET-SERIAL",
        "SECRET-TOPIC",
        "SECRET-PREFIX",
        "/SECRET/journal.db",
        "/SECRET/rte_state.json",
    ] {
        assert!(!json.contains(secret), "{secret} leaked into {json}");
    }
}

/// Pins the exact bytes, the way `world.rs` and `command_tests.rs` do.
///
/// Two things at once: durations are bare seconds rather than serde's
/// `{"secs":_,"nanos":_}`, so the journal reads as numbers throughout; and
/// the key set is fixed, so a knob quietly dropped from `session()` fails
/// here even if `Config` still has it.
#[test]
fn the_session_config_serializes_to_the_exact_pinned_shape() {
    assert_eq!(
        serde_json::to_string(&config().session()).unwrap(),
        r#"{"charge_margin":50,"discharge_margin":5,"charge_start_threshold":-100.0,"discharge_start_threshold":0.0,"min_mode_duration_secs":10,"min_decision_interval_secs":5,"idle_timeout_secs":300,"min_idle_before_discharge_secs":300,"cycle_warn_threshold":200,"min_soc":10,"max_soc":100,"balance_weekday":"Mon","solar_discharge_block_threshold":0.0,"mqtt_timeout_secs":60}"#
    );
}

/// It round-trips, so a replay can read back the tuning it was recorded
/// with rather than re-deriving it from the environment it happens to run in.
#[test]
fn the_session_config_round_trips() {
    let session = config().session();
    let json = serde_json::to_string(&session).unwrap();
    assert_eq!(session, serde_json::from_str(&json).unwrap());
}

/// The knobs that reach the controller are the knobs that get recorded.
/// `Controller::from_config` reads thirteen fields; `SessionConfig` carries
/// those plus `mqtt_timeout`, which `Engine` holds. Stated as a value check
/// rather than a comment so it cannot quietly stop being true.
#[test]
fn the_session_config_matches_what_the_controller_was_built_with() {
    let config = config();
    let session = config.session();

    assert_eq!(session.charge_margin, config.charge_margin);
    assert_eq!(session.discharge_margin, config.discharge_margin);
    assert_eq!(
        session.charge_start_threshold,
        config.charge_start_threshold
    );
    assert_eq!(
        session.discharge_start_threshold,
        config.discharge_start_threshold
    );
    assert_eq!(session.min_soc, config.min_soc);
    assert_eq!(session.max_soc, config.max_soc);
    assert_eq!(session.balance_weekday, config.balance_weekday);
    assert_eq!(session.cycle_warn_threshold, config.cycle_warn_threshold);
    assert_eq!(
        session.solar_discharge_block_threshold,
        config.solar_discharge_block_threshold
    );
    assert_eq!(
        session.min_mode_duration_secs,
        config.min_mode_duration.as_secs()
    );
    assert_eq!(
        session.min_decision_interval_secs,
        config.min_decision_interval.as_secs()
    );
    assert_eq!(session.idle_timeout_secs, config.idle_timeout.as_secs());
    assert_eq!(
        session.min_idle_before_discharge_secs,
        config.min_idle_before_discharge.as_secs()
    );
    assert_eq!(session.mqtt_timeout_secs, config.mqtt_timeout.as_secs());
}

// --- `from_toml_str` --------------------------------------------------------

/// The smallest file that satisfies every fatal requirement: a host, one
/// zendure device, and a Shelly topic. Every test below starts here and
/// changes exactly the one thing it means to test, so a failure is never
/// ambiguous about which knob caused it.
fn minimal_toml() -> String {
    r#"
        [mqtt]
        host = "127.0.0.1"

        [[device]]
        kind = "zendure"
        ip = "192.168.1.253"
        sn = "SN123"
        poll_interval_secs = 10

        [shelly]
        topic = "shellypro3em/status/em:0"
    "#
    .to_string()
}

#[test]
fn the_example_config_parses_with_zero_warnings() {
    let (_, warnings) = Config::from_toml_str(include_str!("../config.example.toml")).unwrap();
    assert_eq!(warnings, Vec::<String>::new(), "{warnings:?}");
}

#[test]
fn missing_mqtt_host_is_fatal() {
    // `[mqtt]` present (so the "absent" coherence check below does not fire
    // first) but empty, so the field-level check is what's being pinned.
    let toml = r#"
        [mqtt]

        [[device]]
        kind = "zendure"
        ip = "192.168.1.253"
        sn = "SN123"
        poll_interval_secs = 10

        [shelly]
        topic = "x"
    "#;
    let err = Config::from_toml_str(toml).unwrap_err();
    assert!(err.contains("mqtt.host"), "{err}");
}

/// The coherence check `missing_mqtt_host_is_fatal` deliberately routes
/// around: no `[mqtt]` table at all, with the default (Shelly) meter, is
/// fatal — the Shelly reading arrives over MQTT, so there is nothing for the
/// engine to decide against without a broker.
#[test]
fn a_shelly_meter_with_no_mqtt_table_is_fatal() {
    let toml = r#"
        [[device]]
        kind = "zendure"
        ip = "192.168.1.253"
        sn = "SN123"
        poll_interval_secs = 10

        [shelly]
        topic = "x"
    "#;
    let err = Config::from_toml_str(toml).unwrap_err();
    assert!(err.contains("[mqtt]"), "{err}");
}

#[test]
fn missing_shelly_topic_is_fatal() {
    let toml = r#"
        [mqtt]
        host = "127.0.0.1"

        [[device]]
        kind = "zendure"
        ip = "192.168.1.253"
        sn = "SN123"
        poll_interval_secs = 10
    "#;
    let err = Config::from_toml_str(toml).unwrap_err();
    assert!(err.contains("shelly.topic"), "{err}");
}

#[test]
fn missing_device_ip_is_fatal() {
    let toml = r#"
        [mqtt]
        host = "127.0.0.1"

        [[device]]
        kind = "zendure"
        sn = "SN123"
        poll_interval_secs = 10

        [shelly]
        topic = "x"
    "#;
    let err = Config::from_toml_str(toml).unwrap_err();
    assert!(err.contains("device.ip"), "{err}");
}

#[test]
fn missing_device_sn_is_fatal() {
    let toml = r#"
        [mqtt]
        host = "127.0.0.1"

        [[device]]
        kind = "zendure"
        ip = "192.168.1.253"
        poll_interval_secs = 10

        [shelly]
        topic = "x"
    "#;
    let err = Config::from_toml_str(toml).unwrap_err();
    assert!(err.contains("device.sn"), "{err}");
}

#[test]
fn a_wrong_typed_connection_setting_is_fatal() {
    let toml = r#"
        [mqtt]
        host = "127.0.0.1"
        port = "1883"

        [[device]]
        kind = "zendure"
        ip = "192.168.1.253"
        sn = "SN123"
        poll_interval_secs = 10

        [shelly]
        topic = "x"
    "#;
    let err = Config::from_toml_str(toml).unwrap_err();
    assert!(err.contains("mqtt.port"), "{err}");
}

#[test]
fn a_non_table_tuning_section_is_fatal_not_fourteen_defaults() {
    // `tuning = "x"` has to come before any `[table]` header: once one is
    // open, a bare key without its own header is a key of *that* table, not
    // of the root — the same reason `minimal_toml()` can't just be appended
    // to here the way the lenient-knob tests below append to it.
    let toml = format!("tuning = \"x\"\n{}", minimal_toml());
    let err = Config::from_toml_str(&toml).unwrap_err();
    assert!(err.contains("tuning"), "{err}");
}

#[test]
fn a_wrong_typed_tuning_knob_warns_and_defaults() {
    let toml = format!("{}\n[tuning]\nmax_soc = \"eighty\"\n", minimal_toml());
    let (config, warnings) = Config::from_toml_str(&toml).unwrap();
    assert_eq!(config.max_soc, Soc::new(100));
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("tuning.max_soc"), "{warnings:?}");
}

#[test]
fn an_unknown_key_warns_and_an_unknown_table_does_not_cascade() {
    let toml = format!(
        "{}\n[tuning]\nmni_soc = 5\n\n[extra_stuff]\nfoo = 1\nbar = 2\n",
        minimal_toml()
    );
    let (_, warnings) = Config::from_toml_str(&toml).unwrap();
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert!(
        warnings.iter().any(|w| w.contains("tuning.mni_soc")),
        "{warnings:?}"
    );
    assert!(
        warnings.iter().any(|w| w.contains("extra_stuff")
            && !w.contains("extra_stuff.foo")
            && !w.contains("extra_stuff.bar")),
        "{warnings:?}"
    );
}

#[test]
fn retention_days_zero_warns_and_keeps_ninety() {
    let toml = format!("{}\n[journal]\nretention_days = 0\n", minimal_toml());
    let (config, warnings) = Config::from_toml_str(&toml).unwrap();
    assert_eq!(
        config.journal_retention_days,
        RetentionDays::new(90).unwrap()
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("journal.retention_days")),
        "{warnings:?}"
    );
}

#[test]
fn retention_days_negative_warns_and_keeps_ninety() {
    let toml = format!("{}\n[journal]\nretention_days = -5\n", minimal_toml());
    let (config, warnings) = Config::from_toml_str(&toml).unwrap();
    assert_eq!(
        config.journal_retention_days,
        RetentionDays::new(90).unwrap()
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("journal.retention_days")),
        "{warnings:?}"
    );
}

#[test]
fn invalid_toml_is_fatal() {
    assert!(Config::from_toml_str("this is [ not valid").is_err());
}

/// The non-obvious one: TOML distinguishes integers from floats, and a
/// person copying `200` out of a YAML template or an old `.env` file into
/// `charge_start_threshold` must not silently disable the solar guard by
/// landing on the wrong branch of that distinction.
#[test]
fn an_integer_parses_where_a_float_is_expected() {
    let as_integer = format!(
        "{}\n[tuning]\ncharge_start_threshold = -100\n",
        minimal_toml()
    );
    let as_float = format!(
        "{}\n[tuning]\ncharge_start_threshold = -100.0\n",
        minimal_toml()
    );

    let (from_int, int_warnings) = Config::from_toml_str(&as_integer).unwrap();
    let (from_float, float_warnings) = Config::from_toml_str(&as_float).unwrap();

    assert_eq!(from_int.charge_start_threshold, GridPower(-100.0));
    assert_eq!(from_float.charge_start_threshold, GridPower(-100.0));
    assert_eq!(int_warnings, Vec::<String>::new(), "{int_warnings:?}");
    assert_eq!(float_warnings, Vec::<String>::new(), "{float_warnings:?}");
}

#[test]
fn balance_weekday_accepts_a_weekday_and_every_spelling_of_off() {
    for (raw, expected) in [
        ("Tue", Some(Weekday::Tue)),
        ("none", None),
        ("None", None),
        ("off", None),
        ("OFF", None),
        ("", None),
    ] {
        let toml = format!(
            "{}\n[tuning]\nbalance_weekday = \"{raw}\"\n",
            minimal_toml()
        );
        let (config, warnings) = Config::from_toml_str(&toml).unwrap();
        assert_eq!(config.balance_weekday, expected, "raw={raw:?}");
        assert_eq!(warnings, Vec::<String>::new(), "raw={raw:?} {warnings:?}");
    }
}

#[test]
fn debug_redacts_the_password() {
    let toml = r#"
        [mqtt]
        host = "127.0.0.1"
        password = "hunter2"

        [[device]]
        kind = "zendure"
        ip = "192.168.1.253"
        sn = "SN123"
        poll_interval_secs = 10

        [shelly]
        topic = "x"
    "#;
    let (config, _) = Config::from_toml_str(toml).unwrap();
    let debug = format!("{config:?}");
    assert!(debug.contains("<redacted>"), "{debug}");
    assert!(!debug.contains("hunter2"), "{debug}");
}

/// This is what stops the config file, the journal's `config_json`, a
/// fixture's `session.config` and `replay --set`'s knob names becoming four
/// vocabularies that quietly drift apart.
#[test]
fn the_tuning_table_has_exactly_the_session_config_keys() {
    let root: toml::Table = include_str!("../config.example.toml").parse().unwrap();
    let tuning = root["tuning"].as_table().unwrap();
    let toml_keys: BTreeSet<&str> = tuning.keys().map(String::as_str).collect();

    let session_json = serde_json::to_value(SessionConfig::test_default()).unwrap();
    let session_keys: BTreeSet<&str> = session_json
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();

    assert_eq!(toml_keys, session_keys);
}

/// Production's values, pinned as an explicit `Config` literal rather than
/// reached for a helper that would hide which value belongs to which field —
/// thirty fields, written out in full. If this ever disagrees with
/// `config.example.toml`, the example file is what is wrong: it is supposed
/// to be the record of what production runs, not the other way around.
///
/// This used to compare `config.example.toml` against `Config::from_vars` fed
/// production's actual environment variables (the twelve the ansible unit
/// file set) — proof that the environment path and the TOML path agreed
/// before the environment path was deleted. That proof lives in git history
/// now (`from_env`/`from_vars` are gone); this literal is what pins it in the
/// tree.
#[test]
fn the_example_config_is_what_production_runs() {
    let production = Config {
        mqtt: Some(MqttConfig {
            host: "127.0.0.1".to_string(),
            port: 1883,
            username: None,
            password: None,
            client_id: "odroid".to_string(),
        }),
        device: DeviceConfig::Zendure {
            ip: "192.168.1.253".to_string(),
            sn: "HEC4NENCN490270".to_string(),
            poll_interval: Duration::from_secs(10),
        },
        shelly: Some(ShellyConfig {
            topic: "shellypro3em-a4f00fcfcc18/status/em:0".to_string(),
            solar_phase: SolarPhase::A,
        }),
        meter: MeterConfig::Shelly,
        web: None,
        ha_publish_prefix: "zendure".to_string(),
        charge_margin: PowerMargin::new(50),
        discharge_margin: PowerMargin::new(5),
        charge_start_threshold: GridPower(-100.0),
        discharge_start_threshold: GridPower(0.0),
        min_mode_duration: Duration::from_secs(10),
        min_decision_interval: Duration::from_secs(5),
        idle_timeout: Duration::from_secs(300),
        cycle_warn_threshold: 200,
        min_soc: Soc::new(10),
        max_soc: Soc::new(80),
        balance_weekday: Some(Weekday::Mon),
        solar_discharge_block_threshold: SolarPower::new(200.0),
        min_idle_before_discharge: Duration::from_secs(300),
        timezone: "Europe/Amsterdam".parse().unwrap(),
        mqtt_timeout: Duration::from_secs(60),
        journal_path: PathBuf::from("/var/lib/zendure/journal.db"),
        journal_retention_days: RetentionDays::new(30).unwrap(),
        rte_state_path: PathBuf::from("/var/lib/zendure/rte_state.json"),
        log_filter: "zendure=info".to_string(),
    };

    let (from_toml, warnings) =
        Config::from_toml_str(include_str!("../config.example.toml")).unwrap();

    assert_eq!(warnings, Vec::<String>::new(), "{warnings:?}");
    assert_eq!(production, from_toml);
}

// --- brokerless / virtual / synthetic ---------------------------------------

/// The smallest brokerless file: no `[mqtt]`, no `[shelly]`, a virtual device,
/// a synthetic meter. The mirror image of `minimal_toml()` — every test below
/// starts here and changes exactly one thing.
fn minimal_virtual_toml() -> String {
    r#"
        [[device]]
        kind = "virtual"
        id = "sim"
        packs = [10000.0]
        soc = 50
        charge_efficiency = 95.0
        discharge_efficiency = 95.0

        [meter]
        kind = "synthetic"
        base_load = 500
        solar_peak = 3000
    "#
    .to_string()
}

#[test]
fn a_brokerless_virtual_config_parses_with_zero_warnings() {
    let (config, warnings) = Config::from_toml_str(&minimal_virtual_toml()).unwrap();
    assert_eq!(warnings, Vec::<String>::new(), "{warnings:?}");
    assert!(config.mqtt.is_none());
    assert!(config.shelly.is_none());
    assert_eq!(
        config.meter,
        MeterConfig::Synthetic {
            base_load: Watts(500),
            solar_peak: Watts(3000),
        }
    );
    assert_eq!(
        config.device,
        DeviceConfig::Virtual {
            id: "sim".to_string(),
            packs: vec![WattHours(10_000.0)],
            soc: Soc::new(50),
            charge_efficiency: Efficiency::new(95.0),
            discharge_efficiency: Efficiency::new(95.0),
        }
    );
}

#[test]
fn a_synthetic_meter_with_a_zendure_device_is_fatal() {
    let toml = r#"
        [[device]]
        kind = "zendure"
        ip = "192.168.1.253"
        sn = "SN123"
        poll_interval_secs = 10

        [meter]
        kind = "synthetic"
        base_load = 500
        solar_peak = 3000
    "#;
    let err = Config::from_toml_str(toml).unwrap_err();
    assert!(err.contains("synthetic"), "{err}");
    assert!(err.contains("virtual"), "{err}");
}

#[test]
fn an_unknown_meter_kind_is_fatal() {
    let toml = format!("{}\n[meter]\nkind = \"telepathic\"\n", minimal_toml());
    let err = Config::from_toml_str(&toml).unwrap_err();
    assert!(err.contains("meter.kind"), "{err}");
}

#[test]
fn missing_device_packs_is_fatal_for_a_virtual_device() {
    let toml = r#"
        [[device]]
        kind = "virtual"
        id = "sim"
        soc = 50
        charge_efficiency = 95.0
        discharge_efficiency = 95.0

        [meter]
        kind = "synthetic"
        base_load = 500
        solar_peak = 3000
    "#;
    let err = Config::from_toml_str(toml).unwrap_err();
    assert!(err.contains("device.packs"), "{err}");
}

#[test]
fn an_unknown_device_kind_is_still_fatal() {
    let toml = r#"
        [[device]]
        kind = "peblar"
        ip = "x"
        sn = "y"
        poll_interval_secs = 10

        [mqtt]
        host = "127.0.0.1"

        [shelly]
        topic = "x"
    "#;
    let err = Config::from_toml_str(toml).unwrap_err();
    assert!(err.contains("device.kind"), "{err}");
}

/// `[mqtt]` may still be configured for a synthetic meter — e.g. to publish
/// over a real broker while simulating the battery — so long as the device is
/// virtual. Only the *combination* "Shelly meter, no mqtt" and "synthetic
/// meter, non-virtual device" are refused.
#[test]
fn mqtt_alongside_a_synthetic_meter_is_allowed() {
    let toml = format!("[mqtt]\nhost = \"127.0.0.1\"\n\n{}", minimal_virtual_toml());
    let (config, warnings) = Config::from_toml_str(&toml).unwrap();
    assert_eq!(warnings, Vec::<String>::new(), "{warnings:?}");
    assert!(config.mqtt.is_some());
}

// --- `[web]` -----------------------------------------------------------------

#[test]
fn no_web_table_means_no_dashboard_server() {
    let (config, warnings) = Config::from_toml_str(&minimal_toml()).unwrap();
    assert_eq!(warnings, Vec::<String>::new(), "{warnings:?}");
    assert!(config.web.is_none());
}

#[test]
fn a_web_table_with_defaults_binds_localhost_8080() {
    let toml = format!("{}\n[web]\n", minimal_toml());
    let (config, warnings) = Config::from_toml_str(&toml).unwrap();
    assert_eq!(warnings, Vec::<String>::new(), "{warnings:?}");
    assert_eq!(
        config.web,
        Some(WebConfig {
            bind_address: std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
            port: 8080,
        })
    );
}

#[test]
fn a_web_table_overrides_bind_address_and_port() {
    let toml = format!(
        "{}\n[web]\nbind_address = \"0.0.0.0\"\nport = 9000\n",
        minimal_toml()
    );
    let (config, warnings) = Config::from_toml_str(&toml).unwrap();
    assert_eq!(warnings, Vec::<String>::new(), "{warnings:?}");
    assert_eq!(
        config.web,
        Some(WebConfig {
            bind_address: std::net::IpAddr::V4(std::net::Ipv4Addr::new(0, 0, 0, 0)),
            port: 9000,
        })
    );
}

#[test]
fn an_unparseable_web_bind_address_is_fatal() {
    let toml = format!("{}\n[web]\nbind_address = \"not-an-ip\"\n", minimal_toml());
    let err = Config::from_toml_str(&toml).unwrap_err();
    assert!(err.contains("web.bind_address"), "{err}");
}

#[test]
fn a_wrong_typed_web_port_is_fatal() {
    let toml = format!("{}\n[web]\nport = \"9000\"\n", minimal_toml());
    let err = Config::from_toml_str(&toml).unwrap_err();
    assert!(err.contains("web.port"), "{err}");
}

/// `[web]` is a connection setting, like `[mqtt]`/`[shelly]` — not a decision
/// input, so it must never reach a replay fixture.
#[test]
fn web_never_leaks_into_session_config() {
    let toml = format!("{}\n[web]\nport = 9999\n", minimal_toml());
    let (config, _) = Config::from_toml_str(&toml).unwrap();
    let json = serde_json::to_string(&config.session()).unwrap();
    assert!(!json.contains("9999"), "{json}");
}
