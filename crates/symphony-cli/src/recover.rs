//! Implementation of the `symphony recover` subcommand
//! (ARCHITECTURE_v2.md §4.7 / CHECKLIST_v2 Phase 12).
//!
//! `recover` reconciles durable orchestrator state with the on-disk /
//! tracker world the live orchestrator would normally reconcile via the
//! [`symphony_core::recovery_runner::RecoveryRunner`]. It is the
//! operator's hand-driven equivalent: a one-shot sweep that surfaces (and
//! optionally reaps) two classes of stale rows:
//!
//! 1. **Expired leases** — `runs` rows whose `lease_expires_at < now`.
//!    These are runs whose lease holder died or stalled past its lease
//!    deadline. Reaping clears the lease columns
//!    ([`symphony_state::repository::RunRepository::release_lease`]) so
//!    the next dispatch pass can re-acquire the row. The recover command
//!    does *not* itself transition the run's `status` — the orchestrator's
//!    recovery dispatcher is responsible for that, since rerouting is a
//!    workflow-policy decision and bypassing it from a CLI utility would
//!    mask the real recovery path.
//! 2. **Orphaned workspace claims** — `workspace_claims` rows that no
//!    live run references and whose `claim_status` is not already a
//!    terminal label. Reaping flips `claim_status` to `'orphaned'`
//!    via [`symphony_state::StateDb::mark_workspace_claim_orphaned`].
//!    On-disk cleanup of the underlying path is left to the configured
//!    workspace cleanup policy; the CLI only adjudicates the durable
//!    label so the next live `RecoveryQueueTick` does not re-surface the
//!    same row.
//!
//! ## Dry-run by default
//!
//! `recover` is dry-run by default. The command prints what *would* be
//! reaped and exits zero without writing. The `--apply` flag promotes the
//! sweep into a write: each expired lease is released and each orphan
//! claim's status is updated. The default-safe behavior matches
//! `symphony cancel`'s "explicit --reason" footgun policy: an operator
//! who wants to mutate the durable store has to ask for it.
//!
//! ## Why not run the full kernel recovery pass?
//!
//! [`symphony_core::recovery_runner::RecoveryRunner`] is a long-lived,
//! lease-aware, semaphore-bounded async dispatcher; spinning one up from
//! a synchronous CLI command would require a tokio runtime, a
//! workflow-loaded dispatcher, and a recovery clock — all heavy for
//! "show me what's stale, optionally reap it". The CLI command therefore
//! invokes the same minimal database mutations the dispatcher would
//! perform (`release_lease`, `mark_workspace_claim_orphaned`) without
//! re-routing the affected work items through the scheduler. An operator
//! who wants the full reroute restarts `symphony run`; the next recovery
//! tick then rediscovers any leases this command did not clear.
//!
//! ## Exit codes
//!
//! - `0` — sweep completed (zero or more candidates surfaced and, with
//!   `--apply`, reaped).
//! - `2` — the durable state database could not be opened or queried.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use symphony_state::repository::{RunId, RunRecord, RunRepository};
use symphony_state::workspace_claims::OrphanWorkspaceClaim;
use symphony_state::{StateDb, StateError};

use crate::cli::RecoverArgs;

/// Outcome of one `symphony recover` invocation.
#[derive(Debug)]
pub enum RecoverOutcome {
    /// Sweep completed. The report describes what was surfaced and
    /// whether mutations were applied.
    Ok(Box<RecoverReport>),
    /// The durable state database could not be opened or queried.
    StateDbFailed { db_path: PathBuf, error: StateError },
}

impl RecoverOutcome {
    /// Stable exit code; see module docs.
    pub fn exit_code(&self) -> i32 {
        match self {
            RecoverOutcome::Ok(_) => 0,
            RecoverOutcome::StateDbFailed { .. } => 2,
        }
    }
}

/// Structured success summary. `applied = false` is dry-run; `applied =
/// true` means `--apply` was passed and each surfaced row was mutated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoverReport {
    /// `true` iff the command was invoked with `--apply` and at least
    /// attempted writes. Independent of whether candidates existed: a
    /// reported "applied" run with zero candidates is meaningful (it
    /// proves the operator's mental model of the state matches reality).
    pub applied: bool,
    /// RFC3339 timestamp used as `now` for the expired-lease query.
    pub now: String,
    /// Path of the state DB that was reconciled.
    pub db_path: PathBuf,
    /// Expired-lease candidates, oldest first.
    pub expired_leases: Vec<ReapedLease>,
    /// Orphaned workspace claims, ascending by id.
    pub orphan_claims: Vec<ReapedClaim>,
}

/// One expired-lease row surfaced by the sweep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReapedLease {
    /// Durable run id.
    pub run_id: i64,
    /// Owning work item.
    pub work_item_id: i64,
    /// Lease holder identifier.
    pub lease_owner: String,
    /// RFC3339 lease expiration as observed.
    pub lease_expires_at: String,
    /// The run's stored `status` at observe time. Carried for debugging
    /// — `recover` does not transition it.
    pub run_status: String,
    /// `true` when the lease columns were cleared this pass (`--apply`
    /// only). `false` for dry-run, and `false` if the row vanished
    /// between observe and write (race with another writer).
    pub cleared: bool,
}

/// One orphaned claim surfaced by the sweep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReapedClaim {
    /// Durable claim id.
    pub claim_id: i64,
    /// Owning work item.
    pub work_item_id: i64,
    /// On-disk path the claim allocated.
    pub path: String,
    /// `claim_status` at observe time (verbatim).
    pub claim_status: String,
    /// `true` when `claim_status` was flipped to `'orphaned'` this pass
    /// (`--apply` only).
    pub marked_orphaned: bool,
}

/// Run one `symphony recover` invocation against the configured DB.
pub fn run(args: &RecoverArgs) -> RecoverOutcome {
    let mut db = match StateDb::open(&args.state_db) {
        Ok(db) => db,
        Err(error) => {
            return RecoverOutcome::StateDbFailed {
                db_path: args.state_db.clone(),
                error,
            };
        }
    };

    let now = args.at.clone().unwrap_or_else(now_rfc3339);

    let expired_rows = match db.find_expired_leases(&now) {
        Ok(rows) => rows,
        Err(error) => {
            return RecoverOutcome::StateDbFailed {
                db_path: args.state_db.clone(),
                error,
            };
        }
    };

    let orphan_rows = match db.list_orphan_workspace_claims() {
        Ok(rows) => rows,
        Err(error) => {
            return RecoverOutcome::StateDbFailed {
                db_path: args.state_db.clone(),
                error,
            };
        }
    };

    let mut expired_leases = Vec::with_capacity(expired_rows.len());
    for row in expired_rows {
        let cleared = if args.apply {
            match db.release_lease(RunId(row.id.0)) {
                Ok(updated) => updated,
                Err(error) => {
                    return RecoverOutcome::StateDbFailed {
                        db_path: args.state_db.clone(),
                        error,
                    };
                }
            }
        } else {
            false
        };
        expired_leases.push(project_lease(&row, cleared));
    }

    let mut orphan_claims = Vec::with_capacity(orphan_rows.len());
    for row in orphan_rows {
        let marked = if args.apply {
            match db.mark_workspace_claim_orphaned(row.id) {
                Ok(updated) => updated,
                Err(error) => {
                    return RecoverOutcome::StateDbFailed {
                        db_path: args.state_db.clone(),
                        error,
                    };
                }
            }
        } else {
            false
        };
        orphan_claims.push(project_claim(row, marked));
    }

    RecoverOutcome::Ok(Box::new(RecoverReport {
        applied: args.apply,
        now,
        db_path: args.state_db.clone(),
        expired_leases,
        orphan_claims,
    }))
}

fn project_lease(row: &RunRecord, cleared: bool) -> ReapedLease {
    ReapedLease {
        run_id: row.id.0,
        work_item_id: row.work_item_id.0,
        lease_owner: row.lease_owner.clone().unwrap_or_default(),
        lease_expires_at: row.lease_expires_at.clone().unwrap_or_default(),
        run_status: row.status.clone(),
        cleared,
    }
}

fn project_claim(row: OrphanWorkspaceClaim, marked: bool) -> ReapedClaim {
    ReapedClaim {
        claim_id: row.id,
        work_item_id: row.work_item_id,
        path: row.path,
        claim_status: row.claim_status,
        marked_orphaned: marked,
    }
}

/// Render the outcome to stdout (success) or stderr (failure) and
/// return the caller's exit code.
pub fn render(outcome: &RecoverOutcome) -> i32 {
    match outcome {
        RecoverOutcome::Ok(report) => render_report(report),
        RecoverOutcome::StateDbFailed { db_path, error } => {
            eprintln!(
                "error: state DB read failed for {}: {error}",
                db_path.display(),
            );
        }
    }
    outcome.exit_code()
}

fn render_report(r: &RecoverReport) {
    let mode = if r.applied { "applied" } else { "dry-run" };
    println!(
        "recover [{mode}]  db={}  now={}  leases={}  orphan_claims={}",
        r.db_path.display(),
        r.now,
        r.expired_leases.len(),
        r.orphan_claims.len(),
    );
    if r.expired_leases.is_empty() && r.orphan_claims.is_empty() {
        println!("  (nothing to reconcile)");
        return;
    }
    if !r.expired_leases.is_empty() {
        println!("  expired leases (oldest first):");
        for l in &r.expired_leases {
            let suffix = if r.applied {
                if l.cleared {
                    " [cleared]"
                } else {
                    " [no-op: row vanished]"
                }
            } else {
                ""
            };
            println!(
                "    run={} work_item={} owner={} expired_at={} status={}{suffix}",
                l.run_id, l.work_item_id, l.lease_owner, l.lease_expires_at, l.run_status,
            );
        }
    }
    if !r.orphan_claims.is_empty() {
        println!("  orphan workspace claims (id ascending):");
        for c in &r.orphan_claims {
            let suffix = if r.applied {
                if c.marked_orphaned {
                    " [marked orphaned]"
                } else {
                    " [no-op: row vanished]"
                }
            } else {
                ""
            };
            println!(
                "    claim={} work_item={} path={} status={}{suffix}",
                c.claim_id, c.work_item_id, c.path, c.claim_status,
            );
        }
    }
}

/// RFC3339 second-precision UTC `now`. Mirrors the simple wall-clock
/// derivation used by [`crate::cancel`] so the two CLI surfaces produce
/// comparable timestamps without pulling in `chrono` or `time`.
fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (yyyy, mm, dd, hh, mi, ss) = epoch_to_components(secs as i64);
    format!("{yyyy:04}-{mm:02}-{dd:02}T{hh:02}:{mi:02}:{ss:02}Z")
}

// Tiny civil-from-days conversion; same algorithm `symphony cancel` uses.
fn epoch_to_components(secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let hh = (tod / 3600) as u32;
    let mi = ((tod % 3600) / 60) as u32;
    let ss = (tod % 60) as u32;
    // Howard Hinnant's civil_from_days, shifted to 1970 epoch.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u32;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i32 + (era * 400) as i32;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let yyyy = if m <= 2 { y + 1 } else { y };
    (yyyy, m, d, hh, mi, ss)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use symphony_state::migrations::migrations;
    use symphony_state::repository::{
        LeaseAcquisition, NewRun, NewWorkItem, RunRepository, WorkItemId, WorkItemRepository,
    };

    fn fresh_db() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let mut db = StateDb::open(&path).unwrap();
        db.migrate(migrations()).unwrap();
        (dir, path)
    }

    fn seed_work_item(path: &std::path::Path, identifier: &str) -> WorkItemId {
        let mut db = StateDb::open(path).unwrap();
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
        .unwrap()
        .id
    }

    fn seed_run_with_expired_lease(
        path: &std::path::Path,
        wi: WorkItemId,
        owner: &str,
        expires_at: &str,
    ) -> RunId {
        let mut db = StateDb::open(path).unwrap();
        let run = db
            .create_run(NewRun {
                work_item_id: wi,
                role: "specialist:ui",
                agent: "codex",
                status: "running",
                workspace_claim_id: None,
                now: "2026-05-08T00:00:00Z",
            })
            .unwrap();
        let outcome = db
            .acquire_lease(run.id, owner, expires_at, "2026-05-08T00:00:00Z")
            .unwrap();
        assert_eq!(outcome, LeaseAcquisition::Acquired);
        run.id
    }

    fn insert_active_claim(path: &std::path::Path, wi: WorkItemId) -> i64 {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute(
            "INSERT INTO workspace_claims \
                    (work_item_id, path, strategy, claim_status, created_at) \
                 VALUES (?1, ?2, 'git_worktree', 'active', '2026-05-08T00:00:00Z')",
            params![wi.0, format!("/tmp/wt-{}", wi.0)],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn read_claim_status(path: &std::path::Path, claim_id: i64) -> String {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.query_row(
            "SELECT claim_status FROM workspace_claims WHERE id = ?1",
            params![claim_id],
            |row| row.get::<_, String>(0),
        )
        .unwrap()
    }

    fn read_lease_owner(path: &std::path::Path, run_id: RunId) -> Option<String> {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.query_row(
            "SELECT lease_owner FROM runs WHERE id = ?1",
            params![run_id.0],
            |row| row.get::<_, Option<String>>(0),
        )
        .unwrap()
    }

    fn args(state_db: PathBuf, at: &str, apply: bool) -> RecoverArgs {
        RecoverArgs {
            state_db,
            at: Some(at.to_string()),
            apply,
        }
    }

    #[test]
    fn empty_db_reports_nothing_to_reconcile() {
        let (_dir, path) = fresh_db();
        let outcome = run(&args(path.clone(), "2026-05-08T01:00:00Z", false));
        let report = match outcome {
            RecoverOutcome::Ok(r) => r,
            other => panic!("expected Ok, got {other:?}"),
        };
        assert!(report.expired_leases.is_empty());
        assert!(report.orphan_claims.is_empty());
        assert!(!report.applied);
    }

    #[test]
    fn unreadable_state_db_yields_state_db_failed() {
        let outcome = run(&args(
            PathBuf::from("/no/such/dir/state.db"),
            "2026-05-08T01:00:00Z",
            false,
        ));
        match outcome {
            RecoverOutcome::StateDbFailed { .. } => {}
            other => panic!("expected StateDbFailed, got {other:?}"),
        }
        assert_eq!(outcome.exit_code(), 2);
    }

    #[test]
    fn dry_run_surfaces_expired_lease_without_clearing() {
        let (_dir, path) = fresh_db();
        let wi = seed_work_item(&path, "OWNER/REPO#1");
        let run_id = seed_run_with_expired_lease(&path, wi, "host/sched/0", "2026-05-08T00:05:00Z");

        let outcome = run(&args(path.clone(), "2026-05-08T00:06:00Z", false));
        let report = match outcome {
            RecoverOutcome::Ok(r) => r,
            other => panic!("expected Ok, got {other:?}"),
        };
        assert_eq!(report.expired_leases.len(), 1);
        let l = &report.expired_leases[0];
        assert_eq!(l.run_id, run_id.0);
        assert_eq!(l.lease_owner, "host/sched/0");
        assert!(!l.cleared, "dry-run must not clear");
        assert_eq!(
            read_lease_owner(&path, run_id).as_deref(),
            Some("host/sched/0"),
            "lease must remain on the row in dry-run",
        );
    }

    #[test]
    fn apply_clears_expired_lease() {
        let (_dir, path) = fresh_db();
        let wi = seed_work_item(&path, "OWNER/REPO#1");
        let run_id = seed_run_with_expired_lease(&path, wi, "host/sched/0", "2026-05-08T00:05:00Z");

        let outcome = run(&args(path.clone(), "2026-05-08T00:06:00Z", true));
        let report = match outcome {
            RecoverOutcome::Ok(r) => r,
            other => panic!("expected Ok, got {other:?}"),
        };
        assert!(report.applied);
        assert_eq!(report.expired_leases.len(), 1);
        assert!(report.expired_leases[0].cleared);
        assert!(read_lease_owner(&path, run_id).is_none());
    }

    #[test]
    fn unexpired_lease_is_not_surfaced() {
        let (_dir, path) = fresh_db();
        let wi = seed_work_item(&path, "OWNER/REPO#1");
        let _ = seed_run_with_expired_lease(&path, wi, "host/sched/0", "2026-05-08T00:10:00Z");
        let outcome = run(&args(path.clone(), "2026-05-08T00:05:00Z", false));
        let report = match outcome {
            RecoverOutcome::Ok(r) => r,
            other => panic!("expected Ok, got {other:?}"),
        };
        assert!(report.expired_leases.is_empty());
    }

    #[test]
    fn dry_run_surfaces_orphan_claim_without_marking() {
        let (_dir, path) = fresh_db();
        let wi = seed_work_item(&path, "OWNER/REPO#1");
        let claim_id = insert_active_claim(&path, wi);
        let outcome = run(&args(path.clone(), "2026-05-08T00:06:00Z", false));
        let report = match outcome {
            RecoverOutcome::Ok(r) => r,
            other => panic!("expected Ok, got {other:?}"),
        };
        assert_eq!(report.orphan_claims.len(), 1);
        assert_eq!(report.orphan_claims[0].claim_id, claim_id);
        assert!(!report.orphan_claims[0].marked_orphaned);
        assert_eq!(read_claim_status(&path, claim_id), "active");
    }

    #[test]
    fn apply_marks_orphan_claim() {
        let (_dir, path) = fresh_db();
        let wi = seed_work_item(&path, "OWNER/REPO#1");
        let claim_id = insert_active_claim(&path, wi);
        let outcome = run(&args(path.clone(), "2026-05-08T00:06:00Z", true));
        let report = match outcome {
            RecoverOutcome::Ok(r) => r,
            other => panic!("expected Ok, got {other:?}"),
        };
        assert!(report.applied);
        assert_eq!(report.orphan_claims.len(), 1);
        assert!(report.orphan_claims[0].marked_orphaned);
        assert_eq!(read_claim_status(&path, claim_id), "orphaned");
    }

    #[test]
    fn render_returns_zero_on_ok_outcome() {
        let (_dir, path) = fresh_db();
        let outcome = run(&args(path, "2026-05-08T01:00:00Z", false));
        assert_eq!(render(&outcome), 0);
    }

    #[test]
    fn render_returns_two_on_state_db_failed() {
        let outcome = run(&args(
            PathBuf::from("/no/such/dir/state.db"),
            "2026-05-08T01:00:00Z",
            false,
        ));
        assert_eq!(render(&outcome), 2);
    }
}
