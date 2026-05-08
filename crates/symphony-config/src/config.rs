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
            status: StatusConfig::default(),
            roles: BTreeMap::new(),
            agents: BTreeMap::new(),
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

/// Tick cadence for the orchestrator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PollingConfig {
    /// Wall-clock milliseconds between successive polls. The
    /// orchestrator adds jitter on top.
    #[serde(default = "default_poll_interval_ms")]
    pub interval_ms: u64,
}

impl Default for PollingConfig {
    fn default() -> Self {
        Self {
            interval_ms: default_poll_interval_ms(),
        }
    }
}

fn default_poll_interval_ms() -> u64 {
    30_000
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

/// Backend class for an [`AgentProfileConfig`]. SPEC v2 §5.5 limits
/// product backends to `codex`, `claude`, and `hermes`; the test-only
/// `mock` variant is preserved here so quickstart fixtures and unit
/// tests can declare profiles without an external binary.
///
/// Composite/tandem behaviour is *not* a backend — SPEC §5.5 defines it
/// as an `agents:` *strategy* that composes other profiles. The next
/// checklist iteration introduces the composite shape as a sibling of
/// this enum; until then, profiles always declare a concrete backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentBackend {
    /// `codex app-server` over JSONL stdio.
    Codex,
    /// `claude -p --output-format stream-json` over stdio.
    Claude,
    /// Hermes Agent CLI in non-interactive mode. SPEC §5.5 reserves
    /// the protocol shape for the next checklist iteration; the
    /// variant exists now so a profile can declare `backend: hermes`
    /// without churning later.
    Hermes,
    /// In-process scripted runner. Test/fixture only — not advertised
    /// as a production backend.
    Mock,
}

/// One entry in the `agents` map (SPEC v2 §5.5).
///
/// A profile bundles a backend with the runtime policy that should be
/// applied when launching it. Names live in their own keyspace and are
/// referenced from [`RoleConfig::agent`], so the same profile can be
/// reused across roles and a role can be re-pointed at a different
/// profile without renaming either.
///
/// Most policy fields are `Option`-typed and default to "inherit": the
/// kernel composes a profile-level value over the global
/// [`AgentConfig`]/[`CodexConfig`] defaults at dispatch time. This
/// keeps profiles small and lets operators declare just the deltas
/// they care about — the typed struct is intentionally a flat record
/// of operator intent, not the resolved runtime view.
///
/// Fields that mirror the upstream Codex/Claude schema (`approval_policy`,
/// `sandbox_policy`) are carried as opaque `serde_yaml::Value` for the
/// same reason as [`CodexConfig`]: those value sets evolve outside our
/// schema and the runtime validates them at dispatch preflight.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentProfileConfig {
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

        let lead = parsed.agents.get("lead_agent").expect("lead present");
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
            parsed.agents.get("qa_agent").map(|a| a.backend),
            Some(AgentBackend::Claude)
        );
        assert_eq!(
            parsed.agents.get("hermes_agent").map(|a| a.backend),
            Some(AgentBackend::Hermes)
        );
        assert_eq!(
            parsed.agents.get("fixture_agent").map(|a| a.backend),
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
        let yaml = r#"
tracker:
  project_slug: ENG
agents:
  bogus:
    backend: gpt5
"#;
        let err = serde_yaml::from_str::<WorkflowConfig>(yaml).unwrap_err();
        assert!(
            err.to_string().contains("gpt5") || err.to_string().contains("unknown variant"),
            "expected unknown-variant error, got: {err}"
        );
    }

    #[test]
    fn agent_unknown_field_is_rejected() {
        let yaml = r#"
tracker:
  project_slug: ENG
agents:
  lead_agent:
    backend: codex
    descritpion: typo
"#;
        let err = serde_yaml::from_str::<WorkflowConfig>(yaml).unwrap_err();
        assert!(
            err.to_string().contains("descritpion") || err.to_string().contains("unknown field"),
            "expected unknown-field error, got: {err}"
        );
    }

    #[test]
    fn agent_backend_is_required() {
        let yaml = r#"
tracker:
  project_slug: ENG
agents:
  bare:
    description: missing backend
"#;
        let err = serde_yaml::from_str::<WorkflowConfig>(yaml).unwrap_err();
        assert!(
            err.to_string().contains("backend") || err.to_string().contains("missing field"),
            "expected missing-field error for backend, got: {err}"
        );
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
        let p = parsed.agents.get("minimal").expect("present");
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
}
