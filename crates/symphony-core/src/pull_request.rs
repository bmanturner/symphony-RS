//! Domain model for pull-request records (SPEC v2 §5.11; ARCH §3.3, §6.2).
//!
//! A [`PullRequestRecord`] is the durable proof that the integration owner
//! opened (and progressed) a pull request for a consolidated parent. The
//! kernel reads it when:
//!
//! * QA is staged against a draft PR (SPEC §5.11: "QA runs against the
//!   draft PR branch and records CI/check status as evidence").
//! * The integration owner promotes draft → ready after QA passes.
//! * Parent closeout needs to know the PR's final state (merged vs
//!   closed) to mark the parent done.
//!
//! This module supplies only the *domain* shape. Persistence (the
//! `pull_request_records` table) lives in `symphony-state`. The pattern
//! mirrors [`crate::IntegrationRecord`] — invariants are enforced once at
//! construction via [`PullRequestRecord::try_new`], and terminal records
//! are durable evidence (operators append a fresh row on each material
//! change rather than mutating a terminal one).

use serde::{Deserialize, Serialize};

use crate::blocker::RunRef;
use crate::integration::IntegrationId;
use crate::role::RoleName;
use crate::work_item::WorkItemId;

/// Strongly-typed primary key for a [`PullRequestRecord`].
///
/// Matches the `i64` row id used by the durable storage table; kept as a
/// distinct Rust type from sibling ids ([`WorkItemId`], [`IntegrationId`])
/// so accidental swaps are type errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PullRequestRecordId(pub i64);

impl PullRequestRecordId {
    /// Construct a [`PullRequestRecordId`] from a raw row id.
    pub fn new(id: i64) -> Self {
        Self(id)
    }

    /// Borrow the inner row id.
    pub fn get(self) -> i64 {
        self.0
    }
}

impl std::fmt::Display for PullRequestRecordId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

/// Forge adapter that produced the PR (SPEC v2 §5.11 `provider`).
///
/// Today only GitHub satisfies the PR mutations described in SPEC §5.11.
/// New providers MUST land alongside an ADR in `ARCHITECTURE_v2.md` so the
/// adapter scope rule (CHECKLIST Phase 4) stays explicit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestProvider {
    /// GitHub PR (REST/GraphQL).
    Github,
}

impl PullRequestProvider {
    /// All variants in declaration order.
    pub const ALL: [Self; 1] = [Self::Github];

    /// Stable lowercase identifier used in YAML config and JSON payloads.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Github => "github",
        }
    }
}

impl std::fmt::Display for PullRequestProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// PR lifecycle state (SPEC v2 §5.11).
///
/// The default flow is `Draft` → `Ready` → `Merged`. `Closed` covers PRs
/// abandoned without merge (parent cancelled, scope changed). Each
/// terminal variant ([`Self::Merged`], [`Self::Closed`]) is sticky.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestState {
    /// Draft PR opened by the integration owner; QA runs against this
    /// branch and records CI/check status as evidence.
    Draft,
    /// Promoted from draft → ready for review (SPEC §5.11
    /// `mark_ready_stage`). Reviewers may now request changes.
    Ready,
    /// PR was merged into the base ref. Terminal; parent closeout reads
    /// this to mark the parent done where the workflow uses PR-on-merge.
    Merged,
    /// PR was closed without merging. Terminal; preserved for audit.
    Closed,
}

impl PullRequestState {
    /// All variants in declaration order.
    pub const ALL: [Self; 4] = [Self::Draft, Self::Ready, Self::Merged, Self::Closed];

    /// Stable lowercase identifier used in YAML config and JSON payloads.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Ready => "ready",
            Self::Merged => "merged",
            Self::Closed => "closed",
        }
    }

    /// True when no further state changes are expected.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Merged | Self::Closed)
    }
}

impl std::fmt::Display for PullRequestState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Invariant violations for [`PullRequestRecord::try_new`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PullRequestRecordError {
    /// `title` was empty/whitespace-only.
    #[error("pull request record requires a non-empty title")]
    EmptyTitle,
    /// `head_branch` was empty/whitespace-only.
    #[error("pull request record requires a non-empty head_branch")]
    EmptyHeadBranch,
    /// A `base_branch`/`head_sha`/`url` field was supplied as an empty
    /// string. We reject the empty-string case rather than treating it
    /// as "set" because the storage layer treats empty strings as
    /// distinct from `NULL` and downstream consumers expect either a
    /// real value or `None`.
    #[error("optional field {0} must be omitted or non-empty")]
    EmptyOptionalField(&'static str),
    /// `state` is [`PullRequestState::Ready`] / [`PullRequestState::Merged`]
    /// without a forge-assigned PR `number`. The integration owner cannot
    /// promote a draft to ready (or record a merge) without the forge id.
    #[error("pull request state {0} requires a forge-assigned number")]
    MissingNumber(PullRequestState),
    /// [`PullRequestState::Merged`] requires a `url` so QA evidence and
    /// the tracker comment can link back to the merged PR.
    #[error("merged pull request record requires a url")]
    MissingUrlForMerged,
}

/// Durable pull-request record (SPEC v2 §5.11; ARCH §3.3, §6.2).
///
/// One row per material PR change observed by the integration owner: open,
/// update, mark-ready, merge, close. Earlier rows are preserved as audit;
/// the parent-closeout gate and the QA brief read the latest row. The
/// optional `integration_id` link is the natural join from the record back
/// to the consolidation attempt that produced the branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestRecord {
    /// Durable id (matches the storage row id).
    pub id: PullRequestRecordId,
    /// The work item the PR is for (typically a decomposed parent).
    pub parent_id: WorkItemId,
    /// Role that owned the PR side effect. Typically the workflow's
    /// `integration_owner` role, mirroring [`crate::IntegrationRecord`].
    pub owner_role: RoleName,
    /// The integration-owner run that produced this row, when one
    /// exists. `None` is allowed for rows the kernel scheduled but did
    /// not yet attribute to a run (e.g. a queued PR-open task).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunRef>,
    /// Integration record this PR consolidates from. `None` for
    /// workflows that bypass the integration owner (rare; only valid
    /// when the workflow disables decomposition entirely).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integration_id: Option<IntegrationId>,
    /// Forge adapter responsible for the PR.
    pub provider: PullRequestProvider,
    /// Lifecycle state.
    pub state: PullRequestState,
    /// Forge-assigned PR number. Required for [`PullRequestState::Ready`],
    /// [`PullRequestState::Merged`], and [`PullRequestState::Closed`];
    /// optional for [`PullRequestState::Draft`] rows that were queued
    /// before the forge accepted the open call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number: Option<i64>,
    /// Canonical PR URL on the forge. Required for
    /// [`PullRequestState::Merged`] so QA evidence and tracker comments
    /// can link back to the artifact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Head ref the PR proposes to merge (i.e. the canonical
    /// integration branch). Required and non-empty so QA always has a
    /// branch to fetch.
    pub head_branch: String,
    /// Base ref the PR targets (e.g. `main`). Optional because the
    /// integration owner may queue a PR-open row before the base is
    /// resolved from `integration_records.base_ref`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
    /// Head SHA at record time, when known. Pairs with the matching
    /// field on [`crate::IntegrationRecord`] so QA can confirm it is
    /// inspecting the same commit on the PR that was integrated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    /// Templated PR title rendered at record time. Required and
    /// non-empty: the SPEC's title template MUST resolve before the PR
    /// is opened, and a missing title would leave the forge call
    /// without a subject.
    pub title: String,
    /// Rendered PR body. Optional because draft rows scheduled before
    /// the integration summary is available may carry no body yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// CI/check rollup string captured by the integration owner. Stored
    /// opaquely; QA consumes it as evidence per SPEC §5.12
    /// `evidence_required.tests` and §5.11
    /// `require_ci_green_before_ready`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ci_status: Option<String>,
}

impl PullRequestRecord {
    /// Construct a record, enforcing the SPEC §5.11 invariants. See
    /// [`PullRequestRecordError`] for the rule set.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        id: PullRequestRecordId,
        parent_id: WorkItemId,
        owner_role: RoleName,
        run_id: Option<RunRef>,
        integration_id: Option<IntegrationId>,
        provider: PullRequestProvider,
        state: PullRequestState,
        number: Option<i64>,
        url: Option<String>,
        head_branch: String,
        base_branch: Option<String>,
        head_sha: Option<String>,
        title: String,
        body: Option<String>,
        ci_status: Option<String>,
    ) -> Result<Self, PullRequestRecordError> {
        if title.trim().is_empty() {
            return Err(PullRequestRecordError::EmptyTitle);
        }
        if head_branch.trim().is_empty() {
            return Err(PullRequestRecordError::EmptyHeadBranch);
        }
        for (name, value) in [
            ("base_branch", base_branch.as_deref()),
            ("head_sha", head_sha.as_deref()),
            ("url", url.as_deref()),
        ] {
            if value.is_some_and(str::is_empty) {
                return Err(PullRequestRecordError::EmptyOptionalField(name));
            }
        }

        match state {
            PullRequestState::Ready | PullRequestState::Merged | PullRequestState::Closed
                if number.is_none() =>
            {
                return Err(PullRequestRecordError::MissingNumber(state));
            }
            _ => {}
        }
        if state == PullRequestState::Merged
            && url.as_deref().is_none_or(|u| u.trim().is_empty())
        {
            return Err(PullRequestRecordError::MissingUrlForMerged);
        }

        Ok(Self {
            id,
            parent_id,
            owner_role,
            run_id,
            integration_id,
            provider,
            state,
            number,
            url,
            head_branch,
            base_branch,
            head_sha,
            title,
            body,
            ci_status,
        })
    }

    /// True when the PR has merged successfully.
    pub fn is_merged(&self) -> bool {
        self.state == PullRequestState::Merged
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role() -> RoleName {
        RoleName::new("platform_lead")
    }

    fn draft() -> PullRequestRecord {
        PullRequestRecord::try_new(
            PullRequestRecordId::new(1),
            WorkItemId::new(10),
            role(),
            Some(RunRef::new(7)),
            Some(IntegrationId::new(3)),
            PullRequestProvider::Github,
            PullRequestState::Draft,
            None,
            None,
            "symphony/integration/PROJ-42".into(),
            Some("main".into()),
            Some("deadbeef".into()),
            "PROJ-42: integrate consolidation".into(),
            Some("body".into()),
            Some("queued".into()),
        )
        .expect("draft constructs")
    }

    #[test]
    fn state_and_provider_round_trip_through_json() {
        for state in PullRequestState::ALL {
            let json = serde_json::to_string(&state).unwrap();
            assert_eq!(json, format!("\"{}\"", state.as_str()));
            let back: PullRequestState = serde_json::from_str(&json).unwrap();
            assert_eq!(back, state);
        }
        assert!(PullRequestState::Merged.is_terminal());
        assert!(PullRequestState::Closed.is_terminal());
        assert!(!PullRequestState::Draft.is_terminal());
        assert!(!PullRequestState::Ready.is_terminal());

        for provider in PullRequestProvider::ALL {
            let json = serde_json::to_string(&provider).unwrap();
            assert_eq!(json, format!("\"{}\"", provider.as_str()));
        }
    }

    #[test]
    fn record_round_trips_through_json() {
        let record = draft();
        let json = serde_json::to_string(&record).unwrap();
        let back: PullRequestRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back, record);
        assert!(!record.is_merged());
    }

    #[test]
    fn empty_title_is_rejected() {
        let err = PullRequestRecord::try_new(
            PullRequestRecordId::new(1),
            WorkItemId::new(10),
            role(),
            None,
            None,
            PullRequestProvider::Github,
            PullRequestState::Draft,
            None,
            None,
            "branch".into(),
            None,
            None,
            "   ".into(),
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(err, PullRequestRecordError::EmptyTitle);
    }

    #[test]
    fn empty_head_branch_is_rejected() {
        let err = PullRequestRecord::try_new(
            PullRequestRecordId::new(1),
            WorkItemId::new(10),
            role(),
            None,
            None,
            PullRequestProvider::Github,
            PullRequestState::Draft,
            None,
            None,
            "".into(),
            None,
            None,
            "title".into(),
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(err, PullRequestRecordError::EmptyHeadBranch);
    }

    #[test]
    fn empty_optional_strings_are_rejected() {
        for (field, builder) in [
            (
                "base_branch",
                Box::new(|| {
                    PullRequestRecord::try_new(
                        PullRequestRecordId::new(1),
                        WorkItemId::new(10),
                        role(),
                        None,
                        None,
                        PullRequestProvider::Github,
                        PullRequestState::Draft,
                        None,
                        None,
                        "branch".into(),
                        Some(String::new()),
                        None,
                        "title".into(),
                        None,
                        None,
                    )
                }) as Box<dyn Fn() -> _>,
            ),
            (
                "head_sha",
                Box::new(|| {
                    PullRequestRecord::try_new(
                        PullRequestRecordId::new(1),
                        WorkItemId::new(10),
                        role(),
                        None,
                        None,
                        PullRequestProvider::Github,
                        PullRequestState::Draft,
                        None,
                        None,
                        "branch".into(),
                        None,
                        Some(String::new()),
                        "title".into(),
                        None,
                        None,
                    )
                }),
            ),
            (
                "url",
                Box::new(|| {
                    PullRequestRecord::try_new(
                        PullRequestRecordId::new(1),
                        WorkItemId::new(10),
                        role(),
                        None,
                        None,
                        PullRequestProvider::Github,
                        PullRequestState::Draft,
                        None,
                        Some(String::new()),
                        "branch".into(),
                        None,
                        None,
                        "title".into(),
                        None,
                        None,
                    )
                }),
            ),
        ] {
            let err = builder().unwrap_err();
            assert_eq!(err, PullRequestRecordError::EmptyOptionalField(field));
        }
    }

    #[test]
    fn ready_merged_closed_require_pr_number() {
        for state in [
            PullRequestState::Ready,
            PullRequestState::Merged,
            PullRequestState::Closed,
        ] {
            let err = PullRequestRecord::try_new(
                PullRequestRecordId::new(1),
                WorkItemId::new(10),
                role(),
                None,
                None,
                PullRequestProvider::Github,
                state,
                None,
                Some("https://github.com/o/r/pull/1".into()),
                "branch".into(),
                None,
                None,
                "title".into(),
                None,
                None,
            )
            .unwrap_err();
            assert_eq!(err, PullRequestRecordError::MissingNumber(state));
        }
    }

    #[test]
    fn merged_requires_url() {
        let err = PullRequestRecord::try_new(
            PullRequestRecordId::new(1),
            WorkItemId::new(10),
            role(),
            Some(RunRef::new(7)),
            Some(IntegrationId::new(3)),
            PullRequestProvider::Github,
            PullRequestState::Merged,
            Some(42),
            None,
            "branch".into(),
            Some("main".into()),
            None,
            "title".into(),
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(err, PullRequestRecordError::MissingUrlForMerged);
    }

    #[test]
    fn merged_with_number_and_url_constructs() {
        let record = PullRequestRecord::try_new(
            PullRequestRecordId::new(1),
            WorkItemId::new(10),
            role(),
            Some(RunRef::new(7)),
            Some(IntegrationId::new(3)),
            PullRequestProvider::Github,
            PullRequestState::Merged,
            Some(42),
            Some("https://github.com/o/r/pull/42".into()),
            "branch".into(),
            Some("main".into()),
            Some("cafef00d".into()),
            "title".into(),
            None,
            Some("success".into()),
        )
        .expect("merged constructs");
        assert!(record.is_merged());
    }

    #[test]
    fn id_round_trips_as_bare_integer() {
        let id = PullRequestRecordId::new(42);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "42");
        let back: PullRequestRecordId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }
}
