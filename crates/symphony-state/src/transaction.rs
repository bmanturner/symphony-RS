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
    EdgeId, NewWorkItemEdge, WorkItemEdgeRecord, create_edge_in, get_edge_in,
    list_open_blockers_for_subtree_in, update_edge_status_in,
};
use crate::events::{EventRecord, NewEvent};
use crate::repository::{
    NewRun, NewWorkItem, RunId, RunRecord, WorkItemId, WorkItemRecord, create_run_in,
    create_work_item_in, get_run_in, get_work_item_in, update_run_status_in,
    update_work_item_status_in,
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
        use crate::edges::{EdgeType, NewWorkItemEdge, WorkItemEdgeRepository};

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
                    now,
                })?;
                let blocker_edge = tx.create_edge(NewWorkItemEdge {
                    parent_id: blocker.id,
                    child_id: child.id,
                    edge_type: EdgeType::Blocks,
                    reason: Some("needs schema"),
                    status: "open",
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
}
