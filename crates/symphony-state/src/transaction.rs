//! Transaction helper for atomically composing a state transition with
//! one or more event appends.
//!
//! The kernel's contract (ARCHITECTURE_v2.md § 4.3) says: "every state
//! transition writes one or more events in the same SQLite transaction
//! as the row mutation." A naive `&mut StateDb` API cannot express that
//! — the trait methods on [`crate::repository::WorkItemRepository`],
//! [`crate::repository::RunRepository`], and
//! [`crate::events::EventRepository`] each open their own implicit
//! transaction. If a process crashes between two of them, the durable
//! event log can disagree with the durable state.
//!
//! [`StateDb::transaction`] solves that by exposing a single closure
//! scope over a [`StateTransaction`] handle. The closure runs inside
//! one SQLite `BEGIN`/`COMMIT`. If the closure returns `Err` the
//! transaction is dropped without commit (rusqlite then rolls back),
//! and if it returns `Ok` the helper commits. Sequence numbers stay
//! monotonic because the underlying append uses
//! `COALESCE(MAX(sequence), 0) + 1` — a reader inside the same
//! transaction sees the last visible row, so two appends within one
//! transaction get sequences `N` and `N+1`.
//!
//! The wrapper deliberately exposes only the mutation surface a state
//! transition typically needs (work item create/update, run
//! create/update, plus event append). Read-only paths can use the
//! repository traits on [`StateDb`] directly.

use rusqlite::Transaction;

use crate::edges::{
    EdgeId, NewDecompositionBlockerEdge, NewWorkItemEdge, WorkItemEdgeRecord,
    create_decomposition_blocker_edges_in, create_edge_in, get_edge_in,
    list_open_blockers_for_subtree_in, update_edge_status_in,
};
use crate::events::{EventRecord, NewEvent};
use crate::handoffs::{HandoffRecord, NewHandoff, create_handoff_in};
use crate::integration_records::{
    IntegrationRecordRow, NewIntegrationRecord, create_integration_record_in,
};
use crate::repository::{
    LeaseAcquisition, NewRun, NewWorkItem, RunId, RunRecord, WorkItemId, WorkItemRecord,
    acquire_lease_in, create_run_in, create_work_item_in, get_run_in, get_work_item_in,
    release_lease_in, update_run_status_in, update_work_item_status_in,
};
use crate::{StateDb, StateResult};

/// Mutation surface available inside a [`StateDb::transaction`] closure.
///
/// All methods execute against the open SQLite transaction; nothing is
/// visible to other connections until the closure returns `Ok` and the
/// helper commits.
pub struct StateTransaction<'conn> {
    tx: Transaction<'conn>,
}

impl<'conn> StateTransaction<'conn> {
    /// Insert a new work item.
    pub fn create_work_item(&mut self, new: NewWorkItem<'_>) -> StateResult<WorkItemRecord> {
        create_work_item_in(&self.tx, new)
    }

    /// Read a work item — useful when a transition needs to verify the
    /// current row before updating it.
    pub fn get_work_item(&self, id: WorkItemId) -> StateResult<Option<WorkItemRecord>> {
        get_work_item_in(&self.tx, id)
    }

    /// Update the cached status class and raw tracker status. Returns
    /// `Ok(false)` if the row does not exist.
    pub fn update_work_item_status(
        &mut self,
        id: WorkItemId,
        status_class: &str,
        tracker_status: &str,
        now: &str,
    ) -> StateResult<bool> {
        update_work_item_status_in(&self.tx, id, status_class, tracker_status, now)
    }

    /// Insert a new run.
    pub fn create_run(&mut self, new: NewRun<'_>) -> StateResult<RunRecord> {
        create_run_in(&self.tx, new)
    }

    /// Read a run.
    pub fn get_run(&self, id: RunId) -> StateResult<Option<RunRecord>> {
        get_run_in(&self.tx, id)
    }

    /// Update only the run's lifecycle status. Returns `Ok(false)` if
    /// the row does not exist.
    pub fn update_run_status(&mut self, id: RunId, status: &str) -> StateResult<bool> {
        update_run_status_in(&self.tx, id, status)
    }

    /// Atomically claim the durable lease on a run inside this
    /// transaction.
    ///
    /// Composing lease acquisition with `update_run_status` + an
    /// `append_event` is what the kernel needs to advance a `queued`
    /// run to `running` without leaving a half-built world if the
    /// process dies mid-dispatch: either the lease, status, and
    /// `run.started` event all land, or none of them do. See
    /// [`crate::repository::RunRepository::acquire_lease`] for the
    /// underlying contention semantics — they are unchanged here.
    pub fn acquire_lease(
        &mut self,
        run_id: RunId,
        owner: &str,
        expires_at: &str,
        now: &str,
    ) -> StateResult<LeaseAcquisition> {
        acquire_lease_in(&self.tx, run_id, owner, expires_at, now)
    }

    /// Clear the lease columns on a run inside this transaction.
    ///
    /// Pairs with `update_run_status` to a terminal status (success,
    /// failure, cancellation) and the matching `run.completed` /
    /// `run.failed` event so the recovery scheduler cannot observe a
    /// run that has finished but still appears leased.
    pub fn release_lease(&mut self, run_id: RunId) -> StateResult<bool> {
        release_lease_in(&self.tx, run_id)
    }

    /// Insert a parent/child, blocker, relates-to, or follow-up edge.
    ///
    /// Composing edge inserts inside the same transaction as the
    /// `work_items` rows they reference is what keeps the
    /// parent-completion gate (Phase 5) consistent: the propagation
    /// query in [`Self::list_open_blockers_for_subtree`] cannot observe
    /// a half-built tree.
    pub fn create_edge(&mut self, new: NewWorkItemEdge<'_>) -> StateResult<WorkItemEdgeRecord> {
        create_edge_in(&self.tx, new)
    }

    /// Insert decomposition-sourced dependency blocker edges inside
    /// this transaction.
    ///
    /// This is the transactional companion to
    /// [`crate::edges::WorkItemEdgeRepository::create_decomposition_blocker_edges`]
    /// for callers that are also creating child `work_items` or
    /// `parent_child` edges in the same recoverable operation.
    pub fn create_decomposition_blocker_edges(
        &mut self,
        edges: &[NewDecompositionBlockerEdge<'_>],
    ) -> StateResult<Vec<WorkItemEdgeRecord>> {
        create_decomposition_blocker_edges_in(&self.tx, edges)
    }

    /// Read an edge — useful when a transition needs to verify the
    /// current row before updating its status.
    pub fn get_edge(&self, id: EdgeId) -> StateResult<Option<WorkItemEdgeRecord>> {
        get_edge_in(&self.tx, id)
    }

    /// Update only the edge's lifecycle status. Returns `Ok(false)` if
    /// the row does not exist.
    pub fn update_edge_status(&mut self, id: EdgeId, status: &str) -> StateResult<bool> {
        update_edge_status_in(&self.tx, id, status)
    }

    /// Propagation query: every open `blocks` edge that gates `target`
    /// either directly or via `parent_child` descendants.
    pub fn list_open_blockers_for_subtree(
        &self,
        target: WorkItemId,
    ) -> StateResult<Vec<WorkItemEdgeRecord>> {
        list_open_blockers_for_subtree_in(&self.tx, target)
    }

    /// Append one event to the durable log inside this transaction.
    ///
    /// Two appends within the same transaction receive consecutive
    /// sequence numbers; nothing is broadcast or visible until commit.
    pub fn append_event(&mut self, new: NewEvent<'_>) -> StateResult<EventRecord> {
        crate::events::append_event_in(&self.tx, new)
    }

    /// Persist a structured agent handoff envelope inside this
    /// transaction.
    ///
    /// Handoffs are typically composed with the `run.completed` event
    /// append so the durable log and the handoff row land atomically:
    /// a crash between the two would let the operator surface contradict
    /// the audit trail.
    pub fn create_handoff(&mut self, new: NewHandoff<'_>) -> StateResult<HandoffRecord> {
        create_handoff_in(&self.tx, new)
    }

    /// Persist an integration record inside this transaction.
    ///
    /// Integration records are typically composed with the
    /// `integration.completed` (or `integration.failed`) event append so
    /// the durable log and the record row land atomically — the parent
    /// closeout gate (ARCH §6.2) reads the latest record and would
    /// observe a half-built world otherwise.
    pub fn create_integration_record(
        &mut self,
        new: NewIntegrationRecord<'_>,
    ) -> StateResult<IntegrationRecordRow> {
        create_integration_record_in(&self.tx, new)
    }
}

impl StateDb {
    /// Run `f` inside a single SQLite transaction.
    ///
    /// On `Ok`, the transaction is committed and the closure's value is
    /// returned. On `Err`, the transaction is dropped without commit
    /// (rusqlite issues `ROLLBACK`) so neither row mutations nor event
    /// appends become visible.
    ///
    /// This is the only sanctioned way the kernel composes "row change
    /// + event row" — see the module docs for the invariant.
    pub fn transaction<F, T>(&mut self, f: F) -> StateResult<T>
    where
        F: FnOnce(&mut StateTransaction<'_>) -> StateResult<T>,
    {
        let tx = self.conn.transaction()?;
        let mut state_tx = StateTransaction { tx };
        let value = f(&mut state_tx)?;
        state_tx.tx.commit()?;
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventRepository;
    use crate::migrations::migrations;
    use crate::repository::{NewRun, NewWorkItem, RunRepository, WorkItemRepository};

    fn open() -> StateDb {
        let mut db = StateDb::open_in_memory().expect("open");
        db.migrate(migrations()).expect("migrate");
        db
    }

    fn sample_work_item<'a>(identifier: &'a str, now: &'a str) -> NewWorkItem<'a> {
        NewWorkItem {
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
            now,
        }
    }

    #[test]
    fn commit_persists_state_change_and_event_atomically() {
        let mut db = open();
        let now = "2026-05-08T00:00:00Z";

        let (work_item, event) = db
            .transaction(|tx| {
                let wi = tx.create_work_item(sample_work_item("ENG-1", now))?;
                let ev = tx.append_event(NewEvent {
                    event_type: "work_item.created",
                    work_item_id: Some(wi.id),
                    run_id: None,
                    payload: r#"{"source":"intake"}"#,
                    now,
                })?;
                Ok((wi, ev))
            })
            .expect("commit");

        let fetched = db.get_work_item(work_item.id).unwrap().unwrap();
        assert_eq!(fetched.identifier, "ENG-1");
        let stored = db.list_events_for_work_item(work_item.id).unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].event_type, "work_item.created");
        assert_eq!(stored[0].sequence, event.sequence);
    }

    #[test]
    fn closure_error_rolls_back_state_and_events() {
        let mut db = open();
        let now = "2026-05-08T00:00:00Z";

        let err = db
            .transaction::<_, ()>(|tx| {
                tx.create_work_item(sample_work_item("ENG-1", now))?;
                tx.append_event(NewEvent {
                    event_type: "work_item.created",
                    work_item_id: None,
                    run_id: None,
                    payload: "{}",
                    now,
                })?;
                // Force a failure by inserting a duplicate identifier.
                tx.create_work_item(sample_work_item("ENG-1", now))?;
                Ok(())
            })
            .expect_err("duplicate identifier must fail the transaction");
        assert!(matches!(err, crate::StateError::Sqlite(_)));

        // Neither the work item nor the event survived the rollback.
        assert!(
            db.find_work_item_by_identifier("github", "ENG-1")
                .unwrap()
                .is_none()
        );
        assert!(db.last_event_sequence().unwrap().is_none());
    }

    #[test]
    fn two_appends_in_one_transaction_get_consecutive_sequences() {
        let mut db = open();
        let now = "2026-05-08T00:00:00Z";

        let (first, second) = db
            .transaction(|tx| {
                let wi = tx.create_work_item(sample_work_item("ENG-1", now))?;
                let a = tx.append_event(NewEvent {
                    event_type: "work_item.created",
                    work_item_id: Some(wi.id),
                    run_id: None,
                    payload: "{}",
                    now,
                })?;
                let b = tx.append_event(NewEvent {
                    event_type: "work_item.routed",
                    work_item_id: Some(wi.id),
                    run_id: None,
                    payload: r#"{"role":"platform_lead"}"#,
                    now,
                })?;
                Ok((a, b))
            })
            .expect("commit");

        assert_eq!(second.sequence.0, first.sequence.0 + 1);
        assert_eq!(db.last_event_sequence().unwrap(), Some(second.sequence));
    }

    #[test]
    fn run_status_transition_with_event_is_atomic() {
        let mut db = open();
        let now = "2026-05-08T00:00:00Z";

        // Seed a work item + run outside the transaction.
        let wi = db.create_work_item(sample_work_item("ENG-1", now)).unwrap();
        let run = db
            .create_run(NewRun {
                work_item_id: wi.id,
                role: "platform_lead",
                agent: "claude",
                status: "queued",
                workspace_claim_id: None,
                now,
            })
            .unwrap();

        db.transaction(|tx| {
            assert!(tx.update_run_status(run.id, "running")?);
            tx.append_event(NewEvent {
                event_type: "run.started",
                work_item_id: Some(wi.id),
                run_id: Some(run.id),
                payload: "{}",
                now,
            })?;
            Ok(())
        })
        .expect("commit");

        assert_eq!(db.get_run(run.id).unwrap().unwrap().status, "running");
        let events = db.list_events_for_work_item(wi.id).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "run.started");
        assert_eq!(events[0].run_id, Some(run.id));
    }

    #[test]
    fn parent_child_edges_and_blocker_propagation_are_atomic() {
        use crate::edges::{EdgeSource, EdgeType, NewWorkItemEdge, WorkItemEdgeRepository};

        let mut db = open();
        let now = "2026-05-08T00:00:00Z";

        let (parent_id, blocker_id) = db
            .transaction(|tx| {
                let parent = tx.create_work_item(sample_work_item("ENG-1", now))?;
                let child = tx.create_work_item(sample_work_item("ENG-2", now))?;
                let blocker = tx.create_work_item(sample_work_item("ENG-3", now))?;

                tx.create_edge(NewWorkItemEdge {
                    parent_id: parent.id,
                    child_id: child.id,
                    edge_type: EdgeType::ParentChild,
                    reason: Some("decomposition"),
                    status: "linked",
                    source: EdgeSource::Decomposition,
                    now,
                })?;
                let blocker_edge = tx.create_edge(NewWorkItemEdge {
                    parent_id: blocker.id,
                    child_id: child.id,
                    edge_type: EdgeType::Blocks,
                    reason: Some("needs schema"),
                    status: "open",
                    source: EdgeSource::Qa,
                    now,
                })?;

                // Inside the same transaction the propagation query
                // already sees the freshly inserted edges.
                let blockers = tx.list_open_blockers_for_subtree(parent.id)?;
                assert_eq!(blockers.len(), 1);
                assert_eq!(blockers[0].id, blocker_edge.id);

                tx.append_event(NewEvent {
                    event_type: "blocker.created",
                    work_item_id: Some(parent.id),
                    run_id: None,
                    payload: r#"{"source":"qa"}"#,
                    now,
                })?;
                Ok((parent.id, blocker_edge.id))
            })
            .expect("commit");

        // After commit, the public read API agrees with the transaction
        // view: the parent has one open propagated blocker.
        let after = db.list_open_blockers_for_subtree(parent_id).unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].id, blocker_id);
    }

    #[test]
    fn rollback_does_not_consume_sequence_numbers() {
        let mut db = open();
        let now = "2026-05-08T00:00:00Z";

        // First, succeed once so sequence is at 1.
        db.transaction(|tx| {
            tx.append_event(NewEvent {
                event_type: "first",
                work_item_id: None,
                run_id: None,
                payload: "{}",
                now,
            })
        })
        .unwrap();

        // Then roll back a transaction that would have allocated 2.
        let _ = db
            .transaction::<_, ()>(|tx| {
                tx.append_event(NewEvent {
                    event_type: "doomed",
                    work_item_id: None,
                    run_id: None,
                    payload: "{}",
                    now,
                })?;
                Err(crate::StateError::Sqlite(
                    rusqlite::Error::QueryReturnedNoRows,
                ))
            })
            .expect_err("forced rollback");

        // The next successful append should pick up at 2, not skip to 3.
        let next = db
            .transaction(|tx| {
                tx.append_event(NewEvent {
                    event_type: "next",
                    work_item_id: None,
                    run_id: None,
                    payload: "{}",
                    now,
                })
            })
            .unwrap();
        assert_eq!(next.sequence.0, 2);
    }

    #[test]
    fn handoff_and_event_compose_atomically() {
        use crate::handoffs::{HandoffRepository, NewHandoff};

        let mut db = open();
        let now = "2026-05-08T00:00:00Z";

        let wi = db.create_work_item(sample_work_item("ENG-1", now)).unwrap();
        let run = db
            .create_run(NewRun {
                work_item_id: wi.id,
                role: "platform_lead",
                agent: "claude",
                status: "running",
                workspace_claim_id: None,
                now,
            })
            .unwrap();

        let handoff = db
            .transaction(|tx| {
                let h = tx.create_handoff(NewHandoff {
                    run_id: run.id,
                    work_item_id: wi.id,
                    ready_for: "qa",
                    summary: "specialist done",
                    changed_files: Some(r#"["src/lib.rs"]"#),
                    tests_run: None,
                    evidence: None,
                    known_risks: None,
                    now,
                })?;
                tx.append_event(NewEvent {
                    event_type: "run.completed",
                    work_item_id: Some(wi.id),
                    run_id: Some(run.id),
                    payload: r#"{"ready_for":"qa"}"#,
                    now,
                })?;
                Ok(h)
            })
            .expect("commit");

        assert_eq!(
            db.latest_handoff_for_work_item(wi.id).unwrap().unwrap().id,
            handoff.id
        );
        let events = db.list_events_for_work_item(wi.id).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "run.completed");
    }

    #[test]
    fn lease_acquisition_status_and_event_commit_together() {
        use crate::repository::LeaseAcquisition;

        let mut db = open();
        let now = "2026-05-08T00:00:00Z";
        let expires = "2026-05-08T00:05:00Z";

        let wi = db.create_work_item(sample_work_item("ENG-1", now)).unwrap();
        let run = db
            .create_run(NewRun {
                work_item_id: wi.id,
                role: "platform_lead",
                agent: "claude",
                status: "queued",
                workspace_claim_id: None,
                now,
            })
            .unwrap();

        db.transaction(|tx| {
            let outcome = tx.acquire_lease(run.id, "scheduler-1/worker-0", expires, now)?;
            assert_eq!(outcome, LeaseAcquisition::Acquired);
            assert!(tx.update_run_status(run.id, "running")?);
            tx.append_event(NewEvent {
                event_type: "run.started",
                work_item_id: Some(wi.id),
                run_id: Some(run.id),
                payload: r#"{"owner":"scheduler-1/worker-0"}"#,
                now,
            })?;
            Ok(())
        })
        .expect("commit");

        let after = db.get_run(run.id).unwrap().unwrap();
        assert_eq!(after.status, "running");
        assert_eq!(after.lease_owner.as_deref(), Some("scheduler-1/worker-0"));
        assert_eq!(after.lease_expires_at.as_deref(), Some(expires));
        let events = db.list_events_for_work_item(wi.id).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "run.started");
        assert_eq!(events[0].run_id, Some(run.id));
    }

    #[test]
    fn lease_acquisition_rolls_back_with_failed_transaction() {
        let mut db = open();
        let now = "2026-05-08T00:00:00Z";
        let expires = "2026-05-08T00:05:00Z";

        let wi = db.create_work_item(sample_work_item("ENG-1", now)).unwrap();
        let run = db
            .create_run(NewRun {
                work_item_id: wi.id,
                role: "platform_lead",
                agent: "claude",
                status: "queued",
                workspace_claim_id: None,
                now,
            })
            .unwrap();

        let _ = db
            .transaction::<_, ()>(|tx| {
                tx.acquire_lease(run.id, "scheduler-1/worker-0", expires, now)?;
                tx.update_run_status(run.id, "running")?;
                tx.append_event(NewEvent {
                    event_type: "run.started",
                    work_item_id: Some(wi.id),
                    run_id: Some(run.id),
                    payload: "{}",
                    now,
                })?;
                Err(crate::StateError::Sqlite(
                    rusqlite::Error::QueryReturnedNoRows,
                ))
            })
            .expect_err("forced rollback");

        // Lease columns, status, and event all rolled back together —
        // no half-acquired lease survives a crash mid-dispatch.
        let after = db.get_run(run.id).unwrap().unwrap();
        assert_eq!(after.status, "queued");
        assert!(after.lease_owner.is_none());
        assert!(after.lease_expires_at.is_none());
        assert!(db.list_events_for_work_item(wi.id).unwrap().is_empty());
    }

    #[test]
    fn lease_release_status_and_event_commit_together() {
        let mut db = open();
        let now = "2026-05-08T00:00:00Z";
        let expires = "2026-05-08T00:05:00Z";

        let wi = db.create_work_item(sample_work_item("ENG-1", now)).unwrap();
        // Seed the run as `running`: the typed run-status gate
        // (Phase 11) only allows `Running -> Completed` for terminal
        // release, not `Queued -> Completed`. The transactional
        // intent of this test (lease release + status update + event
        // append commit together) is unchanged.
        let run = db
            .create_run(NewRun {
                work_item_id: wi.id,
                role: "platform_lead",
                agent: "claude",
                status: "running",
                workspace_claim_id: None,
                now,
            })
            .unwrap();
        // Seed an existing lease via the public API.
        db.acquire_lease(run.id, "scheduler-1/worker-0", expires, now)
            .unwrap();

        db.transaction(|tx| {
            assert!(tx.release_lease(run.id)?);
            assert!(tx.update_run_status(run.id, "completed")?);
            tx.append_event(NewEvent {
                event_type: "run.completed",
                work_item_id: Some(wi.id),
                run_id: Some(run.id),
                payload: r#"{"verdict":"pass"}"#,
                now,
            })?;
            Ok(())
        })
        .expect("commit");

        let after = db.get_run(run.id).unwrap().unwrap();
        assert_eq!(after.status, "completed");
        assert!(after.lease_owner.is_none());
        assert!(after.lease_expires_at.is_none());
        assert!(
            db.find_expired_leases("2099-01-01T00:00:00Z")
                .unwrap()
                .is_empty()
        );
        let events = db.list_events_for_work_item(wi.id).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "run.completed");
    }

    #[test]
    fn lease_release_rolls_back_with_failed_transaction() {
        let mut db = open();
        let now = "2026-05-08T00:00:00Z";
        let expires = "2026-05-08T00:05:00Z";

        let wi = db.create_work_item(sample_work_item("ENG-1", now)).unwrap();
        let run = db
            .create_run(NewRun {
                work_item_id: wi.id,
                role: "platform_lead",
                agent: "claude",
                status: "running",
                workspace_claim_id: None,
                now,
            })
            .unwrap();
        db.acquire_lease(run.id, "scheduler-1/worker-0", expires, now)
            .unwrap();

        let _ = db
            .transaction::<_, ()>(|tx| {
                tx.release_lease(run.id)?;
                tx.update_run_status(run.id, "completed")?;
                tx.append_event(NewEvent {
                    event_type: "run.completed",
                    work_item_id: Some(wi.id),
                    run_id: Some(run.id),
                    payload: "{}",
                    now,
                })?;
                Err(crate::StateError::Sqlite(
                    rusqlite::Error::QueryReturnedNoRows,
                ))
            })
            .expect_err("forced rollback");

        // The lease that was supposed to be released stays held: the
        // recovery scheduler will still see this run as in-flight.
        let after = db.get_run(run.id).unwrap().unwrap();
        assert_eq!(after.status, "running");
        assert_eq!(after.lease_owner.as_deref(), Some("scheduler-1/worker-0"));
        assert_eq!(after.lease_expires_at.as_deref(), Some(expires));
        assert!(db.list_events_for_work_item(wi.id).unwrap().is_empty());
    }

    #[test]
    fn lease_contention_inside_transaction_is_observable_and_rolls_back_companion_writes() {
        use crate::repository::LeaseAcquisition;

        let mut db = open();
        let now = "2026-05-08T00:00:00Z";
        let expires = "2026-05-08T00:05:00Z";

        let wi = db.create_work_item(sample_work_item("ENG-1", now)).unwrap();
        let run = db
            .create_run(NewRun {
                work_item_id: wi.id,
                role: "platform_lead",
                agent: "claude",
                status: "queued",
                workspace_claim_id: None,
                now,
            })
            .unwrap();
        // Pre-existing holder from another scheduler instance.
        db.acquire_lease(run.id, "scheduler-1/worker-0", expires, now)
            .unwrap();

        // Caller asks for the lease, sees Contended, and propagates an
        // error so the companion status flip + event append roll back.
        let err = db
            .transaction::<_, ()>(|tx| {
                let outcome = tx.acquire_lease(run.id, "scheduler-2/worker-0", expires, now)?;
                match outcome {
                    LeaseAcquisition::Contended { .. } => {
                        // Companion writes that would have run on success:
                        tx.update_run_status(run.id, "running")?;
                        tx.append_event(NewEvent {
                            event_type: "run.started",
                            work_item_id: Some(wi.id),
                            run_id: Some(run.id),
                            payload: "{}",
                            now,
                        })?;
                        Err(crate::StateError::Sqlite(
                            rusqlite::Error::QueryReturnedNoRows,
                        ))
                    }
                    other => panic!("expected Contended, got {other:?}"),
                }
            })
            .expect_err("contention path forces rollback");
        assert!(matches!(err, crate::StateError::Sqlite(_)));

        let after = db.get_run(run.id).unwrap().unwrap();
        assert_eq!(after.status, "queued");
        assert_eq!(after.lease_owner.as_deref(), Some("scheduler-1/worker-0"));
        assert!(db.list_events_for_work_item(wi.id).unwrap().is_empty());
    }

    #[test]
    fn rolled_back_handoff_is_not_persisted() {
        use crate::handoffs::{HandoffRepository, NewHandoff};

        let mut db = open();
        let now = "2026-05-08T00:00:00Z";
        let wi = db.create_work_item(sample_work_item("ENG-1", now)).unwrap();
        let run = db
            .create_run(NewRun {
                work_item_id: wi.id,
                role: "platform_lead",
                agent: "claude",
                status: "running",
                workspace_claim_id: None,
                now,
            })
            .unwrap();

        let _ = db
            .transaction::<_, ()>(|tx| {
                tx.create_handoff(NewHandoff {
                    run_id: run.id,
                    work_item_id: wi.id,
                    ready_for: "qa",
                    summary: "doomed",
                    changed_files: None,
                    tests_run: None,
                    evidence: None,
                    known_risks: None,
                    now,
                })?;
                Err(crate::StateError::Sqlite(
                    rusqlite::Error::QueryReturnedNoRows,
                ))
            })
            .expect_err("forced rollback");

        assert!(db.list_handoffs_for_work_item(wi.id).unwrap().is_empty());
    }
}
