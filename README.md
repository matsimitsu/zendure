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
unit does and what it has always meant. Two offline subcommands read the journal
instead, and read **no configuration at all**, so they work against a copied
database on a machine with no broker and no battery:

```
zendure export --from <when> --to <when> [--db <path>] [--out <file>]
zendure replay <fixture> [--verify] [--set <knob>=<value>]...
```

`<when>` is unix milliseconds or RFC 3339. See [Replay](#replay) below.

## Configuration

All configuration is via environment variables:

### MQTT

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `MQTT_HOST` | Yes | — | MQTT broker hostname |
| `MQTT_PORT` | No | `1883` | MQTT broker port |
| `MQTT_USERNAME` | No | — | MQTT username |
| `MQTT_PASSWORD` | No | — | MQTT password |
| `MQTT_CLIENT_ID` | No | `zendure-controller` | MQTT client ID |

### Zendure device

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `ZENDURE_IP` | Yes | — | Zendure device IP address |
| `ZENDURE_SN` | Yes | — | Zendure device serial number |
| `ZENDURE_POLL_INTERVAL` | No | `10` | Seconds between battery state polls |

### MQTT topics

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `SHELLY_TOPIC` | Yes | — | MQTT topic for Shelly Pro 3EM readings (e.g. `shellypro3em-XXXX/status/em:0`) |
| `HA_PUBLISH_PREFIX` | No | `zendure` | Prefix for MQTT topics published to HomeAssistant |

### Control thresholds

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `CHARGE_START_THRESHOLD` | No | `-100.0` | Grid power (W) below which charging starts. Negative = exporting |
| `DISCHARGE_START_THRESHOLD` | No | `0.0` | Grid power (W) above which discharging starts |
| `CHARGE_MARGIN` | No | `50` | Safety margin (W) subtracted from charge power to avoid grid import. Must be zero or positive |
| `DISCHARGE_MARGIN` | No | `5` | Safety margin (W) subtracted from discharge power. Must be zero or positive |
| `MIN_SOC` | No | `10` | Minimum SOC (%) — discharge is blocked at or below this level. Clamped to 0–100 |
| `MAX_SOC` | No | `100` | Maximum SOC (%) — charging is blocked at or above this level. Clamped to 0–100 |
| `BALANCE_WEEKDAY` | No | `mon` | Weekday (`Mon`–`Sun`) on which `MAX_SOC` is raised to 100% so the pack gets a periodic full charge for cell balancing. `none` disables the override |
| `SOLAR_PHASE` | No | `A` | Which Shelly phase (`A`, `B`, or `C`) the solar inverter feeds into |
| `SOLAR_DISCHARGE_BLOCK_THRESHOLD` | No | `0` | Solar export (W) on `SOLAR_PHASE` at or above which discharge is skipped, so large loads (e.g. EV charging) pull from grid+solar instead of draining the battery. `0` disables the guard, as does any negative value |

### Timing and safety

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `MIN_IDLE_BEFORE_DISCHARGE` | No | `300` | Minimum seconds of idle before discharge is allowed (prevents charge→discharge oscillation) |
| `MIN_MODE_DURATION` | No | `10` | Minimum seconds before a charge/discharge toggle is allowed |
| `MIN_DECISION_INTERVAL` | No | `5` | Minimum seconds between any two decisions (API protection) |
| `IDLE_TIMEOUT_MINUTES` | No | `5` | Minutes of continuous idle before entering standby |
| `CYCLE_WARN_THRESHOLD` | No | `200` | Daily mode transitions before forcing standby until midnight (0 = disabled) |
| `MQTT_TIMEOUT` | No | `60` | Seconds without MQTT updates before forcing idle as a safety failsafe. Idle is re-asserted every interval until updates resume, so a failed write is retried rather than disabling the failsafe |

### Other

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `TIMEZONE` | No | `UTC` | IANA timezone for cycle counting (e.g. `Europe/Amsterdam`) |
| `RTE_STATE_PATH` | No | `/var/lib/zendure/rte_state.json` | File path for persisting round-trip efficiency state across restarts. Must survive reboots — `/tmp` is cleared, which loses the rolling 24h window |
| `RUST_LOG` | No | — | Log level filter (e.g. `zendure=debug` for verbose output) |
| `JOURNAL_PATH` | No | `/var/lib/zendure/journal.db` | SQLite journal of events and decisions (see below). If it can't be opened, the journal is disabled and the controller starts normally |
| `JOURNAL_RETENTION_DAYS` | No | `90` | Days of journal history to keep (1–3650); older rows are deleted at startup and once a day. An unparseable or out-of-range value warns and keeps the default rather than failing startup — a logging setting must never stop the controller |

## Journal

Every Shelly reading, every Zendure poll response, every event the engine folds
in, and every decision (with the world and controller state it was decided
from, and the outcome of the command sent to each device) is recorded to a
SQLite database at `JOURNAL_PATH`.

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

`seq` orders the whole file. The two row tables have independent `id`
sequences, so `seq` is the only way to ask "what happened after this row?"
across both — which is what seeding a replay from a decision and then feeding it
the events that followed requires. It is assigned by the single writer thread in
the order records were handed to it, continues across restarts rather than
restarting per session, and leaves gaps where a write failed or a prune deleted.

`events.kind` is one of `shelly` and `zendure_poll` (payloads captured verbatim,
*before* parsing) or `meter`, `device_update` and `mqtt_timeout` (the engine's
own events, replayable). The first row of every session is a `device_update`
carrying the startup poll, so the world a replay rebuilds from events is the same
world the controller decided against from its first reading.

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
affect control — including a bad `JOURNAL_RETENTION_DAYS`, which warns and keeps
the default rather than stopping the controller.

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
min_soc=40`. The knobs are the ones in `sessions.config_json`, and an unknown
name is an error rather than a silent no-op.

**A fixture is hermetic.** It carries the tuning it was decided under, the engine
snapshot to resume from, and the events — and nothing about how to reach a broker
or a battery, because none of those are decision inputs. Both subcommands read no
environment variables at all, so a fixture can be copied off the box and replayed
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
MQTT_HOST=192.168.1.100 ZENDURE_IP=192.168.1.253 ZENDURE_SN=HEC4NENCN490270 SHELLY_TOPIC=shellypro3em-XXXX/status/em:0 cargo run
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
