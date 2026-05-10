//! Work-item-to-run cancellation cascade (SPEC v2 §4.5 / Phase 11.5).
//!
//! When an operator cancels a *work item* (e.g. `symphony cancel ENG-9`),
//! the kernel must do two things:
//!
//! 1. Record the work-item-level cancel intent in the
//!    [`crate::cancellation::CancellationQueue`] and durable mirror so
//!    the operator-visible parent stays cancelled across restarts.
//! 2. Cascade that intent to every in-flight child run so dispatch
//!    runners observe a per-run cancel at their next safe point.
//!
//! The first step is the responsibility of the CLI / operator surface
//! and the existing `CancelRequestRepository`. This module is the
//! second step: a pure cascade that, given a parent work-item cancel,
//! produces the set of per-run [`crate::cancellation::CancelRequest`]s
//! the composition root should enqueue.
//!
//! # Cascade scope
//!
//! Cancellation propagates across `WorkItemEdge::ParentChild` edges
//! (the same edge type the parent-completion gate reads). The cascade
//! is *transitive*: a cancel on a tracking issue with two layers of
//! decomposition still reaches every grandchild run. It is *not*
//! routed via `Blocks` or `RelatesTo` edges — those are workflow
//! gates, not workflow ownership; cancelling a tracking issue should
//! not silently cancel an unrelated work item that happens to gate it.
//!
//! Within the descendant set, only **non-terminal** runs receive a
//! per-run request. A run already in `Completed` / `Failed` /
//! `Cancelled` has nothing for the runner to abort, and enqueueing a
//! request against it would burn a slot in the durable
//! `cancel_requests` table for no observable effect.
//!
//! # Trait-based dependency surface
//!
//! The propagator is pure: it composes two read-only trait surfaces
//! provided by the composition root. Production wiring resolves these
//! against `WorkItemEdgeRepository::list_open_blockers_for_subtree`'s
//! sibling descendant query and
//! `RunRepository::list_runs_for_work_item`. Tests stub them with
//! in-memory fakes — the kernel logic stays free of `rusqlite`.
//!
//! Idempotency: if a per-run cancel already exists in the durable
//! mirror (from a previous propagation pass that crashed mid-fanout),
//! the composition root's `enqueue_cancel_request` collapses it to a
//! payload-replace. The propagator does not need to dedup; it always
//! returns the full descendant set.

use std::collections::BTreeSet;

use crate::blocker::RunRef;
use crate::cancellation::{CancelRequest, CancelSubject};
use crate::work_item::WorkItemId;

/// Source of `parent_child` descendants for a work item.
///
/// Production: backed by a recursive CTE over `work_item_edges` (see
/// `symphony-state::edges`). Tests: in-memory adjacency map.
///
/// The contract is **inclusive of the root**: callers expect the
/// parent's own work item id in the returned set so the cascade
/// covers any run still pinned to the parent (e.g. the
/// integration-owner run on a broad parent that hasn't fully
/// decomposed yet). Implementations that produce an exclusive result
/// must add the root themselves.
pub trait WorkItemDescendantSource {
    /// Return every work item id reachable from `root` via outgoing
    /// `parent_child` edges, *including* `root` itself. Order is
    /// implementation-defined; the propagator does not rely on it.
    fn descendants_via_parent_child(
        &self,
        root: WorkItemId,
    ) -> Result<Vec<WorkItemId>, PropagationError>;
}

/// Source of in-flight runs for a work item.
///
/// Production: backed by `RunRepository::list_runs_for_work_item`
/// filtered to non-terminal `runs.status`. Tests: in-memory map.
///
/// "In-flight" is defined by the trait implementor. The kernel
/// recommends filtering to runs whose status is not in
/// `{Completed, Failed, Cancelled}`; runs in `CancelRequested` are
/// still returned because the cooperative-cancel cycle hasn't
/// finished, and a fresh cascade nudge keeps the queue authoritative.
pub trait ActiveRunSource {
    /// Return the durable run ids currently considered in-flight for
    /// `work_item_id`. May be empty.
    fn active_runs_for_work_item(
        &self,
        work_item_id: WorkItemId,
    ) -> Result<Vec<RunRef>, PropagationError>;
}

/// Failure that bubbles up from the trait surface.
///
/// Wraps the storage-layer error as a string so the kernel does not
/// depend on `symphony-state::StateError`. The composition root maps
/// `StateError` into this at the trait boundary.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
#[error("cancellation propagator backend failure: {0}")]
pub struct PropagationError(pub String);

impl PropagationError {
    /// Convenience constructor for trait implementors.
    pub fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

/// Output of one cascade pass.
///
/// Carries the per-run cancel requests the composition root should
/// enqueue plus the descendant work-item set the cascade walked, so
/// observability surfaces can render "this parent cancelled N
/// descendants and N runs".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CascadeOutcome {
    /// Per-run cancel requests to enqueue, one per non-terminal run
    /// across the descendant set. Sorted by run id so the cascade is
    /// deterministic regardless of trait-implementation ordering.
    pub run_requests: Vec<CancelRequest>,
    /// Work-item ids the cascade walked (including the root). Sorted
    /// by id for the same determinism reason.
    pub descendants: Vec<WorkItemId>,
}

/// Cascade a work-item cancellation across the `parent_child` edge
/// graph into per-run cancel requests.
pub struct CancellationPropagator;

impl CancellationPropagator {
    /// Compute the per-run cancel requests to enqueue for a parent
    /// work-item cancel.
    ///
    /// Errors:
    ///
    /// * Returns `Err(PropagationError::cascade_root_mismatch)` when
    ///   `parent_request.subject` is not a [`CancelSubject::WorkItem`].
    ///   The propagator is for work-item cascades only; per-run
    ///   cancels do not need cascading.
    /// * Bubbles up any [`PropagationError`] from the descendant or
    ///   active-run source.
    ///
    /// `requested_at` and `reason` flow from the parent request.
    /// `requested_by` is rewritten to the cascade form
    /// `cascade:work_item:{id}` so the audit trail distinguishes a
    /// direct operator cancel from a propagated one. The original
    /// `requested_by` is preserved in the parent work-item request
    /// row in the durable table; only the per-run rows carry the
    /// cascade marker.
    pub fn cascade(
        parent_request: &CancelRequest,
        descendants: &dyn WorkItemDescendantSource,
        runs: &dyn ActiveRunSource,
    ) -> Result<CascadeOutcome, PropagationError> {
        let work_item_id = match parent_request.subject {
            CancelSubject::WorkItem { work_item_id } => work_item_id,
            CancelSubject::Run { .. } => {
                return Err(PropagationError::new(
                    "cancellation cascade requires a work-item subject; \
                     per-run requests do not propagate",
                ));
            }
        };

        let walked = descendants.descendants_via_parent_child(work_item_id)?;
        // Defensive: the trait contract says the root is included, but
        // a buggy implementation that returns an exclusive set should
        // not silently skip the root. Re-add it; BTreeSet handles
        // dedup + ordering for free.
        let mut descendant_set: BTreeSet<WorkItemId> = walked.into_iter().collect();
        descendant_set.insert(work_item_id);

        let cascade_actor = format!("cascade:work_item:{}", work_item_id.0);
        let mut all_runs: BTreeSet<RunRef> = BTreeSet::new();
        for descendant in &descendant_set {
            for run in runs.active_runs_for_work_item(*descendant)? {
                all_runs.insert(run);
            }
        }

        let run_requests = all_runs
            .into_iter()
            .map(|run_id| CancelRequest {
                subject: CancelSubject::run(run_id),
                reason: parent_request.reason.clone(),
                requested_by: cascade_actor.clone(),
                requested_at: parent_request.requested_at.clone(),
            })
            .collect();

        Ok(CascadeOutcome {
            run_requests,
            descendants: descendant_set.into_iter().collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    /// In-memory adjacency-map fakes for the two trait surfaces.
    #[derive(Default)]
    struct Fakes {
        children_of: HashMap<WorkItemId, Vec<WorkItemId>>,
        runs_of: HashMap<WorkItemId, Vec<RunRef>>,
        descendant_failure: Option<String>,
        run_failure: Option<String>,
    }

    impl Fakes {
        fn add_child(&mut self, parent: i64, child: i64) {
            self.children_of
                .entry(WorkItemId::new(parent))
                .or_default()
                .push(WorkItemId::new(child));
        }

        fn add_run(&mut self, work_item: i64, run_id: i64) {
            self.runs_of
                .entry(WorkItemId::new(work_item))
                .or_default()
                .push(RunRef::new(run_id));
        }
    }

    impl WorkItemDescendantSource for Fakes {
        fn descendants_via_parent_child(
            &self,
            root: WorkItemId,
        ) -> Result<Vec<WorkItemId>, PropagationError> {
            if let Some(msg) = &self.descendant_failure {
                return Err(PropagationError::new(msg.clone()));
            }
            // BFS, inclusive of root.
            let mut out = Vec::new();
            let mut stack = vec![root];
            while let Some(current) = stack.pop() {
                if out.contains(&current) {
                    continue;
                }
                out.push(current);
                if let Some(children) = self.children_of.get(&current) {
                    stack.extend(children.iter().copied());
                }
            }
            Ok(out)
        }
    }

    impl ActiveRunSource for Fakes {
        fn active_runs_for_work_item(
            &self,
            work_item_id: WorkItemId,
        ) -> Result<Vec<RunRef>, PropagationError> {
            if let Some(msg) = &self.run_failure {
                return Err(PropagationError::new(msg.clone()));
            }
            Ok(self.runs_of.get(&work_item_id).cloned().unwrap_or_default())
        }
    }

    fn parent_request(work_item_id: i64, reason: &str, requested_by: &str) -> CancelRequest {
        CancelRequest::for_work_item(
            WorkItemId::new(work_item_id),
            reason,
            requested_by,
            "2026-05-09T00:00:00Z",
        )
    }

    #[test]
    fn cascade_emits_per_run_request_for_each_active_run_in_descendant_tree() {
        // Tree:
        //   parent (10) — has run #100
        //   ├── child_a (11) — has run #101
        //   │   └── grand (13) — has run #103
        //   └── child_b (12) — no active runs
        let mut fakes = Fakes::default();
        fakes.add_child(10, 11);
        fakes.add_child(10, 12);
        fakes.add_child(11, 13);
        fakes.add_run(10, 100);
        fakes.add_run(11, 101);
        fakes.add_run(13, 103);

        let req = parent_request(10, "rolling back", "alice");
        let outcome = CancellationPropagator::cascade(&req, &fakes, &fakes).expect("cascade ok");

        let run_ids: Vec<i64> = outcome
            .run_requests
            .iter()
            .map(|r| match r.subject {
                CancelSubject::Run { run_id } => run_id.0,
                CancelSubject::WorkItem { .. } => panic!("cascade output must be run-keyed"),
            })
            .collect();
        assert_eq!(run_ids, vec![100, 101, 103]);

        let descendants: Vec<i64> = outcome.descendants.iter().map(|w| w.0).collect();
        assert_eq!(descendants, vec![10, 11, 12, 13]);
    }

    #[test]
    fn cascade_rewrites_requested_by_to_cascade_marker_and_preserves_reason_and_timestamp() {
        let mut fakes = Fakes::default();
        fakes.add_run(10, 100);

        let req = parent_request(10, "operator cancel", "alice");
        let outcome = CancellationPropagator::cascade(&req, &fakes, &fakes).unwrap();
        let only = &outcome.run_requests[0];
        assert_eq!(only.reason, "operator cancel");
        assert_eq!(only.requested_by, "cascade:work_item:10");
        assert_eq!(only.requested_at, "2026-05-09T00:00:00Z");
    }

    #[test]
    fn cascade_with_no_active_runs_returns_empty_run_set_but_still_walks_descendants() {
        let mut fakes = Fakes::default();
        fakes.add_child(10, 11);

        let req = parent_request(10, "no-op", "alice");
        let outcome = CancellationPropagator::cascade(&req, &fakes, &fakes).unwrap();
        assert!(outcome.run_requests.is_empty());
        assert_eq!(
            outcome.descendants,
            vec![WorkItemId::new(10), WorkItemId::new(11)]
        );
    }

    #[test]
    fn cascade_on_leaf_with_only_self_active_run_targets_only_self() {
        let mut fakes = Fakes::default();
        fakes.add_run(10, 100);

        let req = parent_request(10, "leaf cancel", "alice");
        let outcome = CancellationPropagator::cascade(&req, &fakes, &fakes).unwrap();
        assert_eq!(outcome.run_requests.len(), 1);
        assert_eq!(outcome.descendants, vec![WorkItemId::new(10)]);
    }

    #[test]
    fn cascade_rejects_run_subject_with_propagation_error() {
        let fakes = Fakes::default();
        let bad = CancelRequest::for_run(
            RunRef::new(1),
            "run-targeted cancel",
            "alice",
            "2026-05-09T00:00:00Z",
        );
        let err = CancellationPropagator::cascade(&bad, &fakes, &fakes).unwrap_err();
        assert!(err.0.contains("work-item subject"));
    }

    #[test]
    fn cascade_propagates_descendant_source_failure() {
        let fakes = Fakes {
            descendant_failure: Some("simulated db error".into()),
            ..Fakes::default()
        };
        let req = parent_request(10, "x", "alice");
        let err = CancellationPropagator::cascade(&req, &fakes, &fakes).unwrap_err();
        assert_eq!(err.0, "simulated db error");
    }

    #[test]
    fn cascade_propagates_active_run_source_failure() {
        let fakes = Fakes {
            run_failure: Some("simulated db error".into()),
            ..Fakes::default()
        };
        let req = parent_request(10, "x", "alice");
        let err = CancellationPropagator::cascade(&req, &fakes, &fakes).unwrap_err();
        assert_eq!(err.0, "simulated db error");
    }

    #[test]
    fn cascade_dedups_runs_reported_by_duplicate_descendants() {
        // A buggy descendant source could report the same work item
        // twice (e.g. a diamond-shaped graph through a future
        // edge type). The cascade must still produce one cancel per
        // run id.
        struct Doubled(Fakes);
        impl WorkItemDescendantSource for Doubled {
            fn descendants_via_parent_child(
                &self,
                root: WorkItemId,
            ) -> Result<Vec<WorkItemId>, PropagationError> {
                let mut out = self.0.descendants_via_parent_child(root)?;
                out.extend(out.clone());
                Ok(out)
            }
        }
        impl ActiveRunSource for Doubled {
            fn active_runs_for_work_item(
                &self,
                work_item_id: WorkItemId,
            ) -> Result<Vec<RunRef>, PropagationError> {
                self.0.active_runs_for_work_item(work_item_id)
            }
        }

        let mut inner = Fakes::default();
        inner.add_run(10, 100);
        let fakes = Doubled(inner);
        let req = parent_request(10, "x", "alice");
        let outcome = CancellationPropagator::cascade(&req, &fakes, &fakes).unwrap();
        assert_eq!(outcome.run_requests.len(), 1);
        assert_eq!(outcome.descendants, vec![WorkItemId::new(10)]);
    }

    #[test]
    fn cascade_adds_root_when_descendant_source_returns_exclusive_set() {
        // A buggy implementation that omits the root must not cause
        // the propagator to skip the parent's own active runs.
        struct Exclusive(Fakes);
        impl WorkItemDescendantSource for Exclusive {
            fn descendants_via_parent_child(
                &self,
                root: WorkItemId,
            ) -> Result<Vec<WorkItemId>, PropagationError> {
                let mut all = self.0.descendants_via_parent_child(root)?;
                all.retain(|id| *id != root);
                Ok(all)
            }
        }
        impl ActiveRunSource for Exclusive {
            fn active_runs_for_work_item(
                &self,
                work_item_id: WorkItemId,
            ) -> Result<Vec<RunRef>, PropagationError> {
                self.0.active_runs_for_work_item(work_item_id)
            }
        }

        let mut inner = Fakes::default();
        inner.add_child(10, 11);
        inner.add_run(10, 100);
        inner.add_run(11, 101);
        let fakes = Exclusive(inner);
        let req = parent_request(10, "x", "alice");
        let outcome = CancellationPropagator::cascade(&req, &fakes, &fakes).unwrap();
        let run_ids: Vec<i64> = outcome
            .run_requests
            .iter()
            .map(|r| match r.subject {
                CancelSubject::Run { run_id } => run_id.0,
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(run_ids, vec![100, 101]);
    }

    #[test]
    fn cascade_output_run_requests_are_sorted_by_run_id_for_determinism() {
        let mut fakes = Fakes::default();
        fakes.add_child(10, 11);
        fakes.add_child(10, 12);
        // Insertion order is not run-id order — the trait happens to
        // surface them in a different order than we want them out.
        fakes.add_run(11, 200);
        fakes.add_run(12, 100);
        fakes.add_run(10, 300);

        let req = parent_request(10, "x", "alice");
        let outcome = CancellationPropagator::cascade(&req, &fakes, &fakes).unwrap();
        let run_ids: Vec<i64> = outcome
            .run_requests
            .iter()
            .map(|r| match r.subject {
                CancelSubject::Run { run_id } => run_id.0,
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(run_ids, vec![100, 200, 300]);
    }
}
