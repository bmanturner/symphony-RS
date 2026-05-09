//! Integration tests for [`LinearTracker`] driven by `wiremock`.
//!
//! These tests stand up a local mock GraphQL server and point a real
//! [`LinearTracker`] (with a real `reqwest::Client`) at it. They cover
//! the four wire-level scenarios called out by the Phase 2 checklist:
//!
//! 1. **Happy path** — canned GraphQL responses encode the canonical
//!    conformance scenario, the full conformance suite runs against the
//!    adapter, and every per-property assertion passes.
//! 2. **4xx** — non-auth client error maps to
//!    [`TrackerError::Misconfigured`]; auth errors map to
//!    [`TrackerError::Unauthorized`].
//! 3. **5xx** — server error maps to [`TrackerError::Transport`] so the
//!    orchestrator's reconcile pass keeps running.
//! 4. **Malformed** — a 200 OK with a payload that doesn't match
//!    `schema.graphql` maps to [`TrackerError::Malformed`].
//!
//! ## Why a dedicated `Respond` impl
//!
//! The conformance suite drives `fetch_state(&ids)` with an arbitrary id
//! list, so we cannot hard-code per-id responses. Implementing
//! [`wiremock::Respond`] lets the mock inspect each request body, pick
//! out the GraphQL `operationName` and `variables`, and synthesise the
//! right response shape. That keeps the test fixture single-source-of-
//! truth: `canonical_scenario()` defines what the tracker holds, and
//! the responder serialises that scenario into Linear-shaped GraphQL.

use std::sync::Arc;

use secrecy::SecretString;
use serde_json::{Value, json};
use symphony_core::tracker::{Issue, IssueId, IssueState};
use symphony_core::tracker_trait::{
    AddBlockerRequest, AddCommentRequest, ArtifactKind, AttachArtifactRequest, CreateIssueRequest,
    LinkParentChildRequest, TrackerError, TrackerMutations, UpdateIssueRequest,
};
use symphony_tracker::conformance::{Scenario, canonical_scenario, run_full_suite};
use symphony_tracker::{LinearConfig, LinearTracker, TrackerRead};
use url::Url;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

// ---------------------------------------------------------------------------
// Wire-shape helpers — turn an [`Issue`] from the conformance fixture
// into the Linear-side GraphQL JSON the adapter expects to decode.
// ---------------------------------------------------------------------------

/// Render a single fixture issue as a `CandidateIssues` / `IssuesByStates`
/// node. The adapter normalises priority by rounding > 0.0 to i32, so
/// `priority: Some(1)` round-trips as `1.0` on the wire and back.
fn fixture_issue_to_node(issue: &Issue) -> Value {
    let labels: Vec<Value> = issue.labels.iter().map(|l| json!({ "name": l })).collect();
    let blockers: Vec<Value> = issue
        .blocked_by
        .iter()
        .map(|b| {
            json!({
                "type": "blocks",
                "issue": {
                    "id": b.id.as_ref().map(|i| i.as_str()).unwrap_or(""),
                    "identifier": b.identifier.clone().unwrap_or_default(),
                    "state": {
                        "name": b.state.as_ref().map(|s| s.as_str().to_string()).unwrap_or_default()
                    }
                }
            })
        })
        .collect();
    json!({
        "id": issue.id.as_str(),
        "identifier": issue.identifier,
        "title": issue.title,
        "description": issue.description,
        "priority": issue.priority.map(|p| p as f64),
        "branchName": issue.branch_name,
        "url": issue.url,
        "createdAt": issue.created_at,
        "updatedAt": issue.updated_at,
        "state": { "id": format!("state-{}", issue.state.as_str()), "name": issue.state.as_str() },
        "labels": { "nodes": labels },
        "inverseRelations": { "nodes": blockers },
    })
}

/// Wrap a list of pre-rendered nodes in the Relay envelope the adapter
/// expects. We always set `hasNextPage: false` so the paginator
/// terminates on the first request — pagination logic is exercised by
/// `paginate_*` unit tests in a future iteration.
fn issues_envelope(nodes: Vec<Value>) -> Value {
    json!({
        "data": {
            "issues": {
                "pageInfo": { "hasNextPage": false, "endCursor": null },
                "nodes": nodes,
            }
        }
    })
}

/// Custom [`wiremock::Respond`] that routes by GraphQL `operationName`
/// and synthesises responses from a [`Scenario`].
///
/// The conformance suite is the primary client: it issues every trait
/// method against the same server with the same scenario, so the
/// responder must handle all three operations atomically.
struct ScenarioResponder {
    scenario: Scenario,
}

impl Respond for ScenarioResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body: Value = match serde_json::from_slice(&req.body) {
            Ok(v) => v,
            Err(e) => {
                return ResponseTemplate::new(400)
                    .set_body_string(format!("malformed test request: {e}"));
            }
        };
        let op = body
            .get("operationName")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        match op {
            "CandidateIssues" => {
                // Active-state filtering happens on the server side in
                // production; here we just dump every active issue.
                // Conformance suite asserts the adapter doesn't widen
                // this list, so a passive "return what the fixture
                // declared active" is the right model.
                let nodes = self
                    .scenario
                    .active_issues
                    .iter()
                    .map(fixture_issue_to_node)
                    .collect();
                ResponseTemplate::new(200).set_body_json(issues_envelope(nodes))
            }
            "IssuesByStates" => {
                // Read the requested state names out of the variables
                // and filter the terminal bucket against them, matching
                // how Linear would actually behave.
                let requested: Vec<String> = body
                    .pointer("/variables/stateNames")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|s| s.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default();
                let nodes = self
                    .scenario
                    .terminal_issues
                    .iter()
                    .filter(|i| {
                        requested
                            .iter()
                            .any(|r| r.eq_ignore_ascii_case(i.state.as_str()))
                    })
                    .map(fixture_issue_to_node)
                    .collect();
                ResponseTemplate::new(200).set_body_json(issues_envelope(nodes))
            }
            "IssueStatesByIds" => {
                let requested: Vec<String> = body
                    .pointer("/variables/ids")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|s| s.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default();
                let all = self
                    .scenario
                    .active_issues
                    .iter()
                    .chain(self.scenario.terminal_issues.iter());
                // Deliberately *reverse* server-side ordering so the
                // adapter must re-sort to match the caller's input — a
                // weak adapter that just trusts the wire order would
                // fail the conformance suite's ordering check.
                let mut nodes: Vec<Value> = all
                    .filter(|i| requested.contains(&i.id.0))
                    .map(|i| {
                        json!({
                            "id": i.id.as_str(),
                            "identifier": i.identifier,
                            "state": { "name": i.state.as_str() },
                        })
                    })
                    .collect();
                nodes.reverse();
                ResponseTemplate::new(200).set_body_json(json!({
                    "data": { "issues": { "nodes": nodes } }
                }))
            }
            other => {
                ResponseTemplate::new(400).set_body_string(format!("unexpected operation: {other}"))
            }
        }
    }
}

/// Stand up a wiremock server primed with the given scenario and a
/// [`LinearTracker`] pointed at it. Returns both so the test can keep
/// the server alive (drop = teardown) for the duration.
async fn linear_against(scenario: Scenario) -> (MockServer, LinearTracker) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ScenarioResponder {
            scenario: scenario.clone(),
        })
        .mount(&server)
        .await;
    let cfg = LinearConfig {
        api_key: SecretString::from("test-token".to_string()),
        project_slug: "alpha".into(),
        // The conformance scenario uses lowercase active states; we
        // pass them through verbatim.
        active_states: scenario
            .active_states
            .iter()
            .map(|s| s.to_string())
            .collect(),
        endpoint: Url::parse(&format!("{}/", server.uri())).unwrap(),
        page_size: 50,
        team_id: None,
    };
    let tracker = LinearTracker::new(cfg).expect("LinearTracker::new");
    (server, tracker)
}

// ---------------------------------------------------------------------------
// Happy path: full conformance suite passes against a wiremock-backed
// LinearTracker. This is the test that proves the adapter satisfies the
// trait contract end-to-end (transport + decode + normalise).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn linear_tracker_passes_the_full_conformance_suite() {
    let scenario = canonical_scenario();
    let (_server, tracker) = linear_against(scenario.clone()).await;
    let dyn_tracker: Arc<dyn TrackerRead> = Arc::new(tracker);
    run_full_suite(dyn_tracker.as_ref(), &scenario).await;
}

#[tokio::test]
async fn fetch_active_round_trips_priority_and_blockers_through_the_wire() {
    let scenario = canonical_scenario();
    let (_server, tracker) = linear_against(scenario.clone()).await;

    let active = tracker.fetch_active().await.expect("fetch_active");
    assert_eq!(active.len(), scenario.active_issues.len());

    let by_id: std::collections::HashMap<_, _> = active.iter().map(|i| (i.id.clone(), i)).collect();

    // priority Some(1) should survive Float→i32 round-trip.
    let conf_1 = by_id.get(&IssueId::new("conf-1")).unwrap();
    assert_eq!(conf_1.priority, Some(1));
    assert_eq!(conf_1.branch_name.as_deref(), Some("feature/abc-1"));

    // priority None must NOT be fabricated.
    let conf_2 = by_id.get(&IssueId::new("conf-2")).unwrap();
    assert!(conf_2.priority.is_none());
    assert!(conf_2.branch_name.is_none());

    // blocked_by must be populated and route through the type=="blocks"
    // filter (other inverseRelations would be discarded in production;
    // our fixture only emits "blocks" edges).
    let conf_3 = by_id.get(&IssueId::new("conf-3")).unwrap();
    assert_eq!(conf_3.blocked_by.len(), 1);
    assert_eq!(conf_3.blocked_by[0].identifier.as_deref(), Some("ABC-1"));
}

#[tokio::test]
async fn fetch_state_re_sorts_to_match_caller_supplied_id_order() {
    // The wiremock responder reverses the server-side ordering on
    // purpose; this test pins down that the adapter notices and re-
    // sorts. Without that re-sort, the orchestrator's reconcile pass
    // would index the wrong issue at each position.
    let scenario = canonical_scenario();
    let (_server, tracker) = linear_against(scenario.clone()).await;

    let mut ids = scenario.all_ids();
    ids.reverse();
    let refreshed = tracker.fetch_state(&ids).await.expect("fetch_state");
    let returned: Vec<IssueId> = refreshed.iter().map(|i| i.id.clone()).collect();
    assert_eq!(
        returned, ids,
        "adapter must preserve caller order even when wire order is reversed"
    );
}

#[tokio::test]
async fn fetch_terminal_recent_with_empty_filter_short_circuits_and_does_not_call_server() {
    // We don't even mount a responder: if the adapter were to send a
    // request, wiremock would 404 and the call would error. The fact
    // that this test passes proves the documented "empty filter ⇒
    // empty result, no round-trip" behaviour.
    let server = MockServer::start().await;
    let cfg = LinearConfig {
        api_key: SecretString::from("k".to_string()),
        project_slug: "alpha".into(),
        active_states: vec!["todo".into()],
        endpoint: Url::parse(&format!("{}/", server.uri())).unwrap(),
        page_size: 50,
        team_id: None,
    };
    let tracker = LinearTracker::new(cfg).unwrap();

    let out = tracker.fetch_terminal_recent(&[]).await.unwrap();
    assert!(out.is_empty());
}

// ---------------------------------------------------------------------------
// Failure-mode mapping. Each test pins down a single row of the table
// in the module-level docs of `linear/adapter.rs`.
// ---------------------------------------------------------------------------

async fn tracker_with_status(status: u16, body: &'static str) -> (MockServer, LinearTracker) {
    // Return both — the MockServer must stay alive for the duration of
    // the test, so the caller binds it to a guard variable and lets it
    // drop only when the test function returns.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(status).set_body_string(body))
        .mount(&server)
        .await;
    let tracker = LinearTracker::new(LinearConfig {
        api_key: SecretString::from("k".to_string()),
        project_slug: "alpha".into(),
        active_states: vec!["todo".into()],
        endpoint: Url::parse(&format!("{}/", server.uri())).unwrap(),
        page_size: 50,
        team_id: None,
    })
    .unwrap();
    (server, tracker)
}

#[tokio::test]
async fn http_401_maps_to_unauthorized() {
    let (_server, tracker) = tracker_with_status(401, "bad token").await;
    let err = tracker.fetch_active().await.unwrap_err();
    assert!(matches!(err, TrackerError::Unauthorized(_)), "got {err:?}");
}

#[tokio::test]
async fn http_403_maps_to_unauthorized() {
    let (_server, tracker) = tracker_with_status(403, "no scope").await;
    let err = tracker.fetch_active().await.unwrap_err();
    assert!(matches!(err, TrackerError::Unauthorized(_)), "got {err:?}");
}

#[tokio::test]
async fn http_400_maps_to_misconfigured() {
    let (_server, tracker) = tracker_with_status(400, "bad query").await;
    let err = tracker.fetch_active().await.unwrap_err();
    assert!(matches!(err, TrackerError::Misconfigured(_)), "got {err:?}");
}

#[tokio::test]
async fn http_500_maps_to_transport_so_reconcile_keeps_running() {
    let (_server, tracker) = tracker_with_status(500, "internal error").await;
    let err = tracker.fetch_active().await.unwrap_err();
    assert!(matches!(err, TrackerError::Transport(_)), "got {err:?}");
}

#[tokio::test]
async fn http_503_maps_to_transport() {
    let (_server, tracker) = tracker_with_status(503, "service unavailable").await;
    let err = tracker.fetch_active().await.unwrap_err();
    assert!(matches!(err, TrackerError::Transport(_)), "got {err:?}");
}

#[tokio::test]
async fn malformed_json_payload_maps_to_malformed() {
    // 200 OK with a payload whose `data` shape doesn't match
    // `candidate_issues::ResponseData`. graphql_client's envelope decode
    // surfaces this as a serde error which we route to Malformed.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "data": { "issues": "not-an-object" } })),
        )
        .mount(&server)
        .await;
    let tracker = LinearTracker::new(LinearConfig {
        api_key: SecretString::from("k".to_string()),
        project_slug: "alpha".into(),
        active_states: vec!["todo".into()],
        endpoint: Url::parse(&format!("{}/", server.uri())).unwrap(),
        page_size: 50,
        team_id: None,
    })
    .unwrap();
    let err = tracker.fetch_active().await.unwrap_err();
    assert!(matches!(err, TrackerError::Malformed(_)), "got {err:?}");
}

#[tokio::test]
async fn graphql_errors_array_maps_to_transport() {
    // Linear convention: errors come back with HTTP 200 and a populated
    // `errors` array. We treat these as transport-class so the
    // orchestrator keeps polling rather than dispatching against
    // possibly-stale state.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": null,
            "errors": [{ "message": "rate limited" }]
        })))
        .mount(&server)
        .await;
    let tracker = LinearTracker::new(LinearConfig {
        api_key: SecretString::from("k".to_string()),
        project_slug: "alpha".into(),
        active_states: vec!["todo".into()],
        endpoint: Url::parse(&format!("{}/", server.uri())).unwrap(),
        page_size: 50,
        team_id: None,
    })
    .unwrap();
    let err = tracker.fetch_active().await.unwrap_err();
    assert!(matches!(err, TrackerError::Transport(_)), "got {err:?}");
    assert!(err.to_string().contains("rate limited"));
}

#[tokio::test]
async fn label_lowercasing_happens_at_the_adapter_boundary() {
    // Linear may report labels with mixed casing; SPEC §11.3 stores
    // labels lowercased so template lookups match operator-configured
    // rules. Build a one-issue scenario whose wire payload uses mixed
    // case and assert the normalised result is lowercase.
    let server = MockServer::start().await;
    let raw = json!({
        "data": {
            "issues": {
                "pageInfo": { "hasNextPage": false, "endCursor": null },
                "nodes": [{
                    "id": "lin-1",
                    "identifier": "ABC-1",
                    "title": "t",
                    "description": null,
                    "priority": null,
                    "branchName": null,
                    "url": null,
                    "createdAt": null,
                    "updatedAt": null,
                    "state": { "id": "s", "name": "Todo" },
                    "labels": { "nodes": [{ "name": "Bug" }, { "name": "FRONTEND" }] },
                    "inverseRelations": { "nodes": [] }
                }]
            }
        }
    });
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(raw))
        .mount(&server)
        .await;
    let tracker = LinearTracker::new(LinearConfig {
        api_key: SecretString::from("k".to_string()),
        project_slug: "alpha".into(),
        active_states: vec!["todo".into()],
        endpoint: Url::parse(&format!("{}/", server.uri())).unwrap(),
        page_size: 50,
        team_id: None,
    })
    .unwrap();
    let active = tracker.fetch_active().await.unwrap();
    assert_eq!(active[0].labels, vec!["bug".to_string(), "frontend".into()]);
    // And state is preserved with original casing — adapter lowercases
    // labels but NOT the state name (state preserves caller casing per
    // `IssueState`'s contract).
    assert_eq!(active[0].state, IssueState::new("Todo"));
    assert_eq!(active[0].state.as_str(), "Todo");
}

// ---------------------------------------------------------------------------
// TrackerMutations — wiremock-driven happy paths for each Linear mutation.
//
// Each test stands up a single MockServer that route-matches by GraphQL
// `operationName` and returns the canned response shape Linear would send.
// We assert both that the adapter built a syntactically correct mutation
// (operation name, inputs flow into `variables`) and that the response
// decodes into the trait-level response struct without fabrication.
// ---------------------------------------------------------------------------

use wiremock::matchers::body_partial_json;

fn mutation_tracker(server_uri: &str, team_id: Option<&str>) -> LinearTracker {
    LinearTracker::new(LinearConfig {
        api_key: SecretString::from("k".to_string()),
        project_slug: "alpha".into(),
        active_states: vec!["todo".into()],
        endpoint: Url::parse(&format!("{}/", server_uri)).unwrap(),
        page_size: 50,
        team_id: team_id.map(|s| s.to_string()),
    })
    .unwrap()
}

#[tokio::test]
async fn capabilities_advertise_full_mutation_surface() {
    // The adapter promises the FULL capability set (SPEC v2 §7.2). This
    // pins the contract: workflows that gate on structural blockers /
    // sub-issues are allowed to dispatch against Linear without runtime
    // probing.
    use symphony_core::tracker_trait::TrackerCapabilities;
    let server = MockServer::start().await;
    let tracker = mutation_tracker(&server.uri(), None);
    assert_eq!(tracker.capabilities(), TrackerCapabilities::FULL);
}

#[tokio::test]
async fn create_issue_routes_through_issue_create_with_parent_id() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"operationName": "IssueCreate"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "issueCreate": {
                    "success": true,
                    "issue": {
                        "id": "lin-new",
                        "identifier": "ABC-9",
                        "url": "https://linear.app/x/issue/ABC-9"
                    }
                }
            }
        })))
        .mount(&server)
        .await;
    let tracker = mutation_tracker(&server.uri(), Some("team-1"));

    let resp = tracker
        .create_issue(CreateIssueRequest {
            title: "child".into(),
            body: Some("body".into()),
            labels: vec![],
            assignees: vec![],
            parent: Some(IssueId::new("lin-parent")),
            initial_state: None,
        })
        .await
        .expect("create_issue");
    assert_eq!(resp.id, IssueId::new("lin-new"));
    assert_eq!(resp.identifier, "ABC-9");
    assert_eq!(
        resp.url.as_deref(),
        Some("https://linear.app/x/issue/ABC-9")
    );
}

#[tokio::test]
async fn create_issue_without_team_id_is_misconfigured() {
    let server = MockServer::start().await;
    let tracker = mutation_tracker(&server.uri(), None);
    let err = tracker
        .create_issue(CreateIssueRequest {
            title: "x".into(),
            body: None,
            labels: vec![],
            assignees: vec![],
            parent: None,
            initial_state: None,
        })
        .await
        .unwrap_err();
    assert!(matches!(err, TrackerError::Misconfigured(_)), "{err:?}");
}

#[tokio::test]
async fn create_issue_rejects_multi_assignee_for_linear() {
    let server = MockServer::start().await;
    let tracker = mutation_tracker(&server.uri(), Some("team-1"));
    let err = tracker
        .create_issue(CreateIssueRequest {
            title: "x".into(),
            body: None,
            labels: vec![],
            assignees: vec!["u1".into(), "u2".into()],
            parent: None,
            initial_state: None,
        })
        .await
        .unwrap_err();
    assert!(matches!(err, TrackerError::Misconfigured(_)), "{err:?}");
}

#[tokio::test]
async fn create_issue_with_success_false_maps_to_transport() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "issueCreate": { "success": false, "issue": null } }
        })))
        .mount(&server)
        .await;
    let tracker = mutation_tracker(&server.uri(), Some("team-1"));
    let err = tracker
        .create_issue(CreateIssueRequest {
            title: "x".into(),
            body: None,
            labels: vec![],
            assignees: vec![],
            parent: None,
            initial_state: None,
        })
        .await
        .unwrap_err();
    assert!(matches!(err, TrackerError::Transport(_)), "{err:?}");
}

#[tokio::test]
async fn update_issue_echoes_observed_state_from_linear() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"operationName": "IssueUpdate"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "issueUpdate": {
                    "success": true,
                    "issue": { "id": "lin-1", "state": { "name": "In Review" } }
                }
            }
        })))
        .mount(&server)
        .await;
    let tracker = mutation_tracker(&server.uri(), None);
    let resp = tracker
        .update_issue(UpdateIssueRequest {
            id: IssueId::new("lin-1"),
            state: Some(IssueState::new("state-id-rev")),
            title: None,
            body: None,
            add_labels: vec![],
            remove_labels: vec![],
            add_assignees: vec![],
            remove_assignees: vec![],
        })
        .await
        .expect("update_issue");
    // Tracker-observed state wins so the orchestrator persists what
    // Linear actually accepted (workflow rules can rewrite transitions).
    assert_eq!(resp.state, Some(IssueState::new("In Review")));
}

#[tokio::test]
async fn update_issue_rejects_label_name_mutations() {
    let server = MockServer::start().await;
    let tracker = mutation_tracker(&server.uri(), None);
    let err = tracker
        .update_issue(UpdateIssueRequest {
            id: IssueId::new("lin-1"),
            state: None,
            title: None,
            body: None,
            add_labels: vec!["bug".into()],
            remove_labels: vec![],
            add_assignees: vec![],
            remove_assignees: vec![],
        })
        .await
        .unwrap_err();
    assert!(matches!(err, TrackerError::Misconfigured(_)), "{err:?}");
}

#[tokio::test]
async fn add_comment_round_trips_through_comment_create() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"operationName": "CommentCreate"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "commentCreate": {
                    "success": true,
                    "comment": {
                        "id": "comment-1",
                        "url": "https://linear.app/x/issue/ABC-1#comment-1"
                    }
                }
            }
        })))
        .mount(&server)
        .await;
    let tracker = mutation_tracker(&server.uri(), None);
    let resp = tracker
        .add_comment(AddCommentRequest {
            issue: IssueId::new("lin-1"),
            body: "looks good".into(),
            author_role: Some("qa".into()),
        })
        .await
        .expect("add_comment");
    assert_eq!(resp.id.as_deref(), Some("comment-1"));
}

#[tokio::test]
async fn add_blocker_creates_a_blocks_relation() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({
            "operationName": "IssueRelationCreate",
            // The blocker is the *source* issue, blocked is the related.
            "variables": { "input": { "type": "blocks", "issueId": "B", "relatedIssueId": "A" } }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "issueRelationCreate": {
                    "success": true,
                    "issueRelation": { "id": "rel-1" }
                }
            }
        })))
        .mount(&server)
        .await;
    let tracker = mutation_tracker(&server.uri(), None);
    let resp = tracker
        .add_blocker(AddBlockerRequest {
            blocked: IssueId::new("A"),
            blocker: IssueId::new("B"),
            reason: Some("regression".into()),
        })
        .await
        .expect("add_blocker");
    assert_eq!(resp.edge_id.as_deref(), Some("rel-1"));
}

#[tokio::test]
async fn link_parent_child_routes_through_issue_update_parent_id() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({
            "operationName": "IssueUpdate",
            "variables": { "id": "child-1", "input": { "parentId": "parent-1" } }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "issueUpdate": {
                    "success": true,
                    "issue": { "id": "child-1", "state": { "name": "Todo" } }
                }
            }
        })))
        .mount(&server)
        .await;
    let tracker = mutation_tracker(&server.uri(), None);
    let resp = tracker
        .link_parent_child(LinkParentChildRequest {
            parent: IssueId::new("parent-1"),
            child: IssueId::new("child-1"),
        })
        .await
        .expect("link_parent_child");
    // Linear stores the relation on the child's parentId field, not as a
    // discrete edge with its own id.
    assert!(resp.edge_id.is_none());
}

#[tokio::test]
async fn attach_artifact_creates_a_native_attachment() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(
            json!({"operationName": "AttachmentCreate"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "attachmentCreate": {
                    "success": true,
                    "attachment": { "id": "att-1" }
                }
            }
        })))
        .mount(&server)
        .await;
    let tracker = mutation_tracker(&server.uri(), None);
    let resp = tracker
        .attach_artifact(AttachArtifactRequest {
            issue: IssueId::new("lin-1"),
            kind: ArtifactKind::PullRequest,
            uri: "https://github.com/x/y/pull/9".into(),
            label: Some("PR #9".into()),
            note: None,
        })
        .await
        .expect("attach_artifact");
    assert_eq!(resp.id.as_deref(), Some("att-1"));
}
