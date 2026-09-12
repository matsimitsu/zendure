use std::env;
use std::path::PathBuf;
use std::time::Duration;

use chrono::Weekday;
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

// `SolarPhase` is the meter's own idea of how many wires it watches, so it
// belongs to the adapter that reads them. Parsing `SOLAR_PHASE` is still this
// file's job — reading the environment is what `Config` is for.
use crate::source::shelly::SolarPhase;
use crate::units::{GridPower, PowerMargin, RetentionDays, Soc, SolarPower};

/// Where the journal lives unless `JOURNAL_PATH` says otherwise.
///
/// Named so the daemon's default and `export --db`'s default are one string
/// rather than two copies that drift. It is *only* a default, not the effective
/// path: `export` reads no environment at all — that is what makes a fixture
/// reproducible from a copied database — so a deployment that sets
/// `JOURNAL_PATH` has to pass `--db` as well. The alternative, having the tool
/// read one variable, would make "reads no configuration" a claim with an
/// exception in it.
pub const DEFAULT_JOURNAL_PATH: &str = "/var/lib/zendure/journal.db";

fn parse_weekday(s: &str) -> Result<Weekday, String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "mon" | "monday" => Ok(Weekday::Mon),
        "tue" | "tuesday" => Ok(Weekday::Tue),
        "wed" | "wednesday" => Ok(Weekday::Wed),
        "thu" | "thursday" => Ok(Weekday::Thu),
        "fri" | "friday" => Ok(Weekday::Fri),
        "sat" | "saturday" => Ok(Weekday::Sat),
        "sun" | "sunday" => Ok(Weekday::Sun),
        // Names what is wrong, not where it was read from — the caller knows
        // whether that was an environment variable or a TOML key.
        _ => Err("must be one of Mon, Tue, Wed, Thu, Fri, Sat, Sun, or 'none'".to_string()),
    }
}

/// Reads an env var (or `default`) as whole seconds into a `Duration`, keeping
/// the original error-message shape (`"<KEY> must be a number"`).
fn secs_from_env(key: &str, default: &str) -> Result<Duration, String> {
    let secs = env::var(key)
        .unwrap_or_else(|_| default.to_string())
        .parse::<u64>()
        .map_err(|_| format!("{key} must be a number"))?;
    Ok(Duration::from_secs(secs))
}

/// Reads an env var (or `default`) as whole minutes into a `Duration`, keeping
/// the original error-message shape (`"<KEY> must be a number"`).
fn minutes_from_env(key: &str, default: &str) -> Result<Duration, String> {
    let minutes = env::var(key)
        .unwrap_or_else(|_| default.to_string())
        .parse::<u64>()
        .map_err(|_| format!("{key} must be a number"))?;
    Ok(Duration::from_secs(minutes * 60))
}

/// Reads `JOURNAL_RETENTION_DAYS`, **warning and falling back rather than
/// failing**.
///
/// Every other knob in this file is strict, and this one deliberately is not.
/// The journal is a logging concern, and `journal.rs` holds the line that a
/// logging failure must never become a control failure — an unusable
/// `JOURNAL_PATH` already degrades to "no journal" for exactly this reason.
/// Parsing this strictly put a typo in a *logging* variable on the path that
/// exits `main`, so systemd would restart-loop while the battery held whatever
/// command it last received. Loud and running beats silent and stopped.
fn retention_from_env() -> RetentionDays {
    const DEFAULT: i64 = 90;
    let fallback = RetentionDays::new(DEFAULT).expect("90 is a valid retention");

    let Ok(raw) = env::var("JOURNAL_RETENTION_DAYS") else {
        return fallback;
    };
    match raw
        .parse::<i64>()
        .map_err(|e| e.to_string())
        .and_then(RetentionDays::new)
    {
        Ok(days) => days,
        Err(e) => {
            tracing::warn!("JOURNAL_RETENTION_DAYS={raw} ignored ({e}); keeping {DEFAULT} days");
            fallback
        }
    }
}

#[allow(dead_code)]
pub struct Config {
    pub mqtt_host: String,
    pub mqtt_port: u16,
    pub mqtt_username: Option<String>,
    pub mqtt_password: Option<String>,
    pub mqtt_client_id: String,
    pub zendure_ip: String,
    pub zendure_sn: String,
    pub shelly_topic: String,
    pub ha_publish_prefix: String,
    pub zendure_poll_interval: Duration,
    /// Safety margin subtracted from charge power to avoid grid import
    pub charge_margin: PowerMargin,
    /// Safety margin subtracted from discharge power
    pub discharge_margin: PowerMargin,
    /// Grid power below this triggers charging (negative = exporting)
    pub charge_start_threshold: GridPower,
    /// Grid power above this triggers discharging (positive = importing)
    pub discharge_start_threshold: GridPower,
    /// Minimum time before charge↔discharge toggle
    pub min_mode_duration: Duration,
    /// Minimum time between decisions (API protection)
    pub min_decision_interval: Duration,
    /// Idle duration before entering standby
    pub idle_timeout: Duration,
    /// Warn when daily cycle count reaches this threshold
    pub cycle_warn_threshold: u32,
    /// Minimum SOC before discharge is blocked (default 10)
    pub min_soc: Soc,
    /// Maximum SOC before charging is blocked (default 100)
    pub max_soc: Soc,
    /// Weekday on which `max_soc` is raised to 100% for a periodic cell-balancing
    /// full charge. `None` disables the override (default: Monday).
    pub balance_weekday: Option<Weekday>,
    /// Which meter phase the solar inverter feeds into.
    pub solar_phase: SolarPhase,
    /// Solar inverter export on `solar_phase` at or above which discharge is
    /// skipped, so large loads (e.g. EV charging) pull from grid+solar instead
    /// of draining the home battery. 0 disables the guard (default).
    pub solar_discharge_block_threshold: SolarPower,
    /// Minimum idle duration before discharge is allowed (prevents charge→discharge oscillation)
    pub min_idle_before_discharge: Duration,
    /// IANA timezone (e.g. Europe/Amsterdam)
    pub timezone: Tz,
    /// Time without MQTT updates before forcing idle (safety failsafe)
    pub mqtt_timeout: Duration,
    /// SQLite journal of events and decisions.
    pub journal_path: PathBuf,
    /// How long journal rows are kept. The only thing bounding the file.
    pub journal_retention_days: RetentionDays,
}

/// The decision-relevant half of [`Config`], recorded once per session so a
/// replay knows what tuning produced a row.
///
/// Built by hand rather than derived on `Config`, and that is the point:
/// connection settings are not decision inputs, so a fixture carrying them
/// would be neither hermetic nor safe to pass around. Durations are whole
/// seconds because `Duration`'s own serde emits `{"secs":_,"nanos":_}`, which
/// reads badly next to every other bare number in the journal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionConfig {
    pub charge_margin: PowerMargin,
    pub discharge_margin: PowerMargin,
    pub charge_start_threshold: GridPower,
    pub discharge_start_threshold: GridPower,
    pub min_mode_duration_secs: u64,
    pub min_decision_interval_secs: u64,
    pub idle_timeout_secs: u64,
    pub min_idle_before_discharge_secs: u64,
    pub cycle_warn_threshold: u32,
    pub min_soc: Soc,
    pub max_soc: Soc,
    pub balance_weekday: Option<Weekday>,
    pub solar_discharge_block_threshold: SolarPower,
    pub mqtt_timeout_secs: u64,
}

impl SessionConfig {
    /// The permissive tuning tests decide under: no decision-interval cooldown,
    /// generous margins, no balance day.
    ///
    /// One list, shared by `Controller::test_default` and by any test that
    /// needs a fixture's `session.config` — which is the point. A replay is
    /// only a fair comparison if it runs the same knobs the recording did, and
    /// two hand-written copies of fourteen numbers would eventually disagree
    /// about one of them and make a passing `--verify` mean nothing.
    #[cfg(test)]
    pub(crate) fn test_default() -> Self {
        Self {
            charge_margin: PowerMargin::new(50),
            discharge_margin: PowerMargin::new(5),
            charge_start_threshold: GridPower(-100.0),
            discharge_start_threshold: GridPower(0.0),
            min_mode_duration_secs: 10,
            min_decision_interval_secs: 0,
            idle_timeout_secs: 5 * 60,
            min_idle_before_discharge_secs: 300,
            cycle_warn_threshold: 200,
            min_soc: Soc::new(10),
            max_soc: Soc::new(100),
            balance_weekday: None,
            solar_discharge_block_threshold: SolarPower::ZERO,
            mqtt_timeout_secs: 120,
        }
    }
}

impl Config {
    /// The tuning knobs, without anything that says how to reach a device.
    ///
    /// Destructures `self` exhaustively — no `..` — so that adding a field to
    /// `Config` is a compile error here rather than a knob that silently stops
    /// being recorded. That failure would be invisible in the worst way: the
    /// thing you open `config_json` to find out would be the thing missing from
    /// it. `Controller::restore` takes the same precaution for the same reason.
    ///
    /// Everything bound to `_` is deliberate, and the rule is one line long:
    /// a fixture has to be hermetic, so nothing that says how to *reach* a
    /// device belongs in it. `timezone` and `solar_phase` are the two that look
    /// like tuning and are not — every journaled `Event` already carries a
    /// resolved `Clock` and an already-normalised solar figure, so a replay
    /// never re-derives either.
    pub fn session(&self) -> SessionConfig {
        let Config {
            // Connection settings: how to reach things, not what to decide.
            mqtt_host: _,
            mqtt_port: _,
            mqtt_username: _,
            mqtt_password: _,
            mqtt_client_id: _,
            zendure_ip: _,
            zendure_sn: _,
            shelly_topic: _,
            ha_publish_prefix: _,
            zendure_poll_interval: _,
            journal_path: _,
            journal_retention_days: _,
            // Resolved into every event before it is journaled.
            timezone: _,
            solar_phase: _,
            // The decision knobs.
            charge_margin,
            discharge_margin,
            charge_start_threshold,
            discharge_start_threshold,
            min_mode_duration,
            min_decision_interval,
            idle_timeout,
            cycle_warn_threshold,
            min_soc,
            max_soc,
            balance_weekday,
            solar_discharge_block_threshold,
            min_idle_before_discharge,
            mqtt_timeout,
        } = self;

        SessionConfig {
            charge_margin: *charge_margin,
            discharge_margin: *discharge_margin,
            charge_start_threshold: *charge_start_threshold,
            discharge_start_threshold: *discharge_start_threshold,
            min_mode_duration_secs: min_mode_duration.as_secs(),
            min_decision_interval_secs: min_decision_interval.as_secs(),
            idle_timeout_secs: idle_timeout.as_secs(),
            min_idle_before_discharge_secs: min_idle_before_discharge.as_secs(),
            cycle_warn_threshold: *cycle_warn_threshold,
            min_soc: *min_soc,
            max_soc: *max_soc,
            balance_weekday: *balance_weekday,
            solar_discharge_block_threshold: *solar_discharge_block_threshold,
            mqtt_timeout_secs: mqtt_timeout.as_secs(),
        }
    }
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let mqtt_host = env::var("MQTT_HOST").map_err(|_| "MQTT_HOST is required")?;
        let zendure_ip = env::var("ZENDURE_IP").map_err(|_| "ZENDURE_IP is required")?;
        let zendure_sn = env::var("ZENDURE_SN").map_err(|_| "ZENDURE_SN is required")?;

        let mqtt_port = env::var("MQTT_PORT")
            .unwrap_or_else(|_| "1883".to_string())
            .parse::<u16>()
            .map_err(|_| "MQTT_PORT must be a valid port number")?;

        Ok(Config {
            mqtt_host,
            mqtt_port,
            mqtt_username: env::var("MQTT_USERNAME").ok(),
            mqtt_password: env::var("MQTT_PASSWORD").ok(),
            mqtt_client_id: env::var("MQTT_CLIENT_ID")
                .unwrap_or_else(|_| "zendure-controller".to_string()),
            zendure_ip,
            zendure_sn,
            shelly_topic: env::var("SHELLY_TOPIC").map_err(|_| "SHELLY_TOPIC is required")?,
            ha_publish_prefix: env::var("HA_PUBLISH_PREFIX")
                .unwrap_or_else(|_| "zendure".to_string()),
            zendure_poll_interval: secs_from_env("ZENDURE_POLL_INTERVAL", "10")?,
            charge_margin: PowerMargin::new(
                env::var("CHARGE_MARGIN")
                    .unwrap_or_else(|_| "50".to_string())
                    .parse::<u32>()
                    .map_err(|_| "CHARGE_MARGIN must be a number")?,
            ),
            discharge_margin: PowerMargin::new(
                env::var("DISCHARGE_MARGIN")
                    .unwrap_or_else(|_| "5".to_string())
                    .parse::<u32>()
                    .map_err(|_| "DISCHARGE_MARGIN must be a number")?,
            ),
            charge_start_threshold: GridPower(
                env::var("CHARGE_START_THRESHOLD")
                    .unwrap_or_else(|_| "-100.0".to_string())
                    .parse::<f64>()
                    .map_err(|_| "CHARGE_START_THRESHOLD must be a number")?,
            ),
            discharge_start_threshold: GridPower(
                env::var("DISCHARGE_START_THRESHOLD")
                    .unwrap_or_else(|_| "0.0".to_string())
                    .parse::<f64>()
                    .map_err(|_| "DISCHARGE_START_THRESHOLD must be a number")?,
            ),
            min_mode_duration: secs_from_env("MIN_MODE_DURATION", "10")?,
            min_decision_interval: secs_from_env("MIN_DECISION_INTERVAL", "5")?,
            idle_timeout: minutes_from_env("IDLE_TIMEOUT_MINUTES", "5")?,
            cycle_warn_threshold: env::var("CYCLE_WARN_THRESHOLD")
                .unwrap_or_else(|_| "200".to_string())
                .parse::<u32>()
                .map_err(|_| "CYCLE_WARN_THRESHOLD must be a number")?,
            min_soc: Soc::new(
                env::var("MIN_SOC")
                    .unwrap_or_else(|_| "10".to_string())
                    .parse::<u32>()
                    .map_err(|_| "MIN_SOC must be a number")?,
            ),
            max_soc: Soc::new(
                env::var("MAX_SOC")
                    .unwrap_or_else(|_| "100".to_string())
                    .parse::<u32>()
                    .map_err(|_| "MAX_SOC must be a number")?,
            ),
            balance_weekday: {
                let raw = env::var("BALANCE_WEEKDAY").unwrap_or_else(|_| "mon".to_string());
                if matches!(
                    raw.trim().to_ascii_lowercase().as_str(),
                    "none" | "off" | ""
                ) {
                    None
                } else {
                    Some(parse_weekday(&raw).map_err(|e| format!("BALANCE_WEEKDAY {e}"))?)
                }
            },
            solar_phase: SolarPhase::parse(
                &env::var("SOLAR_PHASE").unwrap_or_else(|_| "A".to_string()),
            )
            .map_err(|e| format!("SOLAR_PHASE {e}"))?,
            solar_discharge_block_threshold: SolarPower::new(
                env::var("SOLAR_DISCHARGE_BLOCK_THRESHOLD")
                    .unwrap_or_else(|_| "0".to_string())
                    .parse::<f64>()
                    .map_err(|_| "SOLAR_DISCHARGE_BLOCK_THRESHOLD must be a number")?,
            ),
            min_idle_before_discharge: secs_from_env("MIN_IDLE_BEFORE_DISCHARGE", "300")?,
            timezone: env::var("TIMEZONE")
                .unwrap_or_else(|_| "UTC".to_string())
                .parse::<Tz>()
                .map_err(|_| "TIMEZONE must be a valid IANA timezone (e.g. Europe/Amsterdam)")?,
            mqtt_timeout: secs_from_env("MQTT_TIMEOUT", "60")?,
            journal_path: PathBuf::from(
                env::var("JOURNAL_PATH").unwrap_or_else(|_| DEFAULT_JOURNAL_PATH.to_string()),
            ),
            journal_retention_days: retention_from_env(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A config with every connection setting set to something unmistakable, so
    /// a leak into `SessionConfig` is visible rather than plausible.
    fn config() -> Config {
        Config {
            mqtt_host: "SECRET-HOST".to_string(),
            mqtt_port: 1883,
            mqtt_username: Some("SECRET-USER".to_string()),
            mqtt_password: Some("SECRET-PASSWORD".to_string()),
            mqtt_client_id: "SECRET-CLIENT".to_string(),
            zendure_ip: "SECRET-IP".to_string(),
            zendure_sn: "SECRET-SERIAL".to_string(),
            shelly_topic: "SECRET-TOPIC".to_string(),
            ha_publish_prefix: "SECRET-PREFIX".to_string(),
            zendure_poll_interval: Duration::from_secs(30),
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
            balance_weekday: Some(Weekday::Mon),
            solar_phase: SolarPhase::A,
            solar_discharge_block_threshold: SolarPower::new(0.0),
            min_idle_before_discharge: Duration::from_secs(300),
            timezone: Tz::UTC,
            mqtt_timeout: Duration::from_secs(60),
            journal_path: PathBuf::from("/SECRET/journal.db"),
            journal_retention_days: RetentionDays::new(90).unwrap(),
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
}
