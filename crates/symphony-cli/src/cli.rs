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

    /// Live-tail the orchestrator's status stream. Connects to a
    /// `symphony run` daemon's SSE endpoint (default
    /// `http://127.0.0.1:6280/events`) and prints each
    /// [`symphony_core::events::OrchestratorEvent`] as it arrives.
    ///
    /// On connection loss the client retries with capped exponential
    /// backoff, reporting each `disconnected`/`reconnecting` transition
    /// on stderr so an operator never wonders why the stream went
    /// quiet. The TUI scaffold (next checklist item) replaces this
    /// stdout/stderr surface with `ratatui`; the SSE plumbing itself is
    /// stable from this iteration on.
    Watch(WatchArgs),

    /// Cooperatively cancel an in-flight run or work item (SPEC v2 §4.5
    /// / Phase 11.5).
    ///
    /// Enqueues a [`symphony_core::cancellation::CancelRequest`] in the
    /// durable `cancel_requests` table — runners observe pending entries
    /// at safe points (pre-lease and between agent steps) and transition
    /// the target to `RunStatus::Cancelled`. For work-item subjects, the
    /// command additionally cascades the cancel across the
    /// `parent_child` graph and enqueues per-run requests for every
    /// in-flight descendant run.
    ///
    /// Re-running the command for the same subject is idempotent: the
    /// partial unique index on `(subject_kind, subject_id) WHERE state =
    /// 'pending'` collapses duplicates to an in-place payload replace.
    /// The summary line distinguishes the first enqueue from a nudge.
    Cancel(CancelArgs),

    /// Issue-graph inspection commands (ARCHITECTURE v2 §6 / Phase 12).
    ///
    /// The kernel models work-item relationships as typed edges in
    /// `work_item_edges` (`parent_child`, `blocks`, `relates_to`,
    /// `followup_of`). Operators and tests need a way to read those
    /// edges without writing SQL — this group of subcommands serves that
    /// need.
    Issue(IssueArgs),

    /// Reconcile durable orchestrator state with on-disk / tracker
    /// reality (ARCHITECTURE v2 §4.7 / Phase 12).
    ///
    /// Surfaces (and, with `--apply`, reaps) two classes of stale rows:
    /// expired durable leases on `runs`, and orphaned `workspace_claims`
    /// — i.e. claims no live run references whose `claim_status` is not
    /// already terminal. Read-only by default; the `--apply` flag is
    /// required to mutate the database.
    Recover(RecoverArgs),

    /// QA evidence inspection commands (SPEC v2 §4.9 / §5.12 / Phase 12).
    ///
    /// The QA gate writes one durable `qa_verdicts` row per dispatch with
    /// the verdict, role authorship, optional waiver metadata, evidence,
    /// and acceptance-criteria trace. This namespace lets operators read
    /// those rows back without SQL — useful for audit ("what did QA
    /// decide on ENG-101?") and for debugging gate decisions.
    Qa(QaArgs),
}

/// Arguments for `symphony recover`.
///
/// Defaults are deliberately read-only: the command is dry-run unless
/// `--apply` is passed. The `--at` override exists for tests and for
/// operators who want to reproduce a recovery decision against a frozen
/// timestamp; production use omits it and the command derives a
/// second-precision RFC3339 `now` from the system clock, matching
/// [`crate::cancel`]'s clock plumbing.
#[derive(Debug, clap::Args)]
pub struct RecoverArgs {
    /// Path to the durable state SQLite database. The file must already
    /// exist — `recover` refuses to create one because mutating an
    /// unrecognised file is a footgun. Same default and rationale as
    /// `symphony cancel`.
    #[arg(
        long = "state-db",
        value_name = "PATH",
        default_value = "symphony.sqlite3"
    )]
    pub state_db: PathBuf,

    /// Override the RFC3339 `now` used for the expired-lease query. The
    /// kernel's lease comparison is `lease_expires_at < now`. Tests pass
    /// a fixed value; production omits this and the command derives `now`
    /// from the system clock.
    #[arg(long = "at", value_name = "RFC3339")]
    pub at: Option<String>,

    /// Promote the sweep from dry-run to a write. Without this flag the
    /// command surfaces what *would* be reaped and exits zero without
    /// touching the database.
    #[arg(long = "apply")]
    pub apply: bool,
}

/// Arguments for `symphony qa`.
///
/// Parallels [`IssueArgs`] in shape: the namespace exists so future
/// QA-adjacent inspection commands (e.g. `qa queue`, `qa runs`) can land
/// alongside `qa verdict` without reshuffling the top-level parser.
#[derive(Debug, clap::Args)]
pub struct QaArgs {
    /// The chosen `qa` subcommand.
    #[command(subcommand)]
    pub command: QaCommand,
}

/// Concrete `symphony qa` subcommands.
#[derive(Debug, Subcommand)]
pub enum QaCommand {
    /// Print every QA verdict filed against a given work item, newest
    /// first, by reading the durable `qa_verdicts` table.
    Verdict(QaVerdictArgs),
}

/// Arguments for `symphony qa verdict`.
///
/// The positional id is the durable `work_items.id` — the same
/// identifier shape that `symphony cancel --issue` and `symphony issue
/// graph` accept. Tracker-identifier resolution is a future concern
/// (see [`IssueGraphArgs`]).
#[derive(Debug, clap::Args)]
pub struct QaVerdictArgs {
    /// Numeric `work_items.id` whose verdicts to list.
    #[arg(value_name = "ID")]
    pub id: i64,

    /// Path to the durable state SQLite database. Must already exist —
    /// `qa verdict` is a read-only command and refuses to create a
    /// database on demand for the same reason `symphony cancel` and
    /// `symphony issue graph` do: mutating an unrecognised file is a
    /// footgun.
    #[arg(
        long = "state-db",
        value_name = "PATH",
        default_value = "symphony.sqlite3"
    )]
    pub state_db: PathBuf,
}

/// Arguments for `symphony issue`.
///
/// The `issue` namespace groups read-only inspection commands that
/// answer "what does the orchestrator believe about this work item?"
/// without mutating durable state. The first subcommand is `graph`;
/// future siblings (e.g. `qa-evidence`) will land alongside the
/// remaining Phase 12 items.
#[derive(Debug, clap::Args)]
pub struct IssueArgs {
    /// The chosen `issue` subcommand.
    #[command(subcommand)]
    pub command: IssueCommand,
}

/// Concrete `symphony issue` subcommands.
#[derive(Debug, Subcommand)]
pub enum IssueCommand {
    /// Print the parent/child/blocker/follow-up/relates-to graph rooted
    /// at a given work item, reading the durable `work_items` and
    /// `work_item_edges` tables.
    Graph(IssueGraphArgs),
}

/// Arguments for `symphony issue graph`.
///
/// The positional id is the durable `work_items.id` (matching the shape
/// `symphony cancel --issue` accepts). Tracker-identifier resolution
/// (e.g. `ENG-101` → numeric id) lands when `symphony status` surfaces
/// the durable id alongside each row; until then operators copy the id
/// from a direct DB read or from upcoming JSON status output.
#[derive(Debug, clap::Args)]
pub struct IssueGraphArgs {
    /// Numeric `work_items.id` of the root to inspect.
    #[arg(value_name = "ID")]
    pub id: i64,

    /// Path to the durable state SQLite database. Must already exist —
    /// `issue graph` is a read-only command and refuses to create a
    /// database on demand for the same reason `symphony cancel` does:
    /// mutating an unrecognised file is a footgun.
    #[arg(
        long = "state-db",
        value_name = "PATH",
        default_value = "symphony.sqlite3"
    )]
    pub state_db: PathBuf,
}

/// Arguments for `symphony cancel`.
///
/// Either `--run` or `--issue` must be supplied (clap's `ArgGroup`
/// enforces exactly-one). The numeric id is the durable `runs.id` /
/// `work_items.id`; resolving tracker identifiers (e.g. `ENG-101`) to
/// durable ids is a future enhancement that lands with the
/// observability commands in Phase 12.
#[derive(Debug, clap::Args)]
#[command(group(
    clap::ArgGroup::new("subject").required(true).args(["run", "issue"]),
))]
pub struct CancelArgs {
    /// Cancel a specific durable run by `runs.id`. Mutually exclusive
    /// with `--issue`.
    #[arg(long, value_name = "RUN_ID")]
    pub run: Option<i64>,

    /// Cancel a work item by `work_items.id`; the kernel-side propagator
    /// fans the cancel out to every in-flight descendant run via
    /// `parent_child` edges. Mutually exclusive with `--run`.
    #[arg(long, value_name = "WORK_ITEM_ID")]
    pub issue: Option<i64>,

    /// Operator-visible reason recorded in the durable
    /// `cancel_requests` row and propagated to every cascaded child
    /// request. Required so the audit trail always carries a why.
    #[arg(long, value_name = "TEXT")]
    pub reason: String,

    /// Identity recorded as `requested_by`. Defaults to `$USER` when
    /// unset, falling back to `"unknown"`. Cascaded child requests
    /// always overwrite this with `cascade:work_item:<id>` so the audit
    /// row distinguishes a direct operator cancel from a propagated
    /// one.
    #[arg(long, value_name = "WHO")]
    pub requested_by: Option<String>,

    /// Path to the durable state SQLite database. The file must already
    /// exist (the orchestrator creates it on first run); `cancel`
    /// refuses to create one because mutating an unrecognised database
    /// is a footgun.
    #[arg(
        long = "state-db",
        value_name = "PATH",
        default_value = "symphony.sqlite3"
    )]
    pub state_db: PathBuf,

    /// Override the `requested_at` RFC3339 timestamp written to the
    /// durable row. Tests pass a fixed value; production omits this and
    /// the command derives a UTC second-precision RFC3339 string from
    /// the system clock.
    #[arg(long = "at", value_name = "RFC3339")]
    pub at: Option<String>,
}

/// Arguments for `symphony watch`.
///
/// We accept a single `--url` flag rather than a host/port pair so an
/// operator can point the client at any HTTP endpoint that speaks the
/// Symphony SSE wire format — including a reverse-proxied deployment or
/// a local fixture server in tests.
#[derive(Debug, clap::Args)]
pub struct WatchArgs {
    /// SSE endpoint to subscribe to. Must be the full URL including
    /// path; the default matches the daemon's default bind plus the
    /// `/events` route configured in [`crate::sse::router`].
    #[arg(
        long,
        value_name = "URL",
        default_value = "http://127.0.0.1:6280/events"
    )]
    pub url: String,
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

    /// Optional path to the durable state SQLite database. When
    /// provided, `status` reads the orchestrator's source-of-truth view
    /// directly from the durable store (SPEC v2 §4.7 / ARCH v2 §4):
    /// every non-terminal work item plus the latest persisted handoff
    /// per row. The tracker is *not* called in this mode.
    ///
    /// When omitted, `status` falls back to a tracker-preview snapshot
    /// — useful before any orchestrator has run, but unable to surface
    /// role assignments, run state, or handoffs because none of that
    /// has been persisted yet.
    #[arg(long = "state-db", value_name = "PATH")]
    pub state_db: Option<PathBuf>,

    /// Emit a stable, machine-readable JSON document instead of the
    /// human-readable table. The schema is documented on
    /// [`crate::status::StatusJson`]: a top-level object with
    /// `mode`, `workflow_path`, `tracker`, `config`, and either an
    /// `items` array (durable mode) or an `active` array (tracker
    /// preview). Tooling — TUI panels, dashboards, smoke tests — should
    /// pin to this surface rather than scraping the table.
    ///
    /// Errors still go to stderr and still return their stable exit
    /// code; only the success path changes. Invalid combinations of
    /// `--json` with other flags are not rejected — the flag is purely
    /// an output toggle.
    #[arg(long = "json", default_value_t = false)]
    pub json: bool,
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

    /// Optional path to the durable state SQLite database. When
    /// provided, `symphony run` opens (and migrates) the database and
    /// the SSE surface streams from the durable event tail (SPEC v2
    /// §5.14) — a reconnecting consumer can resume from any sequence
    /// via `Last-Event-ID` or `?after=`. When omitted the SSE surface
    /// falls back to the in-process broadcast channel, suitable for
    /// the headless-CI codepath that does not persist events.
    #[arg(long = "state-db", value_name = "PATH")]
    pub state_db: Option<PathBuf>,
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
