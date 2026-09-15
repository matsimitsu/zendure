mod allocate;
mod announce;
mod backpressure;
mod battery;
mod cli;
mod clock;
mod command;
mod commands;
mod config;
mod controller;
mod device;
mod engine;
mod event;
#[cfg(test)]
mod fixtures;
mod journal;
mod models;
mod mqtt;
mod prediction;
mod publish;
mod registry;
mod replay;
mod rte;
mod run;
mod simulation;
mod source;
mod sync;
mod units;
mod web;
mod world;
mod zendure;

use config::Config;

/// End a subcommand, printing any failure as a message rather than as a
/// `Debug`-formatted error struct.
fn finish(
    result: Result<(), Box<dyn std::error::Error>>,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Err(e) = result {
        eprintln!("{e}");
        std::process::exit(1);
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // `args_os` rather than `args`, which panics on argv that is not UTF-8 —
    // the parser has a clean error for an argument it does not understand, and
    // a panic is not it.
    let args = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned());

    let config_path = match cli::parse(args) {
        // Falls through to the controller below, carrying the path to read.
        // No arguments has always meant "run", and that is how the service
        // invokes it.
        Ok(cli::Invocation::Daemon { config }) => config,
        Ok(cli::Invocation::Help) => {
            print!("{}", cli::HELP);
            return Ok(());
        }
        Ok(cli::Invocation::CheckConfig { config }) => {
            return finish(commands::check_config(&config));
        }
        Ok(cli::Invocation::Export { from, to, db, out }) => {
            return finish(commands::export(&db, from, to, out.as_deref()));
        }
        Ok(cli::Invocation::Replay {
            fixture,
            verify,
            overrides,
        }) => return finish(commands::replay_fixture(&fixture, verify, &overrides)),
        Err(message) => {
            // Printed and exited rather than returned. `main` renders an `Err`
            // with `Debug`, which turns a multi-line usage message into one
            // quoted line full of `\n`.
            eprintln!("{message}");
            std::process::exit(2);
        }
    };

    // Read before the subscriber exists: building it needs `config.log_filter`,
    // and there's no subscriber yet to send a parse error to. `eprintln!` +
    // `exit(1)` rather than `?`, since `main` renders a returned `Err` with
    // `Debug`, turning a multi-line message into one quoted line full of `\n` — still
    // reaches the operator since systemd's `StandardError=` defaults to the journal.
    let (config, warnings) = match Config::from_toml(&config_path) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };

    // `RUST_LOG` wins outright when set: `add_directive` *replaces* a directive
    // with the same target rather than merging, so combining this with a
    // hard-coded `zendure=info` would silently overwrite `RUST_LOG=zendure=debug` back
    // to `info`, making debug lines unreachable.
    let filter = match std::env::var("RUST_LOG") {
        Ok(spec) if !spec.trim().is_empty() => tracing_subscriber::EnvFilter::new(spec),
        _ => tracing_subscriber::EnvFilter::new(config.log_filter.clone()),
    };
    tracing_subscriber::fmt().with_env_filter(filter).init();

    // Only now replayed as `tracing::warn!` — this ordering is the whole
    // reason `Config::from_toml` returns warnings instead of logging them
    // itself: logging one before the subscriber exists would just drop it.
    for warning in warnings {
        tracing::warn!("{warning}");
    }

    // Registered before the loop starts, so a process that cannot be asked to
    // stop fails at startup rather than on the first `systemctl stop`.
    let stop = run::shutdown_signal()?;

    // Built here, at the edge, and handed in — the same treatment `stop`
    // already gets, so something outside `run::run` can hold the device it constructs.
    // See `run::run`'s own doc comment for why that seam matters to a test.
    let devices = registry::from_config(&config);

    run::run(config, devices, stop).await
}
