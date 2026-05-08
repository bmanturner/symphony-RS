//! Implementation of the `symphony status` subcommand.
//!
//! `status` is a *point-in-time* snapshot, distinct from the live TUI
//! that lands in Phase 8 (`symphony watch`). It loads `WORKFLOW.md` with
//! the same [`LayeredLoader`] the daemon uses, materialises the
//! configured [`IssueTracker`], asks it for the current active issues,
//! and prints a compact human-readable table. It is the operational
//! answer to "if I started the orchestrator right now, what would it
//! pick up?".
//!
//! ## Why a snapshot rather than reaching into a running daemon
//!
//! Symphony-RS has no shared state between processes — the orchestrator
//! is a single-process daemon, and there is no socket / pid file to
//! attach to today. Reading from the tracker directly is honest about
//! that constraint: it tells the operator the same thing the next poll
//! tick would have told the daemon, with no risk of staleness from a
//! cached on-disk snapshot. Phase 8's `symphony watch` is the right
//! place to add live attach-to-daemon semantics; we deliberately do not
//! pre-empt it here.
//!
//! ## Exit codes
//!
//! - `0` — snapshot fetched successfully (printed to stdout).
//! - `1` — `WORKFLOW.md` could not be loaded (missing, malformed YAML,
//!   env override that does not satisfy the typed schema).
//! - `2` — the loaded config is semantically invalid.
//! - `3` — the tracker rejected our request (auth, network, malformed
//!   upstream payload). The error is printed to stderr.
//!
//! These match the splits used by `symphony validate` for codes 1 and 2
//! so a CI script can tell "config problem" apart from "tracker
//! problem" with one comparison.

use std::path::Path;
use std::sync::Arc;

use symphony_config::{
    ConfigValidationError, LayeredLoadError, LayeredLoader, LoadedWorkflow, TrackerKind,
};
use symphony_core::tracker::Issue;
use symphony_core::tracker_trait::{IssueTracker, TrackerError};

use crate::run::build_tracker;

/// Outcome of a `symphony status` invocation.
///
/// Mirrors [`crate::validate::ValidateOutcome`] for the loader stages so
/// the two subcommands report config problems with the same exit codes.
/// `TrackerFailed` is the only outcome unique to `status` — it lands when
/// the loader was happy but the live tracker call failed.
#[derive(Debug)]
pub enum StatusOutcome {
    /// Loader, validate, and tracker fetch all succeeded.
    Ok(Box<StatusSnapshot>),
    /// The loader rejected `WORKFLOW.md`.
    LoadFailed(LayeredLoadError),
    /// The loader succeeded but the typed config failed `validate()`.
    Invalid(Box<LoadedWorkflow>, ConfigValidationError),
    /// The loader succeeded but the tracker call returned an error.
    TrackerFailed(Box<LoadedWorkflow>, TrackerError),
}

impl StatusOutcome {
    /// Map an outcome to its stable exit code (see module docs).
    pub fn exit_code(&self) -> i32 {
        match self {
            StatusOutcome::Ok(_) => 0,
            StatusOutcome::LoadFailed(_) => 1,
            StatusOutcome::Invalid(_, _) => 2,
            StatusOutcome::TrackerFailed(_, _) => 3,
        }
    }
}

/// All the data `status` prints. Carrying the loaded config alongside
/// the issues lets the renderer surface the tracker label / agent kind
/// without re-reading anything.
#[derive(Debug)]
pub struct StatusSnapshot {
    /// The loaded `WORKFLOW.md` — exposed so the renderer can show the
    /// configured tracker / agent / polling values next to the live
    /// issue list.
    pub loaded: Box<LoadedWorkflow>,
    /// Active issues as the tracker reports them right now. Empty list
    /// is a legitimate snapshot (no work to dispatch), not an error.
    pub active: Vec<Issue>,
}

/// Top-level entry: load → validate → fetch.
///
/// Errors propagate as typed [`StatusOutcome`] variants so the renderer
/// can map them to stable exit codes. We intentionally do *not* swallow
/// the tracker error and pretend "0 issues" — an operator who runs
/// `symphony status` after configuring a bad token deserves to see the
/// failure, not a misleading empty table.
pub async fn run(path: &Path) -> StatusOutcome {
    let loaded = match LayeredLoader::from_path(path) {
        Ok(l) => l,
        Err(err) => return StatusOutcome::LoadFailed(err),
    };
    if let Err(err) = loaded.config.validate() {
        return StatusOutcome::Invalid(Box::new(loaded), err);
    }
    let tracker = match build_tracker(&loaded.config) {
        Ok(t) => t,
        Err(err) => {
            // `build_tracker` failures are credential / config shaped
            // (missing env var, malformed `owner/repo`). They surface
            // through the tracker channel because that is the layer the
            // operator was reaching toward, even though strictly the
            // failure happened before the network round trip.
            return StatusOutcome::TrackerFailed(
                Box::new(loaded),
                TrackerError::Other(format!("tracker init failed: {err:#}")),
            );
        }
    };
    snapshot_with_tracker(Box::new(loaded), tracker).await
}

/// Fetch the active issues from `tracker` and assemble a [`StatusSnapshot`].
///
/// Split out from [`run`] so unit tests can exercise the
/// snapshot/render path against a [`symphony_tracker::MockTracker`]
/// without touching the file system. Production code only calls [`run`].
pub async fn snapshot_with_tracker(
    loaded: Box<LoadedWorkflow>,
    tracker: Arc<dyn IssueTracker>,
) -> StatusOutcome {
    match tracker.fetch_active().await {
        Ok(active) => StatusOutcome::Ok(Box::new(StatusSnapshot { loaded, active })),
        Err(err) => StatusOutcome::TrackerFailed(loaded, err),
    }
}

/// Render `outcome` to stdout (success) or stderr (failure). Returns the
/// exit code the caller should propagate.
///
/// Output is intentionally human-first: a header line that names the
/// tracker, a short config summary, and an aligned table of active
/// issues. Machine-readable output (`--format=json`) is a future
/// enhancement; carving the typed [`StatusOutcome`] out of the renderer
/// keeps that swap a pure addition.
pub fn render(outcome: &StatusOutcome) -> i32 {
    match outcome {
        StatusOutcome::Ok(snap) => render_snapshot(snap),
        StatusOutcome::LoadFailed(err) => {
            eprintln!("error: failed to load workflow: {err}");
        }
        StatusOutcome::Invalid(loaded, err) => {
            eprintln!(
                "error: workflow at {} is semantically invalid: {err}",
                loaded.source_path.display()
            );
        }
        StatusOutcome::TrackerFailed(loaded, err) => {
            eprintln!(
                "error: tracker fetch failed for {}: {err}",
                loaded.source_path.display()
            );
        }
    }
    outcome.exit_code()
}

/// Print the success-case table. Extracted so [`render`] stays a flat
/// match on outcome variants.
fn render_snapshot(snap: &StatusSnapshot) {
    let cfg = &snap.loaded.config;
    let label = tracker_label(cfg.tracker.kind, cfg);
    println!(
        "SNAPSHOT: {} — {:?} ({}) — {} active",
        snap.loaded.source_path.display(),
        cfg.tracker.kind,
        label,
        snap.active.len(),
    );
    println!(
        "  config: poll_interval={}ms, max_concurrent={}, agent={:?}",
        cfg.polling.interval_ms, cfg.agent.max_concurrent_agents, cfg.agent.kind,
    );

    if snap.active.is_empty() {
        println!("  (no active issues)");
        return;
    }

    // Column widths chosen for the common case: identifier ≤ 12 chars,
    // state ≤ 14, branch ≤ 24. Long values are truncated with an ellipsis
    // so the layout doesn't shear when one issue has a 60-char title.
    println!(
        "  {:<12}  {:<14}  {:<8}  {:<24}  TITLE",
        "ID", "STATE", "PRIORITY", "BRANCH",
    );
    for issue in &snap.active {
        let priority = issue
            .priority
            .map(|p| format!("P{p}"))
            .unwrap_or_else(|| "-".to_string());
        let branch = issue.branch_name.as_deref().unwrap_or("-");
        println!(
            "  {:<12}  {:<14}  {:<8}  {:<24}  {}",
            truncate(&issue.identifier, 12),
            truncate(issue.state.as_str(), 14),
            truncate(&priority, 8),
            truncate(branch, 24),
            truncate(&issue.title, 60),
        );
    }

    // Surface blocker edges as a follow-up section rather than another
    // column — most issues have zero blockers and an extra column would
    // be empty noise. We only print the section when at least one issue
    // declares blockers.
    let with_blockers: Vec<&Issue> = snap
        .active
        .iter()
        .filter(|i| !i.blocked_by.is_empty())
        .collect();
    if !with_blockers.is_empty() {
        println!("  blockers:");
        for issue in with_blockers {
            let blockers: Vec<String> = issue
                .blocked_by
                .iter()
                .map(|b| {
                    b.identifier
                        .clone()
                        .or_else(|| b.id.as_ref().map(|id| id.as_str().to_string()))
                        .unwrap_or_else(|| "?".to_string())
                })
                .collect();
            println!("    {} ← {}", issue.identifier, blockers.join(", "));
        }
    }
}

/// Best-effort one-line label for the tracker (`project_slug` for
/// Linear, `repository` for GitHub). Falls back to `?` when the field
/// the kind expects is missing — `validate()` would have rejected that
/// config, so this branch is defensive.
fn tracker_label(kind: TrackerKind, cfg: &symphony_config::WorkflowConfig) -> String {
    match kind {
        TrackerKind::Linear => cfg
            .tracker
            .project_slug
            .clone()
            .unwrap_or_else(|| "?".to_string()),
        TrackerKind::Github => cfg
            .tracker
            .repository
            .clone()
            .unwrap_or_else(|| "?".to_string()),
    }
}

/// Truncate `s` to at most `max` *characters* (not bytes), appending `…`
/// when truncation occurred. Used by the table renderer so a single
/// pathological issue title can't shear the column layout.
fn truncate(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    // Reserve one column for the ellipsis. `max == 0` is degenerate but
    // guard against it to avoid an underflow panic.
    let take = max.saturating_sub(1);
    let mut out: String = s.chars().take(take).collect();
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use symphony_core::tracker::Issue;
    use symphony_core::tracker_trait::TrackerError;
    use symphony_tracker::MockTracker;

    fn workflow_file(body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("WORKFLOW.md");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        (dir, path)
    }

    fn ok_workflow() -> (tempfile::TempDir, std::path::PathBuf) {
        workflow_file("---\ntracker:\n  kind: linear\n  project_slug: ENG\n---\nbody\n")
    }

    /// The renderer must distinguish "load failed" from "loaded but
    /// invalid" so CI scripts can branch on the exit code.
    #[tokio::test]
    async fn run_returns_load_failed_for_missing_file() {
        let outcome = run(Path::new("/no/such/path/WORKFLOW.md")).await;
        assert!(matches!(outcome, StatusOutcome::LoadFailed(_)));
        assert_eq!(outcome.exit_code(), 1);
    }

    #[tokio::test]
    async fn run_returns_invalid_for_linear_without_project_slug() {
        let (_dir, path) = workflow_file("---\ntracker:\n  kind: linear\n---\nbody\n");
        let outcome = run(&path).await;
        match &outcome {
            StatusOutcome::Invalid(_, ConfigValidationError::LinearMissingProjectSlug) => {}
            other => panic!("expected Invalid(LinearMissingProjectSlug), got {other:?}"),
        }
        assert_eq!(outcome.exit_code(), 2);
    }

    #[tokio::test]
    async fn snapshot_with_tracker_collects_active_issues() {
        let (_dir, path) = ok_workflow();
        let loaded = LayeredLoader::from_path(&path).unwrap();
        let tracker = MockTracker::with_active(vec![
            Issue::minimal("id-1", "ABC-7", "Add fizz to buzz", "todo"),
            Issue::minimal("id-2", "ABC-9", "Wire dispatcher", "in progress"),
        ]);
        let outcome = snapshot_with_tracker(Box::new(loaded), Arc::new(tracker)).await;
        match outcome {
            StatusOutcome::Ok(snap) => {
                assert_eq!(snap.active.len(), 2);
                assert_eq!(snap.active[0].identifier, "ABC-7");
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn snapshot_with_tracker_propagates_tracker_errors() {
        let (_dir, path) = ok_workflow();
        let loaded = LayeredLoader::from_path(&path).unwrap();
        let tracker = MockTracker::new();
        tracker.enqueue_active_error(TrackerError::Other("boom".into()));
        let outcome = snapshot_with_tracker(Box::new(loaded), Arc::new(tracker)).await;
        match &outcome {
            StatusOutcome::TrackerFailed(_, err) => {
                assert!(err.to_string().contains("boom"));
            }
            other => panic!("expected TrackerFailed, got {other:?}"),
        }
        assert_eq!(outcome.exit_code(), 3);
    }

    /// `render` is mostly side-effecting (println!) but it returns the
    /// exit code we route through `main`. Pin the success-path code so a
    /// future refactor can't silently flip it.
    #[tokio::test]
    async fn render_returns_zero_on_ok_outcome() {
        let (_dir, path) = ok_workflow();
        let loaded = LayeredLoader::from_path(&path).unwrap();
        let tracker = MockTracker::with_active(vec![Issue::minimal(
            "id-1",
            "ABC-7",
            "Add fizz to buzz",
            "todo",
        )]);
        let outcome = snapshot_with_tracker(Box::new(loaded), Arc::new(tracker)).await;
        assert_eq!(render(&outcome), 0);
    }

    #[test]
    fn truncate_leaves_short_strings_alone() {
        assert_eq!(truncate("hi", 8), "hi");
    }

    #[test]
    fn truncate_appends_ellipsis_when_too_long() {
        // 8-char input truncated to 5 → 4 chars + ellipsis.
        let out = truncate("abcdefgh", 5);
        assert_eq!(out.chars().count(), 5);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn truncate_handles_max_zero_without_panicking() {
        // Degenerate case — exercising the saturating_sub guard.
        let out = truncate("abcdefgh", 0);
        // We don't pin the exact output (it's just an ellipsis); only
        // assert no panic and bounded length.
        assert!(out.chars().count() <= 1);
    }
}
