# Zendure AC 2400+ Controller

Smart controller for the Zendure AC 2400+ home battery. Reads grid power and solar production from MQTT, decides when to charge or discharge the battery, and publishes decisions to MQTT for HomeAssistant integration.

## How it works

1. **Subscribes to MQTT** for smart meter readings (DSMR P1) and solar inverter production
2. **Polls the Zendure device** via its local REST API for battery state (SOC, power, temperatures, pack data)
3. **Calculates net grid power** — uses kWh delta estimation to correct for the solar inverter on phase 1
4. **Decides what the battery should do**:
   - **Charge** when there's excess solar being exported to the grid (up to 2400W)
   - **Discharge** during evening/night/morning (17:00–07:00) to cover grid demand (up to inverter limit)
   - **Idle** otherwise
   - **Standby** after prolonged idle or when daily cycle limit is reached
5. **Safety guards**:
   - **Cooldown** — prevents rapid charge/discharge toggling
   - **Ramp** — starts at 75% power on mode changes to avoid overshooting
   - **SOC calibration** — idles when the battery reports SOC calibration in progress
   - **Cycle limit** — forces standby when daily mode transitions exceed a threshold
6. **Tracks round-trip efficiency** (RTE) — measures charge vs discharge energy, persisted to disk
7. **Publishes to MQTT** — HomeAssistant auto-discovers all sensors

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
| `METER_TOPIC` | No | `tele/ISK5MT174` | MQTT topic for smart meter readings (JSON) |
| `SOLAR_TOPIC` | No | `homeassistant/solar/inverter_active_power` | MQTT topic for solar production (plain number, watts) |
| `HA_PUBLISH_PREFIX` | No | `zendure` | Prefix for MQTT topics published to HomeAssistant |

### Control thresholds

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `CHARGE_START_THRESHOLD` | No | `-100.0` | Grid power (W) below which charging starts. Negative = exporting |
| `DISCHARGE_START_THRESHOLD` | No | `50.0` | Grid power (W) above which discharging starts during discharge hours |
| `CHARGE_MARGIN` | No | `50` | Safety margin (W) subtracted from charge power to avoid grid import |
| `DISCHARGE_MARGIN` | No | `5` | Safety margin (W) subtracted from discharge power |

### Timing and safety

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `MIN_MODE_DURATION` | No | `10` | Minimum seconds before a charge/discharge toggle is allowed |
| `MIN_DECISION_INTERVAL` | No | `5` | Minimum seconds between any two decisions (API protection) |
| `IDLE_TIMEOUT_MINUTES` | No | `5` | Minutes of continuous idle before entering standby |
| `CYCLE_WARN_THRESHOLD` | No | `200` | Daily mode transitions before forcing standby until midnight (0 = disabled) |

### Other

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `RTE_STATE_PATH` | No | `/tmp/zendure_rte_state.json` | File path for persisting round-trip efficiency state across restarts |
| `RUST_LOG` | No | — | Log level filter (e.g. `zendure=debug` for verbose output) |

## Running

```bash
MQTT_HOST=192.168.1.100 ZENDURE_IP=192.168.1.253 ZENDURE_SN=HEC4NENCN490270 cargo run
```

## HomeAssistant

The controller publishes MQTT discovery config automatically. These sensors appear in HA:

**Sensors:**
- `Zendure Controller Battery Decision Mode` — charge / discharge / idle / standby
- `Zendure Controller Battery Decision Power` — target power (W)
- `Zendure Controller Battery Decision Reason` — human-readable explanation
- `Zendure Controller Grid Power (at decision)` — net grid power used for the decision (W)
- `Zendure Controller Solar Power (at decision)` — solar production at time of decision (W)
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
