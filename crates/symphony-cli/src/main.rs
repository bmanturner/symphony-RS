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

mod cancel;
mod cli;
mod issue;
mod logging;
mod qa;
mod recover;
mod run;
mod scheduler;
mod sse;
mod status;
mod tui;
mod validate;
mod watch;

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
        Some(Command::Watch(args)) => {
            // `watch` is long-lived: it spins a tokio runtime so the
            // SSE client and the SIGINT listener can race the same
            // [`CancellationToken`]. `Ctrl+C` cancels both halves and
            // the function returns normally with exit code 0 — the
            // operator stopped a working command, not a failure.
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            let cancel = tokio_util::sync::CancellationToken::new();
            let cancel_for_signal = cancel.clone();
            let result = runtime.block_on(async move {
                let signal = tokio::spawn(async move {
                    if tokio::signal::ctrl_c().await.is_ok() {
                        cancel_for_signal.cancel();
                    }
                });
                let outcome = watch::run(&args, cancel).await;
                signal.abort();
                outcome
            });
            match result {
                Ok(()) => Ok(ExitCode::SUCCESS),
                Err(err) => {
                    eprintln!("error: {err:#}");
                    Ok(ExitCode::from(1))
                }
            }
        }
        Some(Command::Qa(args)) => {
            // `qa verdict` is read-only and synchronous: it opens the
            // durable SQLite store, joins one work item with its
            // verdict history, and prints the result. No tokio runtime
            // needed.
            let outcome = qa::run(&args.command);
            let code = qa::render(&outcome);
            Ok(ExitCode::from(code as u8))
        }
        Some(Command::Issue(args)) => {
            // `issue graph` is read-only and synchronous: it opens the
            // durable SQLite store, joins one work item plus its
            // typed-edge neighbourhood, and prints the result. No tokio
            // runtime needed.
            let outcome = issue::run(&args.command);
            let code = issue::render(&outcome);
            Ok(ExitCode::from(code as u8))
        }
        Some(Command::Recover(args)) => {
            // `recover` is a synchronous one-shot reconciliation: it
            // opens the durable SQLite database, reads (and optionally
            // mutates) a small number of stale rows, prints a summary,
            // and exits. No tokio runtime needed — every call site is
            // a synchronous repository method.
            let outcome = recover::run(&args);
            let code = recover::render(&outcome);
            Ok(ExitCode::from(code as u8))
        }
        Some(Command::Cancel(args)) => {
            // `cancel` is a synchronous one-shot: it opens the durable
            // SQLite database, mutates one (and at most a small number
            // of cascaded) rows, prints a one-line summary, and exits.
            // No tokio runtime needed — the durable cancel surface is
            // sync, and the kernel's propagator is pure.
            let outcome = cancel::run(&args);
            let code = cancel::render(&outcome);
            Ok(ExitCode::from(code as u8))
        }
        Some(Command::Status(args)) => {
            // `status` performs a single tracker fetch, so it builds a
            // small tokio runtime — the same shape as `run` but without
            // the long-lived poll loop.
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            let outcome = runtime.block_on(status::run(&args.path, args.state_db.as_deref()));
            let code = status::render(&outcome);
            Ok(ExitCode::from(code as u8))
        }
        None => {
            // No subcommand → print help and exit non-zero so callers
            // never mistake a bare invocation for a successful no-op.
            // The bare-invocation-defaults-to-`run` policy named in
            // SPEC §10.3 lands when the orchestrator has a graceful
            // shutdown story; the next checklist item adds it.
            tracing::info!(
                target: "symphony::cli",
                version = env!("CARGO_PKG_VERSION"),
                "no subcommand provided; printing help",
            );
            eprintln!("symphony: missing subcommand; see `symphony --help`");
            Ok(ExitCode::from(64))
        }
    }
}
