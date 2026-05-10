//! Implementation of the `symphony qa` command group (SPEC v2 §4.9 /
//! §5.12 / Phase 12).
//!
//! Today this exposes a single subcommand:
//!
//! * `symphony qa verdict <ID>` — print every QA verdict the gate has
//!   recorded against the given durable work-item id, newest first.
//!
//! The command is read-only: it opens the durable SQLite database with
//! [`StateDb::open`] and never invokes the migration runner. A schema
//! mismatch surfaces as a [`StateError`] rather than a silent upgrade —
//! mutating a database an operator pointed us at by mistake is the same
//! footgun [`crate::cancel`] and [`crate::issue`] are careful to avoid.
//!
//! ## Why work-item id, not run id
//!
//! The QA loop can dispatch a work item more than once (rework after a
//! `failed_with_blockers` verdict, re-QA after re-integration). Operators
//! debugging "did QA pass?" want the *history* — every prior verdict,
//! its evidence, and the role that authored it — not just the latest
//! row. Filtering by work-item id and ordering newest-first puts the
//! most recent decision at the top while preserving the audit trail.
//!
//! ## Exit codes
//!
//! - `0` — verdicts fetched (zero or more rows printed to stdout).
//!   A work item with no verdicts is *not* an error — it simply means
//!   QA has not yet ruled, which is a normal state for in-flight work.
//! - `1` — the requested `<ID>` is not present in `work_items`. Distinct
//!   from "no verdicts": "no work item" deserves a louder signal.
//! - `2` — the durable state database could not be opened or queried.

use std::path::PathBuf;

use symphony_state::qa_verdicts::{QaVerdictRecord, QaVerdictRepository};
use symphony_state::repository::{WorkItemId, WorkItemRecord, WorkItemRepository};
use symphony_state::{StateDb, StateError};

use crate::cli::{QaCommand, QaVerdictArgs};

/// Outcome of a `symphony qa verdict` invocation.
#[derive(Debug)]
pub enum VerdictOutcome {
    /// Root work item exists; zero or more verdict rows were fetched.
    Ok(Box<VerdictListing>),
    /// The requested `<ID>` does not exist in `work_items`.
    NotFound { id: WorkItemId, db_path: PathBuf },
    /// The state database could not be opened or queried.
    StateDbFailed { db_path: PathBuf, error: StateError },
}

impl VerdictOutcome {
    /// Stable exit code; see module docs.
    pub fn exit_code(&self) -> i32 {
        match self {
            VerdictOutcome::Ok(_) => 0,
            VerdictOutcome::NotFound { .. } => 1,
            VerdictOutcome::StateDbFailed { .. } => 2,
        }
    }
}

/// Materialised result: the work item the operator asked about plus
/// every verdict filed against it. Verdicts are ordered newest-first by
/// the storage-layer query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerdictListing {
    /// The work item the operator asked about.
    pub work_item: WorkItemRecord,
    /// Verdict rows, newest first. May be empty.
    pub verdicts: Vec<QaVerdictRecord>,
}

/// Top-level `qa` dispatch: pick a subcommand and run it.
pub fn run(cmd: &QaCommand) -> VerdictOutcome {
    match cmd {
        QaCommand::Verdict(args) => run_verdict(args),
    }
}

/// Render the outcome to stdout (success) or stderr (failure) and
/// return the caller's exit code.
pub fn render(outcome: &VerdictOutcome) -> i32 {
    match outcome {
        VerdictOutcome::Ok(listing) => render_listing(listing),
        VerdictOutcome::NotFound { id, db_path } => {
            eprintln!(
                "error: work item id {} not found in {}",
                id.0,
                db_path.display(),
            );
        }
        VerdictOutcome::StateDbFailed { db_path, error } => {
            eprintln!(
                "error: state DB read failed for {}: {error}",
                db_path.display(),
            );
        }
    }
    outcome.exit_code()
}

/// Build a listing for a single `qa verdict` invocation.
pub fn run_verdict(args: &QaVerdictArgs) -> VerdictOutcome {
    let db = match StateDb::open(&args.state_db) {
        Ok(db) => db,
        Err(error) => {
            return VerdictOutcome::StateDbFailed {
                db_path: args.state_db.clone(),
                error,
            };
        }
    };
    let id = WorkItemId(args.id);
    let work_item = match db.get_work_item(id) {
        Ok(Some(w)) => w,
        Ok(None) => {
            return VerdictOutcome::NotFound {
                id,
                db_path: args.state_db.clone(),
            };
        }
        Err(error) => {
            return VerdictOutcome::StateDbFailed {
                db_path: args.state_db.clone(),
                error,
            };
        }
    };
    let verdicts = match db.list_qa_verdicts_for_work_item(id) {
        Ok(rows) => rows,
        Err(error) => {
            return VerdictOutcome::StateDbFailed {
                db_path: args.state_db.clone(),
                error,
            };
        }
    };
    VerdictOutcome::Ok(Box::new(VerdictListing {
        work_item,
        verdicts,
    }))
}

fn render_listing(listing: &VerdictListing) {
    let w = &listing.work_item;
    println!(
        "issue {}  [{}]  {}/{}  {}",
        w.id.0, w.status_class, w.tracker_id, w.identifier, w.title,
    );
    if listing.verdicts.is_empty() {
        println!("  (no QA verdicts recorded)");
        return;
    }
    println!("  verdicts ({}, newest first):", listing.verdicts.len());
    for v in &listing.verdicts {
        let waiver = v
            .waiver_role
            .as_deref()
            .map(|r| format!(" waiver={r}"))
            .unwrap_or_default();
        let reason = v
            .reason
            .as_deref()
            .map(|r| format!("  reason: {r}"))
            .unwrap_or_default();
        println!(
            "    {}  [{}]  by={} run={}{}{}",
            v.created_at, v.verdict, v.role, v.run_id.0, waiver, reason,
        );
        if let Some(ev) = v.evidence.as_deref() {
            println!("      evidence: {ev}");
        }
        if let Some(trace) = v.acceptance_trace.as_deref() {
            println!("      acceptance_trace: {trace}");
        }
        if let Some(blockers) = v.blockers_created.as_deref() {
            println!("      blockers_created: {blockers}");
        }
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use symphony_state::migrations::migrations;
    use symphony_state::repository::{NewRun, NewWorkItem, RunId, RunRepository};

    fn fresh_db() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let mut db = StateDb::open(&path).unwrap();
        db.migrate(migrations()).unwrap();
        (dir, path)
    }

    fn seed_item(db: &mut StateDb, identifier: &str) -> WorkItemId {
        db.create_work_item(NewWorkItem {
            tracker_id: "linear",
            identifier,
            parent_id: None,
            title: "demo",
            status_class: "qa",
            tracker_status: "in_review",
            assigned_role: None,
            assigned_agent: None,
            priority: None,
            workspace_policy: None,
            branch_policy: None,
            now: "2026-05-08T00:00:00Z",
        })
        .unwrap()
        .id
    }

    fn seed_run(db: &mut StateDb, work_item: WorkItemId, now: &str) -> RunId {
        db.create_run(NewRun {
            work_item_id: work_item,
            role: "qa",
            agent: "claude",
            status: "completed",
            workspace_claim_id: None,
            now,
        })
        .unwrap()
        .id
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_verdict(
        db_path: &std::path::Path,
        work_item: WorkItemId,
        run: RunId,
        role: &str,
        verdict: &str,
        waiver_role: Option<&str>,
        reason: Option<&str>,
        evidence: Option<&str>,
        acceptance_trace: Option<&str>,
        blockers_created: Option<&str>,
        now: &str,
    ) {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute(
            "INSERT INTO qa_verdicts \
                    (work_item_id, run_id, role, verdict, waiver_role, reason, \
                     evidence, acceptance_trace, blockers_created, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                work_item.0,
                run.0,
                role,
                verdict,
                waiver_role,
                reason,
                evidence,
                acceptance_trace,
                blockers_created,
                now,
            ],
        )
        .unwrap();
    }

    #[test]
    fn missing_root_yields_not_found_with_exit_code_one() {
        let (_dir, path) = fresh_db();
        let outcome = run_verdict(&QaVerdictArgs {
            id: 99_999,
            state_db: path.clone(),
        });
        match outcome {
            VerdictOutcome::NotFound { id, db_path } => {
                assert_eq!(id, WorkItemId(99_999));
                assert_eq!(db_path, path);
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn unreadable_state_db_yields_state_db_failed() {
        let outcome = run_verdict(&QaVerdictArgs {
            id: 1,
            state_db: PathBuf::from("/no/such/dir/state.db"),
        });
        match outcome {
            VerdictOutcome::StateDbFailed { .. } => {}
            other => panic!("expected StateDbFailed, got {other:?}"),
        }
        assert_eq!(outcome.exit_code(), 2);
    }

    #[test]
    fn work_item_with_no_verdicts_returns_ok_empty() {
        let (_dir, path) = fresh_db();
        let id = {
            let mut db = StateDb::open(&path).unwrap();
            seed_item(&mut db, "ENG-1")
        };
        let outcome = run_verdict(&QaVerdictArgs {
            id: id.0,
            state_db: path,
        });
        let listing = match outcome {
            VerdictOutcome::Ok(l) => l,
            other => panic!("expected Ok, got {other:?}"),
        };
        assert_eq!(listing.work_item.identifier, "ENG-1");
        assert!(listing.verdicts.is_empty());
    }

    #[test]
    fn verdicts_render_newest_first_with_full_metadata() {
        let (_dir, path) = fresh_db();
        let id;
        let r1;
        let r2;
        {
            let mut db = StateDb::open(&path).unwrap();
            id = seed_item(&mut db, "ENG-1");
            r1 = seed_run(&mut db, id, "2026-05-08T00:01:00Z");
            r2 = seed_run(&mut db, id, "2026-05-08T00:02:00Z");

            insert_verdict(
                &path,
                id,
                r1,
                "qa",
                "failed_needs_rework",
                None,
                Some("flake"),
                None,
                None,
                None,
                "2026-05-08T00:03:00Z",
            );
            insert_verdict(
                &path,
                id,
                r2,
                "qa",
                "passed",
                None,
                None,
                Some(r#"{"ci":"green"}"#),
                Some(r#"[{"id":"AC1"}]"#),
                None,
                "2026-05-08T00:05:00Z",
            );
        }

        let outcome = run_verdict(&QaVerdictArgs {
            id: id.0,
            state_db: path,
        });
        let listing = match outcome {
            VerdictOutcome::Ok(l) => l,
            other => panic!("expected Ok, got {other:?}"),
        };
        assert_eq!(listing.verdicts.len(), 2);
        assert_eq!(listing.verdicts[0].verdict, "passed");
        assert_eq!(listing.verdicts[0].run_id, r2);
        assert_eq!(listing.verdicts[1].verdict, "failed_needs_rework");
        assert_eq!(listing.verdicts[1].run_id, r1);
        assert_eq!(listing.verdicts[1].reason.as_deref(), Some("flake"));
    }

    #[test]
    fn render_returns_zero_on_ok_outcome() {
        let (_dir, path) = fresh_db();
        let id = {
            let mut db = StateDb::open(&path).unwrap();
            seed_item(&mut db, "ENG-1")
        };
        let outcome = run_verdict(&QaVerdictArgs {
            id: id.0,
            state_db: path,
        });
        assert_eq!(render(&outcome), 0);
    }

    #[test]
    fn render_returns_one_on_not_found_outcome() {
        let (_dir, path) = fresh_db();
        let outcome = run_verdict(&QaVerdictArgs {
            id: 12_345,
            state_db: path,
        });
        assert_eq!(render(&outcome), 1);
    }
}
