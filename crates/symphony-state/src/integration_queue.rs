//! Integration queue: durable read view of work items ready for the
//! integration-owner role (SPEC v2 §6.4, ARCH §6.2).
//!
//! The queue is *not* a separate table. It is a query over `work_items`
//! and `work_item_edges` that returns parent items the integration-owner
//! scheduler should consider next. Keeping the queue as a query (rather
//! than a materialized table) means there is no cache-coherence problem
//! between work-item state and queue membership: the moment a child
//! reaches a terminal status or a blocker is filed, the next call to
//! [`IntegrationQueueRepository::list_ready_for_integration`] reflects it.
//!
//! A work item is *ready for integration* when both hold:
//!
//! 1. The work item itself is not in a terminal status class
//!    (`done` / `cancelled`), and
//! 2. **Either**
//!    * its `status_class = 'integration'` — i.e. a specialist signalled
//!      `ready_for: integration` and the kernel has already promoted the
//!      item, *or*
//!    * it has at least one `parent_child` edge and every child it
//!      decomposes into is in a terminal status class (`done` or
//!      `cancelled`).
//!
//! In addition, the parent must have no open `blocks` edge anywhere in
//! its subtree. This mirrors the parent-close gate from Phase 5; the
//! integration owner refuses to consolidate a tree with unresolved
//! blockers (SPEC v2 §6.4, §5.10 `require_no_open_blockers`).
//!
//! The two readiness causes are surfaced separately as
//! [`IntegrationQueueCause`] so the scheduler can record *why* an item
//! entered the queue when emitting events.
//!
//! ## Gate waivers
//!
//! Both readiness gates correspond to the workflow-level switches
//! `IntegrationConfig.require_all_children_terminal` and
//! `IntegrationConfig.require_no_open_blockers` from SPEC v2 §5.10. The
//! defaults match the SPEC: both gates *on*. A workflow may explicitly
//! waive either gate by passing an [`IntegrationQueueGates`] value to
//! [`IntegrationQueueRepository::list_ready_for_integration_with_gates`].
//! When the children-terminal gate is waived, the queue surfaces parents
//! that decompose into one or more children regardless of whether all
//! children have reached a terminal status — the integration owner is
//! then trusted to consolidate whatever has landed. When the blocker
//! gate is waived, the queue stops excluding parents whose subtree has
//! open `blocks` edges. The legacy
//! [`IntegrationQueueRepository::list_ready_for_integration`] keeps the
//! both-gates-on behavior so existing callers do not change.

use rusqlite::Connection;

use crate::repository::{WORK_ITEM_COLUMNS, WorkItemId, WorkItemRecord, map_work_item};
use crate::{StateDb, StateResult};

/// Why a work item is in the integration queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IntegrationQueueCause {
    /// The work item itself is in `status_class = 'integration'` —
    /// typically because a specialist's handoff signalled
    /// `ready_for: integration` and the kernel promoted the item.
    DirectIntegrationRequest,
    /// The work item decomposes into one or more children and every
    /// child has reached a terminal status. The integration owner is
    /// expected to consolidate the children's outputs.
    AllChildrenTerminal,
}

impl IntegrationQueueCause {
    /// Stable lowercase identifier used in event payloads and tests.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DirectIntegrationRequest => "direct_integration_request",
            Self::AllChildrenTerminal => "all_children_terminal",
        }
    }
}

/// One queue entry: the parent work item and why it qualified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrationQueueEntry {
    /// The work item the integration owner should pick up.
    pub work_item: WorkItemRecord,
    /// Why this item qualifies right now.
    pub cause: IntegrationQueueCause,
}

impl IntegrationQueueEntry {
    /// Convenience accessor for the parent work item id.
    pub fn id(&self) -> WorkItemId {
        self.work_item.id
    }
}

/// Workflow-level gate switches that decide which parents the queue
/// surfaces (SPEC v2 §5.10).
///
/// Defaults match the SPEC: both gates *on* — children must be terminal
/// and the subtree must have no open blockers. A workflow may waive
/// either gate by setting the corresponding field to `false`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntegrationQueueGates {
    /// When `true` (default), parents with non-terminal children are
    /// hidden from the decomposed-cause source. When `false`, the gate
    /// is waived and decomposed parents surface as soon as they have at
    /// least one `parent_child` edge.
    pub require_all_children_terminal: bool,
    /// When `true` (default), any open `blocks` edge anywhere in the
    /// parent's subtree excludes the parent from the queue. When
    /// `false`, the gate is waived and the blocker-subtree check is
    /// skipped.
    pub require_no_open_blockers: bool,
}

impl Default for IntegrationQueueGates {
    fn default() -> Self {
        Self {
            require_all_children_terminal: true,
            require_no_open_blockers: true,
        }
    }
}

/// Read-only view over [`integration queue`](self) candidates.
pub trait IntegrationQueueRepository {
    /// Return every work item currently ready for the integration owner
    /// under the SPEC defaults (both gates on), ordered FIFO by
    /// `created_at` then `id` for determinism.
    fn list_ready_for_integration(&self) -> StateResult<Vec<IntegrationQueueEntry>> {
        self.list_ready_for_integration_with_gates(IntegrationQueueGates::default())
    }

    /// Return every work item ready for the integration owner under the
    /// supplied [`IntegrationQueueGates`]. Workflows pass a non-default
    /// value to explicitly waive the children-terminal or open-blocker
    /// gate.
    fn list_ready_for_integration_with_gates(
        &self,
        gates: IntegrationQueueGates,
    ) -> StateResult<Vec<IntegrationQueueEntry>>;
}

const TERMINAL_CLASSES: &str = "('done','cancelled')";

pub(crate) fn list_ready_for_integration_in(
    conn: &Connection,
    gates: IntegrationQueueGates,
) -> StateResult<Vec<IntegrationQueueEntry>> {
    // Two sources, unioned with their cause label, then filtered by the
    // open-blocker subtree exclusion. The recursive CTE walks
    // `parent_child` descendants of each candidate parent (inclusive)
    // and counts open `blocks` edges; non-zero counts are dropped. Each
    // gate filter is conditionally elided when the workflow has waived
    // the corresponding gate via `IntegrationQueueGates`.
    let children_terminal_filter = if gates.require_all_children_terminal {
        format!(
            "AND NOT EXISTS ( \
                 SELECT 1 \
                   FROM work_item_edges e \
                   JOIN work_items c ON c.id = e.child_id \
                  WHERE e.parent_id = w.id \
                    AND e.edge_type = 'parent_child' \
                    AND c.status_class NOT IN {terminal} \
             )",
            terminal = TERMINAL_CLASSES,
        )
    } else {
        String::new()
    };
    let blocker_filter = if gates.require_no_open_blockers {
        "AND NOT EXISTS ( \
             WITH RECURSIVE descendants(id) AS ( \
                 SELECT w.id \
                 UNION \
                 SELECT e.child_id \
                   FROM work_item_edges e \
                   JOIN descendants d ON e.parent_id = d.id \
                  WHERE e.edge_type = 'parent_child' \
             ) \
             SELECT 1 \
               FROM work_item_edges b \
              WHERE b.edge_type = 'blocks' \
                AND b.status = 'open' \
                AND b.child_id IN (SELECT id FROM descendants) \
         )"
        .to_string()
    } else {
        String::new()
    };
    let sql = format!(
        "WITH \
         direct AS ( \
             SELECT id AS parent_id, 'direct_integration_request' AS cause \
               FROM work_items \
              WHERE status_class = 'integration' \
         ), \
         decomposed AS ( \
             SELECT w.id AS parent_id, 'all_children_terminal' AS cause \
               FROM work_items w \
              WHERE w.status_class NOT IN {terminal} \
                AND w.id IN ( \
                    SELECT DISTINCT e.parent_id \
                      FROM work_item_edges e \
                     WHERE e.edge_type = 'parent_child' \
                ) \
                {children_terminal_filter} \
         ), \
         candidates AS ( \
             SELECT parent_id, cause FROM direct \
             UNION \
             SELECT parent_id, cause FROM decomposed \
                WHERE parent_id NOT IN (SELECT parent_id FROM direct) \
         ) \
         SELECT {cols}, c.cause \
           FROM candidates c \
           JOIN work_items w ON w.id = c.parent_id \
          WHERE w.status_class NOT IN {terminal} \
            {blocker_filter} \
          ORDER BY w.created_at ASC, w.id ASC",
        cols = prefixed_columns("w"),
        terminal = TERMINAL_CLASSES,
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        let work_item = map_work_item(row)?;
        let raw_cause: String = row.get(14)?;
        let cause = match raw_cause.as_str() {
            "direct_integration_request" => IntegrationQueueCause::DirectIntegrationRequest,
            "all_children_terminal" => IntegrationQueueCause::AllChildrenTerminal,
            other => {
                return Err(rusqlite::Error::FromSqlConversionFailure(
                    14,
                    rusqlite::types::Type::Text,
                    format!("unknown integration queue cause `{other}`").into(),
                ));
            }
        };
        Ok(IntegrationQueueEntry { work_item, cause })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn prefixed_columns(prefix: &str) -> String {
    WORK_ITEM_COLUMNS
        .split(", ")
        .map(|c| format!("{prefix}.{c}"))
        .collect::<Vec<_>>()
        .join(", ")
}

impl IntegrationQueueRepository for StateDb {
    fn list_ready_for_integration_with_gates(
        &self,
        gates: IntegrationQueueGates,
    ) -> StateResult<Vec<IntegrationQueueEntry>> {
        list_ready_for_integration_in(self.conn(), gates)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edges::{EdgeType, NewWorkItemEdge, WorkItemEdgeRepository};
    use crate::migrations::migrations;
    use crate::repository::{NewWorkItem, WorkItemRepository};

    fn open() -> StateDb {
        let mut db = StateDb::open_in_memory().expect("open");
        db.migrate(migrations()).expect("migrate");
        db
    }

    fn seed_item(
        db: &mut StateDb,
        identifier: &str,
        status_class: &str,
        created_at: &str,
    ) -> WorkItemId {
        db.create_work_item(NewWorkItem {
            tracker_id: "github",
            identifier,
            parent_id: None,
            title: "demo",
            status_class,
            tracker_status: "open",
            assigned_role: None,
            assigned_agent: None,
            priority: None,
            workspace_policy: None,
            branch_policy: None,
            now: created_at,
        })
        .expect("seed work item")
        .id
    }

    fn link_parent_child(db: &mut StateDb, parent: WorkItemId, child: WorkItemId) {
        db.create_edge(NewWorkItemEdge {
            parent_id: parent,
            child_id: child,
            edge_type: EdgeType::ParentChild,
            reason: None,
            status: "linked",
            now: "2026-05-08T00:00:00Z",
        })
        .expect("link parent_child");
    }

    fn open_blocker(db: &mut StateDb, blocker: WorkItemId, blocked: WorkItemId) {
        db.create_edge(NewWorkItemEdge {
            parent_id: blocker,
            child_id: blocked,
            edge_type: EdgeType::Blocks,
            reason: Some("test"),
            status: "open",
            now: "2026-05-08T00:00:00Z",
        })
        .expect("open blocker");
    }

    #[test]
    fn empty_db_returns_empty_queue() {
        let db = open();
        assert!(db.list_ready_for_integration().unwrap().is_empty());
    }

    #[test]
    fn direct_integration_status_appears_in_queue() {
        let mut db = open();
        let id = seed_item(&mut db, "ENG-1", "integration", "2026-05-08T00:00:00Z");
        let queue = db.list_ready_for_integration().unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].id(), id);
        assert_eq!(
            queue[0].cause,
            IntegrationQueueCause::DirectIntegrationRequest
        );
    }

    #[test]
    fn parent_with_all_children_terminal_appears_in_queue() {
        let mut db = open();
        let parent = seed_item(&mut db, "ENG-1", "running", "2026-05-08T00:00:00Z");
        let c1 = seed_item(&mut db, "ENG-2", "done", "2026-05-08T00:00:01Z");
        let c2 = seed_item(&mut db, "ENG-3", "cancelled", "2026-05-08T00:00:02Z");
        link_parent_child(&mut db, parent, c1);
        link_parent_child(&mut db, parent, c2);

        let queue = db.list_ready_for_integration().unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].id(), parent);
        assert_eq!(queue[0].cause, IntegrationQueueCause::AllChildrenTerminal);
    }

    #[test]
    fn parent_with_one_unfinished_child_is_skipped() {
        let mut db = open();
        let parent = seed_item(&mut db, "ENG-1", "running", "2026-05-08T00:00:00Z");
        let c1 = seed_item(&mut db, "ENG-2", "done", "2026-05-08T00:00:01Z");
        let c2 = seed_item(&mut db, "ENG-3", "running", "2026-05-08T00:00:02Z");
        link_parent_child(&mut db, parent, c1);
        link_parent_child(&mut db, parent, c2);
        assert!(db.list_ready_for_integration().unwrap().is_empty());
    }

    #[test]
    fn terminal_parent_is_not_requeued() {
        let mut db = open();
        let parent = seed_item(&mut db, "ENG-1", "done", "2026-05-08T00:00:00Z");
        let child = seed_item(&mut db, "ENG-2", "done", "2026-05-08T00:00:01Z");
        link_parent_child(&mut db, parent, child);
        assert!(db.list_ready_for_integration().unwrap().is_empty());
    }

    #[test]
    fn open_blocker_in_subtree_excludes_parent() {
        let mut db = open();
        let parent = seed_item(&mut db, "ENG-1", "integration", "2026-05-08T00:00:00Z");
        let blocker = seed_item(&mut db, "ENG-9", "blocked", "2026-05-08T00:00:05Z");
        open_blocker(&mut db, blocker, parent);

        assert!(db.list_ready_for_integration().unwrap().is_empty());
    }

    #[test]
    fn open_blocker_on_descendant_excludes_parent() {
        let mut db = open();
        let parent = seed_item(&mut db, "ENG-1", "running", "2026-05-08T00:00:00Z");
        let child = seed_item(&mut db, "ENG-2", "done", "2026-05-08T00:00:01Z");
        link_parent_child(&mut db, parent, child);
        let blocker = seed_item(&mut db, "ENG-9", "blocked", "2026-05-08T00:00:02Z");
        open_blocker(&mut db, blocker, child);

        assert!(db.list_ready_for_integration().unwrap().is_empty());
    }

    #[test]
    fn resolved_blocker_does_not_exclude_parent() {
        let mut db = open();
        let parent = seed_item(&mut db, "ENG-1", "integration", "2026-05-08T00:00:00Z");
        let blocker = seed_item(&mut db, "ENG-9", "done", "2026-05-08T00:00:05Z");
        let edge = db
            .create_edge(NewWorkItemEdge {
                parent_id: blocker,
                child_id: parent,
                edge_type: EdgeType::Blocks,
                reason: Some("test"),
                status: "open",
                now: "2026-05-08T00:00:00Z",
            })
            .unwrap();
        assert!(db.update_edge_status(edge.id, "resolved").unwrap());

        let queue = db.list_ready_for_integration().unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].id(), parent);
    }

    #[test]
    fn direct_integration_takes_precedence_over_decomposed_cause() {
        // A parent that is BOTH in `integration` status AND has all
        // children terminal should appear once with the
        // direct-integration cause (the kernel-promoted state is the
        // load-bearing signal).
        let mut db = open();
        let parent = seed_item(&mut db, "ENG-1", "integration", "2026-05-08T00:00:00Z");
        let child = seed_item(&mut db, "ENG-2", "done", "2026-05-08T00:00:01Z");
        link_parent_child(&mut db, parent, child);

        let queue = db.list_ready_for_integration().unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].id(), parent);
        assert_eq!(
            queue[0].cause,
            IntegrationQueueCause::DirectIntegrationRequest
        );
    }

    #[test]
    fn fifo_ordering_by_created_at() {
        let mut db = open();
        // Two independent items in `integration`. Older first.
        let older = seed_item(&mut db, "ENG-1", "integration", "2026-05-08T00:00:00Z");
        let newer = seed_item(&mut db, "ENG-2", "integration", "2026-05-08T01:00:00Z");
        let queue = db.list_ready_for_integration().unwrap();
        assert_eq!(queue.len(), 2);
        assert_eq!(queue[0].id(), older);
        assert_eq!(queue[1].id(), newer);
    }

    #[test]
    fn parent_with_no_children_and_non_integration_status_is_skipped() {
        let mut db = open();
        seed_item(&mut db, "ENG-1", "running", "2026-05-08T00:00:00Z");
        assert!(db.list_ready_for_integration().unwrap().is_empty());
    }

    #[test]
    fn waived_children_terminal_gate_surfaces_parent_with_unfinished_child() {
        let mut db = open();
        let parent = seed_item(&mut db, "ENG-1", "running", "2026-05-08T00:00:00Z");
        let c1 = seed_item(&mut db, "ENG-2", "done", "2026-05-08T00:00:01Z");
        let c2 = seed_item(&mut db, "ENG-3", "running", "2026-05-08T00:00:02Z");
        link_parent_child(&mut db, parent, c1);
        link_parent_child(&mut db, parent, c2);

        // Default gates: parent is hidden because ENG-3 is still running.
        assert!(db.list_ready_for_integration().unwrap().is_empty());

        // Waiver: workflow opts out of the children-terminal gate.
        let waived = IntegrationQueueGates {
            require_all_children_terminal: false,
            require_no_open_blockers: true,
        };
        let queue = db.list_ready_for_integration_with_gates(waived).unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].id(), parent);
        assert_eq!(queue[0].cause, IntegrationQueueCause::AllChildrenTerminal);
    }

    #[test]
    fn waived_blocker_gate_surfaces_parent_with_open_blocker() {
        let mut db = open();
        let parent = seed_item(&mut db, "ENG-1", "integration", "2026-05-08T00:00:00Z");
        let blocker = seed_item(&mut db, "ENG-9", "blocked", "2026-05-08T00:00:05Z");
        open_blocker(&mut db, blocker, parent);

        // Default gates: parent is hidden because the subtree has an open blocker.
        assert!(db.list_ready_for_integration().unwrap().is_empty());

        // Waiver: workflow opts out of the open-blocker gate.
        let waived = IntegrationQueueGates {
            require_all_children_terminal: true,
            require_no_open_blockers: false,
        };
        let queue = db.list_ready_for_integration_with_gates(waived).unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].id(), parent);
        assert_eq!(
            queue[0].cause,
            IntegrationQueueCause::DirectIntegrationRequest
        );
    }

    #[test]
    fn waiving_only_one_gate_does_not_waive_the_other() {
        let mut db = open();
        // Parent has a non-terminal child AND an open blocker on the child.
        let parent = seed_item(&mut db, "ENG-1", "running", "2026-05-08T00:00:00Z");
        let child = seed_item(&mut db, "ENG-2", "running", "2026-05-08T00:00:01Z");
        link_parent_child(&mut db, parent, child);
        let blocker = seed_item(&mut db, "ENG-9", "blocked", "2026-05-08T00:00:02Z");
        open_blocker(&mut db, blocker, child);

        // Waive only the children-terminal gate; the blocker still excludes.
        let waived = IntegrationQueueGates {
            require_all_children_terminal: false,
            require_no_open_blockers: true,
        };
        assert!(
            db.list_ready_for_integration_with_gates(waived)
                .unwrap()
                .is_empty()
        );

        // Waive both: now the parent surfaces.
        let waived_both = IntegrationQueueGates {
            require_all_children_terminal: false,
            require_no_open_blockers: false,
        };
        let queue = db
            .list_ready_for_integration_with_gates(waived_both)
            .unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].id(), parent);
    }

    #[test]
    fn default_gates_match_spec() {
        let gates = IntegrationQueueGates::default();
        assert!(gates.require_all_children_terminal);
        assert!(gates.require_no_open_blockers);
    }

    #[test]
    fn cause_round_trips_through_as_str() {
        for cause in [
            IntegrationQueueCause::DirectIntegrationRequest,
            IntegrationQueueCause::AllChildrenTerminal,
        ] {
            let s = cause.as_str();
            let back = match s {
                "direct_integration_request" => IntegrationQueueCause::DirectIntegrationRequest,
                "all_children_terminal" => IntegrationQueueCause::AllChildrenTerminal,
                _ => unreachable!(),
            };
            assert_eq!(back, cause);
        }
    }
}
