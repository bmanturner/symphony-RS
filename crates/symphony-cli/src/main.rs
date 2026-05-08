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

mod logging;

fn main() -> anyhow::Result<()> {
    // `dotenvy` is a no-op when `.env` is absent; we call it before
    // logging::init so a developer-local `SYMPHONY_LOG=debug` in `.env`
    // takes effect for the rest of the process.
    let _ = dotenvy::dotenv();

    logging::init()?;

    // Bootstrap iteration: subcommand dispatch lands in Phase 7. Emit a
    // single tracing event so `symphony` exercised in smoke tests proves
    // the subscriber is actually wired.
    tracing::info!(
        target: "symphony::cli",
        version = env!("CARGO_PKG_VERSION"),
        "symphony-cli not yet implemented; subcommands land in Phase 7",
    );
    Ok(())
}
