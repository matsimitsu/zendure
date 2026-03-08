# Zendure AC 2400+ Controller

Smart controller for the Zendure AC 2400+ home battery. Reads grid power and solar production from MQTT, decides when to charge or discharge the battery, and publishes decisions to MQTT for HomeAssistant integration.

## How it works

1. **Subscribes to MQTT** for smart meter readings (DSMR P1) and solar inverter production
2. **Calculates net grid power** — corrects for the solar inverter on phase 1 (the meter can't distinguish import/export per phase)
3. **Decides what the battery should do**:
   - **Charge** when there's excess solar being exported to the grid (up to 2400W)
   - **Discharge** during evening/night/morning (17:00–07:00) to cover grid demand (up to inverter limit, typically 800W)
   - **Idle** otherwise
4. **Publishes decisions to MQTT** — HomeAssistant auto-discovers the sensors for graphing

## Configuration

All configuration is via environment variables:

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `MQTT_HOST` | Yes | — | MQTT broker hostname |
| `MQTT_PORT` | No | `1883` | MQTT broker port |
| `MQTT_USERNAME` | No | — | MQTT username |
| `MQTT_PASSWORD` | No | — | MQTT password |
| `MQTT_CLIENT_ID` | No | `zendure-controller` | MQTT client ID |
| `ZENDURE_IP` | Yes | — | Zendure device IP address |
| `ZENDURE_SN` | Yes | — | Zendure device serial number |
| `METER_TOPIC` | No | `tele/ISK5MT174` | MQTT topic for smart meter readings |
| `SOLAR_TOPIC` | No | `homeassistant/solar/inverter_active_power` | MQTT topic for solar production |
| `HA_PUBLISH_PREFIX` | No | `zendure` | Prefix for MQTT topics published to HA |

## Running

```bash
MQTT_HOST=192.168.1.100 ZENDURE_IP=192.168.1.253 ZENDURE_SN=HEC4NENCN490270 cargo run
```

Enable debug logging with `RUST_LOG=zendure=debug`.

## HomeAssistant

The controller publishes MQTT discovery config automatically. These sensors appear in HA:

- `Zendure Controller Battery Decision Mode` — charge / discharge / idle
- `Zendure Controller Battery Decision Power` — target power (W)
- `Zendure Controller Battery Decision Reason` — human-readable explanation
- `Zendure Controller Grid Power (at decision)` — net grid power used for the decision (W)
- `Zendure Controller Solar Power (at decision)` — solar production at time of decision (W)

## Releasing

```bash
./scripts/release.sh
```

This bumps the patch version in `Cargo.toml`, commits, tags with `vX.Y.Z`, and pushes. GitHub Actions then builds a static Linux binary and creates a release.
