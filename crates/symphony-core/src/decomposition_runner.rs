//! Integration-owner decomposition runner path (SPEC v2 §5.7).
//!
//! [`crate::decomposition`] supplies the durable proposal *shape* and its
//! per-row invariants (unique child keys, DAG-ness, parent self-reference,
//! lifecycle transitions). This module supplies the *workflow-policy seam*
//! that sits between an authored proposal and the kernel:
//!
//! 1. **Eligibility.** Given a parent [`WorkItem`] and a per-call
//!    [`DecompositionContext`] (depth, pre-flight estimates), decide
//!    whether the integration owner should be asked to decompose at all.
//!    Mirrors the SPEC §5.7 trigger surface (`labels_any`,
//!    `estimated_files_over`, `acceptance_items_over`) and the master
//!    `enabled` switch.
//! 2. **Acceptance.** Given an authored draft (summary + child drafts),
//!    enforce the cross-cutting policy invariants the proposal type
//!    cannot see on its own:
//!
//!      * `enabled: true` and `owner_role` populated;
//!      * the author role matches the configured `owner_role`;
//!      * `parent_depth + 1 <= max_depth`;
//!      * `require_acceptance_criteria_per_child` is honoured (delegated
//!        to [`ChildProposal::try_new`]).
//!
//!    Successful acceptance allocates an id and returns a freshly
//!    constructed [`DecompositionProposal`] with status derived from
//!    `child_issue_policy`.
//!
//! The runner is intentionally pure: no I/O, no agent invocation, no
//! tracker mutation. Phase 5's remaining checklist items wire it into the
//! agent runner output path and `TrackerMutations` child issue creation.
//! Keeping this seam pure means routing tests, recovery tests, and
//! property tests can drive the full eligibility/acceptance contract
//! without spinning up a fake agent.

use serde::{Deserialize, Serialize};

use crate::decomposition::{
    ChildKey, ChildProposal, DecompositionError, DecompositionId, DecompositionProposal,
};
use crate::followup::FollowupPolicy;
use crate::role::RoleName;
use crate::work_item::{WorkItem, WorkItemId};

/// Mirror of `symphony_config::DecompositionTriggers` in domain shape.
///
/// Each field is optional/empty by default and additive (OR-of-fields):
/// the runner reports a [`DecompositionEligibility::Decompose`] outcome
/// when *any* configured trigger fires. The crate-seam pattern matches
/// [`crate::routing::RoutingMatch`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecompositionTriggers {
    /// Trigger when the parent work item carries any of these labels.
    /// Compared case-insensitively against [`WorkItem::labels`] to match
    /// the SPEC §11.3 normalization rule.
    #[serde(default)]
    pub labels_any: Vec<String>,
    /// Trigger when the integration owner's pre-flight file-count
    /// estimate strictly exceeds this value. `None` means the trigger is
    /// not configured.
    #[serde(default)]
    pub estimated_files_over: Option<u32>,
    /// Trigger when the parent's acceptance-criteria count strictly
    /// exceeds this value. `None` means the trigger is not configured.
    #[serde(default)]
    pub acceptance_items_over: Option<u32>,
}

impl DecompositionTriggers {
    /// True when no trigger is configured. An empty trigger set never
    /// fires — matching the SPEC §5.7 contract that omitted triggers
    /// disable auto-decomposition even with `enabled: true`.
    pub fn is_empty(&self) -> bool {
        self.labels_any.is_empty()
            && self.estimated_files_over.is_none()
            && self.acceptance_items_over.is_none()
    }
}

/// Domain-side mirror of `symphony_config::DecompositionConfig`.
///
/// Translation lives at the config-load seam (`symphony-cli` /
/// orchestrator wiring) so this crate stays independent of YAML/serde
/// quirks. The runner reasons about a [`FollowupPolicy`] for
/// `child_issue_policy` to share the exact enum
/// [`DecompositionProposal::policy`] uses, eliminating a redundant
/// translation step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecompositionPolicy {
    /// Master switch. When `false`, [`DecompositionRunner::eligibility`]
    /// always reports [`DecompositionEligibility::Skipped`].
    pub enabled: bool,
    /// Role responsible for authoring proposals. `None` is treated the
    /// same as `enabled: false`: the runner cannot proceed without a
    /// concrete owner role to validate authorship against.
    pub owner_role: Option<RoleName>,
    /// Trigger surface — see [`DecompositionTriggers`].
    pub triggers: DecompositionTriggers,
    /// What to do once a proposal is accepted. Drives the resulting
    /// proposal's initial [`crate::DecompositionStatus`] via
    /// [`crate::DecompositionStatus::initial_for`].
    pub child_issue_policy: FollowupPolicy,
    /// Maximum decomposition depth. A parent at depth `d` may be
    /// decomposed only when `d + 1 <= max_depth`. SPEC §5.7 default of
    /// `2` permits parent → child → grandchild and bounds recursion.
    pub max_depth: u32,
    /// Per-child acceptance-criteria requirement. Forwarded to
    /// [`ChildProposal::try_new`] for each draft.
    pub require_acceptance_criteria_per_child: bool,
}

impl Default for DecompositionPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            owner_role: None,
            triggers: DecompositionTriggers::default(),
            child_issue_policy: FollowupPolicy::ProposeForApproval,
            max_depth: 2,
            require_acceptance_criteria_per_child: true,
        }
    }
}

/// Per-call ambient context that does not live on the [`WorkItem`].
///
/// `parent_depth` is the parent's existing decomposition depth (root
/// issues are depth `0`); the runner enforces
/// `parent_depth + 1 <= max_depth`. The two estimate fields come from
/// the integration owner's pre-flight scan and are matched against the
/// trigger thresholds in the runner — keeping them out of [`WorkItem`]
/// preserves the domain type's stability.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DecompositionContext {
    /// Depth of the parent within the decomposition tree (`0` for a
    /// root issue, `1` for a child of a previously-decomposed issue).
    pub parent_depth: u32,
    /// Pre-flight estimate of how many files the parent's work would
    /// touch. Compared against
    /// [`DecompositionTriggers::estimated_files_over`].
    pub estimated_files: Option<u32>,
    /// Count of acceptance-criteria items on the parent. Compared
    /// against [`DecompositionTriggers::acceptance_items_over`].
    pub acceptance_items: Option<u32>,
}

/// Why the runner skipped a candidate parent without producing a
/// proposal. Surfaced verbatim on operator surfaces (status, SSE) so an
/// operator can reason about *why* a broad-looking parent never reached
/// the integration-owner queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// `policy.enabled` is `false`.
    Disabled,
    /// `policy.owner_role` is `None`. Even with `enabled: true`, the
    /// runner cannot validate authorship without a concrete role.
    NoOwnerRole,
    /// `policy.triggers` was empty, so the runner cannot fire on any
    /// parent without explicit operator intent.
    NoTriggersConfigured,
    /// Triggers were configured but none fired against this parent.
    NoTriggerFired,
    /// `parent_depth + 1` would exceed `policy.max_depth`. Hits the
    /// recursion cap; the parent stays a specialist work item.
    DepthCapReached {
        /// The configured cap.
        max_depth: u32,
        /// Parent's pre-decomposition depth.
        parent_depth: u32,
    },
}

/// Why a configured trigger fired. Returned alongside
/// [`DecompositionEligibility::Decompose`] for diagnostics; an empty
/// vector cannot happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriggerHit {
    /// `labels_any` fired because the parent carries one of the listed
    /// labels (case-insensitive). Emits the matched label in its
    /// configured casing for operator-readable messages.
    Label(String),
    /// `estimated_files_over` fired because the pre-flight estimate
    /// strictly exceeded the threshold.
    EstimatedFiles {
        /// Configured threshold.
        threshold: u32,
        /// Observed pre-flight value.
        observed: u32,
    },
    /// `acceptance_items_over` fired because the parent's acceptance
    /// criteria list strictly exceeded the threshold.
    AcceptanceItems {
        /// Configured threshold.
        threshold: u32,
        /// Observed acceptance-criteria count.
        observed: u32,
    },
}

/// Outcome of [`DecompositionRunner::eligibility`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecompositionEligibility {
    /// At least one trigger fired and no skip condition applies.
    Decompose {
        /// Triggers that fired in declaration order.
        hits: Vec<TriggerHit>,
    },
    /// The parent was not eligible. Carries the reason for diagnostics.
    Skipped(SkipReason),
}

/// One child entry in a [`DecompositionDraft`].
///
/// Mirrors the constructor signature of [`ChildProposal::try_new`]
/// minus the `require_acceptance_criteria` knob, which the runner
/// supplies from its [`DecompositionPolicy`]. Drafts are plain data so
/// callers (the agent-output adapter, tests) can build them without
/// touching the proposal-construction path directly.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChildDraft {
    /// Proposal-local identifier. Forwarded to [`ChildProposal::key`].
    pub key: String,
    /// Tracker title. Forwarded to [`ChildProposal::title`].
    pub title: String,
    /// Bounded scope description. Forwarded to [`ChildProposal::scope`].
    pub scope: String,
    /// Role responsible for executing the child work.
    pub assigned_role: Option<RoleName>,
    /// Optional long-form description.
    pub description: Option<String>,
    /// Acceptance criteria bullets. Subject to
    /// `require_acceptance_criteria_per_child`.
    pub acceptance_criteria: Vec<String>,
    /// Other [`ChildKey`]s in the same draft this child depends on.
    pub depends_on: Vec<String>,
    /// Whether the child is required for parent closure. Defaults to
    /// `true` because SPEC §5.10 `integration.required_for` defaults
    /// include `required_children`.
    pub blocking: bool,
    /// Optional branch hint forwarded verbatim to the child.
    pub branch_hint: Option<String>,
}

impl ChildDraft {
    /// Convenience constructor that mirrors the *minimum* viable child:
    /// a key, a title, a scope, an assigned role, and at least one
    /// acceptance criterion. `blocking` defaults to `true` per SPEC
    /// §5.10.
    pub fn new(
        key: impl Into<String>,
        title: impl Into<String>,
        scope: impl Into<String>,
        assigned_role: RoleName,
    ) -> Self {
        Self {
            key: key.into(),
            title: title.into(),
            scope: scope.into(),
            assigned_role: Some(assigned_role),
            description: None,
            acceptance_criteria: Vec::new(),
            depends_on: Vec::new(),
            blocking: true,
            branch_hint: None,
        }
    }
}

/// Full authored proposal handed to the runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecompositionDraft {
    /// Parent work item being decomposed.
    pub parent: WorkItemId,
    /// Role that authored the draft. The runner asserts this matches
    /// [`DecompositionPolicy::owner_role`].
    pub author_role: RoleName,
    /// One-line strategy summary forwarded to the proposal.
    pub summary: String,
    /// Child entries.
    pub children: Vec<ChildDraft>,
}

/// Errors produced by the runner.
///
/// Wraps [`DecompositionError`] for proposal-construction failures and
/// adds policy-level variants for the cross-cutting checks that live in
/// this module (eligibility, owner-role mismatch, depth cap, missing
/// child role).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RunnerError {
    /// Caller invoked acceptance with a [`DecompositionPolicy`] whose
    /// `enabled` flag is `false`. Eligibility is the right place to
    /// detect this; surfaced as an error here so a misuse cannot
    /// silently apply a disabled policy.
    #[error("decomposition policy is disabled")]
    PolicyDisabled,
    /// Caller invoked acceptance with no `owner_role` configured.
    #[error("decomposition policy has no owner_role configured")]
    NoOwnerRoleConfigured,
    /// The draft's `author_role` does not match the configured owner.
    /// SPEC §5.7 names the integration owner as the author of every
    /// proposal; the runner enforces it at the call site rather than
    /// trusting the agent prompt.
    #[error("draft author role {author} does not match owner_role {owner}")]
    AuthorRoleMismatch {
        /// Author the agent reported.
        author: RoleName,
        /// Owner the workflow configured.
        owner: RoleName,
    },
    /// `parent_depth + 1 > max_depth`. Bounded recursion is a workflow
    /// invariant; the runner refuses rather than producing a proposal
    /// the kernel would have to reject downstream.
    #[error("decomposition would exceed max_depth ({max_depth}); parent at depth {parent_depth}")]
    MaxDepthExceeded {
        /// The configured cap.
        max_depth: u32,
        /// Parent's pre-decomposition depth.
        parent_depth: u32,
    },
    /// A child draft did not name an `assigned_role`. The runner cannot
    /// route the child without one; this is the kernel's analogue of
    /// the SPEC §5.7 "owners/roles" requirement applied per-child.
    #[error("child draft {key} did not name an assigned role")]
    ChildMissingRole {
        /// The offending child key.
        key: String,
    },
    /// Construction of a [`ChildProposal`] or [`DecompositionProposal`]
    /// failed. Carries the underlying [`DecompositionError`] verbatim so
    /// callers can pattern-match on the same domain errors regardless of
    /// the runner seam.
    #[error(transparent)]
    Domain(#[from] DecompositionError),
}

/// Pure runner over a fixed [`DecompositionPolicy`].
///
/// Like [`crate::routing::RoutingEngine`], the runner is constructed
/// once at config-load time and reused for every dispatch decision.
/// Methods take `&self` and borrow nothing transient.
#[derive(Debug, Clone)]
pub struct DecompositionRunner {
    policy: DecompositionPolicy,
}

impl DecompositionRunner {
    /// Build a runner from a resolved policy.
    pub fn new(policy: DecompositionPolicy) -> Self {
        Self { policy }
    }

    /// Borrow the underlying policy for inspection.
    pub fn policy(&self) -> &DecompositionPolicy {
        &self.policy
    }

    /// Decide whether the integration owner should be asked to
    /// decompose this parent. Pure function of `(policy, parent, ctx)`.
    pub fn eligibility(
        &self,
        parent: &WorkItem,
        ctx: &DecompositionContext,
    ) -> DecompositionEligibility {
        if !self.policy.enabled {
            return DecompositionEligibility::Skipped(SkipReason::Disabled);
        }
        if self.policy.owner_role.is_none() {
            return DecompositionEligibility::Skipped(SkipReason::NoOwnerRole);
        }
        if ctx.parent_depth.saturating_add(1) > self.policy.max_depth {
            return DecompositionEligibility::Skipped(SkipReason::DepthCapReached {
                max_depth: self.policy.max_depth,
                parent_depth: ctx.parent_depth,
            });
        }
        if self.policy.triggers.is_empty() {
            return DecompositionEligibility::Skipped(SkipReason::NoTriggersConfigured);
        }
        let hits = collect_trigger_hits(&self.policy.triggers, parent, ctx);
        if hits.is_empty() {
            return DecompositionEligibility::Skipped(SkipReason::NoTriggerFired);
        }
        DecompositionEligibility::Decompose { hits }
    }

    /// Validate an authored draft and assemble it into a durable
    /// [`DecompositionProposal`]. Idempotent against `id`: callers
    /// allocate the id (typically a `symphony-state` row id) and pass it
    /// in so this seam stays free of storage.
    pub fn accept(
        &self,
        id: DecompositionId,
        draft: DecompositionDraft,
        parent_depth: u32,
    ) -> Result<DecompositionProposal, RunnerError> {
        if !self.policy.enabled {
            return Err(RunnerError::PolicyDisabled);
        }
        let owner = self
            .policy
            .owner_role
            .as_ref()
            .ok_or(RunnerError::NoOwnerRoleConfigured)?;
        if &draft.author_role != owner {
            return Err(RunnerError::AuthorRoleMismatch {
                author: draft.author_role,
                owner: owner.clone(),
            });
        }
        if parent_depth.saturating_add(1) > self.policy.max_depth {
            return Err(RunnerError::MaxDepthExceeded {
                max_depth: self.policy.max_depth,
                parent_depth,
            });
        }

        let mut children = Vec::with_capacity(draft.children.len());
        for child in draft.children {
            let assigned = child
                .assigned_role
                .ok_or_else(|| RunnerError::ChildMissingRole {
                    key: child.key.clone(),
                })?;
            let depends_on = child.depends_on.into_iter().map(ChildKey::new).collect();
            let proposal_child = ChildProposal::try_new(
                child.key,
                child.title,
                child.scope,
                assigned,
                child.description,
                child.acceptance_criteria,
                depends_on,
                child.blocking,
                child.branch_hint,
                self.policy.require_acceptance_criteria_per_child,
            )?;
            children.push(proposal_child);
        }

        let proposal = DecompositionProposal::try_new(
            id,
            draft.parent,
            draft.author_role,
            draft.summary,
            children,
            self.policy.child_issue_policy,
        )?;
        Ok(proposal)
    }
}

fn collect_trigger_hits(
    triggers: &DecompositionTriggers,
    parent: &WorkItem,
    ctx: &DecompositionContext,
) -> Vec<TriggerHit> {
    let mut hits = Vec::new();
    if !triggers.labels_any.is_empty() {
        let parent_lower: Vec<String> = parent
            .labels
            .iter()
            .map(|l| l.to_ascii_lowercase())
            .collect();
        for label in &triggers.labels_any {
            if parent_lower
                .iter()
                .any(|l| l == &label.to_ascii_lowercase())
            {
                hits.push(TriggerHit::Label(label.clone()));
            }
        }
    }
    if let Some(threshold) = triggers.estimated_files_over
        && let Some(observed) = ctx.estimated_files
        && observed > threshold
    {
        hits.push(TriggerHit::EstimatedFiles {
            threshold,
            observed,
        });
    }
    if let Some(threshold) = triggers.acceptance_items_over
        && let Some(observed) = ctx.acceptance_items
        && observed > threshold
    {
        hits.push(TriggerHit::AcceptanceItems {
            threshold,
            observed,
        });
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decomposition::DecompositionStatus;
    use crate::work_item::{TrackerStatus, WorkItemStatusClass};

    fn parent_with(labels: &[&str]) -> WorkItem {
        WorkItem {
            id: WorkItemId::new(100),
            tracker_id: "github".into(),
            identifier: "OWNER/REPO#100".into(),
            title: "broad parent".into(),
            description: None,
            tracker_status: TrackerStatus::new("Todo"),
            status_class: WorkItemStatusClass::Intake,
            priority: None,
            labels: labels.iter().map(|s| s.to_string()).collect(),
            url: None,
            parent_id: None,
        }
    }

    fn enabled_policy(triggers: DecompositionTriggers) -> DecompositionPolicy {
        DecompositionPolicy {
            enabled: true,
            owner_role: Some(RoleName::new("platform_lead")),
            triggers,
            child_issue_policy: FollowupPolicy::ProposeForApproval,
            max_depth: 2,
            require_acceptance_criteria_per_child: true,
        }
    }

    fn child_draft(key: &str, deps: &[&str], criteria: &[&str]) -> ChildDraft {
        ChildDraft {
            key: key.into(),
            title: format!("title for {key}"),
            scope: format!("scope for {key}"),
            assigned_role: Some(RoleName::new("backend")),
            description: None,
            acceptance_criteria: criteria.iter().map(|s| (*s).to_string()).collect(),
            depends_on: deps.iter().map(|s| (*s).to_string()).collect(),
            blocking: true,
            branch_hint: None,
        }
    }

    fn minimal_draft() -> DecompositionDraft {
        DecompositionDraft {
            parent: WorkItemId::new(100),
            author_role: RoleName::new("platform_lead"),
            summary: "split parent".into(),
            children: vec![child_draft("a", &[], &["criterion"])],
        }
    }

    // ------------------------------------------------------------------ eligibility

    #[test]
    fn disabled_policy_skips() {
        let runner = DecompositionRunner::new(DecompositionPolicy::default());
        let decision =
            runner.eligibility(&parent_with(&["epic"]), &DecompositionContext::default());
        assert_eq!(
            decision,
            DecompositionEligibility::Skipped(SkipReason::Disabled)
        );
    }

    #[test]
    fn missing_owner_role_skips() {
        let mut policy = enabled_policy(DecompositionTriggers {
            labels_any: vec!["epic".into()],
            ..Default::default()
        });
        policy.owner_role = None;
        let runner = DecompositionRunner::new(policy);
        let decision =
            runner.eligibility(&parent_with(&["epic"]), &DecompositionContext::default());
        assert_eq!(
            decision,
            DecompositionEligibility::Skipped(SkipReason::NoOwnerRole)
        );
    }

    #[test]
    fn empty_triggers_skip_even_when_enabled() {
        let runner = DecompositionRunner::new(enabled_policy(DecompositionTriggers::default()));
        let decision =
            runner.eligibility(&parent_with(&["epic"]), &DecompositionContext::default());
        assert_eq!(
            decision,
            DecompositionEligibility::Skipped(SkipReason::NoTriggersConfigured)
        );
    }

    #[test]
    fn label_trigger_fires_case_insensitively() {
        let runner = DecompositionRunner::new(enabled_policy(DecompositionTriggers {
            labels_any: vec!["Epic".into()],
            ..Default::default()
        }));
        let decision = runner.eligibility(
            &parent_with(&["epic", "frontend"]),
            &DecompositionContext::default(),
        );
        assert_eq!(
            decision,
            DecompositionEligibility::Decompose {
                hits: vec![TriggerHit::Label("Epic".into())],
            }
        );
    }

    #[test]
    fn label_trigger_misses_when_no_label_matches() {
        let runner = DecompositionRunner::new(enabled_policy(DecompositionTriggers {
            labels_any: vec!["epic".into()],
            ..Default::default()
        }));
        let decision = runner.eligibility(&parent_with(&["bug"]), &DecompositionContext::default());
        assert_eq!(
            decision,
            DecompositionEligibility::Skipped(SkipReason::NoTriggerFired)
        );
    }

    #[test]
    fn file_estimate_trigger_uses_strictly_greater_than() {
        let runner = DecompositionRunner::new(enabled_policy(DecompositionTriggers {
            estimated_files_over: Some(5),
            ..Default::default()
        }));
        let parent = parent_with(&[]);

        let decision_eq = runner.eligibility(
            &parent,
            &DecompositionContext {
                parent_depth: 0,
                estimated_files: Some(5),
                acceptance_items: None,
            },
        );
        assert_eq!(
            decision_eq,
            DecompositionEligibility::Skipped(SkipReason::NoTriggerFired)
        );

        let decision_over = runner.eligibility(
            &parent,
            &DecompositionContext {
                parent_depth: 0,
                estimated_files: Some(6),
                acceptance_items: None,
            },
        );
        assert_eq!(
            decision_over,
            DecompositionEligibility::Decompose {
                hits: vec![TriggerHit::EstimatedFiles {
                    threshold: 5,
                    observed: 6,
                }],
            }
        );
    }

    #[test]
    fn acceptance_items_trigger_fires_strictly_over() {
        let runner = DecompositionRunner::new(enabled_policy(DecompositionTriggers {
            acceptance_items_over: Some(3),
            ..Default::default()
        }));
        let decision = runner.eligibility(
            &parent_with(&[]),
            &DecompositionContext {
                parent_depth: 0,
                estimated_files: None,
                acceptance_items: Some(4),
            },
        );
        assert_eq!(
            decision,
            DecompositionEligibility::Decompose {
                hits: vec![TriggerHit::AcceptanceItems {
                    threshold: 3,
                    observed: 4,
                }],
            }
        );
    }

    #[test]
    fn depth_cap_blocks_eligibility_before_triggers_fire() {
        let runner = DecompositionRunner::new(enabled_policy(DecompositionTriggers {
            labels_any: vec!["epic".into()],
            ..Default::default()
        }));
        let decision = runner.eligibility(
            &parent_with(&["epic"]),
            &DecompositionContext {
                parent_depth: 2,
                estimated_files: None,
                acceptance_items: None,
            },
        );
        assert_eq!(
            decision,
            DecompositionEligibility::Skipped(SkipReason::DepthCapReached {
                max_depth: 2,
                parent_depth: 2,
            })
        );
    }

    #[test]
    fn multiple_triggers_collect_in_order() {
        let runner = DecompositionRunner::new(enabled_policy(DecompositionTriggers {
            labels_any: vec!["epic".into()],
            estimated_files_over: Some(5),
            acceptance_items_over: Some(2),
        }));
        let decision = runner.eligibility(
            &parent_with(&["epic"]),
            &DecompositionContext {
                parent_depth: 0,
                estimated_files: Some(7),
                acceptance_items: Some(5),
            },
        );
        match decision {
            DecompositionEligibility::Decompose { hits } => {
                assert_eq!(hits.len(), 3);
                assert!(matches!(hits[0], TriggerHit::Label(_)));
                assert!(matches!(hits[1], TriggerHit::EstimatedFiles { .. }));
                assert!(matches!(hits[2], TriggerHit::AcceptanceItems { .. }));
            }
            other => panic!("expected Decompose, got {other:?}"),
        }
    }

    // ------------------------------------------------------------------ acceptance

    #[test]
    fn accept_produces_proposal_with_status_from_policy() {
        let runner = DecompositionRunner::new(enabled_policy(DecompositionTriggers {
            labels_any: vec!["epic".into()],
            ..Default::default()
        }));
        let proposal = runner
            .accept(DecompositionId::new(1), minimal_draft(), 0)
            .unwrap();
        assert_eq!(proposal.status, DecompositionStatus::Proposed);
        assert_eq!(proposal.policy, FollowupPolicy::ProposeForApproval);
        assert_eq!(proposal.children.len(), 1);
        assert_eq!(proposal.parent, WorkItemId::new(100));
    }

    #[test]
    fn accept_uses_create_directly_initial_status() {
        let mut policy = enabled_policy(DecompositionTriggers {
            labels_any: vec!["epic".into()],
            ..Default::default()
        });
        policy.child_issue_policy = FollowupPolicy::CreateDirectly;
        let runner = DecompositionRunner::new(policy);
        let proposal = runner
            .accept(DecompositionId::new(1), minimal_draft(), 0)
            .unwrap();
        assert_eq!(proposal.status, DecompositionStatus::Approved);
    }

    #[test]
    fn accept_rejects_disabled_policy() {
        let runner = DecompositionRunner::new(DecompositionPolicy::default());
        let err = runner
            .accept(DecompositionId::new(1), minimal_draft(), 0)
            .unwrap_err();
        assert_eq!(err, RunnerError::PolicyDisabled);
    }

    #[test]
    fn accept_rejects_missing_owner_role() {
        let mut policy = enabled_policy(DecompositionTriggers {
            labels_any: vec!["epic".into()],
            ..Default::default()
        });
        policy.owner_role = None;
        let runner = DecompositionRunner::new(policy);
        let err = runner
            .accept(DecompositionId::new(1), minimal_draft(), 0)
            .unwrap_err();
        assert_eq!(err, RunnerError::NoOwnerRoleConfigured);
    }

    #[test]
    fn accept_rejects_author_role_mismatch() {
        let runner = DecompositionRunner::new(enabled_policy(DecompositionTriggers {
            labels_any: vec!["epic".into()],
            ..Default::default()
        }));
        let mut draft = minimal_draft();
        draft.author_role = RoleName::new("backend");
        let err = runner
            .accept(DecompositionId::new(1), draft, 0)
            .unwrap_err();
        assert!(matches!(err, RunnerError::AuthorRoleMismatch { .. }));
    }

    #[test]
    fn accept_enforces_max_depth() {
        let runner = DecompositionRunner::new(enabled_policy(DecompositionTriggers {
            labels_any: vec!["epic".into()],
            ..Default::default()
        }));
        let err = runner
            .accept(DecompositionId::new(1), minimal_draft(), 2)
            .unwrap_err();
        assert_eq!(
            err,
            RunnerError::MaxDepthExceeded {
                max_depth: 2,
                parent_depth: 2,
            }
        );
    }

    #[test]
    fn accept_requires_acceptance_when_policy_demands() {
        let runner = DecompositionRunner::new(enabled_policy(DecompositionTriggers {
            labels_any: vec!["epic".into()],
            ..Default::default()
        }));
        let mut draft = minimal_draft();
        draft.children[0].acceptance_criteria.clear();
        let err = runner
            .accept(DecompositionId::new(1), draft, 0)
            .unwrap_err();
        assert!(matches!(
            err,
            RunnerError::Domain(DecompositionError::AcceptanceCriteriaRequired { .. })
        ));
    }

    #[test]
    fn accept_allows_empty_acceptance_when_policy_relaxes() {
        let mut policy = enabled_policy(DecompositionTriggers {
            labels_any: vec!["epic".into()],
            ..Default::default()
        });
        policy.require_acceptance_criteria_per_child = false;
        let runner = DecompositionRunner::new(policy);
        let mut draft = minimal_draft();
        draft.children[0].acceptance_criteria.clear();
        let proposal = runner.accept(DecompositionId::new(1), draft, 0).unwrap();
        assert_eq!(proposal.children.len(), 1);
    }

    #[test]
    fn accept_rejects_child_missing_role() {
        let runner = DecompositionRunner::new(enabled_policy(DecompositionTriggers {
            labels_any: vec!["epic".into()],
            ..Default::default()
        }));
        let mut draft = minimal_draft();
        draft.children[0].assigned_role = None;
        let err = runner
            .accept(DecompositionId::new(1), draft, 0)
            .unwrap_err();
        assert!(matches!(err, RunnerError::ChildMissingRole { .. }));
    }

    #[test]
    fn accept_threads_dependencies_through_to_proposal() {
        let runner = DecompositionRunner::new(enabled_policy(DecompositionTriggers {
            labels_any: vec!["epic".into()],
            ..Default::default()
        }));
        let draft = DecompositionDraft {
            parent: WorkItemId::new(100),
            author_role: RoleName::new("platform_lead"),
            summary: "split parent".into(),
            children: vec![
                child_draft("a", &[], &["criterion-a"]),
                child_draft("b", &["a"], &["criterion-b"]),
            ],
        };
        let proposal = runner.accept(DecompositionId::new(1), draft, 0).unwrap();
        assert_eq!(proposal.children.len(), 2);
        assert_eq!(proposal.roots().len(), 1);
        assert_eq!(proposal.roots()[0].key.as_str(), "a");
    }

    #[test]
    fn accept_propagates_cyclic_dependency_error() {
        let runner = DecompositionRunner::new(enabled_policy(DecompositionTriggers {
            labels_any: vec!["epic".into()],
            ..Default::default()
        }));
        let draft = DecompositionDraft {
            parent: WorkItemId::new(100),
            author_role: RoleName::new("platform_lead"),
            summary: "split parent".into(),
            children: vec![
                child_draft("a", &["b"], &["criterion-a"]),
                child_draft("b", &["a"], &["criterion-b"]),
            ],
        };
        let err = runner
            .accept(DecompositionId::new(1), draft, 0)
            .unwrap_err();
        assert!(matches!(
            err,
            RunnerError::Domain(DecompositionError::CyclicDependencies { .. })
        ));
    }

    #[test]
    fn triggers_is_empty_detects_default() {
        assert!(DecompositionTriggers::default().is_empty());
        assert!(
            !DecompositionTriggers {
                labels_any: vec!["x".into()],
                ..Default::default()
            }
            .is_empty()
        );
    }
}
