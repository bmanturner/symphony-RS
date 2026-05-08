//! Adapter-agnostic conformance suite for the [`IssueTracker`] trait.
//!
//! Every concrete adapter (Linear, GitHub Issues, Mock, …) MUST satisfy a
//! handful of cross-cutting properties that the orchestrator relies on:
//!
//! 1. **Active-state filtering at the adapter boundary.** SPEC §11.1 op 1
//!    says `fetch_active` returns *only* issues currently in one of the
//!    operator-configured `active_states`. The orchestrator does not
//!    re-filter; an adapter that returns a terminal issue from this method
//!    is buggy.
//! 2. **Lowercase normalisation for state comparison.** SPEC §11.3 says
//!    state names compare case-insensitively. Operators write `"In
//!    Progress"` in `WORKFLOW.md`; trackers may report `"in progress"`;
//!    the abstraction must reconcile the two without surprises.
//! 3. **No fabrication of optional fields.** [`Issue::branch_name`],
//!    [`Issue::priority`], and [`Issue::blocked_by`] are `Option`/empty
//!    when the source backend lacks the data. Adapters must not invent
//!    them — see the field docs on [`Issue`].
//! 4. **Stable `fetch_state` ordering.** Adapters must preserve the
//!    caller-requested id order so the orchestrator can index by position
//!    during reconciliation.
//! 5. **`fetch_terminal_recent` honours the caller's terminal-state list.**
//!
//! ## How a real adapter plugs in
//!
//! 1. Build a tracker handle that, for the duration of the test, behaves as
//!    if the configured backend held the issues in [`Scenario::active_issues`]
//!    and [`Scenario::terminal_issues`]. For Linear/GitHub adapters this
//!    means standing up a `wiremock` server returning canned GraphQL/REST
//!    payloads encoding the same issues.
//! 2. Hand the tracker (as `&dyn IssueTracker`) plus the [`Scenario`] to
//!    [`run_full_suite`]. Any property violation panics with a descriptive
//!    message naming the failing rule and the offending issue.
//!
//! The [`MockTracker`] case is wired up in
//! `crates/symphony-tracker/tests/conformance.rs`; future adapters should
//! add an `rstest` `#[case]` there pointing at their fixture-builder.
//!
//! ## Why panics instead of `Result<(), Vec<String>>`
//!
//! Conformance helpers are called from `#[tokio::test]` bodies. A panic
//! with a clear message is the idiomatic test failure mode and integrates
//! cleanly with `assert_*` macros elsewhere in the same test. Returning a
//! result would just push the `.unwrap()` outward without adding signal.

use crate::{IssueTracker, MockTracker};
use std::collections::HashSet;
use symphony_core::tracker::{BlockerRef, Issue, IssueId, IssueState};

/// Canned fixture describing what a tracker is "holding" for the duration
/// of a conformance run.
///
/// Mirrors the operator-configured shape: a list of state names that count
/// as active and a list that count as terminal (both as lowercase strings,
/// matching how `WORKFLOW.md` stores them after `figment` parsing), plus
/// the issues each state bucket contains.
///
/// Adapter-specific test setup turns this into a backend that, when probed
/// via the trait methods, behaves as if these issues were live in the
/// upstream tracker. The conformance assertions then operate against the
/// trait — never against the fixture data directly — so the suite catches
/// adapters that diverge from the contract while loading data correctly.
#[derive(Debug, Clone)]
pub struct Scenario {
    /// State names (lowercased) the operator configured as active. The
    /// adapter MUST treat any issue whose state equals one of these
    /// case-insensitively as active.
    pub active_states: Vec<String>,

    /// State names (lowercased) the operator configured as terminal. The
    /// adapter MUST honour these in `fetch_terminal_recent` and EXCLUDE
    /// them from `fetch_active`.
    pub terminal_states: Vec<String>,

    /// Issues that should be reported by `fetch_active`. Their `state`
    /// field uses mixed casing on purpose — the suite verifies the
    /// adapter normalises during the comparison, not during storage.
    pub active_issues: Vec<Issue>,

    /// Issues that should be reported by `fetch_terminal_recent` when the
    /// caller's `terminal_states` argument matches.
    pub terminal_issues: Vec<Issue>,
}

impl Scenario {
    /// All ids the scenario knows about, regardless of bucket. Used by
    /// [`assert_fetch_state_preserves_caller_ordering`] to drive the
    /// `fetch_state` call.
    pub fn all_ids(&self) -> Vec<IssueId> {
        self.active_issues
            .iter()
            .chain(self.terminal_issues.iter())
            .map(|i| i.id.clone())
            .collect()
    }
}

/// Canonical fixture exercised by the suite.
///
/// Designed to surface every contract rule with a single payload:
///
/// - Active states `["todo", "in progress"]` with mixed-case issue states
///   (`"Todo"`, `"In Progress"`, `"in progress"`) so case-insensitive
///   filtering is exercised.
/// - One issue with `priority = Some(1)`, one with `priority = None` to
///   verify adapters do not synthesize a priority.
/// - One issue with `branch_name = Some(..)`, one with `None` for the
///   same reason.
/// - One issue with `blocked_by` populated, others empty.
/// - Terminal bucket holds two distinct terminal states (`"done"` and
///   `"cancelled"`) so multi-state filtering is exercised.
pub fn canonical_scenario() -> Scenario {
    let active_issues = vec![
        Issue {
            id: IssueId::new("conf-1"),
            identifier: "ABC-1".into(),
            title: "First active issue".into(),
            description: Some("body".into()),
            priority: Some(1),
            state: IssueState::new("Todo"),
            branch_name: Some("feature/abc-1".into()),
            url: Some("https://example.test/ABC-1".into()),
            labels: vec!["bug".into()],
            blocked_by: Vec::new(),
            created_at: Some("2026-05-01T00:00:00Z".into()),
            updated_at: Some("2026-05-02T00:00:00Z".into()),
        },
        Issue {
            id: IssueId::new("conf-2"),
            identifier: "ABC-2".into(),
            title: "Second active issue, no priority or branch".into(),
            description: None,
            priority: None,
            state: IssueState::new("In Progress"),
            branch_name: None,
            url: None,
            labels: Vec::new(),
            blocked_by: Vec::new(),
            created_at: None,
            updated_at: None,
        },
        Issue {
            id: IssueId::new("conf-3"),
            identifier: "ABC-3".into(),
            title: "Third, with a blocker".into(),
            description: None,
            priority: Some(3),
            state: IssueState::new("in progress"),
            branch_name: None,
            url: None,
            labels: vec!["frontend".into()],
            blocked_by: vec![symphony_core::tracker::BlockerRef {
                id: Some(IssueId::new("conf-1")),
                identifier: Some("ABC-1".into()),
                state: Some(IssueState::new("Todo")),
            }],
            created_at: None,
            updated_at: None,
        },
    ];

    let terminal_issues = vec![
        Issue::minimal("conf-9", "ABC-9", "Done one", "Done"),
        Issue::minimal("conf-10", "ABC-10", "Cancelled one", "Cancelled"),
    ];

    Scenario {
        active_states: vec!["todo".into(), "in progress".into()],
        terminal_states: vec!["done".into(), "cancelled".into()],
        active_issues,
        terminal_issues,
    }
}

/// GitHub-flavoured analogue of [`canonical_scenario`].
///
/// The shared [`canonical_scenario`] uses Linear-style identifiers
/// (`ABC-1`) and exercises optional fields the GitHub adapter cannot
/// populate without lying — `priority` is always absent on GitHub Issues
/// (no native field), and `blocked_by` only carries the human identifier
/// (`id` and `state` stay `None` until server-side resolution).
///
/// This fixture mirrors the *shape* of `canonical_scenario` (mixed-case
/// states, multiple terminal states, one issue with a blocker, one
/// issue with `branch_name = Some` recovered via the timeline +
/// linked-PR lookup) while staying honest about what the GitHub backend
/// exposes. Wire-level test setup renders each issue as a GitHub REST
/// payload (with the blocker hint injected into the body so
/// [`crate::github`]'s parser can recover it) and mounts a timeline +
/// pulls responder for the issue with a linked PR.
pub fn github_canonical_scenario() -> Scenario {
    let active_issues = vec![
        Issue {
            id: IssueId::new("1"),
            identifier: "#1".into(),
            title: "First active issue".into(),
            description: Some("Plain body, no blockers.".into()),
            priority: None,
            state: IssueState::new("Todo"),
            branch_name: None,
            url: Some("https://github.test/acme/robot/issues/1".into()),
            labels: vec!["bug".into()],
            blocked_by: Vec::new(),
            created_at: Some("2026-05-01T00:00:00Z".into()),
            updated_at: Some("2026-05-02T00:00:00Z".into()),
        },
        Issue {
            id: IssueId::new("2"),
            identifier: "#2".into(),
            title: "Second active, mixed-case state".into(),
            description: None,
            priority: None,
            state: IssueState::new("In Progress"),
            branch_name: None,
            url: Some("https://github.test/acme/robot/issues/2".into()),
            labels: Vec::new(),
            blocked_by: Vec::new(),
            created_at: Some("2026-05-01T00:00:00Z".into()),
            updated_at: Some("2026-05-02T00:00:00Z".into()),
        },
        Issue {
            id: IssueId::new("3"),
            identifier: "#3".into(),
            title: "Third, blocked on #1".into(),
            // Body text the GitHub adapter scans for blocker hints. The
            // wire fixture sends this as the issue body verbatim.
            description: Some("Cannot start until blocked by #1 lands.".into()),
            priority: None,
            state: IssueState::new("in progress"),
            branch_name: None,
            url: Some("https://github.test/acme/robot/issues/3".into()),
            labels: vec!["frontend".into()],
            blocked_by: vec![BlockerRef {
                id: None,
                identifier: Some("#1".into()),
                state: None,
            }],
            created_at: Some("2026-05-01T00:00:00Z".into()),
            updated_at: Some("2026-05-02T00:00:00Z".into()),
        },
        Issue {
            id: IssueId::new("4"),
            identifier: "#4".into(),
            title: "Fourth, has a linked PR".into(),
            description: Some("Plain body, work already in flight on a PR.".into()),
            priority: None,
            state: IssueState::new("Todo"),
            // Recovered by the adapter via the timeline + pulls
            // follow-up. The wire fixture mounts a CrossReferenced
            // timeline event pointing at PR #200 with head ref
            // `feature/login`, and a `/pulls/200` responder returning
            // that branch.
            branch_name: Some("feature/login".into()),
            url: Some("https://github.test/acme/robot/issues/4".into()),
            labels: Vec::new(),
            blocked_by: Vec::new(),
            created_at: Some("2026-05-01T00:00:00Z".into()),
            updated_at: Some("2026-05-02T00:00:00Z".into()),
        },
    ];

    let terminal_issues = vec![
        Issue {
            id: IssueId::new("9"),
            identifier: "#9".into(),
            title: "Done one".into(),
            description: None,
            priority: None,
            state: IssueState::new("Done"),
            branch_name: None,
            url: Some("https://github.test/acme/robot/issues/9".into()),
            labels: Vec::new(),
            blocked_by: Vec::new(),
            created_at: Some("2026-05-01T00:00:00Z".into()),
            updated_at: Some("2026-05-02T00:00:00Z".into()),
        },
        Issue {
            id: IssueId::new("10"),
            identifier: "#10".into(),
            title: "Cancelled one".into(),
            description: None,
            priority: None,
            state: IssueState::new("Cancelled"),
            branch_name: None,
            url: Some("https://github.test/acme/robot/issues/10".into()),
            labels: Vec::new(),
            blocked_by: Vec::new(),
            created_at: Some("2026-05-01T00:00:00Z".into()),
            updated_at: Some("2026-05-02T00:00:00Z".into()),
        },
    ];

    Scenario {
        active_states: vec!["todo".into(), "in progress".into()],
        terminal_states: vec!["done".into(), "cancelled".into()],
        active_issues,
        terminal_issues,
    }
}

/// Build a [`MockTracker`] pre-loaded so it satisfies the given scenario.
///
/// Used by the in-crate `MockTracker` `#[case]` of the conformance suite;
/// real adapters provide their own analogue (typically a wiremock-backed
/// constructor).
pub fn populate_mock(scenario: &Scenario) -> MockTracker {
    let m = MockTracker::new();
    m.set_active(scenario.active_issues.clone());
    m.set_terminal(scenario.terminal_issues.clone());
    m
}

/// Assert `fetch_active` returns only issues whose state matches one of
/// the operator-configured `active_states` (case-insensitively).
///
/// SPEC §11.1 op 1 / §11.3.
pub async fn assert_active_returns_only_active_states(
    tracker: &dyn IssueTracker,
    scenario: &Scenario,
) {
    let active = tracker
        .fetch_active()
        .await
        .expect("fetch_active must succeed against the canonical scenario");

    let allowed: HashSet<String> = scenario
        .active_states
        .iter()
        .map(|s| s.to_ascii_lowercase())
        .collect();

    for issue in &active {
        let normalized = issue.state.normalized();
        assert!(
            allowed.contains(&normalized),
            "fetch_active returned issue {:?} in state {:?} (normalized {:?}) which is not in active_states {:?} — SPEC §11.1 op 1 violation",
            issue.identifier,
            issue.state.as_str(),
            normalized,
            scenario.active_states,
        );
    }

    // And: every fixture-active issue should appear in the result. If the
    // adapter drops one, the orchestrator silently misses work.
    let returned_ids: HashSet<&IssueId> = active.iter().map(|i| &i.id).collect();
    for fixture in &scenario.active_issues {
        assert!(
            returned_ids.contains(&fixture.id),
            "fetch_active dropped fixture issue {:?} ({}) — adapter is over-filtering",
            fixture.identifier,
            fixture.id,
        );
    }
}

/// Assert that [`IssueState::normalized`] yields lowercase ASCII for every
/// state the adapter surfaces.
///
/// This catches adapters that (re)wrap state names with extra whitespace,
/// embed non-ASCII characters, or otherwise produce values whose
/// normalised form does not equal `to_ascii_lowercase` of the source.
pub async fn assert_state_normalization_is_lowercase(
    tracker: &dyn IssueTracker,
    _scenario: &Scenario,
) {
    let active = tracker
        .fetch_active()
        .await
        .expect("fetch_active must succeed");

    for issue in &active {
        let raw = issue.state.as_str();
        let norm = issue.state.normalized();
        assert_eq!(
            norm,
            raw.to_ascii_lowercase(),
            "IssueState::normalized() must equal as_str().to_ascii_lowercase() — got {:?} vs {:?} for issue {}",
            norm,
            raw.to_ascii_lowercase(),
            issue.identifier,
        );
        assert!(
            norm.chars().all(|c| !c.is_ascii_uppercase()),
            "normalized state still contains uppercase ASCII: {:?}",
            norm,
        );
    }
}

/// Assert that no adapter fabricates `branch_name`, `priority`, or
/// `blocked_by` values that were not in the source fixture.
///
/// We compare the ids returned by the adapter against the fixture and, for
/// each match, require the optional fields to be byte-equal. This catches
/// the failure mode the doc comment on [`Issue`] specifically calls out:
/// an adapter that synthesises a priority from labels, derives a branch
/// name from the identifier, or guesses blockers from free-text body refs
/// and forgets to leave `id = None`.
pub async fn assert_no_fabricated_optionals(tracker: &dyn IssueTracker, scenario: &Scenario) {
    let active = tracker
        .fetch_active()
        .await
        .expect("fetch_active must succeed");

    for returned in &active {
        let Some(fixture) = scenario.active_issues.iter().find(|f| f.id == returned.id) else {
            continue;
        };

        assert_eq!(
            returned.priority, fixture.priority,
            "adapter fabricated/mutated priority for {}: fixture = {:?}, returned = {:?}",
            returned.identifier, fixture.priority, returned.priority,
        );
        assert_eq!(
            returned.branch_name, fixture.branch_name,
            "adapter fabricated/mutated branch_name for {}: fixture = {:?}, returned = {:?}",
            returned.identifier, fixture.branch_name, returned.branch_name,
        );
        assert_eq!(
            returned.blocked_by, fixture.blocked_by,
            "adapter fabricated/mutated blocked_by for {}: fixture = {:?}, returned = {:?}",
            returned.identifier, fixture.blocked_by, returned.blocked_by,
        );
    }
}

/// Assert `fetch_state` returns issues in the same order the caller
/// supplied ids, so the orchestrator's reconcile pass can index by
/// position.
///
/// Adapters that reorder (e.g. because their backend returns by id-asc)
/// MUST re-sort to match input order before returning.
pub async fn assert_fetch_state_preserves_caller_ordering(
    tracker: &dyn IssueTracker,
    scenario: &Scenario,
) {
    let mut ids = scenario.all_ids();
    ids.reverse();

    let refreshed = tracker
        .fetch_state(&ids)
        .await
        .expect("fetch_state must succeed against the canonical scenario");

    // We allow the adapter to skip ids it does not know (the mock does);
    // we only assert that *the ids it does return* appear in the same
    // relative order as the caller-supplied list.
    let returned_order: Vec<&IssueId> = refreshed.iter().map(|i| &i.id).collect();
    let mut expected_order: Vec<&IssueId> = ids
        .iter()
        .filter(|id| returned_order.contains(id))
        .collect();
    expected_order.truncate(returned_order.len());

    assert_eq!(
        returned_order, expected_order,
        "fetch_state must preserve caller-requested ordering; got {:?}, expected {:?}",
        returned_order, expected_order,
    );
}

/// Assert `fetch_terminal_recent` filters by the caller-supplied state
/// list and uses case-insensitive comparison.
pub async fn assert_terminal_recent_filters_by_states(
    tracker: &dyn IssueTracker,
    scenario: &Scenario,
) {
    // Probe with the fixture's terminal-state list, plus mixed-casing to
    // exercise the case-insensitive comparison rule.
    let mixed: Vec<IssueState> = scenario
        .terminal_states
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let s = if i % 2 == 0 {
                s.to_ascii_uppercase()
            } else {
                s.clone()
            };
            IssueState::new(s)
        })
        .collect();

    let returned = tracker
        .fetch_terminal_recent(&mixed)
        .await
        .expect("fetch_terminal_recent must succeed");

    let allowed: HashSet<String> = scenario
        .terminal_states
        .iter()
        .map(|s| s.to_ascii_lowercase())
        .collect();

    for issue in &returned {
        assert!(
            allowed.contains(&issue.state.normalized()),
            "fetch_terminal_recent returned issue {:?} in state {:?} which is not in terminal_states {:?}",
            issue.identifier,
            issue.state.as_str(),
            scenario.terminal_states,
        );
    }

    // And: probing with an empty state list must yield an empty result —
    // adapters MUST NOT fall back to "all terminals" on an empty filter.
    let empty = tracker
        .fetch_terminal_recent(&[])
        .await
        .expect("fetch_terminal_recent must accept an empty filter");
    assert!(
        empty.is_empty(),
        "fetch_terminal_recent(&[]) must return an empty list, got {} issues",
        empty.len(),
    );
}

/// Reconciliation contract: `fetch_state` on a *terminal* issue id MUST
/// return that issue with its terminal state — not silently drop it, and
/// not re-apply active-state filtering that would mask the transition.
///
/// SPEC §16.3 reconciles in-flight workers against the tracker by calling
/// `fetch_issue_states_by_ids` (our [`IssueTracker::fetch_state`]) every
/// tick: any worker whose issue has left the active states gets released.
/// An adapter that mistakenly filters `fetch_state` by `active_states`
/// would silently neuter that release, leaving the orchestrator running
/// against an issue the operator already moved to Done. This assertion
/// catches that whole family of regressions.
///
/// We probe by asking for *every terminal* id the scenario knows about,
/// then asserting:
/// 1. **No silent drops** — every requested id appears in the response.
///    The trait contract on [`IssueTracker::fetch_state`] is explicit
///    about this; a partial-result response would let a "deleted" issue
///    masquerade as still-running.
/// 2. **Correct state** — each returned issue's state normalizes into
///    one of `scenario.terminal_states`. (We compare case-insensitively
///    because `IssueState` preserves source casing — see SPEC §11.3.)
pub async fn assert_state_refresh_reports_terminal_state_for_terminal_ids(
    tracker: &dyn IssueTracker,
    scenario: &Scenario,
) {
    if scenario.terminal_issues.is_empty() {
        // Defensive: a scenario with no terminal bucket can't exercise
        // this rule. Skip silently rather than vacuously passing — a
        // real scenario should always populate both buckets.
        return;
    }
    let ids: Vec<IssueId> = scenario
        .terminal_issues
        .iter()
        .map(|i| i.id.clone())
        .collect();
    let refreshed = tracker
        .fetch_state(&ids)
        .await
        .expect("fetch_state must succeed for terminal ids — the reconcile pass depends on it");

    assert_eq!(
        refreshed.len(),
        ids.len(),
        "fetch_state silently dropped terminal ids — got {} responses for {} requested. \
         Adapter is over-filtering or skipping unknowns; the orchestrator's reconcile pass \
         (SPEC §16.3) would mistake the dropped issues for still-running.",
        refreshed.len(),
        ids.len(),
    );

    let allowed: HashSet<String> = scenario
        .terminal_states
        .iter()
        .map(|s| s.to_ascii_lowercase())
        .collect();
    for issue in &refreshed {
        assert!(
            allowed.contains(&issue.state.normalized()),
            "fetch_state on terminal id {} returned state {:?} which is not one of the \
             scenario's terminal_states {:?}. Reconcile would not detect the transition.",
            issue.identifier,
            issue.state.as_str(),
            scenario.terminal_states,
        );
    }
}

/// Terminal-cleanup contract: when called with the scenario's full
/// terminal-state list, `fetch_terminal_recent` MUST return every issue
/// in the scenario's terminal bucket.
///
/// Used at startup to clean up workspaces for issues that finished while
/// Symphony was offline (SPEC §16.3 terminal cleanup). An adapter that
/// quietly drops some terminal issues — e.g. because it forgets to
/// paginate, or applies an extra label filter — would leave orphaned
/// workspaces. This assertion catches that.
///
/// Companion to [`assert_terminal_recent_filters_by_states`], which
/// covers the *exclusion* direction (no non-terminal leakage) and the
/// empty-filter rule. Together they pin down both sides of the contract.
pub async fn assert_terminal_recent_returns_all_known_terminal_issues(
    tracker: &dyn IssueTracker,
    scenario: &Scenario,
) {
    if scenario.terminal_issues.is_empty() {
        return;
    }
    let states: Vec<IssueState> = scenario
        .terminal_states
        .iter()
        .map(IssueState::new)
        .collect();
    let returned = tracker
        .fetch_terminal_recent(&states)
        .await
        .expect("fetch_terminal_recent must succeed for the scenario's terminal_states");

    let returned_ids: HashSet<&IssueId> = returned.iter().map(|i| &i.id).collect();
    for fixture in &scenario.terminal_issues {
        assert!(
            returned_ids.contains(&fixture.id),
            "fetch_terminal_recent dropped fixture terminal issue {:?} ({}) — \
             startup cleanup would leak its workspace",
            fixture.identifier,
            fixture.id,
        );
    }
}

/// Run every assertion in the suite, in a deterministic order.
///
/// Adapter integration tests typically call this once per scenario after
/// constructing their backend. Individual assertions are also `pub` so
/// adapters can pick a subset (e.g. while iterating on a single failing
/// rule).
pub async fn run_full_suite(tracker: &dyn IssueTracker, scenario: &Scenario) {
    assert_active_returns_only_active_states(tracker, scenario).await;
    assert_state_normalization_is_lowercase(tracker, scenario).await;
    assert_no_fabricated_optionals(tracker, scenario).await;
    assert_fetch_state_preserves_caller_ordering(tracker, scenario).await;
    assert_terminal_recent_filters_by_states(tracker, scenario).await;
    assert_state_refresh_reports_terminal_state_for_terminal_ids(tracker, scenario).await;
    assert_terminal_recent_returns_all_known_terminal_issues(tracker, scenario).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_scenario_includes_mixed_case_states_and_optional_field_variants() {
        let s = canonical_scenario();
        assert!(s.active_issues.iter().any(|i| i.priority.is_some()));
        assert!(s.active_issues.iter().any(|i| i.priority.is_none()));
        assert!(s.active_issues.iter().any(|i| i.branch_name.is_some()));
        assert!(s.active_issues.iter().any(|i| i.branch_name.is_none()));
        assert!(s.active_issues.iter().any(|i| !i.blocked_by.is_empty()));

        // At least one mixed-casing pair so the case-insensitive
        // comparison rule is meaningfully exercised.
        let raws: Vec<&str> = s.active_issues.iter().map(|i| i.state.as_str()).collect();
        assert!(raws.iter().any(|r| r.chars().any(|c| c.is_uppercase())));
        assert!(raws.iter().any(|r| r.chars().all(|c| !c.is_uppercase())));
    }

    #[test]
    fn all_ids_concatenates_active_and_terminal_buckets() {
        let s = canonical_scenario();
        let ids = s.all_ids();
        assert_eq!(ids.len(), s.active_issues.len() + s.terminal_issues.len());
    }
}
