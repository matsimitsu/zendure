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
mod publish;
mod replay;
mod rte;
mod run;
mod simulation;
mod source;
mod sync;
mod units;
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

    match cli::parse(args) {
        // Falls through to the controller below. No arguments has always meant
        // "run", and that is how the service invokes it.
        Ok(cli::Invocation::Daemon) => {}
        Ok(cli::Invocation::Help) => {
            print!("{}", cli::HELP);
            return Ok(());
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
    }

    // `RUST_LOG` wins outright when it is set. It used to be merged with a
    // hard-coded `zendure=info`, and `add_directive` *replaces* a directive with
    // the same target rather than merging — so `RUST_LOG=zendure=debug` was
    // silently overwritten back to `info` and the module's debug lines were
    // unreachable by the one incantation an operator would try.
    let filter = match std::env::var("RUST_LOG") {
        Ok(spec) if !spec.trim().is_empty() => tracing_subscriber::EnvFilter::new(spec),
        _ => tracing_subscriber::EnvFilter::new("zendure=info"),
    };
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let config = Config::from_env()?;

    // Registered before the loop starts, so a process that cannot be asked to
    // stop fails at startup rather than on the first `systemctl stop`.
    let stop = run::shutdown_signal()?;

    run::run(config, stop).await
}
