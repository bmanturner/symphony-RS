//! SQLite-backed [`RecoveryQueueSource`] adapter (CHECKLIST_v2 Phase 11).
//!
//! Surfaces expired-lease reap candidates from
//! [`RunRepository::find_expired_leases`] into the kernel's
//! `RecoveryQueueTick` so the recovery runner picks them up. Orphaned
//! workspace claims are not yet derivable from the durable schema (a
//! dedicated `workspace_claims` view will land alongside the recovery
//! command), so the orphan list is empty — matching the parity scope of
//! the previous in-memory `ActiveSetRecoverySource` and keeping the tick
//! contract intact.
//!
//! Concurrency: the [`RecoveryQueueSource`] trait is synchronous, so this
//! adapter holds the repository behind a [`std::sync::Mutex`] (separate
//! from the `tokio::sync::Mutex` used by the async lease store). SQLite
//! WAL mode allows concurrent readers across distinct connections, so the
//! composition root is expected to open a dedicated [`crate::StateDb`]
//! connection for this source rather than sharing the writer connection.

use std::sync::{Arc, Mutex};

use symphony_core::recovery_tick::{
    ExpiredLeaseCandidate, OrphanedWorkspaceClaimCandidate, RecoveryQueueError,
    RecoveryQueueSource, RecoveryRunId, RecoveryWorkspaceClaimId,
};
use symphony_core::work_item::WorkItemId as CoreWorkItemId;

use crate::repository::RunRepository;

/// Produces an RFC3339 "now" timestamp for the recovery query. Boxed so
/// the composition root can plug in a real wall clock and tests can plug
/// in a deterministic value.
pub trait RecoveryClock: Send + Sync {
    /// Return the current RFC3339 timestamp (lex-comparable to the values
    /// the kernel writes into `runs.lease_expires_at`).
    fn now_rfc3339(&self) -> String;
}

/// Deterministic [`RecoveryClock`] holding a fixed timestamp string. The
/// stored value can be mutated via [`Self::set`] so a single instance can
/// drive multiple tick iterations across a test.
#[derive(Debug)]
pub struct FixedRecoveryClock {
    now: Mutex<String>,
}

impl FixedRecoveryClock {
    /// Construct with an initial timestamp.
    pub fn new(now: impl Into<String>) -> Self {
        Self {
            now: Mutex::new(now.into()),
        }
    }

    /// Replace the held timestamp.
    pub fn set(&self, now: impl Into<String>) {
        *self
            .now
            .lock()
            .expect("fixed recovery clock mutex poisoned") = now.into();
    }
}

impl RecoveryClock for FixedRecoveryClock {
    fn now_rfc3339(&self) -> String {
        self.now
            .lock()
            .expect("fixed recovery clock mutex poisoned")
            .clone()
    }
}

/// SQLite-backed [`RecoveryQueueSource`].
///
/// Reads expired leases via [`RunRepository::find_expired_leases`]; the
/// `now` value compared against `runs.lease_expires_at` is supplied by
/// the configured [`RecoveryClock`].
pub struct SqliteRecoveryQueueSource {
    repo: Arc<Mutex<dyn RunRepository + Send>>,
    clock: Arc<dyn RecoveryClock>,
}

impl SqliteRecoveryQueueSource {
    /// Construct from a sync-locked repository and a clock.
    pub fn new(repo: Arc<Mutex<dyn RunRepository + Send>>, clock: Arc<dyn RecoveryClock>) -> Self {
        Self { repo, clock }
    }
}

impl std::fmt::Debug for SqliteRecoveryQueueSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteRecoveryQueueSource").finish()
    }
}

impl RecoveryQueueSource for SqliteRecoveryQueueSource {
    fn list_expired_leases(&self) -> Result<Vec<ExpiredLeaseCandidate>, RecoveryQueueError> {
        let now = self.clock.now_rfc3339();
        let guard = self
            .repo
            .lock()
            .map_err(|err| RecoveryQueueError::Source(format!("repo mutex poisoned: {err}")))?;
        let rows = guard
            .find_expired_leases(&now)
            .map_err(|err| RecoveryQueueError::Source(err.to_string()))?;
        Ok(rows
            .into_iter()
            .map(|r| ExpiredLeaseCandidate {
                run_id: RecoveryRunId::new(r.id.0),
                work_item_id: CoreWorkItemId::new(r.work_item_id.0),
                lease_owner: r.lease_owner.unwrap_or_default(),
                lease_expires_at: r.lease_expires_at.unwrap_or_default(),
                workspace_claim_id: r.workspace_claim_id.map(RecoveryWorkspaceClaimId::new),
            })
            .collect())
    }

    fn list_orphaned_workspace_claims(
        &self,
    ) -> Result<Vec<OrphanedWorkspaceClaimCandidate>, RecoveryQueueError> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use symphony_core::queue_tick::{QueueTick, QueueTickCadence};
    use symphony_core::recovery_tick::{
        RecoveryDispatchQueue, RecoveryDispatchRequest, RecoveryQueueTick,
    };

    use crate::StateDb;
    use crate::migrations::migrations;
    use crate::repository::{
        LeaseAcquisition, NewRun, NewWorkItem, RunId, RunRecord, WorkItemRecord, WorkItemRepository,
    };

    fn open_db() -> (tempfile::TempDir, StateDb) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut db = StateDb::open(dir.path().join("state.sqlite3")).expect("open");
        db.migrate(migrations()).expect("migrate");
        (dir, db)
    }

    fn seed_work_item(db: &mut StateDb, identifier: &str) -> WorkItemRecord {
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
        .expect("create work item")
    }

    fn seed_run(db: &mut StateDb, work_item: &WorkItemRecord) -> RunRecord {
        db.create_run(NewRun {
            work_item_id: work_item.id,
            role: "specialist:ui",
            agent: "codex",
            status: "running",
            workspace_claim_id: None,
            now: "2026-05-08T00:00:00Z",
        })
        .expect("create run")
    }

    fn acquire(db: &mut StateDb, id: RunId, owner: &str, expires_at: &str, now: &str) {
        let outcome = db
            .acquire_lease(id, owner, expires_at, now)
            .expect("acquire");
        assert_eq!(outcome, LeaseAcquisition::Acquired, "lease must acquire");
    }

    fn cadence() -> QueueTickCadence {
        QueueTickCadence::from_millis(10, 0)
    }

    fn build_source(
        db: StateDb,
        clock: Arc<FixedRecoveryClock>,
    ) -> (
        Arc<Mutex<dyn RunRepository + Send>>,
        SqliteRecoveryQueueSource,
    ) {
        let repo: Arc<Mutex<dyn RunRepository + Send>> = Arc::new(Mutex::new(db));
        let source = SqliteRecoveryQueueSource::new(repo.clone(), clock);
        (repo, source)
    }

    #[test]
    fn empty_repo_yields_no_candidates() {
        let (_dir, db) = open_db();
        let clock = Arc::new(FixedRecoveryClock::new("2026-05-08T01:00:00Z"));
        let (_repo, src) = build_source(db, clock);
        assert!(src.list_expired_leases().expect("ok").is_empty());
        assert!(src.list_orphaned_workspace_claims().expect("ok").is_empty());
    }

    #[test]
    fn unexpired_lease_is_not_surfaced() {
        let (_dir, mut db) = open_db();
        let work = seed_work_item(&mut db, "OWNER/REPO#1");
        let run = seed_run(&mut db, &work);
        acquire(
            &mut db,
            run.id,
            "host/sched/0",
            "2026-05-08T00:05:00Z",
            "2026-05-08T00:00:00Z",
        );
        let clock = Arc::new(FixedRecoveryClock::new("2026-05-08T00:04:00Z"));
        let (_repo, src) = build_source(db, clock);
        assert!(src.list_expired_leases().expect("ok").is_empty());
    }

    #[test]
    fn expired_lease_is_surfaced_with_full_projection() {
        let (_dir, mut db) = open_db();
        let work = seed_work_item(&mut db, "OWNER/REPO#1");
        let run = seed_run(&mut db, &work);
        acquire(
            &mut db,
            run.id,
            "host/sched/0",
            "2026-05-08T00:05:00Z",
            "2026-05-08T00:00:00Z",
        );
        let clock = Arc::new(FixedRecoveryClock::new("2026-05-08T00:06:00Z"));
        let (_repo, src) = build_source(db, clock);
        let candidates = src.list_expired_leases().expect("ok");
        assert_eq!(candidates.len(), 1);
        let c = &candidates[0];
        assert_eq!(c.run_id, RecoveryRunId::new(run.id.0));
        assert_eq!(c.work_item_id, CoreWorkItemId::new(work.id.0));
        assert_eq!(c.lease_owner, "host/sched/0");
        assert_eq!(c.lease_expires_at, "2026-05-08T00:05:00Z");
        assert!(c.workspace_claim_id.is_none());
    }

    #[tokio::test]
    async fn tick_emits_expired_lease_once_then_defers_until_pruned() {
        let (_dir, mut db) = open_db();
        let work = seed_work_item(&mut db, "OWNER/REPO#1");
        let run = seed_run(&mut db, &work);
        acquire(
            &mut db,
            run.id,
            "host/sched/0",
            "2026-05-08T00:05:00Z",
            "2026-05-08T00:00:00Z",
        );
        let clock = Arc::new(FixedRecoveryClock::new("2026-05-08T00:06:00Z"));
        let (repo, src) = build_source(db, clock.clone());
        let queue = Arc::new(RecoveryDispatchQueue::new());
        let mut tick = RecoveryQueueTick::new(
            Arc::new(src) as Arc<dyn RecoveryQueueSource>,
            queue.clone(),
            cadence(),
        );

        // Tick 1: expired lease becomes a dispatch.
        let o1 = tick.tick().await;
        assert_eq!(o1.processed, 1);
        let drained = queue.drain();
        assert_eq!(drained.len(), 1);
        match &drained[0] {
            RecoveryDispatchRequest::ExpiredLease {
                run_id,
                work_item_id,
                lease_owner,
                ..
            } => {
                assert_eq!(*run_id, RecoveryRunId::new(run.id.0));
                assert_eq!(*work_item_id, CoreWorkItemId::new(work.id.0));
                assert_eq!(lease_owner, "host/sched/0");
            }
            other => panic!("expected expired lease, got {other:?}"),
        }

        // Tick 2: source still surfaces the same lease (still expired);
        // the tick claim set defers it instead of re-emitting.
        let o2 = tick.tick().await;
        assert_eq!(o2.processed, 0);
        assert_eq!(o2.deferred, 1);
        assert!(queue.is_empty());

        // Recovery handler clears the lease — source no longer surfaces
        // it, the tick prunes its claim, and a future re-expiry would
        // emit again.
        {
            let mut guard = repo.lock().expect("repo mutex");
            assert!(guard.release_lease(run.id).expect("release"));
        }
        let o3 = tick.tick().await;
        assert_eq!(o3.considered, 0);
        assert_eq!(o3.processed, 0);
        assert_eq!(o3.deferred, 0);
        assert_eq!(tick.claimed_count(), 0);
    }

    #[tokio::test]
    async fn released_lease_does_not_trigger_dispatch() {
        let (_dir, mut db) = open_db();
        let work = seed_work_item(&mut db, "OWNER/REPO#1");
        let run = seed_run(&mut db, &work);
        acquire(
            &mut db,
            run.id,
            "host/sched/0",
            "2026-05-08T00:05:00Z",
            "2026-05-08T00:00:00Z",
        );
        // Release before the tick observes the lease.
        assert!(db.release_lease(run.id).expect("release"));
        let clock = Arc::new(FixedRecoveryClock::new("2026-05-08T00:06:00Z"));
        let (_repo, src) = build_source(db, clock);
        let queue = Arc::new(RecoveryDispatchQueue::new());
        let mut tick = RecoveryQueueTick::new(
            Arc::new(src) as Arc<dyn RecoveryQueueSource>,
            queue.clone(),
            cadence(),
        );
        let o = tick.tick().await;
        assert_eq!(o.considered, 0);
        assert!(queue.is_empty());
    }

    #[tokio::test]
    async fn renewed_lease_prunes_already_claimed_candidate() {
        let (_dir, mut db) = open_db();
        let work = seed_work_item(&mut db, "OWNER/REPO#1");
        let run = seed_run(&mut db, &work);
        acquire(
            &mut db,
            run.id,
            "host/sched/0",
            "2026-05-08T00:05:00Z",
            "2026-05-08T00:00:00Z",
        );
        let clock = Arc::new(FixedRecoveryClock::new("2026-05-08T00:06:00Z"));
        let (repo, src) = build_source(db, clock.clone());
        let queue = Arc::new(RecoveryDispatchQueue::new());
        let mut tick = RecoveryQueueTick::new(
            Arc::new(src) as Arc<dyn RecoveryQueueSource>,
            queue.clone(),
            cadence(),
        );

        // Tick 1: expired lease becomes a dispatch + claim.
        let o1 = tick.tick().await;
        assert_eq!(o1.processed, 1);
        assert_eq!(queue.drain().len(), 1);
        assert_eq!(tick.claimed_count(), 1);

        // The original holder renews — same owner, fresh expiry past the
        // recovery clock. `find_expired_leases` no longer surfaces it.
        {
            let mut guard = repo.lock().expect("repo mutex");
            let outcome = guard
                .acquire_lease(
                    run.id,
                    "host/sched/0",
                    "2026-05-08T00:10:00Z",
                    "2026-05-08T00:06:00Z",
                )
                .expect("renew");
            assert_eq!(outcome, LeaseAcquisition::Acquired);
        }

        // Tick 2: source returns nothing, tick prunes the claim.
        let o2 = tick.tick().await;
        assert_eq!(o2.considered, 0);
        assert_eq!(o2.processed, 0);
        assert_eq!(o2.deferred, 0);
        assert_eq!(tick.claimed_count(), 0);
        assert!(queue.is_empty());
    }

    #[test]
    fn fixed_recovery_clock_set_replaces_value() {
        let c = FixedRecoveryClock::new("2026-05-08T00:00:00Z");
        assert_eq!(c.now_rfc3339(), "2026-05-08T00:00:00Z");
        c.set("2026-05-08T01:00:00Z");
        assert_eq!(c.now_rfc3339(), "2026-05-08T01:00:00Z");
    }
}
