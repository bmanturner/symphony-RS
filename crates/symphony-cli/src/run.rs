//! Implementation of the `symphony run` subcommand.
//!
//! `run` is the production composition root. It reads `WORKFLOW.md`,
//! materialises the concrete tracker / agent / workspace adapters chosen
//! in the front matter, hands them to [`crate::scheduler::build_scheduler_v2`],
//! and drives the resulting [`symphony_core::SchedulerV2`] alongside its dispatch
//! runners until cancelled. Nothing in this module decides *policy* —
//! every knob (cadence, concurrency, kind selection) ultimately resolves
//! back to a typed field on [`WorkflowConfig`].
//!
//! ## Composition shape
//!
//! ```text
//!     WORKFLOW.md --LayeredLoader--> WorkflowConfig
//!                                            |
//!         +--------------+--------+----------+--------+
//!         |              |        |          |        |
//!   build_tracker  build_workspace  build_agent_runner  (cfg passthrough)
//!         |              |        |          |
//!         v              v        v          v
//!   TrackerRead   WorkspaceManager  SymphonyDispatcher
//!         \              \         /
//!          \              v       v
//!           +-----> build_scheduler_v2 (intake/recovery/specialist/integration/qa/followup/budget)
//!                              |
//!                              v
//!                       SchedulerV2 + dispatch runners
//! ```
//!
//! [`symphony_core::PollLoop`] is no longer the production entry point as of the v2
//! checklist switch; it remains exported by `symphony-core` for parity
//! tests that compare the multi-queue scheduler against the flat-loop
//! contract. New production code paths must go through
//! [`crate::scheduler::build_scheduler_v2`].
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
//! [`CancellationToken`] shared by the scheduler and the dispatch
//! runners' driver task:
//!
//! 1. Cancellation halts [`symphony_core::SchedulerV2::run`] before its next tick — no
//!    fresh dispatch requests are enqueued onto any logical queue.
//! 2. The runner driver (see [`run_runner_loop`]) observes the same
//!    token, exits its tick loop, and switches into a drain phase. It
//!    polls each runner's [`SpecialistRunner::reap_completed`] surface
//!    (and the analogous methods on the integration / QA / follow-up
//!    approval / budget-pause runners) until every `inflight_count`
//!    reaches zero.
//! 3. Each spawned dispatcher task already received a child cancel
//!    token from `run_pending`, so the SIGINT signal propagates through
//!    to in-flight agent sessions, mirroring the flat poll loop's
//!    cooperative tear-down. The drain phase is bounded by
//!    [`DRAIN_DEADLINE`]; tasks still running past the deadline are
//!    abandoned to process exit (the runners' `reap_completed` surface
//!    does not yet expose a forcible abort, matching the v2 checklist's
//!    "migrate SIGINT semantics to the runners' reap surface" scope).
//!
//! A second SIGINT is not handled specially — operators who need
//! "force-kill on second Ctrl-C" semantics get them from the shell
//! (tokio's default Ctrl-C handler aborts the process on the second
//! signal). Tightening [`DRAIN_DEADLINE`] is the in-process knob.

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
use symphony_core::poll_loop::Dispatcher;
use symphony_core::state_machine::ReleaseReason;
use symphony_core::tracker::Issue;
use symphony_core::tracker_trait::TrackerRead;
use symphony_core::{
    BudgetPauseRunner, FollowupApprovalRunner, IntegrationDispatchRunner, QaDispatchRunner,
    SpecialistRunner,
};
use symphony_tracker::{
    GitHubConfig, GitHubTracker, LinearConfig, LinearTracker, fixtures as tracker_fixtures,
};
use symphony_workspace::{LocalFsWorkspace, WorkspaceClaim, WorkspaceManager};
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

    let dispatcher: Arc<dyn Dispatcher> = Arc::new(SymphonyDispatcher {
        agent,
        workspace,
        prompt_template: loaded.prompt_template.clone(),
    });

    // Compose the v2 multi-queue scheduler. Until durable state-backed
    // sources/dispatchers for integration / QA / follow-up approval /
    // budget pause land, the composition root supplies the empty
    // production stand-ins published by `crate::scheduler` — each
    // surfaces zero candidates so the corresponding tick is a no-op.
    let bundle = crate::scheduler::build_scheduler_v2(
        &loaded.config,
        tracker,
        dispatcher,
        crate::scheduler::empty_integration_source(),
        crate::scheduler::noop_integration_dispatcher(),
        crate::scheduler::empty_qa_source(),
        crate::scheduler::noop_qa_dispatcher(),
        crate::scheduler::empty_followup_approval_source(),
        crate::scheduler::noop_followup_approval_dispatcher(),
        crate::scheduler::empty_budget_pause_source(),
        crate::scheduler::noop_budget_pause_dispatcher(),
    );

    // The event bus is shared between the orchestrator (the producer)
    // and the optional SSE server (a fan-out consumer). Building it at
    // the composition root lets both sides agree on replay sizing —
    // `observability.sse.replay_buffer` (SPEC v2 §5.14) is the single
    // knob. SchedulerV2 does not yet emit into the bus; the durable
    // event-tail SSE switch is a later checklist item.
    let bus = EventBus::new(loaded.config.observability.sse.replay_buffer);

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

    // Drive the dispatch runners on their own task so the scheduler
    // tick fan and the dispatcher fan run concurrently. The runner
    // task owns its own clone of the cancel token; on SIGINT it drops
    // out of its tick loop and switches into the bounded-deadline drain
    // phase described in the module docstring.
    let runner_interval = Duration::from_millis(loaded.config.polling.interval_ms);
    let specialist_runner = bundle.specialist_runner.clone();
    let integration_runner = bundle.integration_runner.clone();
    let qa_runner = bundle.qa_runner.clone();
    let followup_runner = bundle.followup_approval_runner.clone();
    let budget_runner = bundle.budget_pause_runner.clone();
    let runner_task = {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            run_runner_loop(
                cancel,
                specialist_runner,
                integration_runner,
                qa_runner,
                followup_runner,
                budget_runner,
                runner_interval,
                DRAIN_DEADLINE,
            )
            .await;
        })
    };

    // Drive the scheduler under the operator-facing cancel token. This
    // returns once the cancel token fires — see SchedulerV2::run for
    // the cancellation contract.
    bundle.scheduler.run(cancel.clone()).await;

    // Wait for the runner driver task to finish its drain phase before
    // returning. The runner driver bounds itself on DRAIN_DEADLINE so
    // this awaits at most that long beyond scheduler shutdown.
    match runner_task.await {
        Ok(()) => info!("dispatch runner driver stopped cleanly"),
        Err(err) => warn!(error = %err, "dispatch runner driver task did not join cleanly"),
    }

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

/// Drain deadline for the dispatch runner driver after SIGINT.
///
/// Mirrors the flat poll loop's prior `drain_deadline` of 30 s; long
/// enough for cooperative agent tear-down, short enough that a wedged
/// subprocess cannot hang the daemon's exit. Surface as a typed config
/// knob if a deployment ever needs to retune it.
const DRAIN_DEADLINE: Duration = Duration::from_secs(30);

/// Drive the v2 dispatch runners on a fixed cadence until `cancel`
/// fires, then enter a bounded drain phase.
///
/// Each tick walks the five runners in registration order, calling
/// `run_pending` (which non-blockingly drains its dispatch queue and
/// spawns dispatcher tasks under bounded concurrency) and then
/// `reap_completed` (which non-blockingly collects any joined tasks).
/// On cancellation the loop stops accepting fresh work and switches
/// into a drain phase that polls `inflight_count` across all runners
/// until every count is zero or `drain_deadline` elapses.
///
/// In production the five runners are wired by
/// [`crate::scheduler::build_scheduler_v2`]; the function takes them
/// individually as `Arc<*Runner>` so this driver stays unit-testable
/// against fake runners.
#[allow(clippy::too_many_arguments)]
async fn run_runner_loop(
    cancel: CancellationToken,
    specialist: Arc<SpecialistRunner>,
    integration: Arc<IntegrationDispatchRunner>,
    qa: Arc<QaDispatchRunner>,
    followup: Arc<FollowupApprovalRunner>,
    budget: Arc<BudgetPauseRunner>,
    interval: Duration,
    drain_deadline: Duration,
) {
    info!(
        interval_ms = interval.as_millis() as u64,
        drain_ms = drain_deadline.as_millis() as u64,
        "dispatch runner driver starting",
    );
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(interval) => {}
        }
        // Spawn pass: each runner owns its own bounded concurrency and
        // deferred queue, so capacity-shedding and parking are local to
        // the runner. The driver only orchestrates ordering.
        let _ = specialist.run_pending(cancel.clone()).await;
        let _ = integration.run_pending(cancel.clone()).await;
        let _ = qa.run_pending(cancel.clone()).await;
        let _ = followup.run_pending(cancel.clone()).await;
        let _ = budget.run_pending(cancel.clone()).await;
        // Reap pass: non-blocking — `try_join_next` under the hood, so
        // long-running tasks stay in flight without holding the driver.
        let _ = specialist.reap_completed().await;
        let _ = integration.reap_completed().await;
        let _ = qa.reap_completed().await;
        let _ = followup.reap_completed().await;
        let _ = budget.reap_completed().await;
    }

    // Drain phase: cancel has fired, child cancel tokens are already
    // propagating into in-flight dispatcher tasks. Poll the runners'
    // inflight counts until they all hit zero or the deadline elapses.
    info!("dispatch runner driver entering drain phase");
    let drain = async {
        loop {
            // Reap whatever finished cooperatively first, otherwise the
            // inflight count would never hit zero.
            let _ = specialist.reap_completed().await;
            let _ = integration.reap_completed().await;
            let _ = qa.reap_completed().await;
            let _ = followup.reap_completed().await;
            let _ = budget.reap_completed().await;
            let inflight = specialist.inflight_count().await
                + integration.inflight_count().await
                + qa.inflight_count().await
                + followup.inflight_count().await
                + budget.inflight_count().await;
            if inflight == 0 {
                return;
            }
            // 50 ms is a balance between responsiveness (we exit as soon
            // as the last task drains) and CPU load on the orchestrator
            // process during a deliberate shutdown.
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    match tokio::time::timeout(drain_deadline, drain).await {
        Ok(()) => info!("dispatch runner driver drained cleanly"),
        Err(_) => warn!(
            deadline_ms = drain_deadline.as_millis() as u64,
            "dispatch runner driver drain deadline exceeded; abandoning in-flight tasks to process exit",
        ),
    }
}

/// Spawn a task that fires `cancel` on the first SIGINT.
///
/// Idempotent: a second SIGINT is observed only because this function
/// returns immediately — operators wanting "force-kill on second
/// SIGINT" semantics get them from the shell (Ctrl-C twice in tokio's
/// default handler triggers a process abort). The cooperative drain
/// deadline lives on [`DRAIN_DEADLINE`].
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

/// Build an [`TrackerRead`] from `cfg.tracker`.
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
) -> Result<Arc<dyn TrackerRead>> {
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

/// Per-issue dispatcher used by the v2 [`SpecialistRunner`].
///
/// Lifecycle for one dispatch:
///
/// 1. `workspace.claim(identifier)` — materialize the per-issue dir
///    and return a [`WorkspaceClaim`] (path + strategy + verification);
///    fires `after_create` if newly built.
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
        let WorkspaceClaim { path, .. } = self
            .workspace
            .claim(&identifier)
            .await
            .with_context(|| format!("claim workspace for {identifier}"))?;
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
            workspace: symphony_config::WorkspacePolicyConfig {
                root: Some(nested.clone()),
                ..symphony_config::WorkspacePolicyConfig::default()
            },
            ..WorkflowConfig::default()
        };
        let _ws = build_workspace(&cfg).unwrap();
        assert!(nested.is_dir());
    }

    #[tokio::test]
    async fn run_runner_loop_exits_promptly_on_cancel_when_idle() {
        // Compose all five runners with empty queues and the production
        // stand-in (noop) dispatchers. With no requests in flight the
        // drain phase should observe zero `inflight_count` on its first
        // poll and return immediately, well before `drain_deadline`.
        use symphony_core::{
            ActiveSetStore, BudgetPauseDispatchQueue, BudgetPauseRunner,
            FollowupApprovalDispatchQueue, FollowupApprovalRunner, IntegrationDispatchQueue,
            IntegrationDispatchRunner, QaDispatchQueue, QaDispatchRunner, SpecialistDispatchQueue,
            SpecialistRunner,
        };

        struct NoopDispatcher;
        #[async_trait::async_trait]
        impl Dispatcher for NoopDispatcher {
            async fn dispatch(&self, _issue: Issue, _cancel: CancellationToken) -> ReleaseReason {
                ReleaseReason::Completed
            }
        }

        let active_set = Arc::new(ActiveSetStore::new());
        let specialist = Arc::new(SpecialistRunner::new(
            Arc::new(SpecialistDispatchQueue::new()),
            active_set,
            Arc::new(NoopDispatcher),
            1,
        ));
        let integration = Arc::new(IntegrationDispatchRunner::new(
            Arc::new(IntegrationDispatchQueue::new()),
            crate::scheduler::noop_integration_dispatcher(),
            1,
        ));
        let qa = Arc::new(QaDispatchRunner::new(
            Arc::new(QaDispatchQueue::new()),
            crate::scheduler::noop_qa_dispatcher(),
            1,
        ));
        let followup = Arc::new(FollowupApprovalRunner::new(
            Arc::new(FollowupApprovalDispatchQueue::new()),
            crate::scheduler::noop_followup_approval_dispatcher(),
            1,
        ));
        let budget = Arc::new(BudgetPauseRunner::new(
            Arc::new(BudgetPauseDispatchQueue::new()),
            crate::scheduler::noop_budget_pause_dispatcher(),
            1,
        ));

        let cancel = CancellationToken::new();
        let handle = {
            let cancel = cancel.clone();
            tokio::spawn(async move {
                run_runner_loop(
                    cancel,
                    specialist,
                    integration,
                    qa,
                    followup,
                    budget,
                    // Long tick interval; we exit via cancel before the
                    // first sleep elapses.
                    Duration::from_secs(60),
                    // Drain deadline is generous; the assertion below
                    // checks we exit *well* before this fires.
                    Duration::from_secs(5),
                )
                .await;
            })
        };

        // Give the spawned task a chance to enter its select.
        tokio::time::sleep(Duration::from_millis(10)).await;
        cancel.cancel();

        // The driver should observe cancel, run the drain phase (which
        // sees zero inflight on the first poll), and return well within
        // a second.
        let joined = tokio::time::timeout(Duration::from_secs(2), handle).await;
        assert!(
            joined.is_ok(),
            "run_runner_loop must return promptly when cancelled with no inflight work",
        );
        joined.unwrap().expect("runner driver task panicked");
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
