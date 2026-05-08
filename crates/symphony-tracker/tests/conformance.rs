//! Cross-adapter conformance suite for the [`IssueTracker`] trait.
//!
//! Each adapter shipped under `symphony-tracker` MUST appear here as an
//! `rstest` `#[case]` so the same set of trait-level invariants runs
//! against every implementation. SPEC §11.1/§11.3 properties live in
//! `symphony_tracker::conformance`; this file is the *driver* — its only
//! job is to enumerate adapters and feed each one through the suite.
//!
//! ## How to add a new adapter
//!
//! 1. Build a fixture-loader that, given a [`Scenario`], returns a handle
//!    implementing [`IssueTracker`]. For Linear/GitHub adapters this
//!    means standing up a `wiremock` server keyed on the scenario.
//! 2. Wrap that loader in a [`TrackerHarness::Builder`] entry below and
//!    add it as another `#[case]` to each `#[rstest]` test.
//! 3. The full suite runs against the new adapter automatically; any
//!    failures will surface as a panic from the relevant assertion in
//!    `symphony_tracker::conformance`.

use std::sync::Arc;

use rstest::rstest;
use symphony_tracker::IssueTracker;
use symphony_tracker::conformance::{
    Scenario, assert_active_returns_only_active_states,
    assert_fetch_state_preserves_caller_ordering, assert_no_fabricated_optionals,
    assert_state_normalization_is_lowercase, assert_terminal_recent_filters_by_states,
    canonical_scenario, populate_mock, run_full_suite,
};

/// One adapter under test, packaged as a name + a builder closure.
///
/// We use an opaque `fn(&Scenario) -> Arc<dyn IssueTracker>` rather than a
/// trait so each `#[case]` is a single function pointer — `rstest` plays
/// best with `Copy` case values, and trait objects (or async closures)
/// would force a more elaborate wrapper for no real gain.
///
/// Real adapters that need async setup (HTTP fixtures, temp files) simply
/// expose a synchronous builder that internally drives `tokio::runtime`-
/// independent setup, or returns a handle that lazily provisions on first
/// trait call. The Linear/GitHub adapter authors will pick whichever shape
/// reads cleanest — they only need to satisfy `Fn(&Scenario) ->
/// Arc<dyn IssueTracker>`.
type AdapterBuilder = fn(&Scenario) -> Arc<dyn IssueTracker>;

fn build_mock(scenario: &Scenario) -> Arc<dyn IssueTracker> {
    Arc::new(populate_mock(scenario))
}

/// Convenience constant so future adapters can be added as
/// `#[case::linear(build_linear)]` etc., keeping the case lists short.
#[allow(dead_code)] // Read by rstest macro expansion only.
const MOCK: AdapterBuilder = build_mock;

#[rstest]
#[case::mock(build_mock)]
#[tokio::test]
async fn full_suite_passes_against_canonical_scenario(#[case] build: AdapterBuilder) {
    let scenario = canonical_scenario();
    let tracker = build(&scenario);
    run_full_suite(tracker.as_ref(), &scenario).await;
}

#[rstest]
#[case::mock(build_mock)]
#[tokio::test]
async fn fetch_active_returns_only_configured_active_states(#[case] build: AdapterBuilder) {
    let scenario = canonical_scenario();
    let tracker = build(&scenario);
    assert_active_returns_only_active_states(tracker.as_ref(), &scenario).await;
}

#[rstest]
#[case::mock(build_mock)]
#[tokio::test]
async fn issue_state_normalization_is_lowercase_for_every_returned_issue(
    #[case] build: AdapterBuilder,
) {
    let scenario = canonical_scenario();
    let tracker = build(&scenario);
    assert_state_normalization_is_lowercase(tracker.as_ref(), &scenario).await;
}

#[rstest]
#[case::mock(build_mock)]
#[tokio::test]
async fn adapter_does_not_fabricate_branch_priority_or_blockers(#[case] build: AdapterBuilder) {
    let scenario = canonical_scenario();
    let tracker = build(&scenario);
    assert_no_fabricated_optionals(tracker.as_ref(), &scenario).await;
}

#[rstest]
#[case::mock(build_mock)]
#[tokio::test]
async fn fetch_state_preserves_caller_supplied_id_ordering(#[case] build: AdapterBuilder) {
    let scenario = canonical_scenario();
    let tracker = build(&scenario);
    assert_fetch_state_preserves_caller_ordering(tracker.as_ref(), &scenario).await;
}

#[rstest]
#[case::mock(build_mock)]
#[tokio::test]
async fn fetch_terminal_recent_respects_caller_state_list(#[case] build: AdapterBuilder) {
    let scenario = canonical_scenario();
    let tracker = build(&scenario);
    assert_terminal_recent_filters_by_states(tracker.as_ref(), &scenario).await;
}

// ---------------------------------------------------------------------------
// Negative tests: these confirm the suite *catches* contract violations.
// They construct intentionally-broken scenarios against the mock and assert
// the relevant `assert_*` panics. Without these, a future refactor could
// silently neuter a conformance check and the suite would still pass.
// ---------------------------------------------------------------------------

use symphony_core::tracker::{Issue, IssueId, IssueState};
use symphony_tracker::MockTracker;

#[tokio::test]
async fn suite_rejects_an_adapter_that_leaks_a_terminal_issue_into_active() {
    let mut scenario = canonical_scenario();
    // Slip a "Done" issue into the active bucket. The conformance suite
    // should refuse to accept this even though the adapter dutifully
    // returned what it was given.
    scenario.active_issues.push(Issue::minimal(
        "leaked-1",
        "ABC-LEAK",
        "Leaked terminal",
        "Done",
    ));
    let m = MockTracker::new();
    m.set_active(scenario.active_issues.clone());

    let tracker: Arc<dyn IssueTracker> = Arc::new(m);
    let panicked = tokio::task::spawn(async move {
        assert_active_returns_only_active_states(tracker.as_ref(), &scenario).await;
    })
    .await
    .is_err();
    assert!(
        panicked,
        "suite must panic when an adapter returns a non-active state from fetch_active"
    );
}

#[tokio::test]
async fn suite_rejects_an_adapter_that_fabricates_a_priority() {
    let scenario = canonical_scenario();

    // Build a mock that returns each fixture issue but rewrites priority
    // to `Some(99)` — exactly the GitHub-Issues-from-labels failure mode
    // the contract forbids.
    let m = MockTracker::new();
    let mutated: Vec<Issue> = scenario
        .active_issues
        .iter()
        .map(|i| {
            let mut clone = i.clone();
            clone.priority = Some(99);
            clone
        })
        .collect();
    m.set_active(mutated);

    let tracker: Arc<dyn IssueTracker> = Arc::new(m);
    let scenario_for_task = scenario.clone();
    let panicked = tokio::task::spawn(async move {
        assert_no_fabricated_optionals(tracker.as_ref(), &scenario_for_task).await;
    })
    .await
    .is_err();
    assert!(
        panicked,
        "suite must panic when an adapter mutates the priority field"
    );
}

#[tokio::test]
async fn suite_rejects_an_adapter_whose_terminal_filter_falls_back_on_empty_input() {
    // Build a tracker whose `fetch_terminal_recent` ignores the caller's
    // filter and always returns the entire terminal bucket — exactly the
    // failure mode adapters are tempted into ("if no filter, return all").
    use async_trait::async_trait;
    use symphony_core::tracker_trait::TrackerResult;

    struct BrokenAdapter {
        all_terminal: Vec<Issue>,
    }

    #[async_trait]
    impl IssueTracker for BrokenAdapter {
        async fn fetch_active(&self) -> TrackerResult<Vec<Issue>> {
            Ok(Vec::new())
        }
        async fn fetch_state(&self, _ids: &[IssueId]) -> TrackerResult<Vec<Issue>> {
            Ok(Vec::new())
        }
        async fn fetch_terminal_recent(&self, _states: &[IssueState]) -> TrackerResult<Vec<Issue>> {
            Ok(self.all_terminal.clone())
        }
    }

    let scenario = canonical_scenario();
    let tracker: Arc<dyn IssueTracker> = Arc::new(BrokenAdapter {
        all_terminal: scenario.terminal_issues.clone(),
    });

    let panicked = tokio::task::spawn(async move {
        assert_terminal_recent_filters_by_states(tracker.as_ref(), &scenario).await;
    })
    .await
    .is_err();
    assert!(
        panicked,
        "suite must panic when fetch_terminal_recent ignores an empty filter and returns data"
    );
}
