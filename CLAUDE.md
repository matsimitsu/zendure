# Zendure Controller

## Before committing

Run these checks and fix any issues before creating a commit:

```bash
cargo fmt
cargo clippy -- -D warnings
cargo test
cargo build
```

## Types

Physical quantities get newtypes. Never a bare `f64`/`i32`/`u32` for something
with a unit or a role — the type system must make the confusion unrepresentable
rather than leaving it to review.

- **Units.** `Watts`, `Amps`, `MilliAmps`, `WattHours`, `Percent`, `Millis`. A
  charger setpoint in milliamps (Peblar) and one in whole amps (Vestel) are
  different types; mixing them is a 1000x error that reaches hardware.
- **Roles.** A signed flow (`grid_power`), a non-negative cap
  (`max_charge_power`) and a commanded setpoint (`power_watts`) are all watts
  and must all be distinct types. Converting between them is an explicit call,
  never an implicit assignment.
- **Durations.** Never a bare count of seconds, minutes or millis — use
  `Duration` or a newtype that names the unit.
- **Validation belongs in the constructor.** `Soc::new` clamps once; call sites
  never re-check.
- **`#[serde(transparent)]`** so newtypes serialize as bare numbers and the
  wire formats (MQTT, HA discovery, the journal) are unchanged.

A cast (`as i32`, `as f64`) in the decision path is a smell: it means a quantity
crossed a boundary without anyone saying what the conversion meant.

## Project structure

- `src/main.rs` — Entry point, coordinator loop
- `src/config.rs` — Environment variable configuration
- `src/models.rs` — Wire types (MQTT, Zendure API) and control decisions
- `src/clock.rs` — Time context captured at the edge; the controller never reads a clock
- `src/event.rs` — What the engine can react to
- `src/engine.rs` — The event fold: `step(&Event) -> Step`
- `src/controller.rs` — Control logic (charge/discharge/idle decisions)
- `src/command.rs` — What gets sent to the device, split from the decision
- `src/battery.rs` — Battery state derived from device properties
- `src/mqtt.rs` — MQTT subscriber, HA discovery, publishing
- `src/zendure.rs` — Zendure REST API client
- `src/journal.rs` — Append-only SQLite record of events, decisions and outcomes
- `src/rte.rs` — Round-trip efficiency tracking

## Documentation

When behavior changes or environment variables are added/removed, update `README.md` to reflect the changes.

## Releasing

Run `scripts/release.sh` to bump the patch version, tag, and push. The GitHub Actions release workflow builds a static musl binary and creates a GitHub release.
