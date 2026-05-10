//! Specialist [`QueueTick`] — claims routable specialist work items from
//! the shared [`ActiveSetStore`] and emits dispatch requests
//! (SPEC v2 §5 / ARCHITECTURE v2 §7.1).
//!
//! Phase 11 of the v2 checklist replaces the flat poll loop with a fan
//! of logical queues. The intake tick publishes the active set into a
//! shared store; this tick is the first downstream consumer:
//!
//! 1. Snapshot the current active set from [`ActiveSetStore`].
//! 2. For each issue, build a minimal [`WorkItem`] + [`RoutingContext`]
//!    and run [`RoutingEngine::route`].
//! 3. Skip issues whose routed role kind is kernel-special
//!    (`integration_owner` / `qa_gate`) — those belong to the integration
//!    and QA queues respectively.
//! 4. For routable specialist work, claim by identifier (so re-ticking
//!    the same active set does not re-emit) and enqueue a
//!    [`SpecialistDispatchRequest`] into [`SpecialistDispatchQueue`].
//!
//! Cross-tick claim cleanup: identifiers that leave the active set drop
//! out of the claim set so a re-opened or re-classified issue is
//! eligible to dispatch again. Durable leases for *running* work are a
//! separate Phase 11 task; this tick only deduplicates emission.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::blocker::RunRef;
use crate::intake_tick::ActiveSetStore;
use crate::logical_queue::{LogicalQueue, QueueTickOutcome};
use crate::queue_tick::{QueueTick, QueueTickCadence};
use crate::role::{RoleKind, RoleName};
use crate::routing::{RoutingContext, RoutingDecision, RoutingEngine};
use crate::tracker::Issue;
use crate::work_item::{TrackerStatus, WorkItem, WorkItemId, WorkItemStatusClass};

/// One open blocker that prevents a specialist issue from dispatching.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecialistBlockerSummary {
    /// Identifier of the prerequisite/blocking child.
    pub blocker_identifier: String,
    /// Human-facing reason, usually from the `blocks` edge.
    pub reason: Option<String>,
}

/// A specialist-routable issue parked by dependency blockers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecialistBlockedIssue {
    /// Identifier of the waiting child.
    pub identifier: String,
    /// Open blockers that must clear before dispatch.
    pub blockers: Vec<SpecialistBlockerSummary>,
}

/// Error returned by dependency gate lookups.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("specialist dependency gate failed for {identifier}: {message}")]
pub struct SpecialistDependencyGateError {
    /// Tracker-facing identifier being checked.
    pub identifier: String,
    /// Operator-facing failure detail.
    pub message: String,
}

/// Dependency gate consulted immediately before specialist dispatch.
///
/// The production adapter can implement this over durable
/// `work_item_edges.list_incoming_open_blockers`; the core tick keeps
/// the dependency surface narrow so `symphony-core` does not depend on
/// the state crate.
pub trait SpecialistDependencyGate: Send + Sync {
    /// Return open blockers for `issue`. Empty means dispatch-eligible.
    fn open_blockers(
        &self,
        issue: &Issue,
    ) -> Result<Vec<SpecialistBlockerSummary>, SpecialistDependencyGateError>;
}

#[derive(Debug, Default)]
struct NoopSpecialistDependencyGate;

impl SpecialistDependencyGate for NoopSpecialistDependencyGate {
    fn open_blockers(
        &self,
        _issue: &Issue,
    ) -> Result<Vec<SpecialistBlockerSummary>, SpecialistDependencyGateError> {
        Ok(Vec::new())
    }
}

/// One dispatch request emitted by the specialist queue tick.
///
/// Plain serializable data so the eventual scheduler can persist it and
/// replay it on restart. `rule_index` is `None` when the request was
/// produced from [`RoutingDecision::Default`] (no rule matched, fell
/// through to `default_role`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecialistDispatchRequest {
    /// Stable tracker-internal id for the issue being dispatched.
    pub issue_id: String,
    /// Tracker-facing identifier (e.g. `ENG-42`). Used as the claim key.
    pub identifier: String,
    /// Routed destination role.
    pub role: RoleName,
    /// Index of the matching rule in [`RoutingEngine`]'s table, when a
    /// rule actually matched. `None` for `default_role` fall-through.
    pub rule_index: Option<usize>,
    /// Durable `runs.id` row this dispatch corresponds to, when the
    /// scheduler created the run row before enqueueing. `None` when the
    /// emitter has not yet reserved a run row (e.g. ticks running ahead
    /// of state-backed wiring); runners with leasing configured will
    /// skip lease acquisition for those requests rather than fabricate
    /// an id. See `SpecialistRunner::with_leasing`.
    #[serde(default)]
    pub run_id: Option<RunRef>,
    /// Agent profile name bound to the routed role at emission time, if
    /// any. Plumbed onto the request so concurrency-gated runners can
    /// derive a [`crate::concurrency_gate::DispatchTriple`] without
    /// re-resolving workflow config per dispatch. `None` skips the
    /// `AgentProfile` scope acquisition.
    #[serde(default)]
    pub agent_profile: Option<String>,
    /// Repository slug the dispatch targets, if known. Used by
    /// concurrency-gated runners to acquire the `Repository` scope.
    /// `None` skips the `Repository` scope acquisition.
    #[serde(default)]
    pub repository: Option<String>,
    /// Parent [`WorkItemId`] for cancellation observation. When set,
    /// `observe_for_run_or_parent` (SPEC v2 §4.5 / Phase 11.5) will fall
    /// back to the work-item keyspace if no run-keyed cancel is pending.
    /// `None` reduces the observation to the run keyspace alone (or skips
    /// it entirely when the request also lacks a `run_id`).
    #[serde(default)]
    pub parent_work_item_id: Option<WorkItemId>,
}

/// Shared FIFO of pending [`SpecialistDispatchRequest`]s.
///
/// The specialist tick writes; the eventual scheduler/dispatcher drains
/// via [`Self::drain`]. Order preserved: oldest emission first, so the
/// dispatcher processes older work before newer.
#[derive(Default, Debug)]
pub struct SpecialistDispatchQueue {
    inner: Mutex<Vec<SpecialistDispatchRequest>>,
}

impl SpecialistDispatchQueue {
    /// Construct an empty queue.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of pending dispatch requests.
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .expect("dispatch queue mutex poisoned")
            .len()
    }

    /// True iff no pending requests.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Snapshot the pending requests without removing them. Cheap clone
    /// for the small queues the orchestrator handles.
    pub fn snapshot(&self) -> Vec<SpecialistDispatchRequest> {
        self.inner
            .lock()
            .expect("dispatch queue mutex poisoned")
            .clone()
    }

    /// Drain every pending request in FIFO order.
    pub fn drain(&self) -> Vec<SpecialistDispatchRequest> {
        std::mem::take(&mut *self.inner.lock().expect("dispatch queue mutex poisoned"))
    }

    /// Append a new request. Pub-crate so only the specialist tick (and
    /// its tests) can write; consumers read via [`Self::drain`].
    pub(crate) fn enqueue(&self, req: SpecialistDispatchRequest) {
        self.inner
            .lock()
            .expect("dispatch queue mutex poisoned")
            .push(req);
    }
}

/// Lookup from configured [`RoleName`] to its [`RoleKind`].
///
/// Workflow config validates role-name uniqueness, so a `RoleName` not
/// present here is treated as a config bug surfaced by the routing
/// reference resolver — the tick defers (does not panic) to keep tick
/// logic resilient against hot-reload races.
#[derive(Debug, Clone, Default)]
pub struct RoleKindLookup {
    inner: std::collections::HashMap<String, RoleKind>,
}

impl RoleKindLookup {
    /// Build a lookup from `(name, kind)` pairs. Later entries with the
    /// same name overwrite earlier ones — config validation should have
    /// rejected duplicates upstream.
    pub fn from_pairs<I, S>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (S, RoleKind)>,
        S: Into<String>,
    {
        let mut inner = std::collections::HashMap::new();
        for (name, kind) in pairs {
            inner.insert(name.into(), kind);
        }
        Self { inner }
    }

    /// Resolve a role name to its kind, if known.
    pub fn get(&self, name: &RoleName) -> Option<RoleKind> {
        self.inner.get(name.as_str()).copied()
    }
}

/// Specialist queue tick.
///
/// One tick = one pass over the active-set snapshot. Bounded work,
/// no awaits beyond the trait's async signature (the routing engine is
/// synchronous). Cadence and fan-out belong to the multi-queue
/// scheduler.
pub struct SpecialistQueueTick {
    active_set: Arc<ActiveSetStore>,
    routing: RoutingEngine,
    role_kinds: RoleKindLookup,
    queue: Arc<SpecialistDispatchQueue>,
    dependency_gate: Arc<dyn SpecialistDependencyGate>,
    cadence: QueueTickCadence,
    /// Identifiers already emitted that are still present in the active
    /// set. Pruned at the start of each tick so identifiers leaving the
    /// active set become eligible to re-dispatch.
    claimed: Mutex<HashSet<String>>,
    /// Current tick's dependency-parked issues. This gives status/debug
    /// surfaces a reasoned parked set instead of making blocked children
    /// disappear from the specialist queue.
    blocked: Mutex<Vec<SpecialistBlockedIssue>>,
}

impl SpecialistQueueTick {
    /// Construct a specialist tick.
    pub fn new(
        active_set: Arc<ActiveSetStore>,
        routing: RoutingEngine,
        role_kinds: RoleKindLookup,
        queue: Arc<SpecialistDispatchQueue>,
        cadence: QueueTickCadence,
    ) -> Self {
        Self {
            active_set,
            routing,
            role_kinds,
            queue,
            dependency_gate: Arc::new(NoopSpecialistDependencyGate),
            cadence,
            claimed: Mutex::new(HashSet::new()),
            blocked: Mutex::new(Vec::new()),
        }
    }

    /// Attach a durable dependency gate.
    pub fn with_dependency_gate(mut self, gate: Arc<dyn SpecialistDependencyGate>) -> Self {
        self.dependency_gate = gate;
        self
    }

    /// Borrow the shared dispatch queue. Lets the composition root wire
    /// the consumer side without separately threading the `Arc`.
    pub fn dispatch_queue(&self) -> &Arc<SpecialistDispatchQueue> {
        &self.queue
    }

    /// Number of identifiers currently claimed (emitted and still in the
    /// active set).
    pub fn claimed_count(&self) -> usize {
        self.claimed
            .lock()
            .expect("claimed set mutex poisoned")
            .len()
    }

    /// Snapshot issues parked by open dependency blockers on the most
    /// recent tick.
    pub fn blocked_snapshot(&self) -> Vec<SpecialistBlockedIssue> {
        self.blocked
            .lock()
            .expect("blocked set mutex poisoned")
            .clone()
    }
}

#[async_trait]
impl QueueTick for SpecialistQueueTick {
    fn queue(&self) -> LogicalQueue {
        LogicalQueue::Specialist
    }

    fn cadence(&self) -> QueueTickCadence {
        self.cadence
    }

    async fn tick(&mut self) -> QueueTickOutcome {
        let snapshot = self.active_set.snapshot();
        let considered = snapshot.len();

        // Prune claims: any previously-claimed identifier no longer
        // present in the active set is eligible to re-dispatch when it
        // reappears.
        let active_ids: HashSet<String> = snapshot.iter().map(|i| i.identifier.clone()).collect();
        {
            let mut claimed = self.claimed.lock().expect("claimed set mutex poisoned");
            claimed.retain(|id| active_ids.contains(id));
        }
        self.blocked
            .lock()
            .expect("blocked set mutex poisoned")
            .clear();

        let mut processed = 0usize;
        let mut deferred = 0usize;
        let mut errors = 0usize;

        for issue in &snapshot {
            // Skip if already claimed in a prior tick (and still active).
            {
                let claimed = self.claimed.lock().expect("claimed set mutex poisoned");
                if claimed.contains(&issue.identifier) {
                    deferred += 1;
                    continue;
                }
            }

            let item = work_item_for_routing(issue);
            let ctx = RoutingContext::default();
            match self.routing.route(&item, &ctx) {
                Err(_e) => {
                    errors += 1;
                }
                Ok(RoutingDecision::NoMatch) => {
                    deferred += 1;
                }
                Ok(decision) => {
                    let (role, rule_index) = match decision {
                        RoutingDecision::Matched { role, rule_index } => (role, Some(rule_index)),
                        RoutingDecision::Default { role } => (role, None),
                        RoutingDecision::NoMatch => unreachable!(),
                    };
                    // Resolve role kind. Unknown role names defer (config
                    // drift) instead of panicking inside a tick.
                    match self.role_kinds.get(&role) {
                        Some(kind) if !kind.is_kernel_special() => {
                            let blockers = match self.dependency_gate.open_blockers(issue) {
                                Ok(blockers) => blockers,
                                Err(_e) => {
                                    errors += 1;
                                    continue;
                                }
                            };
                            if !blockers.is_empty() {
                                self.blocked
                                    .lock()
                                    .expect("blocked set mutex poisoned")
                                    .push(SpecialistBlockedIssue {
                                        identifier: issue.identifier.clone(),
                                        blockers,
                                    });
                                deferred += 1;
                                continue;
                            }
                            self.queue.enqueue(SpecialistDispatchRequest {
                                issue_id: issue.id.as_str().to_string(),
                                identifier: issue.identifier.clone(),
                                role,
                                rule_index,
                                run_id: None,
                                agent_profile: None,
                                repository: None,
                                parent_work_item_id: None,
                            });
                            self.claimed
                                .lock()
                                .expect("claimed set mutex poisoned")
                                .insert(issue.identifier.clone());
                            processed += 1;
                        }
                        _ => {
                            deferred += 1;
                        }
                    }
                }
            }
        }

        QueueTickOutcome {
            queue: LogicalQueue::Specialist,
            considered,
            processed,
            deferred,
            errors,
        }
    }
}

/// Build the minimal [`WorkItem`] shape the routing engine needs.
///
/// Routing only inspects [`WorkItem::labels`] (plus [`RoutingContext`]).
/// Other fields are populated with safe placeholders so the engine has a
/// well-formed value to evaluate. The kernel will swap this for the
/// durable [`WorkItem`] record once durable work-item state is wired
/// through the queue (Phase 11 follow-on items).
fn work_item_for_routing(issue: &Issue) -> WorkItem {
    WorkItem {
        id: WorkItemId::new(0),
        tracker_id: String::new(),
        identifier: issue.identifier.clone(),
        title: issue.title.clone(),
        description: issue.description.clone(),
        tracker_status: TrackerStatus::new(issue.state.as_str()),
        status_class: WorkItemStatusClass::Ready,
        priority: issue.priority,
        labels: issue.labels.clone(),
        url: issue.url.clone(),
        parent_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::{RoutingMatch, RoutingMatchMode, RoutingRule, RoutingTable};
    use crate::tracker::Issue;
    use std::collections::HashMap;
    use std::time::Duration;

    fn cadence() -> QueueTickCadence {
        QueueTickCadence::fixed(Duration::from_millis(1))
    }

    fn issue(identifier: &str, labels: &[&str]) -> Issue {
        let mut i = Issue::minimal(identifier, identifier, "title", "Todo");
        i.labels = labels.iter().map(|s| s.to_string()).collect();
        i
    }

    fn rule(priority: Option<u32>, labels: &[&str], role: &str) -> RoutingRule {
        RoutingRule {
            priority,
            when: RoutingMatch {
                labels_any: labels.iter().map(|s| s.to_string()).collect(),
                paths_any: Vec::new(),
                issue_size: None,
            },
            assign_role: RoleName::from(role),
        }
    }

    fn standard_role_kinds() -> RoleKindLookup {
        RoleKindLookup::from_pairs([
            ("platform_lead", RoleKind::IntegrationOwner),
            ("qa", RoleKind::QaGate),
            ("frontend", RoleKind::Specialist),
            ("backend", RoleKind::Specialist),
            ("docs", RoleKind::Specialist),
        ])
    }

    fn build_tick(
        issues: Vec<Issue>,
        table: RoutingTable,
    ) -> (
        SpecialistQueueTick,
        Arc<ActiveSetStore>,
        Arc<SpecialistDispatchQueue>,
    ) {
        let store = Arc::new(ActiveSetStore::new());
        store.replace(issues);
        let queue = Arc::new(SpecialistDispatchQueue::new());
        let tick = SpecialistQueueTick::new(
            store.clone(),
            RoutingEngine::new(table),
            standard_role_kinds(),
            queue.clone(),
            cadence(),
        );
        (tick, store, queue)
    }

    #[derive(Debug, Default)]
    struct FakeDependencyGate {
        blockers: HashMap<String, Vec<SpecialistBlockerSummary>>,
    }

    impl FakeDependencyGate {
        fn with_blocker(mut self, blocked: &str, blocker: &str) -> Self {
            self.blockers
                .entry(blocked.to_string())
                .or_default()
                .push(SpecialistBlockerSummary {
                    blocker_identifier: blocker.to_string(),
                    reason: Some(format!("{blocked} depends on {blocker}")),
                });
            self
        }
    }

    impl SpecialistDependencyGate for FakeDependencyGate {
        fn open_blockers(
            &self,
            issue: &Issue,
        ) -> Result<Vec<SpecialistBlockerSummary>, SpecialistDependencyGateError> {
            Ok(self
                .blockers
                .get(&issue.identifier)
                .cloned()
                .unwrap_or_default())
        }
    }

    fn specialist_default_table() -> RoutingTable {
        RoutingTable {
            default_role: Some(RoleName::from("backend")),
            match_mode: RoutingMatchMode::FirstMatch,
            rules: vec![],
        }
    }

    #[tokio::test]
    async fn tick_emits_dispatch_for_first_match_specialist() {
        let table = RoutingTable {
            default_role: None,
            match_mode: RoutingMatchMode::FirstMatch,
            rules: vec![
                rule(None, &["frontend"], "frontend"),
                rule(None, &["backend"], "backend"),
            ],
        };
        let (mut t, _store, queue) = build_tick(
            vec![issue("ENG-1", &["frontend"]), issue("ENG-2", &["backend"])],
            table,
        );
        let outcome = t.tick().await;
        assert_eq!(outcome.queue, LogicalQueue::Specialist);
        assert_eq!(outcome.considered, 2);
        assert_eq!(outcome.processed, 2);
        assert_eq!(outcome.deferred, 0);
        assert_eq!(outcome.errors, 0);

        let drained = queue.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].identifier, "ENG-1");
        assert_eq!(drained[0].role, RoleName::from("frontend"));
        assert_eq!(drained[0].rule_index, Some(0));
        assert_eq!(drained[1].identifier, "ENG-2");
        assert_eq!(drained[1].role, RoleName::from("backend"));
        assert_eq!(drained[1].rule_index, Some(1));
    }

    #[tokio::test]
    async fn dependency_gate_dispatches_only_root_child_for_sequential_chain() {
        let (tick, _store, queue) = build_tick(
            vec![
                issue("A", &["backend"]),
                issue("B", &["backend"]),
                issue("C", &["backend"]),
            ],
            specialist_default_table(),
        );
        let gate = Arc::new(
            FakeDependencyGate::default()
                .with_blocker("B", "A")
                .with_blocker("C", "B"),
        );
        let mut tick = tick.with_dependency_gate(gate);

        let outcome = tick.tick().await;
        assert_eq!(outcome.considered, 3);
        assert_eq!(outcome.processed, 1);
        assert_eq!(outcome.deferred, 2);
        assert_eq!(outcome.errors, 0);

        let dispatched = queue.drain();
        assert_eq!(dispatched.len(), 1);
        assert_eq!(dispatched[0].identifier, "A");

        let parked = tick.blocked_snapshot();
        assert_eq!(parked.len(), 2);
        assert_eq!(parked[0].identifier, "B");
        assert_eq!(parked[0].blockers[0].blocker_identifier, "A");
        assert_eq!(parked[1].identifier, "C");
        assert_eq!(parked[1].blockers[0].blocker_identifier, "B");
    }

    #[tokio::test]
    async fn dependency_gate_allows_parallel_roots_and_parks_join_child() {
        let (tick, _store, queue) = build_tick(
            vec![
                issue("A", &["backend"]),
                issue("B", &["backend"]),
                issue("C", &["backend"]),
            ],
            specialist_default_table(),
        );
        let gate = Arc::new(
            FakeDependencyGate::default()
                .with_blocker("C", "A")
                .with_blocker("C", "B"),
        );
        let mut tick = tick.with_dependency_gate(gate);

        let outcome = tick.tick().await;
        assert_eq!(outcome.processed, 2);
        assert_eq!(outcome.deferred, 1);

        let dispatched: Vec<String> = queue
            .drain()
            .into_iter()
            .map(|req| req.identifier)
            .collect();
        assert_eq!(dispatched, vec!["A", "B"]);

        let parked = tick.blocked_snapshot();
        assert_eq!(parked.len(), 1);
        assert_eq!(parked[0].identifier, "C");
        let blockers: Vec<&str> = parked[0]
            .blockers
            .iter()
            .map(|b| b.blocker_identifier.as_str())
            .collect();
        assert_eq!(blockers, vec!["A", "B"]);
    }

    #[tokio::test]
    async fn dependency_gate_dispatches_child_after_all_blockers_clear() {
        let (tick, store, queue) =
            build_tick(vec![issue("C", &["backend"])], specialist_default_table());
        let gate = Arc::new(
            FakeDependencyGate::default()
                .with_blocker("C", "A")
                .with_blocker("C", "B"),
        );
        let mut tick = tick.with_dependency_gate(gate);

        let first = tick.tick().await;
        assert_eq!(first.processed, 0);
        assert_eq!(first.deferred, 1);
        assert!(queue.is_empty());
        assert_eq!(tick.blocked_snapshot()[0].blockers.len(), 2);

        store.replace(Vec::new());
        tick.tick().await;

        let gate = Arc::new(FakeDependencyGate::default());
        let mut tick = SpecialistQueueTick::new(
            store.clone(),
            RoutingEngine::new(specialist_default_table()),
            standard_role_kinds(),
            queue.clone(),
            cadence(),
        )
        .with_dependency_gate(gate);
        store.replace(vec![issue("C", &["backend"])]);

        let second = tick.tick().await;
        assert_eq!(second.processed, 1);
        assert_eq!(second.deferred, 0);
        assert!(tick.blocked_snapshot().is_empty());
        assert_eq!(queue.drain()[0].identifier, "C");
    }

    #[tokio::test]
    async fn tick_first_match_picks_first_rule_in_declaration_order() {
        // Both rules match; first-match must pick rule 0 even though a
        // higher-priority rule would win in priority mode.
        let table = RoutingTable {
            default_role: None,
            match_mode: RoutingMatchMode::FirstMatch,
            rules: vec![
                rule(Some(1), &["frontend"], "frontend"),
                rule(Some(9999), &["frontend"], "backend"),
            ],
        };
        let (mut t, _store, queue) = build_tick(vec![issue("ENG-1", &["frontend"])], table);
        let outcome = t.tick().await;
        assert_eq!(outcome.processed, 1);
        let drained = queue.drain();
        assert_eq!(drained[0].role, RoleName::from("frontend"));
        assert_eq!(drained[0].rule_index, Some(0));
    }

    #[tokio::test]
    async fn tick_priority_mode_picks_highest_priority_match() {
        let table = RoutingTable {
            default_role: None,
            match_mode: RoutingMatchMode::Priority,
            rules: vec![
                rule(Some(10), &["frontend"], "frontend"),
                rule(Some(99), &["frontend"], "backend"),
                rule(Some(50), &["frontend"], "docs"),
            ],
        };
        let (mut t, _store, queue) = build_tick(vec![issue("ENG-1", &["frontend"])], table);
        let outcome = t.tick().await;
        assert_eq!(outcome.processed, 1);
        let drained = queue.drain();
        assert_eq!(drained[0].role, RoleName::from("backend"));
        assert_eq!(drained[0].rule_index, Some(1));
    }

    #[tokio::test]
    async fn tick_defers_items_routed_to_kernel_special_roles() {
        // Items routed to integration_owner / qa_gate belong to other
        // queues. The specialist tick must NOT enqueue them.
        let table = RoutingTable {
            default_role: None,
            match_mode: RoutingMatchMode::FirstMatch,
            rules: vec![
                rule(None, &["broad"], "platform_lead"), // integration_owner
                rule(None, &["needs-qa"], "qa"),         // qa_gate
                rule(None, &["frontend"], "frontend"),   // specialist
            ],
        };
        let (mut t, _store, queue) = build_tick(
            vec![
                issue("ENG-1", &["broad"]),
                issue("ENG-2", &["needs-qa"]),
                issue("ENG-3", &["frontend"]),
            ],
            table,
        );
        let outcome = t.tick().await;
        assert_eq!(outcome.considered, 3);
        assert_eq!(outcome.processed, 1);
        assert_eq!(outcome.deferred, 2);
        assert_eq!(outcome.errors, 0);

        let drained = queue.drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].identifier, "ENG-3");
        assert_eq!(drained[0].role, RoleName::from("frontend"));
    }

    #[tokio::test]
    async fn tick_defers_no_match_when_no_default() {
        let table = RoutingTable {
            default_role: None,
            match_mode: RoutingMatchMode::FirstMatch,
            rules: vec![rule(None, &["nope"], "frontend")],
        };
        let (mut t, _store, queue) = build_tick(vec![issue("ENG-1", &["unrelated"])], table);
        let outcome = t.tick().await;
        assert_eq!(outcome.processed, 0);
        assert_eq!(outcome.deferred, 1);
        assert!(queue.is_empty());
    }

    #[tokio::test]
    async fn tick_emits_for_default_role_when_specialist_kind() {
        // No rule matches; default_role is a specialist — emit with
        // rule_index == None.
        let table = RoutingTable {
            default_role: Some(RoleName::from("frontend")),
            match_mode: RoutingMatchMode::FirstMatch,
            rules: vec![rule(None, &["nope"], "backend")],
        };
        let (mut t, _store, queue) = build_tick(vec![issue("ENG-1", &["unrelated"])], table);
        let outcome = t.tick().await;
        assert_eq!(outcome.processed, 1);
        let drained = queue.drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].rule_index, None);
        assert_eq!(drained[0].role, RoleName::from("frontend"));
    }

    #[tokio::test]
    async fn tick_defers_default_role_to_integration_owner() {
        let table = RoutingTable {
            default_role: Some(RoleName::from("platform_lead")),
            match_mode: RoutingMatchMode::FirstMatch,
            rules: vec![],
        };
        let (mut t, _store, queue) = build_tick(vec![issue("ENG-1", &[])], table);
        let outcome = t.tick().await;
        assert_eq!(outcome.processed, 0);
        assert_eq!(outcome.deferred, 1);
        assert!(queue.is_empty());
    }

    #[tokio::test]
    async fn tick_records_errors_for_priority_tie() {
        let table = RoutingTable {
            default_role: None,
            match_mode: RoutingMatchMode::Priority,
            rules: vec![
                rule(Some(50), &["frontend"], "frontend"),
                rule(Some(50), &["frontend"], "backend"),
            ],
        };
        let (mut t, _store, queue) = build_tick(vec![issue("ENG-1", &["frontend"])], table);
        let outcome = t.tick().await;
        assert_eq!(outcome.processed, 0);
        assert_eq!(outcome.errors, 1);
        assert!(queue.is_empty());
    }

    #[tokio::test]
    async fn tick_does_not_re_emit_already_claimed_identifier() {
        let table = RoutingTable {
            default_role: None,
            match_mode: RoutingMatchMode::FirstMatch,
            rules: vec![rule(None, &["frontend"], "frontend")],
        };
        let (mut t, _store, queue) = build_tick(vec![issue("ENG-1", &["frontend"])], table);

        let o1 = t.tick().await;
        assert_eq!(o1.processed, 1);
        let drained = queue.drain();
        assert_eq!(drained.len(), 1);

        // Second tick over the same active set: identifier is claimed,
        // dispatch must NOT be re-emitted (even after consumer drained).
        let o2 = t.tick().await;
        assert_eq!(o2.considered, 1);
        assert_eq!(o2.processed, 0);
        assert_eq!(o2.deferred, 1);
        assert!(queue.is_empty());
    }

    #[tokio::test]
    async fn tick_releases_claim_when_identifier_leaves_active_set() {
        let table = RoutingTable {
            default_role: None,
            match_mode: RoutingMatchMode::FirstMatch,
            rules: vec![rule(None, &["frontend"], "frontend")],
        };
        let (mut t, store, queue) = build_tick(vec![issue("ENG-1", &["frontend"])], table);

        t.tick().await;
        let _ = queue.drain();
        assert_eq!(t.claimed_count(), 1);

        // Active set shrinks: claim must be pruned.
        store.replace(Vec::new());
        let o = t.tick().await;
        assert_eq!(o.considered, 0);
        assert_eq!(t.claimed_count(), 0);

        // Identifier reappears: must be eligible to re-dispatch.
        store.replace(vec![issue("ENG-1", &["frontend"])]);
        let o2 = t.tick().await;
        assert_eq!(o2.processed, 1);
        let drained = queue.drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].identifier, "ENG-1");
    }

    #[tokio::test]
    async fn tick_reports_specialist_queue_and_configured_cadence() {
        let store = Arc::new(ActiveSetStore::new());
        let queue = Arc::new(SpecialistDispatchQueue::new());
        let cad = QueueTickCadence::from_millis(2_500, 100);
        let t = SpecialistQueueTick::new(
            store,
            RoutingEngine::new(RoutingTable::default()),
            standard_role_kinds(),
            queue,
            cad,
        );
        assert_eq!(t.queue(), LogicalQueue::Specialist);
        assert_eq!(t.cadence(), cad);
    }

    #[tokio::test]
    async fn tick_drives_through_dyn_queue_tick_trait_object() {
        // Confirms SpecialistQueueTick composes with the deterministic
        // harness: the scheduler will store ticks as `Box<dyn QueueTick>`.
        use crate::queue_tick::run_queue_tick_n;

        let table = RoutingTable {
            default_role: None,
            match_mode: RoutingMatchMode::FirstMatch,
            rules: vec![rule(None, &["frontend"], "frontend")],
        };
        let (t, _store, queue) = build_tick(vec![issue("ENG-1", &["frontend"])], table);
        let mut boxed: Box<dyn QueueTick> = Box::new(t);
        let outcomes = run_queue_tick_n(boxed.as_mut(), 3).await;
        assert_eq!(outcomes.len(), 3);
        for o in &outcomes {
            assert_eq!(o.queue, LogicalQueue::Specialist);
        }
        // Tick 1 emits, ticks 2/3 see it claimed and defer.
        assert_eq!(outcomes[0].processed, 1);
        assert_eq!(outcomes[1].processed, 0);
        assert_eq!(outcomes[1].deferred, 1);
        assert_eq!(outcomes[2].processed, 0);
        assert_eq!(queue.len(), 1);
    }

    #[tokio::test]
    async fn tick_defers_when_role_kind_is_not_in_lookup() {
        // Routing returns a role name that the lookup does not know
        // about (config drift / hot-reload race). The tick must defer
        // rather than panicking.
        let table = RoutingTable {
            default_role: None,
            match_mode: RoutingMatchMode::FirstMatch,
            rules: vec![rule(None, &["frontend"], "ghost_role")],
        };
        let (mut t, _store, queue) = build_tick(vec![issue("ENG-1", &["frontend"])], table);
        let outcome = t.tick().await;
        assert_eq!(outcome.processed, 0);
        assert_eq!(outcome.deferred, 1);
        assert_eq!(outcome.errors, 0);
        assert!(queue.is_empty());
    }

    #[tokio::test]
    async fn dispatch_queue_drain_returns_fifo_and_empties_queue() {
        let q = SpecialistDispatchQueue::new();
        q.enqueue(SpecialistDispatchRequest {
            issue_id: "1".into(),
            identifier: "ENG-1".into(),
            role: RoleName::from("frontend"),
            rule_index: Some(0),
            run_id: None,
            agent_profile: None,
            repository: None,
            parent_work_item_id: None,
        });
        q.enqueue(SpecialistDispatchRequest {
            issue_id: "2".into(),
            identifier: "ENG-2".into(),
            role: RoleName::from("backend"),
            rule_index: Some(1),
            run_id: None,
            agent_profile: None,
            repository: None,
            parent_work_item_id: None,
        });
        assert_eq!(q.len(), 2);
        let drained = q.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].identifier, "ENG-1");
        assert_eq!(drained[1].identifier, "ENG-2");
        assert!(q.is_empty());
    }
}
