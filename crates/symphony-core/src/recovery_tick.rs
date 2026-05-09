//! Recovery [`QueueTick`] — reconciles expired leases and orphaned
//! workspace claims on each cadence (SPEC v2 §5.16 / ARCHITECTURE v2
//! §7.2).
//!
//! Phase 11 of the v2 checklist replaces the flat poll loop with a fan
//! of logical queues. The recovery tick consumes two upstream signals
//! from durable state:
//!
//! * **Expired leases** — rows in `runs` with
//!   `lease_expires_at IS NOT NULL` whose timestamp is in the past.
//!   These are the runs that were in flight when the previous
//!   orchestrator process died; the kernel is expected to clear lease
//!   columns on terminal status, so anything still leased past `now` is
//!   recoverable work.
//! * **Orphaned workspace claims** — rows in `workspace_claims` that
//!   are no longer referenced by an active run. Common when an agent
//!   crashes after claiming a workspace but before recording a run, or
//!   when a `runs` row has been purged but the claim row outlived it.
//!
//! Each tick emits one [`RecoveryDispatchRequest`] per surfaced
//! candidate. The downstream recovery handler is responsible for
//! actually clearing the lease, releasing the workspace, and (where
//! applicable) re-routing the work item back through intake. The tick's
//! job is to *surface* the reconciliation work, not to perform it. This
//! is intentionally separate from the operator `recovery` command,
//! which performs a one-shot full reconciliation; the tick fires every
//! cadence so transient mid-run crashes do not require operator
//! intervention.
//!
//! Core must not depend on `symphony-state`, so the upstream view is
//! abstracted through the [`RecoveryQueueSource`] trait. The
//! composition root will eventually supply an adapter that selects
//! expired-lease rows and orphaned claim rows from SQLite; tests in
//! this module use a deterministic in-memory fake.
//!
//! Claim semantics: a candidate emitted in tick *N* is recorded in the
//! internal claim set so it does not re-emit on tick *N+1* while the
//! source still surfaces it. When the source stops returning a
//! candidate — because the recovery handler cleared the lease or
//! released the claim — the tick prunes its claim entry on the next
//! tick.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::logical_queue::{LogicalQueue, QueueTickOutcome};
use crate::queue_tick::{QueueTick, QueueTickCadence};
use crate::work_item::WorkItemId;

/// Durable run id surfaced by the recovery tick. Stable claim key for
/// expired leases and the `runs.id` primary key in the v2 schema.
///
/// Defined locally in core (rather than reusing `symphony-state::RunId`)
/// so the kernel can describe expired leases without taking a state
/// dependency. Adapters convert at the boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RecoveryRunId(i64);

impl RecoveryRunId {
    /// Wrap a raw row id.
    pub fn new(id: i64) -> Self {
        Self(id)
    }

    /// Underlying raw id, primarily for state-crate adapters.
    pub fn get(self) -> i64 {
        self.0
    }
}

/// Durable workspace-claim id surfaced by the recovery tick. Stable
/// claim key for orphaned workspace claims and the `workspace_claims.id`
/// primary key in the v2 schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RecoveryWorkspaceClaimId(i64);

impl RecoveryWorkspaceClaimId {
    /// Wrap a raw row id.
    pub fn new(id: i64) -> Self {
        Self(id)
    }

    /// Underlying raw id, primarily for state-crate adapters.
    pub fn get(self) -> i64 {
        self.0
    }
}

/// One expired-lease candidate surfaced by a [`RecoveryQueueSource`].
///
/// Minimal projection of a `runs` row whose `lease_expires_at` is in the
/// past as of "now": enough for the recovery handler to clear the lease
/// and re-queue the work item without re-querying state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpiredLeaseCandidate {
    /// Durable run id. Stable claim key.
    pub run_id: RecoveryRunId,
    /// Work item the leased run was operating on.
    pub work_item_id: WorkItemId,
    /// Lease holder identifier as recorded in `runs.lease_owner`.
    pub lease_owner: String,
    /// RFC3339 expiration timestamp captured from the row at observe
    /// time. Carried through so the recovery handler can log how stale
    /// the lease was without re-reading state.
    pub lease_expires_at: String,
    /// Foreign key into `workspace_claims`, if the run had claimed one.
    pub workspace_claim_id: Option<RecoveryWorkspaceClaimId>,
}

/// One orphaned-workspace-claim candidate surfaced by a
/// [`RecoveryQueueSource`].
///
/// Minimal projection of a `workspace_claims` row that no longer has a
/// live run referencing it. The recovery handler is responsible for
/// running the configured cleanup policy against the on-disk path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrphanedWorkspaceClaimCandidate {
    /// Durable claim id. Stable claim key.
    pub claim_id: RecoveryWorkspaceClaimId,
    /// Work item that originally requested the claim.
    pub work_item_id: WorkItemId,
    /// On-disk path the claim allocated.
    pub path: String,
    /// `claim_status` column verbatim. Free text because workspace
    /// strategies are workflow-defined; the handler decides how to act
    /// per status.
    pub claim_status: String,
}

/// One dispatch request emitted by the recovery queue tick.
///
/// Plain serializable data so a future scheduler can persist and replay
/// it on restart. The variants mirror the two upstream signals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecoveryDispatchRequest {
    /// Lease expired — recovery handler should clear the lease columns
    /// and re-route the work item.
    ExpiredLease {
        /// Durable run id.
        run_id: RecoveryRunId,
        /// Work item the leased run was operating on.
        work_item_id: WorkItemId,
        /// Lease holder.
        lease_owner: String,
        /// RFC3339 lease expiration timestamp.
        lease_expires_at: String,
        /// Claimed workspace, if any.
        workspace_claim_id: Option<RecoveryWorkspaceClaimId>,
    },
    /// Workspace claim no longer referenced by an active run.
    OrphanedWorkspaceClaim {
        /// Durable claim id.
        claim_id: RecoveryWorkspaceClaimId,
        /// Work item that requested the claim.
        work_item_id: WorkItemId,
        /// On-disk path.
        path: String,
        /// Claim status verbatim.
        claim_status: String,
    },
}

/// Errors a [`RecoveryQueueSource`] may surface to the tick.
///
/// Kept opaque on purpose: the tick only counts errors, never branches
/// on them.
#[derive(Debug, thiserror::Error)]
pub enum RecoveryQueueError {
    /// Underlying source failed (state DB error, adapter error, …).
    #[error("recovery queue source error: {0}")]
    Source(String),
}

/// Read-only view over the recovery signals, abstracted so core does
/// not depend on `symphony-state`.
///
/// The source decides what counts as "expired" — typically by comparing
/// `lease_expires_at` to a clock — so the tick stays free of time
/// plumbing. Both methods are called once per tick; failures in either
/// surface as a single tick error and the other signal is still
/// processed when possible.
pub trait RecoveryQueueSource: Send + Sync {
    /// Return every run whose lease has expired as of the source's
    /// notion of "now", in FIFO order (oldest expiration first).
    fn list_expired_leases(&self) -> Result<Vec<ExpiredLeaseCandidate>, RecoveryQueueError>;

    /// Return every workspace claim that no longer has a live owning
    /// run, in FIFO order by id.
    fn list_orphaned_workspace_claims(
        &self,
    ) -> Result<Vec<OrphanedWorkspaceClaimCandidate>, RecoveryQueueError>;
}

/// Shared FIFO of pending [`RecoveryDispatchRequest`]s.
#[derive(Default, Debug)]
pub struct RecoveryDispatchQueue {
    inner: Mutex<Vec<RecoveryDispatchRequest>>,
}

impl RecoveryDispatchQueue {
    /// Construct an empty queue.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of pending dispatch requests.
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .expect("recovery dispatch queue mutex poisoned")
            .len()
    }

    /// True iff no pending requests.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Snapshot the pending requests without removing them.
    pub fn snapshot(&self) -> Vec<RecoveryDispatchRequest> {
        self.inner
            .lock()
            .expect("recovery dispatch queue mutex poisoned")
            .clone()
    }

    /// Drain every pending request in FIFO order.
    pub fn drain(&self) -> Vec<RecoveryDispatchRequest> {
        std::mem::take(
            &mut *self
                .inner
                .lock()
                .expect("recovery dispatch queue mutex poisoned"),
        )
    }

    pub(crate) fn enqueue(&self, req: RecoveryDispatchRequest) {
        self.inner
            .lock()
            .expect("recovery dispatch queue mutex poisoned")
            .push(req);
    }
}

/// Stable claim key spanning both upstream signals so the tick keeps a
/// single de-dup set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum RecoveryClaimKey {
    ExpiredLease(RecoveryRunId),
    OrphanedClaim(RecoveryWorkspaceClaimId),
}

/// Recovery queue tick.
///
/// One tick = one snapshot of each upstream signal followed by FIFO
/// emission of fresh candidates. Bounded work, no awaits beyond the
/// trait signature (the source calls are synchronous).
pub struct RecoveryQueueTick {
    source: Arc<dyn RecoveryQueueSource>,
    queue: Arc<RecoveryDispatchQueue>,
    cadence: QueueTickCadence,
    claimed: Mutex<HashSet<RecoveryClaimKey>>,
}

impl RecoveryQueueTick {
    /// Construct a recovery tick.
    pub fn new(
        source: Arc<dyn RecoveryQueueSource>,
        queue: Arc<RecoveryDispatchQueue>,
        cadence: QueueTickCadence,
    ) -> Self {
        Self {
            source,
            queue,
            cadence,
            claimed: Mutex::new(HashSet::new()),
        }
    }

    /// Borrow the shared dispatch queue.
    pub fn dispatch_queue(&self) -> &Arc<RecoveryDispatchQueue> {
        &self.queue
    }

    /// Number of candidates currently claimed across both signals.
    pub fn claimed_count(&self) -> usize {
        self.claimed
            .lock()
            .expect("recovery claim set mutex poisoned")
            .len()
    }
}

#[async_trait]
impl QueueTick for RecoveryQueueTick {
    fn queue(&self) -> LogicalQueue {
        LogicalQueue::Recovery
    }

    fn cadence(&self) -> QueueTickCadence {
        self.cadence
    }

    async fn tick(&mut self) -> QueueTickOutcome {
        let mut errors = 0usize;

        let leases = match self.source.list_expired_leases() {
            Ok(v) => v,
            Err(_) => {
                errors += 1;
                Vec::new()
            }
        };
        let orphans = match self.source.list_orphaned_workspace_claims() {
            Ok(v) => v,
            Err(_) => {
                errors += 1;
                Vec::new()
            }
        };

        let considered = leases.len() + orphans.len();

        // Prune claims that no source still surfaces — typically because
        // the recovery handler cleared the lease / released the claim.
        let active: HashSet<RecoveryClaimKey> = leases
            .iter()
            .map(|l| RecoveryClaimKey::ExpiredLease(l.run_id))
            .chain(
                orphans
                    .iter()
                    .map(|o| RecoveryClaimKey::OrphanedClaim(o.claim_id)),
            )
            .collect();
        {
            let mut claimed = self
                .claimed
                .lock()
                .expect("recovery claim set mutex poisoned");
            claimed.retain(|k| active.contains(k));
        }

        let mut processed = 0usize;
        let mut deferred = 0usize;

        for l in leases {
            let key = RecoveryClaimKey::ExpiredLease(l.run_id);
            let already = {
                let claimed = self
                    .claimed
                    .lock()
                    .expect("recovery claim set mutex poisoned");
                claimed.contains(&key)
            };
            if already {
                deferred += 1;
                continue;
            }
            self.queue.enqueue(RecoveryDispatchRequest::ExpiredLease {
                run_id: l.run_id,
                work_item_id: l.work_item_id,
                lease_owner: l.lease_owner,
                lease_expires_at: l.lease_expires_at,
                workspace_claim_id: l.workspace_claim_id,
            });
            self.claimed
                .lock()
                .expect("recovery claim set mutex poisoned")
                .insert(key);
            processed += 1;
        }

        for o in orphans {
            let key = RecoveryClaimKey::OrphanedClaim(o.claim_id);
            let already = {
                let claimed = self
                    .claimed
                    .lock()
                    .expect("recovery claim set mutex poisoned");
                claimed.contains(&key)
            };
            if already {
                deferred += 1;
                continue;
            }
            self.queue
                .enqueue(RecoveryDispatchRequest::OrphanedWorkspaceClaim {
                    claim_id: o.claim_id,
                    work_item_id: o.work_item_id,
                    path: o.path,
                    claim_status: o.claim_status,
                });
            self.claimed
                .lock()
                .expect("recovery claim set mutex poisoned")
                .insert(key);
            processed += 1;
        }

        QueueTickOutcome {
            queue: LogicalQueue::Recovery,
            considered,
            processed,
            deferred,
            errors,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue_tick::run_queue_tick_n;
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;

    #[derive(Default)]
    struct FakeSource {
        leases: StdMutex<Vec<ExpiredLeaseCandidate>>,
        orphans: StdMutex<Vec<OrphanedWorkspaceClaimCandidate>>,
        fail_leases: StdMutex<bool>,
        fail_orphans: StdMutex<bool>,
    }

    impl FakeSource {
        fn new() -> Self {
            Self::default()
        }

        fn add_lease(&self, c: ExpiredLeaseCandidate) {
            self.leases.lock().unwrap().push(c);
        }

        fn add_orphan(&self, c: OrphanedWorkspaceClaimCandidate) {
            self.orphans.lock().unwrap().push(c);
        }

        fn clear_lease(&self, id: RecoveryRunId) {
            self.leases.lock().unwrap().retain(|l| l.run_id != id);
        }

        fn clear_orphan(&self, id: RecoveryWorkspaceClaimId) {
            self.orphans.lock().unwrap().retain(|o| o.claim_id != id);
        }

        fn arm_lease_failure(&self) {
            *self.fail_leases.lock().unwrap() = true;
        }

        fn arm_orphan_failure(&self) {
            *self.fail_orphans.lock().unwrap() = true;
        }
    }

    impl RecoveryQueueSource for FakeSource {
        fn list_expired_leases(&self) -> Result<Vec<ExpiredLeaseCandidate>, RecoveryQueueError> {
            let mut fail = self.fail_leases.lock().unwrap();
            if *fail {
                *fail = false;
                return Err(RecoveryQueueError::Source("scripted lease failure".into()));
            }
            Ok(self.leases.lock().unwrap().clone())
        }

        fn list_orphaned_workspace_claims(
            &self,
        ) -> Result<Vec<OrphanedWorkspaceClaimCandidate>, RecoveryQueueError> {
            let mut fail = self.fail_orphans.lock().unwrap();
            if *fail {
                *fail = false;
                return Err(RecoveryQueueError::Source("scripted orphan failure".into()));
            }
            Ok(self.orphans.lock().unwrap().clone())
        }
    }

    fn cadence() -> QueueTickCadence {
        QueueTickCadence::fixed(Duration::from_millis(1))
    }

    fn lease(id: i64, work: i64, claim: Option<i64>) -> ExpiredLeaseCandidate {
        ExpiredLeaseCandidate {
            run_id: RecoveryRunId::new(id),
            work_item_id: WorkItemId::new(work),
            lease_owner: format!("worker-{id}"),
            lease_expires_at: "2026-05-08T00:00:00Z".into(),
            workspace_claim_id: claim.map(RecoveryWorkspaceClaimId::new),
        }
    }

    fn orphan(id: i64, work: i64) -> OrphanedWorkspaceClaimCandidate {
        OrphanedWorkspaceClaimCandidate {
            claim_id: RecoveryWorkspaceClaimId::new(id),
            work_item_id: WorkItemId::new(work),
            path: format!("/tmp/symphony/wt-{id}"),
            claim_status: "active".into(),
        }
    }

    fn build_tick() -> (
        RecoveryQueueTick,
        Arc<FakeSource>,
        Arc<RecoveryDispatchQueue>,
    ) {
        let src = Arc::new(FakeSource::new());
        let q = Arc::new(RecoveryDispatchQueue::new());
        let tick = RecoveryQueueTick::new(
            src.clone() as Arc<dyn RecoveryQueueSource>,
            q.clone(),
            cadence(),
        );
        (tick, src, q)
    }

    #[tokio::test]
    async fn surfaces_expired_leases_and_orphans_in_fifo_order() {
        let (mut t, src, q) = build_tick();
        src.add_lease(lease(1, 100, Some(7)));
        src.add_lease(lease(2, 101, None));
        src.add_orphan(orphan(7, 100));
        src.add_orphan(orphan(8, 102));

        let outcome = t.tick().await;
        assert_eq!(outcome.queue, LogicalQueue::Recovery);
        assert_eq!(outcome.considered, 4);
        assert_eq!(outcome.processed, 4);
        assert_eq!(outcome.deferred, 0);
        assert_eq!(outcome.errors, 0);

        let drained = q.drain();
        assert_eq!(drained.len(), 4);
        // Leases first (FIFO), then orphans (FIFO).
        match &drained[0] {
            RecoveryDispatchRequest::ExpiredLease { run_id, .. } => {
                assert_eq!(*run_id, RecoveryRunId::new(1));
            }
            other => panic!("expected expired lease, got {other:?}"),
        }
        match &drained[1] {
            RecoveryDispatchRequest::ExpiredLease { run_id, .. } => {
                assert_eq!(*run_id, RecoveryRunId::new(2));
            }
            other => panic!("expected expired lease, got {other:?}"),
        }
        match &drained[2] {
            RecoveryDispatchRequest::OrphanedWorkspaceClaim { claim_id, .. } => {
                assert_eq!(*claim_id, RecoveryWorkspaceClaimId::new(7));
            }
            other => panic!("expected orphan, got {other:?}"),
        }
        match &drained[3] {
            RecoveryDispatchRequest::OrphanedWorkspaceClaim { claim_id, .. } => {
                assert_eq!(*claim_id, RecoveryWorkspaceClaimId::new(8));
            }
            other => panic!("expected orphan, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_signals_yield_idle_outcome() {
        let (mut t, _src, q) = build_tick();
        let outcome = t.tick().await;
        assert!(outcome.is_idle());
        assert_eq!(outcome.queue, LogicalQueue::Recovery);
        assert!(q.is_empty());
        assert_eq!(t.claimed_count(), 0);
    }

    #[tokio::test]
    async fn already_claimed_lease_is_not_re_emitted() {
        let (mut t, src, q) = build_tick();
        src.add_lease(lease(1, 100, None));

        let o1 = t.tick().await;
        assert_eq!(o1.processed, 1);
        assert_eq!(q.drain().len(), 1);

        // Re-tick: source still surfaces it → claimed → deferred.
        let o2 = t.tick().await;
        assert_eq!(o2.considered, 1);
        assert_eq!(o2.processed, 0);
        assert_eq!(o2.deferred, 1);
        assert!(q.is_empty());
        assert_eq!(t.claimed_count(), 1);
    }

    #[tokio::test]
    async fn lease_cleared_externally_prunes_claim() {
        let (mut t, src, q) = build_tick();
        src.add_lease(lease(1, 100, None));

        let o1 = t.tick().await;
        assert_eq!(o1.processed, 1);
        assert_eq!(q.drain().len(), 1);
        assert_eq!(t.claimed_count(), 1);

        // Recovery handler clears the lease — source no longer returns it.
        src.clear_lease(RecoveryRunId::new(1));
        let o2 = t.tick().await;
        assert_eq!(o2.considered, 0);
        assert_eq!(o2.processed, 0);
        assert_eq!(o2.deferred, 0);
        assert_eq!(t.claimed_count(), 0);
        assert!(q.is_empty());
    }

    #[tokio::test]
    async fn orphan_released_externally_prunes_claim() {
        let (mut t, src, q) = build_tick();
        src.add_orphan(orphan(7, 100));

        let o1 = t.tick().await;
        assert_eq!(o1.processed, 1);
        assert_eq!(q.drain().len(), 1);
        assert_eq!(t.claimed_count(), 1);

        src.clear_orphan(RecoveryWorkspaceClaimId::new(7));
        let o2 = t.tick().await;
        assert_eq!(o2.considered, 0);
        assert_eq!(t.claimed_count(), 0);
        assert!(q.is_empty());
    }

    #[tokio::test]
    async fn lease_source_failure_counts_one_error_and_still_processes_orphans() {
        let (mut t, src, q) = build_tick();
        src.add_lease(lease(1, 100, None));
        src.add_orphan(orphan(7, 100));
        src.arm_lease_failure();

        let outcome = t.tick().await;
        assert_eq!(outcome.queue, LogicalQueue::Recovery);
        assert_eq!(outcome.errors, 1);
        // Orphans still processed; only the failing signal was skipped.
        assert_eq!(outcome.considered, 1);
        assert_eq!(outcome.processed, 1);
        let drained = q.drain();
        assert_eq!(drained.len(), 1);
        match &drained[0] {
            RecoveryDispatchRequest::OrphanedWorkspaceClaim { claim_id, .. } => {
                assert_eq!(*claim_id, RecoveryWorkspaceClaimId::new(7));
            }
            other => panic!("expected orphan, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn orphan_source_failure_counts_one_error_and_still_processes_leases() {
        let (mut t, src, q) = build_tick();
        src.add_lease(lease(1, 100, None));
        src.add_orphan(orphan(7, 100));
        src.arm_orphan_failure();

        let outcome = t.tick().await;
        assert_eq!(outcome.errors, 1);
        assert_eq!(outcome.considered, 1);
        assert_eq!(outcome.processed, 1);
        let drained = q.drain();
        assert_eq!(drained.len(), 1);
        match &drained[0] {
            RecoveryDispatchRequest::ExpiredLease { run_id, .. } => {
                assert_eq!(*run_id, RecoveryRunId::new(1));
            }
            other => panic!("expected expired lease, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn both_sources_failing_reports_two_errors_and_emits_nothing() {
        let (mut t, src, q) = build_tick();
        src.add_lease(lease(1, 100, None));
        src.add_orphan(orphan(7, 100));
        src.arm_lease_failure();
        src.arm_orphan_failure();

        let outcome = t.tick().await;
        assert_eq!(outcome.errors, 2);
        assert_eq!(outcome.considered, 0);
        assert_eq!(outcome.processed, 0);
        assert!(q.is_empty());
        assert_eq!(t.claimed_count(), 0);
    }

    #[tokio::test]
    async fn drives_through_dyn_queue_tick_trait_object() {
        let (t, src, q) = build_tick();
        src.add_lease(lease(1, 100, None));
        src.add_orphan(orphan(7, 100));
        let mut boxed: Box<dyn QueueTick> = Box::new(t);
        let outcomes = run_queue_tick_n(boxed.as_mut(), 3).await;
        assert_eq!(outcomes.len(), 3);
        for o in &outcomes {
            assert_eq!(o.queue, LogicalQueue::Recovery);
        }
        assert_eq!(outcomes[0].processed, 2);
        assert_eq!(outcomes[1].processed, 0);
        assert_eq!(outcomes[1].deferred, 2);
        assert_eq!(outcomes[2].deferred, 2);
        assert_eq!(q.len(), 2);
    }

    #[tokio::test]
    async fn tick_reports_queue_and_configured_cadence() {
        let src = Arc::new(FakeSource::new());
        let q = Arc::new(RecoveryDispatchQueue::new());
        let cad = QueueTickCadence::from_millis(2_500, 100);
        let t = RecoveryQueueTick::new(src as Arc<dyn RecoveryQueueSource>, q, cad);
        assert_eq!(t.queue(), LogicalQueue::Recovery);
        assert_eq!(t.cadence(), cad);
    }

    #[tokio::test]
    async fn dispatch_queue_drain_is_fifo_and_empties() {
        let q = RecoveryDispatchQueue::new();
        q.enqueue(RecoveryDispatchRequest::ExpiredLease {
            run_id: RecoveryRunId::new(1),
            work_item_id: WorkItemId::new(100),
            lease_owner: "worker-a".into(),
            lease_expires_at: "2026-05-08T00:00:00Z".into(),
            workspace_claim_id: None,
        });
        q.enqueue(RecoveryDispatchRequest::OrphanedWorkspaceClaim {
            claim_id: RecoveryWorkspaceClaimId::new(7),
            work_item_id: WorkItemId::new(101),
            path: "/tmp/symphony/wt-7".into(),
            claim_status: "active".into(),
        });
        assert_eq!(q.len(), 2);
        let drained = q.drain();
        assert_eq!(drained.len(), 2);
        assert!(q.is_empty());
    }

    #[test]
    fn recovery_run_id_round_trips_through_json() {
        let id = RecoveryRunId::new(42);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "42");
        let back: RecoveryRunId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn recovery_workspace_claim_id_round_trips_through_json() {
        let id = RecoveryWorkspaceClaimId::new(7);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "7");
        let back: RecoveryWorkspaceClaimId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn expired_lease_candidate_round_trips_through_json() {
        let c = ExpiredLeaseCandidate {
            run_id: RecoveryRunId::new(1),
            work_item_id: WorkItemId::new(100),
            lease_owner: "worker-a".into(),
            lease_expires_at: "2026-05-08T00:00:00Z".into(),
            workspace_claim_id: Some(RecoveryWorkspaceClaimId::new(7)),
        };
        let json = serde_json::to_string(&c).unwrap();
        let back: ExpiredLeaseCandidate = serde_json::from_str(&json).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn orphaned_workspace_claim_candidate_round_trips_through_json() {
        let c = OrphanedWorkspaceClaimCandidate {
            claim_id: RecoveryWorkspaceClaimId::new(7),
            work_item_id: WorkItemId::new(100),
            path: "/tmp/symphony/wt-7".into(),
            claim_status: "active".into(),
        };
        let json = serde_json::to_string(&c).unwrap();
        let back: OrphanedWorkspaceClaimCandidate = serde_json::from_str(&json).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn dispatch_request_round_trips_through_json_with_kind_tag() {
        let r1 = RecoveryDispatchRequest::ExpiredLease {
            run_id: RecoveryRunId::new(1),
            work_item_id: WorkItemId::new(100),
            lease_owner: "worker-a".into(),
            lease_expires_at: "2026-05-08T00:00:00Z".into(),
            workspace_claim_id: None,
        };
        let json = serde_json::to_string(&r1).unwrap();
        assert!(json.contains("\"kind\":\"expired_lease\""));
        let back: RecoveryDispatchRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r1);

        let r2 = RecoveryDispatchRequest::OrphanedWorkspaceClaim {
            claim_id: RecoveryWorkspaceClaimId::new(7),
            work_item_id: WorkItemId::new(100),
            path: "/tmp/symphony/wt-7".into(),
            claim_status: "active".into(),
        };
        let json = serde_json::to_string(&r2).unwrap();
        assert!(json.contains("\"kind\":\"orphaned_workspace_claim\""));
        let back: RecoveryDispatchRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r2);
    }
}
