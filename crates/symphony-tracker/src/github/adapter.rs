//! `GitHubTracker` — first slice of the GitHub Issues adapter.
//!
//! Implements [`TrackerRead::fetch_active`] against the REST endpoint
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
//! [`TrackerRead::fetch_active`]: crate::TrackerRead::fetch_active

use std::sync::Arc;

use async_trait::async_trait;
use http::Uri;
use octocrab::Octocrab;
use octocrab::models::Event as GhEvent;
use octocrab::models::IssueState as GhIssueState;
use octocrab::models::issues::Issue as GhIssue;
use octocrab::params::State as GhState;
use reqwest::StatusCode;
use secrecy::{ExposeSecret, SecretString};

use crate::TrackerRead;
use symphony_core::tracker::{BlockerRef, Issue, IssueId, IssueState};
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

/// Production [`TrackerRead`] adapter targeting GitHub's REST API.
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
    /// Walk an issue's timeline and recover the `head.ref` of the
    /// most-recently-opened linked pull request, if one exists.
    ///
    /// ## Why timeline + a second pulls call
    ///
    /// GitHub does not expose "linked PRs" as a structured field on the
    /// issue itself — the relationship is a side effect of someone
    /// posting a comment, commit message, or PR body that mentions the
    /// issue (`#42`, `closes #42`, etc.). Each such mention surfaces in
    /// `GET /repos/{owner}/{repo}/issues/{n}/timeline` as a
    /// `cross-referenced` event whose `source.issue` carries the
    /// referencing issue/PR.
    ///
    /// We filter to events whose `source.issue.pull_request.is_some()`
    /// — that distinguishes a referencing pull request from a referencing
    /// plain issue — and pick the most recently created one (its
    /// `created_at` is when the PR was opened, not when the cross-reference
    /// was made; for our purposes, "newest PR" is the right tiebreaker
    /// because operators almost always rebase work onto the latest branch).
    ///
    /// The cross-reference event's `source.issue` is the PR rendered as
    /// an issue, which intentionally does *not* carry `head.ref`. So we
    /// follow up with `GET /repos/{owner}/{repo}/pulls/{n}` to pull the
    /// branch name. One extra round trip per active issue with a linked
    /// PR is acceptable here — these issues are the orchestrator's hot
    /// path and the data is needed before it can dispatch a worker.
    ///
    /// ## Why fail-fast on transport errors
    ///
    /// "Branch name lookup failed" is operationally indistinguishable
    /// from "Linear is down": both invalidate this poll cycle. Bubbling
    /// the error keeps the orchestrator's reconcile loop honest — it
    /// will retry on the next tick rather than dispatching a worker
    /// against the wrong (or stale) branch.
    async fn derive_branch_name(&self, issue_number: u64) -> TrackerResult<Option<String>> {
        // Pull the timeline. Pagination matters here in principle (busy
        // issues can have hundreds of events), so we lean on `all_pages`
        // for parity with the issues listing path.
        let first = self
            .client
            .issues(&self.config.owner, &self.config.repo)
            .list_timeline_events(issue_number)
            .per_page(100u8)
            .send()
            .await
            .map_err(map_octocrab_error)?;
        let events = self
            .client
            .all_pages(first)
            .await
            .map_err(map_octocrab_error)?;

        // Walk events, retain only PR cross-references, key by the PR's
        // `created_at`. `max_by_key` returns the latest — `None` here
        // simply means "no linked PR yet" and `branch_name` stays None.
        let candidate = events
            .into_iter()
            .filter(|ev| matches!(ev.event, GhEvent::CrossReferenced))
            .filter_map(|ev| ev.source)
            .filter(|src| src.issue.pull_request.is_some())
            .map(|src| (src.issue.number, src.issue.created_at))
            .max_by_key(|(_, created_at)| *created_at)
            .map(|(n, _)| n);

        let Some(pr_number) = candidate else {
            return Ok(None);
        };

        let pr = self
            .client
            .pulls(&self.config.owner, &self.config.repo)
            .get(pr_number)
            .await
            .map_err(map_octocrab_error)?;
        Ok(Some(pr.head.ref_field))
    }

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
impl TrackerRead for GitHubTracker {
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
        // Active issues are dispatch candidates, so the orchestrator
        // wants `branch_name` populated when a linked PR exists. We do
        // this *after* state filtering so we never spend a timeline
        // round-trip on issues we'd just drop. See
        // [`Self::derive_branch_name`] for the lookup contract.
        for issue in &mut out {
            let n: u64 = issue.id.as_str().parse().map_err(|e| {
                TrackerError::Other(format!(
                    "internal: gh issue id should be numeric, got {:?}: {e}",
                    issue.id.as_str()
                ))
            })?;
            issue.branch_name = self.derive_branch_name(n).await?;
        }
        Ok(out)
    }

    async fn fetch_state(&self, ids: &[IssueId]) -> TrackerResult<Vec<Issue>> {
        // Per-id GET against `/repos/{owner}/{repo}/issues/{number}`.
        //
        // We deliberately preserve the caller's id ordering in the
        // returned vector — the orchestrator's reconcile pass walks
        // the result alongside its own ordered worker list, and the
        // conformance suite (`assert_fetch_state_preserves_caller_ordering`)
        // pins this contract for every adapter.
        //
        // No silent dropping: any 404 (or other failure) on a single id
        // surfaces as the whole call returning an error, matching the
        // trait contract on [`TrackerRead::fetch_state`]. Otherwise a
        // deleted issue would masquerade as still-running.
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            let number: u64 = id.as_str().parse().map_err(|e| {
                TrackerError::Misconfigured(format!(
                    "GitHub issue ids are positive integers; got {:?}: {e}",
                    id.as_str()
                ))
            })?;
            let gh = self
                .client
                .issues(&self.config.owner, &self.config.repo)
                .get(number)
                .await
                .map_err(map_octocrab_error)?;
            let mut issue = gh_to_issue(gh, &self.config.status_label_prefix);
            // `fetch_state` is the orchestrator's reconcile probe — it
            // wants the same enrichment shape as `fetch_active` so the
            // branch_name a worker is actively running against does not
            // suddenly appear `None` on a refresh.
            issue.branch_name = self.derive_branch_name(number).await?;
            out.push(issue);
        }
        Ok(out)
    }

    async fn fetch_terminal_recent(
        &self,
        terminal_states: &[IssueState],
    ) -> TrackerResult<Vec<Issue>> {
        // Empty filter short-circuits — matches the Linear adapter's
        // contract and avoids a wasted round trip when the operator
        // configured no terminal states.
        if terminal_states.is_empty() {
            return Ok(Vec::new());
        }
        // List closed issues (paginated). GitHub's native `closed`
        // state is the broadest signal; we then narrow to the operator's
        // configured terminal states by re-deriving each issue's state
        // from its labels (same rule as `fetch_active`). Issues with no
        // status label fall back to native `"closed"` — so an operator
        // who lists `"closed"` as terminal gets every closed issue,
        // while one who lists `"done"` only gets issues actually
        // labelled `status:done`.
        let first = self
            .client
            .issues(&self.config.owner, &self.config.repo)
            .list()
            .state(GhState::Closed)
            .per_page(self.config.page_size)
            .send()
            .await
            .map_err(map_octocrab_error)?;
        let raw = self
            .client
            .all_pages(first)
            .await
            .map_err(map_octocrab_error)?;

        let allowed: std::collections::HashSet<String> =
            terminal_states.iter().map(|s| s.normalized()).collect();
        let mut out = Vec::with_capacity(raw.len());
        for gh in raw {
            if gh.pull_request.is_some() {
                continue;
            }
            let issue = gh_to_issue(gh, &self.config.status_label_prefix);
            if allowed.contains(&issue.state.normalized()) {
                out.push(issue);
            }
        }
        Ok(out)
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
/// Pure / synchronous — keeps the mapping testable without HTTP.
/// `branch_name` lands here as `None`; the calling method
/// (`fetch_active` / `fetch_state`) is responsible for the timeline
/// follow-up that fills it in. See [`GitHubTracker::derive_branch_name`].
fn gh_to_issue(gh: GhIssue, status_prefix: &str) -> Issue {
    // Note: `body` is the raw markdown body. We hand it through verbatim
    // so the orchestrator's prompt template can include the user's
    // exact text, *and* feed a copy through `parse_blocked_by` first so
    // the orchestrator gets a structured view of any "blocked by #N" /
    // "depends on #N" refs without having to parse markdown itself.
    let blocked_by = parse_blocked_by(gh.body.as_deref());
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
        // Filled in by the caller via `derive_branch_name` after the
        // sync mapping returns — keeps the pure helper free of HTTP.
        branch_name: None,
        url: Some(gh.html_url.to_string()),
        labels,
        blocked_by,
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

/// Pull `blocked by #N` / `depends on #N` references out of a GitHub
/// issue body.
///
/// GitHub does not expose a structured "blocks" relation (Linear does);
/// the convention is to drop one of these phrases into the issue body and
/// let humans + bots scan for it. Symphony parses them eagerly so the
/// orchestrator can decide whether dispatch should wait on the listed
/// blockers, even though we cannot yet resolve `id`.
///
/// ## Contract — what stays `None`
///
/// Per [`BlockerRef`] and the `Issue` fabrication policy in SPEC §4.1.1,
/// body parsing only knows the human-readable issue *number*. Server-side
/// resolution of `id` and `state` lands in a later iteration; until then
/// both fields stay `None` (never invented). The conformance suite's
/// `assert_no_fabricated_optionals` enforces this.
///
/// ## What we match
///
/// * Case-insensitive `"blocked by"` and `"depends on"` triggers.
/// * Optional whitespace and a single optional `:` after the trigger.
/// * A literal `#`, then one or more ASCII digits.
/// * Duplicates (same number repeated across phrases) collapse — the
///   first occurrence wins.
///
/// ## What we deliberately do not match
///
/// * Comma-separated lists like `"blocked by #1, #2, #3"`. Each blocker
///   gets its own phrase or is missed; we'd rather drop a hint than
///   guess at "did they mean the second `#2` is also a blocker?".
/// * Cross-repo refs like `acme/robot#42`. The orchestrator only ever
///   acts within a single repo today.
fn parse_blocked_by(body: Option<&str>) -> Vec<BlockerRef> {
    let Some(body) = body else {
        return Vec::new();
    };
    // Lowercase mirror used only for trigger lookup; the original `body`
    // remains the source of truth for indexing so byte offsets stay in
    // step (both strings have identical byte layouts because
    // `to_ascii_lowercase` only swaps single-byte characters).
    let lc = body.to_ascii_lowercase();

    let triggers: &[&str] = &["blocked by", "depends on"];
    let mut seen: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
    let mut out: Vec<BlockerRef> = Vec::new();

    for trigger in triggers {
        let mut from = 0usize;
        while let Some(rel) = lc[from..].find(trigger) {
            let after = from + rel + trigger.len();
            // Walk past any whitespace or a single `:` joiner ("blocked
            // by: #42") before the `#`.
            let tail = &body[after..];
            let trimmed = tail.trim_start_matches(|c: char| c.is_whitespace() || c == ':');
            if let Some(after_hash) = trimmed.strip_prefix('#') {
                let digits: String = after_hash
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect();
                if let Ok(n) = digits.parse::<u64>()
                    && seen.insert(n)
                {
                    out.push(BlockerRef {
                        id: None,
                        identifier: Some(format!("#{n}")),
                        state: None,
                    });
                }
            }
            // Advance past this trigger occurrence — without this we'd
            // loop forever when the body contains the trigger phrase
            // without a following `#N`.
            from = after;
        }
    }
    out
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

    #[test]
    fn parse_blocked_by_returns_empty_for_none_or_no_triggers() {
        assert!(parse_blocked_by(None).is_empty());
        assert!(parse_blocked_by(Some("Just a regular description.")).is_empty());
        assert!(
            parse_blocked_by(Some("see #42 for context")).is_empty(),
            "bare `#N` without a trigger phrase must not be picked up — \
             those are usually cross-references, not blockers"
        );
    }

    #[test]
    fn parse_blocked_by_picks_up_blocked_by_and_depends_on_variants() {
        let body = "Blocked by #1.\nAlso depends on #2 because reasons.";
        let refs = parse_blocked_by(Some(body));
        let identifiers: Vec<_> = refs
            .iter()
            .filter_map(|b| b.identifier.as_deref())
            .collect();
        assert_eq!(identifiers, vec!["#1", "#2"]);
        for b in &refs {
            assert!(
                b.id.is_none(),
                "id must stay None until server-side resolution"
            );
            assert!(
                b.state.is_none(),
                "state must stay None — body parsing has no state hint"
            );
        }
    }

    #[test]
    fn parse_blocked_by_is_case_insensitive_and_handles_colon_joiner() {
        let body = "BLOCKED BY: #7\nDepends On  #8";
        let refs = parse_blocked_by(Some(body));
        let identifiers: Vec<_> = refs
            .iter()
            .filter_map(|b| b.identifier.as_deref())
            .collect();
        assert_eq!(identifiers, vec!["#7", "#8"]);
    }

    #[test]
    fn parse_blocked_by_dedupes_by_issue_number() {
        // Same blocker phrased twice should produce one entry, not two.
        let body = "Blocked by #99. We are also blocked by #99 in the meantime.";
        let refs = parse_blocked_by(Some(body));
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].identifier.as_deref(), Some("#99"));
    }

    #[test]
    fn parse_blocked_by_does_not_match_bare_text_after_trigger() {
        // The trigger must be followed by `#N`, not by prose. This
        // protects against bodies like "blocked by upstream rate limits".
        assert!(parse_blocked_by(Some("blocked by upstream limits")).is_empty());
        // And: digits without a `#` are not blockers — that would over-
        // match unrelated numbers in prose.
        assert!(parse_blocked_by(Some("depends on 42 things")).is_empty());
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
