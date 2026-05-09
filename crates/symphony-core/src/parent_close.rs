//! Parent-completion guard (SPEC v2 §5.10 `require_all_children_terminal`).
//!
//! A decomposed parent MUST NOT close while any *required* child is still
//! in a non-terminal status class. SPEC v2 §5.10 phrases the rule as
//! "every child is done or intentionally waived". This module owns the
//! pure check so the same logic is consulted by the integration-owner
//! flow (Phase 8), the QA flow (Phase 9), and any future operator-facing
//! "force close" command — there is exactly one place where the rule
//! lives and exactly one place where it can be reasoned about.
//!
//! Design notes:
//!
//! * The guard takes a `target` [`WorkItemStatusClass`] so the same call
//!   site that wants to write `Done` or `Cancelled` flows through the
//!   same check. Non-terminal targets are rejected up-front; the guard
//!   has no opinion about transient states because those do not "close"
//!   the parent.
//! * Each child carries a `required` flag. `false` encodes the "waived"
//!   case from SPEC §5.10 — the kernel decides waivers at the
//!   integration-owner layer (with policy-driven waiver roles); by the
//!   time a snapshot reaches this guard, that decision is already
//!   reflected as `required: false`. Keeping the guard agnostic about
//!   *why* a child is waived avoids smuggling QA / integration policy
//!   into a primitive close check.
//! * Errors carry the offending children verbatim so the caller can
//!   surface them in tracker comments, blocker payloads, or operator
//!   output without recomputing the diff.

use crate::work_item::{WorkItemId, WorkItemStatusClass};

/// Minimal view of a child work item required by the parent-close guard.
///
/// The kernel materialises one of these per `parent_child` edge before
/// calling [`check_parent_can_close`]. Production callers will source
/// `status_class` from the durable `work_items.status_class` column;
/// tests can construct snapshots inline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildSnapshot {
    /// Domain primary key of the child work item.
    pub id: WorkItemId,
    /// Human-facing tracker identifier (e.g. `OWNER/REPO#42`).
    pub identifier: String,
    /// Normalized status class of the child at snapshot time.
    pub status_class: WorkItemStatusClass,
    /// `true` when the child must be terminal before the parent may
    /// close; `false` when the integration owner has explicitly waived
    /// it (SPEC §5.10).
    pub required: bool,
}

/// Errors produced by [`check_parent_can_close`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParentCloseError {
    /// The proposed target status class is not terminal. The guard only
    /// inspects "close" transitions; non-terminal targets are caller
    /// bugs.
    #[error("target status class `{0}` is not a terminal class")]
    TargetNotTerminal(WorkItemStatusClass),
    /// At least one required child is still non-terminal. The carried
    /// list preserves caller-supplied order so error messages remain
    /// stable across runs.
    #[error("{} required child(ren) still non-terminal", non_terminal.len())]
    ChildrenNotTerminal {
        /// The non-terminal required children, in caller-supplied order.
        non_terminal: Vec<ChildSnapshot>,
    },
}

/// Return `Ok(())` iff the parent may transition into `target`.
///
/// Rules (SPEC v2 §5.10):
///
/// 1. `target` MUST be a terminal class
///    ([`WorkItemStatusClass::is_terminal`]). Non-terminal targets are
///    a programmer error and surface as
///    [`ParentCloseError::TargetNotTerminal`].
/// 2. Every child with `required = true` MUST be in a terminal status
///    class. Children with `required = false` are treated as
///    intentionally waived per SPEC §5.10 and ignored.
///
/// Empty `children` is allowed: a parent with no recorded decomposition
/// children naturally satisfies the gate, and the caller is responsible
/// for ensuring the snapshot list reflects the canonical
/// `parent_child` edges.
pub fn check_parent_can_close(
    target: WorkItemStatusClass,
    children: &[ChildSnapshot],
) -> Result<(), ParentCloseError> {
    if !target.is_terminal() {
        return Err(ParentCloseError::TargetNotTerminal(target));
    }
    let non_terminal: Vec<ChildSnapshot> = children
        .iter()
        .filter(|c| c.required && !c.status_class.is_terminal())
        .cloned()
        .collect();
    if !non_terminal.is_empty() {
        return Err(ParentCloseError::ChildrenNotTerminal { non_terminal });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(id: i64, ident: &str, class: WorkItemStatusClass, required: bool) -> ChildSnapshot {
        ChildSnapshot {
            id: WorkItemId::new(id),
            identifier: ident.into(),
            status_class: class,
            required,
        }
    }

    #[test]
    fn parent_with_no_children_can_close_to_done() {
        check_parent_can_close(WorkItemStatusClass::Done, &[]).unwrap();
    }

    #[test]
    fn parent_with_no_children_can_close_to_cancelled() {
        check_parent_can_close(WorkItemStatusClass::Cancelled, &[]).unwrap();
    }

    #[test]
    fn non_terminal_target_is_rejected() {
        let err = check_parent_can_close(WorkItemStatusClass::Running, &[]).unwrap_err();
        assert_eq!(
            err,
            ParentCloseError::TargetNotTerminal(WorkItemStatusClass::Running)
        );
    }

    #[test]
    fn all_terminal_required_children_pass() {
        let kids = [
            snap(1, "ENG-2", WorkItemStatusClass::Done, true),
            snap(2, "ENG-3", WorkItemStatusClass::Cancelled, true),
        ];
        check_parent_can_close(WorkItemStatusClass::Done, &kids).unwrap();
    }

    #[test]
    fn one_running_required_child_blocks_close() {
        let kids = [
            snap(1, "ENG-2", WorkItemStatusClass::Done, true),
            snap(2, "ENG-3", WorkItemStatusClass::Running, true),
        ];
        let err = check_parent_can_close(WorkItemStatusClass::Done, &kids).unwrap_err();
        match err {
            ParentCloseError::ChildrenNotTerminal { non_terminal } => {
                assert_eq!(non_terminal.len(), 1);
                assert_eq!(non_terminal[0].identifier, "ENG-3");
                assert_eq!(non_terminal[0].status_class, WorkItemStatusClass::Running);
            }
            other => panic!("expected ChildrenNotTerminal, got {other:?}"),
        }
    }

    #[test]
    fn waived_non_required_non_terminal_child_is_ignored() {
        let kids = [
            snap(1, "ENG-2", WorkItemStatusClass::Done, true),
            snap(2, "ENG-3", WorkItemStatusClass::Blocked, false),
        ];
        check_parent_can_close(WorkItemStatusClass::Done, &kids).unwrap();
    }

    #[test]
    fn non_terminal_children_reported_in_input_order() {
        let kids = [
            snap(1, "ENG-2", WorkItemStatusClass::Running, true),
            snap(2, "ENG-3", WorkItemStatusClass::Done, true),
            snap(3, "ENG-4", WorkItemStatusClass::Blocked, true),
            snap(4, "ENG-5", WorkItemStatusClass::Qa, true),
        ];
        let err = check_parent_can_close(WorkItemStatusClass::Done, &kids).unwrap_err();
        let ParentCloseError::ChildrenNotTerminal { non_terminal } = err else {
            panic!("expected ChildrenNotTerminal");
        };
        let idents: Vec<_> = non_terminal.iter().map(|c| c.identifier.as_str()).collect();
        assert_eq!(idents, vec!["ENG-2", "ENG-4", "ENG-5"]);
    }

    #[test]
    fn ignore_status_class_does_not_count_as_terminal() {
        // `Ignore` is non-terminal per WorkItemStatusClass::is_terminal —
        // a parent with a child stuck in `ignore` would otherwise sneak
        // through. Lock the behavior in.
        let kids = [snap(1, "ENG-2", WorkItemStatusClass::Ignore, true)];
        let err = check_parent_can_close(WorkItemStatusClass::Done, &kids).unwrap_err();
        match err {
            ParentCloseError::ChildrenNotTerminal { non_terminal } => {
                assert_eq!(non_terminal.len(), 1);
                assert_eq!(non_terminal[0].status_class, WorkItemStatusClass::Ignore);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn mix_of_waived_and_required_only_reports_required() {
        let kids = [
            snap(1, "ENG-2", WorkItemStatusClass::Running, false), // waived; ignored
            snap(2, "ENG-3", WorkItemStatusClass::Running, true),  // required; reported
        ];
        let err = check_parent_can_close(WorkItemStatusClass::Cancelled, &kids).unwrap_err();
        let ParentCloseError::ChildrenNotTerminal { non_terminal } = err else {
            panic!("expected ChildrenNotTerminal");
        };
        assert_eq!(non_terminal.len(), 1);
        assert_eq!(non_terminal[0].identifier, "ENG-3");
    }

    #[test]
    fn error_message_for_target_not_terminal_names_the_class() {
        let err = check_parent_can_close(WorkItemStatusClass::Qa, &[]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("qa"), "message should embed class name: {msg}");
    }

    #[test]
    fn error_message_for_children_includes_count() {
        let kids = [
            snap(1, "ENG-2", WorkItemStatusClass::Running, true),
            snap(2, "ENG-3", WorkItemStatusClass::Blocked, true),
        ];
        let err = check_parent_can_close(WorkItemStatusClass::Done, &kids).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("2"), "message should include count: {msg}");
    }
}
