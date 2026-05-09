//! Domain model for decomposition proposals (SPEC v2 §5.7, §6.2).
//!
//! Decomposition is the integration owner's authoritative output: a broad
//! parent issue is split into a set of child scopes, each routed to a role,
//! ordered by a dependency graph, and gated by per-child acceptance
//! criteria. The kernel persists the proposal as a single durable artefact
//! before any tracker mutation happens — that way the `child_issue_policy`
//! decision (`create_directly` vs `propose_for_approval`) operates on a
//! frozen record instead of an ad-hoc agent payload.
//!
//! This module supplies only the *domain* shape:
//!
//! * [`DecompositionId`], [`DecompositionProposal`] — the durable proposal;
//! * [`ChildProposal`] — one entry in the proposal's child set;
//! * [`ChildKey`] — a proposal-local identifier the dependency graph
//!   references before tracker ids exist;
//! * [`DecompositionStatus`] — the proposed → approved → applied lifecycle;
//! * [`DecompositionError`] — invariant violations surfaced by
//!   [`DecompositionProposal::try_new`] / [`ChildProposal::try_new`].
//!
//! Persistence (the `decomposition_proposals` table — added with the
//! Phase 5 runner wiring) lives in `symphony-state`. This crate enforces
//! the construction-seam invariants the kernel branches on, mirroring the
//! [`crate::FollowupIssueRequest`] / [`crate::IntegrationRecord`] pattern.

use std::collections::{BTreeSet, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::followup::FollowupPolicy;
use crate::role::RoleName;
use crate::work_item::WorkItemId;

/// Strongly-typed primary key for a [`DecompositionProposal`].
///
/// Matches the `i64` row id used by the durable storage table and is kept
/// as a separate Rust type from [`WorkItemId`] / [`crate::IntegrationId`]
/// so an accidental swap is a type error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DecompositionId(pub i64);

impl DecompositionId {
    /// Construct a [`DecompositionId`] from a raw row id.
    pub fn new(id: i64) -> Self {
        Self(id)
    }

    /// Borrow the inner row id.
    pub fn get(self) -> i64 {
        self.0
    }
}

impl std::fmt::Display for DecompositionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

/// Proposal-local identifier for a child entry.
///
/// Children do not yet have a tracker [`WorkItemId`] when the integration
/// owner emits the proposal — `create_directly` policies write them after
/// approval, and `propose_for_approval` policies wait for a human gate.
/// The dependency graph therefore references children by a stable
/// proposal-local key the agent supplies (e.g. `"backend"`, `"ui"`).
///
/// The key is a free-form string, but [`ChildProposal::try_new`] forbids
/// empty/whitespace-only keys and [`DecompositionProposal::try_new`]
/// enforces uniqueness within a proposal.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChildKey(String);

impl ChildKey {
    /// Construct a [`ChildKey`] from a proposal-local label.
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    /// Borrow the underlying label.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume the wrapper and return the inner `String`.
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl std::fmt::Display for ChildKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for ChildKey {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

impl From<String> for ChildKey {
    fn from(s: String) -> Self {
        Self::new(s)
    }
}

/// Lifecycle of a [`DecompositionProposal`] (SPEC v2 §5.7).
///
/// `Proposed` is the initial state for [`FollowupPolicy::ProposeForApproval`]
/// proposals; `Approved` is the initial state when the workflow's
/// `decomposition.child_issue_policy` is `create_directly`. `Applied` is
/// terminal — children have been created (or linked) in the tracker and
/// the parent's child edges are persisted. `Rejected` and `Cancelled` are
/// sticky terminal off-ramps, mirroring [`crate::FollowupStatus`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecompositionStatus {
    /// Awaiting approval in the integration-owner queue.
    Proposed,
    /// Approved (directly by policy or by an approver) but children not
    /// yet created in the tracker.
    Approved,
    /// Children created/linked in the tracker; parent/child edges
    /// persisted. Terminal.
    Applied,
    /// Approver explicitly rejected the proposal. Terminal.
    Rejected,
    /// Withdrawn (rerun, scope change, …). Terminal.
    Cancelled,
}

impl DecompositionStatus {
    /// All variants in declaration order.
    pub const ALL: [Self; 5] = [
        Self::Proposed,
        Self::Approved,
        Self::Applied,
        Self::Rejected,
        Self::Cancelled,
    ];

    /// Stable lowercase identifier used in YAML and JSON payloads.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::Approved => "approved",
            Self::Applied => "applied",
            Self::Rejected => "rejected",
            Self::Cancelled => "cancelled",
        }
    }

    /// True for statuses that no longer accept further transitions.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Applied | Self::Rejected | Self::Cancelled)
    }

    /// Initial lifecycle status implied by the workflow's resolved policy.
    pub fn initial_for(policy: FollowupPolicy) -> Self {
        match policy {
            FollowupPolicy::CreateDirectly => Self::Approved,
            FollowupPolicy::ProposeForApproval => Self::Proposed,
        }
    }
}

impl std::fmt::Display for DecompositionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Invariant violations raised during proposal construction.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecompositionError {
    /// `key` was empty after trim. Dependency graph references would be
    /// ambiguous without a non-empty key.
    #[error("decomposition child key must not be empty")]
    EmptyChildKey,
    /// `title` was empty after trim. SPEC §5.7 requires child scopes to
    /// be tracker-fileable.
    #[error("decomposition child title must not be empty")]
    EmptyChildTitle,
    /// `scope` was empty after trim. SPEC §5.7 requires every child to
    /// carry a scope distinguishing it from siblings and the parent.
    #[error("decomposition child scope must not be empty")]
    EmptyChildScope,
    /// One of the `acceptance_criteria` entries was empty after trim.
    /// Empty bullets break QA trace rendering downstream.
    #[error("decomposition child acceptance criterion must not be empty")]
    EmptyAcceptanceCriterion,
    /// `acceptance_criteria` was empty when the workflow demanded at
    /// least one (SPEC §5.7
    /// `require_acceptance_criteria_per_child` defaults to `true`).
    #[error("decomposition child {key} must include at least one acceptance criterion")]
    AcceptanceCriteriaRequired {
        /// The child whose acceptance criteria were empty.
        key: ChildKey,
    },
    /// Two children shared the same [`ChildKey`]. Keys must be unique
    /// within a proposal so dependency edges resolve unambiguously.
    #[error("decomposition proposal contains duplicate child key {key}")]
    DuplicateChildKey {
        /// The duplicated key.
        key: ChildKey,
    },
    /// Proposal had zero children. A proposal with no children would not
    /// decompose anything; the integration owner should reject the parent
    /// or route it to a specialist instead.
    #[error("decomposition proposal must include at least one child")]
    NoChildren,
    /// A dependency edge referenced a [`ChildKey`] that does not appear
    /// in the proposal's child set.
    #[error("decomposition child {child} depends on unknown child {missing}")]
    UnknownDependency {
        /// The child whose `depends_on` was bad.
        child: ChildKey,
        /// The missing key it referenced.
        missing: ChildKey,
    },
    /// A child listed itself in its `depends_on` set. Self-edges are not
    /// useful and would always make the graph cyclic.
    #[error("decomposition child {key} cannot depend on itself")]
    SelfDependency {
        /// The offending key.
        key: ChildKey,
    },
    /// The dependency graph contained a cycle. The kernel needs a DAG so
    /// integration ordering is well-defined.
    #[error("decomposition proposal dependency graph is cyclic; cycle includes {key}")]
    CyclicDependencies {
        /// One node from the detected cycle (the first one the
        /// constructor stalls on while toposorting).
        key: ChildKey,
    },
    /// `parent` matched one of the children's `created_issue_id` slots.
    /// A child cannot reference the parent as itself.
    #[error("decomposition child {key} created_issue_id cannot equal parent ({parent})")]
    ChildIsParent {
        /// The offending key.
        key: ChildKey,
        /// Parent work item id.
        parent: WorkItemId,
    },
    /// Lifecycle transition called from a status it is not valid for
    /// (e.g. approving a non-`Proposed` proposal, applying a
    /// non-`Approved` proposal, or mutating a terminal one). Mirrors the
    /// blocker rule: terminal rows are frozen.
    #[error("decomposition {id} cannot transition from {current} to {requested}")]
    InvalidTransition {
        /// The proposal being mutated.
        id: DecompositionId,
        /// Current status.
        current: DecompositionStatus,
        /// Status the caller tried to set.
        requested: DecompositionStatus,
    },
    /// `approve` / `reject` / `cancel` was called with an empty note.
    /// A note is required so a future audit can reconstruct *why* the
    /// proposal was approved, rejected, or withdrawn.
    #[error("decomposition approval/rejection/cancellation requires a non-empty note")]
    EmptyApprovalNote,
    /// [`DecompositionProposal::mark_applied`] was called with a
    /// `created_ids` map missing one of the children. Partial applies
    /// would leave the parent/child edge set inconsistent.
    #[error("decomposition cannot be applied: child {key} has no created_issue_id")]
    MissingCreatedId {
        /// The child whose tracker id was missing.
        key: ChildKey,
    },
}

/// One entry in a [`DecompositionProposal`].
///
/// Carries the SPEC §5.7 required surface — title, scope, role
/// assignment, dependencies, acceptance criteria — plus optional
/// metadata (description, branch hint, blocking flag) the integration
/// owner may emit. `created_issue_id` is `None` until the proposal is
/// applied; it stores the tracker work-item id once `TrackerMutations`
/// has filed the child (SPEC §5.7 `child_issue_policy: create_directly`)
/// or the operator linked an existing issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildProposal {
    /// Proposal-local identifier referenced by the dependency graph.
    pub key: ChildKey,
    /// Tracker title (used for `TrackerMutations::create_issue`).
    pub title: String,
    /// Bounded scope description distinguishing this child from
    /// siblings and the parent. Required by SPEC §5.7.
    pub scope: String,
    /// Role responsible for executing the child work. The kernel
    /// resolves this against `WorkflowConfig::roles` at routing time.
    pub assigned_role: RoleName,
    /// Optional long-form description (stored verbatim in tracker
    /// `body` when the proposal is applied).
    pub description: Option<String>,
    /// Acceptance criteria bullets. Required to be non-empty when the
    /// workflow's `decomposition.require_acceptance_criteria_per_child`
    /// is `true` (the SPEC §5.7 default).
    pub acceptance_criteria: Vec<String>,
    /// Other [`ChildKey`]s in the same proposal whose completion gates
    /// this child. Forms the proposal's dependency DAG.
    pub depends_on: BTreeSet<ChildKey>,
    /// True when this child must complete before the parent may close.
    /// Maps to SPEC §5.10 `integration.required_for: required_children`.
    /// Defaults to `true`; an integration owner sets it `false` only for
    /// explicitly optional children.
    pub blocking: bool,
    /// Optional branch hint the workspace strategy may consume. The
    /// kernel does not interpret this beyond persistence; the workspace
    /// claim path performs branch template expansion.
    pub branch_hint: Option<String>,
    /// Tracker work-item id assigned once the child is filed (or
    /// linked). `None` while the proposal is still
    /// [`DecompositionStatus::Proposed`] / [`DecompositionStatus::Approved`].
    pub created_issue_id: Option<WorkItemId>,
}

impl ChildProposal {
    /// Construct a fresh [`ChildProposal`], enforcing the per-entry
    /// invariants the kernel branches on. Cross-entry invariants
    /// (uniqueness, dependency-target existence, acyclicity) are
    /// enforced by [`DecompositionProposal::try_new`].
    ///
    /// `require_acceptance_criteria` mirrors the
    /// `decomposition.require_acceptance_criteria_per_child` workflow
    /// knob: when `true`, an empty `acceptance_criteria` vector is
    /// rejected. Same construction-seam pattern as
    /// [`crate::FollowupIssueRequest::try_new`].
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        key: impl Into<ChildKey>,
        title: impl Into<String>,
        scope: impl Into<String>,
        assigned_role: RoleName,
        description: Option<String>,
        acceptance_criteria: Vec<String>,
        depends_on: BTreeSet<ChildKey>,
        blocking: bool,
        branch_hint: Option<String>,
        require_acceptance_criteria: bool,
    ) -> Result<Self, DecompositionError> {
        let key = key.into();
        if key.as_str().trim().is_empty() {
            return Err(DecompositionError::EmptyChildKey);
        }
        let title = title.into();
        if title.trim().is_empty() {
            return Err(DecompositionError::EmptyChildTitle);
        }
        let scope = scope.into();
        if scope.trim().is_empty() {
            return Err(DecompositionError::EmptyChildScope);
        }
        for criterion in &acceptance_criteria {
            if criterion.trim().is_empty() {
                return Err(DecompositionError::EmptyAcceptanceCriterion);
            }
        }
        if require_acceptance_criteria && acceptance_criteria.is_empty() {
            return Err(DecompositionError::AcceptanceCriteriaRequired { key: key.clone() });
        }
        if depends_on.contains(&key) {
            return Err(DecompositionError::SelfDependency { key });
        }
        Ok(Self {
            key,
            title,
            scope,
            assigned_role,
            description,
            acceptance_criteria,
            depends_on,
            blocking,
            branch_hint,
            created_issue_id: None,
        })
    }
}

/// Durable decomposition proposal (SPEC v2 §5.7, §6.2).
///
/// The integration owner's authoritative split of a parent issue into
/// scoped, role-routed, dependency-ordered, acceptance-criteria-gated
/// children. Construct via [`Self::try_new`]; mutate the lifecycle via
/// [`Self::approve`], [`Self::reject`], [`Self::mark_applied`], or
/// [`Self::cancel`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecompositionProposal {
    /// Durable id (matches the storage row id).
    pub id: DecompositionId,
    /// Parent work item being decomposed.
    pub parent: WorkItemId,
    /// Role that produced the proposal. Recorded as evidence; the kernel
    /// also asserts the role is of kind
    /// [`crate::RoleKind::IntegrationOwner`] at the call site.
    pub author_role: RoleName,
    /// One-line summary of the decomposition strategy. Required so the
    /// approval queue can show meaningful context without rendering every
    /// child.
    pub summary: String,
    /// Children produced by the decomposition. Non-empty; keys unique;
    /// dependency edges form a DAG.
    pub children: Vec<ChildProposal>,
    /// Resolved creation policy (SPEC §5.7
    /// `decomposition.child_issue_policy`). Determines the initial
    /// [`Self::status`].
    pub policy: FollowupPolicy,
    /// Lifecycle status. Initialised from
    /// [`DecompositionStatus::initial_for`].
    pub status: DecompositionStatus,
    /// Role that approved (or rejected) the proposal. Required for
    /// approval/rejection transitions out of
    /// [`DecompositionStatus::Proposed`].
    pub approval_role: Option<RoleName>,
    /// Free-form approval/rejection/cancellation note.
    pub approval_note: Option<String>,
}

impl DecompositionProposal {
    /// Construct a fresh proposal, enforcing both per-child and
    /// cross-child invariants:
    ///
    /// * non-empty children;
    /// * unique [`ChildKey`]s within the proposal;
    /// * every `depends_on` target exists in the proposal;
    /// * the dependency graph is acyclic;
    /// * `parent` is not referenced as a child's `created_issue_id`.
    ///
    /// `require_acceptance_criteria_per_child` mirrors the workflow
    /// knob: when `true`, every child must carry at least one
    /// acceptance criterion. Per-child checks already ran in
    /// [`ChildProposal::try_new`]; this constructor only re-checks the
    /// parent-id self-reference rule (which is a cross-cutting
    /// invariant).
    pub fn try_new(
        id: DecompositionId,
        parent: WorkItemId,
        author_role: RoleName,
        summary: impl Into<String>,
        children: Vec<ChildProposal>,
        policy: FollowupPolicy,
    ) -> Result<Self, DecompositionError> {
        if children.is_empty() {
            return Err(DecompositionError::NoChildren);
        }
        let mut seen: HashSet<&ChildKey> = HashSet::with_capacity(children.len());
        for child in &children {
            if !seen.insert(&child.key) {
                return Err(DecompositionError::DuplicateChildKey {
                    key: child.key.clone(),
                });
            }
            if let Some(existing_id) = child.created_issue_id
                && existing_id == parent
            {
                return Err(DecompositionError::ChildIsParent {
                    key: child.key.clone(),
                    parent,
                });
            }
        }
        for child in &children {
            for dep in &child.depends_on {
                if !seen.contains(dep) {
                    return Err(DecompositionError::UnknownDependency {
                        child: child.key.clone(),
                        missing: dep.clone(),
                    });
                }
            }
        }
        toposort(&children)?;
        Ok(Self {
            id,
            parent,
            author_role,
            summary: summary.into(),
            children,
            status: DecompositionStatus::initial_for(policy),
            policy,
            approval_role: None,
            approval_note: None,
        })
    }

    /// Children with no incoming dependency edges; the integration owner
    /// dispatches these first.
    pub fn roots(&self) -> Vec<&ChildProposal> {
        self.children
            .iter()
            .filter(|c| c.depends_on.is_empty())
            .collect()
    }

    /// Children whose `blocking` flag is `true`. Used by the parent
    /// closeout gate (ARCH §6.2).
    pub fn blocking_children(&self) -> Vec<&ChildProposal> {
        self.children.iter().filter(|c| c.blocking).collect()
    }

    /// Lookup a child by [`ChildKey`].
    pub fn child(&self, key: &ChildKey) -> Option<&ChildProposal> {
        self.children.iter().find(|c| &c.key == key)
    }

    /// Approve a [`DecompositionStatus::Proposed`] proposal, recording
    /// the approver and note. Transitions to
    /// [`DecompositionStatus::Approved`].
    pub fn approve(
        &mut self,
        approval_role: RoleName,
        note: impl Into<String>,
    ) -> Result<(), DecompositionError> {
        let note = note.into();
        if note.trim().is_empty() {
            return Err(DecompositionError::EmptyApprovalNote);
        }
        if self.status != DecompositionStatus::Proposed {
            return Err(DecompositionError::InvalidTransition {
                id: self.id,
                current: self.status,
                requested: DecompositionStatus::Approved,
            });
        }
        self.status = DecompositionStatus::Approved;
        self.approval_role = Some(approval_role);
        self.approval_note = Some(note);
        Ok(())
    }

    /// Reject a [`DecompositionStatus::Proposed`] proposal.
    pub fn reject(
        &mut self,
        approval_role: RoleName,
        note: impl Into<String>,
    ) -> Result<(), DecompositionError> {
        let note = note.into();
        if note.trim().is_empty() {
            return Err(DecompositionError::EmptyApprovalNote);
        }
        if self.status != DecompositionStatus::Proposed {
            return Err(DecompositionError::InvalidTransition {
                id: self.id,
                current: self.status,
                requested: DecompositionStatus::Rejected,
            });
        }
        self.status = DecompositionStatus::Rejected;
        self.approval_role = Some(approval_role);
        self.approval_note = Some(note);
        Ok(())
    }

    /// Cancel a non-terminal proposal (e.g. owner reran).
    pub fn cancel(&mut self, note: impl Into<String>) -> Result<(), DecompositionError> {
        let note = note.into();
        if note.trim().is_empty() {
            return Err(DecompositionError::EmptyApprovalNote);
        }
        if self.status.is_terminal() {
            return Err(DecompositionError::InvalidTransition {
                id: self.id,
                current: self.status,
                requested: DecompositionStatus::Cancelled,
            });
        }
        self.status = DecompositionStatus::Cancelled;
        self.approval_note = Some(note);
        Ok(())
    }

    /// Mark an approved proposal as applied. `created_ids` maps every
    /// [`ChildKey`] to the tracker [`WorkItemId`] the integration loop
    /// produced; missing keys cause an error so partial applies do not
    /// silently corrupt the parent/child edge set.
    pub fn mark_applied(
        &mut self,
        created_ids: HashMap<ChildKey, WorkItemId>,
    ) -> Result<(), DecompositionError> {
        if self.status != DecompositionStatus::Approved {
            return Err(DecompositionError::InvalidTransition {
                id: self.id,
                current: self.status,
                requested: DecompositionStatus::Applied,
            });
        }
        for child in &self.children {
            let Some(id) = created_ids.get(&child.key) else {
                return Err(DecompositionError::MissingCreatedId {
                    key: child.key.clone(),
                });
            };
            if *id == self.parent {
                return Err(DecompositionError::ChildIsParent {
                    key: child.key.clone(),
                    parent: self.parent,
                });
            }
        }
        for child in &mut self.children {
            child.created_issue_id = created_ids.get(&child.key).copied();
        }
        self.status = DecompositionStatus::Applied;
        Ok(())
    }
}

/// Kahn-style toposort returning the first cyclic key on failure.
///
/// We do not need the sorted order here — the kernel re-derives ready
/// fronts dynamically as children complete — but the existence check is
/// the cheapest place to enforce DAG-ness at the construction seam.
fn toposort(children: &[ChildProposal]) -> Result<(), DecompositionError> {
    let mut indegree: HashMap<&ChildKey, usize> = HashMap::with_capacity(children.len());
    for child in children {
        indegree.insert(&child.key, child.depends_on.len());
    }
    let mut ready: Vec<&ChildKey> = indegree
        .iter()
        .filter_map(|(k, n)| (*n == 0).then_some(*k))
        .collect();
    let mut visited = 0usize;
    while let Some(key) = ready.pop() {
        visited += 1;
        for child in children {
            if child.depends_on.contains(key) {
                let entry = indegree.get_mut(&child.key).expect("present");
                *entry -= 1;
                if *entry == 0 {
                    ready.push(&child.key);
                }
            }
        }
    }
    if visited == children.len() {
        Ok(())
    } else {
        let stuck = indegree
            .iter()
            .find(|(_, n)| **n > 0)
            .map(|(k, _)| (*k).clone())
            .unwrap_or_else(|| ChildKey::new("<unknown>"));
        Err(DecompositionError::CyclicDependencies { key: stuck })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn child(key: &str, deps: &[&str], criteria: &[&str]) -> ChildProposal {
        ChildProposal::try_new(
            key,
            format!("title for {key}"),
            format!("scope for {key}"),
            RoleName::new("backend"),
            None,
            criteria.iter().map(|s| (*s).to_string()).collect(),
            deps.iter().map(|s| ChildKey::new(*s)).collect(),
            true,
            None,
            true,
        )
        .expect("valid child")
    }

    fn proposal(children: Vec<ChildProposal>) -> DecompositionProposal {
        DecompositionProposal::try_new(
            DecompositionId::new(1),
            WorkItemId::new(100),
            RoleName::new("platform_lead"),
            "split parent",
            children,
            FollowupPolicy::ProposeForApproval,
        )
        .expect("valid proposal")
    }

    #[test]
    fn child_key_rejects_empty() {
        let err = ChildProposal::try_new(
            "  ",
            "t",
            "s",
            RoleName::new("backend"),
            None,
            vec!["c".into()],
            BTreeSet::new(),
            true,
            None,
            true,
        )
        .unwrap_err();
        assert!(matches!(err, DecompositionError::EmptyChildKey));
    }

    #[test]
    fn child_requires_acceptance_when_workflow_demands() {
        let err = ChildProposal::try_new(
            "k",
            "t",
            "s",
            RoleName::new("backend"),
            None,
            vec![],
            BTreeSet::new(),
            true,
            None,
            true,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            DecompositionError::AcceptanceCriteriaRequired { .. }
        ));
    }

    #[test]
    fn child_allows_empty_acceptance_when_not_required() {
        let ok = ChildProposal::try_new(
            "k",
            "t",
            "s",
            RoleName::new("backend"),
            None,
            vec![],
            BTreeSet::new(),
            false,
            None,
            false,
        );
        assert!(ok.is_ok());
    }

    #[test]
    fn child_rejects_self_dependency() {
        let mut deps = BTreeSet::new();
        deps.insert(ChildKey::new("self"));
        let err = ChildProposal::try_new(
            "self",
            "t",
            "s",
            RoleName::new("backend"),
            None,
            vec!["c".into()],
            deps,
            true,
            None,
            true,
        )
        .unwrap_err();
        assert!(matches!(err, DecompositionError::SelfDependency { .. }));
    }

    #[test]
    fn proposal_rejects_no_children() {
        let err = DecompositionProposal::try_new(
            DecompositionId::new(1),
            WorkItemId::new(100),
            RoleName::new("platform_lead"),
            "x",
            vec![],
            FollowupPolicy::CreateDirectly,
        )
        .unwrap_err();
        assert!(matches!(err, DecompositionError::NoChildren));
    }

    #[test]
    fn proposal_rejects_duplicate_keys() {
        let a = child("a", &[], &["c"]);
        let b = child("a", &[], &["c"]);
        let err = DecompositionProposal::try_new(
            DecompositionId::new(1),
            WorkItemId::new(100),
            RoleName::new("platform_lead"),
            "x",
            vec![a, b],
            FollowupPolicy::CreateDirectly,
        )
        .unwrap_err();
        assert!(matches!(err, DecompositionError::DuplicateChildKey { .. }));
    }

    #[test]
    fn proposal_rejects_unknown_dependency() {
        let a = child("a", &["ghost"], &["c"]);
        let err = DecompositionProposal::try_new(
            DecompositionId::new(1),
            WorkItemId::new(100),
            RoleName::new("platform_lead"),
            "x",
            vec![a],
            FollowupPolicy::CreateDirectly,
        )
        .unwrap_err();
        assert!(matches!(err, DecompositionError::UnknownDependency { .. }));
    }

    #[test]
    fn proposal_rejects_cyclic_graph() {
        let a = child("a", &["b"], &["c"]);
        let b = child("b", &["a"], &["c"]);
        let err = DecompositionProposal::try_new(
            DecompositionId::new(1),
            WorkItemId::new(100),
            RoleName::new("platform_lead"),
            "x",
            vec![a, b],
            FollowupPolicy::CreateDirectly,
        )
        .unwrap_err();
        assert!(matches!(err, DecompositionError::CyclicDependencies { .. }));
    }

    #[test]
    fn dag_with_diamond_is_accepted() {
        let a = child("a", &[], &["c"]);
        let b = child("b", &["a"], &["c"]);
        let c = child("c", &["a"], &["c"]);
        let d = child("d", &["b", "c"], &["c"]);
        let p = proposal(vec![a, b, c, d]);
        assert_eq!(p.children.len(), 4);
        assert_eq!(p.roots().len(), 1);
        assert_eq!(p.roots()[0].key.as_str(), "a");
    }

    #[test]
    fn initial_status_follows_policy() {
        let propose = DecompositionProposal::try_new(
            DecompositionId::new(1),
            WorkItemId::new(100),
            RoleName::new("platform_lead"),
            "x",
            vec![child("a", &[], &["c"])],
            FollowupPolicy::ProposeForApproval,
        )
        .unwrap();
        assert_eq!(propose.status, DecompositionStatus::Proposed);

        let direct = DecompositionProposal::try_new(
            DecompositionId::new(2),
            WorkItemId::new(100),
            RoleName::new("platform_lead"),
            "x",
            vec![child("a", &[], &["c"])],
            FollowupPolicy::CreateDirectly,
        )
        .unwrap();
        assert_eq!(direct.status, DecompositionStatus::Approved);
    }

    #[test]
    fn approve_then_apply_records_created_ids() {
        let mut p = proposal(vec![child("a", &[], &["c"]), child("b", &["a"], &["c"])]);
        p.approve(RoleName::new("platform_lead"), "looks good")
            .unwrap();
        assert_eq!(p.status, DecompositionStatus::Approved);

        let mut ids = HashMap::new();
        ids.insert(ChildKey::new("a"), WorkItemId::new(101));
        ids.insert(ChildKey::new("b"), WorkItemId::new(102));
        p.mark_applied(ids).unwrap();
        assert_eq!(p.status, DecompositionStatus::Applied);
        assert_eq!(
            p.child(&ChildKey::new("a")).unwrap().created_issue_id,
            Some(WorkItemId::new(101))
        );
    }

    #[test]
    fn mark_applied_rejects_partial_id_map() {
        let mut p = DecompositionProposal::try_new(
            DecompositionId::new(1),
            WorkItemId::new(100),
            RoleName::new("platform_lead"),
            "x",
            vec![child("a", &[], &["c"]), child("b", &[], &["c"])],
            FollowupPolicy::CreateDirectly,
        )
        .unwrap();
        let mut ids = HashMap::new();
        ids.insert(ChildKey::new("a"), WorkItemId::new(101));
        let err = p.mark_applied(ids).unwrap_err();
        assert!(matches!(err, DecompositionError::MissingCreatedId { .. }));
    }

    #[test]
    fn mark_applied_rejects_parent_collision() {
        let mut p = DecompositionProposal::try_new(
            DecompositionId::new(1),
            WorkItemId::new(100),
            RoleName::new("platform_lead"),
            "x",
            vec![child("a", &[], &["c"])],
            FollowupPolicy::CreateDirectly,
        )
        .unwrap();
        let mut ids = HashMap::new();
        ids.insert(ChildKey::new("a"), WorkItemId::new(100));
        let err = p.mark_applied(ids).unwrap_err();
        assert!(matches!(err, DecompositionError::ChildIsParent { .. }));
    }

    #[test]
    fn blocking_children_filters_correctly() {
        let mut a = child("a", &[], &["c"]);
        a.blocking = false;
        let b = child("b", &[], &["c"]);
        let p = proposal(vec![a, b]);
        let blocking = p.blocking_children();
        assert_eq!(blocking.len(), 1);
        assert_eq!(blocking[0].key.as_str(), "b");
    }

    #[test]
    fn round_trip_serde() {
        let p = proposal(vec![
            child("a", &[], &["criterion-a"]),
            child("b", &["a"], &["criterion-b"]),
        ]);
        let json = serde_json::to_string(&p).unwrap();
        let back: DecompositionProposal = serde_json::from_str(&json).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn status_strings_match_yaml() {
        assert_eq!(DecompositionStatus::Proposed.as_str(), "proposed");
        assert_eq!(DecompositionStatus::Approved.as_str(), "approved");
        assert_eq!(DecompositionStatus::Applied.as_str(), "applied");
        assert_eq!(DecompositionStatus::Rejected.as_str(), "rejected");
        assert_eq!(DecompositionStatus::Cancelled.as_str(), "cancelled");
    }
}
