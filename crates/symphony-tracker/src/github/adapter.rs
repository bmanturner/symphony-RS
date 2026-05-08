//! `GitHubTracker` — first slice of the GitHub Issues adapter.
//!
//! Implements [`IssueTracker::fetch_active`] against the REST endpoint
//! `GET /repos/{owner}/{repo}/issues?state=open` via `octocrab`. The
//! remaining trait methods return errors flagged as not-yet-implemented
//! so the orchestrator never silently dispatches against an
//! incomplete adapter; later checklist items (b/c/d) fill them in.
//!
//! ## Why `octocrab` and not raw `reqwest`
//!
//! [`crate::LinearTracker`] uses `reqwest` directly because Linear's
//! GraphQL surface is small enough that adding a wrapper would just be
//! noise. GitHub's REST surface is large and quirky (pagination headers,
//! conditional ETags, the issues endpoint also returning PRs); offloading
//! transport plumbing to `octocrab` keeps this adapter focused on the
//! state-derivation logic that actually differs between projects. The
//! crate-budget ADR for `octocrab` already justifies the dep.
//!
//! ## State derivation
//!
//! GitHub Issues only natively distinguishes `open` from `closed`. The
//! orchestrator wants richer states (`"todo"`, `"in progress"`, …) so we
//! project them out of labels: a label whose name starts with
//! [`GitHubConfig::status_label_prefix`] (default `"status:"`) becomes
//! the issue's state, with the prefix stripped. Issues with no matching
//! label fall back to the literal native state (`"open"` / `"closed"`).
//! The label that supplied the state is *not* re-emitted in
//! [`Issue::labels`] — keeping it would mean the orchestrator's label
//! template logic fires twice on the same string.
//!
//! ## Error mapping
//!
//! | Source                                  | Variant        |
//! |-----------------------------------------|----------------|
//! | `octocrab::Error::GitHub` 401 / 403     | `Unauthorized` |
//! | `octocrab::Error::GitHub` 5xx           | `Transport`    |
//! | `octocrab::Error::GitHub` other 4xx     | `Misconfigured`|
//! | `octocrab::Error::Hyper`/`Service`/etc. | `Transport`    |
//! | `octocrab::Error::Serde`/`Json`         | `Malformed`    |
//!
//! [`IssueTracker::fetch_active`]: crate::IssueTracker::fetch_active

use std::sync::Arc;

use async_trait::async_trait;
use http::Uri;
use octocrab::Octocrab;
use octocrab::models::IssueState as GhIssueState;
use octocrab::models::issues::Issue as GhIssue;
use octocrab::params::State as GhState;
use reqwest::StatusCode;
use secrecy::{ExposeSecret, SecretString};

use crate::IssueTracker;
use symphony_core::tracker::{Issue, IssueId, IssueState};
use symphony_core::tracker_trait::{TrackerError, TrackerResult};

/// Operator-supplied configuration for [`GitHubTracker`].
#[derive(Debug, Clone)]
pub struct GitHubConfig {
    /// Personal access token or fine-grained token. Wrapped in
    /// [`SecretString`] for the same reason [`crate::LinearConfig`] does:
    /// keep accidental log expansion from leaking it.
    pub token: SecretString,

    /// Repository owner (user or org login).
    pub owner: String,

    /// Repository name.
    pub repo: String,

    /// Operator-configured active states from `WORKFLOW.md`. Compared
    /// case-insensitively (SPEC §11.3) against the *derived* state — see
    /// the module doc for how derivation works.
    pub active_states: Vec<String>,

    /// Label prefix that signals "this label is a status marker". The
    /// default `"status:"` matches the convention used by GitHub
    /// projects that mirror Linear-style workflows. Comparison is
    /// case-insensitive ASCII.
    pub status_label_prefix: String,

    /// REST API base URI. Defaults to `https://api.github.com` via
    /// [`GitHubConfig::with_defaults`]; tests override to point at
    /// wiremock.
    pub base_uri: Uri,

    /// Pagination size. GitHub caps this at 100 (per_page).
    pub page_size: u8,
}

impl GitHubConfig {
    /// Build a config with the production base URI, the SPEC-default
    /// page size, and the conventional `"status:"` label prefix.
    pub fn with_defaults(
        token: SecretString,
        owner: impl Into<String>,
        repo: impl Into<String>,
        active_states: Vec<String>,
    ) -> Result<Self, TrackerError> {
        let base_uri: Uri = "https://api.github.com"
            .parse()
            .map_err(|e| TrackerError::Misconfigured(format!("invalid default base_uri: {e}")))?;
        Ok(Self {
            token,
            owner: owner.into(),
            repo: repo.into(),
            active_states,
            status_label_prefix: "status:".to_string(),
            base_uri,
            page_size: 50,
        })
    }
}

/// Production [`IssueTracker`] adapter targeting GitHub's REST API.
///
/// Cheap to clone — the underlying [`Octocrab`] handle is `Arc`-shared.
#[derive(Clone)]
pub struct GitHubTracker {
    client: Arc<Octocrab>,
    config: GitHubConfig,
}

impl std::fmt::Debug for GitHubTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Manual impl so a future field addition doesn't accidentally
        // start logging the token. Mirror of the [`LinearTracker`] impl.
        f.debug_struct("GitHubTracker")
            .field("base_uri", &self.config.base_uri.to_string())
            .field("owner", &self.config.owner)
            .field("repo", &self.config.repo)
            .field("active_states", &self.config.active_states)
            .field("status_label_prefix", &self.config.status_label_prefix)
            .field("page_size", &self.config.page_size)
            .finish()
    }
}

impl GitHubTracker {
    /// Build a tracker from a config. Constructs the underlying
    /// [`Octocrab`] handle with the personal token wired in.
    pub fn new(config: GitHubConfig) -> Result<Self, TrackerError> {
        let builder = Octocrab::builder()
            .base_uri(config.base_uri.clone())
            .map_err(|e| TrackerError::Misconfigured(format!("octocrab base_uri: {e}")))?
            .personal_token(config.token.expose_secret().to_string());
        let client = builder
            .build()
            .map_err(|e| TrackerError::Other(format!("octocrab build: {e}")))?;
        Ok(Self {
            client: Arc::new(client),
            config,
        })
    }
}

#[async_trait]
impl IssueTracker for GitHubTracker {
    async fn fetch_active(&self) -> TrackerResult<Vec<Issue>> {
        // Pull all open issues. GitHub's `/issues` endpoint also returns
        // pull requests (an "issue" in GitHub's data model is the
        // superset); we filter those out below since they are not
        // dispatch candidates.
        let first = self
            .client
            .issues(&self.config.owner, &self.config.repo)
            .list()
            .state(GhState::Open)
            .per_page(self.config.page_size)
            .send()
            .await
            .map_err(map_octocrab_error)?;
        let raw = self
            .client
            .all_pages(first)
            .await
            .map_err(map_octocrab_error)?;

        let allowed = lowercase_set(&self.config.active_states);
        let mut out = Vec::with_capacity(raw.len());
        for gh in raw {
            if gh.pull_request.is_some() {
                // PRs surface on the issues endpoint with a populated
                // `pull_request` link. Skip — they are not dispatch
                // candidates and a future PR-aware integration belongs
                // in its own subsystem.
                continue;
            }
            let issue = gh_to_issue(gh, &self.config.status_label_prefix);
            if allowed.contains(&issue.state.normalized()) {
                out.push(issue);
            }
        }
        Ok(out)
    }

    async fn fetch_state(&self, _ids: &[IssueId]) -> TrackerResult<Vec<Issue>> {
        // Lands in checklist item GitHubTracker (b). Returning a
        // structured error rather than `Ok(vec![])` is deliberate: a
        // silent empty would let the orchestrator's reconcile pass
        // believe every tracked issue had been deleted.
        Err(TrackerError::Other(
            "GitHubTracker::fetch_state not implemented yet (see CHECKLIST item GitHubTracker (b))"
                .into(),
        ))
    }

    async fn fetch_terminal_recent(
        &self,
        _terminal_states: &[IssueState],
    ) -> TrackerResult<Vec<Issue>> {
        Err(TrackerError::Other(
            "GitHubTracker::fetch_terminal_recent not implemented yet \
             (see CHECKLIST item GitHubTracker (b))"
                .into(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Pure helpers — kept private and free-standing so unit tests can hit them
// without spinning up an Octocrab client.
// ---------------------------------------------------------------------------

/// Build a lowercase-ASCII set from a `Vec<String>` of state names.
fn lowercase_set(states: &[String]) -> std::collections::HashSet<String> {
    states.iter().map(|s| s.to_ascii_lowercase()).collect()
}

/// Translate `octocrab`'s issue model into our normalized [`Issue`].
///
/// Keeps the mapping in one place so unit tests can exercise every
/// branch without HTTP. `branch_name` and `blocked_by` stay empty for
/// this slice — items (c) and (d) populate them.
fn gh_to_issue(gh: GhIssue, status_prefix: &str) -> Issue {
    // Note: `body` is the raw markdown body. We hand it through verbatim
    // so the orchestrator's prompt template can include the user's
    // exact text (and so item (c) can later regex over it for blocker
    // refs).
    let (state, labels) = derive_state(&gh, status_prefix);
    Issue {
        id: IssueId::new(gh.number.to_string()),
        identifier: format!("#{}", gh.number),
        title: gh.title,
        description: gh.body,
        // GitHub Issues has no native priority field. Per the
        // fabrication contract on `Issue`, we leave this `None`.
        priority: None,
        state,
        // Lands in (d) — derived from a linked PR's head ref.
        branch_name: None,
        url: Some(gh.html_url.to_string()),
        labels,
        // Lands in (c).
        blocked_by: Vec::new(),
        created_at: Some(gh.created_at.to_rfc3339()),
        updated_at: Some(gh.updated_at.to_rfc3339()),
    }
}

/// Pick the issue's state from the configured `status:` label, falling
/// back to native `open`/`closed`. Returns `(state, remaining_labels)`
/// — the status label itself is removed from the label list so the
/// orchestrator's label-driven rules don't double-fire on it.
fn derive_state(gh: &GhIssue, status_prefix: &str) -> (IssueState, Vec<String>) {
    let prefix_lc = status_prefix.to_ascii_lowercase();
    let mut derived: Option<String> = None;
    let mut others: Vec<String> = Vec::with_capacity(gh.labels.len());
    for label in &gh.labels {
        let name = &label.name;
        let name_lc = name.to_ascii_lowercase();
        if derived.is_none() && name_lc.starts_with(&prefix_lc) {
            // Strip the prefix, then trim any whitespace operators
            // routinely add around the colon ("status: in progress" →
            // "in progress").
            derived = Some(name[prefix_lc.len()..].trim().to_string());
        } else {
            // Lowercase per SPEC §11.3 (labels are stored normalized).
            others.push(name_lc);
        }
    }
    let state = derived.unwrap_or_else(|| match gh.state {
        GhIssueState::Open => "open".to_string(),
        GhIssueState::Closed => "closed".to_string(),
        _ => "open".to_string(),
    });
    (IssueState::new(state), others)
}

/// Translate `octocrab`'s rich error type into our coarse
/// [`TrackerError`] taxonomy. Pulled out so wire-level integration
/// tests can target each row of the table at the top of this module.
fn map_octocrab_error(err: octocrab::Error) -> TrackerError {
    match err {
        octocrab::Error::GitHub { source, .. } => {
            let status = source.status_code;
            let msg = source.message.clone();
            if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
                TrackerError::Unauthorized(format!("HTTP {status}: {msg}"))
            } else if status.is_server_error() {
                TrackerError::Transport(format!("HTTP {status}: {msg}"))
            } else {
                TrackerError::Misconfigured(format!("HTTP {status}: {msg}"))
            }
        }
        octocrab::Error::Hyper { source, .. } => {
            TrackerError::Transport(format!("hyper: {source}"))
        }
        octocrab::Error::Service { source, .. } => {
            TrackerError::Transport(format!("service: {source}"))
        }
        octocrab::Error::Http { source, .. } => TrackerError::Transport(format!("http: {source}")),
        octocrab::Error::Serde { source, .. } => {
            TrackerError::Malformed(format!("serde: {source}"))
        }
        octocrab::Error::Json { source, .. } => TrackerError::Malformed(format!("json: {source}")),
        other => TrackerError::Other(format!("octocrab: {other}")),
    }
}

#[cfg(test)]
mod tests {
    //! Pure-logic tests. Wire-level coverage lives in `tests/github.rs`.

    use super::*;

    #[test]
    fn lowercase_set_normalizes_inputs_to_ascii_lower() {
        let s = lowercase_set(&["Todo".into(), "IN PROGRESS".into(), "in progress".into()]);
        assert!(s.contains("todo"));
        assert!(s.contains("in progress"));
        assert_eq!(s.len(), 2, "duplicates after lowercasing must collapse");
    }

    #[test]
    fn gh_config_with_defaults_uses_production_base_uri_and_status_prefix() {
        let cfg = GitHubConfig::with_defaults(
            SecretString::from("ghp_secret".to_string()),
            "acme",
            "robot",
            vec!["todo".into()],
        )
        .unwrap();
        assert_eq!(cfg.base_uri.to_string(), "https://api.github.com/");
        assert_eq!(cfg.status_label_prefix, "status:");
        assert_eq!(cfg.page_size, 50);
    }

    #[tokio::test]
    async fn debug_impl_does_not_leak_token() {
        let cfg = GitHubConfig::with_defaults(
            SecretString::from("ghp_super_secret_token".to_string()),
            "acme",
            "robot",
            vec!["todo".into()],
        )
        .unwrap();
        let tracker = GitHubTracker::new(cfg).unwrap();
        let dbg = format!("{:?}", tracker);
        assert!(!dbg.contains("ghp_super_secret_token"));
        assert!(dbg.contains("acme"));
        assert!(dbg.contains("robot"));
    }
}
