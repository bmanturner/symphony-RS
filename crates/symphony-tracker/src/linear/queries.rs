//! Typed Linear GraphQL bindings.
//!
//! The three `#[derive(GraphQLQuery)]` blocks below run at compile time
//! (via `graphql_client_codegen`) against `schema.graphql` and
//! `queries.graphql` in `crates/symphony-tracker/graphql/linear/`. Each
//! derive expands into:
//!
//!   * the marker struct itself (e.g. [`CandidateIssues`]) which
//!     implements `graphql_client::GraphQLQuery`,
//!   * a `Variables` struct (re-exported with a flatter name below),
//!   * a `ResponseData` struct describing the typed response payload.
//!
//! The adapter wraps these in convenience methods that handle pagination
//! and error mapping; this module is intentionally just the type surface.
//!
//! ## Custom scalars
//!
//! Linear's `DateTime` scalar is represented as `String` here. SPEC §11.3
//! stores ISO-8601 timestamps as raw strings (we never compare them for
//! ordering), so adding a `chrono`/`time` dependency just to round-trip
//! strings would be wasted weight. graphql_client looks up the scalar by
//! name in the surrounding scope, so the type alias below is what wires
//! it in.

use graphql_client::GraphQLQuery;

/// Linear's `DateTime` scalar mapped onto a Rust `String`.
///
/// graphql_client resolves custom scalars by name within the module that
/// contains the `#[derive(GraphQLQuery)]`, so this alias must live here
/// (not behind a re-export) for the codegen to find it.
pub type DateTime = String;

/// `fetch_candidate_issues` — paginated active-issue dispatch list.
///
/// Variables: `projectSlug`, `stateNames`, `first`, `after?`.
/// Response: `issues.nodes` (each with state, labels, inverseRelations)
/// plus `issues.pageInfo` so the adapter can drive Relay-style pagination.
///
/// The adapter calls this on every poll tick (SPEC §16.3) and is expected
/// to walk all pages — Linear caps `first` at 250 but SPEC §11.2 sets the
/// default page size to 50.
#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "graphql/linear/schema.graphql",
    query_path = "graphql/linear/queries.graphql",
    response_derives = "Debug, Clone, PartialEq",
    variables_derives = "Debug, Clone"
)]
pub struct CandidateIssues;

/// `fetch_issues_by_states` — same shape as [`CandidateIssues`] but used
/// for the startup *terminal cleanup* sweep (SPEC §11.1 op 2). The query
/// body is identical because the only thing that changes is which set of
/// state names the orchestrator passes in `$stateNames`.
///
/// We keep it as a separate operation (rather than reusing
/// [`CandidateIssues`]) so the two call sites are textually distinct in
/// logs and traces — operators should be able to tell from a single log
/// line which sweep emitted a request.
#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "graphql/linear/schema.graphql",
    query_path = "graphql/linear/queries.graphql",
    response_derives = "Debug, Clone, PartialEq",
    variables_derives = "Debug, Clone"
)]
pub struct IssuesByStates;

/// `fetch_issue_states_by_ids` — minimal query that returns only the
/// fields the reconcile pass needs (id, identifier, state name).
///
/// SPEC §16.3 explicitly allows this operation to fail without aborting
/// the orchestrator; the adapter still surfaces a [`crate::TrackerError`]
/// so the poll loop can log and move on.
#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "graphql/linear/schema.graphql",
    query_path = "graphql/linear/queries.graphql",
    response_derives = "Debug, Clone, PartialEq",
    variables_derives = "Debug, Clone"
)]
pub struct IssueStatesByIds;

// graphql_client generates `<operation>::Variables` inside a module named
// after the operation in snake_case. Re-export those with shorter names
// so the eventual adapter (and tests below) don't leak the generated
// module path. Each `Variables` type is the typed argument bag we pass
// to `graphql_client::reqwest::post_graphql`.
pub use candidate_issues::Variables as CandidateIssuesVariables;
pub use issue_states_by_ids::Variables as IssueStatesByIdsVariables;
pub use issues_by_states::Variables as IssuesByStatesVariables;

#[cfg(test)]
mod tests {
    //! These tests exist to catch schema/query drift at *compile* time
    //! (the derives above) and to verify the generated `QueryBody` shape
    //! at *runtime*. graphql_client generates a `build_query(variables)`
    //! associated function on each marker struct that returns the JSON
    //! payload sent over the wire — assertions on that payload's
    //! `query` and `variables` fields are the cheapest way to confirm
    //! we're sending what SPEC §11.2 says we should.
    //!
    //! No HTTP is involved here; the adapter integration tests in
    //! Phase 2's next checklist item exercise wire behavior with
    //! `wiremock`.

    use super::*;
    use graphql_client::GraphQLQuery;

    #[test]
    fn candidate_issues_query_targets_the_right_filter_shape() {
        let body = CandidateIssues::build_query(CandidateIssuesVariables {
            project_slug: "alpha".into(),
            state_names: vec!["Todo".into(), "In Progress".into()],
            first: 50,
            after: None,
        });

        // Operation name plumbed through verbatim so server-side logs
        // (and our own traces) show "CandidateIssues" rather than an
        // anonymous query.
        assert_eq!(body.operation_name, "CandidateIssues");
        // The query text must mention the SPEC §11.2 filter shape so a
        // copy-paste of the query into Linear's API explorer still
        // works. Asserting substrings keeps the test resilient to
        // whitespace changes from graphql_client's prettifier.
        assert!(
            body.query
                .contains("project: { slugId: { eq: $projectSlug } }")
        );
        assert!(body.query.contains("state: { name: { in: $stateNames } }"));
        assert!(body.query.contains("pageInfo"));
        assert!(body.query.contains("inverseRelations"));

        // Variables serialize through serde, so this also exercises that
        // the generated `Variables` struct field names match the GraphQL
        // variable names (camelCase on the wire).
        let json = serde_json::to_value(&body.variables).unwrap();
        assert_eq!(json["projectSlug"], "alpha");
        assert_eq!(
            json["stateNames"],
            serde_json::json!(["Todo", "In Progress"])
        );
        assert_eq!(json["first"], 50);
        // `after: None` must serialize as JSON `null` — Linear treats a
        // missing key as "first page" too, but we want the wire shape
        // to be deterministic.
        assert_eq!(json["after"], serde_json::Value::Null);
    }

    #[test]
    fn issues_by_states_query_is_isolated_from_candidate_issues() {
        // Same shape, distinct operation name — the two call sites must
        // be distinguishable in transport logs.
        let body = IssuesByStates::build_query(IssuesByStatesVariables {
            project_slug: "alpha".into(),
            state_names: vec!["Done".into()],
            first: 100,
            after: Some("cursor-1".into()),
        });
        assert_eq!(body.operation_name, "IssuesByStates");
        assert!(body.query.contains("query IssuesByStates"));

        let json = serde_json::to_value(&body.variables).unwrap();
        assert_eq!(json["after"], "cursor-1");
    }

    #[test]
    fn issue_states_by_ids_uses_the_id_in_filter() {
        let body = IssueStatesByIds::build_query(IssueStatesByIdsVariables {
            ids: vec!["lin-1".into(), "lin-2".into()],
        });
        assert_eq!(body.operation_name, "IssueStatesByIds");
        // SPEC §11.2: "Issue-state refresh query uses GraphQL issue IDs
        // with variable type `[ID!]`". Confirm the filter shape.
        assert!(body.query.contains("filter: { id: { in: $ids } }"));
        assert!(body.query.contains("$ids: [ID!]!"));

        let json = serde_json::to_value(&body.variables).unwrap();
        assert_eq!(json["ids"], serde_json::json!(["lin-1", "lin-2"]));
    }

    #[test]
    fn candidate_issues_response_decodes_a_realistic_linear_payload() {
        // A trimmed-down but realistic response shape, hand-crafted from
        // Linear's documented schema. This test catches drift between
        // schema.graphql and the actual wire format — if Linear ever
        // changes a field type we'd see a serde decode failure here long
        // before we run an integration test.
        let raw = serde_json::json!({
            "issues": {
                "pageInfo": { "hasNextPage": false, "endCursor": null },
                "nodes": [{
                    "id": "lin-1",
                    "identifier": "ABC-1",
                    "title": "Fix the thing",
                    "description": "details",
                    "priority": 2.0,
                    "branchName": "abc-1",
                    "url": "https://linear.app/x/issue/ABC-1",
                    "createdAt": "2024-01-02T03:04:05.000Z",
                    "updatedAt": "2024-01-02T03:04:06.000Z",
                    "state": { "id": "state-1", "name": "In Progress" },
                    "labels": { "nodes": [{ "name": "bug" }] },
                    "inverseRelations": {
                        "nodes": [{
                            "type": "blocks",
                            "issue": {
                                "id": "lin-2",
                                "identifier": "ABC-2",
                                "state": { "name": "Todo" }
                            }
                        }]
                    }
                }]
            }
        });

        let decoded: candidate_issues::ResponseData =
            serde_json::from_value(raw).expect("schema must accept canonical payload");
        assert_eq!(decoded.issues.nodes.len(), 1);
        let node = &decoded.issues.nodes[0];
        assert_eq!(node.identifier, "ABC-1");
        assert_eq!(node.state.name, "In Progress");
        assert_eq!(node.priority, Some(2.0));
        // Inverse-relation passthrough: the adapter filters to
        // type=="blocks" during normalization, so we surface the raw
        // string here untouched.
        assert_eq!(node.inverse_relations.nodes[0].type_, "blocks");
    }

    #[test]
    fn issue_states_by_ids_response_decodes_minimal_payload() {
        // Reconcile responses are tiny on purpose — only state.name is
        // read by the orchestrator. Confirm the minimal shape works.
        let raw = serde_json::json!({
            "issues": {
                "nodes": [
                    { "id": "lin-1", "identifier": "ABC-1", "state": { "name": "Done" } },
                    { "id": "lin-2", "identifier": "ABC-2", "state": { "name": "Cancelled" } }
                ]
            }
        });
        let decoded: issue_states_by_ids::ResponseData =
            serde_json::from_value(raw).expect("schema must accept reconcile payload");
        assert_eq!(decoded.issues.nodes.len(), 2);
        assert_eq!(decoded.issues.nodes[1].state.name, "Cancelled");
    }
}
