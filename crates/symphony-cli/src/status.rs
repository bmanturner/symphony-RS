//! Implementation of the `symphony status` subcommand.
//!
//! `status` is a *point-in-time* snapshot of the orchestrator's view of
//! the world. It loads `WORKFLOW.md` with the same [`LayeredLoader`] the
//! daemon uses, then takes one of two paths depending on whether
//! `--state-db` is supplied:
//!
//! * **Durable mode** (`--state-db PATH`, SPEC v2 §4.7 / ARCH v2 §4):
//!   the durable SQLite store is the source of truth. We enumerate every
//!   non-terminal [`WorkItemRecord`] and join the latest persisted
//!   handoff per row. The tracker is *not* called — the operator wants
//!   to know what the orchestrator believes, not what the upstream
//!   tracker happens to advertise this second.
//! * **Tracker preview mode** (`--state-db` omitted): falls back to the
//!   pre-v2 behavior of asking the configured tracker for active issues
//!   directly. Useful before any orchestrator has run, or when the
//!   operator does not have access to the durable DB. This is honest
//!   about its limits — it cannot show role assignments, run state, or
//!   handoffs because none of that has been persisted yet.
//!
//! ## Why the split rather than always reading the tracker
//!
//! Symphony-RS v2 makes the durable store authoritative. Reading from
//! the tracker would silently lie about state-class normalization,
//! decomposition children, and handoff history — none of which the
//! tracker tracks. The fall-back is a convenience for operators who
//! genuinely have no DB yet, not the recommended path.
//!
//! ## Exit codes
//!
//! - `0` — snapshot fetched successfully (printed to stdout).
//! - `1` — `WORKFLOW.md` could not be loaded (missing, malformed YAML,
//!   env override that does not satisfy the typed schema).
//! - `2` — the loaded config is semantically invalid.
//! - `3` — tracker preview mode was selected and the tracker rejected
//!   our request (auth, network, malformed upstream payload). Only
//!   reachable when `--state-db` is *not* supplied.
//! - `4` — `--state-db` was provided but the durable state database
//!   could not be opened or queried. The tracker is not consulted in
//!   this mode, so there is no partial fallback view.
//!
//! These match the splits used by `symphony validate` for codes 1 and 2
//! so a CI script can tell "config problem" apart from "data plane
//! problem" with one comparison.

use std::path::Path;
use std::sync::Arc;

use serde::Serialize;
use symphony_config::{
    ConfigValidationError, InstructionPackBundle, LayeredLoadError, LayeredLoader, LoadedWorkflow,
    TrackerKind,
};
use symphony_core::tracker::Issue;
use symphony_core::tracker_trait::{TrackerError, TrackerRead};
use symphony_state::handoffs::{HandoffRecord, HandoffRepository};
use symphony_state::repository::{WorkItemRecord, WorkItemRepository};
use symphony_state::{StateDb, StateError};

use crate::run::build_tracker;

/// Stable string the durable state layer uses for the `tracker_id`
/// column. Mirrors the value the orchestrator inserts when it persists a
/// work item.
fn tracker_id_for(kind: TrackerKind) -> &'static str {
    match kind {
        TrackerKind::Linear => "linear",
        TrackerKind::Github => "github",
        TrackerKind::Mock => "mock",
    }
}

/// Outcome of a `symphony status` invocation.
#[derive(Debug)]
pub enum StatusOutcome {
    /// Loader, validate, and either tracker fetch or DB query succeeded.
    Ok(Box<StatusSnapshot>),
    /// The loader rejected `WORKFLOW.md`.
    LoadFailed(LayeredLoadError),
    /// The loader succeeded but the typed config failed `validate()`.
    Invalid(Box<LoadedWorkflow>, ConfigValidationError),
    /// Tracker preview mode (no `--state-db`) but the tracker call
    /// failed.
    TrackerFailed(Box<LoadedWorkflow>, TrackerError),
    /// Durable mode (`--state-db PATH`) but the database could not be
    /// opened or queried. There is no partial view in this mode — the
    /// durable store is the source of truth.
    StateDbFailed(Box<LoadedWorkflow>, StateError),
}

impl StatusOutcome {
    /// Map an outcome to its stable exit code (see module docs).
    pub fn exit_code(&self) -> i32 {
        match self {
            StatusOutcome::Ok(_) => 0,
            StatusOutcome::LoadFailed(_) => 1,
            StatusOutcome::Invalid(_, _) => 2,
            StatusOutcome::TrackerFailed(_, _) => 3,
            StatusOutcome::StateDbFailed(_, _) => 4,
        }
    }
}

/// All the data `status` prints. Carrying the loaded config alongside
/// the rows lets the renderer surface the configured tracker / agent
/// without re-reading anything.
#[derive(Debug)]
pub struct StatusSnapshot {
    /// The loaded `WORKFLOW.md`.
    pub loaded: Box<LoadedWorkflow>,
    /// Either the durable view or the tracker preview, depending on
    /// whether `--state-db` was supplied.
    pub view: StatusView,
}

/// The two distinct views `symphony status` can render.
#[derive(Debug)]
pub enum StatusView {
    /// Source-of-truth view assembled from the durable SQLite store.
    Durable {
        /// Every non-terminal work item the orchestrator has persisted,
        /// most recently updated first.
        items: Vec<DurableWorkItemRow>,
    },
    /// Pre-v2 fallback view assembled by asking the tracker directly.
    /// Used when no `--state-db` is supplied.
    TrackerPreview {
        /// Active issues as the tracker reports them right now.
        active: Vec<Issue>,
    },
}

/// One row in the [`StatusView::Durable`] table — the persisted work
/// item plus its latest handoff envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableWorkItemRow {
    /// The work item as the orchestrator stored it.
    pub work_item: WorkItemRecord,
    /// The most recent persisted handoff for this work item, if any.
    pub latest_handoff: Option<HandoffRecord>,
}

/// Top-level entry: load → validate → fetch.
pub async fn run(path: &Path, state_db: Option<&Path>) -> StatusOutcome {
    let loaded = match LayeredLoader::from_path(path) {
        Ok(l) => l,
        Err(err) => return StatusOutcome::LoadFailed(err),
    };
    if let Err(err) = loaded.config.validate() {
        return StatusOutcome::Invalid(Box::new(loaded), err);
    }
    let loaded = Box::new(loaded);

    match state_db {
        Some(db_path) => snapshot_from_durable(loaded, db_path),
        None => snapshot_from_tracker_preview(loaded).await,
    }
}

/// Durable mode: enumerate non-terminal work items from the SQLite store
/// and attach the latest handoff per row. Tracker is not consulted.
fn snapshot_from_durable(loaded: Box<LoadedWorkflow>, db_path: &Path) -> StatusOutcome {
    match load_durable_rows(db_path) {
        Ok(items) => StatusOutcome::Ok(Box::new(StatusSnapshot {
            loaded,
            view: StatusView::Durable { items },
        })),
        Err(err) => StatusOutcome::StateDbFailed(loaded, err),
    }
}

fn load_durable_rows(db_path: &Path) -> Result<Vec<DurableWorkItemRow>, StateError> {
    let db = StateDb::open(db_path)?;
    let work_items = db.list_active_work_items()?;
    let mut rows = Vec::with_capacity(work_items.len());
    for work_item in work_items {
        let latest_handoff = db.latest_handoff_for_work_item(work_item.id)?;
        rows.push(DurableWorkItemRow {
            work_item,
            latest_handoff,
        });
    }
    Ok(rows)
}

/// Tracker preview mode: build the configured tracker and ask for
/// active issues. Used only when `--state-db` is omitted.
async fn snapshot_from_tracker_preview(loaded: Box<LoadedWorkflow>) -> StatusOutcome {
    let tracker = match build_tracker(&loaded.config, &loaded.source_path) {
        Ok(t) => t,
        Err(err) => {
            return StatusOutcome::TrackerFailed(
                loaded,
                TrackerError::Other(format!("tracker init failed: {err:#}")),
            );
        }
    };
    snapshot_with_tracker(loaded, tracker).await
}

/// Tracker-preview helper exposed for tests so they can inject a
/// [`symphony_tracker::MockTracker`] without touching the file system.
pub async fn snapshot_with_tracker(
    loaded: Box<LoadedWorkflow>,
    tracker: Arc<dyn TrackerRead>,
) -> StatusOutcome {
    match tracker.fetch_active().await {
        Ok(active) => StatusOutcome::Ok(Box::new(StatusSnapshot {
            loaded,
            view: StatusView::TrackerPreview { active },
        })),
        Err(err) => StatusOutcome::TrackerFailed(loaded, err),
    }
}

/// Render `outcome` to stdout (success) or stderr (failure). Returns the
/// exit code the caller should propagate.
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
        StatusOutcome::StateDbFailed(loaded, err) => {
            eprintln!(
                "error: state DB read failed for {}: {err}",
                loaded.source_path.display(),
            );
        }
    }
    outcome.exit_code()
}

/// JSON-mode counterpart to [`render`]. On the success path, prints a
/// stable [`StatusJson`] document to stdout; on the failure paths,
/// prints a `{"error": ...}` envelope to stderr while still returning
/// the same stable exit code as [`render`]. Tooling can therefore
/// distinguish "no work" from "could not produce a snapshot" by
/// looking at exit code first and parsing JSON second.
pub fn render_json(outcome: &StatusOutcome) -> i32 {
    match outcome {
        StatusOutcome::Ok(snap) => {
            let doc = StatusJson::from_snapshot(snap);
            // `serde_json` cannot fail on this shape — every field is
            // a string, bool, integer, or `Vec`. Unwrap is fine; the
            // alternative is an unreachable error branch.
            let s = serde_json::to_string_pretty(&doc).unwrap();
            println!("{s}");
        }
        StatusOutcome::LoadFailed(err) => {
            print_error_json("load_failed", &err.to_string(), None);
        }
        StatusOutcome::Invalid(loaded, err) => {
            print_error_json(
                "invalid_workflow",
                &err.to_string(),
                Some(&loaded.source_path.display().to_string()),
            );
        }
        StatusOutcome::TrackerFailed(loaded, err) => {
            print_error_json(
                "tracker_failed",
                &err.to_string(),
                Some(&loaded.source_path.display().to_string()),
            );
        }
        StatusOutcome::StateDbFailed(loaded, err) => {
            print_error_json(
                "state_db_failed",
                &err.to_string(),
                Some(&loaded.source_path.display().to_string()),
            );
        }
    }
    outcome.exit_code()
}

fn print_error_json(kind: &str, message: &str, workflow_path: Option<&str>) {
    let env = ErrorJson {
        error: kind,
        message,
        workflow_path,
    };
    let s = serde_json::to_string_pretty(&env).unwrap();
    eprintln!("{s}");
}

/// Failure-path JSON envelope. Carrying `error` as a discriminator
/// keeps the contract honest with the documented exit codes (a
/// consumer pinning to `error == "tracker_failed"` matches exit code
/// 3, etc.).
#[derive(Debug, Serialize)]
struct ErrorJson<'a> {
    error: &'a str,
    message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    workflow_path: Option<&'a str>,
}

/// Stable JSON-mode schema for `symphony status --json`.
///
/// The shape is intentionally flat and explicit (no `serde(flatten)`
/// or untagged enums) so a consumer can write a one-line `jq` filter
/// against it without referencing Rust types. Field names are
/// `snake_case` to match the rest of Symphony's persisted payloads.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct StatusJson {
    /// `"durable"` when `--state-db` was supplied, `"tracker_preview"`
    /// otherwise. Mirrors the `[mode]` tag the human renderer prints.
    pub mode: &'static str,
    /// Absolute or operator-supplied path to the loaded `WORKFLOW.md`.
    pub workflow_path: String,
    /// Tracker selection summary derived from the loaded config.
    pub tracker: TrackerJson,
    /// Tunables the orchestrator would apply on a `run`.
    pub config: ConfigJson,
    /// Durable-mode rows. Empty in tracker-preview mode.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<DurableRowJson>,
    /// Tracker-preview rows. Empty in durable mode.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active: Vec<TrackerIssueJson>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct TrackerJson {
    /// Lower-case discriminator (`"linear"`, `"github"`, `"mock"`).
    pub kind: &'static str,
    /// Best-effort label: `project_slug` for Linear, `repository` for
    /// GitHub, fixtures path for Mock. Mirrors [`tracker_label`].
    pub label: String,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct ConfigJson {
    pub poll_interval_ms: u64,
    pub max_concurrent_agents: u32,
    pub agent_kind: String,
    pub role_instruction_provenance: Vec<RoleInstructionProvenanceJson>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct RoleInstructionProvenanceJson {
    pub role: String,
    pub role_prompt_path: Option<String>,
    pub role_prompt_hash: Option<String>,
    pub soul_path: Option<String>,
    pub soul_hash: Option<String>,
}

/// Durable-mode row in JSON form. Keeps the on-wire field set narrow
/// — adding fields is a non-breaking change; renaming or removing one
/// is.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct DurableRowJson {
    pub id: i64,
    pub tracker_id: String,
    pub identifier: String,
    pub title: String,
    pub status_class: String,
    pub tracker_status: String,
    pub assigned_role: Option<String>,
    pub assigned_agent: Option<String>,
    pub priority: Option<String>,
    pub parent_id: Option<i64>,
    pub created_at: String,
    pub updated_at: String,
    /// Latest persisted handoff for this row, if any. The full handoff
    /// history is reachable via future commands; the `--json` snapshot
    /// matches the human renderer's "latest only" projection.
    pub latest_handoff: Option<HandoffJson>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct HandoffJson {
    pub id: i64,
    pub run_id: i64,
    pub ready_for: String,
    pub summary: String,
    pub created_at: String,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct TrackerIssueJson {
    pub id: String,
    pub identifier: String,
    pub title: String,
    pub state: String,
    pub priority: Option<i32>,
    pub branch_name: Option<String>,
    pub blocked_by: Vec<String>,
}

impl StatusJson {
    /// Project a [`StatusSnapshot`] into the stable JSON schema. Pure
    /// — exposed for tests so they can pin field names without spawning
    /// the binary.
    pub fn from_snapshot(snap: &StatusSnapshot) -> StatusJson {
        let cfg = &snap.loaded.config;
        let tracker = TrackerJson {
            kind: tracker_kind_str(cfg.tracker.kind),
            label: tracker_label(cfg.tracker.kind, cfg),
        };
        let config = ConfigJson {
            poll_interval_ms: cfg.polling.interval_ms,
            max_concurrent_agents: cfg.agent.max_concurrent_agents,
            agent_kind: format!("{:?}", cfg.agent.kind),
            role_instruction_provenance: role_instruction_provenance(
                &snap.loaded.instruction_packs,
            ),
        };
        let workflow_path = snap.loaded.source_path.display().to_string();

        match &snap.view {
            StatusView::Durable { items } => {
                let items = items.iter().map(durable_row_to_json).collect();
                StatusJson {
                    mode: "durable",
                    workflow_path,
                    tracker,
                    config,
                    items,
                    active: Vec::new(),
                }
            }
            StatusView::TrackerPreview { active } => {
                let active = active.iter().map(tracker_issue_to_json).collect();
                StatusJson {
                    mode: "tracker_preview",
                    workflow_path,
                    tracker,
                    config,
                    items: Vec::new(),
                    active,
                }
            }
        }
    }
}

fn durable_row_to_json(row: &DurableWorkItemRow) -> DurableRowJson {
    let wi = &row.work_item;
    DurableRowJson {
        id: wi.id.0,
        tracker_id: wi.tracker_id.clone(),
        identifier: wi.identifier.clone(),
        title: wi.title.clone(),
        status_class: wi.status_class.clone(),
        tracker_status: wi.tracker_status.clone(),
        assigned_role: wi.assigned_role.clone(),
        assigned_agent: wi.assigned_agent.clone(),
        priority: wi.priority.clone(),
        parent_id: wi.parent_id.map(|p| p.0),
        created_at: wi.created_at.clone(),
        updated_at: wi.updated_at.clone(),
        latest_handoff: row.latest_handoff.as_ref().map(handoff_to_json),
    }
}

fn handoff_to_json(h: &HandoffRecord) -> HandoffJson {
    HandoffJson {
        id: h.id.0,
        run_id: h.run_id.0,
        ready_for: h.ready_for.clone(),
        summary: h.summary.clone(),
        created_at: h.created_at.clone(),
    }
}

fn tracker_issue_to_json(issue: &Issue) -> TrackerIssueJson {
    let blocked_by = issue
        .blocked_by
        .iter()
        .map(|b| {
            b.identifier
                .clone()
                .or_else(|| b.id.as_ref().map(|id| id.as_str().to_string()))
                .unwrap_or_else(|| "?".to_string())
        })
        .collect();
    TrackerIssueJson {
        id: issue.id.as_str().to_string(),
        identifier: issue.identifier.clone(),
        title: issue.title.clone(),
        state: issue.state.as_str().to_string(),
        priority: issue.priority,
        branch_name: issue.branch_name.clone(),
        blocked_by,
    }
}

pub(crate) fn role_instruction_provenance(
    packs: &InstructionPackBundle,
) -> Vec<RoleInstructionProvenanceJson> {
    packs
        .roles
        .iter()
        .map(|(role, pack)| RoleInstructionProvenanceJson {
            role: role.clone(),
            role_prompt_path: pack
                .role_prompt
                .as_ref()
                .map(|instruction| instruction.source.path.display().to_string()),
            role_prompt_hash: pack
                .role_prompt
                .as_ref()
                .map(|instruction| instruction.source.content_hash.clone()),
            soul_path: pack
                .soul
                .as_ref()
                .map(|instruction| instruction.source.path.display().to_string()),
            soul_hash: pack
                .soul
                .as_ref()
                .map(|instruction| instruction.source.content_hash.clone()),
        })
        .collect()
}

fn tracker_kind_str(kind: TrackerKind) -> &'static str {
    match kind {
        TrackerKind::Linear => "linear",
        TrackerKind::Github => "github",
        TrackerKind::Mock => "mock",
    }
}

fn render_snapshot(snap: &StatusSnapshot) {
    let cfg = &snap.loaded.config;
    let label = tracker_label(cfg.tracker.kind, cfg);
    let (mode_label, count) = match &snap.view {
        StatusView::Durable { items } => ("durable", items.len()),
        StatusView::TrackerPreview { active } => ("tracker preview", active.len()),
    };
    println!(
        "SNAPSHOT: {} — {:?} ({}) — {} active [{}]",
        snap.loaded.source_path.display(),
        cfg.tracker.kind,
        label,
        count,
        mode_label,
    );
    println!(
        "  config: poll_interval={}ms, max_concurrent={}, agent={:?}",
        cfg.polling.interval_ms, cfg.agent.max_concurrent_agents, cfg.agent.kind,
    );
    let provenance = role_instruction_provenance(&snap.loaded.instruction_packs);
    if !provenance.is_empty() {
        println!("  role instructions:");
        for item in provenance {
            println!(
                "    {} role_prompt={} soul={}",
                item.role,
                item.role_prompt_hash.as_deref().unwrap_or("-"),
                item.soul_hash.as_deref().unwrap_or("-")
            );
        }
    }

    match &snap.view {
        StatusView::Durable { items } => render_durable(items, cfg.tracker.kind),
        StatusView::TrackerPreview { active } => render_tracker_preview(active),
    }
}

fn render_durable(items: &[DurableWorkItemRow], expected_kind: TrackerKind) {
    if items.is_empty() {
        println!("  (no active work items in durable state)");
        return;
    }
    let expected_tracker = tracker_id_for(expected_kind);
    println!(
        "  {:<14}  {:<12}  {:<14}  {:<14}  TITLE",
        "ID", "STATUS_CLASS", "TRACKER_STATUS", "ROLE",
    );
    for row in items {
        let wi = &row.work_item;
        let role = wi.assigned_role.as_deref().unwrap_or("-");
        // Visibly mark cross-tracker rows so an operator running
        // `status` against a workflow that points at tracker `X`
        // notices when the DB also holds rows for tracker `Y` (e.g.
        // partial migration, multi-tracker repo). Cheap and honest.
        let id_label = if wi.tracker_id == expected_tracker {
            wi.identifier.clone()
        } else {
            format!("{}:{}", wi.tracker_id, wi.identifier)
        };
        println!(
            "  {:<14}  {:<12}  {:<14}  {:<14}  {}",
            truncate(&id_label, 14),
            truncate(&wi.status_class, 12),
            truncate(&wi.tracker_status, 14),
            truncate(role, 14),
            truncate(&wi.title, 60),
        );
    }

    let with_handoffs: Vec<&DurableWorkItemRow> = items
        .iter()
        .filter(|r| r.latest_handoff.is_some())
        .collect();
    if !with_handoffs.is_empty() {
        println!("  handoffs:");
        for row in with_handoffs {
            let h = row.latest_handoff.as_ref().unwrap();
            println!(
                "    {} → {} — {}",
                row.work_item.identifier,
                h.ready_for,
                truncate(&h.summary, 80),
            );
        }
    }
}

fn render_tracker_preview(active: &[Issue]) {
    if active.is_empty() {
        println!("  (no active issues — tracker preview)");
        return;
    }

    println!(
        "  {:<12}  {:<14}  {:<8}  {:<24}  TITLE",
        "ID", "STATE", "PRIORITY", "BRANCH",
    );
    for issue in active {
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

    let with_blockers: Vec<&Issue> = active.iter().filter(|i| !i.blocked_by.is_empty()).collect();
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
        TrackerKind::Mock => cfg
            .tracker
            .fixtures
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "?".to_string()),
    }
}

fn truncate(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
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

    #[tokio::test]
    async fn run_returns_load_failed_for_missing_file() {
        let outcome = run(Path::new("/no/such/path/WORKFLOW.md"), None).await;
        assert!(matches!(outcome, StatusOutcome::LoadFailed(_)));
        assert_eq!(outcome.exit_code(), 1);
    }

    #[tokio::test]
    async fn run_returns_invalid_for_linear_without_project_slug() {
        let (_dir, path) = workflow_file("---\ntracker:\n  kind: linear\n---\nbody\n");
        let outcome = run(&path, None).await;
        match &outcome {
            StatusOutcome::Invalid(_, ConfigValidationError::LinearMissingProjectSlug) => {}
            other => panic!("expected Invalid(LinearMissingProjectSlug), got {other:?}"),
        }
        assert_eq!(outcome.exit_code(), 2);
    }

    #[tokio::test]
    async fn tracker_preview_returns_active_issues_when_no_state_db() {
        let (_dir, path) = ok_workflow();
        let loaded = LayeredLoader::from_path(&path).unwrap();
        let tracker = MockTracker::with_active(vec![
            Issue::minimal("id-1", "ABC-7", "Add fizz to buzz", "todo"),
            Issue::minimal("id-2", "ABC-9", "Wire dispatcher", "in progress"),
        ]);
        let outcome = snapshot_with_tracker(Box::new(loaded), Arc::new(tracker)).await;
        match outcome {
            StatusOutcome::Ok(snap) => match &snap.view {
                StatusView::TrackerPreview { active } => {
                    assert_eq!(active.len(), 2);
                    assert_eq!(active[0].identifier, "ABC-7");
                }
                other => panic!("expected TrackerPreview, got {other:?}"),
            },
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tracker_preview_propagates_tracker_errors() {
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

    /// Durable mode: when `--state-db` is supplied, `status` reads
    /// every non-terminal work item from the durable store and joins
    /// the latest handoff per row. The tracker is *not* consulted —
    /// SPEC v2 §4.7 says the durable store is the source of truth.
    #[tokio::test]
    async fn durable_mode_lists_persisted_work_items_with_latest_handoff() {
        use symphony_state::handoffs::{HandoffRepository, NewHandoff};
        use symphony_state::migrations::migrations;
        use symphony_state::repository::{NewRun, NewWorkItem, RunRepository, WorkItemRepository};

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("state.db");
        {
            let mut db = StateDb::open(&db_path).unwrap();
            db.migrate(migrations()).unwrap();
            // Active row with two handoffs (latest wins).
            let wi = db
                .create_work_item(NewWorkItem {
                    tracker_id: "linear",
                    identifier: "ABC-7",
                    parent_id: None,
                    title: "Add fizz to buzz",
                    status_class: "ready",
                    tracker_status: "todo",
                    assigned_role: Some("platform_lead"),
                    assigned_agent: None,
                    priority: None,
                    workspace_policy: None,
                    branch_policy: None,
                    now: "2026-05-08T00:00:00Z",
                })
                .unwrap();
            let run = db
                .create_run(NewRun {
                    work_item_id: wi.id,
                    role: "platform_lead",
                    agent: "claude",
                    status: "running",
                    workspace_claim_id: None,
                    now: "2026-05-08T00:00:00Z",
                })
                .unwrap();
            db.create_handoff(NewHandoff {
                run_id: run.id,
                work_item_id: wi.id,
                ready_for: "integration",
                summary: "first pass",
                changed_files: None,
                tests_run: None,
                evidence: None,
                known_risks: None,
                now: "2026-05-08T00:00:01Z",
            })
            .unwrap();
            db.create_handoff(NewHandoff {
                run_id: run.id,
                work_item_id: wi.id,
                ready_for: "qa",
                summary: "ready for QA gate",
                changed_files: None,
                tests_run: None,
                evidence: None,
                known_risks: None,
                now: "2026-05-08T00:00:02Z",
            })
            .unwrap();
            // Active row with no handoff.
            db.create_work_item(NewWorkItem {
                tracker_id: "linear",
                identifier: "ABC-9",
                parent_id: None,
                title: "Untouched",
                status_class: "intake",
                tracker_status: "todo",
                assigned_role: None,
                assigned_agent: None,
                priority: None,
                workspace_policy: None,
                branch_policy: None,
                now: "2026-05-08T00:00:03Z",
            })
            .unwrap();
            // Terminal row that must be filtered out.
            db.create_work_item(NewWorkItem {
                tracker_id: "linear",
                identifier: "ABC-1",
                parent_id: None,
                title: "Already shipped",
                status_class: "done",
                tracker_status: "done",
                assigned_role: None,
                assigned_agent: None,
                priority: None,
                workspace_policy: None,
                branch_policy: None,
                now: "2026-05-08T00:00:04Z",
            })
            .unwrap();
        }

        let (_dir, path) = ok_workflow();
        let outcome = run(&path, Some(&db_path)).await;
        match outcome {
            StatusOutcome::Ok(snap) => match snap.view {
                StatusView::Durable { items } => {
                    let identifiers: Vec<&str> = items
                        .iter()
                        .map(|r| r.work_item.identifier.as_str())
                        .collect();
                    // Terminal `done` row excluded; ordering is
                    // updated_at DESC so ABC-9 (later insert) precedes
                    // ABC-7.
                    assert_eq!(identifiers, vec!["ABC-9", "ABC-7"]);
                    let abc7 = items
                        .iter()
                        .find(|r| r.work_item.identifier == "ABC-7")
                        .unwrap();
                    let h = abc7.latest_handoff.as_ref().expect("ABC-7 handoff");
                    assert_eq!(h.ready_for, "qa");
                    assert_eq!(h.summary, "ready for QA gate");
                    let abc9 = items
                        .iter()
                        .find(|r| r.work_item.identifier == "ABC-9")
                        .unwrap();
                    assert!(abc9.latest_handoff.is_none());
                }
                other => panic!("expected Durable, got {other:?}"),
            },
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    /// Durable mode does not consult the tracker — even an obviously
    /// broken tracker config (e.g. a Linear workflow without a token)
    /// must still produce a snapshot, because the durable store is the
    /// source of truth and a tracker outage cannot mask state.
    #[tokio::test]
    async fn durable_mode_does_not_consult_tracker() {
        use symphony_state::migrations::migrations;
        use symphony_state::repository::{NewWorkItem, WorkItemRepository};

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("state.db");
        {
            let mut db = StateDb::open(&db_path).unwrap();
            db.migrate(migrations()).unwrap();
            db.create_work_item(NewWorkItem {
                tracker_id: "linear",
                identifier: "ABC-7",
                parent_id: None,
                title: "From DB",
                status_class: "ready",
                tracker_status: "todo",
                assigned_role: None,
                assigned_agent: None,
                priority: None,
                workspace_policy: None,
                branch_policy: None,
                now: "2026-05-08T00:00:00Z",
            })
            .unwrap();
        }

        // The workflow file is the same valid Linear config used
        // elsewhere; `build_tracker` would normally try to connect, but
        // durable mode bypasses it entirely. We pin that by checking
        // the snapshot succeeds without a `LINEAR_API_TOKEN` in env.
        // SAFETY: tests share the process env; we read-then-restore
        // around the assertion to avoid bleeding into other tests.
        let prev = std::env::var("LINEAR_API_TOKEN").ok();
        // SAFETY: single-threaded tokio current_thread test; no other
        // test reads LINEAR_API_TOKEN concurrently within this binary.
        unsafe {
            std::env::remove_var("LINEAR_API_TOKEN");
        }
        let (_dir, path) = ok_workflow();
        let outcome = run(&path, Some(&db_path)).await;
        if let Some(v) = prev {
            // SAFETY: see above; restore the prior value.
            unsafe {
                std::env::set_var("LINEAR_API_TOKEN", v);
            }
        }
        match outcome {
            StatusOutcome::Ok(snap) => match snap.view {
                StatusView::Durable { items } => {
                    assert_eq!(items.len(), 1);
                    assert_eq!(items[0].work_item.identifier, "ABC-7");
                }
                other => panic!("expected Durable, got {other:?}"),
            },
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    /// A `--state-db` pointing at an unreadable path is operator
    /// configuration we want to fail loudly: surface as a distinct
    /// outcome (and exit code 4) so an operator never confuses an
    /// empty handoff column with a misconfigured flag.
    #[tokio::test]
    async fn durable_mode_returns_state_db_failed_when_db_unreadable() {
        let unreachable = std::path::PathBuf::from("/no/such/dir/state.db");
        let (_wf_dir, path) = ok_workflow();
        let outcome = run(&path, Some(&unreachable)).await;
        match &outcome {
            StatusOutcome::StateDbFailed(_, _) => {}
            other => panic!("expected StateDbFailed, got {other:?}"),
        }
        assert_eq!(outcome.exit_code(), 4);
    }

    #[test]
    fn tracker_id_for_matches_orchestrator_inserts() {
        assert_eq!(tracker_id_for(TrackerKind::Linear), "linear");
        assert_eq!(tracker_id_for(TrackerKind::Github), "github");
        assert_eq!(tracker_id_for(TrackerKind::Mock), "mock");
    }

    #[test]
    fn truncate_leaves_short_strings_alone() {
        assert_eq!(truncate("hi", 8), "hi");
    }

    #[test]
    fn truncate_appends_ellipsis_when_too_long() {
        let out = truncate("abcdefgh", 5);
        assert_eq!(out.chars().count(), 5);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn truncate_handles_max_zero_without_panicking() {
        let out = truncate("abcdefgh", 0);
        assert!(out.chars().count() <= 1);
    }

    /// Tracker-preview JSON: pin the schema's top-level shape so
    /// downstream tooling can rely on `mode`, `tracker`, `config`, and
    /// `active` without reading code.
    #[tokio::test]
    async fn json_tracker_preview_schema_is_stable() {
        let (_dir, path) = ok_workflow();
        let loaded = LayeredLoader::from_path(&path).unwrap();
        let tracker = MockTracker::with_active(vec![Issue::minimal(
            "id-1",
            "ABC-7",
            "Add fizz to buzz",
            "todo",
        )]);
        let outcome = snapshot_with_tracker(Box::new(loaded), Arc::new(tracker)).await;
        let snap = match &outcome {
            StatusOutcome::Ok(s) => s,
            other => panic!("expected Ok, got {other:?}"),
        };
        let doc = StatusJson::from_snapshot(snap);
        assert_eq!(doc.mode, "tracker_preview");
        assert_eq!(doc.tracker.kind, "linear");
        assert_eq!(doc.tracker.label, "ENG");
        assert!(doc.items.is_empty());
        assert_eq!(doc.active.len(), 1);
        let row = &doc.active[0];
        assert_eq!(row.id, "id-1");
        assert_eq!(row.identifier, "ABC-7");
        assert_eq!(row.title, "Add fizz to buzz");
        assert_eq!(row.state, "todo");
        assert!(row.priority.is_none());
        assert!(row.branch_name.is_none());
        assert!(row.blocked_by.is_empty());

        // serialize and re-parse to lock the on-wire field names.
        let s = serde_json::to_string(&doc).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["mode"], "tracker_preview");
        assert_eq!(v["tracker"]["kind"], "linear");
        assert_eq!(v["tracker"]["label"], "ENG");
        assert_eq!(v["active"][0]["identifier"], "ABC-7");
        // durable bucket omitted via skip_serializing_if when empty.
        assert!(v.get("items").is_none());
    }

    /// Durable JSON: latest handoff projects to the documented shape
    /// and parent_id round-trips when present.
    #[tokio::test]
    async fn json_durable_schema_is_stable() {
        use symphony_state::handoffs::{HandoffRepository, NewHandoff};
        use symphony_state::migrations::migrations;
        use symphony_state::repository::{NewRun, NewWorkItem, RunRepository, WorkItemRepository};

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("state.db");
        {
            let mut db = StateDb::open(&db_path).unwrap();
            db.migrate(migrations()).unwrap();
            let wi = db
                .create_work_item(NewWorkItem {
                    tracker_id: "linear",
                    identifier: "ABC-7",
                    parent_id: None,
                    title: "Add fizz to buzz",
                    status_class: "ready",
                    tracker_status: "todo",
                    assigned_role: Some("platform_lead"),
                    assigned_agent: None,
                    priority: None,
                    workspace_policy: None,
                    branch_policy: None,
                    now: "2026-05-08T00:00:00Z",
                })
                .unwrap();
            let run = db
                .create_run(NewRun {
                    work_item_id: wi.id,
                    role: "platform_lead",
                    agent: "claude",
                    status: "running",
                    workspace_claim_id: None,
                    now: "2026-05-08T00:00:00Z",
                })
                .unwrap();
            db.create_handoff(NewHandoff {
                run_id: run.id,
                work_item_id: wi.id,
                ready_for: "qa",
                summary: "ready for QA",
                changed_files: None,
                tests_run: None,
                evidence: None,
                known_risks: None,
                now: "2026-05-08T00:00:01Z",
            })
            .unwrap();
        }

        let (_dir, path) = ok_workflow();
        let outcome = run(&path, Some(&db_path)).await;
        let snap = match &outcome {
            StatusOutcome::Ok(s) => s,
            other => panic!("expected Ok, got {other:?}"),
        };
        let doc = StatusJson::from_snapshot(snap);
        assert_eq!(doc.mode, "durable");
        assert_eq!(doc.items.len(), 1);
        let row = &doc.items[0];
        assert_eq!(row.identifier, "ABC-7");
        assert_eq!(row.tracker_id, "linear");
        assert_eq!(row.status_class, "ready");
        assert_eq!(row.assigned_role.as_deref(), Some("platform_lead"));
        let h = row.latest_handoff.as_ref().expect("handoff present");
        assert_eq!(h.ready_for, "qa");
        assert_eq!(h.summary, "ready for QA");
        assert!(doc.active.is_empty());

        let v: serde_json::Value = serde_json::to_value(&doc).unwrap();
        assert_eq!(v["mode"], "durable");
        assert_eq!(v["items"][0]["latest_handoff"]["ready_for"], "qa");
        assert!(v.get("active").is_none());
    }

    /// `render_json` returns the same exit codes as `render` so a CI
    /// script can switch on `--json` without re-mapping outcomes.
    #[tokio::test]
    async fn render_json_exit_codes_match_render() {
        // Ok path.
        let (_dir, path) = ok_workflow();
        let loaded = LayeredLoader::from_path(&path).unwrap();
        let tracker =
            MockTracker::with_active(vec![Issue::minimal("id-1", "ABC-7", "Title", "todo")]);
        let outcome = snapshot_with_tracker(Box::new(loaded), Arc::new(tracker)).await;
        assert_eq!(render_json(&outcome), 0);

        // LoadFailed path.
        let outcome = run(Path::new("/no/such/path/WORKFLOW.md"), None).await;
        assert_eq!(render_json(&outcome), 1);

        // TrackerFailed path.
        let (_dir, path) = ok_workflow();
        let loaded = LayeredLoader::from_path(&path).unwrap();
        let tracker = MockTracker::new();
        tracker.enqueue_active_error(TrackerError::Other("boom".into()));
        let outcome = snapshot_with_tracker(Box::new(loaded), Arc::new(tracker)).await;
        assert_eq!(render_json(&outcome), 3);
    }
}
