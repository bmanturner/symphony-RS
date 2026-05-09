//! Domain model for follow-up issue requests (SPEC v2 §4.10, §5.13).
//!
//! A follow-up is *newly discovered work* that falls outside the current
//! issue's acceptance criteria — adjacent bugs, refactors, missing tests,
//! or scope cleanly separable from the current branch. SPEC v2 §2.5
//! makes follow-up creation a first-class agent literacy: every role MAY
//! propose follow-ups, and the workflow decides whether the request
//! becomes a tracker issue immediately or goes through an approval queue.
//!
//! This module supplies the *durable* shape the kernel persists once an
//! agent's lightweight [`crate::HandoffFollowupRequest`] (the wire-level
//! intent in §4.7) survives policy checks. The two types are kept
//! deliberately distinct:
//!
//! * [`crate::HandoffFollowupRequest`] — minimal agent-emitted intent
//!   (title, summary, blocking flag, propose-only flag). Stable across
//!   policy changes so prompt schemas don't churn.
//! * [`FollowupIssueRequest`] — durable kernel record with the
//!   SPEC §5.13 required surface (title, reason, scope, acceptance
//!   criteria, relationship to current work, blocking flag) plus the
//!   resolved [`FollowupPolicy`], lifecycle [`FollowupStatus`], and
//!   approval/creation metadata.
//!
//! Persistence belongs to `symphony-state` (Phase 10 wires the table).
//! Construct via [`FollowupIssueRequest::try_new`] so the invariants the
//! kernel branches on are enforced once at the construction seam, the
//! same pattern as [`crate::Blocker::try_new`].

use serde::{Deserialize, Serialize};

use crate::blocker::BlockerOrigin;
use crate::role::RoleName;
use crate::work_item::WorkItemId;

/// Strongly-typed primary key for a [`FollowupIssueRequest`].
///
/// Matches the `i64` row id used by the durable follow-up table in
/// `symphony-state` (Phase 10). Newtyped so an accidental swap with a
/// [`WorkItemId`] or blocker id surfaces as a type error at the storage
/// seam, mirroring [`crate::BlockerId`] and [`crate::QaVerdictId`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FollowupId(pub i64);

impl FollowupId {
    /// Construct a [`FollowupId`] from a raw row id.
    pub fn new(id: i64) -> Self {
        Self(id)
    }

    /// Borrow the inner row id.
    pub fn get(self) -> i64 {
        self.0
    }
}

impl std::fmt::Display for FollowupId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

/// Resolved follow-up creation policy (SPEC v2 §5.13).
///
/// Mirrors the workflow-config `followups.default_policy` knob (which in
/// turn shares its underlying enum with `decomposition.child_issue_policy`
/// per SPEC §5.7). The domain type is kept independent of the config crate
/// so `symphony-core` does not need to depend on `symphony-config`: the
/// kernel resolves a workflow knob once and stores the resolved policy
/// alongside the durable request.
///
/// Defaults to [`Self::ProposeForApproval`] — the safer path. An agent
/// proposing a follow-up does not get to bypass triage by default; the
/// operator has to opt into [`Self::CreateDirectly`] in the workflow.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum FollowupPolicy {
    /// File the follow-up directly into the tracker via
    /// `TrackerMutations`. The kernel skips the approval queue and
    /// initialises the lifecycle at [`FollowupStatus::Approved`].
    CreateDirectly,
    /// Park the follow-up in the approval queue. The lifecycle starts at
    /// [`FollowupStatus::Proposed`] and only an approval (by a role
    /// configured under `followups.approval_role`) advances it.
    #[default]
    ProposeForApproval,
}

impl FollowupPolicy {
    /// All variants in declaration order.
    pub const ALL: [Self; 2] = [Self::CreateDirectly, Self::ProposeForApproval];

    /// Stable lowercase identifier used in YAML config and JSON payloads.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CreateDirectly => "create_directly",
            Self::ProposeForApproval => "propose_for_approval",
        }
    }

    /// Initial lifecycle status implied by the policy. The kernel uses
    /// this when persisting a fresh request so the queue routing is
    /// derivable from the policy alone.
    pub fn initial_status(self) -> FollowupStatus {
        match self {
            Self::CreateDirectly => FollowupStatus::Approved,
            Self::ProposeForApproval => FollowupStatus::Proposed,
        }
    }
}

impl std::fmt::Display for FollowupPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Lifecycle of a follow-up issue request (SPEC v2 §4.10).
///
/// Linear: `Proposed → Approved → Created` is the happy path; `Rejected`
/// and `Cancelled` are sticky terminal off-ramps. A request initialised
/// via [`FollowupPolicy::CreateDirectly`] starts at [`Self::Approved`]
/// and only the `Created`/`Cancelled` transitions remain meaningful.
///
/// The kernel uses [`Self::is_terminal`] to gate further mutation: once a
/// request reaches `Created`, `Rejected`, or `Cancelled`, the durable
/// row is frozen — operators must file a new request rather than mutate
/// the terminal one. Same rule as [`crate::BlockerStatus`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FollowupStatus {
    /// Awaiting approval in the follow-up queue. Only valid initial state
    /// for [`FollowupPolicy::ProposeForApproval`] requests.
    Proposed,
    /// Approved (either directly by policy or by an approval role) but
    /// not yet filed in the tracker. The integration loop moves the
    /// request from here to [`Self::Created`] once the tracker write
    /// succeeds.
    Approved,
    /// The follow-up issue has been created in the tracker. The created
    /// tracker id is recorded in
    /// [`FollowupIssueRequest::created_issue_id`].
    Created,
    /// An approver explicitly rejected the proposal. Sticky terminal.
    Rejected,
    /// The proposal was withdrawn (agent rerun, scope change, …). Sticky
    /// terminal.
    Cancelled,
}

impl FollowupStatus {
    /// All variants in declaration order.
    pub const ALL: [Self; 5] = [
        Self::Proposed,
        Self::Approved,
        Self::Created,
        Self::Rejected,
        Self::Cancelled,
    ];

    /// Stable lowercase identifier used in YAML and JSON payloads.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::Approved => "approved",
            Self::Created => "created",
            Self::Rejected => "rejected",
            Self::Cancelled => "cancelled",
        }
    }

    /// True for statuses that no longer accept further transitions.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Created | Self::Rejected | Self::Cancelled)
    }

    /// True while the request still needs operator action (proposal
    /// pending or approved-but-not-yet-filed).
    pub fn is_actionable(self) -> bool {
        matches!(self, Self::Proposed | Self::Approved)
    }
}

impl std::fmt::Display for FollowupStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Invariant violations raised by [`FollowupIssueRequest::try_new`] and
/// the lifecycle transitions.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FollowupError {
    /// `title` was empty after trim. SPEC §5.13 lists title as required.
    #[error("followup title must not be empty")]
    EmptyTitle,
    /// `reason` was empty after trim. SPEC §5.13 requires the reason so
    /// triage can decide whether to accept the follow-up.
    #[error("followup reason must not be empty")]
    EmptyReason,
    /// `scope` was empty after trim. SPEC §5.13 requires scope so the
    /// follow-up has an unambiguous boundary distinct from the source
    /// item.
    #[error("followup scope must not be empty")]
    EmptyScope,
    /// One of the `acceptance_criteria` entries was empty after trim.
    /// Empty bullets break trace rendering and QA matching downstream.
    #[error("followup acceptance criterion must not be empty")]
    EmptyAcceptanceCriterion,
    /// `acceptance_criteria` was empty when the workflow demanded at
    /// least one. SPEC §5.13 — `require_acceptance_criteria` defaults to
    /// `true`. This invariant is enforced by [`FollowupIssueRequest::try_new`]
    /// when the caller passes `require_acceptance_criteria = true`.
    #[error("followup must include at least one acceptance criterion")]
    AcceptanceCriteriaRequired,
    /// `created_issue_id` matched `source_work_item`. A follow-up cannot
    /// reference itself as the source item.
    #[error(
        "followup created_issue_id cannot equal source_work_item ({0}) — a follow-up cannot reference itself"
    )]
    SelfReference(WorkItemId),
    /// Attempted to transition a follow-up that is already terminal.
    /// Mirrors the blocker rule: terminal rows are frozen.
    #[error("followup {id} is already {current}; cannot transition to {requested}")]
    AlreadyTerminal {
        /// The follow-up being mutated.
        id: FollowupId,
        /// The terminal status the follow-up is currently in.
        current: FollowupStatus,
        /// The status the caller tried to set.
        requested: FollowupStatus,
    },
    /// [`FollowupIssueRequest::approve`] called from a status other than
    /// [`FollowupStatus::Proposed`]. Approval is only meaningful for
    /// in-queue proposals; directly-created follow-ups bypass the
    /// approval edge.
    #[error(
        "followup {id} cannot be approved from {current} (only proposed follow-ups await approval)"
    )]
    NotApprovable {
        /// The follow-up being approved.
        id: FollowupId,
        /// Its current status.
        current: FollowupStatus,
    },
    /// [`FollowupIssueRequest::approve`] or [`FollowupIssueRequest::reject`]
    /// was called without a recorded approval role or with an empty
    /// note. Both are required so a future audit can reconstruct *who*
    /// approved/rejected and *why*.
    #[error("followup approval/rejection requires both an approval role and a non-empty note")]
    InvalidApproval,
    /// [`FollowupIssueRequest::mark_created`] was called from a status
    /// other than [`FollowupStatus::Approved`]. Creation requires prior
    /// approval (either directly or via the queue) so the tracker write
    /// is never racing the approval gate.
    #[error("followup {id} cannot be marked created from {current} (must be approved first)")]
    NotApproved {
        /// The follow-up being created.
        id: FollowupId,
        /// Its current status.
        current: FollowupStatus,
    },
    /// [`FollowupIssueRequest::source_link`] was called before the
    /// follow-up reached [`FollowupStatus::Created`]. The provenance edge
    /// records *which tracker issue spawned this follow-up*, so it cannot
    /// be derived until the tracker write has assigned a real
    /// [`WorkItemId`] to the follow-up.
    #[error(
        "followup {id} cannot produce a source link from {current} (must be created and have a tracker work item)"
    )]
    NotCreated {
        /// The follow-up being linked.
        id: FollowupId,
        /// Its current status.
        current: FollowupStatus,
    },
}

/// Provenance edge derived from a [`Created`](FollowupStatus::Created)
/// [`FollowupIssueRequest`] (SPEC v2 §4.10; ARCH §6.4).
///
/// Maps cleanly onto the `work_item_edges` `followup_of` row the storage
/// layer inserts: `source_work_item` is the originating issue
/// (`parent_id` in the edge schema), `followup_work_item` is the freshly
/// created follow-up (`child_id`), and `followup_id` lets observers join
/// the durable follow-up record back to the edge for audit and rendering.
///
/// Construct via [`FollowupIssueRequest::source_link`] — the helper
/// rejects requests that have not yet completed the tracker write so the
/// kernel cannot accidentally persist a dangling provenance edge that
/// points at a non-existent work item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FollowupLink {
    /// Durable follow-up record this link belongs to. Round-tripped so
    /// downstream observers can join the edge back to the request without
    /// a second lookup.
    pub followup_id: FollowupId,
    /// Issue that *spawned* the follow-up. Becomes the edge's `parent_id`.
    pub source_work_item: WorkItemId,
    /// Tracker work item created from the follow-up. Becomes the edge's
    /// `child_id`.
    pub followup_work_item: WorkItemId,
    /// Mirrors [`FollowupIssueRequest::blocking`]. Carried on the link so
    /// the storage seam can decide the edge `status` (`open` for
    /// blocking, `linked` for non-blocking) without re-reading the
    /// follow-up row.
    pub blocking: bool,
}

/// Durable follow-up issue request (SPEC v2 §4.10, §5.13).
///
/// Carries the SPEC §5.13 required surface — title, reason, scope,
/// acceptance criteria, relationship to current work, blocking flag —
/// alongside the resolved [`FollowupPolicy`], lifecycle
/// [`FollowupStatus`], and approval/creation metadata. Construct via
/// [`Self::try_new`]; mutate the lifecycle via [`Self::approve`],
/// [`Self::reject`], [`Self::mark_created`], or [`Self::cancel`].
///
/// `blocking` is the SPEC §4.10 distinction between a non-blocking
/// adjacent finding and a follow-up the agent flagged as gating current
/// acceptance. The kernel uses this flag (combined with `qa.blocker_policy`
/// and `decomposition.required_for`) to decide whether the parent issue
/// may close before the follow-up is resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FollowupIssueRequest {
    /// Durable id (matches the storage row id).
    pub id: FollowupId,
    /// Work item that *spawned* the follow-up (the SPEC §5.13
    /// "relationship to current work" pointer).
    pub source_work_item: WorkItemId,
    /// Short tracker title.
    pub title: String,
    /// Operator-facing reason for filing the follow-up.
    pub reason: String,
    /// Bounded scope description distinguishing this work from the
    /// source item.
    pub scope: String,
    /// Acceptance criteria bullets. Required to be non-empty when the
    /// workflow's `followups.require_acceptance_criteria` is `true`
    /// (the SPEC §5.13 default).
    pub acceptance_criteria: Vec<String>,
    /// True when the follow-up gates current acceptance. SPEC §4.10.
    pub blocking: bool,
    /// Resolved creation policy.
    pub policy: FollowupPolicy,
    /// Who filed the request. Reuses [`BlockerOrigin`] because the
    /// "actor that produced this artefact" shape is identical (run /
    /// agent / human) and a parallel enum would be duplication for no
    /// new information.
    pub origin: BlockerOrigin,
    /// Lifecycle status. Initialised from [`FollowupPolicy::initial_status`].
    pub status: FollowupStatus,
    /// Tracker id of the issue created from this request, once the
    /// integration loop has written it. `None` until [`Self::mark_created`].
    pub created_issue_id: Option<WorkItemId>,
    /// Role that approved (or rejected) the proposal. Required for
    /// `Approved`/`Rejected` transitions out of [`FollowupStatus::Proposed`].
    pub approval_role: Option<RoleName>,
    /// Free-form approval/rejection/cancellation note. Required when an
    /// approval role is recorded.
    pub approval_note: Option<String>,
}

impl FollowupIssueRequest {
    /// Construct a fresh follow-up request, enforcing the SPEC §5.13
    /// required surface.
    ///
    /// `require_acceptance_criteria` mirrors the
    /// `followups.require_acceptance_criteria` workflow knob: when
    /// `true`, an empty `acceptance_criteria` vector is rejected. This
    /// keeps the Phase 10 wiring honest — the workflow knob has teeth at
    /// the construction seam, not just in prompt rendering.
    ///
    /// The initial [`Self::status`] is derived from `policy` via
    /// [`FollowupPolicy::initial_status`]. `created_issue_id`,
    /// `approval_role`, and `approval_note` start unset; the lifecycle
    /// methods populate them.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        id: FollowupId,
        source_work_item: WorkItemId,
        title: impl Into<String>,
        reason: impl Into<String>,
        scope: impl Into<String>,
        acceptance_criteria: Vec<String>,
        blocking: bool,
        policy: FollowupPolicy,
        origin: BlockerOrigin,
        require_acceptance_criteria: bool,
    ) -> Result<Self, FollowupError> {
        let title = title.into();
        if title.trim().is_empty() {
            return Err(FollowupError::EmptyTitle);
        }
        let reason = reason.into();
        if reason.trim().is_empty() {
            return Err(FollowupError::EmptyReason);
        }
        let scope = scope.into();
        if scope.trim().is_empty() {
            return Err(FollowupError::EmptyScope);
        }
        for criterion in &acceptance_criteria {
            if criterion.trim().is_empty() {
                return Err(FollowupError::EmptyAcceptanceCriterion);
            }
        }
        if require_acceptance_criteria && acceptance_criteria.is_empty() {
            return Err(FollowupError::AcceptanceCriteriaRequired);
        }
        Ok(Self {
            id,
            source_work_item,
            title,
            reason,
            scope,
            acceptance_criteria,
            blocking,
            policy,
            origin,
            status: policy.initial_status(),
            created_issue_id: None,
            approval_role: None,
            approval_note: None,
        })
    }

    /// True when the follow-up still gates acceptance (i.e. it was
    /// flagged as blocking *and* its status is not yet terminal).
    pub fn is_blocking_gate(&self) -> bool {
        self.blocking && !self.status.is_terminal()
    }

    /// Approve a [`FollowupStatus::Proposed`] request, recording the
    /// approving role and note. Transitions to [`FollowupStatus::Approved`].
    pub fn approve(
        &mut self,
        approval_role: RoleName,
        note: impl Into<String>,
    ) -> Result<(), FollowupError> {
        if self.status.is_terminal() {
            return Err(FollowupError::AlreadyTerminal {
                id: self.id,
                current: self.status,
                requested: FollowupStatus::Approved,
            });
        }
        if self.status != FollowupStatus::Proposed {
            return Err(FollowupError::NotApprovable {
                id: self.id,
                current: self.status,
            });
        }
        let note = note.into();
        if note.trim().is_empty() {
            return Err(FollowupError::InvalidApproval);
        }
        self.approval_role = Some(approval_role);
        self.approval_note = Some(note);
        self.status = FollowupStatus::Approved;
        Ok(())
    }

    /// Reject a [`FollowupStatus::Proposed`] request, recording the
    /// rejecting role and note. Transitions to [`FollowupStatus::Rejected`]
    /// (sticky terminal).
    pub fn reject(
        &mut self,
        approval_role: RoleName,
        note: impl Into<String>,
    ) -> Result<(), FollowupError> {
        if self.status.is_terminal() {
            return Err(FollowupError::AlreadyTerminal {
                id: self.id,
                current: self.status,
                requested: FollowupStatus::Rejected,
            });
        }
        if self.status != FollowupStatus::Proposed {
            return Err(FollowupError::NotApprovable {
                id: self.id,
                current: self.status,
            });
        }
        let note = note.into();
        if note.trim().is_empty() {
            return Err(FollowupError::InvalidApproval);
        }
        self.approval_role = Some(approval_role);
        self.approval_note = Some(note);
        self.status = FollowupStatus::Rejected;
        Ok(())
    }

    /// Record that the tracker issue was created from this request.
    /// Requires the request to be in [`FollowupStatus::Approved`] and
    /// rejects a `created_issue_id` equal to [`Self::source_work_item`].
    pub fn mark_created(&mut self, created_issue_id: WorkItemId) -> Result<(), FollowupError> {
        if self.status.is_terminal() {
            return Err(FollowupError::AlreadyTerminal {
                id: self.id,
                current: self.status,
                requested: FollowupStatus::Created,
            });
        }
        if self.status != FollowupStatus::Approved {
            return Err(FollowupError::NotApproved {
                id: self.id,
                current: self.status,
            });
        }
        if created_issue_id == self.source_work_item {
            return Err(FollowupError::SelfReference(created_issue_id));
        }
        self.created_issue_id = Some(created_issue_id);
        self.status = FollowupStatus::Created;
        Ok(())
    }

    /// Derive the [`FollowupLink`] for the `followup_of` provenance
    /// edge. Requires the request to be in [`FollowupStatus::Created`]
    /// (and therefore to have a populated [`Self::created_issue_id`]);
    /// errors with [`FollowupError::NotCreated`] otherwise so the kernel
    /// cannot persist an edge that points at an unwritten tracker issue.
    pub fn source_link(&self) -> Result<FollowupLink, FollowupError> {
        let followup_work_item = self
            .created_issue_id
            .filter(|_| self.status == FollowupStatus::Created)
            .ok_or(FollowupError::NotCreated {
                id: self.id,
                current: self.status,
            })?;
        Ok(FollowupLink {
            followup_id: self.id,
            source_work_item: self.source_work_item,
            followup_work_item,
            blocking: self.blocking,
        })
    }

    /// Cancel a non-terminal request (proposal withdrawn, scope change,
    /// agent rerun). `note` becomes the [`Self::approval_note`] for
    /// audit trace; `approval_role` is left untouched because a
    /// cancellation is not an approver decision.
    pub fn cancel(&mut self, note: Option<String>) -> Result<(), FollowupError> {
        if self.status.is_terminal() {
            return Err(FollowupError::AlreadyTerminal {
                id: self.id,
                current: self.status,
                requested: FollowupStatus::Cancelled,
            });
        }
        if let Some(n) = note
            && !n.trim().is_empty()
        {
            self.approval_note = Some(n);
        }
        self.status = FollowupStatus::Cancelled;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocker::RunRef;

    fn run_origin() -> BlockerOrigin {
        BlockerOrigin::Run {
            run_id: RunRef::new(7),
            role: Some(RoleName::new("backend")),
        }
    }

    fn proposed_followup() -> FollowupIssueRequest {
        FollowupIssueRequest::try_new(
            FollowupId::new(1),
            WorkItemId::new(42),
            "rate-limit cleanup",
            "tighten retries on 429",
            "symphony-tracker GitHub adapter only",
            vec!["retries respect Retry-After".into()],
            false,
            FollowupPolicy::ProposeForApproval,
            run_origin(),
            true,
        )
        .unwrap()
    }

    fn direct_followup() -> FollowupIssueRequest {
        FollowupIssueRequest::try_new(
            FollowupId::new(2),
            WorkItemId::new(42),
            "missing logs",
            "no warn-level log when poll skips",
            "symphony-core poll loop",
            vec!["warn log emitted on skip".into()],
            true,
            FollowupPolicy::CreateDirectly,
            run_origin(),
            true,
        )
        .unwrap()
    }

    #[test]
    fn followup_policy_round_trips_through_serde_for_every_variant() {
        for p in FollowupPolicy::ALL {
            let json = serde_json::to_string(&p).unwrap();
            assert_eq!(json, format!("\"{}\"", p.as_str()));
            let back: FollowupPolicy = serde_json::from_str(&json).unwrap();
            assert_eq!(back, p);
        }
    }

    #[test]
    fn followup_policy_initial_status_matches_routing() {
        assert_eq!(
            FollowupPolicy::CreateDirectly.initial_status(),
            FollowupStatus::Approved
        );
        assert_eq!(
            FollowupPolicy::ProposeForApproval.initial_status(),
            FollowupStatus::Proposed
        );
    }

    #[test]
    fn followup_policy_default_is_propose_for_approval() {
        assert_eq!(
            FollowupPolicy::default(),
            FollowupPolicy::ProposeForApproval
        );
    }

    #[test]
    fn followup_status_round_trips_through_serde_for_every_variant() {
        for s in FollowupStatus::ALL {
            let json = serde_json::to_string(&s).unwrap();
            assert_eq!(json, format!("\"{}\"", s.as_str()));
            let back: FollowupStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(back, s);
        }
    }

    #[test]
    fn followup_status_terminal_and_actionable_are_disjoint() {
        for s in FollowupStatus::ALL {
            assert!(!(s.is_terminal() && s.is_actionable()));
        }
        assert!(FollowupStatus::Proposed.is_actionable());
        assert!(FollowupStatus::Approved.is_actionable());
        assert!(FollowupStatus::Created.is_terminal());
        assert!(FollowupStatus::Rejected.is_terminal());
        assert!(FollowupStatus::Cancelled.is_terminal());
    }

    #[test]
    fn try_new_rejects_empty_required_text_fields() {
        let blank_cases: &[(&str, &str, &str, FollowupError)] = &[
            ("", "r", "s", FollowupError::EmptyTitle),
            ("   ", "r", "s", FollowupError::EmptyTitle),
            ("t", "", "s", FollowupError::EmptyReason),
            ("t", "  ", "s", FollowupError::EmptyReason),
            ("t", "r", "", FollowupError::EmptyScope),
            ("t", "r", "\n", FollowupError::EmptyScope),
        ];
        for (title, reason, scope, expected) in blank_cases {
            let err = FollowupIssueRequest::try_new(
                FollowupId::new(1),
                WorkItemId::new(2),
                *title,
                *reason,
                *scope,
                vec!["x".into()],
                false,
                FollowupPolicy::ProposeForApproval,
                run_origin(),
                true,
            )
            .unwrap_err();
            assert_eq!(&err, expected);
        }
    }

    #[test]
    fn try_new_rejects_empty_acceptance_criterion() {
        let err = FollowupIssueRequest::try_new(
            FollowupId::new(1),
            WorkItemId::new(2),
            "t",
            "r",
            "s",
            vec!["ok".into(), "  ".into()],
            false,
            FollowupPolicy::ProposeForApproval,
            run_origin(),
            true,
        )
        .unwrap_err();
        assert_eq!(err, FollowupError::EmptyAcceptanceCriterion);
    }

    #[test]
    fn try_new_requires_acceptance_criteria_when_workflow_says_so() {
        let err = FollowupIssueRequest::try_new(
            FollowupId::new(1),
            WorkItemId::new(2),
            "t",
            "r",
            "s",
            vec![],
            false,
            FollowupPolicy::ProposeForApproval,
            run_origin(),
            true,
        )
        .unwrap_err();
        assert_eq!(err, FollowupError::AcceptanceCriteriaRequired);
    }

    #[test]
    fn try_new_allows_empty_acceptance_criteria_when_workflow_disables_requirement() {
        let f = FollowupIssueRequest::try_new(
            FollowupId::new(1),
            WorkItemId::new(2),
            "t",
            "r",
            "s",
            vec![],
            false,
            FollowupPolicy::ProposeForApproval,
            run_origin(),
            false,
        )
        .unwrap();
        assert!(f.acceptance_criteria.is_empty());
        assert_eq!(f.status, FollowupStatus::Proposed);
    }

    #[test]
    fn try_new_initialises_status_from_policy() {
        assert_eq!(proposed_followup().status, FollowupStatus::Proposed);
        assert_eq!(direct_followup().status, FollowupStatus::Approved);
    }

    #[test]
    fn approve_advances_proposed_to_approved_and_records_metadata() {
        let mut f = proposed_followup();
        f.approve(RoleName::new("platform_lead"), "looks good")
            .unwrap();
        assert_eq!(f.status, FollowupStatus::Approved);
        assert_eq!(
            f.approval_role.as_ref().map(RoleName::as_str),
            Some("platform_lead")
        );
        assert_eq!(f.approval_note.as_deref(), Some("looks good"));
    }

    #[test]
    fn approve_rejects_already_approved_followups() {
        let mut f = direct_followup();
        let err = f
            .approve(RoleName::new("platform_lead"), "double-tap")
            .unwrap_err();
        assert_eq!(
            err,
            FollowupError::NotApprovable {
                id: f.id,
                current: FollowupStatus::Approved,
            }
        );
    }

    #[test]
    fn approve_requires_non_empty_note() {
        let mut f = proposed_followup();
        let err = f
            .approve(RoleName::new("platform_lead"), "   ")
            .unwrap_err();
        assert_eq!(err, FollowupError::InvalidApproval);
    }

    #[test]
    fn reject_advances_proposed_to_rejected() {
        let mut f = proposed_followup();
        f.reject(RoleName::new("platform_lead"), "out of scope")
            .unwrap();
        assert_eq!(f.status, FollowupStatus::Rejected);
        assert!(f.status.is_terminal());
    }

    #[test]
    fn reject_from_approved_is_invalid() {
        let mut f = direct_followup();
        let err = f
            .reject(RoleName::new("platform_lead"), "changed mind")
            .unwrap_err();
        assert_eq!(
            err,
            FollowupError::NotApprovable {
                id: f.id,
                current: FollowupStatus::Approved,
            }
        );
    }

    #[test]
    fn mark_created_requires_approved_status() {
        let mut f = proposed_followup();
        let err = f.mark_created(WorkItemId::new(99)).unwrap_err();
        assert_eq!(
            err,
            FollowupError::NotApproved {
                id: f.id,
                current: FollowupStatus::Proposed,
            }
        );
    }

    #[test]
    fn mark_created_advances_approved_to_created() {
        let mut f = direct_followup();
        f.mark_created(WorkItemId::new(99)).unwrap();
        assert_eq!(f.status, FollowupStatus::Created);
        assert_eq!(f.created_issue_id, Some(WorkItemId::new(99)));
    }

    #[test]
    fn mark_created_rejects_self_reference() {
        let mut f = direct_followup();
        let err = f.mark_created(f.source_work_item).unwrap_err();
        assert_eq!(err, FollowupError::SelfReference(f.source_work_item));
        assert_eq!(f.status, FollowupStatus::Approved);
    }

    #[test]
    fn cancel_from_proposed_marks_cancelled_and_records_note() {
        let mut f = proposed_followup();
        f.cancel(Some("agent rerun".into())).unwrap();
        assert_eq!(f.status, FollowupStatus::Cancelled);
        assert_eq!(f.approval_note.as_deref(), Some("agent rerun"));
    }

    #[test]
    fn cancel_with_blank_note_does_not_overwrite_existing_note() {
        let mut f = proposed_followup();
        f.approval_note = Some("prior".into());
        f.cancel(Some("   ".into())).unwrap();
        assert_eq!(f.status, FollowupStatus::Cancelled);
        assert_eq!(f.approval_note.as_deref(), Some("prior"));
    }

    #[test]
    fn terminal_statuses_block_all_transitions() {
        let mut f = direct_followup();
        f.mark_created(WorkItemId::new(99)).unwrap();
        let approve_err = f.approve(RoleName::new("x"), "n").unwrap_err();
        assert!(matches!(approve_err, FollowupError::AlreadyTerminal { .. }));
        let reject_err = f.reject(RoleName::new("x"), "n").unwrap_err();
        assert!(matches!(reject_err, FollowupError::AlreadyTerminal { .. }));
        let create_err = f.mark_created(WorkItemId::new(100)).unwrap_err();
        assert!(matches!(create_err, FollowupError::AlreadyTerminal { .. }));
        let cancel_err = f.cancel(None).unwrap_err();
        assert!(matches!(cancel_err, FollowupError::AlreadyTerminal { .. }));
    }

    #[test]
    fn is_blocking_gate_only_true_when_blocking_and_not_terminal() {
        let mut f = direct_followup();
        assert!(f.blocking);
        assert!(f.is_blocking_gate());
        f.mark_created(WorkItemId::new(99)).unwrap();
        assert!(!f.is_blocking_gate());

        let mut g = proposed_followup();
        assert!(!g.blocking);
        assert!(!g.is_blocking_gate());
        g.cancel(None).unwrap();
        assert!(!g.is_blocking_gate());
    }

    #[test]
    fn source_link_requires_created_status() {
        let approved = direct_followup();
        let err = approved.source_link().unwrap_err();
        assert_eq!(
            err,
            FollowupError::NotCreated {
                id: approved.id,
                current: FollowupStatus::Approved,
            }
        );

        let proposed = proposed_followup();
        let err = proposed.source_link().unwrap_err();
        assert_eq!(
            err,
            FollowupError::NotCreated {
                id: proposed.id,
                current: FollowupStatus::Proposed,
            }
        );
    }

    #[test]
    fn source_link_after_mark_created_carries_full_provenance() {
        let mut f = direct_followup();
        f.mark_created(WorkItemId::new(99)).unwrap();
        let link = f.source_link().unwrap();
        assert_eq!(link.followup_id, f.id);
        assert_eq!(link.source_work_item, WorkItemId::new(42));
        assert_eq!(link.followup_work_item, WorkItemId::new(99));
        assert!(link.blocking, "direct_followup is blocking=true");
    }

    #[test]
    fn source_link_carries_non_blocking_flag_through_to_edge_payload() {
        // proposed_followup is non-blocking; advance it through approval
        // and creation so source_link is reachable.
        let mut f = proposed_followup();
        f.approve(RoleName::new("platform_lead"), "ok").unwrap();
        f.mark_created(WorkItemId::new(77)).unwrap();
        let link = f.source_link().unwrap();
        assert!(!link.blocking);
        assert_eq!(link.followup_work_item, WorkItemId::new(77));
    }

    #[test]
    fn source_link_rejected_after_terminal_off_ramps() {
        // Cancellation and rejection both leave created_issue_id None,
        // so source_link must refuse rather than skipping the gate.
        let mut cancelled = proposed_followup();
        cancelled.cancel(Some("agent rerun".into())).unwrap();
        let err = cancelled.source_link().unwrap_err();
        assert_eq!(
            err,
            FollowupError::NotCreated {
                id: cancelled.id,
                current: FollowupStatus::Cancelled,
            }
        );

        let mut rejected = proposed_followup();
        rejected
            .reject(RoleName::new("platform_lead"), "out of scope")
            .unwrap();
        let err = rejected.source_link().unwrap_err();
        assert_eq!(
            err,
            FollowupError::NotCreated {
                id: rejected.id,
                current: FollowupStatus::Rejected,
            }
        );
    }

    #[test]
    fn followup_link_round_trips_through_serde() {
        let mut f = direct_followup();
        f.mark_created(WorkItemId::new(99)).unwrap();
        let link = f.source_link().unwrap();
        let json = serde_json::to_string(&link).unwrap();
        let back: FollowupLink = serde_json::from_str(&json).unwrap();
        assert_eq!(link, back);
    }

    #[test]
    fn followup_round_trips_through_json_with_full_payload() {
        let mut f = direct_followup();
        f.mark_created(WorkItemId::new(99)).unwrap();
        let json = serde_json::to_string(&f).unwrap();
        let back: FollowupIssueRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(f, back);
    }
}
