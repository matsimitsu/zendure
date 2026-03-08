# Zendure Controller

## Before committing

Run these checks and fix any issues before creating a commit:

```bash
cargo fmt
cargo clippy -- -D warnings
cargo test
cargo build
```

## Project structure

- `src/main.rs` — Entry point, coordinator loop
- `src/config.rs` — Environment variable configuration
- `src/models.rs` — All data types (MQTT, Zendure API, control decisions)
- `src/controller.rs` — Control logic (charge/discharge/idle decisions)
- `src/mqtt.rs` — MQTT subscriber, HA discovery, publishing
- `src/zendure.rs` — Zendure REST API client

## Releasing

Run `scripts/release.sh` to bump the patch version, tag, and push. The GitHub Actions release workflow builds a static musl binary and creates a GitHub release.
