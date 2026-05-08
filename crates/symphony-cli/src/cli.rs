//! `clap` argument surface for the `symphony` binary.
//!
//! Subcommands land incrementally over Phase 7. This module defines the
//! parser and the per-subcommand result types, but does not contain
//! orchestrator wiring — the entry point in `main.rs` dispatches to
//! library calls so the parser stays trivially testable.
//!
//! ## Why a derive parser?
//!
//! `clap`'s derive API gives us a typed `Cli` struct that the binary
//! can match on exhaustively. That keeps `main.rs` honest: adding a new
//! subcommand to the enum is a hard compile error until `main` handles
//! it.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Top-level CLI parser.
///
/// `clap` automatically renders `--help` and `--version`. The binary
/// name shown in usage is taken from `Cargo.toml` (`symphony`).
#[derive(Debug, Parser)]
#[command(
    name = "symphony",
    version,
    about = "Symphony-RS — Rust port of OpenAI's Symphony orchestrator",
    long_about = None,
)]
pub struct Cli {
    /// Subcommand to dispatch. When omitted we fall back to `run` once
    /// Phase 7's `run` subcommand lands; until then a missing
    /// subcommand prints help and exits non-zero so a user is never
    /// surprised by a silent no-op.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// All Symphony-RS subcommands. The variants carry their own typed
/// argument structs so each subcommand owns its surface independently.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Load a `WORKFLOW.md` file, layer environment overrides on top,
    /// and verify that the resulting configuration is internally
    /// consistent (SPEC §5.3 / §10.1).
    ///
    /// Exits 0 on success, non-zero on the first error encountered.
    /// Intended for CI use and pre-deploy smoke tests; the orchestrator
    /// itself runs the same loader on startup.
    Validate(ValidateArgs),

    /// Start the orchestrator daemon: load `WORKFLOW.md`, build the
    /// configured tracker / agent / workspace adapters, and drive the
    /// poll loop until SIGINT is received.
    Run(RunArgs),

    /// Print a point-in-time snapshot of the configured tracker's
    /// active issues — i.e. what `symphony run` would dispatch on the
    /// next poll tick. Distinct from Phase 8's `symphony watch` live
    /// TUI: this command exits as soon as the tracker responds.
    Status(StatusArgs),
}

/// Arguments for `symphony status`.
///
/// Mirrors `validate` and `run`: a single positional path so an operator
/// can swap any of the three on the same command line. The tracker is
/// determined entirely by `WORKFLOW.md` front matter (SPEC §5.4) — no
/// CLI overrides.
#[derive(Debug, clap::Args)]
pub struct StatusArgs {
    /// Path to the `WORKFLOW.md` file to read. Relative paths resolve
    /// against the process's current working directory, matching the
    /// rule [`symphony_config::LayeredLoader`] uses at runtime.
    #[arg(value_name = "PATH", default_value = "WORKFLOW.md")]
    pub path: PathBuf,
}

/// Arguments for `symphony run`.
///
/// The single positional argument mirrors `symphony validate`'s contract
/// so an operator can swap one for the other on the command line. We
/// deliberately do not accept tracker / agent overrides on the CLI —
/// `WORKFLOW.md` is the single source of truth (SPEC §5.4); env layering
/// is the supported escape hatch for per-deployment differences.
#[derive(Debug, clap::Args)]
pub struct RunArgs {
    /// Path to the `WORKFLOW.md` file to drive the orchestrator from.
    #[arg(value_name = "PATH", default_value = "WORKFLOW.md")]
    pub path: PathBuf,
}

/// Arguments for `symphony validate`.
#[derive(Debug, clap::Args)]
pub struct ValidateArgs {
    /// Path to the `WORKFLOW.md` file to load. Relative paths are
    /// resolved against the process's current working directory, the
    /// same rule [`symphony_config::LayeredLoader`] uses at runtime.
    #[arg(value_name = "PATH", default_value = "WORKFLOW.md")]
    pub path: PathBuf,
}
