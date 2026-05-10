//! Follow-up approval [`QueueTick`] — drains pending approval-routed
//! follow-up proposals (SPEC v2 §4.10, §5.13 / ARCHITECTURE v2 §6.4).
//!
//! Phase 11 of the v2 checklist replaces the flat poll loop with a fan
//! of logical queues. The follow-up approval tick consumes one upstream
//! signal — durable [`crate::FollowupIssueRequest`]s in
//! [`crate::FollowupStatus::Proposed`] — and emits one downstream
//! artifact: a [`FollowupApprovalDispatchRequest`] per pending proposal,
//! claimed by [`crate::FollowupId`] so re-ticking the same view does
//! not re-emit the same proposal while it remains in `Proposed`.
//!
//! Core must not depend on `symphony-state`, so the upstream view is
//! abstracted through the [`FollowupApprovalQueueSource`] trait. The
//! composition root will eventually supply an adapter that lists
//! follow-ups whose `policy = propose_for_approval` *and* whose
//! `status = proposed`. Tests in this module use a deterministic
//! in-memory fake.
//!
//! Approve / reject lifecycle:
//!
//! * The tick *surfaces* proposals; the downstream approval handler
//!   advances each via [`crate::FollowupIssueRequest::approve`] or
//!   [`crate::FollowupIssueRequest::reject`]. Both transitions move the
//!   proposal out of [`crate::FollowupStatus::Proposed`], so the source
//!   stops returning it on the next tick. The claim set is pruned and
//!   the proposal does not re-emit.
//! * If the workflow lacks `followups.approval_role`, no proposals can
//!   reach this queue in the first place — [`crate::route_followup`]
//!   refuses with [`crate::FollowupRoutingError::MissingApprovalRole`]
//!   at routing time. The tick assumes its source is already pre-gated
//!   on that constraint and trusts the configured `approval_role`
//!   recorded on the candidate.
//!
//! Constructing the actual approval-handler workflow (UI prompt,
//! tracker comment, audit-log emission) belongs to the runner
//! downstream. The tick's job is to *surface* pending proposals.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::followup::FollowupId;
use crate::logical_queue::{LogicalQueue, QueueTickOutcome};
use crate::queue_tick::{QueueTick, QueueTickCadence};
use crate::role::RoleName;
use crate::work_item::WorkItemId;

/// One pending proposal surfaced by a [`FollowupApprovalQueueSource`].
///
/// Minimal projection of a [`crate::FollowupIssueRequest`] in
/// [`crate::FollowupStatus::Proposed`]. The shape is deliberately small:
/// the approval handler reads the durable record by id when it actually
/// builds the operator-facing view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FollowupApprovalCandidate {
    /// Durable follow-up id. Stable claim key.
    pub followup_id: FollowupId,
    /// Work item that spawned the proposal (SPEC §5.13 "relationship to
    /// current work").
    pub source_work_item: WorkItemId,
    /// Short tracker title proposed for the follow-up.
    pub title: String,
    /// True when the proposal was filed as blocking acceptance.
    pub blocking: bool,
    /// Role configured under `followups.approval_role` at the time of
    /// routing. Captured here so a later config change cannot
    /// retroactively re-route an already-queued proposal — same rule as
    /// [`crate::FollowupRouteDecision::RouteToApprovalQueue`].
    pub approval_role: RoleName,
}

/// Errors a [`FollowupApprovalQueueSource`] may surface to the tick.
///
/// Kept opaque on purpose: the tick only needs to count errors, not
/// branch on them.
#[derive(Debug, thiserror::Error)]
pub enum FollowupApprovalQueueError {
    /// Underlying source failed (state DB error, adapter error, …).
    #[error("followup approval queue source error: {0}")]
    Source(String),
}

/// Read-only view over the follow-up approval queue, abstracted so core
/// does not depend on `symphony-state`.
pub trait FollowupApprovalQueueSource: Send + Sync {
    /// Return every follow-up currently in
    /// [`crate::FollowupStatus::Proposed`] under
    /// [`crate::FollowupPolicy::ProposeForApproval`], in FIFO order.
    /// Order is preserved by the tick when emitting dispatch requests.
    fn list_pending(&self) -> Result<Vec<FollowupApprovalCandidate>, FollowupApprovalQueueError>;
}

/// One dispatch request emitted by the follow-up approval tick.
///
/// Plain serializable data so a future scheduler can persist and replay
/// it on restart. The downstream approval handler is responsible for
/// reading the full [`crate::FollowupIssueRequest`] and driving its
/// `approve`/`reject` lifecycle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FollowupApprovalDispatchRequest {
    /// Durable follow-up id. Stable claim key.
    pub followup_id: FollowupId,
    /// Source work item that spawned the proposal.
    pub source_work_item: WorkItemId,
    /// Short tracker title proposed for the follow-up.
    pub title: String,
    /// True when the proposal was filed as blocking acceptance.
    pub blocking: bool,
    /// Configured approval role.
    pub approval_role: RoleName,
    /// Durable `runs` row this dispatch will animate, when known. The
    /// follow-up approval runner uses this to acquire/release the
    /// durable lease (CHECKLIST_v2 Phase 11). `None` for legacy
    /// producers that do not reserve a run row before queueing — those
    /// dispatches bypass lease acquisition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<crate::blocker::RunRef>,
    /// Optional role key for the multi-scope concurrency gate
    /// (CHECKLIST_v2 Phase 11). When `Some`, the follow-up approval
    /// runner builds a [`crate::concurrency_gate::DispatchTriple`] from
    /// `(role, agent_profile, repository)` and gates dispatch through
    /// the configured [`crate::concurrency_gate::ConcurrencyGate`].
    /// Producers typically populate this with the configured
    /// `followups.approval_role` so approval dispatches respect that
    /// role's cap. `None` bypasses gate acquisition entirely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<RoleName>,
    /// Optional agent-profile scope key. Paired with `role` for gate
    /// acquisition; ignored when `role` is `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_profile: Option<String>,
    /// Optional repository scope key. Paired with `role` for gate
    /// acquisition; ignored when `role` is `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
}

/// Shared FIFO of pending [`FollowupApprovalDispatchRequest`]s.
#[derive(Default, Debug)]
pub struct FollowupApprovalDispatchQueue {
    inner: Mutex<Vec<FollowupApprovalDispatchRequest>>,
}

impl FollowupApprovalDispatchQueue {
    /// Construct an empty queue.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of pending dispatch requests.
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .expect("followup approval dispatch queue mutex poisoned")
            .len()
    }

    /// True iff no pending requests.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Snapshot the pending requests without removing them.
    pub fn snapshot(&self) -> Vec<FollowupApprovalDispatchRequest> {
        self.inner
            .lock()
            .expect("followup approval dispatch queue mutex poisoned")
            .clone()
    }

    /// Drain every pending request in FIFO order.
    pub fn drain(&self) -> Vec<FollowupApprovalDispatchRequest> {
        std::mem::take(
            &mut *self
                .inner
                .lock()
                .expect("followup approval dispatch queue mutex poisoned"),
        )
    }

    pub(crate) fn enqueue(&self, req: FollowupApprovalDispatchRequest) {
        self.inner
            .lock()
            .expect("followup approval dispatch queue mutex poisoned")
            .push(req);
    }
}

/// Follow-up approval queue tick.
///
/// One tick = one snapshot of the upstream
/// [`FollowupApprovalQueueSource`]. Bounded work, no awaits beyond the
/// trait signature (the source call is synchronous). Cadence and
/// fan-out belong to the multi-queue scheduler.
///
/// Claim semantics: a follow-up emitted in tick *N* is recorded in the
/// internal claim set so it does not re-emit on tick *N+1* while the
/// source still surfaces it. When the source stops returning it —
/// because an approver called
/// [`crate::FollowupIssueRequest::approve`] (status →
/// [`crate::FollowupStatus::Approved`]) or
/// [`crate::FollowupIssueRequest::reject`] (status →
/// [`crate::FollowupStatus::Rejected`]) — the claim is pruned.
pub struct FollowupApprovalQueueTick {
    source: Arc<dyn FollowupApprovalQueueSource>,
    queue: Arc<FollowupApprovalDispatchQueue>,
    cadence: QueueTickCadence,
    claimed: Mutex<HashSet<FollowupId>>,
}

impl FollowupApprovalQueueTick {
    /// Construct a follow-up approval tick.
    pub fn new(
        source: Arc<dyn FollowupApprovalQueueSource>,
        queue: Arc<FollowupApprovalDispatchQueue>,
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
    pub fn dispatch_queue(&self) -> &Arc<FollowupApprovalDispatchQueue> {
        &self.queue
    }

    /// Number of follow-ups currently claimed.
    pub fn claimed_count(&self) -> usize {
        self.claimed
            .lock()
            .expect("followup approval claim set mutex poisoned")
            .len()
    }
}

#[async_trait]
impl QueueTick for FollowupApprovalQueueTick {
    fn queue(&self) -> LogicalQueue {
        LogicalQueue::FollowupApproval
    }

    fn cadence(&self) -> QueueTickCadence {
        self.cadence
    }

    async fn tick(&mut self) -> QueueTickOutcome {
        let candidates = match self.source.list_pending() {
            Ok(c) => c,
            Err(_e) => {
                return QueueTickOutcome {
                    queue: LogicalQueue::FollowupApproval,
                    considered: 0,
                    processed: 0,
                    deferred: 0,
                    errors: 1,
                };
            }
        };
        let considered = candidates.len();

        // Prune claims for proposals the source no longer surfaces —
        // typically because an approver moved them out of `Proposed` via
        // approve or reject.
        let active_ids: HashSet<FollowupId> = candidates.iter().map(|c| c.followup_id).collect();
        {
            let mut claimed = self
                .claimed
                .lock()
                .expect("followup approval claim set mutex poisoned");
            claimed.retain(|id| active_ids.contains(id));
        }

        let mut processed = 0usize;
        let mut deferred = 0usize;

        for c in candidates {
            let already_claimed = {
                let claimed = self
                    .claimed
                    .lock()
                    .expect("followup approval claim set mutex poisoned");
                claimed.contains(&c.followup_id)
            };
            if already_claimed {
                deferred += 1;
                continue;
            }

            self.queue.enqueue(FollowupApprovalDispatchRequest {
                followup_id: c.followup_id,
                source_work_item: c.source_work_item,
                title: c.title,
                blocking: c.blocking,
                approval_role: c.approval_role,
                run_id: None,
                role: None,
                agent_profile: None,
                repository: None,
            });
            self.claimed
                .lock()
                .expect("followup approval claim set mutex poisoned")
                .insert(c.followup_id);
            processed += 1;
        }

        QueueTickOutcome {
            queue: LogicalQueue::FollowupApproval,
            considered,
            processed,
            deferred,
            errors: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocker::{BlockerOrigin, RunRef};
    use crate::followup::{FollowupIssueRequest, FollowupPolicy, FollowupStatus};
    use crate::queue_tick::run_queue_tick_n;
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;

    /// Fake source backed by a vector of durable
    /// [`FollowupIssueRequest`] records. Listing filters to
    /// `policy = ProposeForApproval` *and* `status = Proposed`, mirroring
    /// the durable view the future state-side adapter will expose.
    /// Approve/reject helpers mutate the underlying record so tests can
    /// drive the same lifecycle the real approval handler would.
    struct FakeSource {
        records: StdMutex<Vec<FollowupIssueRequest>>,
        approval_role: RoleName,
        fail_next: StdMutex<bool>,
    }

    impl FakeSource {
        fn new(approval_role: RoleName) -> Self {
            Self {
                records: StdMutex::new(Vec::new()),
                approval_role,
                fail_next: StdMutex::new(false),
            }
        }

        fn insert(&self, record: FollowupIssueRequest) {
            self.records.lock().unwrap().push(record);
        }

        fn arm_failure(&self) {
            *self.fail_next.lock().unwrap() = true;
        }

        fn approve(&self, id: FollowupId, note: &str) {
            let mut recs = self.records.lock().unwrap();
            let r = recs.iter_mut().find(|r| r.id == id).expect("missing");
            r.approve(self.approval_role.clone(), note).unwrap();
        }

        fn reject(&self, id: FollowupId, note: &str) {
            let mut recs = self.records.lock().unwrap();
            let r = recs.iter_mut().find(|r| r.id == id).expect("missing");
            r.reject(self.approval_role.clone(), note).unwrap();
        }
    }

    impl FollowupApprovalQueueSource for FakeSource {
        fn list_pending(
            &self,
        ) -> Result<Vec<FollowupApprovalCandidate>, FollowupApprovalQueueError> {
            let mut fail = self.fail_next.lock().unwrap();
            if *fail {
                *fail = false;
                return Err(FollowupApprovalQueueError::Source(
                    "scripted failure".into(),
                ));
            }
            let recs = self.records.lock().unwrap();
            Ok(recs
                .iter()
                .filter(|r| {
                    r.policy == FollowupPolicy::ProposeForApproval
                        && r.status == FollowupStatus::Proposed
                })
                .map(|r| FollowupApprovalCandidate {
                    followup_id: r.id,
                    source_work_item: r.source_work_item,
                    title: r.title.clone(),
                    blocking: r.blocking,
                    approval_role: self.approval_role.clone(),
                })
                .collect())
        }
    }

    fn cadence() -> QueueTickCadence {
        QueueTickCadence::fixed(Duration::from_millis(1))
    }

    fn proposed(id: i64, source: i64, title: &str, blocking: bool) -> FollowupIssueRequest {
        FollowupIssueRequest::try_new(
            FollowupId::new(id),
            WorkItemId::new(source),
            title,
            "tighten retries",
            "scoped to adapter",
            vec!["criterion".into()],
            blocking,
            FollowupPolicy::ProposeForApproval,
            BlockerOrigin::Run {
                run_id: RunRef::new(7),
                role: Some(RoleName::new("backend")),
            },
            true,
        )
        .unwrap()
    }

    fn direct(id: i64, source: i64, title: &str) -> FollowupIssueRequest {
        FollowupIssueRequest::try_new(
            FollowupId::new(id),
            WorkItemId::new(source),
            title,
            "missing log",
            "core poll loop",
            vec!["log emitted".into()],
            false,
            FollowupPolicy::CreateDirectly,
            BlockerOrigin::Run {
                run_id: RunRef::new(7),
                role: Some(RoleName::new("backend")),
            },
            true,
        )
        .unwrap()
    }

    fn build_tick(
        source: Arc<FakeSource>,
    ) -> (
        FollowupApprovalQueueTick,
        Arc<FakeSource>,
        Arc<FollowupApprovalDispatchQueue>,
    ) {
        let queue = Arc::new(FollowupApprovalDispatchQueue::new());
        let tick = FollowupApprovalQueueTick::new(
            source.clone() as Arc<dyn FollowupApprovalQueueSource>,
            queue.clone(),
            cadence(),
        );
        (tick, source, queue)
    }

    #[tokio::test]
    async fn pending_proposals_are_emitted_in_fifo_order() {
        let role = RoleName::new("platform_lead");
        let src = Arc::new(FakeSource::new(role.clone()));
        src.insert(proposed(1, 100, "rate limit", false));
        src.insert(proposed(2, 100, "logs", true));
        let (mut t, _src, q) = build_tick(src);

        let outcome = t.tick().await;
        assert_eq!(outcome.queue, LogicalQueue::FollowupApproval);
        assert_eq!(outcome.considered, 2);
        assert_eq!(outcome.processed, 2);
        assert_eq!(outcome.deferred, 0);
        assert_eq!(outcome.errors, 0);

        let drained = q.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].followup_id, FollowupId::new(1));
        assert_eq!(drained[0].approval_role, role);
        assert!(!drained[0].blocking);
        assert_eq!(drained[1].followup_id, FollowupId::new(2));
        assert!(drained[1].blocking);
    }

    #[tokio::test]
    async fn create_directly_followups_are_excluded_from_the_queue() {
        let role = RoleName::new("platform_lead");
        let src = Arc::new(FakeSource::new(role));
        src.insert(direct(1, 100, "direct one"));
        src.insert(proposed(2, 100, "proposal", false));
        let (mut t, _src, q) = build_tick(src);

        let outcome = t.tick().await;
        assert_eq!(outcome.considered, 1);
        assert_eq!(outcome.processed, 1);
        let drained = q.drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].followup_id, FollowupId::new(2));
    }

    #[tokio::test]
    async fn approve_path_drops_proposal_and_prunes_claim() {
        let role = RoleName::new("platform_lead");
        let src = Arc::new(FakeSource::new(role));
        src.insert(proposed(1, 100, "rate limit", false));
        let (mut t, src, q) = build_tick(src);

        let o1 = t.tick().await;
        assert_eq!(o1.processed, 1);
        let drained = q.drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].followup_id, FollowupId::new(1));
        assert_eq!(t.claimed_count(), 1);

        // Approver acts: status moves to Approved → source no longer
        // surfaces it.
        src.approve(FollowupId::new(1), "looks good");
        let o2 = t.tick().await;
        assert_eq!(o2.considered, 0);
        assert_eq!(o2.processed, 0);
        assert_eq!(o2.deferred, 0);
        assert_eq!(t.claimed_count(), 0);
        assert!(q.is_empty());
    }

    #[tokio::test]
    async fn reject_path_drops_proposal_and_prunes_claim() {
        let role = RoleName::new("platform_lead");
        let src = Arc::new(FakeSource::new(role));
        src.insert(proposed(7, 100, "out of scope", false));
        let (mut t, src, q) = build_tick(src);

        let o1 = t.tick().await;
        assert_eq!(o1.processed, 1);
        assert_eq!(q.drain().len(), 1);
        assert_eq!(t.claimed_count(), 1);

        src.reject(FollowupId::new(7), "out of scope");
        let o2 = t.tick().await;
        assert_eq!(o2.considered, 0);
        assert_eq!(o2.processed, 0);
        assert_eq!(t.claimed_count(), 0);
        assert!(q.is_empty());
    }

    #[tokio::test]
    async fn already_claimed_proposal_is_not_re_emitted() {
        let role = RoleName::new("platform_lead");
        let src = Arc::new(FakeSource::new(role));
        src.insert(proposed(1, 100, "rate limit", false));
        let (mut t, _src, q) = build_tick(src);

        let o1 = t.tick().await;
        assert_eq!(o1.processed, 1);
        assert_eq!(q.drain().len(), 1);

        // Re-tick: still in Proposed → claimed → deferred, no re-emit.
        let o2 = t.tick().await;
        assert_eq!(o2.considered, 1);
        assert_eq!(o2.processed, 0);
        assert_eq!(o2.deferred, 1);
        assert!(q.is_empty());
        assert_eq!(t.claimed_count(), 1);
    }

    #[tokio::test]
    async fn empty_queue_is_idle() {
        let role = RoleName::new("platform_lead");
        let src = Arc::new(FakeSource::new(role));
        let (mut t, _src, q) = build_tick(src);

        let outcome = t.tick().await;
        assert!(outcome.is_idle());
        assert_eq!(outcome.queue, LogicalQueue::FollowupApproval);
        assert!(q.is_empty());
        assert_eq!(t.claimed_count(), 0);
    }

    #[tokio::test]
    async fn source_error_increments_errors_and_emits_nothing() {
        let role = RoleName::new("platform_lead");
        let src = Arc::new(FakeSource::new(role));
        src.insert(proposed(1, 100, "rate limit", false));
        let (mut t, src, q) = build_tick(src);

        src.arm_failure();
        let o = t.tick().await;
        assert_eq!(o.queue, LogicalQueue::FollowupApproval);
        assert_eq!(o.errors, 1);
        assert_eq!(o.processed, 0);
        assert_eq!(o.considered, 0);
        assert!(q.is_empty());
        assert_eq!(t.claimed_count(), 0);
    }

    #[tokio::test]
    async fn drives_through_dyn_queue_tick_trait_object() {
        let role = RoleName::new("platform_lead");
        let src = Arc::new(FakeSource::new(role));
        src.insert(proposed(1, 100, "rate limit", false));
        let (t, _src, q) = build_tick(src);
        let mut boxed: Box<dyn QueueTick> = Box::new(t);
        let outcomes = run_queue_tick_n(boxed.as_mut(), 3).await;
        assert_eq!(outcomes.len(), 3);
        for o in &outcomes {
            assert_eq!(o.queue, LogicalQueue::FollowupApproval);
        }
        assert_eq!(outcomes[0].processed, 1);
        assert_eq!(outcomes[1].processed, 0);
        assert_eq!(outcomes[1].deferred, 1);
        assert_eq!(outcomes[2].deferred, 1);
        assert_eq!(q.len(), 1);
    }

    #[tokio::test]
    async fn tick_reports_queue_and_configured_cadence() {
        let role = RoleName::new("platform_lead");
        let src = Arc::new(FakeSource::new(role));
        let q = Arc::new(FollowupApprovalDispatchQueue::new());
        let cad = QueueTickCadence::from_millis(2_500, 100);
        let t = FollowupApprovalQueueTick::new(src as Arc<dyn FollowupApprovalQueueSource>, q, cad);
        assert_eq!(t.queue(), LogicalQueue::FollowupApproval);
        assert_eq!(t.cadence(), cad);
    }

    #[tokio::test]
    async fn dispatch_queue_drain_is_fifo_and_empties() {
        let q = FollowupApprovalDispatchQueue::new();
        let role = RoleName::new("platform_lead");
        q.enqueue(FollowupApprovalDispatchRequest {
            followup_id: FollowupId::new(1),
            source_work_item: WorkItemId::new(100),
            title: "first".into(),
            blocking: false,
            approval_role: role.clone(),
            run_id: None,
            role: None,
            agent_profile: None,
            repository: None,
        });
        q.enqueue(FollowupApprovalDispatchRequest {
            followup_id: FollowupId::new(2),
            source_work_item: WorkItemId::new(100),
            title: "second".into(),
            blocking: true,
            approval_role: role,
            run_id: None,
            role: None,
            agent_profile: None,
            repository: None,
        });
        assert_eq!(q.len(), 2);
        let drained = q.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].followup_id, FollowupId::new(1));
        assert_eq!(drained[1].followup_id, FollowupId::new(2));
        assert!(q.is_empty());
    }

    #[test]
    fn candidate_round_trips_through_json() {
        let c = FollowupApprovalCandidate {
            followup_id: FollowupId::new(42),
            source_work_item: WorkItemId::new(100),
            title: "rate limit".into(),
            blocking: true,
            approval_role: RoleName::new("platform_lead"),
        };
        let json = serde_json::to_string(&c).unwrap();
        let back: FollowupApprovalCandidate = serde_json::from_str(&json).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn dispatch_request_round_trips_through_json() {
        let r = FollowupApprovalDispatchRequest {
            followup_id: FollowupId::new(7),
            source_work_item: WorkItemId::new(100),
            title: "verify".into(),
            blocking: false,
            approval_role: RoleName::new("platform_lead"),
            run_id: None,
            role: None,
            agent_profile: None,
            repository: None,
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: FollowupApprovalDispatchRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn dispatch_request_round_trips_when_run_id_present() {
        let r = FollowupApprovalDispatchRequest {
            followup_id: FollowupId::new(7),
            source_work_item: WorkItemId::new(100),
            title: "verify".into(),
            blocking: false,
            approval_role: RoleName::new("platform_lead"),
            run_id: Some(crate::blocker::RunRef::new(99)),
            role: None,
            agent_profile: None,
            repository: None,
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: FollowupApprovalDispatchRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
        assert!(json.contains("run_id"));
    }

    #[test]
    fn dispatch_request_round_trips_when_scope_keys_present() {
        let r = FollowupApprovalDispatchRequest {
            followup_id: FollowupId::new(8),
            source_work_item: WorkItemId::new(100),
            title: "verify".into(),
            blocking: false,
            approval_role: RoleName::new("platform_lead"),
            run_id: None,
            role: Some(RoleName::new("platform_lead")),
            agent_profile: Some("claude".into()),
            repository: Some("acme/widgets".into()),
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: FollowupApprovalDispatchRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
        assert!(json.contains("agent_profile"));
        assert!(json.contains("repository"));
    }
}
