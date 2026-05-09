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
use crate::lease::{LeaseClock, LeaseOwner};
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

/// Heartbeat helper that tracks a held lease's `(owner, run_id, last
/// observed `expires_at`, renewal margin)` and re-acquires through
/// [`RunLeaseStore::acquire`] when the renewal window opens.
///
/// `LeaseHeartbeat` is a value object: it holds no timers and spawns no
/// tasks. The composing runner is expected to call [`Self::due`] on its
/// own scheduling cadence (a runner tick, a periodic timer, etc.) and
/// invoke [`Self::pulse`] when due. This keeps the kernel free of wall
/// clocks and lets test scenarios exercise the renewal logic
/// deterministically.
///
/// # Lex comparison contract
///
/// [`Self::due`] compares two strings produced by the same
/// [`LeaseClock`] implementation: the heartbeat's `last_expires_at`
/// (originally written by the store at acquisition) and a probe
/// computed by `clock.timestamps(renewal_margin_ms).expires_at`.
/// Heartbeat is "due" when the probe is `>=` the recorded expiry,
/// i.e. less than `renewal_margin_ms` of TTL remains. The comparison
/// relies on the clock's wire format being lex-monotonic with respect
/// to wall time — which both [`crate::lease::FixedLeaseClock`] and any
/// canonical RFC3339 producer satisfy.
///
/// # Outcome handling
///
/// [`Self::pulse`] forwards the underlying [`LeaseAcquireOutcome`]:
///
/// * [`LeaseAcquireOutcome::Acquired`] — `last_expires_at` is updated to
///   the probe's `expires_at`. Subsequent [`Self::due`] calls reflect
///   the new TTL window.
/// * [`LeaseAcquireOutcome::Contended`] — another owner holds the lease.
///   `last_expires_at` is left untouched so the runner can report or
///   relinquish. The heartbeat does not retry on its own.
/// * [`LeaseAcquireOutcome::NotFound`] — the run row was reaped. Same
///   no-state-change semantics as `Contended`.
///
/// Backend errors propagate untouched as [`RunLeaseError`].
#[derive(Clone)]
pub struct LeaseHeartbeat {
    store: Arc<dyn RunLeaseStore>,
    run_id: RunRef,
    owner: LeaseOwner,
    last_expires_at: String,
    renewal_margin_ms: u64,
}

impl std::fmt::Debug for LeaseHeartbeat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LeaseHeartbeat")
            .field("run_id", &self.run_id)
            .field("owner", &self.owner)
            .field("last_expires_at", &self.last_expires_at)
            .field("renewal_margin_ms", &self.renewal_margin_ms)
            .finish()
    }
}

impl LeaseHeartbeat {
    /// Construct a heartbeat for an already-acquired lease.
    ///
    /// `initial_expires_at` should match the value passed to the
    /// successful [`RunLeaseStore::acquire`] that established this
    /// lease, so [`Self::due`] reflects the live TTL window.
    pub fn new(
        store: Arc<dyn RunLeaseStore>,
        run_id: RunRef,
        owner: LeaseOwner,
        initial_expires_at: impl Into<String>,
        renewal_margin_ms: u64,
    ) -> Self {
        Self {
            store,
            run_id,
            owner,
            last_expires_at: initial_expires_at.into(),
            renewal_margin_ms,
        }
    }

    /// Run row this heartbeat targets.
    pub fn run_id(&self) -> RunRef {
        self.run_id
    }

    /// Owner identity used for renewals.
    pub fn owner(&self) -> &LeaseOwner {
        &self.owner
    }

    /// Last `expires_at` we successfully wrote (or were constructed with).
    pub fn last_expires_at(&self) -> &str {
        &self.last_expires_at
    }

    /// Renewal margin (ms) — how close to expiry [`Self::due`] fires.
    pub fn renewal_margin_ms(&self) -> u64 {
        self.renewal_margin_ms
    }

    /// True when the renewal window has opened: less than
    /// `renewal_margin_ms` of TTL remains as observed by `clock`.
    pub fn due(&self, clock: &dyn LeaseClock) -> bool {
        let probe = clock.timestamps(self.renewal_margin_ms);
        probe.expires_at.as_str() >= self.last_expires_at.as_str()
    }

    /// Re-acquire the lease through [`RunLeaseStore::acquire`].
    ///
    /// On [`LeaseAcquireOutcome::Acquired`] the recorded
    /// `last_expires_at` advances to `clock.timestamps(ttl_ms).expires_at`.
    /// Other outcomes leave the recorded expiry untouched and are
    /// returned to the caller for handling.
    pub async fn pulse(
        &mut self,
        clock: &dyn LeaseClock,
        ttl_ms: u64,
    ) -> Result<LeaseAcquireOutcome, RunLeaseError> {
        let ts = clock.timestamps(ttl_ms);
        let outcome = self
            .store
            .acquire(self.run_id, &self.owner, &ts.expires_at, &ts.now)
            .await?;
        if matches!(outcome, LeaseAcquireOutcome::Acquired) {
            self.last_expires_at = ts.expires_at;
        }
        Ok(outcome)
    }
}

/// Typed outcome of a single heartbeat pulse, for runners that prefer a
/// flat enum over the `Result<LeaseAcquireOutcome, RunLeaseError>` shape
/// returned by [`LeaseHeartbeat::pulse`].
///
/// This is the renewal-time analogue of [`LeaseAcquireOutcome`]: it folds
/// the backend-error case into a fourth variant so the runner can match
/// once and surface the result as a typed in-flight observation rather
/// than threading `Result` through every dispatch path. See
/// CHECKLIST_v2 Phase 11 (heartbeat-wiring scaffolding).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeartbeatPulseObservation {
    /// Lease was successfully renewed; `last_expires_at` advanced.
    Renewed,
    /// Another owner now holds the lease.
    Contended {
        /// Canonical [`LeaseOwner`] string of the current holder.
        holder: String,
        /// RFC3339 string of the contending lease's expiration.
        expires_at: String,
    },
    /// The run row no longer exists in the durable store.
    NotFound,
    /// The lease-store backend errored during the pulse.
    BackendError {
        /// Human-readable error message from the backend.
        message: String,
    },
}

impl HeartbeatPulseObservation {
    /// True when the heartbeat successfully renewed its lease.
    pub fn is_renewed(&self) -> bool {
        matches!(self, Self::Renewed)
    }
}

/// Map a [`LeaseHeartbeat::pulse`] call onto a single [`HeartbeatPulseObservation`].
///
/// This is the adapter runners call from their pulse cadence: it folds
/// the backend-error case into the typed outcome so callers can match
/// once and emit a runner observation, rather than threading
/// `Result<LeaseAcquireOutcome, RunLeaseError>` through dispatch code.
pub async fn pulse_observed(
    heartbeat: &mut LeaseHeartbeat,
    clock: &dyn LeaseClock,
    ttl_ms: u64,
) -> HeartbeatPulseObservation {
    match heartbeat.pulse(clock, ttl_ms).await {
        Ok(LeaseAcquireOutcome::Acquired) => HeartbeatPulseObservation::Renewed,
        Ok(LeaseAcquireOutcome::Contended { holder, expires_at }) => {
            HeartbeatPulseObservation::Contended { holder, expires_at }
        }
        Ok(LeaseAcquireOutcome::NotFound) => HeartbeatPulseObservation::NotFound,
        Err(RunLeaseError::Backend(message)) => HeartbeatPulseObservation::BackendError { message },
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

    // ---- LeaseHeartbeat ---------------------------------------------------

    use crate::lease::FixedLeaseClock;

    /// Establish a held lease for `owner_a` and return the heartbeat plus the
    /// store the test can reuse.
    async fn seed_heartbeat(
        clock: &FixedLeaseClock,
        ttl_ms: u64,
        renewal_margin_ms: u64,
    ) -> (Arc<InMemoryRunLeaseStore>, LeaseHeartbeat) {
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(1)).await;
        let ts = clock.timestamps(ttl_ms);
        let outcome = store
            .acquire(RunRef::new(1), &owner(0), &ts.expires_at, &ts.now)
            .await
            .unwrap();
        assert_eq!(outcome, LeaseAcquireOutcome::Acquired);
        let hb = LeaseHeartbeat::new(
            store.clone(),
            RunRef::new(1),
            owner(0),
            ts.expires_at,
            renewal_margin_ms,
        );
        (store, hb)
    }

    #[tokio::test]
    async fn heartbeat_is_not_due_when_full_ttl_remains() {
        let clock = FixedLeaseClock::new("2026-01-01T00:00:00Z");
        let (_store, hb) = seed_heartbeat(&clock, 60_000, 15_000).await;
        assert!(!hb.due(&clock));
    }

    #[tokio::test]
    async fn heartbeat_is_due_inside_renewal_window() {
        let acquire_clock = FixedLeaseClock::new("2026-01-01T00:00:00Z");
        let (_store, hb) = seed_heartbeat(&acquire_clock, 60_000, 15_000).await;
        // Advance to t=50s — only 10s remains, inside the 15s margin.
        let later = FixedLeaseClock::new("2026-01-01T00:00:50Z");
        assert!(hb.due(&later));
    }

    #[tokio::test]
    async fn heartbeat_pulse_extends_recorded_expiry_on_acquired() {
        let early = FixedLeaseClock::new("2026-01-01T00:00:00Z");
        let (store, mut hb) = seed_heartbeat(&early, 60_000, 15_000).await;
        let original_expiry = hb.last_expires_at().to_string();

        let later = FixedLeaseClock::new("2026-01-01T00:00:50Z");
        let outcome = hb.pulse(&later, 60_000).await.unwrap();
        assert_eq!(outcome, LeaseAcquireOutcome::Acquired);

        assert!(hb.last_expires_at() > original_expiry.as_str());
        // Store reflects the renewed expiry.
        let snap = store.snapshot(RunRef::new(1)).await.unwrap();
        assert_eq!(snap.0, owner(0).to_string());
        assert_eq!(snap.1, hb.last_expires_at());
    }

    #[tokio::test]
    async fn heartbeat_pulse_preserves_recorded_expiry_on_contention() {
        let acquire_clock = FixedLeaseClock::new("2026-01-01T00:00:00Z");
        let (store, mut hb) = seed_heartbeat(&acquire_clock, 60_000, 15_000).await;
        let recorded = hb.last_expires_at().to_string();

        // Forcibly take over the lease with a different owner using a
        // far-future expiry that beats the takeover-on-expiry rule.
        let intruder_ts = FixedLeaseClock::new("2030-01-01T00:00:00Z").timestamps(60_000);
        let intruder = owner(1);
        // Direct mutation through the store API: previous holder's lease
        // is unexpired, but we simulate an authority change by clearing
        // and re-acquiring via the intruder's `release` + `acquire`.
        let _ = store.release(RunRef::new(1)).await.unwrap();
        let outcome = store
            .acquire(
                RunRef::new(1),
                &intruder,
                &intruder_ts.expires_at,
                &intruder_ts.now,
            )
            .await
            .unwrap();
        assert_eq!(outcome, LeaseAcquireOutcome::Acquired);

        // Heartbeat for owner-A pulses against an unexpired owner-B lease.
        let pulse_clock = FixedLeaseClock::new("2026-01-01T00:00:30Z");
        let outcome = hb.pulse(&pulse_clock, 60_000).await.unwrap();
        match outcome {
            LeaseAcquireOutcome::Contended { holder, .. } => {
                assert_eq!(holder, owner(1).to_string());
            }
            other => panic!("expected Contended, got {other:?}"),
        }
        // Recorded expiry untouched.
        assert_eq!(hb.last_expires_at(), recorded);
    }

    #[tokio::test]
    async fn heartbeat_pulse_surfaces_not_found_without_state_change() {
        let store = Arc::new(InMemoryRunLeaseStore::new());
        // No `register_run` — the run row is absent.
        let mut hb = LeaseHeartbeat::new(
            store.clone(),
            RunRef::new(1),
            owner(0),
            "2026-01-01T00:01:00Z",
            15_000,
        );
        let recorded = hb.last_expires_at().to_string();
        let pulse_clock = FixedLeaseClock::new("2026-01-01T00:00:30Z");
        let outcome = hb.pulse(&pulse_clock, 60_000).await.unwrap();
        assert_eq!(outcome, LeaseAcquireOutcome::NotFound);
        assert_eq!(hb.last_expires_at(), recorded);
    }

    #[tokio::test]
    async fn heartbeat_pulse_propagates_backend_errors() {
        let clock = FixedLeaseClock::new("2026-01-01T00:00:00Z");
        let (store, mut hb) = seed_heartbeat(&clock, 60_000, 15_000).await;
        let recorded = hb.last_expires_at().to_string();
        store.fail_next("disk full").await;
        let err = hb.pulse(&clock, 60_000).await.unwrap_err();
        assert!(matches!(err, RunLeaseError::Backend(msg) if msg == "disk full"));
        assert_eq!(hb.last_expires_at(), recorded);
    }

    // ---- pulse_observed adapter -----------------------------------------

    #[tokio::test]
    async fn pulse_observed_returns_renewed_on_acquired() {
        let early = FixedLeaseClock::new("2026-01-01T00:00:00Z");
        let (store, mut hb) = seed_heartbeat(&early, 60_000, 15_000).await;
        let original_expiry = hb.last_expires_at().to_string();

        let later = FixedLeaseClock::new("2026-01-01T00:00:50Z");
        let observation = pulse_observed(&mut hb, &later, 60_000).await;
        assert_eq!(observation, HeartbeatPulseObservation::Renewed);
        assert!(observation.is_renewed());

        // Recorded expiry advanced and the store reflects the renewal.
        assert!(hb.last_expires_at() > original_expiry.as_str());
        let snap = store.snapshot(RunRef::new(1)).await.unwrap();
        assert_eq!(snap.1, hb.last_expires_at());
    }

    #[tokio::test]
    async fn pulse_observed_returns_contended_when_other_owner_holds_lease() {
        let acquire_clock = FixedLeaseClock::new("2026-01-01T00:00:00Z");
        let (store, mut hb) = seed_heartbeat(&acquire_clock, 60_000, 15_000).await;
        let recorded = hb.last_expires_at().to_string();

        // Hand the lease to a different owner with a far-future expiry.
        let intruder_ts = FixedLeaseClock::new("2030-01-01T00:00:00Z").timestamps(60_000);
        store.release(RunRef::new(1)).await.unwrap();
        let outcome = store
            .acquire(
                RunRef::new(1),
                &owner(1),
                &intruder_ts.expires_at,
                &intruder_ts.now,
            )
            .await
            .unwrap();
        assert_eq!(outcome, LeaseAcquireOutcome::Acquired);

        let pulse_clock = FixedLeaseClock::new("2026-01-01T00:00:30Z");
        let observation = pulse_observed(&mut hb, &pulse_clock, 60_000).await;
        match observation {
            HeartbeatPulseObservation::Contended { holder, expires_at } => {
                assert_eq!(holder, owner(1).to_string());
                assert_eq!(expires_at, intruder_ts.expires_at);
            }
            other => panic!("expected Contended, got {other:?}"),
        }
        // Recorded expiry untouched on contention.
        assert_eq!(hb.last_expires_at(), recorded);
    }

    #[tokio::test]
    async fn pulse_observed_returns_not_found_when_run_row_missing() {
        let store = Arc::new(InMemoryRunLeaseStore::new());
        // No `register_run` — the run row is absent.
        let mut hb = LeaseHeartbeat::new(
            store.clone(),
            RunRef::new(1),
            owner(0),
            "2026-01-01T00:01:00Z",
            15_000,
        );
        let recorded = hb.last_expires_at().to_string();
        let pulse_clock = FixedLeaseClock::new("2026-01-01T00:00:30Z");
        let observation = pulse_observed(&mut hb, &pulse_clock, 60_000).await;
        assert_eq!(observation, HeartbeatPulseObservation::NotFound);
        assert_eq!(hb.last_expires_at(), recorded);
    }

    #[tokio::test]
    async fn pulse_observed_folds_backend_error_into_typed_observation() {
        let clock = FixedLeaseClock::new("2026-01-01T00:00:00Z");
        let (store, mut hb) = seed_heartbeat(&clock, 60_000, 15_000).await;
        let recorded = hb.last_expires_at().to_string();
        store.fail_next("disk full").await;
        let observation = pulse_observed(&mut hb, &clock, 60_000).await;
        assert_eq!(
            observation,
            HeartbeatPulseObservation::BackendError {
                message: "disk full".to_string(),
            }
        );
        assert_eq!(hb.last_expires_at(), recorded);
    }

    #[tokio::test]
    async fn heartbeat_accessors_reflect_construction() {
        let clock = FixedLeaseClock::new("2026-01-01T00:00:00Z");
        let (_store, hb) = seed_heartbeat(&clock, 60_000, 15_000).await;
        assert_eq!(hb.run_id(), RunRef::new(1));
        assert_eq!(hb.owner(), &owner(0));
        assert_eq!(hb.renewal_margin_ms(), 15_000);
    }
}
