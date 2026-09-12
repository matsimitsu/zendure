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

/// Where the rolling round-trip-efficiency window is persisted.
///
/// Under `/var/lib` rather than `/tmp` because the window is 24 hours long and
/// `/tmp` is cleared on boot, which silently rebuilt it from scratch on every
/// restart.
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
/// Hand-written rather than routed through `chrono-tz`'s own `serde` feature:
/// that feature's error is just "not a valid timezone", and drops the "e.g.
/// Europe/Amsterdam" half that tells an operator what to write instead.
/// `TIMEZONE` has given that hint since the environment-variable days; a
/// configuration file deserves the same one, not a stricter, less helpful
/// gate that happens to live in a different crate.
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
/// trimmed) disable the periodic cell-balancing full charge; anything else is
/// read as a weekday through the same [`parse_weekday`] `BALANCE_WEEKDAY` has
/// always used, so a configuration file and an environment variable accept
/// identical spellings.
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

/// Consumes a parsed TOML table one key at a time.
///
/// Every accessor removes the key it reads, so whatever is left once every
/// key this schema knows about has been asked for is — by construction, not
/// by a second pass that has to remember to check — a key the schema does
/// not recognise. [`Taker::finish`] turns what remains into warnings.
///
/// All three leaf accessors return `Result`, including the lenient one: a
/// *leaf* being the wrong type can be forgiven with a default, but a
/// *container* being the wrong type cannot, because there is nothing
/// underneath it to fall back to. `tuning = "x"` cannot warn-and-default
/// fourteen times over; it has to fail once, loudly, before any of those
/// fourteen defaults are chosen.
struct Taker {
    root: toml::Table,
    /// Dotted paths of every table this reader has explicitly walked
    /// through — `"mqtt"`, `"tuning"`, and so on. `finish` uses this to tell
    /// a specific unknown key inside a known section (`tuning.mni_soc`,
    /// named on its own) apart from a whole section this schema never
    /// mentions at all (`[extra_stuff]`, named once, however many keys it
    /// holds). The two look identical once the known keys have been
    /// drained — both are "a table with something left in it" — so nothing
    /// short of remembering which tables were ever asked about can
    /// distinguish them.
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

    /// Removes and returns the value at `path`, or `None` if any segment of
    /// it is simply absent — an ordinary, unremarkable case for every
    /// caller. Descending through a value that *is* present but is not a
    /// table is the one error every caller shares, so it is handled once
    /// here rather than three times over in `required`, `optional` and
    /// `lenient`.
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

    /// A connection setting with a default the caller applies itself (e.g.
    /// `.unwrap_or(1883)`). Absence is fine — that is what the default is
    /// for — but a value that is *present* and the wrong type is still
    /// fatal: `port = "1883"` is a typo worth stopping for, not a tuning
    /// knob worth guessing past.
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
    /// caller finds out why via the returned warnings rather than a startup
    /// failure. Getting one of these wrong means the controller decides
    /// slightly differently; it must never be the reason the controller
    /// stops running.
    ///
    /// **Warning and falling back rather than failing is deliberate, not
    /// laziness.** `Config` is read at process startup, and systemd restarts
    /// this service on failure — so parsing a knob strictly would put a typo
    /// on the path that exits `main`, and the daemon would restart-loop while
    /// the battery held whatever command it last received. Loud and running
    /// beats silent and stopped. `journal.retention_days` is the sharpest
    /// example: a bad value there is a *logging* concern, and `journal.rs`
    /// holds the line that a logging failure must never become a control
    /// failure — an unusable journal path already degrades to "no journal"
    /// for exactly this reason.
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

    /// Turns whatever is left after every known key has been taken into one
    /// warning per surviving leaf, and returns them alongside every warning
    /// `lenient` already collected.
    ///
    /// A table this reader walked through (`known_tables`) recurses, so a
    /// leftover key inside it is named on its own (`tuning.mni_soc`). A
    /// table it never touched at all does not recurse — it is named once,
    /// at its own path, with nothing underneath it examined. A fully-drained
    /// known table recurses into nothing and produces no warning at all,
    /// which is what makes a config with every key spelled correctly
    /// perfectly silent.
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

/// Pulls the required `[[device]]` array out of the root table before
/// `Taker` ever sees it.
///
/// An array of tables does not fit the dotted-path model the rest of this
/// file uses, and there would be no lenient path to share with it anyway:
/// every field here is fatal, per the failure policy `config.example.toml`
/// states — a device that cannot be reached is the "cannot talk at all"
/// case, not the "decides slightly differently" one.
///
/// Only `kind = "zendure"` is accepted, and only one entry. A device *list*
/// on `Config` is a later commit's job; this one only has one battery to
/// hand back.
fn take_device(root: &mut toml::Table) -> Result<(String, String, Duration), String> {
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
    if kind != "zendure" {
        return Err(format!(
            "device.kind must be \"zendure\" (the only kind this build supports), found {kind:?}"
        ));
    }
    let ip = take_device_field(&mut table, "ip")?;
    let sn = take_device_field(&mut table, "sn")?;
    let poll_interval_secs = take_device_secs(&mut table, "poll_interval_secs")?;

    if let Some(key) = table.keys().next() {
        return Err(format!("device.{key} is not a recognised field"));
    }

    Ok((ip, sn, Duration::from_secs(poll_interval_secs)))
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

#[cfg_attr(test, derive(PartialEq))]
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
/// `mqtt_password` as `Some("hunter2")`, and `--check` (step 9) exists
/// precisely to print a `Config` on the terminal. A broker credential has no
/// business being one log line away from a support paste.
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("mqtt_host", &self.mqtt_host)
            .field("mqtt_port", &self.mqtt_port)
            .field("mqtt_username", &self.mqtt_username)
            .field(
                "mqtt_password",
                &self.mqtt_password.as_ref().map(|_| "<redacted>"),
            )
            .field("mqtt_client_id", &self.mqtt_client_id)
            .field("zendure_ip", &self.zendure_ip)
            .field("zendure_sn", &self.zendure_sn)
            .field("shelly_topic", &self.shelly_topic)
            .field("ha_publish_prefix", &self.ha_publish_prefix)
            .field("zendure_poll_interval", &self.zendure_poll_interval)
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
            .field("solar_phase", &self.solar_phase)
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
/// replay knows what tuning produced a row.
///
/// Built by hand rather than derived on `Config`, and that is the point:
/// connection settings are not decision inputs, so a fixture carrying them
/// would be neither hermetic nor safe to pass around. Durations are whole
/// seconds because `Duration`'s own serde emits `{"secs":_,"nanos":_}`, which
/// reads badly next to every other bare number in the journal.
///
/// These fourteen field names are also, verbatim, `config.example.toml`'s
/// `[tuning]` table — a test asserts the two key sets are equal, so a name
/// cannot drift between "what a replay can `--set`" and "what a config file
/// can spell".
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
            rte_state_path: _,
            // A tracing filter, not a decision input.
            log_filter: _,
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
    /// Parses a TOML configuration file, per the failure policy
    /// `config.example.toml`'s header documents: not valid TOML, a table
    /// that is not a table, a missing or wrong-typed connection setting, or
    /// anything about a device is fatal; a wrong-typed tuning knob, a bad
    /// journal or logging setting, or an unknown key warns and falls back to
    /// its default.
    ///
    /// Returns the warnings rather than logging them: this runs before the
    /// tracing subscriber exists (`main` needs `Config.log_filter` to build
    /// it), and a test asserting on the exact wording would otherwise need
    /// one. `replay.rs`'s `from_recording` returns its warnings the same way,
    /// for the same reason.
    ///
    /// Builds `Config` from an exhaustive struct literal — no
    /// `..Default::default()` — so a field added to `Config` without a
    /// matching line here is a compile error, the same guarantee
    /// `Config::session()` gives the other direction.
    pub fn from_toml_str(text: &str) -> Result<(Config, Vec<String>), String> {
        let mut root = text.parse::<toml::Table>().map_err(|e| e.to_string())?;

        let (zendure_ip, zendure_sn, zendure_poll_interval) = take_device(&mut root)?;

        let mut taker = Taker::new(root);

        let mqtt_host = taker.required::<String>("mqtt.host")?;
        let mqtt_port = taker.optional::<u16>("mqtt.port")?.unwrap_or(1883);
        let mqtt_username = taker.optional::<String>("mqtt.username")?;
        let mqtt_password = taker.optional::<String>("mqtt.password")?;
        let mqtt_client_id = taker
            .optional::<String>("mqtt.client_id")?
            .unwrap_or_else(|| "zendure-controller".to_string());

        let shelly_topic = taker.required::<String>("shelly.topic")?;
        let solar_phase = taker.lenient::<SolarPhase>("shelly.solar_phase", SolarPhase::A)?;

        let ha_publish_prefix =
            taker.lenient::<String>("homeassistant.publish_prefix", "zendure".to_string())?;

        let timezone = taker
            .lenient::<TimezoneName>("clock.timezone", TimezoneName(Tz::UTC))?
            .0;

        let journal_path =
            taker.lenient::<PathBuf>("journal.path", PathBuf::from(DEFAULT_JOURNAL_PATH))?;
        // MUST go through `lenient`. `RetentionDays::new` rejects 0 and
        // negatives, and its `Deserialize` surfaces that as a deserialize
        // error — see the type's own doc comment. Routed through `required`
        // instead, `retention_days = 0` would be a fatal startup error, and
        // systemd's `Restart=on-failure` would restart-loop the daemon while
        // the battery held whatever command it last received.
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
                mqtt_host,
                mqtt_port,
                mqtt_username,
                mqtt_password,
                mqtt_client_id,
                zendure_ip,
                zendure_sn,
                shelly_topic,
                ha_publish_prefix,
                zendure_poll_interval,
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
                solar_phase,
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
