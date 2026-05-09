//! SQLite-backed [`RunLeaseStore`] adapter (CHECKLIST_v2 Phase 11).
//!
//! The kernel runners
//! ([`SpecialistRunner`](symphony_core::run_lease::RunLeaseStore) and
//! peers) depend on the narrow [`RunLeaseStore`] trait declared in
//! `symphony-core`. This module provides the production adapter that
//! delegates to [`RunRepository::acquire_lease`] and
//! [`RunRepository::release_lease`] under a shared
//! `Arc<tokio::sync::Mutex<dyn RunRepository + Send>>`.
//!
//! The mutex composition matches the orchestrator's existing pattern:
//! a single owning task hands an `Arc` clone to each runner so
//! `acquire`/`release` calls serialize against any other lease-touching
//! transaction (e.g. `StateTransaction::start_run`). SQLite itself
//! serializes writers, so the mutex is mainly there to satisfy the
//! `&mut self` receiver on the synchronous repository trait.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use symphony_core::blocker::RunRef;
use symphony_core::lease::LeaseOwner;
use symphony_core::run_lease::{LeaseAcquireOutcome, RunLeaseError, RunLeaseStore};

use crate::repository::{LeaseAcquisition, RunId, RunRepository};

/// Adapter wrapping an `Arc<Mutex<dyn RunRepository + Send>>` into a
/// [`RunLeaseStore`].
///
/// Cloning this handle is cheap (it's an `Arc` clone); the inner mutex
/// is shared so all clones serialize through the same SQLite connection.
#[derive(Clone)]
pub struct SqliteRunLeaseStore {
    repo: Arc<Mutex<dyn RunRepository + Send>>,
}

impl SqliteRunLeaseStore {
    /// Wrap an existing `Arc<Mutex<dyn RunRepository + Send>>`.
    pub fn new(repo: Arc<Mutex<dyn RunRepository + Send>>) -> Self {
        Self { repo }
    }

    /// Borrow the underlying repository handle. Useful when the caller
    /// already owns the `Arc` and wants to compose lease operations with
    /// other repository calls inside the same lock window.
    pub fn repo(&self) -> &Arc<Mutex<dyn RunRepository + Send>> {
        &self.repo
    }
}

impl std::fmt::Debug for SqliteRunLeaseStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteRunLeaseStore").finish()
    }
}

#[async_trait]
impl RunLeaseStore for SqliteRunLeaseStore {
    async fn acquire(
        &self,
        run_id: RunRef,
        owner: &LeaseOwner,
        expires_at: &str,
        now: &str,
    ) -> Result<LeaseAcquireOutcome, RunLeaseError> {
        let canonical = owner.to_string();
        let mut guard = self.repo.lock().await;
        let outcome = guard
            .acquire_lease(RunId(run_id.get()), &canonical, expires_at, now)
            .map_err(|err| RunLeaseError::Backend(err.to_string()))?;
        Ok(map_outcome(outcome))
    }

    async fn release(&self, run_id: RunRef) -> Result<bool, RunLeaseError> {
        let mut guard = self.repo.lock().await;
        guard
            .release_lease(RunId(run_id.get()))
            .map_err(|err| RunLeaseError::Backend(err.to_string()))
    }
}

fn map_outcome(outcome: LeaseAcquisition) -> LeaseAcquireOutcome {
    match outcome {
        LeaseAcquisition::Acquired => LeaseAcquireOutcome::Acquired,
        LeaseAcquisition::Contended { holder, expires_at } => {
            LeaseAcquireOutcome::Contended { holder, expires_at }
        }
        LeaseAcquisition::NotFound => LeaseAcquireOutcome::NotFound,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::TempDir;

    use crate::StateDb;
    use crate::migrations::migrations;
    use crate::repository::{NewRun, NewWorkItem, WorkItemRecord, WorkItemRepository};

    fn open_db(dir: &TempDir) -> StateDb {
        let mut db = StateDb::open(dir.path().join("state.sqlite3")).expect("open");
        db.migrate(migrations()).expect("migrate");
        db
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

    fn seed_run(db: &mut StateDb, work_item: &WorkItemRecord) -> RunId {
        let record = db
            .create_run(NewRun {
                work_item_id: work_item.id,
                role: "specialist:ui",
                agent: "codex",
                status: "queued",
                workspace_claim_id: None,
                now: "2026-05-08T00:00:00Z",
            })
            .expect("create run");
        record.id
    }

    fn owner(slot: u32) -> LeaseOwner {
        LeaseOwner::new("host", "scheduler-1", slot).unwrap()
    }

    #[tokio::test]
    async fn acquire_returns_not_found_for_missing_run() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = open_db(&dir);
        let repo: Arc<Mutex<dyn RunRepository + Send>> = Arc::new(Mutex::new(db));
        let store = SqliteRunLeaseStore::new(repo);
        let outcome = store
            .acquire(
                RunRef::new(999),
                &owner(0),
                "2026-05-08T00:01:00Z",
                "2026-05-08T00:00:00Z",
            )
            .await
            .expect("acquire");
        assert_eq!(outcome, LeaseAcquireOutcome::NotFound);
    }

    #[tokio::test]
    async fn acquire_writes_lease_and_release_clears_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut db = open_db(&dir);
        let work = seed_work_item(&mut db, "OWNER/REPO#1");
        let run_id = seed_run(&mut db, &work);
        let repo: Arc<Mutex<dyn RunRepository + Send>> = Arc::new(Mutex::new(db));
        let store = SqliteRunLeaseStore::new(Arc::clone(&repo));

        let outcome = store
            .acquire(
                RunRef::new(run_id.0),
                &owner(0),
                "2026-05-08T00:05:00Z",
                "2026-05-08T00:00:00Z",
            )
            .await
            .expect("acquire");
        assert_eq!(outcome, LeaseAcquireOutcome::Acquired);

        // Lease columns are populated.
        {
            let guard = repo.lock().await;
            let row = guard.get_run(run_id).expect("get_run").expect("row");
            assert_eq!(
                row.lease_owner.as_deref(),
                Some(owner(0).to_string().as_str())
            );
            assert_eq!(
                row.lease_expires_at.as_deref(),
                Some("2026-05-08T00:05:00Z")
            );
        }

        // Released lease clears the columns.
        let released = store.release(RunRef::new(run_id.0)).await.expect("release");
        assert!(released);
        {
            let guard = repo.lock().await;
            let row = guard.get_run(run_id).expect("get_run").expect("row");
            assert!(row.lease_owner.is_none());
            assert!(row.lease_expires_at.is_none());
        }

        // Releasing a missing run returns Ok(false).
        let missing = store
            .release(RunRef::new(9_999))
            .await
            .expect("release missing");
        assert!(!missing);
    }

    #[tokio::test]
    async fn lease_is_only_surfaced_in_find_expired_after_expiry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut db = open_db(&dir);
        let work = seed_work_item(&mut db, "OWNER/REPO#2");
        let run_id = seed_run(&mut db, &work);
        let repo: Arc<Mutex<dyn RunRepository + Send>> = Arc::new(Mutex::new(db));
        let store = SqliteRunLeaseStore::new(Arc::clone(&repo));

        store
            .acquire(
                RunRef::new(run_id.0),
                &owner(0),
                "2026-05-08T00:05:00Z",
                "2026-05-08T00:00:00Z",
            )
            .await
            .expect("acquire");

        // Before expiry: not surfaced.
        {
            let guard = repo.lock().await;
            let expired = guard
                .find_expired_leases("2026-05-08T00:05:00Z")
                .expect("find_expired_leases at boundary");
            assert!(
                expired.iter().all(|r| r.id != run_id),
                "lease at exact expiration must not appear yet (strict <)"
            );
        }

        // After expiry: surfaced.
        {
            let guard = repo.lock().await;
            let expired = guard
                .find_expired_leases("2026-05-08T00:05:01Z")
                .expect("find_expired_leases past expiry");
            assert!(
                expired.iter().any(|r| r.id == run_id),
                "expired lease must be surfaced for recovery"
            );
        }

        // Releasing removes it from the expired set.
        store.release(RunRef::new(run_id.0)).await.expect("release");
        {
            let guard = repo.lock().await;
            let expired = guard
                .find_expired_leases("2999-01-01T00:00:00Z")
                .expect("find_expired_leases far future");
            assert!(expired.iter().all(|r| r.id != run_id));
        }
    }

    #[tokio::test]
    async fn acquire_reports_contention_for_other_unexpired_owner() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut db = open_db(&dir);
        let work = seed_work_item(&mut db, "OWNER/REPO#3");
        let run_id = seed_run(&mut db, &work);
        let repo: Arc<Mutex<dyn RunRepository + Send>> = Arc::new(Mutex::new(db));
        let store = SqliteRunLeaseStore::new(Arc::clone(&repo));

        store
            .acquire(
                RunRef::new(run_id.0),
                &owner(0),
                "2026-05-08T00:05:00Z",
                "2026-05-08T00:00:00Z",
            )
            .await
            .expect("acquire");

        let outcome = store
            .acquire(
                RunRef::new(run_id.0),
                &owner(1),
                "2026-05-08T00:06:00Z",
                "2026-05-08T00:00:30Z",
            )
            .await
            .expect("acquire other");
        match outcome {
            LeaseAcquireOutcome::Contended { holder, expires_at } => {
                assert_eq!(holder, owner(0).to_string());
                assert_eq!(expires_at, "2026-05-08T00:05:00Z");
            }
            other => panic!("expected Contended, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn acquire_is_idempotent_for_same_owner() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut db = open_db(&dir);
        let work = seed_work_item(&mut db, "OWNER/REPO#4");
        let run_id = seed_run(&mut db, &work);
        let repo: Arc<Mutex<dyn RunRepository + Send>> = Arc::new(Mutex::new(db));
        let store = SqliteRunLeaseStore::new(repo);

        store
            .acquire(
                RunRef::new(run_id.0),
                &owner(0),
                "2026-05-08T00:01:00Z",
                "2026-05-08T00:00:00Z",
            )
            .await
            .expect("acquire");
        let outcome = store
            .acquire(
                RunRef::new(run_id.0),
                &owner(0),
                "2026-05-08T00:02:00Z",
                "2026-05-08T00:00:30Z",
            )
            .await
            .expect("re-acquire");
        assert_eq!(outcome, LeaseAcquireOutcome::Acquired);
    }
}
