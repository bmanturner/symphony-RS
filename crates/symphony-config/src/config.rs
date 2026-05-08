//! Typed `WORKFLOW.md` front-matter model.
//!
//! This module defines [`WorkflowConfig`] and its child structs, which
//! together form the in-memory representation of the YAML front matter
//! described in SPEC §5.3. The shape mirrors the spec verbatim so that
//! adding or renaming a key happens in exactly one place.
//!
//! ## Why a typed struct (and not a `serde_yaml::Value`)?
//!
//! The orchestrator branches on a handful of these fields on every tick
//! (`polling.interval_ms`, `agent.max_concurrent_agents`,
//! `tracker.kind`). Pushing them through a typed struct gives us:
//!
//! 1. **Validation at the boundary.** Bad values fail loudly during
//!    `WorkflowLoader::from_path`, not three hours into a run.
//! 2. **Defaults documented as code.** Every `#[serde(default = "...")]`
//!    points at a function whose name encodes the SPEC default value, so
//!    a reviewer can audit a default with `git grep`.
//! 3. **Strictness over ambiguity.** Unknown keys at *any* level are
//!    rejected (SPEC v2 §5: "Unknown top-level and nested keys MUST be
//!    rejected"). Typos in `polling.interva_ms` should not silently fall
//!    back to defaults, and a stray top-level `routes:` (when the schema
//!    expects `routing:`) should fail load loudly.
//!
//! ## Deviations from the upstream Symphony spec
//!
//! - `agent.kind` is added so we can pick between the `codex`, `claude`,
//!   and experimental `tandem` runners. The upstream spec hard-codes
//!   Codex; we abstract it behind [`AgentRunner`](../../symphony_agent/index.html).
//! - `tracker.kind` is widened from a single `linear` literal to an enum
//!   that also accepts `github` for the GitHub Issues adapter we ship in
//!   v1.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Root of the `WORKFLOW.md` front matter.
///
/// Round-trips losslessly through `serde_yaml`. Constructed either via
/// `serde_yaml::from_str` (during load) or `WorkflowConfig::default()`
/// (for tests and the in-memory base layer of the figment merge).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowConfig {
    /// Workflow schema version. Optional while the project is
    /// pre-adoption: omitting it defaults to [`SUPPORTED_SCHEMA_VERSION`]
    /// (currently `1`). If present, it MUST equal that constant. Any
    /// other value is a validation error so a stale `WORKFLOW.md` that
    /// expected a different schema fails loudly at load time rather than
    /// being silently coerced. See SPEC v2 §5.0.
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,

    /// Issue-tracker selection and credentials. See SPEC §5.3.1.
    #[serde(default)]
    pub tracker: TrackerConfig,

    /// Polling cadence for the orchestrator's tick loop. SPEC §5.3.2.
    #[serde(default)]
    pub polling: PollingConfig,

    /// Per-issue workspace root and isolation rules. SPEC §5.3.3.
    #[serde(default)]
    pub workspace: WorkspaceConfig,

    /// Optional shell hooks fired around each workspace lifecycle event.
    /// SPEC §5.3.4.
    #[serde(default)]
    pub hooks: HooksConfig,

    /// Concurrency, turn caps, and backoff for the coding-agent layer.
    /// SPEC §5.3.5 plus our `agent.kind` deviation.
    #[serde(default)]
    pub agent: AgentConfig,

    /// Codex-specific pass-through config. SPEC §5.3.6.
    ///
    /// Fields are intentionally typed loosely (`serde_yaml::Value`)
    /// because Codex evolves its own schema and we are not the source of
    /// truth for it. The runtime validates these at dispatch preflight.
    #[serde(default)]
    pub codex: CodexConfig,

    /// Hermes Agent CLI defaults. SPEC v2 §5.5.
    ///
    /// Mirrors [`CodexConfig`] for the Hermes backend: the global block
    /// carries the default command, source tag, and timeout knobs that a
    /// per-profile [`AgentBackendProfile`] inherits. Fields beyond the
    /// CLI command are kept narrow because Hermes' policy surface lives
    /// inside its own configuration, not ours.
    #[serde(default)]
    pub hermes: HermesAgentConfig,

    /// Out-of-process status surface (Phase 8). Controls whether the
    /// daemon exposes `GET /events` as an SSE feed and where it binds.
    /// Defaults are loopback-only and on, so a fresh `WORKFLOW.md`
    /// "just works" with `symphony watch` without surprising the
    /// operator with an open port on a public interface.
    #[serde(default)]
    pub status: StatusConfig,

    /// Workflow-defined roles keyed by their (workflow-chosen) name.
    /// SPEC v2 §5.4. Names like `platform_lead` or `qa` are
    /// configurable; the `kind` discriminant ([`RoleKind`]) carries
    /// the semantic identity that the kernel consumes.
    ///
    /// Empty by default so existing v1-shaped fixtures still load
    /// while later phases incrementally enforce that
    /// `kind: integration_owner` (and `kind: qa_gate` when QA is
    /// required) are present.
    #[serde(default)]
    pub roles: BTreeMap<String, RoleConfig>,

    /// Workflow-defined agent profiles keyed by their (workflow-chosen)
    /// name. SPEC v2 §4.5 / §5.5. Profiles are *decoupled from role
    /// names*: a [`RoleConfig`] references a profile by name via
    /// [`RoleConfig::agent`], so two roles can share one profile and
    /// one role can be re-pointed at a different profile without
    /// renaming. Empty by default; later phases enforce that every
    /// `RoleConfig::agent` reference resolves to a key here.
    #[serde(default)]
    pub agents: BTreeMap<String, AgentProfileConfig>,

    /// Deterministic routing rules that map a normalised work item to a
    /// role name (key into [`WorkflowConfig::roles`]). SPEC v2 §5.6.
    /// Defaults to an empty rule set with no `default_role`; later
    /// phases enforce that every `assign_role` and `default_role`
    /// reference resolves to a configured role.
    #[serde(default)]
    pub routing: RoutingConfig,

    /// Broad-issue decomposition policy. SPEC v2 §5.7. Defaults to
    /// disabled with no `owner_role` so a workflow that does not
    /// mention decomposition behaves identically to v1; later phases
    /// enforce that `owner_role` resolves to a role of
    /// `kind: integration_owner` once decomposition is enabled.
    #[serde(default)]
    pub decomposition: DecompositionConfig,

    /// Integration-owner consolidation policy. SPEC v2 §5.10. Defaults
    /// to an empty `required_for` set with no `owner_role`, so a
    /// workflow that omits the block behaves identically to v1; later
    /// phases enforce that `owner_role` resolves to a role of
    /// `kind: integration_owner` once `required_for` is non-empty.
    #[serde(default)]
    pub integration: IntegrationConfig,

    /// Pull-request lifecycle policy. SPEC v2 §5.11. Defaults to
    /// `enabled: false` so a workflow that omits the block behaves
    /// like v1 (no PR side effects). Later phases enforce that
    /// `owner_role` resolves to a role of `kind: integration_owner`
    /// and that `provider` lines up with the configured tracker
    /// (only GitHub supports PR mutations today).
    #[serde(default)]
    pub pull_requests: PullRequestConfig,
}

/// The only `schema_version` accepted by this build. Bumped only when
/// `WORKFLOW.md` requires an explicit format migration; ordinary product
/// evolution should not bump this constant.
pub const SUPPORTED_SCHEMA_VERSION: u32 = 1;

fn default_schema_version() -> u32 {
    SUPPORTED_SCHEMA_VERSION
}

impl Default for WorkflowConfig {
    fn default() -> Self {
        Self {
            schema_version: SUPPORTED_SCHEMA_VERSION,
            tracker: TrackerConfig::default(),
            polling: PollingConfig::default(),
            workspace: WorkspaceConfig::default(),
            hooks: HooksConfig::default(),
            agent: AgentConfig::default(),
            codex: CodexConfig::default(),
            hermes: HermesAgentConfig::default(),
            status: StatusConfig::default(),
            roles: BTreeMap::new(),
            agents: BTreeMap::new(),
            routing: RoutingConfig::default(),
            decomposition: DecompositionConfig::default(),
            integration: IntegrationConfig::default(),
            pull_requests: PullRequestConfig::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// tracker
// ---------------------------------------------------------------------------

/// Issue tracker configuration.
///
/// `api_key` is stored as the raw string literal (or `$VAR_NAME`
/// placeholder) read from disk. Environment expansion happens during
/// the figment-layered load step so that the typed struct is plain old
/// data with no IO side effects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackerConfig {
    /// Which adapter to instantiate. We default to `Linear` so an
    /// out-of-the-box `WORKFLOW.md` matches the upstream Symphony spec.
    #[serde(default)]
    pub kind: TrackerKind,

    /// Tracker API endpoint. `None` means "use the adapter default".
    #[serde(default)]
    pub endpoint: Option<String>,

    /// Tracker API token. May be a literal value or a `$VAR_NAME`
    /// placeholder; resolution happens during workflow loading.
    #[serde(default)]
    pub api_key: Option<String>,

    /// Linear project slug. REQUIRED for dispatch when `kind == Linear`.
    /// Ignored for the GitHub adapter, which derives the project scope
    /// from `repository`.
    #[serde(default)]
    pub project_slug: Option<String>,

    /// GitHub `owner/repo` slug. REQUIRED for dispatch when
    /// `kind == Github`. Mirrors `project_slug`'s role for Linear.
    #[serde(default)]
    pub repository: Option<String>,

    /// Path to a YAML file of canned issues. REQUIRED when
    /// `kind == Mock`; ignored otherwise. Used by the Quickstart fixture
    /// flow so an operator can run `symphony run` end-to-end without a
    /// Linear or GitHub credential. Path is interpreted relative to the
    /// `WORKFLOW.md` file when the CLI loads it.
    #[serde(default)]
    pub fixtures: Option<PathBuf>,

    /// State names that count as "needs work". Stored case-sensitive
    /// because operators write them naturally; conformance tests assert
    /// adapters do the lowercase comparison.
    #[serde(default = "default_active_states")]
    pub active_states: Vec<String>,

    /// State names treated as terminal for cleanup purposes.
    #[serde(default = "default_terminal_states")]
    pub terminal_states: Vec<String>,
}

impl Default for TrackerConfig {
    fn default() -> Self {
        Self {
            kind: TrackerKind::default(),
            endpoint: None,
            api_key: None,
            project_slug: None,
            repository: None,
            fixtures: None,
            active_states: default_active_states(),
            terminal_states: default_terminal_states(),
        }
    }
}

/// Adapter discriminator for the `IssueTracker` trait.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrackerKind {
    /// Linear via GraphQL. Default for parity with upstream Symphony.
    #[default]
    Linear,
    /// GitHub Issues via REST + GraphQL.
    Github,
    /// In-memory `MockTracker` populated from a YAML fixture file. Used
    /// by the Quickstart so the binary can be exercised without a real
    /// tracker credential. Not intended for production use — the runtime
    /// keeps the canned set in memory and never refreshes it.
    Mock,
}

fn default_active_states() -> Vec<String> {
    vec!["Todo".to_string(), "In Progress".to_string()]
}

fn default_terminal_states() -> Vec<String> {
    vec![
        "Closed".to_string(),
        "Cancelled".to_string(),
        "Canceled".to_string(),
        "Duplicate".to_string(),
        "Done".to_string(),
    ]
}

// ---------------------------------------------------------------------------
// polling
// ---------------------------------------------------------------------------

/// Tick cadence for the orchestrator (SPEC v2 §5.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PollingConfig {
    /// Wall-clock milliseconds between successive polls. The
    /// orchestrator adds jitter on top.
    #[serde(default = "default_poll_interval_ms")]
    pub interval_ms: u64,

    /// Maximum random jitter (in ms) added to each poll interval to
    /// prevent thundering-herd behaviour when multiple kernels run
    /// against the same tracker. `0` disables jitter. Default: `5000`.
    #[serde(default = "default_poll_jitter_ms")]
    pub jitter_ms: u64,

    /// On startup, fetch recently terminal issues alongside active ones
    /// so stale leases/workspaces can be reconciled after tracker-side
    /// state changes. SPEC v2 §5.2 / §3 startup recovery. Default: `true`.
    #[serde(default = "default_startup_reconcile_recent_terminal")]
    pub startup_reconcile_recent_terminal: bool,
}

impl Default for PollingConfig {
    fn default() -> Self {
        Self {
            interval_ms: default_poll_interval_ms(),
            jitter_ms: default_poll_jitter_ms(),
            startup_reconcile_recent_terminal: default_startup_reconcile_recent_terminal(),
        }
    }
}

fn default_poll_interval_ms() -> u64 {
    30_000
}

fn default_poll_jitter_ms() -> u64 {
    5_000
}

fn default_startup_reconcile_recent_terminal() -> bool {
    true
}

// ---------------------------------------------------------------------------
// workspace
// ---------------------------------------------------------------------------

/// Per-issue workspace root.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    /// Root directory for per-issue workspaces. `~` and `$VAR`
    /// expansion happen during loading. `None` resolves at runtime to
    /// `<system-temp>/symphony_workspaces` per SPEC §5.3.3.
    #[serde(default)]
    pub root: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// hooks
// ---------------------------------------------------------------------------

/// Shell hook scripts fired around the workspace lifecycle.
///
/// Each script is the raw `bash -lc` body (multi-line strings are
/// natural in YAML). The runtime is responsible for composing the
/// command, not for parsing it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HooksConfig {
    /// Runs once when the workspace directory is newly created. A
    /// non-zero exit aborts workspace creation.
    #[serde(default)]
    pub after_create: Option<String>,

    /// Runs before each agent attempt, after workspace prep. Failure
    /// aborts the attempt.
    #[serde(default)]
    pub before_run: Option<String>,

    /// Runs after each agent attempt regardless of outcome. Failure is
    /// logged and ignored.
    #[serde(default)]
    pub after_run: Option<String>,

    /// Runs before workspace deletion if the directory exists. Failure
    /// is logged and ignored; cleanup still proceeds.
    #[serde(default)]
    pub before_remove: Option<String>,

    /// Wall-clock timeout applied to every hook in this section.
    #[serde(default = "default_hook_timeout_ms")]
    pub timeout_ms: u64,
}

impl Default for HooksConfig {
    fn default() -> Self {
        Self {
            after_create: None,
            before_run: None,
            after_run: None,
            before_remove: None,
            timeout_ms: default_hook_timeout_ms(),
        }
    }
}

fn default_hook_timeout_ms() -> u64 {
    60_000
}

// ---------------------------------------------------------------------------
// agent
// ---------------------------------------------------------------------------

/// Coding-agent dispatch policy.
///
/// `kind` is our agent-agnostic deviation: it picks which `AgentRunner`
/// the orchestrator instantiates. Everything else mirrors SPEC §5.3.5.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    /// Which `AgentRunner` to instantiate.
    #[serde(default)]
    pub kind: AgentKind,

    /// Hard cap on simultaneously running coding-agent sessions.
    #[serde(default = "default_max_concurrent_agents")]
    pub max_concurrent_agents: u32,

    /// Hard cap on the number of agent turns within one session.
    /// MUST be positive; validated at load time.
    #[serde(default = "default_max_turns")]
    pub max_turns: u32,

    /// Upper bound on the exponential-backoff delay between retries.
    #[serde(default = "default_max_retry_backoff_ms")]
    pub max_retry_backoff_ms: u64,

    /// Optional per-state concurrency caps. Keys SHOULD be lowercase;
    /// the orchestrator normalises before lookup.
    #[serde(default)]
    pub max_concurrent_agents_by_state: BTreeMap<String, u32>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            kind: AgentKind::default(),
            max_concurrent_agents: default_max_concurrent_agents(),
            max_turns: default_max_turns(),
            max_retry_backoff_ms: default_max_retry_backoff_ms(),
            max_concurrent_agents_by_state: BTreeMap::new(),
        }
    }
}

/// Which coding-agent backend to drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    /// `codex app-server` over JSONL stdio. Default for parity with
    /// upstream Symphony.
    #[default]
    Codex,
    /// `claude -p --output-format stream-json` over stdio.
    Claude,
    /// Run both runners concurrently with one designated lead. The
    /// strategy and lead are configured in a future `tandem` block; this
    /// variant just turns the wrapper on.
    Tandem,
    /// In-process scripted runner that emits a deterministic
    /// `Started → Message → Completed` sequence per turn. Lets
    /// `symphony run` exercise the dispatch path end-to-end without a
    /// `codex` or `claude` binary on PATH — wired by the Quickstart
    /// fixture and the CLI smoke tests. Not for production use.
    Mock,
}

fn default_max_concurrent_agents() -> u32 {
    10
}

fn default_max_turns() -> u32 {
    20
}

fn default_max_retry_backoff_ms() -> u64 {
    300_000
}

// ---------------------------------------------------------------------------
// codex
// ---------------------------------------------------------------------------

/// Codex-specific pass-through config.
///
/// We deliberately do not type `approval_policy`, `thread_sandbox`, or
/// `turn_sandbox_policy` because their value sets evolve with the Codex
/// app-server and SPEC §5.3.6 explicitly directs implementors to treat
/// them as opaque. They're carried through as `serde_yaml::Value` and
/// validated at dispatch preflight.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodexConfig {
    /// Shell command launched via `bash -lc` from the workspace root.
    #[serde(default = "default_codex_command")]
    pub command: String,

    /// Codex `AskForApproval` value. Pass-through; not validated here.
    #[serde(default)]
    pub approval_policy: Option<serde_yaml::Value>,

    /// Codex `SandboxMode` value. Pass-through; not validated here.
    #[serde(default)]
    pub thread_sandbox: Option<serde_yaml::Value>,

    /// Codex `SandboxPolicy` value. Pass-through; not validated here.
    #[serde(default)]
    pub turn_sandbox_policy: Option<serde_yaml::Value>,

    /// Per-turn timeout in milliseconds. SPEC default: 1 hour.
    #[serde(default = "default_codex_turn_timeout_ms")]
    pub turn_timeout_ms: u64,

    /// Idle-read timeout in milliseconds. SPEC default: 5 seconds.
    #[serde(default = "default_codex_read_timeout_ms")]
    pub read_timeout_ms: u64,

    /// Stall-detection window. `0` disables stall detection. SPEC
    /// default: 5 minutes.
    #[serde(default = "default_codex_stall_timeout_ms")]
    pub stall_timeout_ms: u64,
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            command: default_codex_command(),
            approval_policy: None,
            thread_sandbox: None,
            turn_sandbox_policy: None,
            turn_timeout_ms: default_codex_turn_timeout_ms(),
            read_timeout_ms: default_codex_read_timeout_ms(),
            stall_timeout_ms: default_codex_stall_timeout_ms(),
        }
    }
}

fn default_codex_command() -> String {
    "codex app-server".to_string()
}

fn default_codex_turn_timeout_ms() -> u64 {
    3_600_000
}

fn default_codex_read_timeout_ms() -> u64 {
    5_000
}

fn default_codex_stall_timeout_ms() -> u64 {
    300_000
}

// ---------------------------------------------------------------------------
// hermes (SPEC v2 §5.5)
// ---------------------------------------------------------------------------

/// Hermes Agent CLI pass-through config (SPEC v2 §5.5).
///
/// Shape mirrors [`CodexConfig`] so an operator who knows the Codex block
/// can read this one. Hermes is invoked in non-interactive mode with the
/// rendered prompt streamed through stdin or a temporary file (`--query-file
/// -`) and a stable source tag for telemetry/audit. The remaining policy
/// (model, approvals, sandbox) lives inside Hermes' own configuration —
/// we only carry what the orchestrator needs to spawn the process.
///
/// If Hermes later exposes a stable ACP/server protocol the adapter may
/// switch to it behind the same `backend: hermes` contract; this config
/// shape stays the same because operators continue to declare just the
/// process-launch knobs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HermesAgentConfig {
    /// Shell command launched via `bash -lc` from the workspace root.
    /// Defaults to the SPEC §5.5 example so a fresh `WORKFLOW.md` runs
    /// without an explicit override.
    #[serde(default = "default_hermes_command")]
    pub command: String,

    /// Source tag passed to Hermes for telemetry/audit. Defaults to
    /// `symphony` so multiple deployments are distinguishable in Hermes'
    /// own logs without per-workflow customisation.
    #[serde(default = "default_hermes_source")]
    pub source: String,

    /// Per-turn timeout in milliseconds. SPEC default: 1 hour, matching
    /// Codex so workflows don't need to special-case the backend.
    #[serde(default = "default_hermes_turn_timeout_ms")]
    pub turn_timeout_ms: u64,

    /// Idle-read timeout in milliseconds. SPEC default: 5 seconds.
    #[serde(default = "default_hermes_read_timeout_ms")]
    pub read_timeout_ms: u64,

    /// Stall-detection window. `0` disables stall detection. SPEC
    /// default: 5 minutes.
    #[serde(default = "default_hermes_stall_timeout_ms")]
    pub stall_timeout_ms: u64,
}

impl Default for HermesAgentConfig {
    fn default() -> Self {
        Self {
            command: default_hermes_command(),
            source: default_hermes_source(),
            turn_timeout_ms: default_hermes_turn_timeout_ms(),
            read_timeout_ms: default_hermes_read_timeout_ms(),
            stall_timeout_ms: default_hermes_stall_timeout_ms(),
        }
    }
}

fn default_hermes_command() -> String {
    "hermes chat --query-file - --source symphony".to_string()
}

fn default_hermes_source() -> String {
    "symphony".to_string()
}

fn default_hermes_turn_timeout_ms() -> u64 {
    3_600_000
}

fn default_hermes_read_timeout_ms() -> u64 {
    5_000
}

fn default_hermes_stall_timeout_ms() -> u64 {
    300_000
}

// ---------------------------------------------------------------------------
// status (Phase 8 — SSE event surface)
// ---------------------------------------------------------------------------

/// Configuration for the out-of-process status surface.
///
/// The status surface is a narrow `axum` HTTP server inside the daemon
/// that re-emits the orchestrator's broadcast bus over Server-Sent
/// Events. `symphony watch` (and any third-party observer) connects to
/// `GET /events` and receives one `OrchestratorEvent` per SSE frame.
///
/// The server is intentionally simple — no auth, no TLS — because the
/// default bind is `127.0.0.1`. Operators who want to expose the
/// surface beyond localhost are expected to put a reverse proxy in
/// front of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusConfig {
    /// Whether to start the SSE server alongside the orchestrator.
    /// `true` keeps `symphony watch` working out of the box; setting
    /// this to `false` skips the bind entirely (useful in CI, in
    /// systemd unit tests, or when running multiple daemons on the
    /// same host).
    #[serde(default = "default_status_enabled")]
    pub enabled: bool,

    /// Address to bind the SSE listener on. Defaults to
    /// `127.0.0.1:6280` — loopback so the server isn't accidentally
    /// reachable from the network. Anything `SocketAddr` accepts is
    /// valid; the runtime parses lazily on bind, so a malformed value
    /// surfaces at startup rather than at config-load time.
    #[serde(default = "default_status_bind")]
    pub bind: String,

    /// Capacity of the `tokio::sync::broadcast` channel that fans
    /// orchestrator events out to subscribers. Higher values absorb
    /// slower consumers at the cost of memory; a slow consumer that
    /// falls behind by more than this many events sees a `Lagged`
    /// signal and is expected to reconnect. Default mirrors the
    /// orchestrator's own internal default (256).
    #[serde(default = "default_status_replay_buffer")]
    pub replay_buffer: usize,
}

impl Default for StatusConfig {
    fn default() -> Self {
        Self {
            enabled: default_status_enabled(),
            bind: default_status_bind(),
            replay_buffer: default_status_replay_buffer(),
        }
    }
}

fn default_status_enabled() -> bool {
    true
}

fn default_status_bind() -> String {
    "127.0.0.1:6280".to_string()
}

fn default_status_replay_buffer() -> usize {
    256
}

// ---------------------------------------------------------------------------
// roles (SPEC v2 §4.4 / §5.4)
// ---------------------------------------------------------------------------

/// Semantic role kind. The kernel branches on this discriminant; the
/// workflow-chosen role *name* is just a label.
///
/// Only [`IntegrationOwner`] and [`QaGate`] are semantically special in
/// core v2 — every other variant is configurable behaviour driven by
/// workflow rules. `Custom` is a *role kind*, not an adapter extension
/// point: it lets workflows declare a role that does not map onto any
/// of the well-known kinds without forcing them through `Specialist`.
///
/// [`IntegrationOwner`]: RoleKind::IntegrationOwner
/// [`QaGate`]: RoleKind::QaGate
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleKind {
    /// May decompose, assign, merge child outputs, request QA, and
    /// close parent work. Exactly one role of this kind is required
    /// per workflow once Phase 1 validation lands.
    IntegrationOwner,
    /// May verify, reject, file blockers, and create follow-up issues.
    /// Required when `qa.required: true`.
    QaGate,
    /// Implements scoped work and reports evidence.
    Specialist,
    /// Reviews design/security/performance/content without owning
    /// implementation.
    Reviewer,
    /// Performs runtime/deployment/data tasks.
    Operator,
    /// Workflow-defined behaviour outside the well-known kinds. Not a
    /// new adapter type — the kernel still treats it as a routable role.
    Custom,
}

/// One entry in the `roles` map (SPEC v2 §5.4).
///
/// Most authority flags default to "unset" (`None`) rather than `false`
/// so the kernel can distinguish "operator did not configure this" from
/// "operator explicitly disabled it". Phase 5+ collapses these into
/// effective authority by combining the role kind with the explicit
/// flags; until then the typed struct just preserves what the YAML
/// declares so the loader is honest about operator intent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleConfig {
    /// Semantic kind. The kernel reads this; the role's *name* (the
    /// map key in [`WorkflowConfig::roles`]) is just a label.
    pub kind: RoleKind,

    /// Free-form description shown in operator surfaces (status, TUI,
    /// validation errors). Optional.
    #[serde(default)]
    pub description: Option<String>,

    /// Name of the agent profile (key into `WorkflowConfig::agents`)
    /// that runs work for this role. Optional at parse time so
    /// fixtures can declare a role before its agent is wired up;
    /// dispatch-time validation requires the reference to resolve.
    #[serde(default)]
    pub agent: Option<String>,

    /// Hard cap on simultaneously running agents for this role. `None`
    /// inherits the global `agent.max_concurrent_agents` cap.
    #[serde(default)]
    pub max_concurrent: Option<u32>,

    /// `integration_owner` authority: may decompose broad work into
    /// child issues. `None` defers to the role-kind default.
    #[serde(default)]
    pub can_decompose: Option<bool>,

    /// `integration_owner` authority: may assign children to roles.
    #[serde(default)]
    pub can_assign: Option<bool>,

    /// `integration_owner` authority: may request QA on a draft PR or
    /// integration handoff.
    #[serde(default)]
    pub can_request_qa: Option<bool>,

    /// `integration_owner` authority: may close the parent issue once
    /// children, integration, and QA gates are all satisfied.
    #[serde(default)]
    pub can_close_parent: Option<bool>,

    /// `qa_gate` authority: may file blocker work items against the
    /// item under review.
    #[serde(default)]
    pub can_file_blockers: Option<bool>,

    /// `qa_gate` authority: may file follow-up work items (per
    /// `followups.default_policy`).
    #[serde(default)]
    pub can_file_followups: Option<bool>,

    /// `qa_gate` policy: this role's pass is required before the
    /// parent can be closed. Equivalent to `qa.required: true` scoped
    /// to a single role definition.
    #[serde(default)]
    pub required_for_done: Option<bool>,
}

// ---------------------------------------------------------------------------
// agents (SPEC v2 §4.5 / §5.5)
// ---------------------------------------------------------------------------

/// Backend class for an [`AgentBackendProfile`]. SPEC v2 §5.5 limits
/// product backends to `codex`, `claude`, and `hermes`; the test-only
/// `mock` variant is preserved here so quickstart fixtures and unit
/// tests can declare profiles without an external binary.
///
/// Composite/tandem behaviour is *not* a backend — SPEC §5.5 defines it
/// as an `agents:` *strategy* that composes other profiles, modelled
/// here as [`AgentCompositeProfile`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentBackend {
    /// `codex app-server` over JSONL stdio.
    Codex,
    /// `claude -p --output-format stream-json` over stdio.
    Claude,
    /// Hermes Agent CLI in non-interactive mode. The orchestrator
    /// composes [`HermesAgentConfig`] defaults under per-profile
    /// overrides at dispatch time.
    Hermes,
    /// In-process scripted runner. Test/fixture only — not advertised
    /// as a production backend.
    Mock,
}

/// One entry in the `agents` map (SPEC v2 §5.5).
///
/// A profile is either a concrete [`AgentBackendProfile`] (`backend:
/// codex|claude|hermes|mock`) or an [`AgentCompositeProfile`] that
/// composes other profiles via a strategy (`strategy: tandem`).
/// Discriminated by the field name `backend` vs `strategy`; serde tries
/// the backend variant first.
///
/// Profile names live in their own keyspace and are referenced from
/// [`RoleConfig::agent`] (and from `lead`/`follower` inside a composite
/// profile), so the same profile can be reused across roles and a role
/// can be re-pointed at a different profile without renaming.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AgentProfileConfig {
    /// Concrete backend profile (`codex`, `claude`, `hermes`, or `mock`).
    /// Boxed so the enum size is dominated by the smaller composite
    /// variant rather than the policy-heavy backend struct.
    Backend(Box<AgentBackendProfile>),
    /// Composite agent that wraps two configured profiles in a strategy
    /// (today: `tandem`). Repackages the existing tandem runner as an
    /// `agents:` entry so it composes any combination of `codex`,
    /// `claude`, or `hermes` lead/follower seats.
    Composite(AgentCompositeProfile),
}

impl AgentProfileConfig {
    /// Backend tag for the concrete-backend variant. `None` for
    /// composite profiles, whose lead/follower references resolve to
    /// concrete backends via lookup in `WorkflowConfig::agents`.
    pub fn backend(&self) -> Option<AgentBackend> {
        match self {
            AgentProfileConfig::Backend(p) => Some(p.backend),
            AgentProfileConfig::Composite(_) => None,
        }
    }

    /// Borrow the backend variant, if any. Sugar over a `match`.
    pub fn as_backend(&self) -> Option<&AgentBackendProfile> {
        match self {
            AgentProfileConfig::Backend(p) => Some(p.as_ref()),
            AgentProfileConfig::Composite(_) => None,
        }
    }

    /// Borrow the composite variant, if any. Sugar over a `match`.
    pub fn as_composite(&self) -> Option<&AgentCompositeProfile> {
        match self {
            AgentProfileConfig::Backend(_) => None,
            AgentProfileConfig::Composite(c) => Some(c),
        }
    }
}

/// Strategy discriminant for an [`AgentCompositeProfile`].
///
/// Today only `tandem` is defined; the enum is left open for future
/// composition strategies (e.g. `voting`, `pipeline`) without a schema
/// break, which is why it isn't collapsed into a unit struct.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStrategy {
    /// Lead/follower pair driven by a [`TandemMode`]. Repackages the
    /// existing in-tree tandem runner.
    Tandem,
}

/// Tandem strategy mode. Mirrors the in-tree tandem runner's strategy
/// enum so a workflow can pick draft/review, split-implement, or
/// consensus without leaking implementation types into the schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TandemMode {
    /// Lead drafts the turn, follower reviews it.
    DraftReview,
    /// Lead plans, follower executes claimed subtasks.
    SplitImplement,
    /// Both run in parallel; pick the higher-scoring transcript.
    Consensus,
}

/// `agents:` entry that composes two configured profiles via a
/// [`AgentStrategy`] (SPEC v2 §5.5). `lead` and `follower` are profile
/// names that MUST resolve in `WorkflowConfig::agents` and MUST point at
/// concrete backend profiles — composite-of-composite is not allowed.
/// Validation of those references is deferred to a later checklist
/// iteration; the parsed struct only captures operator intent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentCompositeProfile {
    /// Composition strategy. Required; today only `tandem` is defined.
    pub strategy: AgentStrategy,

    /// Profile name (key in `WorkflowConfig::agents`) for the lead seat.
    pub lead: String,

    /// Profile name for the follower seat. May reference the same
    /// concrete profile as `lead` — running two instances of the same
    /// backend with different system prompts is a supported pattern.
    pub follower: String,

    /// Strategy-specific mode. Required for `tandem`; future strategies
    /// may treat this differently, which is why it's typed as the
    /// shared [`TandemMode`] for now and revisited when a second
    /// strategy lands.
    pub mode: TandemMode,

    /// Free-form description shown in operator surfaces.
    #[serde(default)]
    pub description: Option<String>,
}

/// Concrete-backend `agents:` entry (SPEC v2 §5.5).
///
/// A profile bundles a backend with the runtime policy that should be
/// applied when launching it. Most policy fields are `Option`-typed and
/// default to "inherit": the kernel composes a profile-level value over
/// the global [`AgentConfig`]/[`CodexConfig`]/[`HermesAgentConfig`]
/// defaults at dispatch time. This keeps profiles small and lets
/// operators declare just the deltas they care about.
///
/// Fields that mirror the upstream Codex/Claude schema (`approval_policy`,
/// `sandbox_policy`) are carried as opaque `serde_yaml::Value` for the
/// same reason as [`CodexConfig`]: those value sets evolve outside our
/// schema and the runtime validates them at dispatch preflight.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentBackendProfile {
    /// Backend class to instantiate. Required — there is no sensible
    /// default once a profile is declared.
    pub backend: AgentBackend,

    /// Free-form description shown in operator surfaces. Optional.
    #[serde(default)]
    pub description: Option<String>,

    /// Shell command launched via `bash -lc`. `None` defers to the
    /// backend's default command (e.g. `codex app-server`).
    #[serde(default)]
    pub command: Option<String>,

    /// Model identifier passed through to the backend. `None` uses the
    /// backend's configured default.
    #[serde(default)]
    pub model: Option<String>,

    /// System prompt or named profile fragment to prepend. `None`
    /// inherits the role/workflow prompt only.
    #[serde(default)]
    pub system_prompt: Option<String>,

    /// Tool/toolset names enabled for this profile. Empty list means
    /// "no tools beyond backend defaults".
    #[serde(default)]
    pub tools: Vec<String>,

    /// Extra environment variables exported into the agent process.
    /// Values may contain `$VAR_NAME` placeholders; resolution
    /// happens during workflow loading.
    #[serde(default)]
    pub env: BTreeMap<String, String>,

    /// Memory policy hint (`none`, `session`, `persistent`, etc.).
    /// Carried as an opaque string because the value set is owned by
    /// the backend, not the schema.
    #[serde(default)]
    pub memory: Option<String>,

    /// Backend approval policy. Pass-through; not validated here.
    #[serde(default)]
    pub approval_policy: Option<serde_yaml::Value>,

    /// Backend sandbox policy. Pass-through; not validated here.
    #[serde(default)]
    pub sandbox_policy: Option<serde_yaml::Value>,

    /// Hard cap on the number of agent turns within one session.
    /// `None` inherits [`AgentConfig::max_turns`].
    #[serde(default)]
    pub max_turns: Option<u32>,

    /// Per-turn timeout in milliseconds. `None` inherits the backend
    /// default (e.g. [`CodexConfig::turn_timeout_ms`]).
    #[serde(default)]
    pub turn_timeout_ms: Option<u64>,

    /// Stall-detection window. `None` inherits the backend default.
    #[serde(default)]
    pub stall_timeout_ms: Option<u64>,

    /// Token budget for the profile. `None` means unlimited at the
    /// profile level (global budgets still apply).
    #[serde(default)]
    pub token_budget: Option<u64>,

    /// Cost budget in USD for the profile. `None` means unlimited at
    /// the profile level.
    #[serde(default)]
    pub cost_budget_usd: Option<f64>,
}

// ---------------------------------------------------------------------------
// routing (SPEC v2 §5.6)
// ---------------------------------------------------------------------------

/// Deterministic role-routing configuration.
///
/// The kernel evaluates [`RoutingConfig::rules`] against a normalised
/// work item to pick a destination role. Determinism is the contract:
/// the same input must pick the same role on every run.
///
/// `match_mode` decides how rules compose:
/// - [`RoutingMatchMode::FirstMatch`] evaluates rules in *file order*
///   and picks the first whose `when` clause matches; per SPEC v2 §5.6
///   it ignores `priority` entirely.
/// - [`RoutingMatchMode::Priority`] picks the matching rule with the
///   highest numeric `priority`. Ties are validation errors (enforced
///   in a later checklist iteration); the parsed struct only captures
///   operator intent here.
///
/// `default_role` is the fallback when no rule matches. It is optional
/// at parse time so partial fixtures can declare a routing block before
/// roles are wired up; dispatch-time validation requires the reference
/// to resolve into [`WorkflowConfig::roles`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingConfig {
    /// Fallback role name (key into [`WorkflowConfig::roles`]) used when
    /// no rule in `rules` matches. `None` means "no fallback configured"
    /// — later phases treat this as a routing failure that surfaces in
    /// operator surfaces rather than silently dropping work.
    #[serde(default)]
    pub default_role: Option<String>,

    /// How to combine matching rules. Defaults to
    /// [`RoutingMatchMode::FirstMatch`] so operators who omit the field
    /// get the simpler, file-order semantics by default.
    #[serde(default)]
    pub match_mode: RoutingMatchMode,

    /// Ordered rule list. Order matters for [`RoutingMatchMode::FirstMatch`];
    /// for [`RoutingMatchMode::Priority`] order is informational only.
    #[serde(default)]
    pub rules: Vec<RoutingRule>,
}

/// How [`RoutingConfig::rules`] are combined when more than one matches.
///
/// SPEC v2 §5.6 defines exactly two modes; the enum is closed because
/// adding a third mode would change the routing contract and should
/// require an explicit schema decision rather than a silent extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingMatchMode {
    /// Evaluate rules in declaration order and return the first match.
    /// `priority` fields on rules are ignored in this mode. Default
    /// because file-order is the easiest behaviour for an operator to
    /// reason about when reading a `WORKFLOW.md`.
    #[default]
    FirstMatch,
    /// Pick the matching rule with the highest numeric `priority`. Rules
    /// without an explicit `priority` are treated as priority `0` by
    /// the kernel; ties between matching rules are a validation error.
    Priority,
}

/// One entry in [`RoutingConfig::rules`].
///
/// `priority` is optional so the same rule shape works in both match
/// modes: `first_match` ignores the field, `priority` mode reads it.
/// `when` is the (open-ended but typed) predicate clause; `assign_role`
/// names the destination role. Both `priority`-tie validation and
/// `assign_role` reference resolution are deferred to later checklist
/// items — this struct just preserves operator intent.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingRule {
    /// Numeric priority used by [`RoutingMatchMode::Priority`]. Ignored
    /// by [`RoutingMatchMode::FirstMatch`]. `None` is treated as `0` by
    /// the kernel; we keep it as `Option<u32>` so the parsed struct
    /// distinguishes "operator omitted priority" from "operator wrote 0".
    #[serde(default)]
    pub priority: Option<u32>,

    /// Predicate clause. A rule matches when *every* configured field
    /// in this struct matches the work item; an empty clause matches
    /// every work item, which is how operators express a catch-all
    /// rule without relying on `default_role`.
    #[serde(default)]
    pub when: RoutingMatch,

    /// Destination role name (key into [`WorkflowConfig::roles`]).
    /// Required — a rule without a target is meaningless.
    pub assign_role: String,
}

/// Predicate clause for a [`RoutingRule`] (SPEC v2 §5.6 `when:` block).
///
/// Each field is optional and additive: when multiple fields are set,
/// *all* of them must match (AND-of-fields). Within a single list-typed
/// field the semantics are "any of" (OR-of-elements) — `labels_any` is
/// satisfied when the work item has *any* of the listed labels. The
/// field set is intentionally narrow: SPEC v2 §5.6 only specifies
/// `labels_any`, `paths_any`, and `issue_size`; new predicates require
/// a schema decision rather than a silent extension.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingMatch {
    /// Match when the work item carries any of these labels. Empty list
    /// means "this predicate is not configured" and matches every item.
    #[serde(default)]
    pub labels_any: Vec<String>,

    /// Match when any changed-path glob in the work item intersects any
    /// of these patterns. Glob expansion happens in the routing engine,
    /// not here; the typed config keeps the raw operator strings.
    #[serde(default)]
    pub paths_any: Vec<String>,

    /// Match on coarse work-item size (e.g. `broad`, `narrow`). Carried
    /// as `Option<String>` because the SPEC enumerates only `broad` in
    /// the worked example and we don't want to lock the value set into
    /// the schema until the routing engine lands.
    #[serde(default)]
    pub issue_size: Option<String>,
}

// ---------------------------------------------------------------------------
// decomposition (SPEC v2 §5.7)
// ---------------------------------------------------------------------------

/// Broad-issue decomposition policy.
///
/// Decomposition is the integration owner's first move on broad work:
/// a parent issue is split into child scopes that each map to a
/// specialist role, then later consolidated by the same owner. SPEC v2
/// §5.7 captures the operator-facing surface; this struct is the
/// typed mirror of that block.
///
/// Defaults are deliberately conservative: `enabled: false` with no
/// `owner_role` means a workflow that omits the block behaves like v1
/// (no auto-decomposition). The remaining defaults — `max_depth: 2`,
/// `require_acceptance_criteria_per_child: true`, and
/// [`ChildIssuePolicy::ProposeForApproval`] — match the SPEC's worked
/// example with the safer policy chosen on ties: human approval is the
/// safer default than direct tracker writes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecompositionConfig {
    /// Master switch. When `false` (the default), the runner skips
    /// decomposition even on issues whose triggers would otherwise
    /// fire — operators must opt in by setting `enabled: true` *and*
    /// naming an `owner_role`.
    #[serde(default)]
    pub enabled: bool,

    /// Role name (key into [`WorkflowConfig::roles`]) responsible for
    /// running decomposition. Optional at parse time so partial
    /// fixtures can declare an empty `decomposition:` block; later
    /// phases enforce that the reference resolves to a role of
    /// `kind: integration_owner`.
    #[serde(default)]
    pub owner_role: Option<String>,

    /// Conditions under which an issue is considered "broad" and
    /// therefore eligible for decomposition. Empty by default; an
    /// empty trigger set means "never auto-decompose" even when
    /// `enabled: true`, which matches operator intuition that omitted
    /// triggers shouldn't fire.
    #[serde(default)]
    pub triggers: DecompositionTriggers,

    /// What to do once the integration owner has a child plan: write
    /// the child issues straight through `TrackerMutations` or queue
    /// them for human/integration-owner approval first. SPEC v2 §5.7
    /// shares this enum with `followups.default_policy` (§5.13).
    #[serde(default)]
    pub child_issue_policy: ChildIssuePolicy,

    /// Maximum recursion depth for nested decomposition (parent →
    /// child → grandchild). `2` matches the SPEC example and keeps
    /// recursion bounded; the kernel rejects child plans that would
    /// exceed this depth.
    #[serde(default = "default_decomposition_max_depth")]
    pub max_depth: u32,

    /// When `true`, every child scope MUST carry its own acceptance
    /// criteria; the integration owner's plan is rejected otherwise.
    /// Defaults to `true` so operators get the SPEC's documented
    /// safety behaviour without needing to write the field.
    #[serde(default = "default_true")]
    pub require_acceptance_criteria_per_child: bool,
}

impl Default for DecompositionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            owner_role: None,
            triggers: DecompositionTriggers::default(),
            child_issue_policy: ChildIssuePolicy::default(),
            max_depth: default_decomposition_max_depth(),
            require_acceptance_criteria_per_child: true,
        }
    }
}

fn default_decomposition_max_depth() -> u32 {
    2
}

fn default_true() -> bool {
    true
}

/// Predicate clause for [`DecompositionConfig::triggers`] (SPEC v2 §5.7).
///
/// Each field is optional and additive: a work item is eligible when
/// *any* configured trigger fires (OR-of-fields), so an operator can
/// declare both a label list and a size threshold and have either
/// path light up decomposition. Fields are kept narrow to the SPEC's
/// worked example; new triggers require a schema decision.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecompositionTriggers {
    /// Trigger when the work item carries any of these labels. Empty
    /// list means "this trigger is not configured" and never fires —
    /// the kernel does not decompose every issue by default.
    #[serde(default)]
    pub labels_any: Vec<String>,

    /// Trigger when the integration owner's pre-flight estimate of
    /// touched files exceeds this number. `None` means "this trigger
    /// is not configured"; `Some(0)` would fire on every non-empty
    /// estimate and is preserved as operator intent.
    #[serde(default)]
    pub estimated_files_over: Option<u32>,

    /// Trigger when the work item's acceptance-criteria list exceeds
    /// this number of items. Same `None` vs `Some(0)` semantics as
    /// [`DecompositionTriggers::estimated_files_over`].
    #[serde(default)]
    pub acceptance_items_over: Option<u32>,
}

/// Policy enum shared between `decomposition.child_issue_policy`
/// (SPEC v2 §5.7) and `followups.default_policy` (§5.13).
///
/// `create_directly` writes the child/follow-up issue through
/// `TrackerMutations` as soon as the agent emits it.
/// `propose_for_approval` instead enqueues a durable approval item
/// for the integration owner or a human, deferring the tracker
/// mutation until the queue advances. The default is the safer
/// approval path: a fresh workflow shouldn't silently file new
/// issues against an operator's tracker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildIssuePolicy {
    /// Create the issue immediately via `TrackerMutations`. Use when
    /// the workflow has high trust in its agents and a low tolerance
    /// for queueing latency.
    CreateDirectly,
    /// Queue the proposed issue for approval. Default — protects an
    /// operator's tracker from agent over-eagerness on day one.
    #[default]
    ProposeForApproval,
}

// ---------------------------------------------------------------------------
// integration (SPEC v2 §5.10)
// ---------------------------------------------------------------------------

/// Integration-owner consolidation policy.
///
/// Where decomposition fans broad work *out* into specialist children,
/// integration fans them back *in*: a single owner role consolidates
/// child handoffs onto a canonical integration branch/worktree, runs
/// integration tests, opens or refreshes the draft PR (when PR policy
/// requires it), and produces the QA handoff. SPEC v2 §5.10 specifies
/// the operator-facing surface; this struct is the typed mirror.
///
/// Defaults are deliberately conservative: with no `owner_role` and an
/// empty `required_for` set, a workflow that omits the block behaves
/// like v1 (no auto-integration gate). When the block is present,
/// later phases enforce that `owner_role` resolves to a role of
/// `kind: integration_owner`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntegrationConfig {
    /// Role name (key into [`WorkflowConfig::roles`]) responsible for
    /// running integration. Optional at parse time so partial fixtures
    /// can declare an empty `integration:` block; later phases enforce
    /// that the reference resolves to a role of
    /// `kind: integration_owner`.
    #[serde(default)]
    pub owner_role: Option<String>,

    /// Conditions under which the integration gate fires. Empty by
    /// default; an empty list means "never auto-route to integration"
    /// even if an integration owner is configured. Operators opt in
    /// explicitly per SPEC v2 §5.10's worked example.
    #[serde(default)]
    pub required_for: Vec<IntegrationRequirement>,

    /// How the integration owner consolidates child changes onto the
    /// canonical integration branch. Defaults to
    /// [`MergeStrategy::SequentialCherryPick`] — the conservative
    /// linear-history option matching the SPEC's worked example.
    #[serde(default)]
    pub merge_strategy: MergeStrategy,

    /// What happens when consolidation fails. Defaults to
    /// [`ConflictPolicy::IntegrationOwnerRepairTurn`], the SPEC v2
    /// §5.10 documented default.
    #[serde(default)]
    pub conflict_policy: ConflictPolicy,

    /// When `true` (the default), the integration gate refuses to
    /// run while any required child is still non-terminal. The
    /// kernel's "premature parent completion" guard depends on this
    /// being on; turning it off is an explicit operator override.
    #[serde(default = "default_true")]
    pub require_all_children_terminal: bool,

    /// When `true` (the default), open blockers prevent the
    /// integration owner from advancing to PR/QA. SPEC v2 §5.10
    /// pairs this with the QA-blocker policy in §5.12.
    #[serde(default = "default_true")]
    pub require_no_open_blockers: bool,

    /// When `true` (the default), the integration owner MUST emit a
    /// structured integration summary in their handoff. PR body
    /// templates (SPEC v2 §5.11) interpolate this summary, so the
    /// safer default is to require it.
    #[serde(default = "default_true")]
    pub require_integration_summary: bool,

    /// Tracker state to drive the parent issue into after a successful
    /// integration handoff. Optional at parse time because the state
    /// name is workflow-specific (e.g. `qa`, `In Review`); later
    /// phases enforce that the value matches a configured tracker
    /// state when integration is active.
    #[serde(default)]
    pub next_state_after_integration: Option<String>,
}

impl Default for IntegrationConfig {
    fn default() -> Self {
        Self {
            owner_role: None,
            required_for: Vec::new(),
            merge_strategy: MergeStrategy::default(),
            conflict_policy: ConflictPolicy::default(),
            require_all_children_terminal: true,
            require_no_open_blockers: true,
            require_integration_summary: true,
            next_state_after_integration: None,
        }
    }
}

/// Conditions that route a work item through the integration gate
/// (SPEC v2 §5.10 `required_for`).
///
/// The list is OR-of-elements: an item is required to pass through
/// integration when *any* configured condition is true. The variant
/// set is intentionally narrow to the SPEC's worked example; new
/// conditions require a schema decision rather than a silent
/// extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationRequirement {
    /// The parent item was decomposed into one or more children. The
    /// integration owner must consolidate the children before the
    /// parent can close.
    DecomposedParent,
    /// The work spans multiple child branches/worktrees, even without
    /// formal decomposition. Integration serialises the merge.
    MultipleChildBranches,
}

/// How the integration owner consolidates child changes onto the
/// canonical integration branch (SPEC v2 §5.10 `merge_strategy`).
///
/// The default is [`MergeStrategy::SequentialCherryPick`]: it keeps
/// linear history and surfaces conflicts one child at a time, which
/// pairs well with [`ConflictPolicy::IntegrationOwnerRepairTurn`].
/// Operators with explicit shared-branch workflows (and
/// `branching.allow_same_branch_for_children: true`) opt in to
/// [`MergeStrategy::SharedBranch`] knowingly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeStrategy {
    /// Cherry-pick each child branch's commits onto the integration
    /// branch in dependency order. Default — preserves linear history
    /// and isolates conflicts per child.
    #[default]
    SequentialCherryPick,
    /// Use real merge commits per child branch. Use when the workflow
    /// values branch-shape provenance over linear history.
    MergeCommits,
    /// Children share a single branch with the integration owner;
    /// no consolidation is performed because there is nothing to
    /// merge. Requires the shared-branch workspace strategy.
    SharedBranch,
}

/// What happens when integration consolidation fails (SPEC v2 §5.10
/// `conflict_policy`).
///
/// The default is [`ConflictPolicy::IntegrationOwnerRepairTurn`], the
/// SPEC's documented default: the integration owner gets a repair
/// turn in the canonical integration workspace before the work item
/// becomes blocked with conflict evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictPolicy {
    /// Hand the integration owner a repair turn in the canonical
    /// integration workspace. If the repair turn fails, the work
    /// item is blocked with conflict evidence. Default per SPEC v2
    /// §5.10.
    #[default]
    IntegrationOwnerRepairTurn,
    /// Route the conflict back to the child owner that produced the
    /// offending change for resolution in their own workspace.
    RouteToChildOwner,
    /// Stop and require human intervention. Use when the workflow
    /// has zero tolerance for autonomous merge repair.
    BlockForHuman,
}

// ---------------------------------------------------------------------------
// pull_requests (SPEC v2 §5.11)
// ---------------------------------------------------------------------------

/// Pull-request lifecycle policy.
///
/// Specialists do not open PRs by default; the integration owner owns
/// PR creation, draft → ready transition, and final state. SPEC v2
/// §5.11 specifies the operator-facing surface; this struct is the
/// typed mirror.
///
/// Defaults are deliberately conservative: `enabled: false` with no
/// `owner_role` means a workflow that omits the block behaves
/// identically to v1 (no PR side effects). When enabled, later phases
/// enforce that `owner_role` resolves to a role of
/// `kind: integration_owner` and that `provider` lines up with the
/// configured tracker — only GitHub supports the PR mutations
/// described in SPEC v2 §5.11 today.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PullRequestConfig {
    /// Master switch. When `false` (the default), the orchestrator
    /// skips every PR side effect even if other fields are set.
    /// Pairs with the SPEC's "PR is optional" stance: integration
    /// can complete without a PR if the workflow omits this block.
    #[serde(default)]
    pub enabled: bool,

    /// Role name (key into [`WorkflowConfig::roles`]) responsible for
    /// opening, updating, and marking the PR ready. Optional at parse
    /// time so partial fixtures can declare an empty `pull_requests:`
    /// block; later phases enforce that the reference resolves to a
    /// role of `kind: integration_owner` whenever `enabled` is true.
    #[serde(default)]
    pub owner_role: Option<String>,

    /// Which forge adapter is responsible for the PR mutations.
    /// Defaults to [`PrProvider::Github`] — the only PR-capable
    /// adapter today. Later phases reject `enabled: true` when the
    /// configured tracker cannot satisfy this provider.
    #[serde(default)]
    pub provider: PrProvider,

    /// When the PR is first opened. Defaults to
    /// [`PrOpenStage::AfterIntegrationVerification`] per the SPEC's
    /// worked example. [`PrOpenStage::Manual`] disables auto-open
    /// without disabling the rest of the lifecycle (the integration
    /// owner can still update title/body and mark ready).
    #[serde(default)]
    pub open_stage: PrOpenStage,

    /// Initial PR state on creation. Defaults to
    /// [`PrInitialState::Draft`]: QA runs against the draft and the
    /// integration owner promotes it on `mark_ready_stage`.
    #[serde(default)]
    pub initial_state: PrInitialState,

    /// When the PR transitions from draft → ready for review.
    /// Defaults to [`PrMarkReadyStage::AfterQaPasses`] per the SPEC's
    /// worked example. [`PrMarkReadyStage::Manual`] keeps the PR in
    /// draft until an operator-driven transition.
    #[serde(default)]
    pub mark_ready_stage: PrMarkReadyStage,

    /// Templated PR title. `None` falls back to a built-in template
    /// at render time. Variable rendering is the strict
    /// `{{path.to.value}}` form introduced in CHECKLIST §7; unknown
    /// variables MUST fail the render rather than silently producing
    /// "{{...}}" in a real PR title.
    #[serde(default)]
    pub title_template: Option<String>,

    /// Templated PR body. Same rendering rules as `title_template`.
    /// The SPEC v2 §5.11 example uses `{{integration.summary}}`,
    /// `{{qa.status}}`, and `{{links}}`; the renderer surfaces those
    /// values from the integration record / QA verdict / linked
    /// work.
    #[serde(default)]
    pub body_template: Option<String>,

    /// When `true` (the default), the PR description includes
    /// machine-readable links to the originating tracker issues so
    /// the tracker auto-closes the parent on merge where supported.
    #[serde(default = "default_true")]
    pub link_tracker_issues: bool,

    /// When `true` (the default), the integration owner must wait
    /// for green CI before promoting a draft PR to ready for
    /// review. Pairs with `qa.evidence_required` (SPEC v2 §5.12)
    /// when QA also asserts CI status.
    #[serde(default = "default_true")]
    pub require_ci_green_before_ready: bool,
}

impl Default for PullRequestConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            owner_role: None,
            provider: PrProvider::default(),
            open_stage: PrOpenStage::default(),
            initial_state: PrInitialState::default(),
            mark_ready_stage: PrMarkReadyStage::default(),
            title_template: None,
            body_template: None,
            link_tracker_issues: true,
            require_ci_green_before_ready: true,
        }
    }
}

/// Forge adapter responsible for PR mutations (SPEC v2 §5.11
/// `provider`).
///
/// Today only GitHub supports the create/update/mark-ready/check
/// surface required by the SPEC. Other forges are rejected at
/// validation time so an enabled PR config does not silently drop
/// mutations on the floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrProvider {
    /// GitHub via the REST + GraphQL adapter. Default.
    #[default]
    Github,
}

/// When the PR is first opened (SPEC v2 §5.11 `open_stage`).
///
/// The default — [`PrOpenStage::AfterIntegrationVerification`] —
/// matches the SPEC's worked example: the integration owner opens
/// the PR only after consolidation has actually verified.
/// [`PrOpenStage::Manual`] is the operator escape hatch for
/// workflows that drive PR opening from outside the orchestrator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrOpenStage {
    /// Open the PR once the integration owner has produced an
    /// integration record (canonical branch verified, no open
    /// blockers). Default per SPEC v2 §5.11.
    #[default]
    AfterIntegrationVerification,
    /// The orchestrator never auto-opens the PR. The integration
    /// owner may still maintain it (update body, mark ready) once
    /// it exists.
    Manual,
}

/// Initial PR state on creation (SPEC v2 §5.11 `initial_state`).
///
/// Defaults to [`PrInitialState::Draft`] per the SPEC: QA runs
/// against drafts so a failing verdict never spams reviewers, and
/// the integration owner promotes after QA passes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrInitialState {
    /// Open as draft. Default per SPEC v2 §5.11.
    #[default]
    Draft,
    /// Open in ready-for-review state. Use only when QA is disabled
    /// or the workflow explicitly accepts reviewer pings on every
    /// integration handoff.
    ReadyForReview,
}

/// When a draft PR is promoted to ready for review (SPEC v2 §5.11
/// `mark_ready_stage`).
///
/// Defaults to [`PrMarkReadyStage::AfterQaPasses`] per the SPEC.
/// [`PrMarkReadyStage::Manual`] leaves promotion to an operator —
/// the integration owner still records the QA pass, but does not
/// flip the PR's draft bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrMarkReadyStage {
    /// Mark ready as soon as QA records a passing verdict against
    /// the draft branch. Default per SPEC v2 §5.11.
    #[default]
    AfterQaPasses,
    /// The orchestrator never auto-promotes. The integration owner
    /// or an operator marks the PR ready out-of-band.
    Manual,
}

// ---------------------------------------------------------------------------
// validation
// ---------------------------------------------------------------------------

/// Validation errors raised by [`WorkflowConfig::validate`].
///
/// These are the "value out of range" class of errors that serde cannot
/// catch on its own — for example, a `max_turns: 0` deserialises fine
/// but is semantically invalid per SPEC §5.3.5.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigValidationError {
    /// `schema_version` must equal [`SUPPORTED_SCHEMA_VERSION`]. We
    /// reject every other value (including `0`) so a stale or
    /// future-dated workflow file fails fast instead of being coerced.
    #[error("schema_version {0} is not supported (only {SUPPORTED_SCHEMA_VERSION} is accepted)")]
    UnsupportedSchemaVersion(u32),

    /// `agent.max_turns` must be a positive integer.
    #[error("agent.max_turns must be > 0 (got {0})")]
    MaxTurnsZero(u32),

    /// `agent.max_concurrent_agents` must be a positive integer.
    #[error("agent.max_concurrent_agents must be > 0 (got {0})")]
    MaxConcurrentZero(u32),

    /// `polling.interval_ms` must be > 0 — a zero interval would
    /// produce a busy-spin tick loop.
    #[error("polling.interval_ms must be > 0 (got {0})")]
    PollingIntervalZero(u64),

    /// `tracker.kind == Linear` requires `tracker.project_slug`.
    #[error("tracker.project_slug is required when tracker.kind = linear")]
    LinearMissingProjectSlug,

    /// `tracker.kind == Github` requires `tracker.repository`.
    #[error("tracker.repository is required when tracker.kind = github")]
    GithubMissingRepository,

    /// `tracker.kind == Mock` requires `tracker.fixtures`.
    #[error("tracker.fixtures is required when tracker.kind = mock")]
    MockMissingFixtures,

    /// `status.replay_buffer` must be > 0; `tokio::sync::broadcast`
    /// rejects zero-capacity channels at construction time.
    #[error("status.replay_buffer must be > 0 (got {0})")]
    StatusReplayBufferZero(usize),

    /// `status.bind` must be a parseable `SocketAddr`.
    #[error("status.bind is not a valid socket address: {0}")]
    StatusBindInvalid(String),
}

impl WorkflowConfig {
    /// Reject configs whose values parse but cannot be dispatched.
    ///
    /// Run this after deserialisation but before handing the config to
    /// the orchestrator. The CLI's `symphony validate` subcommand maps
    /// the resulting errors to non-zero exits.
    pub fn validate(&self) -> Result<(), ConfigValidationError> {
        if self.schema_version != SUPPORTED_SCHEMA_VERSION {
            return Err(ConfigValidationError::UnsupportedSchemaVersion(
                self.schema_version,
            ));
        }
        if self.agent.max_turns == 0 {
            return Err(ConfigValidationError::MaxTurnsZero(self.agent.max_turns));
        }
        if self.agent.max_concurrent_agents == 0 {
            return Err(ConfigValidationError::MaxConcurrentZero(
                self.agent.max_concurrent_agents,
            ));
        }
        if self.polling.interval_ms == 0 {
            return Err(ConfigValidationError::PollingIntervalZero(
                self.polling.interval_ms,
            ));
        }
        match self.tracker.kind {
            TrackerKind::Linear => {
                if self.tracker.project_slug.is_none() {
                    return Err(ConfigValidationError::LinearMissingProjectSlug);
                }
            }
            TrackerKind::Github => {
                if self.tracker.repository.is_none() {
                    return Err(ConfigValidationError::GithubMissingRepository);
                }
            }
            TrackerKind::Mock => {
                if self.tracker.fixtures.is_none() {
                    return Err(ConfigValidationError::MockMissingFixtures);
                }
            }
        }
        if self.status.enabled {
            if self.status.replay_buffer == 0 {
                return Err(ConfigValidationError::StatusReplayBufferZero(
                    self.status.replay_buffer,
                ));
            }
            // Parse-only; the runtime binds later. We surface the
            // failure at validate-time so `symphony validate` catches
            // it before the daemon is ever started.
            if self.status.bind.parse::<std::net::SocketAddr>().is_err() {
                return Err(ConfigValidationError::StatusBindInvalid(
                    self.status.bind.clone(),
                ));
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_schema_version_is_supported() {
        let cfg = WorkflowConfig::default();
        assert_eq!(cfg.schema_version, SUPPORTED_SCHEMA_VERSION);
    }

    #[test]
    fn schema_version_defaults_when_omitted() {
        let yaml = "tracker:\n  project_slug: ENG\n";
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        assert_eq!(parsed.schema_version, SUPPORTED_SCHEMA_VERSION);
        assert_eq!(parsed.validate(), Ok(()));
    }

    #[test]
    fn schema_version_explicit_one_is_accepted() {
        let yaml = "schema_version: 1\ntracker:\n  project_slug: ENG\n";
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        assert_eq!(parsed.schema_version, 1);
        assert_eq!(parsed.validate(), Ok(()));
    }

    #[test]
    fn schema_version_zero_is_rejected_by_validate() {
        let yaml = "schema_version: 0\ntracker:\n  project_slug: ENG\n";
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        assert_eq!(
            parsed.validate(),
            Err(ConfigValidationError::UnsupportedSchemaVersion(0))
        );
    }

    #[test]
    fn schema_version_future_value_is_rejected_by_validate() {
        let yaml = "schema_version: 2\ntracker:\n  project_slug: ENG\n";
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        assert_eq!(
            parsed.validate(),
            Err(ConfigValidationError::UnsupportedSchemaVersion(2))
        );
    }

    #[test]
    fn unknown_top_level_key_is_rejected() {
        // SPEC v2 §5: unknown top-level keys MUST be rejected.
        let yaml = "schema_version: 1\nroutes:\n  default: lead\n";
        let err = serde_yaml::from_str::<WorkflowConfig>(yaml).unwrap_err();
        assert!(
            err.to_string().contains("routes") || err.to_string().contains("unknown field"),
            "expected unknown-field error, got: {err}"
        );
    }

    #[test]
    fn schema_version_roundtrips() {
        let mut cfg = WorkflowConfig::default();
        cfg.tracker.project_slug = Some("ENG".into());
        let yaml = serde_yaml::to_string(&cfg).expect("serialises");
        assert!(
            yaml.contains("schema_version: 1"),
            "expected schema_version in serialised YAML, got: {yaml}"
        );
        let reparsed: WorkflowConfig = serde_yaml::from_str(&yaml).expect("re-parses");
        assert_eq!(reparsed.schema_version, 1);
        assert_eq!(reparsed, cfg);
    }

    #[test]
    fn defaults_match_spec_5_3() {
        // Mirrors SPEC §5.3 defaults explicitly so a future spec drift
        // forces the test to be updated alongside the constants.
        let cfg = WorkflowConfig::default();
        assert_eq!(cfg.tracker.kind, TrackerKind::Linear);
        assert_eq!(cfg.tracker.active_states, vec!["Todo", "In Progress"]);
        assert_eq!(
            cfg.tracker.terminal_states,
            vec!["Closed", "Cancelled", "Canceled", "Duplicate", "Done"]
        );
        assert_eq!(cfg.polling.interval_ms, 30_000);
        assert_eq!(cfg.polling.jitter_ms, 5_000);
        assert!(cfg.polling.startup_reconcile_recent_terminal);
        assert_eq!(cfg.hooks.timeout_ms, 60_000);
        assert_eq!(cfg.agent.kind, AgentKind::Codex);
        assert_eq!(cfg.agent.max_concurrent_agents, 10);
        assert_eq!(cfg.agent.max_turns, 20);
        assert_eq!(cfg.agent.max_retry_backoff_ms, 300_000);
        assert!(cfg.agent.max_concurrent_agents_by_state.is_empty());
        assert_eq!(cfg.codex.command, "codex app-server");
        assert_eq!(cfg.codex.turn_timeout_ms, 3_600_000);
        assert_eq!(cfg.codex.read_timeout_ms, 5_000);
        assert_eq!(cfg.codex.stall_timeout_ms, 300_000);
        assert!(cfg.status.enabled);
        assert_eq!(cfg.status.bind, "127.0.0.1:6280");
        assert_eq!(cfg.status.replay_buffer, 256);
    }

    #[test]
    fn yaml_roundtrip_preserves_overrides() {
        let yaml = r#"
tracker:
  kind: github
  repository: foglet-io/rust-symphony
  active_states: [open]
polling:
  interval_ms: 5000
agent:
  kind: claude
  max_turns: 7
  max_concurrent_agents_by_state:
    in_progress: 2
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        assert_eq!(parsed.tracker.kind, TrackerKind::Github);
        assert_eq!(
            parsed.tracker.repository.as_deref(),
            Some("foglet-io/rust-symphony")
        );
        assert_eq!(parsed.tracker.active_states, vec!["open"]);
        // Defaults still flow through for keys we did not override.
        assert_eq!(parsed.tracker.terminal_states, default_terminal_states());
        assert_eq!(parsed.polling.interval_ms, 5_000);
        assert_eq!(parsed.agent.kind, AgentKind::Claude);
        assert_eq!(parsed.agent.max_turns, 7);
        assert_eq!(
            parsed
                .agent
                .max_concurrent_agents_by_state
                .get("in_progress"),
            Some(&2)
        );

        let reserialised = serde_yaml::to_string(&parsed).expect("serialises");
        let reparsed: WorkflowConfig = serde_yaml::from_str(&reserialised).expect("re-parses");
        assert_eq!(parsed, reparsed);
    }

    #[test]
    fn unknown_nested_key_is_rejected() {
        // Forward-compat at the *top* level only. Typos inside known
        // sections (e.g. `interva_ms`) should not silently default.
        let yaml = r#"
polling:
  interva_ms: 10
"#;
        let err = serde_yaml::from_str::<WorkflowConfig>(yaml).unwrap_err();
        assert!(
            err.to_string().contains("interva_ms") || err.to_string().contains("unknown field"),
            "expected unknown-field error, got: {err}"
        );
    }

    #[test]
    fn validate_rejects_zero_max_turns() {
        let mut cfg = WorkflowConfig::default();
        cfg.tracker.project_slug = Some("ENG".into());
        cfg.agent.max_turns = 0;
        assert_eq!(cfg.validate(), Err(ConfigValidationError::MaxTurnsZero(0)));
    }

    #[test]
    fn validate_rejects_zero_concurrency() {
        let mut cfg = WorkflowConfig::default();
        cfg.tracker.project_slug = Some("ENG".into());
        cfg.agent.max_concurrent_agents = 0;
        assert_eq!(
            cfg.validate(),
            Err(ConfigValidationError::MaxConcurrentZero(0))
        );
    }

    #[test]
    fn validate_rejects_zero_poll_interval() {
        let mut cfg = WorkflowConfig::default();
        cfg.tracker.project_slug = Some("ENG".into());
        cfg.polling.interval_ms = 0;
        assert_eq!(
            cfg.validate(),
            Err(ConfigValidationError::PollingIntervalZero(0))
        );
    }

    #[test]
    fn polling_v2_fields_default_when_omitted() {
        // Operators who omit the new SPEC v2 §5.2 fields still get the
        // documented defaults so an existing v1-shaped fixture loads
        // without surprise.
        let yaml = "tracker:\n  project_slug: ENG\npolling:\n  interval_ms: 1000\n";
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        assert_eq!(parsed.polling.interval_ms, 1_000);
        assert_eq!(parsed.polling.jitter_ms, 5_000);
        assert!(parsed.polling.startup_reconcile_recent_terminal);
    }

    #[test]
    fn polling_v2_fields_roundtrip() {
        let yaml = r#"
tracker:
  project_slug: ENG
polling:
  interval_ms: 15000
  jitter_ms: 0
  startup_reconcile_recent_terminal: false
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        assert_eq!(parsed.polling.interval_ms, 15_000);
        assert_eq!(parsed.polling.jitter_ms, 0);
        assert!(!parsed.polling.startup_reconcile_recent_terminal);

        let reserialised = serde_yaml::to_string(&parsed).expect("serialises");
        let reparsed: WorkflowConfig = serde_yaml::from_str(&reserialised).expect("re-parses");
        assert_eq!(parsed, reparsed);
    }

    #[test]
    fn polling_unknown_nested_key_is_rejected() {
        // Typos in the polling section must surface, not silently
        // default — the rest of the schema enforces this and operators
        // should expect the same here.
        let yaml = r#"
tracker:
  project_slug: ENG
polling:
  jiter_ms: 100
"#;
        let err = serde_yaml::from_str::<WorkflowConfig>(yaml).unwrap_err();
        assert!(
            err.to_string().contains("jiter_ms") || err.to_string().contains("unknown field"),
            "expected unknown-field error, got: {err}"
        );
    }

    #[test]
    fn validate_requires_project_slug_for_linear() {
        let cfg = WorkflowConfig::default();
        assert_eq!(
            cfg.validate(),
            Err(ConfigValidationError::LinearMissingProjectSlug)
        );
    }

    #[test]
    fn validate_requires_repository_for_github() {
        let mut cfg = WorkflowConfig::default();
        cfg.tracker.kind = TrackerKind::Github;
        assert_eq!(
            cfg.validate(),
            Err(ConfigValidationError::GithubMissingRepository)
        );
    }

    #[test]
    fn validate_accepts_minimal_linear_config() {
        let mut cfg = WorkflowConfig::default();
        cfg.tracker.project_slug = Some("ENG".into());
        assert_eq!(cfg.validate(), Ok(()));
    }

    #[test]
    fn validate_requires_fixtures_for_mock() {
        let mut cfg = WorkflowConfig::default();
        cfg.tracker.kind = TrackerKind::Mock;
        assert_eq!(
            cfg.validate(),
            Err(ConfigValidationError::MockMissingFixtures)
        );
    }

    #[test]
    fn validate_accepts_minimal_mock_config() {
        let mut cfg = WorkflowConfig::default();
        cfg.tracker.kind = TrackerKind::Mock;
        cfg.tracker.fixtures = Some(PathBuf::from("issues.yaml"));
        assert_eq!(cfg.validate(), Ok(()));
    }

    #[test]
    fn yaml_roundtrip_preserves_mock_kind_and_fixtures() {
        let yaml = r#"
tracker:
  kind: mock
  fixtures: ./quickstart/issues.yaml
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        assert_eq!(parsed.tracker.kind, TrackerKind::Mock);
        assert_eq!(
            parsed.tracker.fixtures.as_deref(),
            Some(std::path::Path::new("./quickstart/issues.yaml"))
        );

        let reserialised = serde_yaml::to_string(&parsed).expect("serialises");
        let reparsed: WorkflowConfig = serde_yaml::from_str(&reserialised).expect("re-parses");
        assert_eq!(parsed, reparsed);
    }

    #[test]
    fn yaml_roundtrip_preserves_mock_agent_kind() {
        let yaml = r#"
tracker:
  kind: mock
  fixtures: ./issues.yaml
agent:
  kind: mock
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        assert_eq!(parsed.agent.kind, AgentKind::Mock);
        let reserialised = serde_yaml::to_string(&parsed).expect("serialises");
        let reparsed: WorkflowConfig = serde_yaml::from_str(&reserialised).expect("re-parses");
        assert_eq!(parsed, reparsed);
    }

    #[test]
    fn yaml_roundtrip_preserves_status_overrides() {
        let yaml = r#"
tracker:
  project_slug: ENG
status:
  enabled: false
  bind: 0.0.0.0:7000
  replay_buffer: 1024
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        assert!(!parsed.status.enabled);
        assert_eq!(parsed.status.bind, "0.0.0.0:7000");
        assert_eq!(parsed.status.replay_buffer, 1024);

        let reserialised = serde_yaml::to_string(&parsed).expect("serialises");
        let reparsed: WorkflowConfig = serde_yaml::from_str(&reserialised).expect("re-parses");
        assert_eq!(parsed, reparsed);
    }

    #[test]
    fn status_unknown_nested_key_is_rejected() {
        // Mirrors the polling test: typos inside the status section
        // must not silently default — operators expect feedback.
        let yaml = r#"
tracker:
  project_slug: ENG
status:
  enabld: true
"#;
        let err = serde_yaml::from_str::<WorkflowConfig>(yaml).unwrap_err();
        assert!(
            err.to_string().contains("enabld") || err.to_string().contains("unknown field"),
            "expected unknown-field error, got: {err}"
        );
    }

    #[test]
    fn validate_rejects_zero_replay_buffer_when_status_enabled() {
        let mut cfg = WorkflowConfig::default();
        cfg.tracker.project_slug = Some("ENG".into());
        cfg.status.replay_buffer = 0;
        assert_eq!(
            cfg.validate(),
            Err(ConfigValidationError::StatusReplayBufferZero(0))
        );
    }

    #[test]
    fn validate_ignores_zero_replay_buffer_when_status_disabled() {
        // A disabled status surface never constructs the broadcast
        // channel, so a zero buffer is harmless. Surfacing it as an
        // error would force operators to delete the (otherwise
        // illustrative) value just to start the daemon with status off.
        let mut cfg = WorkflowConfig::default();
        cfg.tracker.project_slug = Some("ENG".into());
        cfg.status.enabled = false;
        cfg.status.replay_buffer = 0;
        assert_eq!(cfg.validate(), Ok(()));
    }

    #[test]
    fn validate_rejects_unparseable_bind() {
        let mut cfg = WorkflowConfig::default();
        cfg.tracker.project_slug = Some("ENG".into());
        cfg.status.bind = "not-a-socket".into();
        assert_eq!(
            cfg.validate(),
            Err(ConfigValidationError::StatusBindInvalid(
                "not-a-socket".into()
            ))
        );
    }

    #[test]
    fn validate_accepts_minimal_github_config() {
        let mut cfg = WorkflowConfig::default();
        cfg.tracker.kind = TrackerKind::Github;
        cfg.tracker.repository = Some("foglet-io/rust-symphony".into());
        assert_eq!(cfg.validate(), Ok(()));
    }

    // -----------------------------------------------------------------
    // roles (SPEC v2 §5.4)
    // -----------------------------------------------------------------

    #[test]
    fn roles_default_is_empty() {
        let cfg = WorkflowConfig::default();
        assert!(cfg.roles.is_empty());
    }

    #[test]
    fn roles_yaml_parses_all_kinds() {
        // Mirrors the SPEC §5.4 worked example: integration owner, QA
        // gate, and a configurable specialist.
        let yaml = r#"
tracker:
  project_slug: ENG
roles:
  platform_lead:
    kind: integration_owner
    description: Owns decomposition and integration.
    agent: lead_agent
    max_concurrent: 1
    can_decompose: true
    can_assign: true
    can_request_qa: true
    can_close_parent: true
  qa:
    kind: qa_gate
    agent: qa_agent
    max_concurrent: 2
    can_file_blockers: true
    can_file_followups: true
    required_for_done: true
  backend_engineer:
    kind: specialist
    agent: codex_fast
  reviewer:
    kind: reviewer
  ops:
    kind: operator
  custom_role:
    kind: custom
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        assert_eq!(parsed.roles.len(), 6);

        let lead = parsed.roles.get("platform_lead").expect("lead present");
        assert_eq!(lead.kind, RoleKind::IntegrationOwner);
        assert_eq!(lead.agent.as_deref(), Some("lead_agent"));
        assert_eq!(lead.max_concurrent, Some(1));
        assert_eq!(lead.can_decompose, Some(true));
        assert_eq!(lead.can_close_parent, Some(true));
        // QA-only flags stay None when unset, distinct from `Some(false)`.
        assert_eq!(lead.can_file_blockers, None);

        let qa = parsed.roles.get("qa").expect("qa present");
        assert_eq!(qa.kind, RoleKind::QaGate);
        assert_eq!(qa.required_for_done, Some(true));

        assert_eq!(
            parsed.roles.get("backend_engineer").map(|r| r.kind),
            Some(RoleKind::Specialist)
        );
        assert_eq!(
            parsed.roles.get("reviewer").map(|r| r.kind),
            Some(RoleKind::Reviewer)
        );
        assert_eq!(
            parsed.roles.get("ops").map(|r| r.kind),
            Some(RoleKind::Operator)
        );
        assert_eq!(
            parsed.roles.get("custom_role").map(|r| r.kind),
            Some(RoleKind::Custom)
        );
    }

    #[test]
    fn role_kind_serialises_snake_case() {
        // Each variant should round-trip through the snake_case YAML
        // rendering operators write by hand.
        for (variant, literal) in [
            (RoleKind::IntegrationOwner, "integration_owner"),
            (RoleKind::QaGate, "qa_gate"),
            (RoleKind::Specialist, "specialist"),
            (RoleKind::Reviewer, "reviewer"),
            (RoleKind::Operator, "operator"),
            (RoleKind::Custom, "custom"),
        ] {
            let yaml = serde_yaml::to_string(&variant).expect("serialises");
            assert!(
                yaml.contains(literal),
                "expected {literal} in {yaml:?} for {variant:?}"
            );
            let reparsed: RoleKind = serde_yaml::from_str(&yaml).expect("re-parses");
            assert_eq!(reparsed, variant);
        }
    }

    #[test]
    fn role_kind_rejects_unknown_value() {
        let yaml = r#"
tracker:
  project_slug: ENG
roles:
  bogus:
    kind: wizard
"#;
        let err = serde_yaml::from_str::<WorkflowConfig>(yaml).unwrap_err();
        assert!(
            err.to_string().contains("wizard") || err.to_string().contains("unknown variant"),
            "expected unknown-variant error, got: {err}"
        );
    }

    #[test]
    fn role_unknown_field_is_rejected() {
        // Typos like `descritpion` must surface, not silently default —
        // they would otherwise hide a misconfigured authority flag.
        let yaml = r#"
tracker:
  project_slug: ENG
roles:
  qa:
    kind: qa_gate
    descritpion: typo
"#;
        let err = serde_yaml::from_str::<WorkflowConfig>(yaml).unwrap_err();
        assert!(
            err.to_string().contains("descritpion") || err.to_string().contains("unknown field"),
            "expected unknown-field error, got: {err}"
        );
    }

    #[test]
    fn role_kind_is_required() {
        let yaml = r#"
tracker:
  project_slug: ENG
roles:
  qa:
    description: missing kind
"#;
        let err = serde_yaml::from_str::<WorkflowConfig>(yaml).unwrap_err();
        assert!(
            err.to_string().contains("kind") || err.to_string().contains("missing field"),
            "expected missing-field error for kind, got: {err}"
        );
    }

    // -----------------------------------------------------------------
    // agents (SPEC v2 §5.5)
    // -----------------------------------------------------------------

    #[test]
    fn agents_default_is_empty() {
        let cfg = WorkflowConfig::default();
        assert!(cfg.agents.is_empty());
    }

    #[test]
    fn agents_yaml_parses_all_backends() {
        let yaml = r#"
tracker:
  project_slug: ENG
agents:
  lead_agent:
    backend: codex
    description: Integration owner backend.
    command: codex app-server
    model: o4
    tools: [git, github, tracker]
    memory: persistent
    max_turns: 30
    turn_timeout_ms: 1800000
    token_budget: 1000000
    cost_budget_usd: 12.5
    env:
      RUST_LOG: info
  qa_agent:
    backend: claude
    command: claude -p --output-format stream-json
    tools: [git, github]
  hermes_agent:
    backend: hermes
  fixture_agent:
    backend: mock
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        assert_eq!(parsed.agents.len(), 4);

        let lead = parsed
            .agents
            .get("lead_agent")
            .and_then(|a| a.as_backend())
            .expect("lead_agent is a backend profile");
        assert_eq!(lead.backend, AgentBackend::Codex);
        assert_eq!(lead.command.as_deref(), Some("codex app-server"));
        assert_eq!(lead.model.as_deref(), Some("o4"));
        assert_eq!(lead.tools, vec!["git", "github", "tracker"]);
        assert_eq!(lead.memory.as_deref(), Some("persistent"));
        assert_eq!(lead.max_turns, Some(30));
        assert_eq!(lead.turn_timeout_ms, Some(1_800_000));
        assert_eq!(lead.token_budget, Some(1_000_000));
        assert_eq!(lead.cost_budget_usd, Some(12.5));
        assert_eq!(lead.env.get("RUST_LOG").map(String::as_str), Some("info"));

        assert_eq!(
            parsed.agents.get("qa_agent").and_then(|a| a.backend()),
            Some(AgentBackend::Claude)
        );
        assert_eq!(
            parsed.agents.get("hermes_agent").and_then(|a| a.backend()),
            Some(AgentBackend::Hermes)
        );
        assert_eq!(
            parsed.agents.get("fixture_agent").and_then(|a| a.backend()),
            Some(AgentBackend::Mock)
        );
    }

    #[test]
    fn agent_backend_serialises_lowercase() {
        for (variant, literal) in [
            (AgentBackend::Codex, "codex"),
            (AgentBackend::Claude, "claude"),
            (AgentBackend::Hermes, "hermes"),
            (AgentBackend::Mock, "mock"),
        ] {
            let yaml = serde_yaml::to_string(&variant).expect("serialises");
            assert!(
                yaml.contains(literal),
                "expected {literal} in {yaml:?} for {variant:?}"
            );
            let reparsed: AgentBackend = serde_yaml::from_str(&yaml).expect("re-parses");
            assert_eq!(reparsed, variant);
        }
    }

    #[test]
    fn agent_backend_rejects_unknown_value() {
        // An unknown `backend:` value can't be a backend profile (unknown
        // variant) and can't be a composite either (unknown `backend`
        // field, missing `strategy`). Both arms of the untagged enum must
        // reject; the loader surfaces an error.
        let yaml = r#"
tracker:
  project_slug: ENG
agents:
  bogus:
    backend: gpt5
"#;
        serde_yaml::from_str::<WorkflowConfig>(yaml)
            .expect_err("unknown backend value must be rejected");
    }

    #[test]
    fn agent_unknown_field_is_rejected() {
        // `deny_unknown_fields` on the backend variant must still fire
        // through the untagged enum: a typo'd field can't sneak through
        // by being mis-classified as a composite (which also rejects
        // the unknown field and is missing `strategy`).
        let yaml = r#"
tracker:
  project_slug: ENG
agents:
  lead_agent:
    backend: codex
    descritpion: typo
"#;
        serde_yaml::from_str::<WorkflowConfig>(yaml)
            .expect_err("typo'd field must be rejected by deny_unknown_fields");
    }

    #[test]
    fn agent_profile_requires_discriminator() {
        // A profile must declare either `backend` (concrete) or
        // `strategy` (composite). Neither: both arms fail and the
        // untagged enum surfaces an error.
        let yaml = r#"
tracker:
  project_slug: ENG
agents:
  bare:
    description: missing both backend and strategy
"#;
        serde_yaml::from_str::<WorkflowConfig>(yaml)
            .expect_err("profile without backend or strategy must be rejected");
    }

    #[test]
    fn agents_yaml_roundtrip_preserves_entries() {
        let yaml = r#"
tracker:
  project_slug: ENG
agents:
  lead_agent:
    backend: codex
    command: codex app-server
    tools: [git, github]
    env:
      RUST_LOG: info
  qa_agent:
    backend: claude
    max_turns: 12
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        let reserialised = serde_yaml::to_string(&parsed).expect("serialises");
        let reparsed: WorkflowConfig = serde_yaml::from_str(&reserialised).expect("re-parses");
        assert_eq!(parsed, reparsed);
        assert_eq!(parsed.agents.len(), 2);
    }

    #[test]
    fn agent_profile_defaults_to_inherit() {
        // A minimal profile leaves every policy field at None / empty so
        // dispatch-time resolution can fall back to global defaults
        // without needing to distinguish "operator wrote nothing" from
        // "operator wrote the same default".
        let yaml = r#"
tracker:
  project_slug: ENG
agents:
  minimal:
    backend: codex
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        let p = parsed
            .agents
            .get("minimal")
            .and_then(|a| a.as_backend())
            .expect("minimal is a backend profile");
        assert_eq!(p.backend, AgentBackend::Codex);
        assert!(p.description.is_none());
        assert!(p.command.is_none());
        assert!(p.model.is_none());
        assert!(p.system_prompt.is_none());
        assert!(p.tools.is_empty());
        assert!(p.env.is_empty());
        assert!(p.memory.is_none());
        assert!(p.approval_policy.is_none());
        assert!(p.sandbox_policy.is_none());
        assert!(p.max_turns.is_none());
        assert!(p.turn_timeout_ms.is_none());
        assert!(p.stall_timeout_ms.is_none());
        assert!(p.token_budget.is_none());
        assert!(p.cost_budget_usd.is_none());
    }

    #[test]
    fn agents_keyspace_is_independent_of_role_names() {
        // SPEC v2 §5.5: profiles live in their own keyspace. Two roles
        // can reference one profile, and a profile name need not match
        // any role name. The schema must accept that without coupling
        // the two maps.
        let yaml = r#"
tracker:
  project_slug: ENG
roles:
  platform_lead:
    kind: integration_owner
    agent: shared_codex
  backend_engineer:
    kind: specialist
    agent: shared_codex
agents:
  shared_codex:
    backend: codex
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        assert_eq!(parsed.roles.len(), 2);
        assert_eq!(parsed.agents.len(), 1);
        for role in parsed.roles.values() {
            assert_eq!(role.agent.as_deref(), Some("shared_codex"));
        }
        // The profile name is *not* a role name — that's the point.
        assert!(!parsed.roles.contains_key("shared_codex"));
    }

    #[test]
    fn roles_yaml_roundtrip_preserves_entries() {
        let yaml = r#"
tracker:
  project_slug: ENG
roles:
  platform_lead:
    kind: integration_owner
    agent: lead_agent
    max_concurrent: 1
    can_decompose: true
  qa:
    kind: qa_gate
    required_for_done: true
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        let reserialised = serde_yaml::to_string(&parsed).expect("serialises");
        let reparsed: WorkflowConfig = serde_yaml::from_str(&reserialised).expect("re-parses");
        assert_eq!(parsed, reparsed);
        assert_eq!(parsed.roles.len(), 2);
    }

    // -----------------------------------------------------------------
    // hermes (SPEC v2 §5.5)
    // -----------------------------------------------------------------

    #[test]
    fn hermes_defaults_match_spec_example() {
        let cfg = WorkflowConfig::default();
        assert_eq!(
            cfg.hermes.command,
            "hermes chat --query-file - --source symphony"
        );
        assert_eq!(cfg.hermes.source, "symphony");
        assert_eq!(cfg.hermes.turn_timeout_ms, 3_600_000);
        assert_eq!(cfg.hermes.read_timeout_ms, 5_000);
        assert_eq!(cfg.hermes.stall_timeout_ms, 300_000);
    }

    #[test]
    fn hermes_yaml_block_overrides_defaults() {
        let yaml = r#"
tracker:
  project_slug: ENG
hermes:
  command: hermes chat --source ci
  source: ci
  turn_timeout_ms: 600000
  read_timeout_ms: 1000
  stall_timeout_ms: 0
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        assert_eq!(parsed.hermes.command, "hermes chat --source ci");
        assert_eq!(parsed.hermes.source, "ci");
        assert_eq!(parsed.hermes.turn_timeout_ms, 600_000);
        assert_eq!(parsed.hermes.read_timeout_ms, 1_000);
        assert_eq!(parsed.hermes.stall_timeout_ms, 0);
    }

    #[test]
    fn hermes_unknown_field_is_rejected() {
        let yaml = r#"
tracker:
  project_slug: ENG
hermes:
  comand: typo
"#;
        let err = serde_yaml::from_str::<WorkflowConfig>(yaml).unwrap_err();
        assert!(
            err.to_string().contains("comand") || err.to_string().contains("unknown field"),
            "expected unknown-field error, got: {err}"
        );
    }

    // -----------------------------------------------------------------
    // composite agents (SPEC v2 §5.5 — `strategy: tandem`)
    // -----------------------------------------------------------------

    #[test]
    fn composite_tandem_profile_parses() {
        // The SPEC example: a composite profile composes two named
        // backend profiles into a tandem pair with an explicit mode.
        let yaml = r#"
tracker:
  project_slug: ENG
agents:
  lead_agent:
    backend: codex
  qa_agent:
    backend: claude
  lead_pair:
    strategy: tandem
    lead: lead_agent
    follower: qa_agent
    mode: draft_review
    description: Codex drafts, Claude reviews.
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        assert_eq!(parsed.agents.len(), 3);

        // Backend variants still resolve via the accessor.
        assert_eq!(
            parsed.agents.get("lead_agent").and_then(|a| a.backend()),
            Some(AgentBackend::Codex)
        );

        // The composite variant exposes lead/follower/mode without
        // claiming a backend tag of its own.
        let pair = parsed
            .agents
            .get("lead_pair")
            .and_then(|a| a.as_composite())
            .expect("lead_pair is a composite profile");
        assert_eq!(pair.strategy, AgentStrategy::Tandem);
        assert_eq!(pair.lead, "lead_agent");
        assert_eq!(pair.follower, "qa_agent");
        assert_eq!(pair.mode, TandemMode::DraftReview);
        assert_eq!(
            pair.description.as_deref(),
            Some("Codex drafts, Claude reviews.")
        );

        // A composite has no concrete backend.
        assert!(parsed.agents.get("lead_pair").unwrap().backend().is_none());
    }

    #[test]
    fn composite_tandem_modes_all_parse() {
        // Each tandem mode round-trips through the schema. Guards
        // against drift between the YAML literals and the in-tree
        // tandem runner enum.
        for (literal, mode) in [
            ("draft_review", TandemMode::DraftReview),
            ("split_implement", TandemMode::SplitImplement),
            ("consensus", TandemMode::Consensus),
        ] {
            let yaml = format!(
                r#"
tracker:
  project_slug: ENG
agents:
  pair:
    strategy: tandem
    lead: a
    follower: b
    mode: {literal}
"#
            );
            let parsed: WorkflowConfig = serde_yaml::from_str(&yaml).expect("yaml parses");
            let composite = parsed
                .agents
                .get("pair")
                .and_then(|a| a.as_composite())
                .expect("pair is a composite profile");
            assert_eq!(
                composite.mode, mode,
                "mode literal {literal} must round-trip"
            );
        }
    }

    #[test]
    fn composite_unknown_field_is_rejected() {
        // Typos inside a composite must surface; deny_unknown_fields
        // applies on the inner struct even through the untagged enum.
        let yaml = r#"
tracker:
  project_slug: ENG
agents:
  pair:
    strategy: tandem
    lead: a
    follower: b
    mode: draft_review
    leed: typo
"#;
        serde_yaml::from_str::<WorkflowConfig>(yaml).expect_err("composite typo must be rejected");
    }

    #[test]
    fn composite_unknown_strategy_is_rejected() {
        // Today only `tandem` is defined. An unknown strategy must
        // fail-loud rather than silently fall back.
        let yaml = r#"
tracker:
  project_slug: ENG
agents:
  pair:
    strategy: voting
    lead: a
    follower: b
    mode: consensus
"#;
        serde_yaml::from_str::<WorkflowConfig>(yaml)
            .expect_err("unknown strategy must be rejected");
    }

    #[test]
    fn composite_unknown_mode_is_rejected() {
        let yaml = r#"
tracker:
  project_slug: ENG
agents:
  pair:
    strategy: tandem
    lead: a
    follower: b
    mode: rotate
"#;
        serde_yaml::from_str::<WorkflowConfig>(yaml)
            .expect_err("unknown tandem mode must be rejected");
    }

    #[test]
    fn composite_round_trips_through_yaml() {
        let yaml = r#"
tracker:
  project_slug: ENG
agents:
  lead_agent:
    backend: codex
  follower_agent:
    backend: claude
  lead_pair:
    strategy: tandem
    lead: lead_agent
    follower: follower_agent
    mode: split_implement
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        let reserialised = serde_yaml::to_string(&parsed).expect("serialises");
        let reparsed: WorkflowConfig = serde_yaml::from_str(&reserialised).expect("re-parses");
        assert_eq!(parsed, reparsed);
        // Sanity: composite survives the round-trip with its strategy
        // tag and mode intact.
        let pair = reparsed
            .agents
            .get("lead_pair")
            .and_then(|a| a.as_composite())
            .expect("composite present after round-trip");
        assert_eq!(pair.strategy, AgentStrategy::Tandem);
        assert_eq!(pair.mode, TandemMode::SplitImplement);
    }

    // -----------------------------------------------------------------
    // routing (SPEC v2 §5.6)
    // -----------------------------------------------------------------

    #[test]
    fn routing_default_is_empty_first_match() {
        let cfg = WorkflowConfig::default();
        assert_eq!(cfg.routing.match_mode, RoutingMatchMode::FirstMatch);
        assert!(cfg.routing.default_role.is_none());
        assert!(cfg.routing.rules.is_empty());
    }

    #[test]
    fn routing_first_match_yaml_parses() {
        // Mirrors the quickstart fixture's `match_mode: first_match`
        // shape: rules without explicit `priority` are legal because
        // first-match ignores the field.
        let yaml = r#"
tracker:
  project_slug: ENG
routing:
  default_role: worker
  match_mode: first_match
  rules:
    - when:
        labels_any: [qa]
      assign_role: qa
    - when:
        labels_any: [epic, broad]
      assign_role: platform_lead
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        assert_eq!(parsed.routing.match_mode, RoutingMatchMode::FirstMatch);
        assert_eq!(parsed.routing.default_role.as_deref(), Some("worker"));
        assert_eq!(parsed.routing.rules.len(), 2);

        let first = &parsed.routing.rules[0];
        assert_eq!(first.priority, None);
        assert_eq!(first.when.labels_any, vec!["qa".to_string()]);
        assert!(first.when.paths_any.is_empty());
        assert_eq!(first.assign_role, "qa");

        let second = &parsed.routing.rules[1];
        assert_eq!(
            second.when.labels_any,
            vec!["epic".to_string(), "broad".to_string()]
        );
        assert_eq!(second.assign_role, "platform_lead");
    }

    #[test]
    fn routing_priority_yaml_parses_with_when_predicates() {
        // Mirrors SPEC §5.6's worked example: priority mode with
        // labels_any, paths_any, and issue_size predicates.
        let yaml = r#"
tracker:
  project_slug: ENG
routing:
  default_role: platform_lead
  match_mode: priority
  rules:
    - priority: 100
      when:
        labels_any: [qa]
      assign_role: qa
    - priority: 50
      when:
        paths_any: ["lib/**", "test/**"]
      assign_role: backend_engineer
    - priority: 10
      when:
        issue_size: broad
      assign_role: platform_lead
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        assert_eq!(parsed.routing.match_mode, RoutingMatchMode::Priority);
        assert_eq!(parsed.routing.rules.len(), 3);

        assert_eq!(parsed.routing.rules[0].priority, Some(100));
        assert_eq!(parsed.routing.rules[1].priority, Some(50));
        assert_eq!(
            parsed.routing.rules[1].when.paths_any,
            vec!["lib/**".to_string(), "test/**".to_string()]
        );
        assert_eq!(
            parsed.routing.rules[2].when.issue_size.as_deref(),
            Some("broad")
        );
    }

    #[test]
    fn routing_match_mode_serialises_snake_case() {
        for (variant, literal) in [
            (RoutingMatchMode::FirstMatch, "first_match"),
            (RoutingMatchMode::Priority, "priority"),
        ] {
            let yaml = serde_yaml::to_string(&variant).expect("serialises");
            assert!(
                yaml.contains(literal),
                "expected {literal} in {yaml:?} for {variant:?}"
            );
            let reparsed: RoutingMatchMode = serde_yaml::from_str(&yaml).expect("re-parses");
            assert_eq!(reparsed, variant);
        }
    }

    #[test]
    fn routing_rejects_unknown_match_mode() {
        // SPEC v2 §5.6 enumerates exactly two modes; any third value
        // must fail load loudly rather than silently degrading.
        let yaml = r#"
tracker:
  project_slug: ENG
routing:
  match_mode: round_robin
"#;
        let err = serde_yaml::from_str::<WorkflowConfig>(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("round_robin")
                || msg.contains("unknown variant")
                || msg.contains("first_match"),
            "expected unknown-variant error, got: {msg}"
        );
    }

    #[test]
    fn routing_rejects_unknown_when_key() {
        // Unknown keys inside `when:` must be rejected so a typo like
        // `lables_any` does not silently produce a no-op predicate.
        let yaml = r#"
tracker:
  project_slug: ENG
routing:
  rules:
    - when:
        lables_any: [qa]
      assign_role: qa
"#;
        let err = serde_yaml::from_str::<WorkflowConfig>(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("lables_any") || msg.contains("unknown field"),
            "expected unknown-field error, got: {msg}"
        );
    }

    #[test]
    fn routing_rule_requires_assign_role() {
        // `assign_role` is required — a rule without a destination is
        // meaningless and should fail at parse time.
        let yaml = r#"
tracker:
  project_slug: ENG
routing:
  rules:
    - when:
        labels_any: [qa]
"#;
        let err = serde_yaml::from_str::<WorkflowConfig>(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("assign_role") || msg.contains("missing field"),
            "expected missing-field error for assign_role, got: {msg}"
        );
    }

    #[test]
    fn decomposition_default_is_disabled() {
        let cfg = WorkflowConfig::default();
        assert!(!cfg.decomposition.enabled);
        assert!(cfg.decomposition.owner_role.is_none());
        assert!(cfg.decomposition.triggers.labels_any.is_empty());
        assert!(cfg.decomposition.triggers.estimated_files_over.is_none());
        assert!(cfg.decomposition.triggers.acceptance_items_over.is_none());
        assert_eq!(
            cfg.decomposition.child_issue_policy,
            ChildIssuePolicy::ProposeForApproval
        );
        assert_eq!(cfg.decomposition.max_depth, 2);
        assert!(cfg.decomposition.require_acceptance_criteria_per_child);
    }

    #[test]
    fn decomposition_partial_block_inherits_documented_defaults() {
        // An operator who writes only `enabled: true` should still
        // get max_depth=2, require_acceptance_criteria_per_child=true,
        // and the safer propose_for_approval policy.
        let yaml = r#"
tracker:
  project_slug: ENG
decomposition:
  enabled: true
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        assert!(parsed.decomposition.enabled);
        assert_eq!(parsed.decomposition.max_depth, 2);
        assert!(parsed.decomposition.require_acceptance_criteria_per_child);
        assert_eq!(
            parsed.decomposition.child_issue_policy,
            ChildIssuePolicy::ProposeForApproval
        );
    }

    #[test]
    fn decomposition_full_block_parses_spec_example() {
        // Mirrors the SPEC v2 §5.7 worked example verbatim.
        let yaml = r#"
tracker:
  project_slug: ENG
decomposition:
  enabled: true
  owner_role: platform_lead
  triggers:
    labels_any: [epic, broad, umbrella]
    estimated_files_over: 5
    acceptance_items_over: 3
  child_issue_policy: create_directly
  max_depth: 2
  require_acceptance_criteria_per_child: true
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        let d = &parsed.decomposition;
        assert!(d.enabled);
        assert_eq!(d.owner_role.as_deref(), Some("platform_lead"));
        assert_eq!(
            d.triggers.labels_any,
            vec![
                "epic".to_string(),
                "broad".to_string(),
                "umbrella".to_string()
            ]
        );
        assert_eq!(d.triggers.estimated_files_over, Some(5));
        assert_eq!(d.triggers.acceptance_items_over, Some(3));
        assert_eq!(d.child_issue_policy, ChildIssuePolicy::CreateDirectly);
        assert_eq!(d.max_depth, 2);
        assert!(d.require_acceptance_criteria_per_child);
    }

    #[test]
    fn decomposition_child_issue_policy_serialises_snake_case() {
        for (variant, literal) in [
            (ChildIssuePolicy::CreateDirectly, "create_directly"),
            (ChildIssuePolicy::ProposeForApproval, "propose_for_approval"),
        ] {
            let yaml = serde_yaml::to_string(&variant).expect("serialises");
            assert!(
                yaml.contains(literal),
                "expected {literal} in {yaml:?} for {variant:?}"
            );
            let reparsed: ChildIssuePolicy = serde_yaml::from_str(&yaml).expect("re-parses");
            assert_eq!(reparsed, variant);
        }
    }

    #[test]
    fn decomposition_rejects_unknown_child_issue_policy() {
        let yaml = r#"
tracker:
  project_slug: ENG
decomposition:
  child_issue_policy: file_directly
"#;
        let err = serde_yaml::from_str::<WorkflowConfig>(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("file_directly")
                || msg.contains("unknown variant")
                || msg.contains("create_directly"),
            "expected unknown-variant error, got: {msg}"
        );
    }

    #[test]
    fn decomposition_rejects_unknown_trigger_key() {
        // Typos inside `triggers:` must surface so `lables_any` does
        // not silently produce a no-op predicate.
        let yaml = r#"
tracker:
  project_slug: ENG
decomposition:
  triggers:
    lables_any: [epic]
"#;
        let err = serde_yaml::from_str::<WorkflowConfig>(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("lables_any") || msg.contains("unknown field"),
            "expected unknown-field error, got: {msg}"
        );
    }

    #[test]
    fn decomposition_rejects_unknown_top_level_key() {
        let yaml = r#"
tracker:
  project_slug: ENG
decomposition:
  enable: true
"#;
        let err = serde_yaml::from_str::<WorkflowConfig>(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("enable") || msg.contains("unknown field"),
            "expected unknown-field error, got: {msg}"
        );
    }

    #[test]
    fn decomposition_round_trips_through_yaml() {
        let yaml = r#"
tracker:
  project_slug: ENG
decomposition:
  enabled: true
  owner_role: platform_lead
  triggers:
    labels_any: [epic]
    acceptance_items_over: 4
  child_issue_policy: propose_for_approval
  max_depth: 3
  require_acceptance_criteria_per_child: false
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        let reserialised = serde_yaml::to_string(&parsed).expect("serialises");
        let reparsed: WorkflowConfig = serde_yaml::from_str(&reserialised).expect("re-parses");
        assert_eq!(parsed, reparsed);
    }

    #[test]
    fn integration_block_defaults_when_omitted() {
        // SPEC v2 §5.10 defaults: an omitted `integration:` block must
        // produce the safe defaults (no owner, empty requirements,
        // sequential cherry-pick, integration-owner repair turn, all
        // gates on, no next-state).
        let cfg = WorkflowConfig::default();
        assert!(cfg.integration.owner_role.is_none());
        assert!(cfg.integration.required_for.is_empty());
        assert_eq!(
            cfg.integration.merge_strategy,
            MergeStrategy::SequentialCherryPick
        );
        assert_eq!(
            cfg.integration.conflict_policy,
            ConflictPolicy::IntegrationOwnerRepairTurn
        );
        assert!(cfg.integration.require_all_children_terminal);
        assert!(cfg.integration.require_no_open_blockers);
        assert!(cfg.integration.require_integration_summary);
        assert!(cfg.integration.next_state_after_integration.is_none());
    }

    #[test]
    fn integration_partial_block_inherits_documented_defaults() {
        // An operator who writes only `owner_role` should still get the
        // SPEC defaults for merge/conflict policy and the three gates.
        let yaml = r#"
tracker:
  project_slug: ENG
integration:
  owner_role: platform_lead
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        let i = &parsed.integration;
        assert_eq!(i.owner_role.as_deref(), Some("platform_lead"));
        assert!(i.required_for.is_empty());
        assert_eq!(i.merge_strategy, MergeStrategy::SequentialCherryPick);
        assert_eq!(
            i.conflict_policy,
            ConflictPolicy::IntegrationOwnerRepairTurn
        );
        assert!(i.require_all_children_terminal);
        assert!(i.require_no_open_blockers);
        assert!(i.require_integration_summary);
    }

    #[test]
    fn integration_full_block_parses_spec_example() {
        // Mirrors the SPEC v2 §5.10 worked example verbatim.
        let yaml = r#"
tracker:
  project_slug: ENG
integration:
  owner_role: platform_lead
  required_for:
    - decomposed_parent
    - multiple_child_branches
  merge_strategy: sequential_cherry_pick
  conflict_policy: integration_owner_repair_turn
  require_all_children_terminal: true
  require_no_open_blockers: true
  require_integration_summary: true
  next_state_after_integration: qa
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        let i = &parsed.integration;
        assert_eq!(i.owner_role.as_deref(), Some("platform_lead"));
        assert_eq!(
            i.required_for,
            vec![
                IntegrationRequirement::DecomposedParent,
                IntegrationRequirement::MultipleChildBranches,
            ]
        );
        assert_eq!(i.merge_strategy, MergeStrategy::SequentialCherryPick);
        assert_eq!(
            i.conflict_policy,
            ConflictPolicy::IntegrationOwnerRepairTurn
        );
        assert!(i.require_all_children_terminal);
        assert!(i.require_no_open_blockers);
        assert!(i.require_integration_summary);
        assert_eq!(i.next_state_after_integration.as_deref(), Some("qa"));
    }

    #[test]
    fn integration_merge_strategy_serialises_snake_case() {
        for (variant, literal) in [
            (
                MergeStrategy::SequentialCherryPick,
                "sequential_cherry_pick",
            ),
            (MergeStrategy::MergeCommits, "merge_commits"),
            (MergeStrategy::SharedBranch, "shared_branch"),
        ] {
            let yaml = serde_yaml::to_string(&variant).expect("serialises");
            assert!(
                yaml.contains(literal),
                "expected {literal} in {yaml:?} for {variant:?}"
            );
            let reparsed: MergeStrategy = serde_yaml::from_str(&yaml).expect("re-parses");
            assert_eq!(reparsed, variant);
        }
    }

    #[test]
    fn integration_conflict_policy_serialises_snake_case() {
        for (variant, literal) in [
            (
                ConflictPolicy::IntegrationOwnerRepairTurn,
                "integration_owner_repair_turn",
            ),
            (ConflictPolicy::RouteToChildOwner, "route_to_child_owner"),
            (ConflictPolicy::BlockForHuman, "block_for_human"),
        ] {
            let yaml = serde_yaml::to_string(&variant).expect("serialises");
            assert!(
                yaml.contains(literal),
                "expected {literal} in {yaml:?} for {variant:?}"
            );
            let reparsed: ConflictPolicy = serde_yaml::from_str(&yaml).expect("re-parses");
            assert_eq!(reparsed, variant);
        }
    }

    #[test]
    fn integration_required_for_serialises_snake_case() {
        for (variant, literal) in [
            (
                IntegrationRequirement::DecomposedParent,
                "decomposed_parent",
            ),
            (
                IntegrationRequirement::MultipleChildBranches,
                "multiple_child_branches",
            ),
        ] {
            let yaml = serde_yaml::to_string(&variant).expect("serialises");
            assert!(
                yaml.contains(literal),
                "expected {literal} in {yaml:?} for {variant:?}"
            );
            let reparsed: IntegrationRequirement = serde_yaml::from_str(&yaml).expect("re-parses");
            assert_eq!(reparsed, variant);
        }
    }

    #[test]
    fn integration_rejects_unknown_merge_strategy() {
        let yaml = r#"
tracker:
  project_slug: ENG
integration:
  merge_strategy: rebase_onto
"#;
        let err = serde_yaml::from_str::<WorkflowConfig>(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("rebase_onto")
                || msg.contains("unknown variant")
                || msg.contains("sequential_cherry_pick"),
            "expected unknown-variant error, got: {msg}"
        );
    }

    #[test]
    fn integration_rejects_unknown_conflict_policy() {
        let yaml = r#"
tracker:
  project_slug: ENG
integration:
  conflict_policy: escalate_to_oncall
"#;
        let err = serde_yaml::from_str::<WorkflowConfig>(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("escalate_to_oncall")
                || msg.contains("unknown variant")
                || msg.contains("integration_owner_repair_turn"),
            "expected unknown-variant error, got: {msg}"
        );
    }

    #[test]
    fn integration_rejects_unknown_required_for_variant() {
        let yaml = r#"
tracker:
  project_slug: ENG
integration:
  required_for:
    - any_decomposition
"#;
        let err = serde_yaml::from_str::<WorkflowConfig>(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("any_decomposition")
                || msg.contains("unknown variant")
                || msg.contains("decomposed_parent"),
            "expected unknown-variant error, got: {msg}"
        );
    }

    #[test]
    fn integration_rejects_unknown_top_level_key() {
        let yaml = r#"
tracker:
  project_slug: ENG
integration:
  owner_roles: [platform_lead]
"#;
        let err = serde_yaml::from_str::<WorkflowConfig>(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("owner_roles") || msg.contains("unknown field"),
            "expected unknown-field error, got: {msg}"
        );
    }

    #[test]
    fn integration_round_trips_through_yaml() {
        let yaml = r#"
tracker:
  project_slug: ENG
integration:
  owner_role: platform_lead
  required_for:
    - decomposed_parent
  merge_strategy: merge_commits
  conflict_policy: route_to_child_owner
  require_all_children_terminal: false
  require_no_open_blockers: true
  require_integration_summary: true
  next_state_after_integration: In Review
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        let reserialised = serde_yaml::to_string(&parsed).expect("serialises");
        let reparsed: WorkflowConfig = serde_yaml::from_str(&reserialised).expect("re-parses");
        assert_eq!(parsed, reparsed);
        assert_eq!(
            parsed.integration.merge_strategy,
            MergeStrategy::MergeCommits
        );
        assert_eq!(
            parsed.integration.conflict_policy,
            ConflictPolicy::RouteToChildOwner
        );
        assert!(!parsed.integration.require_all_children_terminal);
    }

    #[test]
    fn pull_requests_block_defaults_when_omitted() {
        // SPEC v2 §5.11 defaults: an omitted `pull_requests:` block must
        // produce the safe defaults — disabled, no owner, GitHub
        // provider, draft after integration, ready after QA, link
        // tracker issues, require green CI.
        let cfg = WorkflowConfig::default();
        let p = &cfg.pull_requests;
        assert!(!p.enabled);
        assert!(p.owner_role.is_none());
        assert_eq!(p.provider, PrProvider::Github);
        assert_eq!(p.open_stage, PrOpenStage::AfterIntegrationVerification);
        assert_eq!(p.initial_state, PrInitialState::Draft);
        assert_eq!(p.mark_ready_stage, PrMarkReadyStage::AfterQaPasses);
        assert!(p.title_template.is_none());
        assert!(p.body_template.is_none());
        assert!(p.link_tracker_issues);
        assert!(p.require_ci_green_before_ready);
    }

    #[test]
    fn pull_requests_partial_block_inherits_documented_defaults() {
        // An operator who writes only `enabled` and `owner_role` should
        // still get the SPEC defaults for provider, lifecycle stages,
        // tracker linking, and CI gating.
        let yaml = r#"
tracker:
  project_slug: ENG
pull_requests:
  enabled: true
  owner_role: platform_lead
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        let p = &parsed.pull_requests;
        assert!(p.enabled);
        assert_eq!(p.owner_role.as_deref(), Some("platform_lead"));
        assert_eq!(p.provider, PrProvider::Github);
        assert_eq!(p.open_stage, PrOpenStage::AfterIntegrationVerification);
        assert_eq!(p.initial_state, PrInitialState::Draft);
        assert_eq!(p.mark_ready_stage, PrMarkReadyStage::AfterQaPasses);
        assert!(p.link_tracker_issues);
        assert!(p.require_ci_green_before_ready);
    }

    #[test]
    fn pull_requests_full_block_parses_spec_example() {
        // Mirrors the SPEC v2 §5.11 worked example verbatim.
        let yaml = r#"
tracker:
  project_slug: ENG
pull_requests:
  enabled: true
  owner_role: platform_lead
  provider: github
  open_stage: after_integration_verification
  initial_state: draft
  mark_ready_stage: after_qa_passes
  title_template: "{{identifier}}: {{title}}"
  body_template: |
    ## Summary
    {{integration.summary}}

    ## QA
    {{qa.status}}

    ## Linked work
    {{links}}
  link_tracker_issues: true
  require_ci_green_before_ready: true
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        let p = &parsed.pull_requests;
        assert!(p.enabled);
        assert_eq!(p.owner_role.as_deref(), Some("platform_lead"));
        assert_eq!(p.provider, PrProvider::Github);
        assert_eq!(p.open_stage, PrOpenStage::AfterIntegrationVerification);
        assert_eq!(p.initial_state, PrInitialState::Draft);
        assert_eq!(p.mark_ready_stage, PrMarkReadyStage::AfterQaPasses);
        assert_eq!(
            p.title_template.as_deref(),
            Some("{{identifier}}: {{title}}")
        );
        let body = p.body_template.as_deref().expect("body_template parses");
        assert!(body.contains("{{integration.summary}}"));
        assert!(body.contains("{{qa.status}}"));
        assert!(body.contains("{{links}}"));
        assert!(p.link_tracker_issues);
        assert!(p.require_ci_green_before_ready);
    }

    #[test]
    fn pull_requests_open_stage_serialises_snake_case() {
        for (variant, literal) in [
            (
                PrOpenStage::AfterIntegrationVerification,
                "after_integration_verification",
            ),
            (PrOpenStage::Manual, "manual"),
        ] {
            let yaml = serde_yaml::to_string(&variant).expect("serialises");
            assert!(
                yaml.contains(literal),
                "expected {literal} in {yaml:?} for {variant:?}"
            );
            let reparsed: PrOpenStage = serde_yaml::from_str(&yaml).expect("re-parses");
            assert_eq!(reparsed, variant);
        }
    }

    #[test]
    fn pull_requests_initial_state_serialises_snake_case() {
        for (variant, literal) in [
            (PrInitialState::Draft, "draft"),
            (PrInitialState::ReadyForReview, "ready_for_review"),
        ] {
            let yaml = serde_yaml::to_string(&variant).expect("serialises");
            assert!(
                yaml.contains(literal),
                "expected {literal} in {yaml:?} for {variant:?}"
            );
            let reparsed: PrInitialState = serde_yaml::from_str(&yaml).expect("re-parses");
            assert_eq!(reparsed, variant);
        }
    }

    #[test]
    fn pull_requests_mark_ready_stage_serialises_snake_case() {
        for (variant, literal) in [
            (PrMarkReadyStage::AfterQaPasses, "after_qa_passes"),
            (PrMarkReadyStage::Manual, "manual"),
        ] {
            let yaml = serde_yaml::to_string(&variant).expect("serialises");
            assert!(
                yaml.contains(literal),
                "expected {literal} in {yaml:?} for {variant:?}"
            );
            let reparsed: PrMarkReadyStage = serde_yaml::from_str(&yaml).expect("re-parses");
            assert_eq!(reparsed, variant);
        }
    }

    #[test]
    fn pull_requests_provider_serialises_snake_case() {
        let yaml = serde_yaml::to_string(&PrProvider::Github).expect("serialises");
        assert!(yaml.contains("github"), "expected github in {yaml:?}");
        let reparsed: PrProvider = serde_yaml::from_str(&yaml).expect("re-parses");
        assert_eq!(reparsed, PrProvider::Github);
    }

    #[test]
    fn pull_requests_rejects_unknown_open_stage() {
        let yaml = r#"
tracker:
  project_slug: ENG
pull_requests:
  open_stage: on_branch_push
"#;
        let err = serde_yaml::from_str::<WorkflowConfig>(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("on_branch_push")
                || msg.contains("unknown variant")
                || msg.contains("after_integration_verification"),
            "expected unknown-variant error, got: {msg}"
        );
    }

    #[test]
    fn pull_requests_rejects_unknown_initial_state() {
        let yaml = r#"
tracker:
  project_slug: ENG
pull_requests:
  initial_state: in_review
"#;
        let err = serde_yaml::from_str::<WorkflowConfig>(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("in_review") || msg.contains("unknown variant") || msg.contains("draft"),
            "expected unknown-variant error, got: {msg}"
        );
    }

    #[test]
    fn pull_requests_rejects_unknown_mark_ready_stage() {
        let yaml = r#"
tracker:
  project_slug: ENG
pull_requests:
  mark_ready_stage: after_review_request
"#;
        let err = serde_yaml::from_str::<WorkflowConfig>(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("after_review_request")
                || msg.contains("unknown variant")
                || msg.contains("after_qa_passes"),
            "expected unknown-variant error, got: {msg}"
        );
    }

    #[test]
    fn pull_requests_rejects_unknown_provider() {
        let yaml = r#"
tracker:
  project_slug: ENG
pull_requests:
  provider: gitlab
"#;
        let err = serde_yaml::from_str::<WorkflowConfig>(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("gitlab") || msg.contains("unknown variant") || msg.contains("github"),
            "expected unknown-variant error, got: {msg}"
        );
    }

    #[test]
    fn pull_requests_rejects_unknown_top_level_key() {
        let yaml = r#"
tracker:
  project_slug: ENG
pull_requests:
  auto_merge: true
"#;
        let err = serde_yaml::from_str::<WorkflowConfig>(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("auto_merge") || msg.contains("unknown field"),
            "expected unknown-field error, got: {msg}"
        );
    }

    #[test]
    fn pull_requests_round_trips_through_yaml() {
        let yaml = r#"
tracker:
  project_slug: ENG
pull_requests:
  enabled: true
  owner_role: platform_lead
  provider: github
  open_stage: manual
  initial_state: ready_for_review
  mark_ready_stage: manual
  title_template: "draft: {{identifier}}"
  body_template: "see {{links}}"
  link_tracker_issues: false
  require_ci_green_before_ready: false
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        let reserialised = serde_yaml::to_string(&parsed).expect("serialises");
        let reparsed: WorkflowConfig = serde_yaml::from_str(&reserialised).expect("re-parses");
        assert_eq!(parsed, reparsed);
        assert_eq!(parsed.pull_requests.open_stage, PrOpenStage::Manual);
        assert_eq!(
            parsed.pull_requests.initial_state,
            PrInitialState::ReadyForReview
        );
        assert_eq!(
            parsed.pull_requests.mark_ready_stage,
            PrMarkReadyStage::Manual
        );
        assert!(!parsed.pull_requests.link_tracker_issues);
        assert!(!parsed.pull_requests.require_ci_green_before_ready);
    }

    #[test]
    fn routing_round_trips_through_yaml() {
        let yaml = r#"
tracker:
  project_slug: ENG
routing:
  default_role: platform_lead
  match_mode: priority
  rules:
    - priority: 100
      when:
        labels_any: [qa]
      assign_role: qa
    - priority: 10
      when:
        issue_size: broad
      assign_role: platform_lead
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        let reserialised = serde_yaml::to_string(&parsed).expect("serialises");
        let reparsed: WorkflowConfig = serde_yaml::from_str(&reserialised).expect("re-parses");
        assert_eq!(parsed, reparsed);
    }
}
