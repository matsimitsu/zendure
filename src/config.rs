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

fn parse_weekday(s: &str) -> Result<Weekday, String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "mon" | "monday" => Ok(Weekday::Mon),
        "tue" | "tuesday" => Ok(Weekday::Tue),
        "wed" | "wednesday" => Ok(Weekday::Wed),
        "thu" | "thursday" => Ok(Weekday::Thu),
        "fri" | "friday" => Ok(Weekday::Fri),
        "sat" | "saturday" => Ok(Weekday::Sat),
        "sun" | "sunday" => Ok(Weekday::Sun),
        _ => Err(
            "BALANCE_WEEKDAY must be one of Mon, Tue, Wed, Thu, Fri, Sat, Sun, or 'none'"
                .to_string(),
        ),
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

impl Config {
    /// The tuning knobs, without anything that says how to reach a device.
    pub fn session(&self) -> SessionConfig {
        SessionConfig {
            charge_margin: self.charge_margin,
            discharge_margin: self.discharge_margin,
            charge_start_threshold: self.charge_start_threshold,
            discharge_start_threshold: self.discharge_start_threshold,
            min_mode_duration_secs: self.min_mode_duration.as_secs(),
            min_decision_interval_secs: self.min_decision_interval.as_secs(),
            idle_timeout_secs: self.idle_timeout.as_secs(),
            min_idle_before_discharge_secs: self.min_idle_before_discharge.as_secs(),
            cycle_warn_threshold: self.cycle_warn_threshold,
            min_soc: self.min_soc,
            max_soc: self.max_soc,
            balance_weekday: self.balance_weekday,
            solar_discharge_block_threshold: self.solar_discharge_block_threshold,
            mqtt_timeout_secs: self.mqtt_timeout.as_secs(),
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
                    Some(parse_weekday(&raw)?)
                }
            },
            solar_phase: SolarPhase::parse(
                &env::var("SOLAR_PHASE").unwrap_or_else(|_| "A".to_string()),
            )?,
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
                env::var("JOURNAL_PATH")
                    .unwrap_or_else(|_| "/var/lib/zendure/journal.db".to_string()),
            ),
            journal_retention_days: retention_from_env(),
        })
    }
}
