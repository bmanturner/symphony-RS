//! Narrow lease-store trait for the production runners (CHECKLIST_v2 Phase 11).
//!
//! `RunRepository::{acquire_lease, release_lease}` lives in
//! `symphony-state` and is the durable source of truth. The kernel
//! runners (`SpecialistRunner`, `IntegrationDispatchRunner`,
//! `QaDispatchRunner`, `FollowupApprovalRunner`, `BudgetPauseRunner`,
//! `RecoveryRunner`) only need a tiny slice of that surface — acquire
//! on dispatch, release on terminal completion — so this module
//! exposes a focused [`RunLeaseStore`] trait that runners depend on
//! instead of the full `RunRepository`.
//!
//! The trait is async (the SQLite-backed adapter in `symphony-state`
//! takes a `Mutex` and may block briefly), `Send + Sync`, and intentionally
//! does not expose the run-row lifecycle: callers compose lease
//! acquisition with `update_run_status` + event append through
//! `StateTransaction` separately. See SPEC v2 §7.2 / ARCHITECTURE_v2 §7.2.
//!
//! [`RunLeaseGuard`] is the RAII helper runners use in practice. It
//! acquires on construction and releases on `Drop`, so a panicking or
//! cancelled dispatch still clears the lease. Callers that want a
//! defined release point (e.g. inside an event-emitting transaction)
//! should consume the guard with [`RunLeaseGuard::release`] before
//! it falls out of scope.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use thiserror::Error;
use tokio::sync::Mutex;

use crate::blocker::RunRef;
use crate::lease::LeaseOwner;
use std::collections::HashMap;

/// Outcome of [`RunLeaseStore::acquire`].
///
/// Mirrors `symphony_state::LeaseAcquisition` so the SQLite adapter is
/// a thin shim, but lives in the kernel so runners never depend on the
/// state crate directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseAcquireOutcome {
    /// Lease acquired (or refreshed by the same owner).
    Acquired,
    /// Another owner holds an unexpired lease.
    Contended {
        /// Canonical [`LeaseOwner`] string of the current holder.
        holder: String,
        /// RFC3339 string of the contending lease's expiration.
        expires_at: String,
    },
    /// The run row does not exist in the durable store.
    NotFound,
}

impl LeaseAcquireOutcome {
    /// True when the caller now owns the lease.
    pub fn is_acquired(&self) -> bool {
        matches!(self, Self::Acquired)
    }
}

/// Errors a lease-store backend may surface.
///
/// The kernel only models the broad shape; concrete adapters (SQLite,
/// in-memory) flatten their specific errors into [`Self::Backend`] with
/// a human-readable message so runners can log and park without
/// re-implementing per-backend error matching.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RunLeaseError {
    /// Underlying store reported an error (I/O, lock, encoding).
    #[error("lease store backend error: {0}")]
    Backend(String),
}

/// Narrow async lease store the production runners depend on.
///
/// The kernel only requires `acquire` + `release`. Heartbeat/renewal is
/// expressed through repeated `acquire` calls by the same owner (which
/// the SQLite adapter accepts as an idempotent refresh).
#[async_trait]
pub trait RunLeaseStore: Send + Sync {
    /// Atomically claim the lease on `run_id` for `owner` until
    /// `expires_at`. `now` is supplied explicitly so callers can keep
    /// clock semantics consistent with composed event timestamps.
    async fn acquire(
        &self,
        run_id: RunRef,
        owner: &LeaseOwner,
        expires_at: &str,
        now: &str,
    ) -> Result<LeaseAcquireOutcome, RunLeaseError>;

    /// Clear the lease columns on `run_id`.
    ///
    /// Returns `Ok(true)` when the run row exists (regardless of whether
    /// it previously held a lease) and `Ok(false)` when the run row is
    /// missing. The kernel calls this exactly once per run, on terminal
    /// completion.
    async fn release(&self, run_id: RunRef) -> Result<bool, RunLeaseError>;
}

/// RAII guard that releases the lease on drop unless explicitly consumed.
///
/// Use [`RunLeaseGuard::acquire`] at the top of a runner dispatch and
/// either consume the guard with [`RunLeaseGuard::release`] at the
/// terminal point, or let it drop on panic/early-return so the lease is
/// still cleared.
///
/// Drop-time release schedules a best-effort `release` on the current
/// tokio runtime. Callers that need observable release semantics (tests,
/// event-emitting transactions) should call [`Self::release`] directly.
pub struct RunLeaseGuard {
    store: Arc<dyn RunLeaseStore>,
    run_id: RunRef,
    released: AtomicBool,
}

impl std::fmt::Debug for RunLeaseGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunLeaseGuard")
            .field("run_id", &self.run_id)
            .field("released", &self.released.load(Ordering::SeqCst))
            .finish()
    }
}

impl RunLeaseGuard {
    /// Attempt to acquire the lease and wrap the result in a guard.
    ///
    /// Returns:
    /// * `Ok(Ok(guard))` on successful acquisition.
    /// * `Ok(Err(outcome))` when the store rejected the request
    ///   (`Contended` or `NotFound`); the caller can park or drop the
    ///   dispatch.
    /// * `Err(_)` when the backend itself errored.
    pub async fn acquire(
        store: Arc<dyn RunLeaseStore>,
        run_id: RunRef,
        owner: &LeaseOwner,
        expires_at: &str,
        now: &str,
    ) -> Result<Result<Self, LeaseAcquireOutcome>, RunLeaseError> {
        match store.acquire(run_id, owner, expires_at, now).await? {
            LeaseAcquireOutcome::Acquired => Ok(Ok(Self {
                store,
                run_id,
                released: AtomicBool::new(false),
            })),
            other => Ok(Err(other)),
        }
    }

    /// Run row this guard owns the lease on.
    pub fn run_id(&self) -> RunRef {
        self.run_id
    }

    /// Explicitly release the lease, consuming the guard.
    ///
    /// Returns the underlying [`RunLeaseStore::release`] outcome.
    pub async fn release(self) -> Result<bool, RunLeaseError> {
        // Mark released first so Drop becomes a no-op even if release
        // returns an error; the caller saw the failure explicitly.
        self.released.store(true, Ordering::SeqCst);
        self.store.release(self.run_id).await
    }
}

impl Drop for RunLeaseGuard {
    fn drop(&mut self) {
        if self.released.swap(true, Ordering::SeqCst) {
            return;
        }
        let store = Arc::clone(&self.store);
        let run_id = self.run_id;
        // Best-effort: the runtime may already be shutting down, in
        // which case the spawn fails silently and the SQLite adapter's
        // `find_expired_leases` reaper picks the lease up after TTL.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = store.release(run_id).await;
            });
        }
    }
}

/// Deterministic in-memory [`RunLeaseStore`] for tests.
///
/// Backed by a `tokio::sync::Mutex<HashMap>` so behaviour matches the
/// async trait without timer drift. RFC3339 strings are compared
/// lexicographically (the `runs.lease_expires_at` column uses the same
/// canonical timestamp format throughout the kernel).
#[derive(Debug, Default)]
pub struct InMemoryRunLeaseStore {
    inner: Mutex<InMemoryState>,
}

#[derive(Debug, Default)]
struct InMemoryState {
    /// `Some` when a row exists, even if no lease is currently held.
    rows: HashMap<RunRef, Option<LeaseEntry>>,
    /// When `true`, every `acquire`/`release` call returns
    /// [`RunLeaseError::Backend`] so tests can drive the error path.
    fail_with: Option<String>,
    release_calls: Vec<RunRef>,
}

#[derive(Debug, Clone)]
struct LeaseEntry {
    holder: String,
    expires_at: String,
}

impl InMemoryRunLeaseStore {
    /// Empty store with no registered run rows.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `run_id` as an existing run row so `acquire` may return
    /// [`LeaseAcquireOutcome::Acquired`] / `Contended` instead of
    /// `NotFound`.
    pub async fn register_run(&self, run_id: RunRef) {
        let mut state = self.inner.lock().await;
        state.rows.entry(run_id).or_insert(None);
    }

    /// Force the next call to return [`RunLeaseError::Backend`] with `msg`.
    pub async fn fail_next(&self, msg: impl Into<String>) {
        self.inner.lock().await.fail_with = Some(msg.into());
    }

    /// Snapshot the current `(holder, expires_at)` for `run_id`.
    pub async fn snapshot(&self, run_id: RunRef) -> Option<(String, String)> {
        self.inner
            .lock()
            .await
            .rows
            .get(&run_id)
            .and_then(|slot| slot.clone())
            .map(|entry| (entry.holder, entry.expires_at))
    }

    /// All run ids `release` was invoked on, in call order.
    pub async fn release_calls(&self) -> Vec<RunRef> {
        self.inner.lock().await.release_calls.clone()
    }
}

#[async_trait]
impl RunLeaseStore for InMemoryRunLeaseStore {
    async fn acquire(
        &self,
        run_id: RunRef,
        owner: &LeaseOwner,
        expires_at: &str,
        now: &str,
    ) -> Result<LeaseAcquireOutcome, RunLeaseError> {
        let mut state = self.inner.lock().await;
        if let Some(msg) = state.fail_with.take() {
            return Err(RunLeaseError::Backend(msg));
        }
        let canonical = owner.to_string();
        let slot = match state.rows.get_mut(&run_id) {
            Some(slot) => slot,
            None => return Ok(LeaseAcquireOutcome::NotFound),
        };
        match slot {
            None => {
                *slot = Some(LeaseEntry {
                    holder: canonical,
                    expires_at: expires_at.to_string(),
                });
                Ok(LeaseAcquireOutcome::Acquired)
            }
            Some(existing) if existing.holder == canonical => {
                existing.expires_at = expires_at.to_string();
                Ok(LeaseAcquireOutcome::Acquired)
            }
            Some(existing) if existing.expires_at.as_str() < now => {
                *slot = Some(LeaseEntry {
                    holder: canonical,
                    expires_at: expires_at.to_string(),
                });
                Ok(LeaseAcquireOutcome::Acquired)
            }
            Some(existing) => Ok(LeaseAcquireOutcome::Contended {
                holder: existing.holder.clone(),
                expires_at: existing.expires_at.clone(),
            }),
        }
    }

    async fn release(&self, run_id: RunRef) -> Result<bool, RunLeaseError> {
        let mut state = self.inner.lock().await;
        if let Some(msg) = state.fail_with.take() {
            return Err(RunLeaseError::Backend(msg));
        }
        state.release_calls.push(run_id);
        match state.rows.get_mut(&run_id) {
            Some(slot) => {
                *slot = None;
                Ok(true)
            }
            None => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner(slot: u32) -> LeaseOwner {
        LeaseOwner::new("host", "scheduler-1", slot).unwrap()
    }

    #[tokio::test]
    async fn acquire_returns_not_found_for_unregistered_run() {
        let store = InMemoryRunLeaseStore::new();
        let outcome = store
            .acquire(
                RunRef::new(1),
                &owner(0),
                "2026-01-01T00:01:00Z",
                "2026-01-01T00:00:00Z",
            )
            .await
            .unwrap();
        assert_eq!(outcome, LeaseAcquireOutcome::NotFound);
    }

    #[tokio::test]
    async fn acquire_succeeds_when_run_is_vacant() {
        let store = InMemoryRunLeaseStore::new();
        store.register_run(RunRef::new(1)).await;
        let outcome = store
            .acquire(
                RunRef::new(1),
                &owner(0),
                "2026-01-01T00:01:00Z",
                "2026-01-01T00:00:00Z",
            )
            .await
            .unwrap();
        assert_eq!(outcome, LeaseAcquireOutcome::Acquired);
        let snap = store.snapshot(RunRef::new(1)).await.unwrap();
        assert_eq!(snap.0, owner(0).to_string());
        assert_eq!(snap.1, "2026-01-01T00:01:00Z");
    }

    #[tokio::test]
    async fn acquire_is_idempotent_for_same_owner() {
        let store = InMemoryRunLeaseStore::new();
        store.register_run(RunRef::new(1)).await;
        let _ = store
            .acquire(
                RunRef::new(1),
                &owner(0),
                "2026-01-01T00:01:00Z",
                "2026-01-01T00:00:00Z",
            )
            .await
            .unwrap();
        let outcome = store
            .acquire(
                RunRef::new(1),
                &owner(0),
                "2026-01-01T00:02:00Z",
                "2026-01-01T00:00:30Z",
            )
            .await
            .unwrap();
        assert_eq!(outcome, LeaseAcquireOutcome::Acquired);
        // Renewal extended the expiration.
        assert_eq!(
            store.snapshot(RunRef::new(1)).await.unwrap().1,
            "2026-01-01T00:02:00Z"
        );
    }

    #[tokio::test]
    async fn acquire_reports_contention_for_unexpired_other_owner() {
        let store = InMemoryRunLeaseStore::new();
        store.register_run(RunRef::new(1)).await;
        let _ = store
            .acquire(
                RunRef::new(1),
                &owner(0),
                "2026-01-01T00:05:00Z",
                "2026-01-01T00:00:00Z",
            )
            .await
            .unwrap();
        let outcome = store
            .acquire(
                RunRef::new(1),
                &owner(1),
                "2026-01-01T00:06:00Z",
                "2026-01-01T00:00:30Z",
            )
            .await
            .unwrap();
        match outcome {
            LeaseAcquireOutcome::Contended { holder, expires_at } => {
                assert_eq!(holder, owner(0).to_string());
                assert_eq!(expires_at, "2026-01-01T00:05:00Z");
            }
            other => panic!("expected Contended, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn acquire_takes_over_expired_lease() {
        let store = InMemoryRunLeaseStore::new();
        store.register_run(RunRef::new(1)).await;
        let _ = store
            .acquire(
                RunRef::new(1),
                &owner(0),
                "2026-01-01T00:01:00Z",
                "2026-01-01T00:00:00Z",
            )
            .await
            .unwrap();
        let outcome = store
            .acquire(
                RunRef::new(1),
                &owner(1),
                "2026-01-01T00:10:00Z",
                "2026-01-01T00:05:00Z",
            )
            .await
            .unwrap();
        assert_eq!(outcome, LeaseAcquireOutcome::Acquired);
        assert_eq!(
            store.snapshot(RunRef::new(1)).await.unwrap().0,
            owner(1).to_string()
        );
    }

    #[tokio::test]
    async fn release_clears_lease_and_reports_run_existence() {
        let store = InMemoryRunLeaseStore::new();
        store.register_run(RunRef::new(1)).await;
        let _ = store
            .acquire(
                RunRef::new(1),
                &owner(0),
                "2026-01-01T00:01:00Z",
                "2026-01-01T00:00:00Z",
            )
            .await
            .unwrap();
        assert!(store.release(RunRef::new(1)).await.unwrap());
        assert!(store.snapshot(RunRef::new(1)).await.is_none());
        assert!(!store.release(RunRef::new(2)).await.unwrap());
    }

    #[tokio::test]
    async fn backend_error_propagates() {
        let store = InMemoryRunLeaseStore::new();
        store.fail_next("disk full").await;
        let err = store
            .acquire(
                RunRef::new(1),
                &owner(0),
                "2026-01-01T00:01:00Z",
                "2026-01-01T00:00:00Z",
            )
            .await
            .unwrap_err();
        assert!(matches!(err, RunLeaseError::Backend(msg) if msg == "disk full"));
    }

    #[tokio::test]
    async fn guard_acquires_on_dispatch_and_releases_explicitly() {
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(1)).await;

        let guard = RunLeaseGuard::acquire(
            store.clone(),
            RunRef::new(1),
            &owner(0),
            "2026-01-01T00:01:00Z",
            "2026-01-01T00:00:00Z",
        )
        .await
        .unwrap()
        .expect("acquire should succeed against vacant run");

        assert_eq!(guard.run_id(), RunRef::new(1));
        assert!(store.snapshot(RunRef::new(1)).await.is_some());

        let released = guard.release().await.unwrap();
        assert!(released);
        assert!(store.snapshot(RunRef::new(1)).await.is_none());
        assert_eq!(store.release_calls().await, vec![RunRef::new(1)]);
    }

    #[tokio::test]
    async fn guard_surfaces_contention_without_creating_guard() {
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(1)).await;
        let _ = store
            .acquire(
                RunRef::new(1),
                &owner(0),
                "2026-01-01T00:05:00Z",
                "2026-01-01T00:00:00Z",
            )
            .await
            .unwrap();

        let attempt = RunLeaseGuard::acquire(
            store.clone(),
            RunRef::new(1),
            &owner(1),
            "2026-01-01T00:06:00Z",
            "2026-01-01T00:00:30Z",
        )
        .await
        .unwrap();

        match attempt {
            Err(LeaseAcquireOutcome::Contended { holder, .. }) => {
                assert_eq!(holder, owner(0).to_string());
            }
            other => panic!("expected Contended, got {other:?}"),
        }
        // Original holder is untouched.
        assert_eq!(
            store.snapshot(RunRef::new(1)).await.unwrap().0,
            owner(0).to_string()
        );
        assert!(store.release_calls().await.is_empty());
    }

    #[tokio::test]
    async fn guard_drop_releases_lease_when_not_consumed() {
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(1)).await;

        {
            let _guard = RunLeaseGuard::acquire(
                store.clone(),
                RunRef::new(1),
                &owner(0),
                "2026-01-01T00:01:00Z",
                "2026-01-01T00:00:00Z",
            )
            .await
            .unwrap()
            .expect("acquire should succeed");
            assert!(store.snapshot(RunRef::new(1)).await.is_some());
            // Guard drops here.
        }

        // Drop schedules the release on the runtime; yield until it runs.
        for _ in 0..32 {
            if store.snapshot(RunRef::new(1)).await.is_none() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(store.snapshot(RunRef::new(1)).await.is_none());
        assert_eq!(store.release_calls().await, vec![RunRef::new(1)]);
    }

    #[tokio::test]
    async fn guard_release_is_idempotent_against_drop() {
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(1)).await;

        let guard = RunLeaseGuard::acquire(
            store.clone(),
            RunRef::new(1),
            &owner(0),
            "2026-01-01T00:01:00Z",
            "2026-01-01T00:00:00Z",
        )
        .await
        .unwrap()
        .expect("acquire should succeed");

        guard.release().await.unwrap();
        // Yield to give any (incorrectly-scheduled) drop release a
        // chance to run.
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        // Exactly one release was emitted.
        assert_eq!(store.release_calls().await, vec![RunRef::new(1)]);
    }
}
