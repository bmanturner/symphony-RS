//! Symphony-RS command-line entry point.
//!
//! Subcommands (filled in over Phase 7):
//!
//! - `symphony run`       — start the orchestrator daemon (default).
//! - `symphony validate`  — load `WORKFLOW.md` and report any errors.
//! - `symphony status`    — print a snapshot of in-flight runs.
//!
//! The binary's only job is composition: install logging, parse CLI
//! arguments, build the concrete adapters, and hand them to
//! `symphony_core::Orchestrator`. Nothing here should contain business
//! logic — anything that looks like policy belongs in a library crate.

mod cli;
mod logging;
mod run;
mod validate;

use std::process::ExitCode;

use clap::Parser;

use crate::cli::{Cli, Command};

fn main() -> anyhow::Result<ExitCode> {
    // `dotenvy` is a no-op when `.env` is absent; we call it before
    // logging::init so a developer-local `SYMPHONY_LOG=debug` in `.env`
    // takes effect for the rest of the process.
    let _ = dotenvy::dotenv();

    logging::init()?;

    let cli = Cli::parse();

    match cli.command {
        Some(Command::Validate(args)) => {
            // `validate::run` returns a typed outcome; `render` prints
            // it and yields the stable exit code documented on
            // `ValidateOutcome`. Going through `ExitCode` (rather than
            // `std::process::exit`) lets `main`'s destructors run.
            let outcome = validate::run(&args.path);
            let code = validate::render(&outcome);
            Ok(ExitCode::from(code as u8))
        }
        Some(Command::Run(args)) => {
            // `run` owns the orchestrator's lifecycle, which means it
            // owns the tokio runtime. We build it here (rather than
            // making `main` `#[tokio::main]`) so the synchronous
            // subcommands keep their cheap startup path — the tokio
            // runtime adds tens of milliseconds to a `validate` that
            // does no I/O.
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            match runtime.block_on(run::run(&args)) {
                Ok(()) => Ok(ExitCode::SUCCESS),
                Err(err) => {
                    eprintln!("error: {err:#}");
                    Ok(ExitCode::from(1))
                }
            }
        }
        Some(Command::Status) | None => {
            // `status` and the bare invocation land later in Phase 7.
            // Until then we emit a tracing event so smoke tests can
            // confirm the binary started, and exit non-zero so CI does
            // not treat a stub invocation as success.
            tracing::info!(
                target: "symphony::cli",
                version = env!("CARGO_PKG_VERSION"),
                "subcommand not yet implemented; only `validate` and `run` are wired in this iteration",
            );
            eprintln!(
                "symphony: only `validate` and `run` are implemented in this iteration; \
                 see `symphony --help`",
            );
            Ok(ExitCode::from(64))
        }
    }
}
