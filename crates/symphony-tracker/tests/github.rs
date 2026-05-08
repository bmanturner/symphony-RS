//! Integration tests for [`GitHubTracker`] driven by `wiremock`.
//!
//! Covers the full surface: `fetch_active` happy path, status-label
//! state derivation, PR filtering, native-state fallback, every row of
//! the [`TrackerError`] mapping table, `fetch_state` ordering and
//! failure modes, `fetch_terminal_recent` filtering, body-text
//! `blocked_by` recovery, and timeline-derived `branch_name` resolution
//! against the most-recently-opened linked PR.
//!
//! ## Why a hand-rolled JSON builder
//!
//! `octocrab::models::issues::Issue` has ~20 required fields (URLs,
//! authors, comment counts, …) so we cannot just author each test
//! issue by struct-literal. [`gh_issue_json`] is a builder that fills
//! every required field with sensible defaults derived from the issue
//! number, leaving the test free to override only the fields that
//! matter (state, labels, body, etc.).
//!
//! [`IssueTracker::fetch_active`]: symphony_tracker::IssueTracker::fetch_active

use std::sync::Arc;

use http::Uri;
use secrecy::SecretString;
use serde_json::{Value, json};
use symphony_core::tracker::{Issue, IssueId, IssueState};
use symphony_core::tracker_trait::TrackerError;
use symphony_tracker::conformance::{Scenario, github_canonical_scenario, run_full_suite};
use symphony_tracker::{GitHubConfig, GitHubTracker, IssueTracker};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Build the JSON payload for a single GitHub issue with every
/// required field populated. Optional knobs cover the cases this slice
/// asserts on.
fn gh_issue_json(
    number: u64,
    title: &str,
    body: Option<&str>,
    state: &str,
    labels: &[&str],
    is_pull_request: bool,
) -> Value {
    let urls = |suffix: &str| format!("https://api.test/repos/acme/robot/issues/{number}{suffix}");
    let labels_json: Vec<Value> = labels
        .iter()
        .enumerate()
        .map(|(i, name)| {
            json!({
                "id": (number * 100 + i as u64),
                "node_id": format!("L_{number}_{i}"),
                "url": format!("https://api.test/repos/acme/robot/labels/{name}"),
                "name": name,
                "color": "ededed",
                "default": false,
            })
        })
        .collect();
    let author = json!({
        "login": "octocat",
        "id": 1u64,
        "node_id": "MDQ6VXNlcjE=",
        "avatar_url": "https://api.test/avatars/octocat",
        "gravatar_id": "",
        "url": "https://api.test/users/octocat",
        "html_url": "https://github.test/octocat",
        "followers_url": "https://api.test/users/octocat/followers",
        "following_url": "https://api.test/users/octocat/following{/other_user}",
        "gists_url": "https://api.test/users/octocat/gists{/gist_id}",
        "starred_url": "https://api.test/users/octocat/starred{/owner}{/repo}",
        "subscriptions_url": "https://api.test/users/octocat/subscriptions",
        "organizations_url": "https://api.test/users/octocat/orgs",
        "repos_url": "https://api.test/users/octocat/repos",
        "events_url": "https://api.test/users/octocat/events{/privacy}",
        "received_events_url": "https://api.test/users/octocat/received_events",
        "type": "User",
        "site_admin": false,
        "name": null,
        "patch_url": null,
    });
    let mut issue = json!({
        "id": number,
        "node_id": format!("I_{number}"),
        "url": urls(""),
        "repository_url": "https://api.test/repos/acme/robot",
        "labels_url": urls("/labels{/name}"),
        "comments_url": urls("/comments"),
        "events_url": urls("/events"),
        "html_url": format!("https://github.test/acme/robot/issues/{number}"),
        "number": number,
        "state": state,
        "title": title,
        "body": body,
        "user": author,
        "labels": labels_json,
        "assignees": [],
        "locked": false,
        "comments": 0,
        "created_at": "2026-05-01T00:00:00Z",
        "updated_at": "2026-05-02T00:00:00Z",
    });
    if is_pull_request {
        // GitHub flags PRs that show up on the issues endpoint with
        // this nested object. Match the wire shape so octocrab decodes
        // it as `Some(PullRequestLink)`.
        issue["pull_request"] = json!({
            "url": urls(""),
            "html_url": format!("https://github.test/acme/robot/pull/{number}"),
            "diff_url": format!("https://github.test/acme/robot/pull/{number}.diff"),
            "patch_url": format!("https://github.test/acme/robot/pull/{number}.patch"),
        });
    }
    issue
}

/// Build a `cross-referenced` timeline event whose `source.issue` is a
/// pull request. This is the wire shape the adapter scans when deriving
/// `branch_name`.
///
/// `opened_at` is the PR's `created_at`, *not* the cross-reference time
/// — the adapter ranks candidates by the PR's open time so newer PRs
/// win. Tests pass an explicit RFC-3339 timestamp so ordering is stable.
fn gh_timeline_cross_ref_pr(pr_number: u64, opened_at: &str) -> Value {
    // The PR-as-issue payload nested under `source.issue`. Reuse
    // [`gh_issue_json`] for parity with the real shape, then mark it as
    // a PR so `pull_request.is_some()` on octocrab's side.
    let mut pr_as_issue = gh_issue_json(
        pr_number,
        &format!("PR #{pr_number}"),
        Some("PR body"),
        "open",
        &[],
        true,
    );
    // Override `created_at` so the test controls ordering. The default
    // baked into `gh_issue_json` is fine for issues, but PR ordering
    // matters here.
    pr_as_issue["created_at"] = json!(opened_at);
    json!({
        "id": pr_number * 1000,
        "node_id": format!("E_{pr_number}"),
        "url": format!("https://api.test/repos/acme/robot/issues/events/{}", pr_number * 1000),
        "actor": null,
        // Kebab case on the wire — octocrab maps it to
        // `Event::CrossReferenced` via `#[serde(rename = "cross-referenced")]`.
        "event": "cross-referenced",
        "commit_id": null,
        "commit_url": null,
        "created_at": opened_at,
        "source": {
            "type": "issue",
            "issue": pr_as_issue,
        },
    })
}

/// Build the JSON payload for a single pull request response with the
/// minimum fields octocrab's `pulls::PullRequest` struct deserializes.
fn gh_pull_request_json(pr_number: u64, head_ref: &str) -> Value {
    let urls =
        |suffix: &str| format!("https://api.test/repos/acme/robot/pulls/{pr_number}{suffix}");
    let user = json!({
        "login": "octocat",
        "id": 1u64,
        "node_id": "MDQ6VXNlcjE=",
        "avatar_url": "https://api.test/avatars/octocat",
        "gravatar_id": "",
        "url": "https://api.test/users/octocat",
        "html_url": "https://github.test/octocat",
        "followers_url": "https://api.test/users/octocat/followers",
        "following_url": "https://api.test/users/octocat/following{/other_user}",
        "gists_url": "https://api.test/users/octocat/gists{/gist_id}",
        "starred_url": "https://api.test/users/octocat/starred{/owner}{/repo}",
        "subscriptions_url": "https://api.test/users/octocat/subscriptions",
        "organizations_url": "https://api.test/users/octocat/orgs",
        "repos_url": "https://api.test/users/octocat/repos",
        "events_url": "https://api.test/users/octocat/events{/privacy}",
        "received_events_url": "https://api.test/users/octocat/received_events",
        "type": "User",
        "site_admin": false,
        "name": null,
        "patch_url": null,
    });
    let head_or_base = |ref_field: &str| {
        json!({
            "label": format!("acme:{ref_field}"),
            "ref": ref_field,
            "sha": "deadbeefcafef00ddeadbeefcafef00ddeadbeef",
            "user": user,
            "repo": null,
        })
    };
    json!({
        "url": urls(""),
        "id": pr_number * 7,
        "node_id": format!("PR_{pr_number}"),
        "html_url": format!("https://github.test/acme/robot/pull/{pr_number}"),
        "diff_url": urls(".diff"),
        "patch_url": urls(".patch"),
        "issue_url": format!("https://api.test/repos/acme/robot/issues/{pr_number}"),
        "commits_url": urls("/commits"),
        "review_comments_url": urls("/comments"),
        "review_comment_url": "https://api.test/repos/acme/robot/pulls/comments{/number}",
        "comments_url": format!("https://api.test/repos/acme/robot/issues/{pr_number}/comments"),
        "statuses_url": urls("/statuses/abc"),
        "number": pr_number,
        "state": "open",
        "title": format!("PR #{pr_number}"),
        "body": null,
        "labels": [],
        "created_at": "2026-05-03T00:00:00Z",
        "updated_at": "2026-05-03T00:00:00Z",
        "closed_at": null,
        "merged_at": null,
        "merge_commit_sha": null,
        // octocrab requires these on `PullRequest` deserialisation —
        // they are not `#[serde(default)]`. Empty/false values are fine
        // for our branch-name lookup.
        "merged": false,
        "assignees": [],
        "requested_reviewers": [],
        "requested_teams": [],
        "head": head_or_base(head_ref),
        "base": head_or_base("main"),
        "_links": {
            "self": { "href": urls("") },
            "html": { "href": format!("https://github.test/acme/robot/pull/{pr_number}") },
            "issue": { "href": format!("https://api.test/repos/acme/robot/issues/{pr_number}") },
            "comments": { "href": format!("https://api.test/repos/acme/robot/issues/{pr_number}/comments") },
            "review_comments": { "href": urls("/comments") },
            "review_comment": { "href": "https://api.test/repos/acme/robot/pulls/comments{/number}" },
            "commits": { "href": urls("/commits") },
            "statuses": { "href": urls("/statuses/abc") },
        },
        "user": user,
        "author_association": "OWNER",
        "additions": 0,
        "deletions": 0,
        "changed_files": 0,
        "commits": 1,
        "review_comments": 0,
        "comments": 0,
    })
}

/// Mount the timeline + `/pulls/{n}` responders that the adapter walks
/// when recovering `branch_name`. Replaces any default-empty timeline
/// previously mounted for the same issue (wiremock matches on insertion
/// order; later mounts win for the same path).
async fn install_linked_pr(
    server: &MockServer,
    issue_number: u64,
    pr_number: u64,
    head_ref: &str,
    opened_at: &str,
) {
    Mock::given(method("GET"))
        .and(path(format!(
            "/repos/acme/robot/issues/{issue_number}/timeline"
        )))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!([gh_timeline_cross_ref_pr(pr_number, opened_at),])),
        )
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/repos/acme/robot/pulls/{pr_number}")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(gh_pull_request_json(pr_number, head_ref)),
        )
        .mount(server)
        .await;
}

/// Stand up a wiremock server that returns the supplied issues from
/// `GET /repos/acme/robot/issues`, plus a [`GitHubTracker`] pointed at
/// it. Returns both so the test can keep the server alive.
async fn tracker_serving(issues: Vec<Value>) -> (MockServer, GitHubTracker) {
    let server = MockServer::start().await;
    // Mount a default-empty timeline for every issue we serve. The
    // adapter walks `/issues/{n}/timeline` to recover `branch_name`;
    // without these mounts every test would fail with a wiremock 404
    // even when it doesn't care about branch derivation.
    for issue in &issues {
        if let Some(n) = issue.get("number").and_then(|v| v.as_u64()) {
            Mock::given(method("GET"))
                .and(path(format!("/repos/acme/robot/issues/{n}/timeline")))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
                .mount(&server)
                .await;
        }
    }
    Mock::given(method("GET"))
        .and(path("/repos/acme/robot/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(Value::Array(issues)))
        .mount(&server)
        .await;

    let base_uri: Uri = server.uri().parse().expect("parse wiremock uri");
    let cfg = GitHubConfig {
        token: SecretString::from("ghp_test".to_string()),
        owner: "acme".into(),
        repo: "robot".into(),
        active_states: vec!["todo".into(), "in progress".into()],
        status_label_prefix: "status:".into(),
        base_uri,
        page_size: 50,
    };
    let tracker = GitHubTracker::new(cfg).expect("GitHubTracker::new");
    (server, tracker)
}

#[tokio::test]
async fn fetch_active_filters_by_status_label_and_drops_pull_requests() {
    let issues = vec![
        // Active via status label "status:todo" → state == "todo".
        gh_issue_json(
            1,
            "Add login form",
            Some("body 1"),
            "open",
            &["status:todo", "frontend"],
            false,
        ),
        // Active via "status:In Progress" — exercise case-insensitive
        // prefix matching and whitespace trimming after the colon.
        gh_issue_json(
            2,
            "Wire backend",
            Some("body 2"),
            "open",
            &["status: In Progress"],
            false,
        ),
        // Open but state derives to "blocked" → not in active_states,
        // must be filtered out.
        gh_issue_json(3, "Held up", None, "open", &["status:blocked"], false),
        // Open with no status label — falls back to native "open",
        // which is not in active_states, must be filtered out.
        gh_issue_json(4, "Loose triage", None, "open", &["frontend"], false),
        // Pull request with status:todo — must be excluded even though
        // state matches, because PRs are not dispatch candidates.
        gh_issue_json(5, "Refactor", None, "open", &["status:todo"], true),
    ];
    let (_server, tracker) = tracker_serving(issues).await;

    let active = tracker.fetch_active().await.expect("fetch_active");
    let identifiers: Vec<String> = active.iter().map(|i| i.identifier.clone()).collect();
    assert_eq!(
        identifiers,
        vec!["#1".to_string(), "#2".into()],
        "only the two status-label-matching non-PR issues should pass through"
    );

    // Spot-check the normalisation rules:
    let i1 = &active[0];
    assert_eq!(i1.id.as_str(), "1");
    assert_eq!(i1.state.as_str(), "todo");
    // The status label itself is stripped from the label list so
    // downstream rules don't double-fire on it.
    assert_eq!(i1.labels, vec!["frontend".to_string()]);
    assert!(
        i1.priority.is_none(),
        "GitHub has no native priority — adapter must leave this None"
    );
    assert!(
        i1.branch_name.is_none(),
        "no linked PR in this fixture's timeline → branch_name must stay None"
    );
    assert!(i1.blocked_by.is_empty(), "blocked_by lands in (c)");
    assert_eq!(i1.description.as_deref(), Some("body 1"));

    // Mixed-case status label round-trips through case-insensitive
    // prefix matching and gets its whitespace trimmed.
    let i2 = &active[1];
    assert_eq!(i2.state.as_str(), "In Progress");
    assert_eq!(i2.state.normalized(), "in progress");
}

#[tokio::test]
async fn fetch_active_falls_back_to_native_state_when_no_status_label_matches() {
    // Use an `active_states` list that includes "open" so the native
    // fallback actually surfaces an issue. This pins down the rule
    // that issues without any `status:` label keep `state = "open"`
    // and are visible only when the operator opts into it.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/robot/issues"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!([gh_issue_json(
                7,
                "Loose",
                None,
                "open",
                &["frontend"],
                false
            )])),
        )
        .mount(&server)
        .await;
    // No linked PR — branch_name derivation expects an empty timeline.
    Mock::given(method("GET"))
        .and(path("/repos/acme/robot/issues/7/timeline"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    let cfg = GitHubConfig {
        token: SecretString::from("k".to_string()),
        owner: "acme".into(),
        repo: "robot".into(),
        active_states: vec!["open".into()],
        status_label_prefix: "status:".into(),
        base_uri: server.uri().parse().unwrap(),
        page_size: 50,
    };
    let tracker = GitHubTracker::new(cfg).unwrap();

    let active = tracker.fetch_active().await.unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].state.as_str(), "open");
}

// ---------------------------------------------------------------------------
// fetch_state — per-id GET, caller order preserved
// ---------------------------------------------------------------------------

/// Build a tracker pointing at `server` with the canonical config used
/// by the failure-mode tests below. Pulled out so each test can mount
/// its own responders and not duplicate the config block.
fn cfg_for(server: &MockServer) -> GitHubConfig {
    GitHubConfig {
        token: SecretString::from("ghp_test".to_string()),
        owner: "acme".into(),
        repo: "robot".into(),
        active_states: vec!["todo".into(), "in progress".into()],
        status_label_prefix: "status:".into(),
        base_uri: server.uri().parse().expect("parse wiremock uri"),
        page_size: 50,
    }
}

#[tokio::test]
async fn fetch_state_preserves_caller_order_across_per_issue_gets() {
    // Mount one responder per issue. Caller asks in order [10, 7, 42];
    // the wiremock responses are mounted in a different sequence (42
    // first, then 7, then 10) so the test would fail if the adapter
    // returned them in mount/discovery order rather than caller order.
    let server = MockServer::start().await;
    for n in [42u64, 7, 10] {
        Mock::given(method("GET"))
            .and(path(format!("/repos/acme/robot/issues/{n}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(gh_issue_json(
                n,
                &format!("issue-{n}"),
                None,
                "open",
                &["status:todo"],
                false,
            )))
            .mount(&server)
            .await;
        // Empty timeline → no linked PR → branch_name stays None.
        // Without this mount the per-id branch-name follow-up triggers
        // a wiremock 404 and the test fails for the wrong reason.
        Mock::given(method("GET"))
            .and(path(format!("/repos/acme/robot/issues/{n}/timeline")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&server)
            .await;
    }
    let tracker = GitHubTracker::new(cfg_for(&server)).unwrap();

    let ids = vec![IssueId::new("10"), IssueId::new("7"), IssueId::new("42")];
    let refreshed = tracker.fetch_state(&ids).await.expect("fetch_state");

    let returned: Vec<String> = refreshed
        .iter()
        .map(|i| i.id.as_str().to_string())
        .collect();
    assert_eq!(returned, vec!["10", "7", "42"]);
    // Spot-check normalization round-trips for one of them.
    assert_eq!(refreshed[1].state.as_str(), "todo");
    assert_eq!(refreshed[1].identifier, "#7");
}

#[tokio::test]
async fn fetch_state_with_non_numeric_id_is_misconfigured_not_a_panic() {
    // No responder mounted — we expect to fail before any HTTP.
    let server = MockServer::start().await;
    let tracker = GitHubTracker::new(cfg_for(&server)).unwrap();
    let err = tracker
        .fetch_state(&[IssueId::new("not-a-number")])
        .await
        .unwrap_err();
    assert!(matches!(err, TrackerError::Misconfigured(_)), "got {err:?}");
}

#[tokio::test]
async fn fetch_state_404_on_one_id_fails_the_whole_call() {
    // Trait contract: adapters MUST NOT silently drop ids — return a
    // structured error or every id with its state. A 404 here therefore
    // bubbles up as a TrackerError.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/robot/issues/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(gh_issue_json(
            1,
            "still here",
            None,
            "open",
            &["status:todo"],
            false,
        )))
        .mount(&server)
        .await;
    // #1's branch-name follow-up needs an empty timeline so the
    // failure under test is the #2 lookup, not an unmounted timeline.
    Mock::given(method("GET"))
        .and(path("/repos/acme/robot/issues/1/timeline"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/robot/issues/2"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "message": "Not Found",
            "documentation_url": "https://docs.test/rest"
        })))
        .mount(&server)
        .await;
    let tracker = GitHubTracker::new(cfg_for(&server)).unwrap();

    let err = tracker
        .fetch_state(&[IssueId::new("1"), IssueId::new("2")])
        .await
        .unwrap_err();
    assert!(matches!(err, TrackerError::Misconfigured(_)), "got {err:?}");
}

// ---------------------------------------------------------------------------
// fetch_terminal_recent — closed listing + state filter
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fetch_terminal_recent_with_empty_filter_short_circuits_no_request() {
    // No responder mounted: a short-circuit means we never hit the
    // server. If the adapter changed to send a request, wiremock would
    // 404 and the result would be a TrackerError instead of Ok(vec![]).
    let server = MockServer::start().await;
    let tracker = GitHubTracker::new(cfg_for(&server)).unwrap();
    let out = tracker.fetch_terminal_recent(&[]).await.unwrap();
    assert!(out.is_empty());
}

#[tokio::test]
async fn fetch_terminal_recent_lists_closed_and_filters_by_derived_state() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/robot/issues"))
        .and(query_param("state", "closed"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            // status:done — should pass the "done" terminal filter.
            gh_issue_json(11, "Done one", None, "closed", &["status:done"], false),
            // status:wontfix — derived state "wontfix", not in the
            // operator's terminal list, must be dropped.
            gh_issue_json(12, "Skipped", None, "closed", &["status:wontfix"], false),
            // No status label, native "closed" — operator did NOT list
            // "closed" as terminal so this must be dropped too.
            gh_issue_json(13, "Naked close", None, "closed", &[], false),
            // PR with a matching label — must be excluded regardless.
            gh_issue_json(14, "Refactor PR", None, "closed", &["status:done"], true),
        ])))
        .mount(&server)
        .await;
    let tracker = GitHubTracker::new(cfg_for(&server)).unwrap();

    let out = tracker
        .fetch_terminal_recent(&[IssueState::new("Done")])
        .await
        .expect("fetch_terminal_recent");
    let identifiers: Vec<String> = out.iter().map(|i| i.identifier.clone()).collect();
    assert_eq!(identifiers, vec!["#11".to_string()]);
    assert_eq!(out[0].state.as_str(), "done");
}

// ---------------------------------------------------------------------------
// Failure-mode mapping — one test per row of the error table in
// `github/adapter.rs`. Driven through `fetch_active` for parity with the
// Linear suite; the error mapper is shared across all three trait
// methods, so covering one entry point is enough.
// ---------------------------------------------------------------------------

async fn tracker_with_status(status: u16, body: Value) -> (MockServer, GitHubTracker) {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/robot/issues"))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(&server)
        .await;
    let tracker = GitHubTracker::new(cfg_for(&server)).unwrap();
    (server, tracker)
}

fn gh_error_body(message: &str) -> Value {
    // Shape the GitHub API uses for non-2xx responses; octocrab decodes
    // this into `Error::GitHub { source, .. }` which we then map.
    json!({
        "message": message,
        "documentation_url": "https://docs.test/rest"
    })
}

#[tokio::test]
async fn http_401_maps_to_unauthorized() {
    let (_server, tracker) = tracker_with_status(401, gh_error_body("bad creds")).await;
    let err = tracker.fetch_active().await.unwrap_err();
    assert!(matches!(err, TrackerError::Unauthorized(_)), "got {err:?}");
}

#[tokio::test]
async fn http_403_maps_to_unauthorized() {
    let (_server, tracker) = tracker_with_status(403, gh_error_body("no scope")).await;
    let err = tracker.fetch_active().await.unwrap_err();
    assert!(matches!(err, TrackerError::Unauthorized(_)), "got {err:?}");
}

#[tokio::test]
async fn http_422_maps_to_misconfigured() {
    // Other 4xx (here: validation failed) lands in Misconfigured per the
    // table — operators usually need to fix the call shape, not retry.
    let (_server, tracker) = tracker_with_status(422, gh_error_body("validation failed")).await;
    let err = tracker.fetch_active().await.unwrap_err();
    assert!(matches!(err, TrackerError::Misconfigured(_)), "got {err:?}");
}

#[tokio::test]
async fn http_500_maps_to_transport_so_reconcile_keeps_running() {
    let (_server, tracker) = tracker_with_status(500, gh_error_body("boom")).await;
    let err = tracker.fetch_active().await.unwrap_err();
    assert!(matches!(err, TrackerError::Transport(_)), "got {err:?}");
}

#[tokio::test]
async fn http_503_maps_to_transport() {
    let (_server, tracker) = tracker_with_status(503, gh_error_body("unavailable")).await;
    let err = tracker.fetch_active().await.unwrap_err();
    assert!(matches!(err, TrackerError::Transport(_)), "got {err:?}");
}

#[tokio::test]
async fn malformed_json_payload_maps_to_malformed() {
    // 200 OK whose body is not parseable JSON. octocrab's page decoder
    // tries `serde_json::from_slice` first; the failure surfaces as
    // `Error::Serde`, which our mapper routes to `Malformed`.
    //
    // Note: a JSON *object* (rather than the expected array) takes a
    // different code path inside octocrab — it tries to find a known
    // pagination attribute (`items`, `workflow_runs`, …) and on miss
    // raises `Error::Other`. That branch is not interesting for our
    // mapping table; the variant we care about is the "wire isn't even
    // JSON" case, which is what this test pins down.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/robot/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not json{"))
        .mount(&server)
        .await;
    let tracker = GitHubTracker::new(cfg_for(&server)).unwrap();
    let err = tracker.fetch_active().await.unwrap_err();
    assert!(matches!(err, TrackerError::Malformed(_)), "got {err:?}");
}

// ---------------------------------------------------------------------------
// Full conformance suite, GitHub-flavoured
// ---------------------------------------------------------------------------
//
// The shared `canonical_scenario()` uses Linear-style identifiers and
// optional fields (`priority = Some(..)`, structured `BlockerRef`s with
// `id` populated) that the GitHub adapter cannot honestly produce — the
// REST surface has no priority field, and `blocked_by` is recovered from
// body text so the blocker's `id` and `state` stay `None`.
//
// `github_canonical_scenario()` mirrors the *shape* of the canonical
// fixture (mixed-case states, multiple terminals, one issue with a
// blocker hint) while staying within what the GitHub backend exposes.
// The wire fixture below renders each scenario issue as a GitHub REST
// payload, with the blocker hint injected verbatim into the issue body
// so `parse_blocked_by` can recover it.

/// Render a [`Scenario`] issue as the GitHub-shaped JSON payload the
/// adapter will receive on the wire.
fn scenario_issue_to_gh_json(issue: &Issue) -> Value {
    // Status label that round-trips to `issue.state` after the adapter's
    // status-prefix derivation. Preserves the fixture's casing so the
    // mixed-case states from the scenario survive the trip.
    let status_label = format!("status:{}", issue.state.as_str());
    // The status label is stripped from `Issue::labels` by `derive_state`,
    // so we need to *add* it on the wire on top of the normal labels.
    let mut wire_labels: Vec<&str> = vec![status_label.as_str()];
    for l in &issue.labels {
        wire_labels.push(l);
    }
    let n: u64 = issue
        .id
        .as_str()
        .parse()
        .expect("github fixture ids are numeric");
    gh_issue_json(
        n,
        &issue.title,
        issue.description.as_deref(),
        // Native state — we drive everything through the status label,
        // so the native field is just whatever shape octocrab needs.
        "open",
        &wire_labels,
        false,
    )
}

/// Stand up a wiremock server primed with a [`Scenario`] and a
/// [`GitHubTracker`] pointed at it.
///
/// Mounts three responder families:
/// * `GET /repos/acme/robot/issues?state=open` → all active issues
/// * `GET /repos/acme/robot/issues?state=closed` → all terminal issues
/// * `GET /repos/acme/robot/issues/{n}` → per-id lookup
///
/// Each per-id mount is keyed on the actual scenario id, so adding a
/// new fixture issue just works.
async fn github_against(scenario: Scenario) -> (MockServer, GitHubTracker) {
    let server = MockServer::start().await;

    let active_payload: Vec<Value> = scenario
        .active_issues
        .iter()
        .map(scenario_issue_to_gh_json)
        .collect();
    Mock::given(method("GET"))
        .and(path("/repos/acme/robot/issues"))
        .and(query_param("state", "open"))
        .respond_with(ResponseTemplate::new(200).set_body_json(Value::Array(active_payload)))
        .mount(&server)
        .await;

    let terminal_payload: Vec<Value> = scenario
        .terminal_issues
        .iter()
        .map(scenario_issue_to_gh_json)
        .collect();
    Mock::given(method("GET"))
        .and(path("/repos/acme/robot/issues"))
        .and(query_param("state", "closed"))
        .respond_with(ResponseTemplate::new(200).set_body_json(Value::Array(terminal_payload)))
        .mount(&server)
        .await;

    for issue in scenario
        .active_issues
        .iter()
        .chain(scenario.terminal_issues.iter())
    {
        let n: u64 = issue.id.as_str().parse().unwrap();
        Mock::given(method("GET"))
            .and(path(format!("/repos/acme/robot/issues/{n}")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(scenario_issue_to_gh_json(issue)),
            )
            .mount(&server)
            .await;

        // Issues with `branch_name = Some(..)` get a real linked-PR
        // timeline below; everyone else gets a default-empty timeline
        // so the adapter's branch-name follow-up cleanly returns None.
        if let Some(head_ref) = issue.branch_name.as_deref() {
            // PR number is synthesized — anything not colliding with an
            // issue id works. We use `n + 1000` so the relationship is
            // obvious in failure output.
            install_linked_pr(&server, n, n + 1000, head_ref, "2026-05-03T00:00:00Z").await;
        } else {
            Mock::given(method("GET"))
                .and(path(format!("/repos/acme/robot/issues/{n}/timeline")))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
                .mount(&server)
                .await;
        }
    }

    let cfg = GitHubConfig {
        token: SecretString::from("ghp_test".to_string()),
        owner: "acme".into(),
        repo: "robot".into(),
        active_states: scenario.active_states.clone(),
        status_label_prefix: "status:".into(),
        base_uri: server.uri().parse().expect("parse wiremock uri"),
        page_size: 50,
    };
    let tracker = GitHubTracker::new(cfg).expect("GitHubTracker::new");
    (server, tracker)
}

#[tokio::test]
async fn github_tracker_passes_the_full_conformance_suite() {
    let scenario = github_canonical_scenario();
    let (_server, tracker) = github_against(scenario.clone()).await;
    let dyn_tracker: Arc<dyn IssueTracker> = Arc::new(tracker);
    run_full_suite(dyn_tracker.as_ref(), &scenario).await;
}

#[tokio::test]
async fn fetch_active_recovers_branch_name_from_most_recently_opened_linked_pr() {
    // Stand up a single-issue server, then mount a timeline with two
    // CrossReferenced PR events. The newer PR's head ref must win.
    let issues = vec![gh_issue_json(
        42,
        "Wire login",
        Some("body"),
        "open",
        &["status:todo"],
        false,
    )];
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/robot/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(Value::Array(issues)))
        .mount(&server)
        .await;
    // Two PRs: an older one on `feature/login-old`, a newer one on
    // `feature/login-new`. The adapter must pick the newer.
    // A non-PR cross-reference (issue → issue) must be ignored: build
    // it from the helper, then strip the `pull_request` flag so the
    // adapter sees a plain issue cross-reference, not a PR one.
    let mut non_pr_event = gh_timeline_cross_ref_pr(101, "2026-04-15T00:00:00Z");
    non_pr_event["source"]["issue"]["pull_request"] = Value::Null;
    let timeline = json!([
        gh_timeline_cross_ref_pr(100, "2026-04-01T00:00:00Z"),
        non_pr_event,
        gh_timeline_cross_ref_pr(200, "2026-05-01T00:00:00Z"),
    ]);
    Mock::given(method("GET"))
        .and(path("/repos/acme/robot/issues/42/timeline"))
        .respond_with(ResponseTemplate::new(200).set_body_json(timeline))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/robot/pulls/100"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(gh_pull_request_json(100, "feature/login-old")),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/robot/pulls/200"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(gh_pull_request_json(200, "feature/login-new")),
        )
        .mount(&server)
        .await;
    let tracker = GitHubTracker::new(cfg_for(&server)).unwrap();

    let active = tracker.fetch_active().await.expect("fetch_active");
    assert_eq!(active.len(), 1);
    assert_eq!(
        active[0].branch_name.as_deref(),
        Some("feature/login-new"),
        "the newer linked PR's head ref must win the tiebreak",
    );
}

#[tokio::test]
async fn fetch_active_recovers_blocked_by_from_issue_body_text() {
    // The conformance suite enforces this end-to-end via the
    // fabrication check, but it's worth a focused assertion so a future
    // refactor that breaks body parsing surfaces with a clear failure
    // (rather than a generic "fabricated optionals" panic).
    let scenario = github_canonical_scenario();
    let (_server, tracker) = github_against(scenario.clone()).await;

    let active = tracker.fetch_active().await.expect("fetch_active");
    let issue_3 = active
        .iter()
        .find(|i| i.id.as_str() == "3")
        .expect("scenario includes #3");
    assert_eq!(issue_3.blocked_by.len(), 1);
    let b = &issue_3.blocked_by[0];
    assert_eq!(b.identifier.as_deref(), Some("#1"));
    assert!(b.id.is_none(), "id stays None until server-side resolution");
    assert!(
        b.state.is_none(),
        "state stays None — body text has no state hint"
    );
}
