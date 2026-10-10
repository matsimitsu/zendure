# Zendure AC 2400+ Controller

Smart controller for the Zendure AC 2400+ home battery. Reads net grid power from a Shelly Pro 3EM over its local HTTP API, decides when to charge or discharge the battery, and publishes decisions to MQTT for HomeAssistant integration.

## How it works

1. **Scans every source on one fixed tick** — the Shelly Pro 3EM at `/rpc/EM.GetStatus?id=0` (signed per-phase and total active power) and the Zendure over its local REST API (SOC, power, temperatures, pack data). A round decides only once every source has answered it, so every figure the decision reads is the same age
2. **Stands the fleet down** when a source has produced nothing usable for `[tuning] mqtt_timeout_secs` — an age bound, so a source that hangs and never reports a failure is caught too — and picks back up on its own as soon as it answers again
3. **Decides what the battery should do**:
   - **Charge** when there's excess solar being exported to the grid (up to 2400W)
   - **Discharge** to cover grid demand (up to inverter limit), after a configurable idle period to prevent charge/discharge oscillation
   - **Idle** otherwise
   - **Standby** after prolonged idle or when daily cycle limit is reached
5. **Safety guards**:
   - **SOC limits** — stops charging at max SOC (default 100%) and discharging at min SOC (default 10%); on the configured `[tuning] balance_weekday` (default Monday), max SOC is raised to 100% whatever `max_soc` says, so a deployment that keeps it lower for longevity (production runs 95) still gets a periodic cell-balancing full charge
   - **Cooldown** — prevents rapid charge/discharge toggling
   - **Ramp** — the first non-zero setpoint after a mode change goes out at 75% power, to avoid overshooting
   - **SOC calibration** — idles when the battery reports SOC calibration in progress
   - **Cycle limit** — forces standby when daily mode transitions exceed a threshold
   - **Device fault** — idles when the device reports an error (`isError`); `faultLevel` is ignored since it also goes non-zero for benign conditions like WiFi issues or firmware update checks
   - **Power caps** — the device stores its charge (`chargeMaxLimit`, 2400W) and discharge (`inverseMaxPower`, 800W) limits as setpoints it can reset to 0, which stalls all power flow. The controller writes them **once at startup** and never mid-run. If the device zeroes a cap while running, the controller honors it (that direction stops) rather than overwriting a value the device changed for reasons we can't see — recovery is a deliberate process restart.
   - **Device charge ceiling (`socSet`)** — the device has its own charge target, separate from anything this controller commands, and stops charging (`socLimit: 1`) at whatever it last held regardless of `[tuning] max_soc`. Written **once at startup** from `max_soc` (best-effort, like the power caps above) so raising `max_soc` actually raises the ceiling instead of silently changing nothing.
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

Three offline subcommands read the journal instead, and read **no configuration
at all** — not even `--config`'s default path — so they work against a copied
database on a machine with no broker and no battery:

```
zendure export  --from <when> --to <when> [--db <path>] [--out <file>]
zendure analyze --from <when> --to <when> [--db <path>]
zendure replay  <fixture> [--verify] [--set <knob>=<value>]...
```

`<when>` is unix milliseconds or RFC 3339. `--db` defaults to
`/var/lib/zendure/journal.db` and does **not** read `[journal] path` from a
config file — these subcommands read no configuration at all, so a deployment
that moves the journal has to say `--db` too. See [Analyze](#analyze) and
[Replay](#replay) below.

## Configuration

Configuration is a TOML file — `/etc/zendure/config.toml` by default, or
whatever `--config` points at. [`config.example.toml`](config.example.toml) is
the authoritative reference: every key, its default, and the failure policy
(what's fatal versus what warns and falls back) live there as comments,
production runs it verbatim, and a test in `config_tests.rs` pins the file to
that fact. The shape, briefly — a skeleton that mostly leans on the **built-in
defaults**, where `config.example.toml` records **what production actually
runs**, so the two differ wherever a knob has been tuned (`max_soc` and
`solar_discharge_block_threshold` today). Where they disagree, the example file
is the one describing a live system:

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
poll_interval_secs = 3      # the control tick; minimum 3, timeout derives from it

[shelly]
ip = "192.168.1.11"         # required
solar_phase = "A"           # A, B or C — which phase the solar inverter feeds

# Which meter feeds the engine. Absent means kind = "shelly", i.e. the
# [shelly] table above. See "Running against the simulator" below.
# [meter]
# kind = "shelly"              # or "synthetic"
# base_load = 500              # synthetic only, watts; required for it
# solar_peak = 3000            # synthetic only, watts; required for it

[homeassistant]
publish_prefix = "zendure"

# Presence, not a flag, turns the live dashboard on — absent runs no HTTP
# listener at all. See "Dashboard" below.
# [web]
# bind_address = "127.0.0.1"   # default
# port = 8080                  # default

# Presence, not a flag, turns the solar forecast poller on — absent runs none
# and the dashboard's forecast panel is empty. See "Dashboard" below.
# [prediction]
# kind = "solcast"             # or "simulated"
# api_key = "…"                # solcast only, required for it
# site_east = "…"              # solcast only, required for it
# site_west = "…"              # solcast only, required for it
# state_path = "/var/lib/zendure/prediction_state.json"
# poll_times = ["06:00", "09:30", "12:30", "15:30", "18:30"]   # default

# Presence, not a flag, turns electricity-price fetching on. All prices are
# ct/kWh; a negative value is legitimate.
# [prices]
# kind = "energyzero"          # or "simulated"; required
# poll_times = ["00:05", "15:00", "17:00"]   # default
# backfill_days = 60           # default; no point exceeding [journal] retention_days
#
# A dynamic contract: wholesale price plus these, then VAT.
# [prices.dynamic]
# markup = 1.5                 # supplier surcharge on import, excl. VAT
# energy_tax = 9.161           # excl. VAT, 2026 rate; it changes every January
# export_markup = 0.0          # deducted from the wholesale price on export, excl. VAT
# vat = 21.0                   # percent
#
# The fixed contract to compare against: flat rates, already incl. VAT.
# [prices.fixed]
# import = 28.0
# export = 5.0

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
charge_deadband = 25        # meter noise floor: adjustments smaller than this are dropped
discharge_deadband = 25
gain = 50                   # K: scales the error before it's integrated, 100 = unity
slew_limit = 400            # max change in commanded power per decision; anti-windup width too
charge_start_threshold = -100.0
discharge_start_threshold = 50.0   # > 0: discharge stops at 0 W, so this is the band
min_mode_duration_secs = 10
min_decision_interval_secs = 0
idle_timeout_secs = 300
min_idle_before_discharge_secs = 300
cycle_warn_threshold = 200
min_soc = 10
max_soc = 100
balance_weekday = "Mon"     # or "none"/"off" to disable the full-charge day
solar_discharge_block_threshold = 0.0
mqtt_timeout_secs = 60
```

**`mqtt.*`, `[[device]]` and `shelly.ip` are fatal if missing or the wrong
type** — getting one of those wrong means the controller talks to the wrong
thing or cannot talk at all. **`web.bind_address` and `web.port` are fatal only
when present and the wrong type**; it is the `[web]` table's presence that
decides whether the dashboard runs at all, and either key may be left out to
take its default. **Everything in `[tuning]`, plus the journal, rte and logging
settings, warns and falls back to its default** — getting one
of those wrong means the controller decides slightly differently, and a typo
there must never be the reason systemd restart-loops a controller that is
holding a battery command. `RUST_LOG`, when set and non-empty, overrides
`[logging].filter` outright — that's a `tracing` convention applied at the
point the subscriber is built, not something the config file itself reads.

**`device.poll_interval_secs` is the control tick.** It paces every source's
sample, and one decision is made per round. Every adapter's HTTP request
timeout is derived from it — the meter's as well as the battery's, half the
period, raised to 2 s and capped at 5 s wherever that still leaves it under the
period — so a stalled request is always abandoned before the next round is due,
and one unresponsive box never holds the tick at all: its sampler runs in its
own task and simply leaves its slot with nothing new. Below 3 s, the device's
own report refresh, it warns and is raised to 3. A `kind = "virtual"` device
answers in-process, so the floor does not apply to it: the key is optional there
and defaults to 1 s, and only zero is refused.

`[tuning] mqtt_timeout_secs` is measured against the same tick, so it must
outlast several rounds of it: a healthy source is one tick old when a tick reads
it, and a window inside that would stand the fleet down with everything
answering. Anything at or under three ticks warns and is raised to three.

`[tuning] discharge_start_threshold` is the import that starts a discharge, and
since only 0 W stops one, it is also the width of the band between idle and
discharge. At or below 0 there is no band and the mode flips on meter noise, so
such a value warns and takes the default, 50 W.

`[tuning] charge_deadband`/`discharge_deadband` guard the power law itself,
not mode selection: below this width, a tick's fresh adjustment is dropped and
the last commanded power holds steady instead. Unlike the margins above (which
exist to stay on the safe side of a reading), this exists because the reading
itself has a noise floor — live measurement put it at 10-30 W in the device's
reported power — and committing every sub-threshold wiggle as a new setpoint
is what turns a steady load into one that reverses direction on nearly every
other tick.

`[tuning] gain`/`slew_limit` tune the velocity-form law that reads the
deadbanded figure above: each decision integrates `gain` (`K`, percent, 100 =
unity) of the error onto the controller's own last commanded power, then
`slew_limit` bounds how far that can move in one decision — and, doubling as
anti-windup, how far the accumulator may run ahead of what the device last
reported actually achieving, so a battery too empty or full to follow a
setpoint doesn't wind up chasing one it can never reach. Either at 0 would
freeze the loop outright, so both warn and take their default the same way
`discharge_start_threshold` does.

`zendure --check --config <path>` runs the same parse, but strictly: a parse
error is fatal exactly as it is for the daemon, and **any warning is promoted
to a failure too**. Ansible runs it as a `validate:` hook before a rendered
config is moved into place, so a typo fails the deploy instead of quietly
degrading a running controller.

### Migrating from environment variables

Configuration moved from environment variables to a TOML file in version
0.2.27; the variables below are no longer read. If you had
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
| `SHELLY_TOPIC` | `[shelly] ip` | The meter is polled over HTTP; give its address, not a topic |
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
own events, replayable), or `solar_forecast` and `energy_price` (what the
forecast and price pollers fetched, for the dashboard and `analyze`; never
folded). The first *foldable* event of every session is a
`device_update` carrying the startup poll, so the world a replay rebuilds from
events is the same world the controller decided against from its first reading.
It is not necessarily the first row: the startup handshake captures its raw
`zendure_poll` body before anything parses it, so that one lands first. Those
are not fold inputs, which is why it does not matter.

Timestamps say when each reading was taken, not only when it was used. A
`device_update`'s `at` is when the battery answered. A `meter` event's `at` is
the tick that decided on it, and its `sampled_at` is when the meter was read.
The world in `state_json` keeps both under `sampled_at`, so the gap between the
meter and battery readings behind any decision can be read straight off its
row. Rows written before these existed read them as unknown (`null` or absent).

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

## Analyze

The journal records power and never energy — `rte.rs` is the only thing that
integrates, and it keeps a rolling 24 h window it never persists. `analyze`
re-integrates a stretch of the journal into daily totals, which is how you ask
whether a battery is short of capacity or short of power.

```
zendure analyze --from 2026-09-13T00:00:00Z --to 2026-09-17T00:00:00Z --db ./journal.db
```

```
day           cover   import   export  unstored  charged  discharged   >cap      soc
2026-09-14    100%     0.25     6.97      6.84     0.89        1.53   0.02   50-86%
2026-09-15    100%     0.74     8.81      8.64     1.87        2.49   0.44   11-80%

day            A imp    A exp    B imp    B exp    C imp    C exp
2026-09-14     1.05     8.36     1.47     0.96     0.07     0.00
2026-09-15     1.00    11.70     3.41     0.87     0.08     0.00
```

All kWh, bucketed by local calendar day. Two columns carry the argument:

- **`unstored`** is export while the battery was *not charging* — surplus that
  reached the grid because nothing took it. This is what more **capacity** would
  have held. It is deliberately not "export while at max SoC": a full battery is
  only one reason surplus escapes, and the question does not care which applied.
- **`>cap`** is demand above the inverter's own reported discharge limit,
  measured against the grid *underlying* the battery. This is what a second
  **inverter** would buy, and nothing else does.

**`cover` is not decoration.** The journal drops rows when its write queue is
full, deletes them when retention prunes, and a restart leaves a hole the width
of the outage. Intervals longer than 30 s are skipped rather than integrated
across — a gap beyond the scan cadence is an outage, not a measurement — so a
day under 100% is short by whatever happened in the hole, and anything under 95%
is called out below the tables.

Figures that depend on the battery (`unstored`, `>cap`, `charged`, `discharged`)
are absent rather than zero before the range's first `device_update`: with no
poll yet there is no way to know whether the battery was taking the surplus, and
reading "unknown" as "idle" would inflate exactly the number being asked for.
Battery telemetry arrives on the same tick as the meter reading it is decided
against, so the two are the same age; both are up to one tick old against the
instant the decision reaches the device.

### What-if cost: dynamic vs fixed

Given `--config` pointing at a file with both `[prices.dynamic]` and
`[prices.fixed]`, and a journal holding `energy_price` rows from the price
poller, `analyze` also prices the same import and export under each contract:

```
zendure analyze --from 2026-09-13T00:00:00Z --to 2026-09-17T00:00:00Z \
  --db ./journal.db --config /etc/zendure/config.toml
```

```
day          priced  dyn.import  dyn.export   dyn.net  fix.import  fix.export   fix.net         Δ
2026-09-13    100%       €0.24       €0.00     €0.24       €0.30       €0.00     €0.30    -€0.06
2026-09-14    100%       €0.24       €0.00     €0.24       €0.30       €0.00     €0.30    -€0.06
total         100%       €0.48       €0.00     €0.48       €0.60       €0.00     €0.60    -€0.12
```

- **`priced`** is the share of measured time that had a known price. Only that
  time is costed, under *both* contracts, so the two sides always describe the
  same energy; a gap in the price feed shows up here rather than as a cheaper day.
- **`dyn.import`** is import at `(wholesale + markup + energy_tax)` plus VAT;
  **`dyn.export`** credits export at `wholesale − export_markup`, VAT-free.
  Each interval is priced at the price in force when it started.
- **`fix.import`/`fix.export`** use the flat, VAT-inclusive rates.
- **net** is import paid less export credited. **Δ** is `dyn.net − fix.net`:
  negative means the dynamic contract would have been cheaper.

Each day is rounded to the cent once, and the total row is the sum of the rounded
days. Without `--config`, or without both tariffs, or with no prices in the
journal, the energy tables print as usual and the cost table is replaced by a
one-line note saying why. Price rows are read regardless of `--from`/`--to`: a
row is stamped when it was fetched, and a backfill fetches weeks of past prices
at once.

**Caveat: this ignores net metering (salderingsregeling).** Until it ends on
2027-01-01, a fixed contract nets export against import at the full import rate,
which this table does not do — it shows the post-2027 picture, and for 2026 it
understates how good the fixed contract is.

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

### Running against the simulator

No Zendure, no Shelly, no MQTT broker — `config.example.virtual.toml` runs
the whole controller against a `VirtualBattery` (a real integrating energy
model, not a stub that agrees with whatever it's told) fed by a synthetic
house meter that folds the battery's own flow back into its readings. The
simulated pack closes on a commanded setpoint at 800 W/s — the ~3 s response
time Zendure publishes — rather than arriving at it instantly, and the
synthetic solar curve moves continuously through the day rather than in
hourly steps, so what the loop is tuned against is not kinder than the
hardware. It works from a fresh clone with no setup:

```bash
cargo run -- --config config.example.virtual.toml
```

See that file for what each table means (and why `[mqtt]` and `[shelly]` are
both absent from it); briefly, `[[device]] kind = "virtual"` picks the
simulated battery and `[meter] kind = "synthetic"` picks the simulated house,
in place of `kind = "zendure"` and a real Shelly to poll. With no
`[mqtt]`, `run.rs` publishes through a `NullPublisher` instead of a real
queued sink — decisions and telemetry are made exactly as they would be
against real hardware, they just have nowhere to go over MQTT.

## HomeAssistant

The controller publishes MQTT discovery config automatically. These sensors appear in HA:

**Sensors:**
- `Zendure Controller Battery Decision Mode` — charge / discharge / idle / standby
- `Zendure Controller Battery Decision Power` — target power (W)
- `Zendure Controller Battery Decision Reason` — human-readable explanation
- `Zendure Controller Grid Power (at decision)` — net grid power used for the decision (W)
- `Zendure Controller Battery Round-Trip Efficiency` — charge/discharge RTE (%)
- `Zendure Controller Battery Usable Energy` — estimated usable energy remaining (kWh), measured above the higher of the device's own minimum SOC and `[tuning] min_soc`
- `Zendure Controller Battery Total Capacity` — total pack capacity (kWh)
- `Zendure Controller Battery Enclosure Temperature` — enclosure temperature (°C)
- `Zendure Controller Battery Pack N Temperature` — per-pack temperature (°C, dynamic)
- `Zendure Controller Battery Daily Mode Transitions` — charge/discharge/idle transitions today
- `Zendure Controller Battery Daily Cooldown Suppressions` — suppressed rapid toggles today

When `[car_battery]` is configured (see Dashboard, below), one more sensor is
published, under its own HA device ("Car Battery (via Zendure Controller)")
rather than "Zendure Controller" — it describes the car, not the stationary
battery:
- `Car Battery State of Charge` — the car's own SoC (%)

**Binary sensors:**
- `Zendure Controller Battery SOC Calibrating` — ON when SOC calibration is in progress

## Dashboard

Add `[web]` to run a live browser dashboard (`src/web/`) — solar/home/grid
stat cards, the battery panel (SOC, mode, RTE, usable energy, capacity, and
a row per pack with its model, serial, SOC, flow, temperature and capacity),
and a decision log, all real data, updating every scan tick over
server-sent events. Six routes: `GET /` (the full page), `GET /events`
(the SSE stream fragments it swaps in via htmx), `GET
/fragments/energy-flows?day=YYYY-MM-DD&interval=1h|15m` (the energy flows
panel alone, for one day), `GET /fragments/price-panel?day=YYYY-MM-DD` (the
price panel alone, for one day), `GET /fragments/forecast-panel?day=YYYY-MM-DD`
(the solar forecast panel alone, today or tomorrow, tomorrow only once its
forecast is complete) and `GET /detail/{entity}`
(`solar`, `home`, `grid` or `battery`; anything else is a 404). `GET /` takes
the same `day` and `interval` parameters, plus `price_day` for the price
panel and `solar_day` for the solar panel, so the three panels' days are chosen independently. Every parameter is
optional: a missing `day` means today, a day after today shows today, and a
value that isn't a date (or an interval other than `1h`/`15m`) is a 400. The solar,
home and grid cards and the battery panel open `/detail/{entity}` in a modal
`<dialog>` (Esc, the close button or a click beside it dismisses it); the
modal is a snapshot taken when opened, not live. The solar, home and grid
panels summarise the rolling 24 hours of 15-minute averages (now, peak,
energy produced/consumed or imported/exported/net, and the average) above a
power chart, where an interval with no readings is a break in the line
rather than an interpolation. The battery panel shows its SOC now, the energy
charged and discharged, and the SOC range over the same window; a table of
each pack's SOC range, charged and discharged energy and temperature range
over the 24 hours; then an SOC chart with the controller's limits dashed and
the SOC outside them shaded (a limit at 0% or 100% draws neither, so before
any limits are known the chart shows no window), and a power chart (positive
is discharge). Before
the battery has reported anything it says so instead. With htmx the route returns
just the panel; without it (JavaScript off, a pasted link) it returns a full
page with a link back to the dashboard. The EV card is not clickable. No `[web]` table means no
HTTP listener at all — the same brokerless-by-default rule `[mqtt]` follows —
and a bind failure warns and runs without the dashboard rather than failing
startup.

Every section of the page is live, including the status badge — it reads
`Meter offline` once a source has stopped answering and the controller has
stood the battery down, so a frozen page cannot keep claiming `Operational`.

The decision log is seeded from the journal at startup, so it survives a
restart. Rows from an earlier day are dated; the battery's mode badge is not
seeded and reads `Awaiting decision` until this process makes its first
decision, since a journalled row describes what the battery *was* doing, not
what it is doing now.

Consecutive decisions that command the same thing — the same mode and the same
setpoint — collapse into one row, marked `×12` with the run's start time behind
it. A quiet Idle stretch decides the same thing every few seconds, and without
this it would fill all twenty rows in under two minutes and push out everything
worth reading. The mode badge still follows every decision, collapsed or not.

The EV card shows the car's own battery state of charge when `[car_battery]`
is configured — see below — and a "No vehicle configured" placeholder
otherwise.

The energy flows panel charts today's solar, home, grid and battery power as
grouped bars on one signed kW scale, per hour (default) or per 15 minutes.
Grid reads positive while importing and negative while exporting; the
battery positive while discharging and negative while charging. Only
finished intervals are drawn, and a dashed line marks the one in progress.
The legend shows the most recent finished interval's averages. The panel
reads the dashboard's 15-minute history, which is seeded from the journal at
startup, so a restart keeps the day's bars. EV charging has no series of its
own: nothing measures it separately, so it counts as home usage.

The `‹`/`›` buttons in the panel's header step to earlier days, and `Today`
returns. A past day is read from the journal, folded through the same
history as the live chart and counting only the configured batteries, and
the last few rendered days are cached since a finished day does not change
(from a few minutes after its midnight, once its last rows have landed). A
day the journal cannot be read for says so instead of drawing an empty chart.
Days follow the configured timezone, so a DST change gives 23 or 25 hourly
bars (92 or 100 quarters) with the time labels over the hours they name; in
a zone that skips midnight itself, the day starts when the gap ends.
`‹` is disabled on the oldest day the journal still holds (or on any day,
when the journal's oldest day cannot be read), `›` on today.
Only today is live: while a past day is shown the stream's updates to the
panel are ignored, and going back to today resumes them. The buttons are
plain links to `/?day=…` too, so they work without JavaScript; the interval
being shown is kept when the day changes.

The forecast panel shows real solar predictions when `[prediction]` is
configured — one bar per half-hour of the local day (Solcast's own
resolution; 46 or 50 on a DST change) for the forecast, a line for today's
actual measured production drawn over the same axis, so the two are directly
comparable as the day unfolds. The line is each half-hour's mean power, read
from the same 15-minute averages as the flows chart, so a restart keeps it,
and it stops at the last finished half-hour. A readout above the chart shows
the current half-hour's forecast and actual; beside it, *So far* compares
today's measured production with the forecast for the same half-hours, and
*Still expected* sums the forecast from now to the last sunny half-hour.
`kind = "solcast"` fetches 48 hours of two Solcast rooftop forecasts
(east/west-facing panels on one array) and sums them; Solcast's free tier caps usage at 10 requests/day
account-wide, so by default the poller spends exactly 5 requests/site at
five fixed times spread across daylight hours (06:00, 09:30, 12:30, 15:30,
18:30 local — configurable via `prediction.poll_times`), skipping the night
hours nothing changes in. Solcast's response is forward-looking only, so
each fetch is merged into the cached series by timestamp rather than
replacing it outright — an already-elapsed slot keeps the bar an earlier
fetch gave it instead of losing it the moment a new poll lands, while a
timestamp both fetches cover takes the fresher estimate. A failed fetch
simply forfeits that slot's update, leaving the cache as it was. Every
successful fetch is also recorded to the journal (`solar_forecast` events),
independent of the dashboard's own today-only cache, so forecast history
survives past midnight for future historical views. `kind = "simulated"`
draws a synthetic clear-sky curve for today and tomorrow instead, with no
network call and no daily quota, for local testing. No `[prediction]` table means no poller runs at
all — the same rule `[mqtt]`/`[web]` follow — and the panel renders an empty
state. The poller only ever feeds the dashboard: nothing here reaches
`src/controller.rs`.

Electricity prices are fetched when both `[web]` and `[prices]` are
configured. At each `prices.poll_times` anchor (00:05, 15:00 and 17:00 local
by default) the poller fetches local today and tomorrow: day-ahead prices are
published around 14:00–15:00, so the 17:00 anchor is the retry for a late
publication. Anchors missed while the process was down collapse into one
fetch, and a failed fetch forfeits its anchor, the same as the forecast
poller. Every fetch that returns prices is recorded to the journal as an
`energy_price` event whose payload is a JSON array of
`{"from", "until", "wholesale"}` points (epoch milliseconds, ct/kWh); an
empty answer is logged and not recorded. Each EnergyZero point lasts one
hour, so an hour missing from the feed stays unpriced. On startup, after
the first due anchor's fetch, the poller backfills: for each of the last
`prices.backfill_days` local days (today excluded) that the recorded
`energy_price` payloads do not price every hour of, it fetches that day —
one request per day, in sequence — and journals it, then logs how many days
were fetched, already present or failed. A row's timestamp is when it was
fetched, so coverage is decided from the prices inside the payloads. The
dashboard is seeded from the journal's prices for the last 6 days onwards,
stamped with the newest row's fetch time. Nothing here reaches
`src/controller.rs`.

The price panel charts one local day at a time, one bar per local hour
(quarter-hour prices are averaged into their hour), tinted cheap, normal or
expensive by that day's thirds. With `[prices.dynamic]` configured the bars
are the all-in import price — `(wholesale + markup + energy_tax)` plus VAT,
the same figure `analyze` charges — and the subtitle says so; without it they
are the bare wholesale price. Every day shares one scale, and negative prices
hang below a zero line. Today shows a now line, dims the hours gone by, reads
the current hour by default and finds the cheapest and priciest 3 hours
still ahead; any other day reads its average and searches the whole day. The
day nav steps from 6 days back to tomorrow once tomorrow is fully priced
(today until then), with a Today button on past days; a day in range with no
prices says so and keeps the nav. Out-of-range days clamp to the nearest end.
Only today is live: the stream's updates are ignored while another day is
shown, except that a panel left on tomorrow goes live when the server's day
rolls over to it. The steps are plain links to `/?price_day=…` too, so they
work without JavaScript, and each panel's links keep the other panel's day.
A DST change gives 23 or 25 bars; on the autumn day the repeated hour names
its offset (`02:00–03:00 CEST`, then `02:00–03:00 CET`), and a range across
the change names both (`01:00 CEST–03:00 CET` is three hours).
Without `[prices]` the panel says how to configure it; with it, but before
the first fetch lands, it says it is waiting for prices. If today has no
prices while earlier days do, it waits with the nav still there.

The EV card shows a car's battery state of charge when `[car_battery]` is
configured. `kind = "vw_portal"` reads it from the VW Group EU Data Act
portal (`eu-data-act.drivesomethinggreater.com`) — the same subscription the
official VW/Cupra app can enable, and independent of any Home Assistant
integration for it. Requires `email`, `password` (a VW account with the
subscription active) and `vin` (found in the car's own app or registration
document — not auto-discovered, since this integration skips the portal's
vehicle-listing endpoint entirely). `country`/`language` default to `"de"`;
`poll_interval_secs` defaults to 900 (15 minutes), matching the portal's own
delivery cadence — polling faster only re-downloads the same snapshot.
`kind = "simulated"` draws a slowly drifting SoC instead, with no VW account
needed, for local testing. No `[car_battery]` table means no poller runs at
all and the EV card stays a placeholder. Like `[prediction]`, this only ever
feeds the dashboard and Home Assistant (see below): nothing here reaches
`src/controller.rs`.

## Releasing

```bash
./scripts/release.sh
```

This bumps the patch version in `Cargo.toml`, commits, tags with `vX.Y.Z`, and pushes. GitHub Actions then builds a static Linux binary and creates a release.
