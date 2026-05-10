//! Normalized issue model shared by every tracker adapter.
//!
//! This module encodes SPEC §4.1.1 ("Issue") as Rust types. The orchestrator
//! only ever sees these structs — never a Linear GraphQL node, never an
//! `octocrab::models::issues::Issue`, never a Jira REST payload. That
//! architectural seam is what lets us swap trackers without touching the
//! poll loop or state machine.
//!
//! ## Optional-vs-required: the contract for adapters
//!
//! Three fields are deliberately `Option`: [`Issue::branch_name`],
//! [`Issue::priority`], and [`Issue::blocked_by`] (empty list versus
//! "unknown" is part of the contract — see the field docs). Adapters
//! **must not fabricate** these when the source backend lacks them. For
//! example, GitHub Issues has no native priority field; the GitHub adapter
//! must leave [`Issue::priority`] as `None` rather than synthesizing one
//! from labels. The conformance suite (Phase 2) verifies this property
//! against every adapter we ship.
//!
//! Lowercase normalization for [`Issue::labels`] and the per-adapter state
//! comparison rule (SPEC §11.3) are also enforced at the adapter boundary,
//! not by these types — these structs are intentionally `pub` data so that
//! tests can construct them directly.

use serde::{Deserialize, Serialize};

/// Stable tracker-internal identifier for an issue.
///
/// This is the `id` field from SPEC §4.1.1 — the opaque primary key the
/// tracker uses internally (Linear's UUID-ish string, GitHub's numeric id
/// rendered as a string, Jira's issue id, etc). It is **not** the
/// human-readable ticket key (`ABC-123`); that lives in
/// [`Issue::identifier`].
///
/// We wrap it in a newtype so the orchestrator cannot accidentally swap
/// `id` for `identifier` at a call site — the two are semantically
/// different and confusing them silently breaks reconciliation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct IssueId(pub String);

impl IssueId {
    /// Construct an [`IssueId`] from anything string-like.
    ///
    /// Adapters use this when translating their native id type into the
    /// normalized model; tests use it for fixture issues.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Borrow the inner string. Useful when building HTTP query parameters
    /// or log fields where allocating a new `String` would be wasteful.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for IssueId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A tracker state name as reported by the source backend.
///
/// The orchestrator compares states using lowercase equality (SPEC §11.3):
/// `"In Progress"`, `"in progress"`, and `"IN PROGRESS"` are the same
/// state. We preserve the original casing inside the struct so logs and
/// templated prompts can echo the operator-facing name verbatim, but
/// equality and hashing are case-insensitive ASCII.
///
/// The case-insensitive ASCII rule (rather than full Unicode case folding)
/// matches the upstream Symphony Python implementation and keeps the
/// conformance suite deterministic across platforms.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct IssueState(String);

impl IssueState {
    /// Construct an [`IssueState`] preserving the caller's casing.
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// The original, operator-facing state name. Use this for logs and
    /// prompt rendering — never for comparisons.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Lowercase ASCII form. Use this whenever you need to match a state
    /// against [`crate::tracker`] config keys like `active_states` or
    /// `terminal_states`, both of which arrive lowercased from
    /// `WORKFLOW.md`.
    pub fn normalized(&self) -> String {
        self.0.to_ascii_lowercase()
    }
}

impl PartialEq for IssueState {
    fn eq(&self, other: &Self) -> bool {
        self.0.eq_ignore_ascii_case(&other.0)
    }
}

impl Eq for IssueState {}

impl std::hash::Hash for IssueState {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        for b in self.0.as_bytes() {
            b.to_ascii_lowercase().hash(state);
        }
    }
}

impl std::fmt::Display for IssueState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One entry in [`Issue::blocked_by`] — a reference to a blocking issue.
///
/// SPEC §4.1.1 leaves every field nullable because trackers vary in what
/// they expose for inverse relations: Linear gives both `id` and
/// `identifier`, GitHub may give only the issue number parsed from a
/// `blocked by #N` body reference (so `id` is unknown until we resolve
/// it), and the blocker's state may not be embedded in the parent issue's
/// payload at all.
///
/// Templates iterate this list to render "blocked by ABC-12 (In Progress)"
/// style hints, so all three fields are exposed even when only some are
/// populated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockerRef {
    /// Tracker-internal id of the blocker, if the source payload included
    /// it. GitHub body-ref parsing leaves this `None` until the blocker is
    /// resolved server-side.
    pub id: Option<IssueId>,
    /// Human-readable identifier of the blocker (`ABC-12`, `#42`).
    pub identifier: Option<String>,
    /// Current state of the blocker, if embedded in the parent payload.
    pub state: Option<IssueState>,
}

/// Minimal tracker comment shape returned by v5 read tools.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueComment {
    /// Tracker-native comment id.
    pub id: String,
    /// Display name or login of the author.
    pub author: String,
    /// Comment body, usually Markdown.
    pub body: String,
    /// Tracker-created timestamp, when exposed.
    pub created_at: Option<String>,
}

/// Full related-issue context returned by the v5 `get_related_issues`
/// read tool.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RelatedIssues {
    /// Structural parent, if known.
    pub parent: Option<Issue>,
    /// Structural children, if known.
    pub children: Vec<Issue>,
    /// Issues that block the requested issue.
    pub blockers: Vec<Issue>,
}

/// Normalized issue record exchanged between trackers and the orchestrator.
///
/// This is the canonical representation defined in SPEC §4.1.1. Construct
/// it directly in tests; let adapters construct it from their native
/// payloads at runtime. The orchestrator treats it as read-only data.
///
/// ## Fabrication policy
///
/// Adapters **must not fabricate** [`Issue::branch_name`],
/// [`Issue::priority`], or [`Issue::blocked_by`] when the source backend
/// does not provide that information. Use `None` / empty list to signal
/// "unknown" — see the per-field documentation. The Phase 2 conformance
/// suite asserts this contract against every adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    /// Stable tracker-internal id (SPEC §4.1.1 `id`).
    pub id: IssueId,
    /// Human-readable ticket key like `ABC-123` or `#42`. The orchestrator
    /// uses this as the workspace key after sanitization.
    pub identifier: String,
    /// Issue title, verbatim from the tracker.
    pub title: String,
    /// Issue body. `None` distinguishes "no description set" from an empty
    /// string the user explicitly entered.
    pub description: Option<String>,
    /// Lower numbers are higher priority for dispatch sorting (SPEC §4.1.1).
    /// `None` means the tracker did not expose a numeric priority for this
    /// issue. Adapters **must not** synthesize one (e.g. GitHub Issues
    /// must leave this `None` rather than mapping labels to numbers).
    pub priority: Option<i32>,
    /// Current tracker state. Compared case-insensitively (see
    /// [`IssueState`]).
    pub state: IssueState,
    /// Tracker-provided git branch hint, when available. For Linear this
    /// is `Issue.branchName`; for GitHub it is derived from the head ref
    /// of a linked PR if any. `None` means the tracker has no branch
    /// metadata for this issue — adapters **must not** invent one from
    /// the identifier.
    pub branch_name: Option<String>,
    /// Web URL the operator can click in logs. `None` if the tracker
    /// doesn't expose one.
    pub url: Option<String>,
    /// Lowercase-normalized labels (SPEC §11.3). Empty list means "no
    /// labels" — distinct from "unknown".
    pub labels: Vec<String>,
    /// Inverse `blocks` relations exposed by the tracker. Empty list
    /// means "no blockers" — adapters **must not** populate this with
    /// guesses; if the source payload omits blocker data entirely, leave
    /// it empty rather than fabricating entries.
    pub blocked_by: Vec<BlockerRef>,
    /// ISO-8601 timestamp string, as reported by the tracker. We store
    /// the raw string to avoid an early `chrono`/`time` dependency; the
    /// orchestrator never compares timestamps for ordering, only echoes
    /// them in logs and templated prompts.
    pub created_at: Option<String>,
    /// ISO-8601 timestamp string for the most recent tracker update.
    /// Same storage rationale as [`Issue::created_at`].
    pub updated_at: Option<String>,
}

impl Issue {
    /// Convenience constructor for the minimal viable issue used in unit
    /// tests. Real adapters set the optional fields explicitly so the
    /// fabrication contract is visible at the call site.
    pub fn minimal(
        id: impl Into<String>,
        identifier: impl Into<String>,
        title: impl Into<String>,
        state: impl Into<String>,
    ) -> Self {
        Self {
            id: IssueId::new(id),
            identifier: identifier.into(),
            title: title.into(),
            description: None,
            priority: None,
            state: IssueState::new(state),
            branch_name: None,
            url: None,
            labels: Vec::new(),
            blocked_by: Vec::new(),
            created_at: None,
            updated_at: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_id_round_trips_through_serde_as_a_bare_string() {
        let id = IssueId::new("abc-123");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"abc-123\"");
        let back: IssueId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn issue_id_display_emits_the_inner_string() {
        assert_eq!(IssueId::new("ABC-9").to_string(), "ABC-9");
    }

    #[test]
    fn issue_state_equality_is_case_insensitive_ascii() {
        let a = IssueState::new("In Progress");
        let b = IssueState::new("in progress");
        let c = IssueState::new("IN PROGRESS");
        assert_eq!(a, b);
        assert_eq!(b, c);
    }

    #[test]
    fn issue_state_inequality_holds_for_distinct_names() {
        assert_ne!(IssueState::new("Todo"), IssueState::new("Done"));
    }

    #[test]
    fn issue_state_hash_matches_equality() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(IssueState::new("In Progress"));
        // Re-inserting a differently cased equivalent must not grow the set.
        set.insert(IssueState::new("IN progress"));
        assert_eq!(set.len(), 1);
        assert!(set.contains(&IssueState::new("in progress")));
    }

    #[test]
    fn issue_state_preserves_original_casing_for_display() {
        let s = IssueState::new("In Progress");
        assert_eq!(s.as_str(), "In Progress");
        assert_eq!(s.to_string(), "In Progress");
        assert_eq!(s.normalized(), "in progress");
    }

    #[test]
    fn issue_minimal_leaves_optional_fields_unset_so_adapters_must_set_them_explicitly() {
        let issue = Issue::minimal("id-1", "ABC-1", "Title", "Todo");
        assert!(issue.description.is_none());
        assert!(
            issue.priority.is_none(),
            "priority must default to None — see fabrication contract"
        );
        assert!(
            issue.branch_name.is_none(),
            "branch_name must default to None — see fabrication contract"
        );
        assert!(issue.url.is_none());
        assert!(issue.labels.is_empty());
        assert!(
            issue.blocked_by.is_empty(),
            "blocked_by must default to empty — see fabrication contract"
        );
        assert!(issue.created_at.is_none());
        assert!(issue.updated_at.is_none());
    }

    #[test]
    fn blocker_ref_round_trips_through_json_with_all_fields_optional() {
        let blocker = BlockerRef {
            id: Some(IssueId::new("blk-1")),
            identifier: Some("ABC-9".into()),
            state: Some(IssueState::new("Todo")),
        };
        let json = serde_json::to_string(&blocker).unwrap();
        let back: BlockerRef = serde_json::from_str(&json).unwrap();
        assert_eq!(back, blocker);

        // All-None case: GitHub adapter parsing "blocked by #42" before
        // resolving the blocker server-side.
        let sparse = BlockerRef {
            id: None,
            identifier: Some("#42".into()),
            state: None,
        };
        let json = serde_json::to_string(&sparse).unwrap();
        let back: BlockerRef = serde_json::from_str(&json).unwrap();
        assert_eq!(back, sparse);
    }

    #[test]
    fn issue_round_trips_through_json_preserving_optionality() {
        let issue = Issue {
            id: IssueId::new("id-1"),
            identifier: "ABC-1".into(),
            title: "Fix the thing".into(),
            description: Some("body".into()),
            priority: Some(2),
            state: IssueState::new("In Progress"),
            branch_name: Some("feature/abc-1".into()),
            url: Some("https://linear.app/x/issue/ABC-1".into()),
            labels: vec!["bug".into(), "frontend".into()],
            blocked_by: vec![BlockerRef {
                id: Some(IssueId::new("blk-1")),
                identifier: Some("ABC-2".into()),
                state: Some(IssueState::new("Todo")),
            }],
            created_at: Some("2024-01-02T03:04:05Z".into()),
            updated_at: Some("2024-01-02T03:04:06Z".into()),
        };
        let json = serde_json::to_string(&issue).unwrap();
        let back: Issue = serde_json::from_str(&json).unwrap();
        assert_eq!(back, issue);
    }
}
