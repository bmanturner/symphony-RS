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
use symphony_tracker::{GitHubConfig, GitHubTracker, IssueTracker};
use wiremock::matchers::{method, path};
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
