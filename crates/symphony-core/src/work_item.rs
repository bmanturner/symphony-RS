//! Domain model for normalized work items (SPEC v2 §4.2 / §4.3).
//!
//! A work item is the orchestrator's internal representation of a unit of
//! product work. It may correspond 1:1 with a tracker issue, be a child
//! created by decomposition, or a transient proposal awaiting approval.
//!
//! This module introduces three pieces that the rest of v2 builds on:
//!
//! * [`WorkItemId`] — strongly-typed primary key. We reuse the storage
//!   crate's `i64` shape so a domain `WorkItem` can be reconstructed from a
//!   `symphony-state::WorkItemRecord` without going through string parsing.
//!   Conversion happens at the storage seam; this crate stays free of
//!   `rusqlite`/`symphony-state` dependencies.
//! * [`WorkItemStatusClass`] — the eleven normalized status classes the
//!   kernel reasons about. Workflow-defined raw tracker states map into
//!   these via [`StatusClassifier`].
//! * [`WorkItem`] — the normalized record itself. Required fields per
//!   SPEC §4.2; richer optional structures (parent/children, blockers,
//!   QA, integration, PR) land in their own modules in later checklist
//!   items.
//!
//! [`StatusClassifier`] also encodes the case-insensitive raw-state rule
//! (SPEC §5.1 + §11.3) and the `unknown_state_policy` choice. The config
//! crate is responsible for surfacing _config-time_ errors (duplicate raw
//! states across classes, etc.); this module's responsibility is the
//! _runtime_ classify call the kernel makes against a tracker payload.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Strongly-typed primary key for a work item.
///
/// Matches the `i64` row id used by `symphony-state::WorkItemRecord`. We
/// keep them as separate Rust types in separate crates rather than
/// re-exporting so an accidental crate-level swap (storage row id vs
/// domain id) shows up as a type error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkItemId(pub i64);

impl WorkItemId {
    /// Construct a [`WorkItemId`] from a raw row id.
    pub fn new(id: i64) -> Self {
        Self(id)
    }

    /// Borrow the inner row id.
    pub fn get(self) -> i64 {
        self.0
    }
}

impl std::fmt::Display for WorkItemId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

/// Normalized status classes the orchestrator reasons about.
///
/// Workflow YAML maps any number of raw tracker state names into each of
/// these classes (SPEC v2 §5.1). The kernel only ever switches on the
/// normalized class — the raw string is preserved on the [`WorkItem`] for
/// display, prompt rendering, and round-trip back to the tracker.
///
/// `active` is *not* a class. It is derived as every non-terminal,
/// non-`ignore` class; see [`Self::is_active`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkItemStatusClass {
    /// Out of scope; the orchestrator does not act on this work item.
    Ignore,
    /// Eligible for triage/decomposition by the integration owner.
    Intake,
    /// Eligible for specialist dispatch.
    Ready,
    /// Actively owned by an in-flight run.
    Running,
    /// Waiting on dependencies or a human decision.
    Blocked,
    /// Ready for consolidation by the integration owner.
    Integration,
    /// Ready for the QA gate.
    Qa,
    /// Failed QA or integration and needs more work.
    Rework,
    /// Pending human review or approval.
    Review,
    /// Terminal: accepted by all workflow gates.
    Done,
    /// Terminal: intentionally abandoned.
    Cancelled,
}

impl WorkItemStatusClass {
    /// All eleven classes, in the SPEC §4.3 ordering. Useful for
    /// exhaustive iteration in config validation and tests.
    pub const ALL: [Self; 11] = [
        Self::Ignore,
        Self::Intake,
        Self::Ready,
        Self::Running,
        Self::Blocked,
        Self::Integration,
        Self::Qa,
        Self::Rework,
        Self::Review,
        Self::Done,
        Self::Cancelled,
    ];

    /// Stable lowercase identifier used in YAML config and JSON payloads.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ignore => "ignore",
            Self::Intake => "intake",
            Self::Ready => "ready",
            Self::Running => "running",
            Self::Blocked => "blocked",
            Self::Integration => "integration",
            Self::Qa => "qa",
            Self::Rework => "rework",
            Self::Review => "review",
            Self::Done => "done",
            Self::Cancelled => "cancelled",
        }
    }

    /// Parse the lowercase identifier emitted by [`Self::as_str`].
    ///
    /// Matches case-insensitively so config snippets like `Done` round-trip
    /// the same as `done`. Returns `None` for any other string.
    pub fn from_normalized(s: &str) -> Option<Self> {
        let lower = s.to_ascii_lowercase();
        Self::ALL.into_iter().find(|c| c.as_str() == lower)
    }

    /// Terminal classes never re-enter the work pipeline.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Cancelled)
    }

    /// Active classes are every non-terminal class that can still produce
    /// work — i.e. everything except [`Self::Ignore`], [`Self::Done`], and
    /// [`Self::Cancelled`]. SPEC v2 §5.1.
    pub fn is_active(self) -> bool {
        !matches!(self, Self::Ignore | Self::Done | Self::Cancelled)
    }
}

impl std::fmt::Display for WorkItemStatusClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Raw tracker status name with case-insensitive ASCII equality.
///
/// Mirrors the established [`crate::tracker::IssueState`] semantics
/// (preserve original casing for display, compare lower-ASCII) while
/// living in the v2 `WorkItem` namespace so the kernel can migrate off
/// `Issue::state` without reaching into the legacy module.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TrackerStatus(String);

impl TrackerStatus {
    /// Construct a [`TrackerStatus`] preserving the caller's casing.
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// Original operator-facing label. Use for logs and prompt rendering.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Lowercase ASCII form. Use for state-mapping lookups.
    pub fn normalized(&self) -> String {
        self.0.to_ascii_lowercase()
    }
}

impl PartialEq for TrackerStatus {
    fn eq(&self, other: &Self) -> bool {
        self.0.eq_ignore_ascii_case(&other.0)
    }
}

impl Eq for TrackerStatus {}

impl std::hash::Hash for TrackerStatus {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        for b in self.0.as_bytes() {
            b.to_ascii_lowercase().hash(state);
        }
    }
}

impl std::fmt::Display for TrackerStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Normalized work item record (SPEC v2 §4.2 + §4.3).
///
/// Required fields are non-optional; optional structured fields (parent,
/// children, blockers, QA, integration, PR, run summary) are introduced
/// in dedicated modules in subsequent Phase 3 checklist items so their
/// invariants live next to their types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkItem {
    /// Domain primary key (matches the storage row id).
    pub id: WorkItemId,
    /// Tracker adapter identifier (`github`, `linear`, …).
    pub tracker_id: String,
    /// Human-facing key, e.g. `OWNER/REPO#42` or `ENG-101`.
    pub identifier: String,
    /// Tracker title, verbatim.
    pub title: String,
    /// Tracker description. `None` distinguishes unset from empty.
    pub description: Option<String>,
    /// Raw tracker status string at last sync. Preserved verbatim so the
    /// orchestrator can write back the exact label and templated prompts
    /// can echo it for the operator.
    pub tracker_status: TrackerStatus,
    /// Normalized status class derived from `tracker_status` via
    /// [`StatusClassifier::classify`].
    pub status_class: WorkItemStatusClass,
    /// Tracker-provided priority. `None` if the tracker has no numeric
    /// priority field for this issue. Adapters MUST NOT fabricate one.
    pub priority: Option<i32>,
    /// Lowercase-normalized labels (SPEC §11.3). Empty list means the
    /// tracker reported zero labels — distinct from "unknown".
    pub labels: Vec<String>,
    /// Operator-clickable URL, when the tracker exposes one.
    pub url: Option<String>,
    /// Decomposition parent, when this work item was filed as a child.
    pub parent_id: Option<WorkItemId>,
}

impl WorkItem {
    /// Convenience constructor for tests with the minimum required fields.
    pub fn minimal(
        id: i64,
        tracker_id: impl Into<String>,
        identifier: impl Into<String>,
        title: impl Into<String>,
        tracker_status: impl Into<String>,
        status_class: WorkItemStatusClass,
    ) -> Self {
        Self {
            id: WorkItemId::new(id),
            tracker_id: tracker_id.into(),
            identifier: identifier.into(),
            title: title.into(),
            description: None,
            tracker_status: TrackerStatus::new(tracker_status),
            status_class,
            priority: None,
            labels: Vec::new(),
            url: None,
            parent_id: None,
        }
    }

    /// Convenience: the work item is in a terminal class.
    pub fn is_terminal(&self) -> bool {
        self.status_class.is_terminal()
    }
}

/// Workflow choice for raw tracker states the [`StatusClassifier`] does
/// not recognize.
///
/// SPEC v2 §5.1: `error` is the default. `ignore` silently maps unknown
/// raw states to [`WorkItemStatusClass::Ignore`] so a stray tracker state
/// can never block startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnknownStatePolicy {
    /// Treat unknown raw states as a runtime/config error.
    #[default]
    Error,
    /// Treat unknown raw states as `ignore`.
    Ignore,
}

/// Errors produced when classifying a raw tracker state.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClassifyError {
    /// The raw state is not configured under any class and the policy is
    /// `error`.
    #[error("unknown tracker state '{raw}' (no mapping under any status class)")]
    UnknownState {
        /// The raw state name as reported by the tracker, original casing.
        raw: String,
    },
    /// The same raw state was registered under more than one class. This
    /// is a config-time invariant; surfacing it at classify time means a
    /// caller built a [`StatusClassifier`] without going through the
    /// validating constructor.
    #[error(
        "tracker state '{raw}' is mapped to multiple classes ({first} and {second}); each raw state must appear in at most one class"
    )]
    DuplicateRawState {
        /// The offending raw state, lowercased.
        raw: String,
        /// The class first observed for this raw state.
        first: WorkItemStatusClass,
        /// The conflicting class.
        second: WorkItemStatusClass,
    },
}

/// Maps raw tracker state strings into normalized [`WorkItemStatusClass`]
/// values using case-insensitive ASCII matching.
///
/// Construct via [`Self::try_new`], which enforces the "each raw state
/// appears in at most one class" invariant once at build time. The
/// runtime [`Self::classify`] call is then a single hash lookup plus the
/// `unknown_state_policy` decision.
#[derive(Debug, Clone)]
pub struct StatusClassifier {
    by_lower: BTreeMap<String, WorkItemStatusClass>,
    unknown_policy: UnknownStatePolicy,
}

impl StatusClassifier {
    /// Build a classifier from a per-class list of raw tracker state
    /// names.
    ///
    /// Raw state names are folded to lower-ASCII for matching but are
    /// otherwise taken verbatim. If the same raw state appears under more
    /// than one class, returns [`ClassifyError::DuplicateRawState`] —
    /// callers should map this onto their config-load error type.
    pub fn try_new<I, J, S>(
        mappings: I,
        unknown_policy: UnknownStatePolicy,
    ) -> Result<Self, ClassifyError>
    where
        I: IntoIterator<Item = (WorkItemStatusClass, J)>,
        J: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut by_lower: BTreeMap<String, WorkItemStatusClass> = BTreeMap::new();
        for (class, raws) in mappings {
            for raw in raws {
                let key = raw.as_ref().to_ascii_lowercase();
                if let Some(existing) = by_lower.get(&key)
                    && *existing != class
                {
                    return Err(ClassifyError::DuplicateRawState {
                        raw: key,
                        first: *existing,
                        second: class,
                    });
                }
                by_lower.insert(key, class);
            }
        }
        Ok(Self {
            by_lower,
            unknown_policy,
        })
    }

    /// The configured unknown-state policy.
    pub fn unknown_policy(&self) -> UnknownStatePolicy {
        self.unknown_policy
    }

    /// Number of distinct raw states currently mapped.
    pub fn len(&self) -> usize {
        self.by_lower.len()
    }

    /// True when no raw states are configured.
    pub fn is_empty(&self) -> bool {
        self.by_lower.is_empty()
    }

    /// Classify a raw tracker state.
    ///
    /// Matches case-insensitively. Unknown raw states return either
    /// `Ok(Ignore)` or `Err(UnknownState)` depending on
    /// [`UnknownStatePolicy`].
    pub fn classify(&self, raw: &str) -> Result<WorkItemStatusClass, ClassifyError> {
        let key = raw.to_ascii_lowercase();
        if let Some(class) = self.by_lower.get(&key) {
            return Ok(*class);
        }
        match self.unknown_policy {
            UnknownStatePolicy::Ignore => Ok(WorkItemStatusClass::Ignore),
            UnknownStatePolicy::Error => Err(ClassifyError::UnknownState {
                raw: raw.to_string(),
            }),
        }
    }

    /// Convenience: classify a [`TrackerStatus`] preserving the original
    /// casing in any error.
    pub fn classify_status(
        &self,
        status: &TrackerStatus,
    ) -> Result<WorkItemStatusClass, ClassifyError> {
        self.classify(status.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_mapping() -> Vec<(WorkItemStatusClass, Vec<&'static str>)> {
        vec![
            (WorkItemStatusClass::Ignore, vec!["Archived"]),
            (WorkItemStatusClass::Intake, vec!["Backlog", "Todo"]),
            (WorkItemStatusClass::Ready, vec!["Ready"]),
            (WorkItemStatusClass::Running, vec!["In Progress"]),
            (WorkItemStatusClass::Blocked, vec!["Blocked"]),
            (WorkItemStatusClass::Integration, vec!["Integration"]),
            (WorkItemStatusClass::Qa, vec!["QA", "In Review"]),
            (WorkItemStatusClass::Rework, vec!["Rework"]),
            (WorkItemStatusClass::Review, vec!["Review"]),
            (WorkItemStatusClass::Done, vec!["Done"]),
            (WorkItemStatusClass::Cancelled, vec!["Cancelled"]),
        ]
    }

    #[test]
    fn work_item_id_round_trips_through_serde_as_a_bare_integer() {
        let id = WorkItemId::new(42);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "42");
        let back: WorkItemId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn status_class_str_round_trip_is_lossless_for_every_variant() {
        for class in WorkItemStatusClass::ALL {
            let s = class.as_str();
            assert_eq!(WorkItemStatusClass::from_normalized(s), Some(class));
        }
    }

    #[test]
    fn status_class_from_normalized_is_case_insensitive() {
        assert_eq!(
            WorkItemStatusClass::from_normalized("Done"),
            Some(WorkItemStatusClass::Done)
        );
        assert_eq!(
            WorkItemStatusClass::from_normalized("INTAKE"),
            Some(WorkItemStatusClass::Intake)
        );
        assert_eq!(WorkItemStatusClass::from_normalized("nope"), None);
    }

    #[test]
    fn status_class_terminal_and_active_partition_per_spec() {
        let terminal: Vec<_> = WorkItemStatusClass::ALL
            .into_iter()
            .filter(|c| c.is_terminal())
            .collect();
        assert_eq!(
            terminal,
            vec![WorkItemStatusClass::Done, WorkItemStatusClass::Cancelled]
        );

        let active: Vec<_> = WorkItemStatusClass::ALL
            .into_iter()
            .filter(|c| c.is_active())
            .collect();
        // SPEC v2 §5.1: active is every non-terminal, non-`ignore` class.
        assert_eq!(
            active,
            vec![
                WorkItemStatusClass::Intake,
                WorkItemStatusClass::Ready,
                WorkItemStatusClass::Running,
                WorkItemStatusClass::Blocked,
                WorkItemStatusClass::Integration,
                WorkItemStatusClass::Qa,
                WorkItemStatusClass::Rework,
                WorkItemStatusClass::Review,
            ]
        );

        // No class is both terminal and active.
        for class in WorkItemStatusClass::ALL {
            assert!(!(class.is_terminal() && class.is_active()));
        }
    }

    #[test]
    fn status_class_serializes_as_snake_case() {
        let json = serde_json::to_string(&WorkItemStatusClass::Cancelled).unwrap();
        assert_eq!(json, "\"cancelled\"");
        let back: WorkItemStatusClass = serde_json::from_str(&json).unwrap();
        assert_eq!(back, WorkItemStatusClass::Cancelled);
    }

    #[test]
    fn tracker_status_equality_is_case_insensitive_ascii() {
        let a = TrackerStatus::new("In Progress");
        let b = TrackerStatus::new("in progress");
        let c = TrackerStatus::new("IN PROGRESS");
        assert_eq!(a, b);
        assert_eq!(b, c);
        assert_ne!(a, TrackerStatus::new("Done"));
    }

    #[test]
    fn tracker_status_preserves_original_casing_for_display() {
        let s = TrackerStatus::new("In Progress");
        assert_eq!(s.as_str(), "In Progress");
        assert_eq!(s.to_string(), "In Progress");
        assert_eq!(s.normalized(), "in progress");
    }

    #[test]
    fn classifier_resolves_known_states_case_insensitively() {
        let cls = StatusClassifier::try_new(sample_mapping(), UnknownStatePolicy::Error).unwrap();
        assert_eq!(
            cls.classify("Backlog").unwrap(),
            WorkItemStatusClass::Intake
        );
        assert_eq!(
            cls.classify("backlog").unwrap(),
            WorkItemStatusClass::Intake
        );
        assert_eq!(
            cls.classify("IN PROGRESS").unwrap(),
            WorkItemStatusClass::Running
        );
        assert_eq!(cls.classify("in review").unwrap(), WorkItemStatusClass::Qa);
    }

    #[test]
    fn classifier_unknown_state_with_error_policy_returns_error() {
        let cls = StatusClassifier::try_new(sample_mapping(), UnknownStatePolicy::Error).unwrap();
        let err = cls.classify("Triage").unwrap_err();
        assert!(matches!(err, ClassifyError::UnknownState { raw } if raw == "Triage"));
    }

    #[test]
    fn classifier_unknown_state_with_ignore_policy_returns_ignore_class() {
        let cls = StatusClassifier::try_new(sample_mapping(), UnknownStatePolicy::Ignore).unwrap();
        assert_eq!(cls.classify("Triage").unwrap(), WorkItemStatusClass::Ignore);
    }

    #[test]
    fn classifier_rejects_raw_state_mapped_to_multiple_classes() {
        // 'Done' under both Done and Cancelled — invalid per SPEC v2 §5.1.
        let mappings = vec![
            (WorkItemStatusClass::Done, vec!["Done"]),
            (WorkItemStatusClass::Cancelled, vec!["DONE"]),
        ];
        let err = StatusClassifier::try_new(mappings, UnknownStatePolicy::Error).unwrap_err();
        assert!(matches!(
            err,
            ClassifyError::DuplicateRawState {
                raw,
                first: WorkItemStatusClass::Done,
                second: WorkItemStatusClass::Cancelled,
            } if raw == "done"
        ));
    }

    #[test]
    fn classifier_allows_same_raw_state_repeated_within_one_class() {
        // Operator put 'Todo' twice in the same class — harmless.
        let mappings = vec![(WorkItemStatusClass::Intake, vec!["Todo", "todo"])];
        let cls = StatusClassifier::try_new(mappings, UnknownStatePolicy::Error).unwrap();
        assert_eq!(cls.len(), 1);
        assert_eq!(cls.classify("Todo").unwrap(), WorkItemStatusClass::Intake);
    }

    #[test]
    fn classify_status_preserves_original_casing_in_error() {
        let cls = StatusClassifier::try_new(sample_mapping(), UnknownStatePolicy::Error).unwrap();
        let status = TrackerStatus::new("Triage");
        let err = cls.classify_status(&status).unwrap_err();
        assert!(matches!(err, ClassifyError::UnknownState { raw } if raw == "Triage"));
    }

    #[test]
    fn work_item_round_trips_through_json_preserving_optionality() {
        let item = WorkItem {
            id: WorkItemId::new(7),
            tracker_id: "github".into(),
            identifier: "OWNER/REPO#42".into(),
            title: "Fix the thing".into(),
            description: Some("body".into()),
            tracker_status: TrackerStatus::new("In Progress"),
            status_class: WorkItemStatusClass::Running,
            priority: Some(2),
            labels: vec!["bug".into(), "frontend".into()],
            url: Some("https://example.com/42".into()),
            parent_id: Some(WorkItemId::new(3)),
        };
        let json = serde_json::to_string(&item).unwrap();
        let back: WorkItem = serde_json::from_str(&json).unwrap();
        assert_eq!(back, item);
    }

    #[test]
    fn work_item_minimal_leaves_optional_fields_unset() {
        let item = WorkItem::minimal(
            1,
            "github",
            "OWNER/REPO#1",
            "Title",
            "Todo",
            WorkItemStatusClass::Intake,
        );
        assert_eq!(item.id, WorkItemId::new(1));
        assert!(item.description.is_none());
        assert!(item.priority.is_none());
        assert!(item.url.is_none());
        assert!(item.labels.is_empty());
        assert!(item.parent_id.is_none());
        assert!(!item.is_terminal());
    }
}
