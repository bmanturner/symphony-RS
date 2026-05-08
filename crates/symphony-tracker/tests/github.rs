//! Integration tests for [`GitHubTracker`] driven by `wiremock`.
//!
//! This first slice exercises the happy path of [`IssueTracker::fetch_active`]:
//! a wiremock server returns a small list of GitHub issues, and the
//! adapter normalises them into our [`Issue`] model. Wider scenarios
//! (4xx/5xx/malformed, `fetch_state`, `fetch_terminal_recent`,
//! `blocked_by`, `branch_name`) land alongside the matching CHECKLIST
//! items (b/c/d).
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

use http::Uri;
use secrecy::SecretString;
use serde_json::{Value, json};
use symphony_core::tracker::{IssueId, IssueState};
use symphony_core::tracker_trait::TrackerError;
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

/// Stand up a wiremock server that returns the supplied issues from
/// `GET /repos/acme/robot/issues`, plus a [`GitHubTracker`] pointed at
/// it. Returns both so the test can keep the server alive.
async fn tracker_serving(issues: Vec<Value>) -> (MockServer, GitHubTracker) {
    let server = MockServer::start().await;
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
    assert!(i1.branch_name.is_none(), "branch_name lands in (d)");
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
