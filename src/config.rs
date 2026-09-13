use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::Weekday;
use chrono_tz::Tz;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

// `SolarPhase` is the meter's own idea of how many wires it watches, so it
// belongs to the adapter that reads them. Parsing `SOLAR_PHASE` is still this
// file's job — reading the environment is what `Config` is for.
use crate::source::shelly::SolarPhase;
use crate::units::{
    Efficiency, GridPower, PowerMargin, RetentionDays, Soc, SolarPower, WattHours, Watts,
};

/// Where the journal lives unless `JOURNAL_PATH` says otherwise.
///
/// Only a default, not the effective path: `export` reads no environment at
/// all, so a deployment that sets `JOURNAL_PATH` must pass `--db` too.
pub const DEFAULT_JOURNAL_PATH: &str = "/var/lib/zendure/journal.db";

/// Where the rolling round-trip-efficiency window is persisted. Not `/tmp`:
/// the window is 24 hours long and `/tmp` clears on boot, which would
/// silently rebuild it from scratch every restart.
pub const DEFAULT_RTE_STATE_PATH: &str = "/var/lib/zendure/rte_state.json";

/// Where `--config` reads from unless told otherwise.
///
/// Also `cli.rs`'s default for the daemon (and `--check`) when `--config` is
/// not given, so this one string is the only place that default is spelled.
pub const DEFAULT_CONFIG_PATH: &str = "/etc/zendure/config.toml";

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

/// `[clock] timezone`, wrapping `chrono_tz::Tz`.
///
/// Hand-written so the error names an example (`e.g. Europe/Amsterdam`);
/// `chrono-tz`'s own `serde` feature says only "not a valid timezone".
#[derive(Debug, Clone, Copy)]
struct TimezoneName(Tz);

impl<'de> Deserialize<'de> for TimezoneName {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?
            .parse::<Tz>()
            .map(TimezoneName)
            .map_err(|_| {
                serde::de::Error::custom("must be a valid IANA timezone (e.g. Europe/Amsterdam)")
            })
    }
}

/// `[tuning] balance_weekday`. `"none"`, `"off"` or `""` (case-insensitive,
/// trimmed) disable the periodic cell-balancing full charge; anything else
/// parses as a weekday via [`parse_weekday`].
#[derive(Debug, Clone, Copy)]
struct BalanceDay(Option<Weekday>);

impl<'de> Deserialize<'de> for BalanceDay {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if matches!(s.trim().to_ascii_lowercase().as_str(), "none" | "off" | "") {
            Ok(BalanceDay(None))
        } else {
            parse_weekday(&s)
                .map(|w| BalanceDay(Some(w)))
                .map_err(serde::de::Error::custom)
        }
    }
}

/// Consumes a parsed TOML table one key at a time; every accessor removes
/// the key it reads, so whatever remains is by construction unrecognised
/// and [`Taker::finish`] turns it into warnings. A wrong-typed *leaf* can
/// fall back to a default, but a wrong-typed *container* has nothing underneath it to
/// fall back to, so it must fail once, loudly, rather than warn-and-default many times
/// over.
struct Taker {
    root: toml::Table,
    /// Dotted paths of every table this reader walked into. `finish` needs it
    /// to tell an unknown key inside a known section (`tuning.mni_soc`) from a
    /// section the schema never mentions (`[extra_stuff]`, named once): once
    /// the known keys are drained both are just "a table with something left".
    known_tables: HashSet<String>,
    /// Warnings from `lenient` leaves whose value was present but the wrong
    /// shape. `finish` appends the unknown-key warnings to these and hands
    /// back one combined list, so a caller has to look in only one place.
    warnings: Vec<String>,
}

impl Taker {
    fn new(root: toml::Table) -> Self {
        Taker {
            root,
            known_tables: HashSet::new(),
            warnings: Vec::new(),
        }
    }

    /// Removes and returns the value at `path`, or `None` if any segment is
    /// absent. Descending through a present-but-not-a-table value is the one
    /// error every caller shares, so it is handled here rather than in each.
    fn take(&mut self, path: &str) -> Result<Option<toml::Value>, String> {
        let mut segments = path.split('.');
        let leaf = segments.next_back().expect("path is never empty");

        let mut table = &mut self.root;
        let mut prefix = String::new();
        for segment in segments {
            prefix = if prefix.is_empty() {
                segment.to_string()
            } else {
                format!("{prefix}.{segment}")
            };
            self.known_tables.insert(prefix.clone());

            match table.get_mut(segment) {
                None => return Ok(None),
                Some(toml::Value::Table(t)) => table = t,
                Some(v) => {
                    return Err(format!("{prefix} is not a table, found {}", v.type_str()));
                }
            }
        }
        Ok(table.remove(leaf))
    }

    /// A connection setting with no default: absent or the wrong type is
    /// fatal either way, so the only thing this adds over `take` is the
    /// error's wording.
    fn required<T: DeserializeOwned>(&mut self, path: &str) -> Result<T, String> {
        match self.take(path)? {
            None => Err(format!("{path} is required")),
            Some(v) => v.try_into::<T>().map_err(|e| format!("{path}: {e}")),
        }
    }

    /// A connection setting with a caller-applied default (e.g.
    /// `.unwrap_or(1883)`). Absence takes the default, but a value that is
    /// *present* and the wrong type is still fatal: `port = "1883"` is a
    /// typo worth stopping for, not a knob worth guessing past.
    fn optional<T: DeserializeOwned>(&mut self, path: &str) -> Result<Option<T>, String> {
        match self.take(path)? {
            None => Ok(None),
            Some(v) => v
                .try_into::<T>()
                .map(Some)
                .map_err(|e| format!("{path}: {e}")),
        }
    }

    /// A tuning knob: absent or wrong-shaped, `default` is used and the
    /// caller finds out via the returned warnings. Warning rather than
    /// failing is deliberate: `Config` is read at startup and systemd
    /// restarts a failed unit, so a strict parse would restart-loop the daemon while
    /// the battery held its last command.
    fn lenient<T: DeserializeOwned + std::fmt::Debug>(
        &mut self,
        path: &str,
        default: T,
    ) -> Result<T, String> {
        match self.take(path)? {
            None => Ok(default),
            Some(v) => match v.try_into::<T>() {
                Ok(v) => Ok(v),
                Err(e) => {
                    // `toml`'s errors carry a multi-line span, which reads
                    // badly inside a one-line warning and worse in a systemd
                    // journal that splits on newlines. The first line is the
                    // part that names what was wrong.
                    let reason = e.to_string();
                    let reason = reason.lines().next().unwrap_or("invalid").trim();
                    self.warnings
                        .push(format!("{path} ignored ({reason}); keeping {default:?}"));
                    Ok(default)
                }
            },
        }
    }

    /// Whether a top-level table is present, without removing anything from
    /// it, checked *before* any field in it is read. `mqtt`, `shelly` and
    /// `meter` need this: their presence, not a flag inside them, selects a
    /// backend, and `known_tables` (which only records tables walked *into*) can't
    /// answer that for an absent one. A present-but-not-a-table key is still fatal.
    fn has_table(&self, name: &str) -> Result<bool, String> {
        match self.root.get(name) {
            None => Ok(false),
            Some(toml::Value::Table(_)) => Ok(true),
            Some(v) => Err(format!("{name} is not a table, found {}", v.type_str())),
        }
    }

    /// Turns whatever is left after every known key is taken into one
    /// warning per surviving leaf, alongside every warning `lenient` already
    /// collected. A table walked into recurses, naming a leftover key inside
    /// it (`tuning.mni_soc`); an untouched table is named once at its own path, and a
    /// fully-drained known table warns about nothing.
    fn finish(self) -> Vec<String> {
        fn walk(
            table: toml::Table,
            known_tables: &HashSet<String>,
            prefix: &str,
            warnings: &mut Vec<String>,
        ) {
            for (key, value) in table {
                let path = if prefix.is_empty() {
                    key
                } else {
                    format!("{prefix}.{key}")
                };
                match value {
                    toml::Value::Table(t) if known_tables.contains(&path) => {
                        walk(t, known_tables, &path, warnings);
                    }
                    _ => warnings.push(format!("unknown key `{path}`; ignored")),
                }
            }
        }

        let Taker {
            root,
            known_tables,
            mut warnings,
        } = self;
        walk(root, &known_tables, "", &mut warnings);
        warnings
    }
}

/// A single battery, as `[[device]]` describes it. `registry::from_config` is
/// the only place this becomes a [`crate::registry::Battery`]. Two kinds exist,
/// and the array is checked for exactly one entry (see [`take_device`]).
#[derive(Debug, Clone, PartialEq)]
pub enum DeviceConfig {
    Zendure {
        ip: String,
        sn: String,
        poll_interval: Duration,
    },
    /// `simulation::VirtualBattery`'s constructor, minus the rated
    /// [`crate::device::BatterySpec`]: a virtual device always simulates the
    /// one model this crate knows (`AC2400_PLUS`), the same way `Zendure`
    /// never lets a config file pick a rating — hardware rating is a fact, not a
    /// setting.
    Virtual {
        id: String,
        packs: Vec<WattHours>,
        soc: Soc,
        charge_efficiency: Efficiency,
        discharge_efficiency: Efficiency,
    },
}

impl DeviceConfig {
    /// What `run.rs`'s startup log line calls this box: the identity it will
    /// carry in the world, before `registry::from_config` has built anything.
    pub fn identity(&self) -> &str {
        match self {
            DeviceConfig::Zendure { sn, .. } => sn,
            DeviceConfig::Virtual { id, .. } => id,
        }
    }

    /// How often `run.rs`'s poll timer fires. `Virtual` has no
    /// `poll_interval_secs` to read — no network round trip to pace, only
    /// an in-process model — so this returns a fixed cadence close to a real Zendure's
    /// instead of a config key nothing needs yet.
    pub fn poll_interval(&self) -> Duration {
        match self {
            DeviceConfig::Zendure { poll_interval, .. } => *poll_interval,
            DeviceConfig::Virtual { .. } => Duration::from_secs(10),
        }
    }
}

/// `[mqtt]`, present or not. Presence, not a flag inside it, is what selects
/// the broker backend — see `Taker::has_table`'s doc comment — so this is an
/// `Option<MqttConfig>` on `Config` rather than a `connected: bool` next to a
/// host string that means nothing when it is `false`.
#[derive(Clone, PartialEq)]
pub struct MqttConfig {
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    pub client_id: String,
}

/// Hand-written for the reason `Config`'s own `Debug` is: a derived one would
/// print `password` as `Some("hunter2")`, and this struct rides inside
/// `Config`'s `Debug` output through its `mqtt` field.
impl std::fmt::Debug for MqttConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MqttConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("client_id", &self.client_id)
            .finish()
    }
}

/// `[shelly]`, present or not. Required when [`MeterConfig::Shelly`] is in
/// effect (the default), optional when the meter is synthetic — a synthetic
/// house has no Shelly to configure.
#[derive(Debug, Clone, PartialEq)]
pub struct ShellyConfig {
    pub topic: String,
    pub solar_phase: SolarPhase,
}

/// `[web]`, present or not. Presence selects whether the live dashboard
/// server runs at all, the same rule [`MqttConfig`] follows — a deployment
/// that never adds `[web]` gets no HTTP listener.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WebConfig {
    pub bind_address: std::net::IpAddr,
    pub port: u16,
}

/// Which meter feeds the engine its grid readings. Defaults to `Shelly`,
/// selected by leaving `[meter]` out. `Synthetic` lets a laptop with no
/// broker feed the engine — see `source::synthetic` for why the battery's own flow must
/// feed back.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MeterConfig {
    Shelly,
    Synthetic { base_load: Watts, solar_peak: Watts },
}

/// Pulls the required `[[device]]` array out of the root table before
/// `Taker` ever sees it: an array of tables doesn't fit the dotted-path
/// model, and every field here is fatal anyway — an unreachable device is
/// "cannot talk at all", not "decides slightly differently". Exactly one entry, `kind =
/// "zendure"` or `"virtual"`.
fn take_device(root: &mut toml::Table) -> Result<DeviceConfig, String> {
    let value = root
        .remove("device")
        .ok_or_else(|| "device is required (at least one [[device]] entry)".to_string())?;

    let entries = match value {
        toml::Value::Array(entries) => entries,
        other => {
            return Err(format!(
                "device must be an array of tables ([[device]]), found {}",
                other.type_str()
            ));
        }
    };

    if entries.len() != 1 {
        return Err(format!(
            "expected exactly one [[device]] entry, found {}",
            entries.len()
        ));
    }

    let mut table = match entries.into_iter().next().expect("checked len == 1") {
        toml::Value::Table(t) => t,
        other => {
            return Err(format!(
                "[[device]] entries must be tables, found {}",
                other.type_str()
            ));
        }
    };

    let kind = take_device_field(&mut table, "kind")?;
    let device = match kind.as_str() {
        "zendure" => {
            let ip = take_device_field(&mut table, "ip")?;
            let sn = take_device_field(&mut table, "sn")?;
            let poll_interval_secs = take_device_secs(&mut table, "poll_interval_secs")?;
            DeviceConfig::Zendure {
                ip,
                sn,
                poll_interval: Duration::from_secs(poll_interval_secs),
            }
        }
        "virtual" => {
            let id = take_device_field(&mut table, "id")?;
            let packs = take_device_packs(&mut table, "packs")?;
            let soc = take_device_soc(&mut table, "soc")?;
            let charge_efficiency = take_device_f64(&mut table, "charge_efficiency")?;
            let discharge_efficiency = take_device_f64(&mut table, "discharge_efficiency")?;
            DeviceConfig::Virtual {
                id,
                packs,
                soc,
                charge_efficiency: Efficiency::new(charge_efficiency),
                discharge_efficiency: Efficiency::new(discharge_efficiency),
            }
        }
        other => {
            return Err(format!(
                "device.kind must be \"zendure\" or \"virtual\", found {other:?}"
            ));
        }
    };

    if let Some(key) = table.keys().next() {
        return Err(format!("device.{key} is not a recognised field"));
    }

    Ok(device)
}

fn take_device_field(table: &mut toml::Table, key: &str) -> Result<String, String> {
    match table.remove(key) {
        None => Err(format!("device.{key} is required")),
        Some(toml::Value::String(s)) => Ok(s),
        Some(v) => Err(format!(
            "device.{key} must be a string, found {}",
            v.type_str()
        )),
    }
}

fn take_device_secs(table: &mut toml::Table, key: &str) -> Result<u64, String> {
    match table.remove(key) {
        None => Err(format!("device.{key} is required")),
        Some(toml::Value::Integer(n)) => u64::try_from(n)
            .map_err(|_| format!("device.{key} must be a non-negative integer, found {n}")),
        Some(v) => Err(format!(
            "device.{key} must be an integer, found {}",
            v.type_str()
        )),
    }
}

/// A whole, non-negative percentage: `device.soc` on a `[[device]] kind =
/// "virtual"` entry. `Soc::new` does the clamping every other reader of a
/// percentage in this file already goes through.
fn take_device_soc(table: &mut toml::Table, key: &str) -> Result<Soc, String> {
    match table.remove(key) {
        None => Err(format!("device.{key} is required")),
        Some(toml::Value::Integer(n)) => u32::try_from(n)
            .map(Soc::new)
            .map_err(|_| format!("device.{key} must be a non-negative integer, found {n}")),
        Some(v) => Err(format!(
            "device.{key} must be an integer, found {}",
            v.type_str()
        )),
    }
}

/// A bare `f64`, for `device.charge_efficiency` / `device.discharge_efficiency`
/// — TOML distinguishes integers from floats, and a person writing `95` rather
/// than `95.0` must not be met with a fatal type error over a distinction they
/// had no reason to think mattered.
fn take_device_f64(table: &mut toml::Table, key: &str) -> Result<f64, String> {
    match table.remove(key) {
        None => Err(format!("device.{key} is required")),
        Some(toml::Value::Float(f)) => Ok(f),
        Some(toml::Value::Integer(n)) => Ok(n as f64),
        Some(v) => Err(format!(
            "device.{key} must be a number, found {}",
            v.type_str()
        )),
    }
}

/// `device.packs` on a virtual device: the capacity of each connected pack,
/// in watt-hours. An array rather than a single total, mirroring
/// `VirtualBattery`'s own field — see its doc comment for why a heterogeneous
/// fleet is the reason this is a list at all.
fn take_device_packs(table: &mut toml::Table, key: &str) -> Result<Vec<WattHours>, String> {
    match table.remove(key) {
        None => Err(format!("device.{key} is required")),
        Some(toml::Value::Array(items)) => items
            .into_iter()
            .map(|v| match v {
                toml::Value::Integer(n) => Ok(WattHours(n as f64)),
                toml::Value::Float(f) => Ok(WattHours(f)),
                other => Err(format!(
                    "device.{key} entries must be numbers, found {}",
                    other.type_str()
                )),
            })
            .collect(),
        Some(v) => Err(format!(
            "device.{key} must be an array, found {}",
            v.type_str()
        )),
    }
}

#[cfg_attr(test, derive(PartialEq))]
pub struct Config {
    /// `None` when `[mqtt]` is absent from the file — brokerless, per
    /// `run.rs`'s choice of a [`crate::publish::NullPublisher`] in that case.
    /// It is the table's *presence* that selects the backend, not a flag
    /// inside it: see `Taker::has_table`'s doc comment.
    pub mqtt: Option<MqttConfig>,
    pub device: DeviceConfig,
    /// `None` when `[shelly]` is absent — only possible when
    /// [`MeterConfig::Synthetic`] is in effect; `from_toml_str` refuses a
    /// [`MeterConfig::Shelly`] with no `[shelly]` to configure it.
    pub shelly: Option<ShellyConfig>,
    /// Which meter feeds the engine. Defaults to `Shelly` when `[meter]` is
    /// absent, matching every config written before this field existed.
    pub meter: MeterConfig,
    /// `None` when `[web]` is absent — no live dashboard server runs.
    pub web: Option<WebConfig>,
    pub ha_publish_prefix: String,
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
    /// Solar inverter export on the Shelly's solar phase at or above which discharge is
    /// skipped, so large loads (e.g. EV charging) pull from grid+solar instead
    /// of draining the home battery. 0 disables the guard (default).
    pub solar_discharge_block_threshold: SolarPower,
    /// Minimum idle duration before discharge is allowed (prevents charge→discharge
    /// oscillation)
    pub min_idle_before_discharge: Duration,
    /// IANA timezone (e.g. Europe/Amsterdam)
    pub timezone: Tz,
    /// Time without MQTT updates before forcing idle (safety failsafe)
    pub mqtt_timeout: Duration,
    /// SQLite journal of events and decisions.
    pub journal_path: PathBuf,
    /// How long journal rows are kept. The only thing bounding the file.
    pub journal_retention_days: RetentionDays,
    /// Where the rolling RTE window is persisted.
    pub rte_state_path: PathBuf,
    /// `tracing_subscriber::EnvFilter` string, e.g. `"zendure=info"`.
    ///
    /// `RUST_LOG` overrides this at the point the subscriber is built — an
    /// operator's `systemctl edit` override, not configuration this file
    /// owns — so `from_toml_str` never reads it.
    pub log_filter: String,
}

/// Hand-written rather than derived: a derived `Debug` would print
/// `mqtt_password` as `Some("hunter2")`, and `--check` exists
/// precisely to print a `Config` on the terminal. A broker credential has no
/// business being one log line away from a support paste.
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("mqtt", &self.mqtt)
            .field("device", &self.device)
            .field("shelly", &self.shelly)
            .field("meter", &self.meter)
            .field("web", &self.web)
            .field("ha_publish_prefix", &self.ha_publish_prefix)
            .field("charge_margin", &self.charge_margin)
            .field("discharge_margin", &self.discharge_margin)
            .field("charge_start_threshold", &self.charge_start_threshold)
            .field("discharge_start_threshold", &self.discharge_start_threshold)
            .field("min_mode_duration", &self.min_mode_duration)
            .field("min_decision_interval", &self.min_decision_interval)
            .field("idle_timeout", &self.idle_timeout)
            .field("cycle_warn_threshold", &self.cycle_warn_threshold)
            .field("min_soc", &self.min_soc)
            .field("max_soc", &self.max_soc)
            .field("balance_weekday", &self.balance_weekday)
            .field(
                "solar_discharge_block_threshold",
                &self.solar_discharge_block_threshold,
            )
            .field("min_idle_before_discharge", &self.min_idle_before_discharge)
            .field("timezone", &self.timezone)
            .field("mqtt_timeout", &self.mqtt_timeout)
            .field("journal_path", &self.journal_path)
            .field("journal_retention_days", &self.journal_retention_days)
            .field("rte_state_path", &self.rte_state_path)
            .field("log_filter", &self.log_filter)
            .finish()
    }
}

/// The decision-relevant half of [`Config`], recorded once per session so a
/// replay knows the tuning that produced it. Connection settings are
/// excluded to keep a fixture hermetic; durations are whole seconds rather
/// than `Duration`'s own `{"secs":_,"nanos":_}` serde. These field names match
/// `config.example.toml`'s `[tuning]` table verbatim, checked equal by a test.
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
    /// The permissive tuning tests decide under: no decision-interval
    /// cooldown, generous margins, no balance day. Shared by
    /// `Controller::test_default` and any fixture needing `session.config`,
    /// so a replay runs the same knobs the recording did instead of two hand-written
    /// copies drifting apart and making `--verify` meaningless.
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
    /// Destructures `self` exhaustively (no `..`) so a new `Config` field is
    /// a compile error here, not a knob silently missing from `config_json`.
    /// `timezone` is excluded because every journaled `Event` already carries a
    /// resolved `Clock`; solar phase is excluded via `shelly`.
    pub fn session(&self) -> SessionConfig {
        let Config {
            // Connection settings: how to reach things, not what to decide.
            mqtt: _,
            device: _,
            shelly: _,
            meter: _,
            web: _,
            ha_publish_prefix: _,
            journal_path: _,
            journal_retention_days: _,
            rte_state_path: _,
            // A tracing filter, not a decision input.
            log_filter: _,
            // Resolved into every event before it is journaled.
            timezone: _,
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
    /// Parses a TOML configuration file: a missing/wrong-typed connection
    /// setting or anything about a device is fatal; a wrong-typed tuning
    /// knob, journal/logging setting, or unknown key warns and falls back
    /// to its default. Returns warnings instead of logging them, since this runs before
    /// the tracing subscriber exists; builds `Config` from an exhaustive struct literal
    /// (no `..Default::default()`), so a new field is a compile error.
    pub fn from_toml_str(text: &str) -> Result<(Config, Vec<String>), String> {
        let mut root = text.parse::<toml::Table>().map_err(|e| e.to_string())?;

        let device = take_device(&mut root)?;

        let mut taker = Taker::new(root);

        // Presence, not a field inside it, is what selects the broker
        // backend — see `Taker::has_table`'s doc comment. `run.rs` reads
        // `mqtt.is_none()` to decide whether to build a real `AsyncClient` at
        // all.
        let mqtt = if taker.has_table("mqtt")? {
            Some(MqttConfig {
                host: taker.required::<String>("mqtt.host")?,
                port: taker.optional::<u16>("mqtt.port")?.unwrap_or(1883),
                username: taker.optional::<String>("mqtt.username")?,
                password: taker.optional::<String>("mqtt.password")?,
                client_id: taker
                    .optional::<String>("mqtt.client_id")?
                    .unwrap_or_else(|| "zendure-controller".to_string()),
            })
        } else {
            None
        };

        let shelly = if taker.has_table("shelly")? {
            Some(ShellyConfig {
                topic: taker.required::<String>("shelly.topic")?,
                solar_phase: taker.lenient::<SolarPhase>("shelly.solar_phase", SolarPhase::A)?,
            })
        } else {
            None
        };

        // `[meter]` picks which source feeds the engine; absent means
        // `Shelly`. `[mqtt]`/`[shelly]` default to *absent* on absence,
        // while this defaults to a *variant*, since "no meter at all" is
        // never a coherent controller.
        let meter = if taker.has_table("meter")? {
            let kind = taker.required::<String>("meter.kind")?;
            match kind.as_str() {
                "shelly" => MeterConfig::Shelly,
                "synthetic" => MeterConfig::Synthetic {
                    base_load: taker.required::<Watts>("meter.base_load")?,
                    solar_peak: taker.required::<Watts>("meter.solar_peak")?,
                },
                other => {
                    return Err(format!(
                        "meter.kind must be \"shelly\" or \"synthetic\", found {other:?}"
                    ));
                }
            }
        } else {
            MeterConfig::Shelly
        };

        // Two coherence checks a per-field reader cannot express, because
        // each is a relationship *between* tables rather than a property of
        // one. Both fatal: getting either wrong means the controller cannot
        // talk to the thing it was just told to use.
        if matches!(meter, MeterConfig::Shelly) && mqtt.is_none() {
            return Err(
                "meter is Shelly (the default) but [mqtt] is absent — the Shelly reading \
                 arrives over MQTT; add [mqtt], or set [meter] kind = \"synthetic\" to run \
                 without a broker"
                    .to_string(),
            );
        }
        if matches!(meter, MeterConfig::Shelly) && shelly.is_none() {
            return Err(
                "meter is Shelly (the default) but [shelly] is absent — shelly.topic is \
                 required; add [shelly], or set [meter] kind = \"synthetic\""
                    .to_string(),
            );
        }
        if matches!(meter, MeterConfig::Synthetic { .. })
            && !matches!(device, DeviceConfig::Virtual { .. })
        {
            return Err(
                "meter kind = \"synthetic\" requires [[device]] kind = \"virtual\" — the \
                 synthetic meter feeds the battery's own simulated flow back into its reading, \
                 which only a virtual battery can supply"
                    .to_string(),
            );
        }

        // Presence, not a field inside it, selects whether the dashboard
        // server runs at all — the same rule `[mqtt]` follows.
        // `bind_address`/`port` are connection settings, read via `optional`:
        // present-and-wrong-type is fatal, absent takes the default.
        let web = if taker.has_table("web")? {
            Some(WebConfig {
                bind_address: taker
                    .optional::<std::net::IpAddr>("web.bind_address")?
                    .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1))),
                port: taker.optional::<u16>("web.port")?.unwrap_or(8080),
            })
        } else {
            None
        };

        let ha_publish_prefix =
            taker.lenient::<String>("homeassistant.publish_prefix", "zendure".to_string())?;

        let timezone = taker
            .lenient::<TimezoneName>("clock.timezone", TimezoneName(Tz::UTC))?
            .0;

        let journal_path =
            taker.lenient::<PathBuf>("journal.path", PathBuf::from(DEFAULT_JOURNAL_PATH))?;
        // MUST go through `lenient`: `RetentionDays::new` rejects 0 and
        // negatives via `Deserialize`. Routed through `required` instead,
        // `retention_days = 0` would be a fatal startup error, and systemd's
        // `Restart=on-failure` would restart-loop the daemon while the battery held its
        // last command.
        let journal_retention_days = taker.lenient::<RetentionDays>(
            "journal.retention_days",
            RetentionDays::new(90).expect("90 is a valid retention"),
        )?;

        let rte_state_path =
            taker.lenient::<PathBuf>("rte.state_path", PathBuf::from(DEFAULT_RTE_STATE_PATH))?;

        let log_filter = taker.lenient::<String>("logging.filter", "zendure=info".to_string())?;

        // `[tuning]` is `SessionConfig`, verbatim — see that struct's doc
        // comment and `the_tuning_table_has_exactly_the_session_config_keys`.
        let charge_margin =
            taker.lenient::<PowerMargin>("tuning.charge_margin", PowerMargin::new(50))?;
        let discharge_margin =
            taker.lenient::<PowerMargin>("tuning.discharge_margin", PowerMargin::new(5))?;
        let charge_start_threshold =
            taker.lenient::<GridPower>("tuning.charge_start_threshold", GridPower(-100.0))?;
        let discharge_start_threshold =
            taker.lenient::<GridPower>("tuning.discharge_start_threshold", GridPower(0.0))?;
        let min_mode_duration =
            Duration::from_secs(taker.lenient::<u64>("tuning.min_mode_duration_secs", 10)?);
        let min_decision_interval =
            Duration::from_secs(taker.lenient::<u64>("tuning.min_decision_interval_secs", 5)?);
        let idle_timeout =
            Duration::from_secs(taker.lenient::<u64>("tuning.idle_timeout_secs", 300)?);
        let min_idle_before_discharge = Duration::from_secs(
            taker.lenient::<u64>("tuning.min_idle_before_discharge_secs", 300)?,
        );
        let cycle_warn_threshold = taker.lenient::<u32>("tuning.cycle_warn_threshold", 200)?;
        let min_soc = taker.lenient::<Soc>("tuning.min_soc", Soc::new(10))?;
        let max_soc = taker.lenient::<Soc>("tuning.max_soc", Soc::new(100))?;
        let balance_weekday = taker
            .lenient::<BalanceDay>("tuning.balance_weekday", BalanceDay(Some(Weekday::Mon)))?
            .0;
        let solar_discharge_block_threshold = taker
            .lenient::<SolarPower>("tuning.solar_discharge_block_threshold", SolarPower::ZERO)?;
        let mqtt_timeout =
            Duration::from_secs(taker.lenient::<u64>("tuning.mqtt_timeout_secs", 60)?);

        let warnings = taker.finish();

        Ok((
            Config {
                mqtt,
                device,
                shelly,
                meter,
                web,
                ha_publish_prefix,
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
                timezone,
                mqtt_timeout,
                journal_path,
                journal_retention_days,
                rte_state_path,
                log_filter,
            },
            warnings,
        ))
    }

    /// [`Config::from_toml_str`], reading the file at `path` first.
    pub fn from_toml(path: &Path) -> Result<(Config, Vec<String>), String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::from_toml_str(&text)
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
