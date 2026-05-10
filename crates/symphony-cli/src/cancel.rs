//! Implementation of the `symphony cancel` subcommand (SPEC v2 §4.5 /
//! Phase 11.5).
//!
//! The command's job is small but operationally load-bearing: persist a
//! cooperative cancel request and, for work-item subjects, cascade it to
//! every in-flight descendant run so dispatch runners observe the
//! cancel at their next safe point even if the operator only knew about
//! the parent.
//!
//! Architecturally this is a thin shell over three pieces that already
//! exist:
//!
//! 1. [`symphony_state::cancel_requests::CancelRequestRepository`] — the
//!    durable mirror of the in-memory cancel queue. Idempotency is
//!    enforced here by the partial unique index on
//!    `(subject_kind, subject_id) WHERE state = 'pending'`, so re-runs
//!    of `symphony cancel` against the same subject collapse to an
//!    in-place payload replace.
//! 2. [`symphony_core::cancellation_propagator::CancellationPropagator`]
//!    — the pure cascade. Given a parent work-item cancel, it walks
//!    `parent_child` descendants and the in-flight run set on each, and
//!    returns the per-run requests the composition root should enqueue.
//! 3. The state-side `WorkItemDescendantSource` and `ActiveRunSource`
//!    adapters defined in this module that bridge the kernel's pure
//!    cascade to live SQLite reads via
//!    [`symphony_state::edges::WorkItemEdgeRepository::list_descendants_via_parent_child`]
//!    and [`symphony_state::repository::RunRepository::list_runs_for_work_item`].
//!
//! ## Exit codes
//!
//! - `0` — request enqueued (or replaced an existing pending entry).
//! - `1` — durable state could not be opened or queried (missing path,
//!   permissions, schema mismatch).
//! - `2` — propagation failed (work-item descendant query / active-run
//!   query reported an error). The parent request is still durably
//!   enqueued; only the cascade fan-out aborted.
//!
//! ## Why no tracker round-trip
//!
//! `symphony cancel` deliberately operates on durable ids only. Tracker
//! identifier resolution (e.g. `ENG-101` → `work_items.id`) requires a
//! `WORKFLOW.md` load and a tracker connection — both heavy for a
//! command whose sole purpose is to flip a row. The status / TUI
//! commands in Phase 12 surface the durable id alongside each issue, so
//! an operator who knows the tracker key can copy the matching numeric
//! id without leaving the terminal.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use symphony_core::blocker::RunRef;
use symphony_core::cancellation::{CancelRequest, CancelSubject};
use symphony_core::cancellation_propagator::{
    ActiveRunSource, CancellationPropagator, PropagationError, WorkItemDescendantSource,
};
use symphony_core::run_status::RunStatus;
use symphony_core::work_item::WorkItemId as CoreWorkItemId;
use symphony_state::cancel_requests::CancelRequestRepository;
use symphony_state::edges::WorkItemEdgeRepository;
use symphony_state::repository::{RunRepository, WorkItemId as StateWorkItemId};
use symphony_state::{StateDb, StateError};

use crate::cli::CancelArgs;

/// Outcome of one `symphony cancel` invocation. Splitting the structured
/// result from rendering keeps the unit tests honest — they assert on
/// the typed value rather than parsing stdout.
#[derive(Debug)]
pub enum CancelOutcome {
    /// The parent request was enqueued (or replaced an existing pending
    /// row) and, for work-item subjects, the cascade fan-out completed.
    Ok(CancelReport),
    /// The state DB could not be opened or queried.
    StateDbFailed(PathBuf, StateError),
    /// The parent request was persisted but the propagator failed to
    /// walk the descendant graph or query in-flight runs. The audit
    /// trail records the durably-persisted parent so operators can
    /// retry the cascade after fixing whatever broke.
    PropagationFailed(CancelReport, PropagationError),
}

impl CancelOutcome {
    /// Stable exit code; see module docs.
    pub fn exit_code(&self) -> i32 {
        match self {
            CancelOutcome::Ok(_) => 0,
            CancelOutcome::StateDbFailed(_, _) => 1,
            CancelOutcome::PropagationFailed(_, _) => 2,
        }
    }
}

/// Structured success summary. The renderer formats this into a single
/// human-readable line; tests assert on the fields directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelReport {
    /// The subject whose cancel was enqueued.
    pub subject: CancelSubject,
    /// `true` when this enqueue produced a fresh pending row; `false`
    /// when an existing pending row was replaced (idempotent re-run).
    pub parent_inserted: bool,
    /// The reason persisted on the parent row. Round-trips through the
    /// repository so the renderer prints exactly what was stored.
    pub reason: String,
    /// Identity recorded on the parent row.
    pub requested_by: String,
    /// RFC3339 timestamp recorded on the parent row.
    pub requested_at: String,
    /// Per-run cascade fan-out, populated only for work-item subjects.
    /// Empty for run-keyed subjects (those don't propagate).
    pub cascaded: Vec<CascadedRun>,
    /// Descendant work-item ids walked by the cascade, including the
    /// root. Empty for run-keyed subjects.
    pub descendants: Vec<CoreWorkItemId>,
}

impl CancelReport {
    /// Number of per-run cancels that produced a fresh pending row.
    pub fn cascaded_inserted(&self) -> usize {
        self.cascaded.iter().filter(|c| c.inserted).count()
    }

    /// Number of per-run cancels that replaced an existing pending row.
    pub fn cascaded_replaced(&self) -> usize {
        self.cascaded.iter().filter(|c| !c.inserted).count()
    }
}

/// One per-run cascade target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CascadedRun {
    /// Run id that received the cascaded cancel.
    pub run_id: RunRef,
    /// `true` if this enqueue inserted a new pending row, `false` if it
    /// replaced an existing pending one.
    pub inserted: bool,
}

/// Run the command end-to-end. Splitting from `render` keeps the unit
/// tests free of stdout coupling.
pub fn run(args: &CancelArgs) -> CancelOutcome {
    let mut db = match StateDb::open(&args.state_db) {
        Ok(db) => db,
        Err(err) => return CancelOutcome::StateDbFailed(args.state_db.clone(), err),
    };
    run_with_db(&mut db, args)
}

/// Variant of [`run`] that takes an already-open [`StateDb`]. Used by
/// integration tests that need to seed the database before invoking
/// the command.
pub fn run_with_db(db: &mut StateDb, args: &CancelArgs) -> CancelOutcome {
    let subject = match (args.run, args.issue) {
        (Some(run_id), None) => CancelSubject::run(RunRef::new(run_id)),
        (None, Some(work_item_id)) => CancelSubject::work_item(CoreWorkItemId::new(work_item_id)),
        // Clap's `ArgGroup(required = true)` rules out both arms; the
        // patterns are still here so `match` stays exhaustive.
        (Some(_), Some(_)) | (None, None) => {
            unreachable!("clap ArgGroup must enforce exactly one of --run / --issue");
        }
    };

    let requested_by = args
        .requested_by
        .clone()
        .unwrap_or_else(|| std::env::var("USER").unwrap_or_else(|_| "unknown".into()));
    let requested_at = args
        .at
        .clone()
        .unwrap_or_else(|| current_rfc3339_seconds(SystemTime::now()));

    let parent = CancelRequest {
        subject,
        reason: args.reason.clone(),
        requested_by,
        requested_at,
    };

    let parent_inserted = match db.enqueue_cancel_request(&parent, &parent.requested_at) {
        Ok(inserted) => inserted,
        Err(err) => return CancelOutcome::StateDbFailed(args.state_db.clone(), err),
    };

    let mut report = CancelReport {
        subject: parent.subject,
        parent_inserted,
        reason: parent.reason.clone(),
        requested_by: parent.requested_by.clone(),
        requested_at: parent.requested_at.clone(),
        cascaded: Vec::new(),
        descendants: Vec::new(),
    };

    if let CancelSubject::WorkItem { .. } = parent.subject {
        let sources = StateBackedSources { db };
        let outcome = match CancellationPropagator::cascade(&parent, &sources, &sources) {
            Ok(o) => o,
            Err(err) => return CancelOutcome::PropagationFailed(report, err),
        };
        report.descendants = outcome.descendants;
        for child in outcome.run_requests {
            let inserted = match db.enqueue_cancel_request(&child, &child.requested_at) {
                Ok(b) => b,
                Err(err) => return CancelOutcome::StateDbFailed(args.state_db.clone(), err),
            };
            let run_id = match child.subject {
                CancelSubject::Run { run_id } => run_id,
                CancelSubject::WorkItem { .. } => {
                    // Propagator output is documented to be run-keyed.
                    unreachable!("cascade output must be run-keyed");
                }
            };
            report.cascaded.push(CascadedRun { run_id, inserted });
        }
    }

    CancelOutcome::Ok(report)
}

/// Render `outcome` to stdout/stderr; returns the exit code.
///
/// Success format is one line so a watcher pipeline can `grep` it
/// without parsing. The line carries: subject, idempotency, identity,
/// reason, and cascade fan-out summary.
pub fn render(outcome: &CancelOutcome) -> i32 {
    match outcome {
        CancelOutcome::Ok(report) => {
            println!("{}", format_summary(report));
        }
        CancelOutcome::StateDbFailed(path, err) => {
            eprintln!(
                "error: state DB at {} could not be opened or queried: {err}",
                path.display()
            );
        }
        CancelOutcome::PropagationFailed(report, err) => {
            // Print the parent-line so the operator sees the durable
            // request that was enqueued, then surface the cascade
            // failure on stderr. The parent cancel still applies; only
            // the fan-out aborted.
            println!("{}", format_summary(report));
            eprintln!("error: cancel cascade failed: {err}");
        }
    }
    outcome.exit_code()
}

fn format_summary(report: &CancelReport) -> String {
    let subject_label = match report.subject {
        CancelSubject::Run { run_id } => format!("run #{}", run_id.get()),
        CancelSubject::WorkItem { work_item_id } => format!("work_item #{}", work_item_id.get()),
    };
    let parent_word = if report.parent_inserted {
        "enqueued"
    } else {
        "nudged"
    };
    match report.subject {
        CancelSubject::Run { .. } => format!(
            "cancel {parent_word}: {subject_label} (by {by}, reason: {reason})",
            by = report.requested_by,
            reason = report.reason,
        ),
        CancelSubject::WorkItem { .. } => format!(
            "cancel {parent_word}: {subject_label} \
             (by {by}, reason: {reason}; cascaded {new} new + {dup} replaced \
             across {descendants} work item{plural})",
            by = report.requested_by,
            reason = report.reason,
            new = report.cascaded_inserted(),
            dup = report.cascaded_replaced(),
            descendants = report.descendants.len(),
            plural = if report.descendants.len() == 1 {
                ""
            } else {
                "s"
            },
        ),
    }
}

/// Format `at` as `YYYY-MM-DDTHH:MM:SSZ` (UTC, second precision).
///
/// The kernel and durable layer treat the timestamp as opaque text, so
/// any monotone-comparable format would suffice; we pick canonical
/// RFC3339-with-`Z` for human legibility. The civil-date arithmetic is
/// inlined (rather than pulling in `time`/`chrono`) to keep the
/// dependency surface small — see [`days_to_civil`].
pub fn current_rfc3339_seconds(at: SystemTime) -> String {
    let dur = at.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = dur.as_secs() as i64;
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let (year, month, day) = days_to_civil(days);
    let hour = secs_of_day / 3_600;
    let min = (secs_of_day % 3_600) / 60;
    let sec = secs_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}Z")
}

/// Convert "days since 1970-01-01" to a `(year, month, day)` tuple
/// using Howard Hinnant's `days_from_civil` inverse. Valid for the
/// proleptic Gregorian calendar across the full `i64` range, which is
/// far more than the operator timestamps need.
fn days_to_civil(days: i64) -> (i64, u32, u32) {
    // See https://howardhinnant.github.io/date_algorithms.html#civil_from_days
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// State-side glue exposing
/// [`symphony_state::edges::WorkItemEdgeRepository`] and
/// [`symphony_state::repository::RunRepository`] as the kernel's
/// [`WorkItemDescendantSource`] / [`ActiveRunSource`].
struct StateBackedSources<'a> {
    db: &'a StateDb,
}

impl WorkItemDescendantSource for StateBackedSources<'_> {
    fn descendants_via_parent_child(
        &self,
        root: CoreWorkItemId,
    ) -> Result<Vec<CoreWorkItemId>, PropagationError> {
        let raw = self
            .db
            .list_descendants_via_parent_child(StateWorkItemId(root.0))
            .map_err(|e| PropagationError::new(e.to_string()))?;
        Ok(raw.into_iter().map(|w| CoreWorkItemId::new(w.0)).collect())
    }
}

impl ActiveRunSource for StateBackedSources<'_> {
    fn active_runs_for_work_item(
        &self,
        work_item_id: CoreWorkItemId,
    ) -> Result<Vec<RunRef>, PropagationError> {
        let runs = self
            .db
            .list_runs_for_work_item(StateWorkItemId(work_item_id.0))
            .map_err(|e| PropagationError::new(e.to_string()))?;
        let mut out = Vec::new();
        for run in runs {
            let parsed = RunStatus::from_label(&run.status);
            // Treat unknown labels as in-flight: a tampered/legacy row
            // is exactly the kind of state we want a cancel to nudge,
            // not silently skip.
            let in_flight = match parsed {
                Some(s) => !matches!(
                    s,
                    RunStatus::Completed | RunStatus::Failed | RunStatus::Cancelled
                ),
                None => true,
            };
            if in_flight {
                out.push(RunRef::new(run.id.0));
            }
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use symphony_state::cancel_requests::CancelRequestState;
    use symphony_state::edges::{EdgeType, NewWorkItemEdge, WorkItemEdgeRepository};
    use symphony_state::migrations::migrations;
    use symphony_state::repository::{NewRun, NewWorkItem, WorkItemRepository};

    fn open_db() -> StateDb {
        let mut db = StateDb::open_in_memory().expect("open");
        db.migrate(migrations()).expect("migrate");
        db
    }

    fn seed_item(db: &mut StateDb, identifier: &str) -> StateWorkItemId {
        db.create_work_item(NewWorkItem {
            tracker_id: "github",
            identifier,
            parent_id: None,
            title: "demo",
            status_class: "ready",
            tracker_status: "open",
            assigned_role: None,
            assigned_agent: None,
            priority: None,
            workspace_policy: None,
            branch_policy: None,
            now: "2026-05-08T00:00:00Z",
        })
        .expect("seed work item")
        .id
    }

    fn seed_run(db: &mut StateDb, work_item: StateWorkItemId, status: &str) -> i64 {
        db.create_run(NewRun {
            work_item_id: work_item,
            role: "specialist",
            agent: "claude",
            status,
            workspace_claim_id: None,
            now: "2026-05-08T00:00:00Z",
        })
        .expect("seed run")
        .id
        .0
    }

    fn args(state_db: PathBuf) -> CancelArgs {
        CancelArgs {
            run: None,
            issue: None,
            reason: "operator cancel".into(),
            requested_by: Some("alice".into()),
            state_db,
            at: Some("2026-05-09T12:00:00Z".into()),
        }
    }

    #[test]
    fn run_subject_persists_a_pending_row_and_reports_inserted() {
        let mut db = open_db();
        let wi = seed_item(&mut db, "ENG-1");
        let run_id = seed_run(&mut db, wi, "running");

        let mut a = args(PathBuf::from(":memory:")); // ignored by run_with_db
        a.run = Some(run_id);
        let outcome = run_with_db(&mut db, &a);
        let report = match outcome {
            CancelOutcome::Ok(r) => r,
            other => panic!("expected Ok, got {other:?}"),
        };
        assert!(report.parent_inserted);
        assert_eq!(report.reason, "operator cancel");
        assert_eq!(report.requested_by, "alice");
        assert!(report.cascaded.is_empty());
        assert!(report.descendants.is_empty());

        let pending = db.list_pending_cancel_requests().expect("list");
        assert_eq!(pending.len(), 1);
        assert!(matches!(
            pending[0].request.subject,
            CancelSubject::Run { .. }
        ));
        assert_eq!(pending[0].state, CancelRequestState::Pending);
    }

    #[test]
    fn re_running_against_same_run_subject_is_idempotent_and_reports_replaced() {
        let mut db = open_db();
        let wi = seed_item(&mut db, "ENG-1");
        let run_id = seed_run(&mut db, wi, "running");

        let mut a = args(PathBuf::from(":memory:"));
        a.run = Some(run_id);
        let _ = run_with_db(&mut db, &a);
        let outcome = run_with_db(&mut db, &a);
        let report = match outcome {
            CancelOutcome::Ok(r) => r,
            other => panic!("expected Ok, got {other:?}"),
        };
        assert!(!report.parent_inserted, "re-run must report nudged");

        let pending = db.list_pending_cancel_requests().expect("list");
        assert_eq!(
            pending.len(),
            1,
            "partial unique index must collapse re-runs"
        );
    }

    #[test]
    fn work_item_subject_cascades_to_active_descendant_runs() {
        // Tree:
        //   parent (id1) — running run
        //   ├── child_a (id2) — running run
        //   │   └── grand (id4) — running run
        //   └── child_b (id3) — completed run (must NOT be cascaded)
        let mut db = open_db();
        let parent = seed_item(&mut db, "ENG-1");
        let child_a = seed_item(&mut db, "ENG-2");
        let child_b = seed_item(&mut db, "ENG-3");
        let grand = seed_item(&mut db, "ENG-4");
        let now = "2026-05-08T00:00:00Z";
        db.create_edge(NewWorkItemEdge {
            parent_id: parent,
            child_id: child_a,
            edge_type: EdgeType::ParentChild,
            reason: None,
            status: "linked",
            now,
        })
        .unwrap();
        db.create_edge(NewWorkItemEdge {
            parent_id: parent,
            child_id: child_b,
            edge_type: EdgeType::ParentChild,
            reason: None,
            status: "linked",
            now,
        })
        .unwrap();
        db.create_edge(NewWorkItemEdge {
            parent_id: child_a,
            child_id: grand,
            edge_type: EdgeType::ParentChild,
            reason: None,
            status: "linked",
            now,
        })
        .unwrap();

        let r_parent = seed_run(&mut db, parent, "running");
        let r_child_a = seed_run(&mut db, child_a, "running");
        let r_grand = seed_run(&mut db, grand, "running");
        let r_child_b = seed_run(&mut db, child_b, "completed");

        let mut a = args(PathBuf::from(":memory:"));
        a.issue = Some(parent.0);
        let outcome = run_with_db(&mut db, &a);
        let report = match outcome {
            CancelOutcome::Ok(r) => r,
            other => panic!("expected Ok, got {other:?}"),
        };

        assert!(report.parent_inserted);
        let cascaded_ids: Vec<i64> = report.cascaded.iter().map(|c| c.run_id.get()).collect();
        // Must include all running descendants but NOT the completed one.
        assert!(cascaded_ids.contains(&r_parent));
        assert!(cascaded_ids.contains(&r_child_a));
        assert!(cascaded_ids.contains(&r_grand));
        assert!(!cascaded_ids.contains(&r_child_b));
        assert_eq!(report.descendants.len(), 4);
        assert!(report.cascaded.iter().all(|c| c.inserted));

        // Durable mirror: parent + 3 cascaded run rows = 4 pending.
        let pending = db.list_pending_cancel_requests().expect("list");
        assert_eq!(pending.len(), 4);
    }

    #[test]
    fn cascaded_child_requests_carry_cascade_marker_in_requested_by() {
        let mut db = open_db();
        let parent = seed_item(&mut db, "ENG-1");
        seed_run(&mut db, parent, "running");

        let mut a = args(PathBuf::from(":memory:"));
        a.issue = Some(parent.0);
        let _ = run_with_db(&mut db, &a);

        let pending = db.list_pending_cancel_requests().expect("list");
        // One parent (work-item subject, by=alice) + one cascaded
        // child (run subject, by=cascade:work_item:<parent.0>).
        let parent_row = pending
            .iter()
            .find(|p| matches!(p.request.subject, CancelSubject::WorkItem { .. }))
            .expect("parent");
        let child_row = pending
            .iter()
            .find(|p| matches!(p.request.subject, CancelSubject::Run { .. }))
            .expect("cascaded child");
        assert_eq!(parent_row.request.requested_by, "alice");
        assert_eq!(
            child_row.request.requested_by,
            format!("cascade:work_item:{}", parent.0)
        );
        assert_eq!(child_row.request.reason, "operator cancel");
        assert_eq!(child_row.request.requested_at, "2026-05-09T12:00:00Z");
    }

    #[test]
    fn re_running_work_item_cancel_is_idempotent_across_cascade() {
        let mut db = open_db();
        let parent = seed_item(&mut db, "ENG-1");
        seed_run(&mut db, parent, "running");

        let mut a = args(PathBuf::from(":memory:"));
        a.issue = Some(parent.0);
        let _ = run_with_db(&mut db, &a);
        let outcome = run_with_db(&mut db, &a);
        let report = match outcome {
            CancelOutcome::Ok(r) => r,
            other => panic!("expected Ok, got {other:?}"),
        };
        assert!(!report.parent_inserted);
        assert_eq!(report.cascaded.len(), 1);
        assert!(
            !report.cascaded[0].inserted,
            "second pass must report cascaded child as replaced",
        );

        let pending = db.list_pending_cancel_requests().expect("list");
        assert_eq!(pending.len(), 2, "no duplicate rows after re-run");
    }

    #[test]
    fn work_item_with_no_active_runs_still_records_the_parent_request() {
        let mut db = open_db();
        let parent = seed_item(&mut db, "ENG-1");
        // No runs seeded.

        let mut a = args(PathBuf::from(":memory:"));
        a.issue = Some(parent.0);
        let outcome = run_with_db(&mut db, &a);
        let report = match outcome {
            CancelOutcome::Ok(r) => r,
            other => panic!("expected Ok, got {other:?}"),
        };
        assert!(report.parent_inserted);
        assert!(report.cascaded.is_empty());
        assert_eq!(report.descendants, vec![CoreWorkItemId::new(parent.0)]);
    }

    #[test]
    fn render_emits_one_line_summary_for_run_subject() {
        let report = CancelReport {
            subject: CancelSubject::run(RunRef::new(7)),
            parent_inserted: true,
            reason: "abandon".into(),
            requested_by: "alice".into(),
            requested_at: "2026-05-09T12:00:00Z".into(),
            cascaded: vec![],
            descendants: vec![],
        };
        let line = format_summary(&report);
        assert!(line.starts_with("cancel enqueued: run #7 ("));
        assert!(line.contains("by alice"));
        assert!(line.contains("reason: abandon"));
        // Single line.
        assert!(!line.contains('\n'));
    }

    #[test]
    fn render_emits_one_line_summary_for_work_item_subject_with_cascade_counts() {
        let report = CancelReport {
            subject: CancelSubject::work_item(CoreWorkItemId::new(11)),
            parent_inserted: false,
            reason: "scope change".into(),
            requested_by: "bob".into(),
            requested_at: "2026-05-09T12:00:00Z".into(),
            cascaded: vec![
                CascadedRun {
                    run_id: RunRef::new(100),
                    inserted: true,
                },
                CascadedRun {
                    run_id: RunRef::new(101),
                    inserted: false,
                },
            ],
            descendants: vec![CoreWorkItemId::new(11), CoreWorkItemId::new(12)],
        };
        let line = format_summary(&report);
        assert!(line.starts_with("cancel nudged: work_item #11"));
        assert!(line.contains("cascaded 1 new + 1 replaced across 2 work items"));
        assert!(!line.contains('\n'));
    }

    #[test]
    fn current_rfc3339_seconds_round_trips_canonical_unix_epoch() {
        let s = current_rfc3339_seconds(UNIX_EPOCH);
        assert_eq!(s, "1970-01-01T00:00:00Z");
    }

    #[test]
    fn current_rfc3339_seconds_handles_a_known_civil_date() {
        // 2024-02-29T12:34:56Z (a leap-day past noon) — epoch seconds
        // 1_709_210_096. Picked to exercise civil-date math through a
        // leap-year edge case without depending on a real date crate.
        let at = UNIX_EPOCH + std::time::Duration::from_secs(1_709_210_096);
        let s = current_rfc3339_seconds(at);
        assert_eq!(s, "2024-02-29T12:34:56Z");
    }

    #[test]
    fn outcome_exit_codes_are_stable() {
        let report = CancelReport {
            subject: CancelSubject::run(RunRef::new(1)),
            parent_inserted: true,
            reason: "x".into(),
            requested_by: "y".into(),
            requested_at: "2026-05-09T00:00:00Z".into(),
            cascaded: vec![],
            descendants: vec![],
        };
        assert_eq!(CancelOutcome::Ok(report.clone()).exit_code(), 0);
        let err = StateError::OutOfOrderMigration {
            found: 1,
            previous: 5,
        };
        assert_eq!(
            CancelOutcome::StateDbFailed(PathBuf::from("/tmp/x"), err).exit_code(),
            1
        );
        assert_eq!(
            CancelOutcome::PropagationFailed(report, PropagationError::new("nope")).exit_code(),
            2
        );
    }
}
