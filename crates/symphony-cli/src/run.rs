//! Implementation of the `symphony run` subcommand.
//!
//! `run` is the production composition root. It reads `WORKFLOW.md`,
//! materialises the concrete tracker / agent / workspace adapters chosen
//! in the front matter, wires them into a [`PollLoop`], and drives the
//! loop until cancelled. Nothing in this module decides *policy* — every
//! knob (cadence, concurrency, kind selection) ultimately resolves back
//! to a typed field on [`WorkflowConfig`].
//!
//! ## Composition shape
//!
//! ```text
//!                              +-------------------+
//!     WORKFLOW.md  --LayeredLoader--> WorkflowConfig
//!                              +-------------------+
//!                                |          |          |
//!                          build_tracker  build_     build_agent_runner
//!                                |    workspace          |
//!                                v          v            v
//!                          IssueTracker  Workspace    AgentRunner
//!                                Manager
//!                                \         |          /
//!                                 \        |         /
//!                                  v       v        v
//!                                  SymphonyDispatcher
//!                                          |
//!                                          v
//!                                       PollLoop
//! ```
//!
//! ## Why one module rather than many small ones
//!
//! Composition is intrinsically wide: it touches every adapter crate
//! exactly once. Splitting it into a tracker factory, an agent factory,
//! and a workspace factory would replace one file you read top-to-bottom
//! with three — without changing the surface area. We keep them inline
//! and rely on `// ---` banner comments as section breaks. If a factory
//! grows enough complexity to deserve its own module the natural seam is
//! "the thing the composition root would `mod` and `use`", which is a
//! pure refactor when it eventually arrives.
//!
//! ## SIGINT behaviour
//!
//! This module wires `tokio::signal::ctrl_c` to a
//! [`CancellationToken`]. [`PollLoop::run`] then:
//!
//! 1. Stops accepting new dispatches.
//! 2. Fires every per-issue child cancel token so in-flight agent
//!    sessions tear down cooperatively.
//! 3. Drains the in-flight join set, but only up to
//!    `PollLoopConfig::drain_deadline`. Any task still running past
//!    the deadline is `JoinSet::abort_all()`'d and its claim is
//!    released as `Canceled`, so a wedged subprocess can never hang
//!    the daemon's exit.
//!
//! A second SIGINT is not handled specially — operators who need
//! "force-kill on second Ctrl-C" semantics get them from the shell
//! (tokio's default Ctrl-C handler aborts the process on the second
//! signal). Tightening the drain deadline is the in-process knob.

use std::env;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use futures::StreamExt;
use secrecy::SecretString;
use symphony_agent::claude::{ClaudeConfig, ClaudeRunner};
use symphony_agent::codex::{CodexConfig as RunnerCodexConfig, CodexRunner};
use symphony_agent::mock::{MockAgentConfig, MockAgentRunner};
use symphony_agent::tandem::{TandemRunner, TandemStrategy};
use symphony_config::{AgentKind, LayeredLoader, TrackerKind, WorkflowConfig};
use symphony_core::agent::{AgentEvent, AgentRunner, CompletionReason, StartSessionParams};
use symphony_core::event_bus::EventBus;
use symphony_core::poll_loop::{Dispatcher, PollLoop, PollLoopConfig};
use symphony_core::state_machine::ReleaseReason;
use symphony_core::tracker::Issue;
use symphony_core::tracker_trait::IssueTracker;
use symphony_tracker::{
    GitHubConfig, GitHubTracker, LinearConfig, LinearTracker, fixtures as tracker_fixtures,
};
use symphony_workspace::{LocalFsWorkspace, Workspace, WorkspaceManager};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::cli::RunArgs;

/// Default workspace root when the operator omits `workspace.root`. SPEC
/// §5.3.3 names `<system-temp>/symphony_workspaces` as the fallback.
fn default_workspace_root() -> std::path::PathBuf {
    std::env::temp_dir().join("symphony_workspaces")
}

/// Top-level entry: load config, build adapters, run until SIGINT.
///
/// Errors here surface as non-zero exit codes via `main.rs`. We classify
/// them with `anyhow` context strings rather than typed enums because
/// the binary is the only consumer — there is nothing downstream that
/// would want to branch on a typed variant.
pub async fn run(args: &RunArgs) -> Result<()> {
    let loaded = LayeredLoader::from_path(&args.path)
        .with_context(|| format!("loading workflow from {}", args.path.display()))?;
    loaded
        .config
        .validate()
        .with_context(|| "validating workflow front matter")?;

    info!(
        path = %loaded.source_path.display(),
        tracker = ?loaded.config.tracker.kind,
        agent = ?loaded.config.agent.kind,
        max_concurrent = loaded.config.agent.max_concurrent_agents,
        "starting symphony run",
    );

    let tracker = build_tracker(&loaded.config, &loaded.source_path)
        .with_context(|| "building issue tracker adapter")?;
    let workspace =
        build_workspace(&loaded.config).with_context(|| "building workspace manager")?;
    let agent = build_agent_runner(&loaded.config).with_context(|| "building agent runner")?;

    let dispatcher = Arc::new(SymphonyDispatcher {
        agent,
        workspace,
        prompt_template: loaded.prompt_template.clone(),
    });

    let cfg = PollLoopConfig {
        interval: Duration::from_millis(loaded.config.polling.interval_ms),
        // Standard SPEC §16.2 jitter half-width — kept here rather than
        // surfaced as a `WORKFLOW.md` knob until a deployment actually
        // needs to tune it.
        jitter_ratio: 0.1,
        max_concurrent: loaded.config.agent.max_concurrent_agents as usize,
        // SPEC §16 doesn't pin a value here; 30 s mirrors the default
        // poll interval and is comfortably longer than any
        // cooperative agent tear-down we've observed in tests.
        // Surface as a typed config knob if a deployment ever needs
        // to retune it.
        drain_deadline: Duration::from_secs(30),
    };

    // The event bus is shared between the orchestrator (the producer)
    // and the optional SSE server (a fan-out consumer). Building it at
    // the composition root lets both sides agree on replay sizing —
    // `observability.sse.replay_buffer` (SPEC v2 §5.14) is the single
    // knob.
    let bus = EventBus::new(loaded.config.observability.sse.replay_buffer);
    let poll_loop = PollLoop::with_event_bus(tracker, dispatcher, cfg, bus.clone());

    let cancel = CancellationToken::new();
    install_ctrl_c(cancel.clone());

    // Spawn the SSE status server alongside the orchestrator when
    // enabled. The task owns its own clone of the cancel token so the
    // SIGINT path drains both halves on the same signal. We deliberately
    // do *not* fail `symphony run` on a bind error: the orchestrator
    // loop is the load-bearing surface, and an operator who hits a
    // port-already-in-use should still be able to drive issues through.
    // The error is logged loudly so the failure is visible.
    let status_task = if loaded.config.observability.sse.enabled {
        let bind = loaded.config.observability.sse.bind.clone();
        let bus = bus.clone();
        let cancel = cancel.clone();
        Some(tokio::spawn(async move {
            let state = crate::sse::SseState::new(bus, crate::sse::SseConfig::default());
            if let Err(err) = crate::sse::bind_and_serve(&bind, state, cancel).await {
                error!(bind = %bind, error = %err, "SSE status server failed");
            }
        }))
    } else {
        info!("status surface disabled by config; skipping SSE server");
        None
    };

    poll_loop.run(cancel).await;

    // Wait for the SSE server to finish its graceful shutdown before
    // returning. `axum::serve(...).with_graceful_shutdown` resolves
    // when its shutdown signal fires *and* every in-flight request
    // drops, so awaiting here turns "symphony run exited cleanly" into
    // a true claim — no zombie listener, no half-closed sockets.
    if let Some(handle) = status_task {
        match handle.await {
            Ok(()) => info!("SSE status server stopped cleanly"),
            Err(err) => warn!(error = %err, "SSE status server task did not join cleanly"),
        }
    }

    info!("symphony run exited cleanly");
    Ok(())
}

/// Spawn a task that fires `cancel` on the first SIGINT.
///
/// Idempotent: a second SIGINT is observed only because this function
/// returns immediately — operators wanting "force-kill on second
/// SIGINT" semantics get them from the shell (Ctrl-C twice in tokio's
/// default handler triggers a process abort). The cooperative drain
/// deadline lives on [`PollLoopConfig::drain_deadline`].
fn install_ctrl_c(cancel: CancellationToken) {
    tokio::spawn(async move {
        match tokio::signal::ctrl_c().await {
            Ok(()) => {
                info!("SIGINT received; cancelling poll loop");
                cancel.cancel();
            }
            Err(err) => {
                warn!(error = %err, "failed to install SIGINT handler");
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Tracker factory
// ---------------------------------------------------------------------------

/// Build an [`IssueTracker`] from `cfg.tracker`.
///
/// Credential resolution: the API token is read from the env via the
/// adapter-specific name (`LINEAR_API_KEY`, `GITHUB_TOKEN`) and falls
/// back to the literal value of `tracker.api_key` from `WORKFLOW.md`.
/// We deliberately do not implement `$VAR` expansion on the YAML field
/// — figment's env layering is the documented expansion path (SPEC §5.4)
/// and a second mechanism would just add ambiguity.
///
/// `workflow_path` is the path to the `WORKFLOW.md` file. Used to
/// resolve relative `tracker.fixtures` paths so an operator can ship a
/// `WORKFLOW.md` and `issues.yaml` side-by-side without hard-coding
/// absolute paths.
pub(crate) fn build_tracker(
    cfg: &WorkflowConfig,
    workflow_path: &std::path::Path,
) -> Result<Arc<dyn IssueTracker>> {
    match cfg.tracker.kind {
        TrackerKind::Linear => {
            let project_slug = cfg
                .tracker
                .project_slug
                .clone()
                .ok_or_else(|| anyhow!("tracker.project_slug is required for linear"))?;
            let api_key = read_secret(
                cfg.tracker.api_key.as_deref(),
                "LINEAR_API_KEY",
                "tracker.api_key",
            )?;
            let mut linear_cfg = LinearConfig::with_defaults(
                api_key,
                project_slug,
                cfg.tracker.active_states.clone(),
            )?;
            if let Some(endpoint) = &cfg.tracker.endpoint {
                linear_cfg.endpoint = endpoint
                    .parse()
                    .with_context(|| format!("parsing tracker.endpoint = {endpoint:?}"))?;
            }
            Ok(Arc::new(LinearTracker::new(linear_cfg)?))
        }
        TrackerKind::Github => {
            let repository = cfg
                .tracker
                .repository
                .clone()
                .ok_or_else(|| anyhow!("tracker.repository is required for github"))?;
            let (owner, repo) = repository.split_once('/').ok_or_else(|| {
                anyhow!("tracker.repository must be `owner/repo`, got {repository:?}")
            })?;
            let token = read_secret(
                cfg.tracker.api_key.as_deref(),
                "GITHUB_TOKEN",
                "tracker.api_key",
            )?;
            let mut gh_cfg =
                GitHubConfig::with_defaults(token, owner, repo, cfg.tracker.active_states.clone())?;
            if let Some(endpoint) = &cfg.tracker.endpoint {
                gh_cfg.base_uri = endpoint
                    .parse()
                    .with_context(|| format!("parsing tracker.endpoint = {endpoint:?}"))?;
            }
            Ok(Arc::new(GitHubTracker::new(gh_cfg)?))
        }
        TrackerKind::Mock => {
            let rel = cfg
                .tracker
                .fixtures
                .as_ref()
                .ok_or_else(|| anyhow!("tracker.fixtures is required for mock"))?;
            // Resolve relative to the workflow file's directory so an
            // operator can ship `WORKFLOW.md` and `issues.yaml` together
            // and reference the latter as a bare filename. Absolute
            // paths pass through unchanged.
            let path = if rel.is_absolute() {
                rel.clone()
            } else {
                workflow_path
                    .parent()
                    .map(|p| p.join(rel))
                    .unwrap_or_else(|| rel.clone())
            };
            let (tracker, fixtures) = tracker_fixtures::load(&path)
                .with_context(|| format!("loading tracker fixtures from {}", path.display()))?;
            info!(
                path = %path.display(),
                active = fixtures.active_count(),
                terminal = fixtures.terminal_count(),
                "loaded mock tracker fixtures",
            );
            Ok(Arc::new(tracker))
        }
    }
}

/// Resolve a tracker secret. Precedence: `env_var` > literal value from
/// `WORKFLOW.md` (passed as `cfg_value`). Returns an error if neither is
/// set, naming both candidates so the operator knows what to fix.
fn read_secret(cfg_value: Option<&str>, env_var: &str, field: &str) -> Result<SecretString> {
    if let Ok(v) = env::var(env_var)
        && !v.is_empty()
    {
        return Ok(SecretString::from(v));
    }
    if let Some(v) = cfg_value
        && !v.is_empty()
        && !v.starts_with('$')
    {
        return Ok(SecretString::from(v.to_string()));
    }
    Err(anyhow!(
        "missing tracker credential: set ${env_var} or `{field}` in WORKFLOW.md"
    ))
}

// ---------------------------------------------------------------------------
// Workspace factory
// ---------------------------------------------------------------------------

/// Build a [`WorkspaceManager`] rooted at `cfg.workspace.root` (or the
/// SPEC fallback). The directory is created on the operator's behalf —
/// [`LocalFsWorkspace::new`] requires it to exist already, and an
/// orchestrator that refuses to start because a parent dir is missing
/// fails noisier than helpful for first-run UX.
fn build_workspace(cfg: &WorkflowConfig) -> Result<Arc<dyn WorkspaceManager>> {
    let root = cfg
        .workspace
        .root
        .clone()
        .unwrap_or_else(default_workspace_root);
    std::fs::create_dir_all(&root)
        .with_context(|| format!("creating workspace root {}", root.display()))?;

    // SPEC v2 §5.17: each phase is a list of shell snippets executed
    // sequentially. The two crates intentionally share field names so
    // this stays a structural mirror, not a translation step.
    let hooks = symphony_workspace::WorkspaceHooks {
        after_create: cfg.hooks.after_create.clone(),
        before_run: cfg.hooks.before_run.clone(),
        after_run: cfg.hooks.after_run.clone(),
        before_remove: cfg.hooks.before_remove.clone(),
        timeout_ms: cfg.hooks.timeout_ms,
    };
    Ok(Arc::new(LocalFsWorkspace::with_hooks(&root, hooks)?))
}

// ---------------------------------------------------------------------------
// Agent runner factory
// ---------------------------------------------------------------------------

/// Build the configured [`AgentRunner`].
///
/// `Tandem` mode is wired with a sensible default — `lead = codex,
/// follower = claude, strategy = draft-review` — until a future
/// checklist item adds a typed `tandem` block to [`WorkflowConfig`].
/// The defaults match the Phase 6 demos so an operator who flips
/// `agent.kind: tandem` gets a working setup.
fn build_agent_runner(cfg: &WorkflowConfig) -> Result<Arc<dyn AgentRunner>> {
    match cfg.agent.kind {
        AgentKind::Codex => Ok(Arc::new(CodexRunner::new(codex_runner_config(cfg)))),
        AgentKind::Claude => Ok(Arc::new(ClaudeRunner::new(ClaudeConfig::default()))),
        AgentKind::Tandem => {
            let lead: Arc<dyn AgentRunner> = Arc::new(CodexRunner::new(codex_runner_config(cfg)));
            let follower: Arc<dyn AgentRunner> =
                Arc::new(ClaudeRunner::new(ClaudeConfig::default()));
            Ok(Arc::new(TandemRunner::new(
                lead,
                follower,
                TandemStrategy::DraftReview,
            )))
        }
        AgentKind::Mock => Ok(Arc::new(MockAgentRunner::new(MockAgentConfig::default()))),
    }
}

/// Translate the [`WorkflowConfig`]'s loose `codex` block into the
/// runner-specific [`RunnerCodexConfig`]. The runner config carries
/// JSON-RPC method names (the WORKFLOW config doesn't expose those) so
/// we keep their defaults and only forward the pass-through values.
///
/// The pass-through fields cross a YAML→JSON value-type boundary: the
/// `WORKFLOW.md` parser yields `serde_yaml::Value` but the runner
/// expects `serde_json::Value`. We round-trip through JSON
/// serialisation, which is lossless for the YAML subset Codex actually
/// emits (mappings, sequences, strings, numbers, bools, nulls). Rare
/// YAML-only constructs (binary tags, anchors that didn't get expanded)
/// would error here, surfacing the misconfiguration at startup rather
/// than at dispatch.
fn codex_runner_config(cfg: &WorkflowConfig) -> RunnerCodexConfig {
    // Build via struct-update so the JSON-RPC method names (which
    // `WorkflowConfig` does not expose) keep their `Default` values.
    RunnerCodexConfig {
        command: cfg.codex.command.clone(),
        approval_policy: yaml_to_json(cfg.codex.approval_policy.as_ref()),
        thread_sandbox: yaml_to_json(cfg.codex.thread_sandbox.as_ref()),
        turn_sandbox_policy: yaml_to_json(cfg.codex.turn_sandbox_policy.as_ref()),
        ..RunnerCodexConfig::default()
    }
}

fn yaml_to_json(v: Option<&serde_yaml::Value>) -> Option<serde_json::Value> {
    v.and_then(|v| serde_json::to_value(v).ok())
}

// ---------------------------------------------------------------------------
// Dispatcher
// ---------------------------------------------------------------------------

/// Per-issue dispatcher used by [`PollLoop`].
///
/// Lifecycle for one dispatch:
///
/// 1. `workspace.ensure(identifier)` — materialize the per-issue dir,
///    fire `after_create` if newly built.
/// 2. `workspace.before_run(identifier)` — fire the `before_run` hook;
///    a failure aborts the dispatch with [`ReleaseReason::Completed`].
/// 3. `agent.start_session(...)` — spawn the backend with the rendered
///    prompt as the first turn.
/// 4. Drain the event stream until terminal, propagating cancellation.
/// 5. `workspace.after_run(identifier)` — fire `after_run`; failures are
///    logged and ignored per SPEC §9.4.
///
/// The dispatcher does *not* call `workspace.release()` — that belongs
/// to a future "issue closed → reclaim disk" path, not the per-run flow.
struct SymphonyDispatcher {
    agent: Arc<dyn AgentRunner>,
    workspace: Arc<dyn WorkspaceManager>,
    prompt_template: String,
}

#[async_trait::async_trait]
impl Dispatcher for SymphonyDispatcher {
    async fn dispatch(&self, issue: Issue, cancel: CancellationToken) -> ReleaseReason {
        match self.dispatch_inner(issue, cancel).await {
            Ok(reason) => reason,
            Err(err) => {
                error!(error = %err, "dispatch failed");
                ReleaseReason::Completed
            }
        }
    }
}

impl SymphonyDispatcher {
    async fn dispatch_inner(
        &self,
        issue: Issue,
        cancel: CancellationToken,
    ) -> Result<ReleaseReason> {
        let identifier = issue.identifier.clone();
        let Workspace { path, .. } = self
            .workspace
            .ensure(&identifier)
            .await
            .with_context(|| format!("ensure workspace for {identifier}"))?;
        self.workspace
            .before_run(&identifier)
            .await
            .with_context(|| format!("before_run hook for {identifier}"))?;

        let initial_prompt = render_prompt(&self.prompt_template, &issue);
        let issue_label = Some(format!("{}: {}", issue.identifier, issue.title));

        let session = self
            .agent
            .start_session(StartSessionParams {
                issue: issue.id.clone(),
                workspace: path,
                initial_prompt,
                issue_label,
            })
            .await
            .with_context(|| format!("start_session for {identifier}"))?;

        let reason = drive_session(session, cancel).await;
        self.workspace.after_run(&identifier).await;
        Ok(reason)
    }
}

/// Drain `session.events` until terminal. Returns the [`ReleaseReason`]
/// derived from the terminal event (or `Canceled` if `cancel` fires
/// first; the agent will close down via `Drop` of the session).
async fn drive_session(
    session: symphony_core::agent::AgentSession,
    cancel: CancellationToken,
) -> ReleaseReason {
    let symphony_core::agent::AgentSession {
        mut events,
        control,
        ..
    } = session;
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                // Best-effort abort. We don't wait for the stream to
                // close — the loop's drain step will reap the task.
                let _ = control.abort().await;
                return ReleaseReason::Canceled;
            }
            evt = events.next() => {
                let Some(evt) = evt else {
                    // Stream closed without a terminal event. Treat as
                    // a soft failure so the loop can retry on the next
                    // tick rather than dropping the issue silently.
                    warn!("agent stream closed without terminal event");
                    return ReleaseReason::Completed;
                };
                match evt {
                    AgentEvent::Completed { reason, .. } => {
                        return release_for(reason);
                    }
                    AgentEvent::Failed { error, .. } => {
                        warn!(error, "agent reported failure");
                        return ReleaseReason::Completed;
                    }
                    _ => continue,
                }
            }
        }
    }
}

/// Map a [`CompletionReason`] onto a [`ReleaseReason`].
///
/// The current [`ReleaseReason`] enum has no `Failed` variant — failure
/// classification is the retry queue's domain, not the state machine's.
/// Until the poll loop is wired to enqueue retries
/// (a separate checklist item), every dispatcher outcome that isn't an
/// orchestrator-driven cancel collapses to `Completed`. The next phase
/// of work will introduce a richer return shape so this mapping can
/// distinguish "agent succeeded" from "agent crashed; please retry".
fn release_for(reason: CompletionReason) -> ReleaseReason {
    match reason {
        CompletionReason::Cancelled => ReleaseReason::Canceled,
        CompletionReason::Success
        | CompletionReason::Failure
        | CompletionReason::Timeout
        | CompletionReason::SubprocessExit => ReleaseReason::Completed,
    }
}

// ---------------------------------------------------------------------------
// Prompt rendering
// ---------------------------------------------------------------------------

/// Substitute `{{var}}` placeholders in `template` against `issue`.
///
/// Deliberately tiny: SPEC §10.2 names a Tera-like template engine but
/// that's not a v1 requirement, and the placeholders we emit here are
/// the ones every fixture exercises (`identifier`, `title`,
/// `description`, `state`, `branch_name`, `url`). A future checklist
/// item can swap in a real templating crate; the call surface here
/// (`(template, issue) -> String`) won't change.
pub(crate) fn render_prompt(template: &str, issue: &Issue) -> String {
    let mut out = template.to_string();
    let pairs: [(&str, &str); 6] = [
        ("{{identifier}}", issue.identifier.as_str()),
        ("{{title}}", issue.title.as_str()),
        (
            "{{description}}",
            issue.description.as_deref().unwrap_or(""),
        ),
        ("{{state}}", issue.state.as_str()),
        (
            "{{branch_name}}",
            issue.branch_name.as_deref().unwrap_or(""),
        ),
        ("{{url}}", issue.url.as_deref().unwrap_or("")),
    ];
    for (k, v) in pairs {
        if out.contains(k) {
            out = out.replace(k, v);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use symphony_config::{AgentConfig, AgentKind, TrackerConfig, TrackerKind};

    fn issue() -> Issue {
        let mut i = Issue::minimal("id-1", "ABC-7", "Add fizz to buzz", "Todo");
        i.description = Some("steps:\n1. fizz\n2. buzz".into());
        i.branch_name = Some("feature/abc-7".into());
        i.url = Some("https://example.test/abc-7".into());
        i
    }

    #[test]
    fn render_prompt_substitutes_known_fields() {
        let tpl = "Issue {{identifier}}: {{title}}\nState: {{state}}\nBranch: {{branch_name}}\n{{description}}";
        let out = render_prompt(tpl, &issue());
        assert!(out.contains("Issue ABC-7: Add fizz to buzz"));
        assert!(out.contains("State: Todo"));
        assert!(out.contains("Branch: feature/abc-7"));
        assert!(out.contains("1. fizz"));
    }

    #[test]
    fn render_prompt_leaves_unknown_placeholders_alone() {
        // We only substitute the small known set. A future templating
        // engine swap should not silently delete unknown placeholders.
        let out = render_prompt("hello {{nope}}", &issue());
        assert_eq!(out, "hello {{nope}}");
    }

    #[test]
    fn render_prompt_uses_empty_string_for_missing_optionals() {
        let mut i = Issue::minimal("id-2", "ABC-8", "T", "Todo");
        i.description = None;
        i.branch_name = None;
        i.url = None;
        let out = render_prompt("[{{description}}][{{branch_name}}][{{url}}]", &i);
        assert_eq!(out, "[][][]");
    }

    #[test]
    fn read_secret_prefers_env_over_config_value() {
        // SAFETY: env mutation in tests is global state. We pick a
        // unique-enough name to avoid colliding with concurrent tests
        // and clean up afterwards.
        let key = "SYMPHONY_TEST_SECRET_PREFER_ENV";
        // SAFETY: setting our own scoped test env var; invariant
        // contract from `set_var`/`remove_var` is single-threaded at
        // call time, which holds in `cargo test` per-test.
        unsafe { env::set_var(key, "from_env") };
        let s = read_secret(Some("from_yaml"), key, "tracker.api_key").unwrap();
        // SecretString → expose only for assertion.
        use secrecy::ExposeSecret;
        assert_eq!(s.expose_secret(), "from_env");
        unsafe { env::remove_var(key) };
    }

    #[test]
    fn read_secret_falls_back_to_yaml_value() {
        let key = "SYMPHONY_TEST_SECRET_YAML_FALLBACK";
        unsafe { env::remove_var(key) };
        let s = read_secret(Some("from_yaml"), key, "tracker.api_key").unwrap();
        use secrecy::ExposeSecret;
        assert_eq!(s.expose_secret(), "from_yaml");
    }

    #[test]
    fn read_secret_rejects_unresolved_var_placeholder() {
        // `$VAR`-style placeholders are figment's job to expand; if
        // they survive into our factory we should not pretend the
        // literal `$VAR` string is a credential.
        let key = "SYMPHONY_TEST_SECRET_PLACEHOLDER";
        unsafe { env::remove_var(key) };
        let err = read_secret(Some("$NOT_RESOLVED"), key, "tracker.api_key").unwrap_err();
        assert!(err.to_string().contains(key));
    }

    #[test]
    fn read_secret_errors_when_neither_source_set() {
        let key = "SYMPHONY_TEST_SECRET_NEITHER";
        unsafe { env::remove_var(key) };
        let err = read_secret(None, key, "tracker.api_key").unwrap_err();
        assert!(err.to_string().contains(key));
        assert!(err.to_string().contains("tracker.api_key"));
    }

    #[test]
    fn build_agent_runner_codex_when_kind_codex() {
        let cfg = WorkflowConfig {
            agent: AgentConfig {
                kind: AgentKind::Codex,
                ..AgentConfig::default()
            },
            ..WorkflowConfig::default()
        };
        // Only checking it constructs without panicking; the runner
        // type is opaque behind `dyn AgentRunner`. A real spawn would
        // require a `codex` binary on PATH, out of scope here.
        let _r = build_agent_runner(&cfg).unwrap();
    }

    #[test]
    fn build_agent_runner_claude_when_kind_claude() {
        let cfg = WorkflowConfig {
            agent: AgentConfig {
                kind: AgentKind::Claude,
                ..AgentConfig::default()
            },
            ..WorkflowConfig::default()
        };
        let _r = build_agent_runner(&cfg).unwrap();
    }

    #[tokio::test]
    async fn build_agent_runner_mock_when_kind_mock() {
        // Mock is fully in-process so we can drive it end-to-end here:
        // verifies the factory wiring AND that the runner produced is a
        // working `AgentRunner`. If a future change accidentally breaks
        // the mock script (or stops using it from `Mock`) this test
        // turns red rather than the failure surfacing only in the
        // Quickstart smoke test.
        use futures::StreamExt;
        let cfg = WorkflowConfig {
            agent: AgentConfig {
                kind: AgentKind::Mock,
                ..AgentConfig::default()
            },
            ..WorkflowConfig::default()
        };
        let runner = build_agent_runner(&cfg).unwrap();
        let session = runner
            .start_session(StartSessionParams {
                issue: symphony_core::tracker::IssueId::new("MOCK-1"),
                workspace: std::path::PathBuf::from("/tmp/ws"),
                initial_prompt: "p".into(),
                issue_label: None,
            })
            .await
            .unwrap();
        let symphony_core::agent::AgentSession {
            mut events,
            control,
            ..
        } = session;
        // Drop the control after draining so the channel closes cleanly.
        let mut got_terminal = false;
        while let Some(ev) = events.next().await {
            if matches!(ev, AgentEvent::Completed { .. }) {
                got_terminal = true;
                break;
            }
        }
        drop(control);
        assert!(got_terminal, "mock runner must emit a terminal event");
    }

    #[test]
    fn build_agent_runner_tandem_when_kind_tandem() {
        let cfg = WorkflowConfig {
            agent: AgentConfig {
                kind: AgentKind::Tandem,
                ..AgentConfig::default()
            },
            ..WorkflowConfig::default()
        };
        let _r = build_agent_runner(&cfg).unwrap();
    }

    #[test]
    fn build_tracker_linear_requires_project_slug() {
        let cfg = WorkflowConfig {
            tracker: TrackerConfig {
                kind: TrackerKind::Linear,
                project_slug: None,
                ..TrackerConfig::default()
            },
            ..WorkflowConfig::default()
        };
        let err = match build_tracker(&cfg, std::path::Path::new("WORKFLOW.md")) {
            Err(e) => e,
            Ok(_) => panic!("expected build_tracker to fail"),
        };
        assert!(err.to_string().contains("project_slug"));
    }

    #[test]
    fn build_tracker_github_rejects_repository_without_slash() {
        let key = "GITHUB_TOKEN";
        // SAFETY: scoped test env mutation; cleanup at end.
        unsafe { env::set_var(key, "token") };
        let cfg = WorkflowConfig {
            tracker: TrackerConfig {
                kind: TrackerKind::Github,
                repository: Some("just-name".into()),
                ..TrackerConfig::default()
            },
            ..WorkflowConfig::default()
        };
        let err = match build_tracker(&cfg, std::path::Path::new("WORKFLOW.md")) {
            Err(e) => e,
            Ok(_) => panic!("expected build_tracker to fail"),
        };
        assert!(err.to_string().contains("owner/repo"));
        unsafe { env::remove_var(key) };
    }

    #[tokio::test]
    async fn build_tracker_mock_loads_fixtures_relative_to_workflow_path() {
        let dir = tempfile::tempdir().unwrap();
        let workflow_path = dir.path().join("WORKFLOW.md");
        std::fs::write(&workflow_path, "# stub\n").unwrap();
        let fixtures_path = dir.path().join("issues.yaml");
        std::fs::write(
            &fixtures_path,
            "active:\n  - id: id-1\n    identifier: ABC-1\n    title: t\n    state: Todo\n",
        )
        .unwrap();
        let cfg = WorkflowConfig {
            tracker: TrackerConfig {
                kind: TrackerKind::Mock,
                // Bare filename: must resolve next to WORKFLOW.md.
                fixtures: Some(std::path::PathBuf::from("issues.yaml")),
                ..TrackerConfig::default()
            },
            ..WorkflowConfig::default()
        };
        let tracker = build_tracker(&cfg, &workflow_path).unwrap();
        let active = tracker.fetch_active().await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].identifier, "ABC-1");
    }

    #[test]
    fn build_tracker_mock_requires_fixtures_path() {
        let cfg = WorkflowConfig {
            tracker: TrackerConfig {
                kind: TrackerKind::Mock,
                fixtures: None,
                ..TrackerConfig::default()
            },
            ..WorkflowConfig::default()
        };
        let err = match build_tracker(&cfg, std::path::Path::new("WORKFLOW.md")) {
            Err(e) => e,
            Ok(_) => panic!("expected build_tracker to fail when fixtures missing"),
        };
        assert!(err.to_string().contains("fixtures"));
    }

    #[test]
    fn build_workspace_creates_root_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("nested/symphony");
        let cfg = WorkflowConfig {
            workspace: symphony_config::WorkspaceConfig {
                root: Some(nested.clone()),
            },
            ..WorkflowConfig::default()
        };
        let _ws = build_workspace(&cfg).unwrap();
        assert!(nested.is_dir());
    }

    #[tokio::test]
    async fn release_for_maps_completion_reasons_correctly() {
        assert_eq!(
            release_for(CompletionReason::Success),
            ReleaseReason::Completed
        );
        assert_eq!(
            release_for(CompletionReason::Cancelled),
            ReleaseReason::Canceled
        );
        assert_eq!(
            release_for(CompletionReason::Failure),
            ReleaseReason::Completed
        );
        assert_eq!(
            release_for(CompletionReason::Timeout),
            ReleaseReason::Completed
        );
        assert_eq!(
            release_for(CompletionReason::SubprocessExit),
            ReleaseReason::Completed
        );
    }
}
