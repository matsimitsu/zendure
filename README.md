# Zendure AC 2400+ Controller

Smart controller for the Zendure AC 2400+ home battery. Reads net grid power from a Shelly Pro 3EM via MQTT, decides when to charge or discharge the battery, and publishes decisions to MQTT for HomeAssistant integration.

## How it works

1. **Subscribes to MQTT** for Shelly Pro 3EM power readings (signed per-phase and total active power, updated every second)
2. **Polls the Zendure device** via its local REST API for battery state (SOC, power, temperatures, pack data)
3. **Decides what the battery should do**:
   - **Charge** when there's excess solar being exported to the grid (up to 2400W)
   - **Discharge** to cover grid demand (up to inverter limit), after a configurable idle period to prevent charge/discharge oscillation
   - **Idle** otherwise
   - **Standby** after prolonged idle or when daily cycle limit is reached
5. **Safety guards**:
   - **SOC limits** — stops charging at max SOC (default 100%) and discharging at min SOC (default 10%); on the configured `BALANCE_WEEKDAY` (default Monday), max SOC is raised to 100% for a periodic cell-balancing full charge
   - **Cooldown** — prevents rapid charge/discharge toggling
   - **Ramp** — starts at 75% power on mode changes to avoid overshooting
   - **SOC calibration** — idles when the battery reports SOC calibration in progress
   - **Cycle limit** — forces standby when daily mode transitions exceed a threshold
   - **Device fault** — idles when the device reports an error (`isError`); `faultLevel` is ignored since it also goes non-zero for benign conditions like WiFi issues or firmware update checks
   - **Power caps** — the device stores its charge (`chargeMaxLimit`, 2400W) and discharge (`inverseMaxPower`, 800W) limits as setpoints it can reset to 0, which stalls all power flow. The controller writes them **once at startup** and never mid-run. If the device zeroes a cap while running, the controller honors it (that direction stops) rather than overwriting a value the device changed for reasons we can't see — recovery is a deliberate process restart.
6. **Tracks round-trip efficiency** (RTE) — measures charge vs discharge energy, persisted to disk
7. **Publishes to MQTT** — HomeAssistant auto-discovers all sensors

## Running it

With no arguments, `zendure` runs the controller — which is what the systemd
unit does and what it has always meant. `--config <path>` points it at a TOML
configuration file (default `/etc/zendure/config.toml`); `--check` parses that
file, prints the effective configuration, and exits instead of starting
anything:

```
zendure [--config <path>] [--check]
```

Two offline subcommands read the journal instead, and read **no configuration
at all** — not even `--config`'s default path — so they work against a copied
database on a machine with no broker and no battery:

```
zendure export --from <when> --to <when> [--db <path>] [--out <file>]
zendure replay <fixture> [--verify] [--set <knob>=<value>]...
```

`<when>` is unix milliseconds or RFC 3339. `--db` defaults to
`/var/lib/zendure/journal.db` and does **not** read `[journal] path` from a
config file — these subcommands read no configuration at all, so a deployment
that moves the journal has to say `--db` too. See [Replay](#replay) below.

## Configuration

Configuration is a TOML file — `/etc/zendure/config.toml` by default, or
whatever `--config` points at. [`config.example.toml`](config.example.toml) is
the authoritative reference: every key, its default, and the failure policy
(what's fatal versus what warns and falls back) live there as comments,
production runs it verbatim, and a test in `config_tests.rs` pins the file to
that fact. The shape, briefly:

```toml
[mqtt]
host = "127.0.0.1"          # required
port = 1883
client_id = "odroid"
# username = "…"
# password = "…"

[[device]]
kind = "zendure"
ip = "192.168.1.253"        # required
sn = "HEC4NENCN490270"      # required
poll_interval_secs = 10

[shelly]
topic = "shellypro3em-XXXX/status/em:0"   # required
solar_phase = "A"           # A, B or C — which phase the solar inverter feeds

[homeassistant]
publish_prefix = "zendure"

[clock]
timezone = "Europe/Amsterdam"   # IANA name

[journal]
path = "/var/lib/zendure/journal.db"
retention_days = 30             # 1–3650

[rte]
state_path = "/var/lib/zendure/rte_state.json"

[logging]
filter = "zendure=info"

[tuning]
charge_margin = 50
discharge_margin = 5
charge_start_threshold = -100.0
discharge_start_threshold = 0.0
min_mode_duration_secs = 10
min_decision_interval_secs = 5
idle_timeout_secs = 300
min_idle_before_discharge_secs = 300
cycle_warn_threshold = 200
min_soc = 10
max_soc = 100
balance_weekday = "Mon"     # or "none"/"off" to disable the full-charge day
solar_discharge_block_threshold = 0.0
mqtt_timeout_secs = 60
```

**`mqtt.*`, `[[device]]` and `shelly.topic` are fatal if missing or the wrong
type** — getting one of those wrong means the controller talks to the wrong
thing or cannot talk at all. **Everything in `[tuning]`, plus the journal,
rte and logging settings, warns and falls back to its default** — getting one
of those wrong means the controller decides slightly differently, and a typo
there must never be the reason systemd restart-loops a controller that is
holding a battery command. `RUST_LOG`, when set and non-empty, overrides
`[logging].filter` outright — that's a `tracing` convention applied at the
point the subscriber is built, not something the config file itself reads.

`zendure --check --config <path>` runs the same parse, but strictly: a parse
error is fatal exactly as it is for the daemon, and **any warning is promoted
to a failure too**. Ansible runs it as a `validate:` hook before a rendered
config is moved into place, so a typo fails the deploy instead of quietly
degrading a running controller.

### Migrating from environment variables

Configuration was replaced by a TOML file in version 0.3.0. If you had
environment variables set, this table maps each one to its new location:

| Old env var | New TOML location | Notes |
|-------------|-------------------|-------|
| `MQTT_HOST` | `[mqtt] host` | |
| `MQTT_PORT` | `[mqtt] port` | |
| `MQTT_USERNAME` | `[mqtt] username` | |
| `MQTT_PASSWORD` | `[mqtt] password` | |
| `MQTT_CLIENT_ID` | `[mqtt] client_id` | |
| `ZENDURE_IP` | `[[device]] ip` | Now in array; see below |
| `ZENDURE_SN` | `[[device]] sn` | Now in array; see below |
| `ZENDURE_POLL_INTERVAL` | `[[device]] poll_interval_secs` | Now in array; unit is already seconds |
| `SHELLY_TOPIC` | `[shelly] topic` | |
| `SOLAR_PHASE` | `[shelly] solar_phase` | |
| `HA_PUBLISH_PREFIX` | `[homeassistant] publish_prefix` | |
| `TIMEZONE` | `[clock] timezone` | |
| `CHARGE_START_THRESHOLD` | `[tuning] charge_start_threshold` | |
| `DISCHARGE_START_THRESHOLD` | `[tuning] discharge_start_threshold` | |
| `CHARGE_MARGIN` | `[tuning] charge_margin` | |
| `DISCHARGE_MARGIN` | `[tuning] discharge_margin` | |
| `MIN_SOC` | `[tuning] min_soc` | |
| `MAX_SOC` | `[tuning] max_soc` | |
| `BALANCE_WEEKDAY` | `[tuning] balance_weekday` | |
| `SOLAR_DISCHARGE_BLOCK_THRESHOLD` | `[tuning] solar_discharge_block_threshold` | |
| `MIN_IDLE_BEFORE_DISCHARGE` | `[tuning] min_idle_before_discharge_secs` | Unit is seconds; name clarified |
| `MIN_MODE_DURATION` | `[tuning] min_mode_duration_secs` | Unit is seconds; name clarified |
| `MIN_DECISION_INTERVAL` | `[tuning] min_decision_interval_secs` | Unit is seconds; name clarified |
| `IDLE_TIMEOUT_MINUTES` | `[tuning] idle_timeout_secs` | **Unit change: multiply by 60.** Was minutes, now seconds. `IDLE_TIMEOUT_MINUTES=10` becomes `idle_timeout_secs = 600` |
| `CYCLE_WARN_THRESHOLD` | `[tuning] cycle_warn_threshold` | |
| `MQTT_TIMEOUT` | `[tuning] mqtt_timeout_secs` | Unit is seconds; name clarified |
| `JOURNAL_PATH` | `[journal] path` | |
| `JOURNAL_RETENTION_DAYS` | `[journal] retention_days` | |
| `RTE_STATE_PATH` | `[rte] state_path` | |
| `RUST_LOG` | Environment variable | Still works; overrides `[logging] filter` when set |
| `JOURNAL_RAW_PATH` | — | Removed; this variable is gone and does nothing |

**Why `[[device]]` is an array:** The TOML format exists precisely so a device
list can be expressed — one entry today, more when a second battery or charger
joins the system. Configuration as environment variables could not represent
that, which is why this migration exists.

**Deploying a new config:** The config file and systemd unit must move
together. A systemd unit built for the old binary (`MQTT_HOST=…` in
`EnvironmentFile=`) will fail when the new binary runs with `--config`, because
the binary does not recognise those environment variables and the unit does not
pass `--config`. That mismatch causes the binary to exit with an unknown-argument
error, and systemd will restart-loop it. Make sure both arrive in the same
deployment.

`zendure --check --config <path>` validates a new config file before it is
deployed. It exits non-zero on anything that would be fatal at runtime, and
also on anything that would only warn — so a typo fails early, during testing,
not after the file is already in place. Run it as part of your deployment
validation:

```bash
zendure --check --config /etc/zendure/config.toml
```

## Journal

Every Shelly reading, every Zendure poll response, every event the engine folds
in, and every decision (with the world and controller state it was decided
from, and the outcome of the command sent to each device) is recorded to a
SQLite database at `[journal] path`.

```sql
sessions  (id, started_ms, version, config_json)
events    (id, session_id, seq, ts_ms, kind, payload_json)
decisions (id, session_id, seq, ts_ms, device, kind, payload_json, state_json,
           command, outcome, error, pre_battery_net_w)
```

`state_json` is the engine's whole snapshot — world, controller history and the
failsafe latch — so a single decision row is enough to seed a replay. Every row
carries the `session_id` of the process that wrote it, which is what joins it to
the `config_json` that governed it.

`seq` orders the whole file, and `export` is the reason it exists. The two row tables have independent `id`
sequences, so `seq` is the only way to ask "what happened after this row?"
across both — which is what seeding a replay from a decision and then feeding it
the events that followed requires. It is assigned by the single writer thread in
the order records were handed to it, continues across restarts rather than
restarting per session, and leaves gaps where a write failed or a prune deleted.

`events.kind` is one of `shelly` and `zendure_poll` (payloads captured verbatim,
*before* parsing) or `meter`, `device_update` and `mqtt_timeout` (the engine's
own events, replayable). The first *foldable* event of every session is a
`device_update` carrying the startup poll, so the world a replay rebuilds from
events is the same world the controller decided against from its first reading.
It is not necessarily the first row: the MQTT subscriber starts a moment earlier
and writes its raw `shelly` captures from its own task, so one of those often
lands first. Those are not fold inputs, which is why it does not matter.

`decisions.kind` is `decision` or `failsafe`, with one
row per device actuated — one today, more once a second battery or a charger
joins the world. A decision that commanded nothing still gets a row, with a null
`device`.

```
sqlite3 /var/lib/zendure/journal.db \
  "SELECT datetime(ts_ms/1000,'unixepoch'), device, command, outcome, pre_battery_net_w
     FROM decisions ORDER BY ts_ms DESC LIMIT 20;"
```

Raw payloads are stored exactly as received rather than re-serialized from
parsed types, so undocumented device fields are kept and a response we failed to
decode is still on record.

This replaced an NDJSON capture under `JOURNAL_RAW_PATH`. That variable is gone
and is now ignored if set; **the files it wrote are not cleaned up**, and nothing
prunes them any more, so an existing `/var/lib/zendure/raw` should be removed by
hand once you no longer want it. This exists so that when something looks wrong in a
graph, the inputs that produced it still exist — recorded data cannot be
backfilled.

The control loop never touches SQLite: records go to a bounded queue and a
writer thread owns the connection. If that queue ever fills, records are dropped
and counted rather than making the decision path wait. Journal failures never
affect control — including a bad `[journal] retention_days`, which warns and
keeps the default rather than stopping the controller.

Dropped records and failed writes are both counted and warned about (on the
first and then at powers of two, so a wedged writer cannot flood the log), and
summarised on shutdown. `RUST_LOG` overrides the default `zendure=info` filter
outright, so `RUST_LOG=zendure=debug` shows the per-record detail.

## Replay

A journal is only worth keeping if you can ask it questions. `export` turns a
stretch of it into a self-contained fixture, and `replay` runs that fixture's
events back through the decision engine.

```
zendure export --from 2026-09-12T19:50:00Z --to 2026-09-12T20:10:00Z \
  --db ./journal.db --out incident.json
zendure replay incident.json --verify
```

`--verify` compares the commands the engine produces now against the ones the
daemon actually issued, and exits non-zero if they differ, naming the first frame
that diverged. That is the question a refactor raises — *did this change
behaviour?* — and the answer is a diff of decisions, not of some aggregate
metric. Without `--verify` it just prints the run:

```
1789228429227ms: —
1789228429229ms: TESTSN set_idle
```

One line per event, with an em dash where the event decided nothing, and the
device named on every command so a fleet that was only partly commanded cannot
look like one that was commanded fully.

`--set <knob>=<value>` changes one tuning knob before replaying, for asking what
a different setting would have done — `zendure replay incident.json --set
min_soc=40`. The knobs are the ones in `sessions.config_json`; an unknown name is
an error rather than a silent no-op, and a value outside a knob's range is
clamped by the same constructor the daemon uses rather than waved through.
It cannot be combined with `--verify`, which would compare two differently tuned
controllers and report a divergence by construction.

**A fixture is hermetic.** It carries the tuning it was decided under, the engine
snapshot to resume from, and the events — and nothing about how to reach a broker
or a battery, because none of those are decision inputs. Both subcommands read
no configuration at all, so a fixture can be copied off the box and replayed
anywhere.

**The range is anchored to a decision, not to `--from`.** A replay has to resume
from a recorded snapshot, and those live on decision rows, so the fixture starts
at the last decision at or before `--from` and can therefore begin a little
earlier than asked.

**This is decision diff, not simulation.** The recorded meter readings were
caused in part by the controller's own output, so replaying *different* logic
against them answers a question about a world that logic would have changed.
Simulating forward needs a battery model and the old controller removed from the
recording; `pre_battery_net_w` is stored so that stays possible, but nothing here
does it.

## Running

```bash
cargo run -- --config ./config.example.toml
```

## HomeAssistant

The controller publishes MQTT discovery config automatically. These sensors appear in HA:

**Sensors:**
- `Zendure Controller Battery Decision Mode` — charge / discharge / idle / standby
- `Zendure Controller Battery Decision Power` — target power (W)
- `Zendure Controller Battery Decision Reason` — human-readable explanation
- `Zendure Controller Grid Power (at decision)` — net grid power used for the decision (W)
- `Zendure Controller Battery Round-Trip Efficiency` — charge/discharge RTE (%)
- `Zendure Controller Battery Usable Energy` — estimated usable energy remaining (kWh)
- `Zendure Controller Battery Total Capacity` — total pack capacity (kWh)
- `Zendure Controller Battery Enclosure Temperature` — enclosure temperature (°C)
- `Zendure Controller Battery Pack N Temperature` — per-pack temperature (°C, dynamic)
- `Zendure Controller Battery Daily Mode Transitions` — charge/discharge/idle transitions today
- `Zendure Controller Battery Daily Cooldown Suppressions` — suppressed rapid toggles today

**Binary sensors:**
- `Zendure Controller Battery SOC Calibrating` — ON when SOC calibration is in progress

## Releasing

```bash
./scripts/release.sh
```

This bumps the patch version in `Cargo.toml`, commits, tags with `vX.Y.Z`, and pushes. GitHub Actions then builds a static Linux binary and creates a release.
