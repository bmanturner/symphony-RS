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

    /// Per-issue workspace root, named strategies, and runner-side
    /// invariants (cwd containment, untracked-file policy). SPEC v2
    /// §5.8. Renamed from `WorkspaceConfig` so the v2 structural shape
    /// — a strategy registry plus policy flags — has its own type
    /// distinct from the v1 "just a `root` path" stub.
    #[serde(default)]
    pub workspace: WorkspacePolicyConfig,

    /// Branch templates and tree-cleanliness policy that govern how
    /// child/integration branches are named and when an agent is
    /// allowed to start. SPEC v2 §5.9. Defaults to the documented
    /// `symphony/{{identifier}}` /
    /// `symphony/integration/{{identifier}}` templates with
    /// same-branch children disallowed and clean-tree-required on,
    /// matching the SPEC's "safer setting wins" stance.
    #[serde(default)]
    pub branching: BranchPolicyConfig,

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

    /// Operator-facing observability surfaces. SPEC v2 §5.14. Wraps
    /// the structured-logs format, the in-process event bus toggle,
    /// the Server-Sent Events (SSE) HTTP feed (formerly the top-level
    /// `status:` block), and the TUI/dashboard switches.
    ///
    /// Defaults keep the daemon runnable out of the box: SSE bound to
    /// loopback, event bus on, TUI on, dashboard off.
    #[serde(default)]
    pub observability: ObservabilityConfig,

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

    /// QA gate policy. SPEC v2 §5.12. Defaults to `required: false`
    /// with no `owner_role` so a workflow that omits the block behaves
    /// identically to v1 (no QA gate). When `required` flips to true,
    /// later phases enforce that `owner_role` resolves to a role of
    /// `kind: qa_gate` and that the gate observes the documented
    /// blocker-policy semantics.
    #[serde(default)]
    pub qa: QaConfig,

    /// Follow-up issue policy. SPEC v2 §5.13. Defaults to
    /// `enabled: false` with the safer
    /// [`ChildIssuePolicy::ProposeForApproval`] default so a workflow
    /// that omits the block does not silently let agents file new
    /// tracker issues. When `enabled` flips to `true`, later phases
    /// enforce that `approval_role` resolves to a configured role.
    #[serde(default)]
    pub followups: FollowupConfig,

    /// Multi-scope concurrency limits. Captures global, per-agent-profile,
    /// and per-repository caps that the kernel's `ConcurrencyGate`
    /// (added in a follow-up subtask) acquires before each dispatch.
    /// Per-role caps remain on `roles.<name>.max_concurrent`.
    ///
    /// The legacy `agent.max_concurrent_agents` field continues to work
    /// as a deprecated alias for [`ConcurrencyConfig::global_max`]: when
    /// the typed `concurrency.global_max` is set it wins; when it is
    /// unset, [`WorkflowConfig::resolved_global_concurrency`] falls back
    /// to `agent.max_concurrent_agents` so existing fixtures keep
    /// working unchanged.
    #[serde(default)]
    pub concurrency: ConcurrencyConfig,

    /// Per-issue and per-run budget caps that the orchestrator enforces
    /// as hard ceilings. SPEC v2 §5.15. Defaults match the SPEC example
    /// (6 parallel runs, $10/issue, 20 turns/run, 3 retries,
    /// `block_work_item`).
    #[serde(default)]
    pub budgets: BudgetsConfig,
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
            workspace: WorkspacePolicyConfig::default(),
            branching: BranchPolicyConfig::default(),
            hooks: HooksConfig::default(),
            agent: AgentConfig::default(),
            codex: CodexConfig::default(),
            observability: ObservabilityConfig::default(),
            roles: BTreeMap::new(),
            agents: BTreeMap::new(),
            routing: RoutingConfig::default(),
            decomposition: DecompositionConfig::default(),
            integration: IntegrationConfig::default(),
            pull_requests: PullRequestConfig::default(),
            qa: QaConfig::default(),
            followups: FollowupConfig::default(),
            concurrency: ConcurrencyConfig::default(),
            budgets: BudgetsConfig::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// concurrency
// ---------------------------------------------------------------------------

/// Multi-scope concurrency limits surfaced as typed config so the
/// kernel's `ConcurrencyGate` can acquire `(scope_kind, scope_key)`
/// permits before each dispatch.
///
/// Three scopes live here:
///
/// 1. [`ConcurrencyConfig::global_max`] — overall in-flight cap. When
///    set, it supersedes the deprecated `agent.max_concurrent_agents`
///    alias. When unset, [`WorkflowConfig::resolved_global_concurrency`]
///    falls back to `agent.max_concurrent_agents`.
/// 2. [`ConcurrencyConfig::per_agent_profile`] — caps keyed by
///    [`WorkflowConfig::agents`] profile names. Validated against the
///    `agents` map at load time.
/// 3. [`ConcurrencyConfig::per_repository`] — caps keyed by repository
///    slug. Validated against [`TrackerConfig::repository`] at load
///    time; future multi-repo support will widen the registry without
///    a schema change.
///
/// Per-role caps are intentionally *not* defined here — they remain on
/// [`RoleConfig::max_concurrent`] to keep the role definition the
/// single source of truth for role authority. The gate primitive will
/// query both surfaces when it acquires permits.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConcurrencyConfig {
    /// Overall in-flight dispatch cap across the orchestrator. `None`
    /// inherits the deprecated `agent.max_concurrent_agents` alias.
    /// Zero is rejected by [`WorkflowConfig::validate`].
    #[serde(default)]
    pub global_max: Option<u32>,

    /// Per-agent-profile dispatch caps keyed by entries in
    /// [`WorkflowConfig::agents`]. Unknown keys and zero values are
    /// rejected at validate time.
    #[serde(default)]
    pub per_agent_profile: BTreeMap<String, u32>,

    /// Per-repository dispatch caps keyed by repository slug. Today
    /// only [`TrackerConfig::repository`] is recognised; unknown keys
    /// and zero values are rejected at validate time.
    #[serde(default)]
    pub per_repository: BTreeMap<String, u32>,
}

// ---------------------------------------------------------------------------
// budgets
// ---------------------------------------------------------------------------

/// Per-issue and per-run budget caps. Mirrors SPEC v2 §5.15.
///
/// The orchestrator enforces these caps as hard ceilings: when one is
/// exceeded the offending work item or run is parked into a durable
/// budget pause (driven by the existing `BudgetPauseDispatchQueue`) and
/// an `OrchestratorEvent::BudgetExceeded` is broadcast and persisted.
/// They never silently fail the work or spin retries — see the
/// "Budget exhaustion MUST" sentence in SPEC §5.15.
///
/// The defaults match SPEC §5.15 verbatim and are tuned for an
/// out-of-the-box workflow: 6 parallel runs, $10/issue, 20 turns/run,
/// 3 retries, and `block_work_item` as the pause scope.
///
/// Zero on any numeric cap is rejected at validate time. A zero cap
/// would either deadlock the gate (no work could proceed) or, in the
/// `max_retries` case, contradict the operator's opt-in to a non-zero
/// retry policy elsewhere in the config — fail loudly instead.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetsConfig {
    /// Cap on simultaneously running dispatches across the orchestrator.
    /// Conceptually overlaps with [`ConcurrencyConfig::global_max`] but
    /// the budgets surface owns the user-facing knob; the kernel may
    /// reconcile the two in a future subtask. SPEC v2 §5.15.
    #[serde(default = "default_budgets_max_parallel_runs")]
    pub max_parallel_runs: u32,

    /// Cumulative cost ceiling per work item, in USD. Crossed when
    /// agent providers report spend that pushes the running total over
    /// this cap. SPEC v2 §5.15.
    #[serde(default = "default_budgets_max_cost_per_issue_usd")]
    pub max_cost_per_issue_usd: f64,

    /// Maximum number of agent turns allowed in a single run before
    /// the run is parked. SPEC v2 §5.15.
    #[serde(default = "default_budgets_max_turns_per_run")]
    pub max_turns_per_run: u32,

    /// Maximum retry attempts before further retries are refused with a
    /// `BudgetExceeded` decision. Mirrored by `RetryPolicyDecision`
    /// in `symphony-core::retry` (added in a follow-up subtask). SPEC
    /// v2 §5.15.
    #[serde(default = "default_budgets_max_retries")]
    pub max_retries: u32,

    /// Whether budget exhaustion blocks the entire work item or only
    /// the offending run. SPEC v2 §5.15.
    #[serde(default)]
    pub pause_policy: BudgetPausePolicy,
}

impl Default for BudgetsConfig {
    fn default() -> Self {
        Self {
            max_parallel_runs: default_budgets_max_parallel_runs(),
            max_cost_per_issue_usd: default_budgets_max_cost_per_issue_usd(),
            max_turns_per_run: default_budgets_max_turns_per_run(),
            max_retries: default_budgets_max_retries(),
            pause_policy: BudgetPausePolicy::default(),
        }
    }
}

fn default_budgets_max_parallel_runs() -> u32 {
    6
}

fn default_budgets_max_cost_per_issue_usd() -> f64 {
    10.00
}

fn default_budgets_max_turns_per_run() -> u32 {
    20
}

fn default_budgets_max_retries() -> u32 {
    3
}

/// Scope of a durable budget pause. SPEC v2 §5.15 names the two
/// variants explicitly; the wire format uses `snake_case` to match
/// the YAML in the SPEC example.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetPausePolicy {
    /// Pause every run associated with the offending work item until
    /// an operator changes policy or approves continuation. The SPEC
    /// default and the safer setting.
    #[default]
    BlockWorkItem,
    /// Pause only the offending run; sibling runs on the same work
    /// item keep going.
    BlockRun,
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

/// Adapter discriminator for the `TrackerRead` trait.
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

    /// Durable-lease tuning for the scheduler runners
    /// (ARCHITECTURE_v2 §7.2). Carries the default lease TTL and the
    /// renewal margin every runner uses when acquiring or refreshing a
    /// lease. Lives under `polling` so all per-tick cadence knobs sit
    /// together; a future top-level `scheduler:` block, if introduced,
    /// would absorb both.
    #[serde(default)]
    pub lease: LeaseConfig,
}

impl Default for PollingConfig {
    fn default() -> Self {
        Self {
            interval_ms: default_poll_interval_ms(),
            jitter_ms: default_poll_jitter_ms(),
            startup_reconcile_recent_terminal: default_startup_reconcile_recent_terminal(),
            lease: LeaseConfig::default(),
        }
    }
}

/// Durable-lease tuning shared by every scheduler runner.
///
/// `default_ttl_ms` is the wall-clock TTL written to
/// `runs.lease_expires_at` when a runner first acquires a lease.
/// `renewal_margin_ms` is the slack a heartbeating runner uses to
/// refresh its lease *before* expiry; it MUST be strictly less than
/// `default_ttl_ms` so renewals win the race against reaping. The
/// validation step at config-load time enforces that invariant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeaseConfig {
    /// Default lease TTL (ms) written to `runs.lease_expires_at` on
    /// acquisition. Default: `60_000` (one minute).
    #[serde(default = "default_lease_ttl_ms")]
    pub default_ttl_ms: u64,

    /// How far before TTL expiry (ms) a runner should attempt to renew
    /// its lease. Default: `15_000` (renew at ~75% of the default TTL).
    #[serde(default = "default_lease_renewal_margin_ms")]
    pub renewal_margin_ms: u64,
}

impl Default for LeaseConfig {
    fn default() -> Self {
        Self {
            default_ttl_ms: default_lease_ttl_ms(),
            renewal_margin_ms: default_lease_renewal_margin_ms(),
        }
    }
}

fn default_lease_ttl_ms() -> u64 {
    60_000
}

fn default_lease_renewal_margin_ms() -> u64 {
    15_000
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

/// Per-issue workspace policy (SPEC v2 §5.8).
///
/// Where v1 only carried a single `root` path, v2 turns this block
/// into a registry: a `default_strategy` name plus a map of named
/// [`WorkspaceStrategyConfig`] entries that the kernel resolves when
/// it builds a `WorkspaceClaim`. Two runner-side invariants travel
/// alongside the registry — `require_cwd_in_workspace` and
/// `forbid_untracked_outside_workspace` — because SPEC v2 §6 / §3
/// makes verifying actual cwd a non-negotiable safety feature, not a
/// per-strategy nuance.
///
/// Defaults are deliberately *empty-but-strict*: no strategies are
/// registered, no `default_strategy` is set, but both safety flags
/// are on. A workflow that omits the block keeps v1's "just use
/// `root`" feel for now while later phases enforce that
/// `default_strategy` resolves to a configured key once strategies
/// are in use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspacePolicyConfig {
    /// Root directory for per-issue workspaces. `~` and `$VAR`
    /// expansion happen during loading. `None` resolves at runtime to
    /// `<system-temp>/symphony_workspaces` per SPEC §5.3.3 / §5.8.
    #[serde(default)]
    pub root: Option<PathBuf>,

    /// Name of the default strategy entry (key into
    /// [`WorkspacePolicyConfig::strategies`]) used when a work item
    /// does not specify one explicitly. Optional at parse time so
    /// partial fixtures can declare the block without yet enumerating
    /// strategies; later phases enforce that, when non-`None`, the
    /// value resolves to a configured strategy.
    #[serde(default)]
    pub default_strategy: Option<String>,

    /// Named workspace strategies keyed by their workflow-chosen
    /// name. Empty by default. Each entry's [`WorkspaceStrategyConfig`]
    /// declares whether the kernel should create a fresh git worktree,
    /// reuse an existing one, or share a branch with the integration
    /// owner.
    #[serde(default)]
    pub strategies: BTreeMap<String, WorkspaceStrategyConfig>,

    /// When `true` (the default), the runner verifies that the
    /// process cwd equals the resolved workspace path immediately
    /// before launching an agent. SPEC v2 §6 makes this verification
    /// a non-negotiable safety gate; turning it off is an explicit
    /// operator override.
    #[serde(default = "default_true")]
    pub require_cwd_in_workspace: bool,

    /// When `true` (the default), the runner refuses to launch a
    /// mutation-capable agent if the workspace's git tree contains
    /// untracked files outside the configured workspace path.
    /// Pairs with [`BranchPolicyConfig::require_clean_tree_before_run`]
    /// to keep branch state honest.
    #[serde(default = "default_true")]
    pub forbid_untracked_outside_workspace: bool,
}

impl Default for WorkspacePolicyConfig {
    fn default() -> Self {
        Self {
            root: None,
            default_strategy: None,
            strategies: BTreeMap::new(),
            require_cwd_in_workspace: true,
            forbid_untracked_outside_workspace: true,
        }
    }
}

/// One named entry in [`WorkspacePolicyConfig::strategies`].
///
/// The shape is a flat record because SPEC v2 §5.8 documents
/// strategies as YAML maps with a `kind` discriminant and shape-
/// dependent siblings (`base`/`branch_template` for `git_worktree`,
/// `path`/`require_branch` for `existing_worktree`). Keeping the
/// struct flat with optional siblings preserves `deny_unknown_fields`
/// at the YAML level; later validation phases enforce that the
/// fields present match the declared `kind`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceStrategyConfig {
    /// Which strategy variant this entry implements.
    #[serde(default)]
    pub kind: WorkspaceStrategyKind,

    /// Base ref for [`WorkspaceStrategyKind::GitWorktree`]. `None`
    /// means "fall back to [`BranchPolicyConfig::default_base`]".
    /// Ignored by [`WorkspaceStrategyKind::ExistingWorktree`] and
    /// [`WorkspaceStrategyKind::SharedBranch`].
    #[serde(default)]
    pub base: Option<String>,

    /// Branch-name template for [`WorkspaceStrategyKind::GitWorktree`].
    /// `None` means "fall back to
    /// [`BranchPolicyConfig::child_branch_template`]". Ignored by
    /// other kinds. Variable rendering is the strict
    /// `{{path.to.value}}` form; unknown variables MUST fail render
    /// rather than silently producing literal `{{...}}` segments.
    #[serde(default)]
    pub branch_template: Option<String>,

    /// Cleanup policy for the worktree directory. Defaults to
    /// [`WorkspaceCleanupPolicy::RetainUntilDone`] — the SPEC v2
    /// §5.8 documented default — so workspaces stick around until a
    /// terminal state lets the kernel reclaim them.
    #[serde(default)]
    pub cleanup: WorkspaceCleanupPolicy,

    /// Filesystem path for [`WorkspaceStrategyKind::ExistingWorktree`].
    /// `~` and `$VAR` expansion happen during loading. Ignored by
    /// [`WorkspaceStrategyKind::GitWorktree`] (where the path is
    /// derived from `root` and the issue identifier) and by
    /// [`WorkspaceStrategyKind::SharedBranch`].
    #[serde(default)]
    pub path: Option<PathBuf>,

    /// Required branch/ref for
    /// [`WorkspaceStrategyKind::ExistingWorktree`] or
    /// [`WorkspaceStrategyKind::SharedBranch`]. When set, the runner
    /// verifies the actual git ref matches before launching a
    /// mutation-capable agent (SPEC v2 §6).
    #[serde(default)]
    pub require_branch: Option<String>,
}

/// Discriminator for [`WorkspaceStrategyConfig`] (SPEC v2 §5.8 `kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceStrategyKind {
    /// Create a fresh git worktree per work item under
    /// [`WorkspacePolicyConfig::root`], on a branch derived from
    /// `branch_template`. Default — the documented "isolated per
    /// issue" baseline that SPEC v2 §2.6 recommends.
    #[default]
    GitWorktree,
    /// Reuse a worktree that already exists at `path`, optionally
    /// asserting it is checked out on `require_branch`. Used for
    /// shared integration setups where the integration owner
    /// maintains a long-lived worktree.
    ExistingWorktree,
    /// Multiple work items share one branch and one worktree
    /// (typically an integration owner's). Only legal when
    /// [`BranchPolicyConfig::allow_same_branch_for_children`] is
    /// `true`; later phases enforce that pairing.
    SharedBranch,
}

/// Cleanup policy applied to a worktree once its work item reaches a
/// terminal state (SPEC v2 §5.8 `cleanup`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceCleanupPolicy {
    /// Keep the worktree on disk until the work item reaches a
    /// terminal class, then reclaim it. Default per SPEC v2 §5.8 —
    /// preserves debugging artefacts while in-flight without
    /// hoarding directories indefinitely.
    #[default]
    RetainUntilDone,
    /// Remove the worktree as soon as the run completes. Use when
    /// disk pressure outweighs post-mortem convenience.
    RemoveAfterRun,
    /// Never remove the worktree. The operator owns cleanup. Use
    /// for shared integration worktrees that live across many work
    /// items.
    RetainAlways,
}

// ---------------------------------------------------------------------------
// branching (SPEC v2 §5.9)
// ---------------------------------------------------------------------------

/// Branch templates and tree-cleanliness policy.
///
/// SPEC v2 §5.9 describes how the orchestrator names branches for
/// child specialist work and the canonical integration branch, plus
/// the safety stance on running with a dirty tree. The default is
/// the safer one: same-branch children are *off*, clean-tree is
/// *required*. Operators who want shared-branch execution must opt
/// in explicitly here *and* select a shared workspace strategy in
/// [`WorkspacePolicyConfig::strategies`]; if those disagree,
/// validation rejects the config rather than guessing (SPEC v2 §5.9:
/// "the safer setting wins by rejecting the config").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BranchPolicyConfig {
    /// Default base ref for newly created child branches when a
    /// strategy does not override it. Defaults to `"main"` per the
    /// SPEC v2 §5.9 worked example.
    #[serde(default = "default_branch_base")]
    pub default_base: String,

    /// Template applied when minting a child specialist's branch
    /// name. Defaults to `"symphony/{{identifier}}"`. Variable
    /// rendering is the strict `{{path.to.value}}` form; unknown
    /// variables MUST fail render.
    #[serde(default = "default_child_branch_template")]
    pub child_branch_template: String,

    /// Template applied when minting the canonical integration
    /// branch name. Defaults to
    /// `"symphony/integration/{{identifier}}"`. Same rendering rules
    /// as `child_branch_template`.
    #[serde(default = "default_integration_branch_template")]
    pub integration_branch_template: String,

    /// When `true`, child specialists may execute on the same branch
    /// as the integration owner instead of getting their own. Only
    /// legal when paired with a shared workspace strategy in
    /// [`WorkspacePolicyConfig::strategies`]. Defaults to `false`,
    /// matching SPEC v2 §5.9's safer-default stance.
    #[serde(default)]
    pub allow_same_branch_for_children: bool,

    /// When `true` (the default), the runner refuses to launch a
    /// mutation-capable agent against a dirty tree. Pairs with
    /// [`WorkspacePolicyConfig::forbid_untracked_outside_workspace`]
    /// to keep branch state honest before each agent attempt.
    #[serde(default = "default_true")]
    pub require_clean_tree_before_run: bool,
}

impl Default for BranchPolicyConfig {
    fn default() -> Self {
        Self {
            default_base: default_branch_base(),
            child_branch_template: default_child_branch_template(),
            integration_branch_template: default_integration_branch_template(),
            allow_same_branch_for_children: false,
            require_clean_tree_before_run: true,
        }
    }
}

fn default_branch_base() -> String {
    "main".to_string()
}

fn default_child_branch_template() -> String {
    "symphony/{{identifier}}".to_string()
}

fn default_integration_branch_template() -> String {
    "symphony/integration/{{identifier}}".to_string()
}

// ---------------------------------------------------------------------------
// hooks
// ---------------------------------------------------------------------------

/// Shell hook scripts fired around the workspace lifecycle.
///
/// SPEC v2 §5.17: each phase is a list of shell snippets. Snippets in a
/// list run sequentially under `sh -lc <snippet>` with the workspace as
/// `cwd`; the first non-zero exit aborts the phase. An empty list is
/// equivalent to "no hook configured." `timeout_ms` applies per snippet,
/// not per phase, so a long-running phase composed of several quick
/// snippets stays bounded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HooksConfig {
    /// Runs once when the workspace directory is newly created. A
    /// non-zero exit from any snippet aborts workspace creation.
    #[serde(default)]
    pub after_create: Vec<String>,

    /// Runs before each agent attempt, after workspace prep. A non-zero
    /// exit from any snippet aborts the attempt.
    #[serde(default)]
    pub before_run: Vec<String>,

    /// Runs after each agent attempt regardless of outcome. Failures
    /// are logged and ignored.
    #[serde(default)]
    pub after_run: Vec<String>,

    /// Runs before workspace deletion if the directory exists. Failures
    /// are logged and ignored; cleanup still proceeds.
    #[serde(default)]
    pub before_remove: Vec<String>,

    /// Per-snippet wall-clock timeout. SPEC v2 §5.17 raised the default
    /// from 60s to 5 minutes because v2 hooks routinely do branch prep
    /// or worktree creation.
    #[serde(default = "default_hook_timeout_ms")]
    pub timeout_ms: u64,
}

impl Default for HooksConfig {
    fn default() -> Self {
        Self {
            after_create: Vec::new(),
            before_run: Vec::new(),
            after_run: Vec::new(),
            before_remove: Vec::new(),
            timeout_ms: default_hook_timeout_ms(),
        }
    }
}

fn default_hook_timeout_ms() -> u64 {
    300_000
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
// observability (SPEC v2 §5.14)
// ---------------------------------------------------------------------------

/// Operator-facing observability surfaces.
///
/// SPEC v2 §5.14 collects the previously top-level `status:` block plus
/// the structured-logs format, the in-process event bus toggle, and the
/// TUI/dashboard switches under one section. The kernel itself remains
/// headless; everything in this struct is optional out-of-process
/// machinery layered over durable state and the event stream.
///
/// All sub-blocks default in a way that keeps the daemon runnable
/// without explicit configuration: SSE bound to loopback, event bus on,
/// TUI on, dashboard off.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservabilityConfig {
    /// Structured-log shape. SPEC v2 §5.14.
    #[serde(default)]
    pub logs: LogsConfig,

    /// Whether the in-process event bus is enabled. Disabling the bus
    /// suppresses both the SSE server and any in-process subscriber;
    /// it exists so headless deployments (e.g. CI smoke tests) can
    /// trim the broadcast machinery without disabling individual
    /// surfaces one by one. SPEC v2 §5.14.
    #[serde(default = "default_true")]
    pub event_bus: bool,

    /// Server-Sent Events HTTP feed. Formerly the top-level
    /// `status:` block; SPEC v2 §5.14 specifies the move under
    /// `observability.sse` and preserves the `bind` / `replay_buffer`
    /// validation. See [`SseConfig`].
    #[serde(default)]
    pub sse: SseConfig,

    /// In-process TUI surface. SPEC v2 §5.14.
    #[serde(default)]
    pub tui: TuiConfig,

    /// Optional web dashboard surface. SPEC v2 §5.14. Defaults
    /// disabled so a stock workflow does not silently bind a port.
    #[serde(default)]
    pub dashboard: DashboardConfig,
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            logs: LogsConfig::default(),
            event_bus: true,
            sse: SseConfig::default(),
            tui: TuiConfig::default(),
            dashboard: DashboardConfig::default(),
        }
    }
}

/// Structured-log configuration block. Currently only the format
/// discriminant is exposed; level, sink, and sampling stay implicit
/// (driven by `RUST_LOG` and the tracing layer in the binary). SPEC v2
/// §5.14.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogsConfig {
    /// Output format for structured logs.
    #[serde(default)]
    pub format: LogFormat,
}

/// Shape of structured-log output. Mirrors SPEC v2 §5.14 worked
/// example: `json` for ingest, `pretty` for interactive runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// Newline-delimited JSON. Default — matches the daemon's
    /// production tracing layer and is the only format external log
    /// shippers can ingest losslessly.
    #[default]
    Json,
    /// Human-readable pretty-printed lines, intended for interactive
    /// `symphony run` shells. Emits ANSI colour when the underlying
    /// writer is a TTY.
    Pretty,
}

/// Configuration for the out-of-process SSE surface (formerly
/// `StatusConfig`).
///
/// The SSE surface is a narrow `axum` HTTP server inside the daemon
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
pub struct SseConfig {
    /// Whether to start the SSE server alongside the orchestrator.
    /// `true` keeps `symphony watch` working out of the box; setting
    /// this to `false` skips the bind entirely (useful in CI, in
    /// systemd unit tests, or when running multiple daemons on the
    /// same host).
    #[serde(default = "default_sse_enabled")]
    pub enabled: bool,

    /// Address to bind the SSE listener on. Defaults to
    /// `127.0.0.1:6280` — loopback so the server isn't accidentally
    /// reachable from the network. Anything `SocketAddr` accepts is
    /// valid; the runtime parses lazily on bind, so a malformed value
    /// surfaces at startup rather than at config-load time.
    #[serde(default = "default_sse_bind")]
    pub bind: String,

    /// Capacity of the `tokio::sync::broadcast` channel that fans
    /// orchestrator events out to subscribers. Higher values absorb
    /// slower consumers at the cost of memory; a slow consumer that
    /// falls behind by more than this many events sees a `Lagged`
    /// signal and is expected to reconnect. Default mirrors the
    /// orchestrator's own internal default (256).
    #[serde(default = "default_sse_replay_buffer")]
    pub replay_buffer: usize,
}

impl Default for SseConfig {
    fn default() -> Self {
        Self {
            enabled: default_sse_enabled(),
            bind: default_sse_bind(),
            replay_buffer: default_sse_replay_buffer(),
        }
    }
}

fn default_sse_enabled() -> bool {
    true
}

fn default_sse_bind() -> String {
    "127.0.0.1:6280".to_string()
}

fn default_sse_replay_buffer() -> usize {
    256
}

/// Configuration for the in-process TUI surface. SPEC v2 §5.14
/// promises only an `enabled` switch today; richer panel-level
/// toggles can land alongside the Phase 12 TUI work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TuiConfig {
    /// Whether the TUI binary may be started against this workflow.
    /// Defaults to `true` so an operator running `symphony tui`
    /// against a fresh `WORKFLOW.md` does not have to opt in.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Configuration for the optional web dashboard surface. SPEC v2
/// §5.14. Dashboard implementation is post-Phase-12 but the schema
/// reserves the slot so workflows do not need to be rewritten when
/// the surface ships.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DashboardConfig {
    /// Defaults to `false`: the dashboard is opt-in so a stock
    /// workflow does not silently bind a port or host static assets.
    #[serde(default)]
    pub enabled: bool,
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

impl RoleKind {
    fn child_owning_catalog_role(self) -> bool {
        matches!(self, Self::Specialist | Self::Reviewer | Self::Operator)
    }
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

    /// File-backed role instruction pack paths.
    #[serde(default)]
    pub instructions: RoleInstructionConfig,

    /// Structured assignment metadata rendered into the platform-lead
    /// role catalog.
    #[serde(default)]
    pub assignment: RoleAssignmentMetadata,
}

/// File-backed role doctrine paths for v4 instruction packs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RoleInstructionConfig {
    /// Long-form operating instructions for the role.
    #[serde(default)]
    pub role_prompt: Option<PathBuf>,

    /// Stable behavioral doctrine / quality bar.
    #[serde(default)]
    pub soul: Option<PathBuf>,
}

/// Role-local assignment metadata for generated platform-lead catalogs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RoleAssignmentMetadata {
    /// Work this role should receive.
    #[serde(default)]
    pub owns: Vec<String>,

    /// Work this role should not receive.
    #[serde(default)]
    pub does_not_own: Vec<String>,

    /// Context or prerequisites this role usually needs.
    #[serde(default)]
    pub requires: Vec<String>,

    /// Evidence expected in this role's handoff.
    #[serde(default)]
    pub handoff_expectations: Vec<String>,

    /// Advisory role-local routing hints for catalog rendering.
    #[serde(default)]
    pub routing_hints: RoleRoutingHints,
}

impl RoleAssignmentMetadata {
    fn is_empty(&self) -> bool {
        self.owns.is_empty()
            && self.does_not_own.is_empty()
            && self.requires.is_empty()
            && self.handoff_expectations.is_empty()
            && self.routing_hints.is_empty()
    }
}

/// Advisory routing hints local to one role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RoleRoutingHints {
    /// Path globs this role usually owns.
    #[serde(default)]
    pub paths_any: Vec<String>,

    /// Labels that suggest this role.
    #[serde(default)]
    pub labels_any: Vec<String>,

    /// Issue type hints that suggest this role.
    #[serde(default)]
    pub issue_types_any: Vec<String>,

    /// Domain strings that suggest this role.
    #[serde(default)]
    pub domains_any: Vec<String>,
}

impl RoleRoutingHints {
    fn is_empty(&self) -> bool {
        self.paths_any.is_empty()
            && self.labels_any.is_empty()
            && self.issue_types_any.is_empty()
            && self.domains_any.is_empty()
    }
}

// ---------------------------------------------------------------------------
// agents (SPEC v2 §4.5 / §5.5)
// ---------------------------------------------------------------------------

/// Backend class for an [`AgentBackendProfile`]. SPEC v5 removes the
/// former Hermes backend; product backends are `codex` and `claude`.
/// The test-only `mock` variant is preserved here so quickstart
/// fixtures and unit tests can declare profiles without an external
/// binary.
///
/// Composite/tandem behaviour is *not* a backend — SPEC §5.5 defines it
/// as an `agents:` *strategy* that composes other profiles, modelled
/// here as [`AgentCompositeProfile`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentBackend {
    /// `codex app-server` over JSONL stdio.
    Codex,
    /// `claude -p --output-format stream-json` over stdio.
    Claude,
    /// In-process scripted runner. Test/fixture only — not advertised
    /// as a production backend.
    Mock,
}

impl Serialize for AgentBackend {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(match self {
            AgentBackend::Codex => "codex",
            AgentBackend::Claude => "claude",
            AgentBackend::Mock => "mock",
        })
    }
}

impl<'de> Deserialize<'de> for AgentBackend {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        match raw.as_str() {
            "codex" => Ok(AgentBackend::Codex),
            "claude" => Ok(AgentBackend::Claude),
            "mock" => Ok(AgentBackend::Mock),
            "hermes" => Err(serde::de::Error::custom(
                "backend `hermes` is no longer supported; see SPEC_v5.md §10",
            )),
            other => Err(serde::de::Error::unknown_variant(
                other,
                &["codex", "claude", "mock"],
            )),
        }
    }
}

/// One entry in the `agents` map (SPEC v2 §5.5).
///
/// A profile is either a concrete [`AgentBackendProfile`] (`backend:
/// codex|claude|mock`) or an [`AgentCompositeProfile`] that
/// composes other profiles via a strategy (`strategy: tandem`).
/// Discriminated by the field name `backend` vs `strategy`; serde tries
/// the backend variant first.
///
/// Profile names live in their own keyspace and are referenced from
/// [`RoleConfig::agent`] (and from `lead`/`follower` inside a composite
/// profile), so the same profile can be reused across roles and a role
/// can be re-pointed at a different profile without renaming.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum AgentProfileConfig {
    /// Concrete backend profile (`codex`, `claude`, or `mock`).
    /// Boxed so the enum size is dominated by the smaller composite
    /// variant rather than the policy-heavy backend struct.
    Backend(Box<AgentBackendProfile>),
    /// Composite agent that wraps two configured profiles in a strategy
    /// (today: `tandem`). Repackages the existing tandem runner as an
    /// `agents:` entry so it composes any combination of `codex`,
    /// `claude`, or `mock` lead/follower seats.
    Composite(AgentCompositeProfile),
}

impl<'de> Deserialize<'de> for AgentProfileConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_yaml::Value::deserialize(deserializer)?;
        let Some(map) = value.as_mapping() else {
            return Err(serde::de::Error::custom(
                "agent profile must be a mapping with either `backend` or `strategy`",
            ));
        };
        let backend_key = serde_yaml::Value::String("backend".into());
        let strategy_key = serde_yaml::Value::String("strategy".into());
        let has_backend = map.contains_key(&backend_key);
        let has_strategy = map.contains_key(&strategy_key);
        match (has_backend, has_strategy) {
            (true, false) => {
                let profile: AgentBackendProfile =
                    serde_yaml::from_value(value).map_err(serde::de::Error::custom)?;
                Ok(AgentProfileConfig::Backend(Box::new(profile)))
            }
            (false, true) => {
                let profile: AgentCompositeProfile =
                    serde_yaml::from_value(value).map_err(serde::de::Error::custom)?;
                Ok(AgentProfileConfig::Composite(profile))
            }
            (true, true) => Err(serde::de::Error::custom(
                "agent profile must declare only one of `backend` or `strategy`",
            )),
            (false, false) => Err(serde::de::Error::custom(
                "agent profile must declare either `backend` or `strategy`",
            )),
        }
    }
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
/// the global [`AgentConfig`]/[`CodexConfig`] defaults at dispatch time.
/// This keeps profiles small and lets
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

    /// Executable or shell entrypoint. `None` defers to the backend's
    /// default command (e.g. `codex app-server` for legacy fixtures).
    #[serde(default)]
    pub command: Option<String>,

    /// Structured base arguments for the backend command.
    #[serde(default)]
    pub args: Vec<String>,

    /// Operator-supplied arguments appended after Symphony-required
    /// protocol flags when backend ordering requires that.
    #[serde(default)]
    pub extra_args: Vec<String>,

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

    /// Symphony MCP tool names enabled for this profile. Empty means
    /// no Symphony MCP tools are injected.
    #[serde(default)]
    pub mcp_tools: Vec<String>,

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

    /// v3 dependency-orchestration policy for proposal-local
    /// `depends_on` edges. These controls decide whether dependency
    /// edges are materialized into durable `blocks` rows, whether those
    /// rows are mirrored to the tracker, whether they gate specialist
    /// dispatch, and whether decomposition-sourced blockers auto-resolve
    /// when their prerequisite child reaches a terminal state.
    #[serde(default)]
    pub dependency_policy: DependencyPolicy,
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
            dependency_policy: DependencyPolicy::default(),
        }
    }
}

fn default_decomposition_max_depth() -> u32 {
    2
}

fn default_true() -> bool {
    true
}

/// v3 policy for turning decomposition-local `depends_on` declarations
/// into durable dependency edges and optional tracker-side blockers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DependencyPolicy {
    /// Persist dependency edges as local `blocks` rows. Defaults on
    /// because local durable state is the enforcement source.
    #[serde(default = "default_true")]
    pub materialize_edges: bool,

    /// How aggressively to mirror local dependency edges to the
    /// configured tracker when it has native structural blocker support.
    #[serde(default)]
    pub tracker_sync: DependencyTrackerSyncPolicy,

    /// Gate specialist dispatch on incoming open dependency blockers.
    /// Defaults on; disabling it makes dependency edges observational.
    #[serde(default = "default_true")]
    pub dispatch_gate: bool,

    /// Resolve decomposition-sourced blockers automatically when their
    /// prerequisite child reaches a workflow-terminal state.
    #[serde(default = "default_true")]
    pub auto_resolve_on_terminal: bool,
}

impl Default for DependencyPolicy {
    fn default() -> Self {
        Self {
            materialize_edges: true,
            tracker_sync: DependencyTrackerSyncPolicy::default(),
            dispatch_gate: true,
            auto_resolve_on_terminal: true,
        }
    }
}

/// Tracker sync policy for v3 decomposition dependency edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyTrackerSyncPolicy {
    /// Tracker-native blocker creation must succeed or be recorded as
    /// required retry state before the decomposition can be fully applied.
    Required,
    /// Best-effort mirror to the tracker while durable local gating stays
    /// authoritative. Default.
    #[default]
    BestEffort,
    /// Never call tracker blocker APIs for decomposition dependencies.
    LocalOnly,
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
// qa (SPEC v2 §5.12)
// ---------------------------------------------------------------------------

/// QA gate policy.
///
/// SPEC v2 §5.12 makes QA a first-class gate: by the time a parent
/// closes, QA must either have signed off or been waived by an
/// authorised role. This struct is the typed mirror of the YAML
/// surface; the kernel consumes the discriminants on it (notably
/// [`BlockerPolicy`] and the [`QaEvidenceRequired`] flags) directly.
///
/// Defaults are deliberately conservative: `required: false` with no
/// `owner_role` means an omitted `qa:` block behaves identically to v1
/// (no QA gate). When the operator flips `required: true`, later
/// phases enforce that `owner_role` resolves to a role of
/// `kind: qa_gate` and that any waiver references resolve to
/// configured roles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QaConfig {
    /// Role name (key into [`WorkflowConfig::roles`]) responsible for
    /// running the QA gate. Optional at parse time so partial fixtures
    /// can declare an empty `qa:` block; later phases enforce that the
    /// reference resolves to a role of `kind: qa_gate` whenever
    /// `required` is true.
    #[serde(default)]
    pub owner_role: Option<String>,

    /// Master switch. When `false` (the default), the orchestrator
    /// skips the QA gate entirely and the integration owner can
    /// promote PRs / close parents without a QA verdict. The SPEC
    /// recommends `true` for any workflow with non-trivial runtime
    /// behaviour; the default stays off so a stub workflow loads
    /// cleanly.
    #[serde(default)]
    pub required: bool,

    /// When `true` (the default once enabled), QA is expected to walk
    /// every acceptance criterion against the final integration
    /// branch, not just the diff. SPEC v2 §5.12 calls this out
    /// explicitly because QA reading just the unified diff is the
    /// most common way QA misses runtime regressions.
    #[serde(default = "default_true")]
    pub exhaustive: bool,

    /// When `false` (the default), QA MUST capture runtime/visual
    /// evidence for runtime-sensitive work (TUI, UI, async I/O). A
    /// value of `true` lets QA close out from static review alone —
    /// use only for read-only tooling or workflows that explicitly
    /// accept the static-only risk.
    #[serde(default)]
    pub allow_static_only: bool,

    /// When `true` (the default), QA may file blockers that route
    /// back to specialists or the integration owner. Setting this to
    /// `false` neuters QA's authority and is rarely correct; it is
    /// preserved for read-only environments where the tracker
    /// adapter cannot create blocker records.
    #[serde(default = "default_true")]
    pub can_file_blockers: bool,

    /// When `true` (the default), QA may file follow-up issues for
    /// adjacent problems uncovered during review. Pairs with the
    /// follow-up policy in SPEC v2 §5.13 (typed in a later phase).
    #[serde(default = "default_true")]
    pub can_file_followups: bool,

    /// What a QA-filed blocker means for the parent. Defaults to
    /// [`BlockerPolicy::BlocksParent`] per SPEC v2 §5.12: an open QA
    /// blocker keeps the parent from closing until a waiver or
    /// resolution lands.
    #[serde(default)]
    pub blocker_policy: BlockerPolicy,

    /// Role names (keys into [`WorkflowConfig::roles`]) authorised to
    /// record a `waived` QA verdict. Empty by default — a fresh
    /// workflow refuses waivers until an operator names the
    /// authorising role explicitly. The SPEC requires every waiver
    /// to include a reason; that's enforced on the verdict surface
    /// rather than this list.
    #[serde(default)]
    pub waiver_roles: Vec<String>,

    /// Evidence categories QA must produce for an accepting verdict.
    /// SPEC v2 §5.12 lists four bundled defaults; per-category
    /// flipping (e.g. dropping `visual_or_runtime_evidence_when_applicable`
    /// for read-only adapters) is allowed but discouraged. See
    /// [`QaEvidenceRequired`].
    #[serde(default)]
    pub evidence_required: QaEvidenceRequired,

    /// When `true` (the default), the QA gate reruns automatically
    /// once every blocker it filed is resolved. Setting this to
    /// `false` keeps QA on the manual queue after a blocker round —
    /// useful when the QA agent is expensive and operators want to
    /// batch reruns.
    #[serde(default = "default_true")]
    pub rerun_after_blockers_resolved: bool,
}

impl Default for QaConfig {
    fn default() -> Self {
        Self {
            owner_role: None,
            required: false,
            exhaustive: true,
            allow_static_only: false,
            can_file_blockers: true,
            can_file_followups: true,
            blocker_policy: BlockerPolicy::default(),
            waiver_roles: Vec::new(),
            evidence_required: QaEvidenceRequired::default(),
            rerun_after_blockers_resolved: true,
        }
    }
}

/// What a QA-filed blocker means for parent completion (SPEC v2
/// §5.12 `blocker_policy`).
///
/// Defaults to [`BlockerPolicy::BlocksParent`]: the kernel refuses to
/// close the parent while any QA blocker is open unless a role from
/// `qa.waiver_roles` records a waiver with a reason.
/// [`BlockerPolicy::Advisory`] is the escape hatch for workflows that
/// want QA's voice surfaced as comments/labels without the hard gate
/// — rarely correct, but required for some read-only audit pipelines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockerPolicy {
    /// QA blockers block parent completion until resolved or waived.
    /// Default per SPEC v2 §5.12.
    #[default]
    BlocksParent,
    /// QA blockers are recorded but do not gate parent completion.
    /// Use only for advisory pipelines where QA's role is reporting,
    /// not gating.
    Advisory,
}

/// Evidence categories the QA gate must capture for an accepting
/// verdict (SPEC v2 §5.12 `evidence_required`).
///
/// All four flags default to `true`. Per-category flipping is
/// allowed (e.g. `visual_or_runtime_evidence_when_applicable: false`
/// for a workflow whose only runtime is HTTP that CI already
/// covers), but the kernel surfaces a warning whenever an evidence
/// category is dropped so operators don't quietly weaken QA over
/// time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QaEvidenceRequired {
    /// Test runs (unit/integration as applicable) must be exercised
    /// against the integration branch. Default `true`.
    #[serde(default = "default_true")]
    pub tests: bool,

    /// Every changed file in the integration diff must be reviewed.
    /// Default `true`. Pairs with `exhaustive` on [`QaConfig`]: the
    /// flag here speaks to *coverage*, the `exhaustive` flag speaks
    /// to *depth*.
    #[serde(default = "default_true")]
    pub changed_files_review: bool,

    /// Each acceptance criterion on the work item must be traced to
    /// concrete evidence (test, screenshot, log excerpt, etc.).
    /// Default `true`.
    #[serde(default = "default_true")]
    pub acceptance_criteria_trace: bool,

    /// Runtime-sensitive work (TUI, UI, async I/O) must include
    /// visual or runtime evidence (screenshot, recording, captured
    /// log). When the work is not runtime-sensitive QA records the
    /// not-applicable rationale; this flag never silently degrades
    /// to "skipped". Default `true`.
    #[serde(default = "default_true")]
    pub visual_or_runtime_evidence_when_applicable: bool,
}

impl Default for QaEvidenceRequired {
    fn default() -> Self {
        Self {
            tests: true,
            changed_files_review: true,
            acceptance_criteria_trace: true,
            visual_or_runtime_evidence_when_applicable: true,
        }
    }
}

// ---------------------------------------------------------------------------
// followups (SPEC v2 §5.13)
// ---------------------------------------------------------------------------

/// Follow-up issue policy.
///
/// A follow-up is newly discovered work that falls outside the current
/// acceptance criteria. SPEC v2 §5.13 makes follow-up creation a
/// first-class agent literacy: any role may identify follow-up work,
/// and the workflow decides whether the issue is filed directly or
/// queued for approval.
///
/// `default_policy` reuses [`ChildIssuePolicy`] — the same enum that
/// gates `decomposition.child_issue_policy` (§5.7) — so an operator who
/// learns one knob learns the other. Defaults are conservative:
/// `enabled: false`, [`ChildIssuePolicy::ProposeForApproval`], a
/// recognisable label pair (`follow-up` / `blocker`), and both reason
/// and acceptance-criteria fields required so an agent cannot drop a
/// vague "TODO" into the tracker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FollowupConfig {
    /// Master switch. When `false` (the default), the runner ignores
    /// follow-up requests in agent handoffs — they are dropped after
    /// being recorded for observability but no tracker mutation occurs.
    /// Operators must opt in by setting `enabled: true`.
    #[serde(default)]
    pub enabled: bool,

    /// What to do when an agent emits a follow-up: write the issue
    /// straight through `TrackerMutations` or queue it for approval.
    /// Shares [`ChildIssuePolicy`] with `decomposition.child_issue_policy`
    /// (SPEC v2 §5.7 / §5.13). Defaults to
    /// [`ChildIssuePolicy::ProposeForApproval`] — the safer path.
    #[serde(default)]
    pub default_policy: ChildIssuePolicy,

    /// Role name (key into [`WorkflowConfig::roles`]) responsible for
    /// approving follow-ups when `default_policy` is
    /// [`ChildIssuePolicy::ProposeForApproval`]. Optional at parse
    /// time so a partial fixture can declare an empty `followups:`
    /// block; later phases enforce that the reference resolves to a
    /// configured role whenever `enabled` is true.
    #[serde(default)]
    pub approval_role: Option<String>,

    /// Optional tracker state to assign newly created follow-up issues.
    /// When set, the value MUST NOT overlap `tracker.active_states`
    /// (case-insensitive): the contract is that freshly filed
    /// follow-ups start dormant so intake does not immediately pull them
    /// back into the active queue. Human operators promote them into an
    /// active state (for example `Todo`) when the team is ready to work
    /// them.
    #[serde(default)]
    pub initial_tracker_state: Option<String>,

    /// Tracker label applied to non-blocking follow-ups. Defaults to
    /// `"follow-up"` per SPEC v2 §5.13. Empty string means "do not
    /// label", which is preserved as operator intent.
    #[serde(default = "default_followup_non_blocking_label")]
    pub non_blocking_label: String,

    /// Tracker label applied to blocking follow-ups (i.e. follow-ups
    /// the agent flagged as blocking current acceptance). Defaults to
    /// `"blocker"` per SPEC v2 §5.13.
    #[serde(default = "default_followup_blocking_label")]
    pub blocking_label: String,

    /// When `true` (the default), every follow-up MUST include a
    /// reason; the kernel rejects handoffs whose follow-up payloads
    /// drop the field. SPEC v2 §5.13 requires the reason so triage
    /// can decide whether to accept the follow-up.
    #[serde(default = "default_true")]
    pub require_reason: bool,

    /// When `true` (the default), every follow-up MUST include
    /// acceptance criteria. SPEC v2 §5.13 lists this alongside
    /// `require_reason` so a follow-up enters the tracker with the
    /// same minimum content guarantees as a normal child issue.
    #[serde(default = "default_true")]
    pub require_acceptance_criteria: bool,
}

impl Default for FollowupConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            default_policy: ChildIssuePolicy::default(),
            approval_role: None,
            initial_tracker_state: None,
            non_blocking_label: default_followup_non_blocking_label(),
            blocking_label: default_followup_blocking_label(),
            require_reason: true,
            require_acceptance_criteria: true,
        }
    }
}

fn default_followup_non_blocking_label() -> String {
    "follow-up".to_string()
}

fn default_followup_blocking_label() -> String {
    "blocker".to_string()
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

    /// `concurrency.global_max` must be a positive integer when set.
    /// Mirrors [`ConfigValidationError::MaxConcurrentZero`] for the
    /// typed replacement of the deprecated `agent.max_concurrent_agents`
    /// alias.
    #[error("concurrency.global_max must be > 0 when set (got {0})")]
    ConcurrencyGlobalMaxZero(u32),

    /// A `concurrency.per_agent_profile` or `concurrency.per_repository`
    /// entry has a zero cap. Zero would deadlock the gate at the
    /// matching scope, so reject at load time rather than wedge a
    /// running orchestrator.
    #[error("{field} cap for `{key}` must be > 0 (got 0)")]
    ConcurrencyZeroCap {
        /// `concurrency.per_agent_profile` or
        /// `concurrency.per_repository`.
        field: String,
        /// The offending map key (profile name or repository slug).
        key: String,
    },

    /// A `concurrency.per_agent_profile` key does not resolve to a
    /// profile defined in [`WorkflowConfig::agents`].
    #[error(
        "concurrency.per_agent_profile references profile `{profile}` which is not defined under `agents`"
    )]
    UnknownConcurrencyAgentProfile {
        /// The agent profile name that failed to resolve.
        profile: String,
    },

    /// A `concurrency.per_repository` key does not resolve to a known
    /// repository slug. Today the only recognised slug is
    /// [`TrackerConfig::repository`]; future multi-repo support will
    /// widen the registry without a schema change.
    #[error(
        "concurrency.per_repository references repository `{repository}` which is not a known repository slug"
    )]
    UnknownConcurrencyRepository {
        /// The repository slug that failed to resolve.
        repository: String,
    },

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

    /// `observability.sse.replay_buffer` must be > 0;
    /// `tokio::sync::broadcast` rejects zero-capacity channels at
    /// construction time.
    #[error("observability.sse.replay_buffer must be > 0 (got {0})")]
    ObservabilitySseReplayBufferZero(usize),

    /// `observability.sse.bind` must be a parseable `SocketAddr`.
    #[error("observability.sse.bind is not a valid socket address: {0}")]
    ObservabilitySseBindInvalid(String),

    /// At least one role must declare `kind: integration_owner` once any
    /// integration-owner-driven feature is active (decomposition,
    /// integration `required_for`, or pull-request lifecycle). SPEC v2
    /// §5.4 / §5.7 / §5.10 / §5.11.
    #[error(
        "workflow requires at least one role with `kind: integration_owner` because {feature} is active"
    )]
    MissingIntegrationOwnerRole {
        /// Which feature triggered the requirement (for operator-facing
        /// diagnostics — `decomposition`, `integration`, or
        /// `pull_requests`).
        feature: String,
    },

    /// At least one role must declare `kind: qa_gate` when QA is
    /// required. SPEC v2 §5.4 / §5.12.
    #[error("workflow requires at least one role with `kind: qa_gate` because qa.required is true")]
    MissingQaGateRole,

    /// Decomposition needs at least one role that can receive normal
    /// child implementation/review/operator work in the generated
    /// platform-lead catalog.
    #[error(
        "decomposition.enabled requires at least one child-owning role (`specialist`, `reviewer`, or `operator`) for the platform-lead catalog"
    )]
    MissingEligibleChildOwningRole,

    /// A `*_role` field references a role name that is not defined in
    /// `roles`. Covers `routing.default_role`, every `routing.rules[].assign_role`,
    /// `decomposition.owner_role`, `integration.owner_role`,
    /// `pull_requests.owner_role`, `qa.owner_role`, every entry in
    /// `qa.waiver_roles`, and `followups.approval_role`.
    #[error("{field} references role `{role}` which is not defined under `roles`")]
    UnknownRoleReference {
        /// Dotted-path locator of the offending field, e.g.
        /// `routing.rules[2].assign_role`.
        field: String,
        /// The role name that failed to resolve.
        role: String,
    },

    /// A `*_role` field references a role whose `kind` does not match
    /// the kind required by that field. SPEC v2 §5.4 ties decomposition,
    /// integration, and PR ownership to `kind: integration_owner` and
    /// the QA owner to `kind: qa_gate`.
    #[error("{field} references role `{role}` of kind {actual:?}, expected {expected:?}")]
    RoleKindMismatch {
        /// Dotted-path locator of the offending field.
        field: String,
        /// The configured role name.
        role: String,
        /// The role's declared kind.
        actual: RoleKind,
        /// The kind required by the field.
        expected: RoleKind,
    },

    /// A `RoleConfig::agent` field references an agent profile that is
    /// not defined in `agents`. SPEC v2 §5.4 / §5.5.
    #[error("role `{role}`.agent references profile `{agent}` which is not defined under `agents`")]
    UnknownAgentProfileReference {
        /// Role name whose `agent` field failed to resolve.
        role: String,
        /// The agent profile name that was not found.
        agent: String,
    },

    /// A composite agent profile's `lead` or `follower` references a
    /// profile that is not defined in `agents`. SPEC v2 §5.5.
    #[error(
        "composite agent `{profile}`.{seat} references profile `{target}` which is not defined under `agents`"
    )]
    UnknownCompositeAgentReference {
        /// Composite profile name (key in `agents`).
        profile: String,
        /// Seat (`lead` or `follower`) that failed to resolve.
        seat: String,
        /// The referenced profile name that was not found.
        target: String,
    },

    /// A composite agent profile points at another composite profile.
    /// SPEC v2 §5.5 forbids composite-of-composite — `lead`/`follower`
    /// MUST resolve to concrete backend profiles.
    #[error(
        "composite agent `{profile}`.{seat} points at composite profile `{target}`; lead/follower must reference concrete backend profiles"
    )]
    NestedCompositeAgentProfile {
        /// Outer composite profile name.
        profile: String,
        /// Seat (`lead` or `follower`) that nested.
        seat: String,
        /// The inner composite profile name.
        target: String,
    },

    /// A backend profile configured a model for a backend that does not
    /// support model override wiring.
    #[error(
        "agent profile `{profile}` uses backend {backend:?}, which does not support `model` overrides"
    )]
    UnsupportedAgentProfileModel {
        /// Profile name.
        profile: String,
        /// Backend that cannot accept model overrides.
        backend: AgentBackend,
    },

    /// A profile mixed legacy whitespace command strings with
    /// structured args.
    #[error(
        "agent profile `{profile}` sets structured args but command `{command}` contains whitespace; use command as executable and move arguments into args/extra_args"
    )]
    UnsupportedAgentArgsOrdering {
        /// Profile name.
        profile: String,
        /// Configured command.
        command: String,
    },

    /// `workspace.default_strategy` references a strategy that is not
    /// defined in `workspace.strategies`. SPEC v2 §5.8.
    #[error("workspace.default_strategy `{0}` is not defined under workspace.strategies")]
    UnknownDefaultWorkspaceStrategy(String),

    /// `tracker.active_states` and `tracker.terminal_states` must be
    /// disjoint. Comparison is case-insensitive because SPEC v2 §5.3.1
    /// documents tracker state matching as case-insensitive.
    #[error(
        "tracker state `{state}` appears in both tracker.active_states and tracker.terminal_states"
    )]
    DuplicateTrackerState {
        /// The offending state name (preserved in the operator's casing).
        state: String,
    },

    /// `followups.initial_tracker_state`, when set, must keep newly
    /// created follow-up issues outside the active intake set.
    #[error(
        "followups.initial_tracker_state `{state}` must not appear in tracker.active_states"
    )]
    FollowupInitialTrackerStateIsActive {
        /// The configured follow-up initial tracker state.
        state: String,
    },

    /// `decomposition.max_depth` must be > 0 once decomposition is
    /// enabled — a zero depth would forbid the integration owner from
    /// decomposing anything, which contradicts `enabled: true`.
    #[error("decomposition.max_depth must be > 0 when decomposition.enabled is true (got {0})")]
    DecompositionMaxDepthZero(u32),

    /// A branch template is empty or missing the required
    /// `{{identifier}}` placeholder. SPEC v2 §5.9 documents both
    /// `child_branch_template` and `integration_branch_template` as
    /// per-issue templates; without `{{identifier}}` the orchestrator
    /// would mint colliding branch names.
    #[error("branching.{field} `{template}` is invalid: {reason}")]
    InvalidBranchTemplate {
        /// `child_branch_template` or `integration_branch_template`.
        field: String,
        /// The offending template string (preserved verbatim).
        template: String,
        /// Human-readable reason for the rejection.
        reason: String,
    },

    /// `pull_requests.enabled: true` requires a GitHub-capable tracker.
    /// SPEC v2 §5.11 only documents the GitHub PR surface today; other
    /// trackers cannot satisfy the create/update/mark-ready mutations.
    #[error("pull_requests.enabled requires tracker.kind = github (got {actual:?})")]
    PullRequestsRequireGithubTracker {
        /// The configured tracker kind that does not support PRs.
        actual: TrackerKind,
    },

    /// `decomposition.dependency_policy.tracker_sync: required` requires
    /// a tracker with native structural blocker support. SPEC v3 §8.2
    /// forbids label/comment-only representations from satisfying this.
    #[error(
        "decomposition.dependency_policy.tracker_sync = required requires TrackerCapabilities.add_blocker = true (tracker.kind = {actual:?})"
    )]
    DependencyTrackerSyncRequiresAddBlocker {
        /// The configured tracker kind that does not advertise
        /// structural blocker/dependency creation.
        actual: TrackerKind,
    },

    /// Direct decomposition child creation requires native parent/child
    /// support. Workflows using trackers without that support must keep
    /// child creation in approval/advisory mode so the tracker is not
    /// treated as structural truth by convention alone.
    #[error(
        "decomposition.child_issue_policy = create_directly requires TrackerCapabilities.link_parent_child = true (tracker.kind = {actual:?}); use propose_for_approval for advisory/local-only parent links"
    )]
    DecompositionDirectChildCreationRequiresParentLink {
        /// The configured tracker kind that does not advertise
        /// structural parent/child linking.
        actual: TrackerKind,
    },

    /// Workspace strategy and branching policy disagree on shared
    /// branch usage. SPEC v2 §5.9 chooses the safer setting by
    /// rejecting the config rather than silently picking one side.
    #[error("contradictory shared-branch policy: {detail}")]
    ContradictorySharedBranchPolicy {
        /// Human-readable description of which two settings disagree.
        detail: String,
    },

    /// A `budgets.*` numeric cap is zero. SPEC v2 §5.15 budgets are
    /// hard ceilings; a zero ceiling would either deadlock the
    /// dispatcher or contradict the operator's opt-in to a non-zero
    /// retry policy. Rejected at load time rather than wedge a running
    /// orchestrator. The `field` carries the dotted path
    /// (e.g. `budgets.max_parallel_runs`) so operators can fix it
    /// without grepping.
    #[error("{field} must be > 0 (got 0)")]
    BudgetZero {
        /// Dotted-path locator of the offending field, e.g.
        /// `budgets.max_parallel_runs` or `budgets.max_cost_per_issue_usd`.
        field: String,
    },
}

/// Non-fatal validation warnings raised by [`WorkflowConfig::validation_warnings`].
///
/// These represent accepted-but-risky workflow choices. They are kept
/// typed so CLIs and future JSON output can render loud operator
/// warnings without scraping strings from logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigValidationWarning {
    /// `dispatch_gate: false` makes dependency edges observability-only:
    /// decomposition dependencies can be rendered, but they no longer
    /// stop specialist dispatch.
    DependencyDispatchGateDisabled,
    /// A specialist role has only a terse description and no structured
    /// assignment metadata for the platform-lead catalog.
    SpecialistRoleMissingAssignmentMetadata { role: String },
    /// Decomposition is enabled but the generated catalog will lean on
    /// fallback descriptions/routing rules for most child-owning roles.
    DecompositionCatalogMostlyFallback,
}

impl std::fmt::Display for ConfigValidationWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DependencyDispatchGateDisabled => write!(
                f,
                "WARNING: decomposition.dependency_policy.dispatch_gate = false makes dependency edges observability-only; specialists may dispatch before prerequisites clear"
            ),
            Self::SpecialistRoleMissingAssignmentMetadata { role } => write!(
                f,
                "WARNING: role `{role}` has no structured assignment metadata; platform-lead catalog will fall back to terse description/routing hints"
            ),
            Self::DecompositionCatalogMostlyFallback => write!(
                f,
                "WARNING: decomposition is enabled but most child-owning roles lack structured assignment metadata; platform-lead catalog may be too thin for reliable assignment"
            ),
        }
    }
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
        if self.observability.sse.enabled {
            if self.observability.sse.replay_buffer == 0 {
                return Err(ConfigValidationError::ObservabilitySseReplayBufferZero(
                    self.observability.sse.replay_buffer,
                ));
            }
            // Parse-only; the runtime binds later. We surface the
            // failure at validate-time so `symphony validate` catches
            // it before the daemon is ever started.
            if self
                .observability
                .sse
                .bind
                .parse::<std::net::SocketAddr>()
                .is_err()
            {
                return Err(ConfigValidationError::ObservabilitySseBindInvalid(
                    self.observability.sse.bind.clone(),
                ));
            }
        }
        self.validate_tracker_states()?;
        self.validate_followup_initial_tracker_state()?;
        self.validate_branching_templates()?;
        self.validate_decomposition_depth()?;
        self.validate_dependency_policy_tracker_capabilities()?;
        self.validate_decomposition_parent_link_capabilities()?;
        self.validate_pr_provider_pairing()?;
        self.validate_shared_branch_consistency()?;
        self.validate_workspace_default_strategy()?;
        self.validate_role_and_agent_references()?;
        self.validate_required_role_kinds()?;
        self.validate_decomposition_catalog_roles()?;
        self.validate_concurrency()?;
        self.validate_budgets()?;
        Ok(())
    }

    /// Return accepted-but-risky workflow choices that operators should
    /// see during validation. Warnings never make the config invalid;
    /// callers that want a stricter policy can promote them at their
    /// own boundary.
    pub fn validation_warnings(&self) -> Vec<ConfigValidationWarning> {
        let mut warnings = Vec::new();
        if !self.decomposition.dependency_policy.dispatch_gate {
            warnings.push(ConfigValidationWarning::DependencyDispatchGateDisabled);
        }
        let child_roles: Vec<(&String, &RoleConfig)> = self
            .roles
            .iter()
            .filter(|(_, role)| role.kind.child_owning_catalog_role())
            .collect();
        let child_role_count = child_roles.len();
        let mut thin_child_roles = 0usize;
        for (name, role) in child_roles {
            if role.assignment.is_empty() {
                thin_child_roles += 1;
                if role.kind == RoleKind::Specialist {
                    warnings.push(
                        ConfigValidationWarning::SpecialistRoleMissingAssignmentMetadata {
                            role: name.clone(),
                        },
                    );
                }
            }
        }
        if self.decomposition.enabled && thin_child_roles > child_role_count.saturating_div(2) {
            warnings.push(ConfigValidationWarning::DecompositionCatalogMostlyFallback);
        }
        warnings
    }

    /// Validate the typed [`BudgetsConfig`] block. Every numeric cap
    /// must be strictly positive; SPEC v2 §5.15 documents these as
    /// hard ceilings, and a zero ceiling cannot be enforced
    /// meaningfully. `pause_policy` is an enum and parses or fails
    /// at deserialise time, so it does not need a runtime check.
    fn validate_budgets(&self) -> Result<(), ConfigValidationError> {
        if self.budgets.max_parallel_runs == 0 {
            return Err(ConfigValidationError::BudgetZero {
                field: "budgets.max_parallel_runs".to_string(),
            });
        }
        if self.budgets.max_cost_per_issue_usd <= 0.0 {
            return Err(ConfigValidationError::BudgetZero {
                field: "budgets.max_cost_per_issue_usd".to_string(),
            });
        }
        if self.budgets.max_turns_per_run == 0 {
            return Err(ConfigValidationError::BudgetZero {
                field: "budgets.max_turns_per_run".to_string(),
            });
        }
        if self.budgets.max_retries == 0 {
            return Err(ConfigValidationError::BudgetZero {
                field: "budgets.max_retries".to_string(),
            });
        }
        Ok(())
    }

    /// Validate the typed [`ConcurrencyConfig`] block. Rejects zero
    /// caps, unknown agent-profile keys, and unknown repository
    /// slugs. Per-role caps are validated against `roles` elsewhere.
    fn validate_concurrency(&self) -> Result<(), ConfigValidationError> {
        if let Some(0) = self.concurrency.global_max {
            return Err(ConfigValidationError::ConcurrencyGlobalMaxZero(0));
        }
        for (profile, cap) in &self.concurrency.per_agent_profile {
            if *cap == 0 {
                return Err(ConfigValidationError::ConcurrencyZeroCap {
                    field: "concurrency.per_agent_profile".to_string(),
                    key: profile.clone(),
                });
            }
            if !self.agents.contains_key(profile) {
                return Err(ConfigValidationError::UnknownConcurrencyAgentProfile {
                    profile: profile.clone(),
                });
            }
        }
        for (repo, cap) in &self.concurrency.per_repository {
            if *cap == 0 {
                return Err(ConfigValidationError::ConcurrencyZeroCap {
                    field: "concurrency.per_repository".to_string(),
                    key: repo.clone(),
                });
            }
            let known = self.tracker.repository.as_deref() == Some(repo.as_str());
            if !known {
                return Err(ConfigValidationError::UnknownConcurrencyRepository {
                    repository: repo.clone(),
                });
            }
        }
        Ok(())
    }

    fn validate_decomposition_catalog_roles(&self) -> Result<(), ConfigValidationError> {
        if self.decomposition.enabled
            && !self
                .roles
                .values()
                .any(|role| role.kind.child_owning_catalog_role())
        {
            return Err(ConfigValidationError::MissingEligibleChildOwningRole);
        }
        Ok(())
    }

    /// Effective global concurrency cap. Prefers
    /// [`ConcurrencyConfig::global_max`] when set, falling back to the
    /// deprecated [`AgentConfig::max_concurrent_agents`] alias so
    /// existing fixtures keep working unchanged.
    pub fn resolved_global_concurrency(&self) -> u32 {
        self.concurrency
            .global_max
            .unwrap_or(self.agent.max_concurrent_agents)
    }

    /// `active_states` and `terminal_states` must be disjoint
    /// (case-insensitive). A state name appearing in both lists would
    /// make the runner's "needs work vs. terminal" classification
    /// ambiguous — flag it at validate time instead of resolving the
    /// ambiguity at dispatch.
    fn validate_tracker_states(&self) -> Result<(), ConfigValidationError> {
        let active_lower: Vec<String> = self
            .tracker
            .active_states
            .iter()
            .map(|s| s.to_lowercase())
            .collect();
        for terminal in &self.tracker.terminal_states {
            if active_lower.contains(&terminal.to_lowercase()) {
                return Err(ConfigValidationError::DuplicateTrackerState {
                    state: terminal.clone(),
                });
            }
        }
        Ok(())
    }

    /// `followups.initial_tracker_state`, when configured, must point at
    /// a dormant state rather than one of `tracker.active_states`.
    fn validate_followup_initial_tracker_state(&self) -> Result<(), ConfigValidationError> {
        let Some(initial_state) = self.followups.initial_tracker_state.as_deref() else {
            return Ok(());
        };
        if self
            .tracker
            .active_states
            .iter()
            .any(|state| state.eq_ignore_ascii_case(initial_state))
        {
            return Err(ConfigValidationError::FollowupInitialTrackerStateIsActive {
                state: initial_state.to_string(),
            });
        }
        Ok(())
    }

    /// Branch templates MUST be non-empty and MUST contain the
    /// `{{identifier}}` placeholder so per-issue branch names do not
    /// collide. SPEC v2 §5.9.
    fn validate_branching_templates(&self) -> Result<(), ConfigValidationError> {
        for (field, template) in [
            (
                "child_branch_template",
                &self.branching.child_branch_template,
            ),
            (
                "integration_branch_template",
                &self.branching.integration_branch_template,
            ),
        ] {
            if template.is_empty() {
                return Err(ConfigValidationError::InvalidBranchTemplate {
                    field: field.to_string(),
                    template: template.clone(),
                    reason: "must not be empty".to_string(),
                });
            }
            if !template.contains("{{identifier}}") {
                return Err(ConfigValidationError::InvalidBranchTemplate {
                    field: field.to_string(),
                    template: template.clone(),
                    reason: "must contain `{{identifier}}` placeholder".to_string(),
                });
            }
        }
        Ok(())
    }

    /// `decomposition.max_depth` is only meaningful when decomposition
    /// is enabled; a zero depth there would forbid the very behaviour
    /// the operator opted into.
    fn validate_decomposition_depth(&self) -> Result<(), ConfigValidationError> {
        if self.decomposition.enabled && self.decomposition.max_depth == 0 {
            return Err(ConfigValidationError::DecompositionMaxDepthZero(
                self.decomposition.max_depth,
            ));
        }
        Ok(())
    }

    /// `tracker_sync: required` means the tracker must expose a native
    /// structural blocker relation. Keep this mapping close to config
    /// validation so `symphony validate` can fail before runtime adapter
    /// construction; adapter conformance tests separately ensure the
    /// concrete adapter capability flags match these product claims.
    fn validate_dependency_policy_tracker_capabilities(&self) -> Result<(), ConfigValidationError> {
        if self.decomposition.dependency_policy.tracker_sync
            == DependencyTrackerSyncPolicy::Required
            && !self.tracker.kind.supports_structural_blockers()
        {
            return Err(
                ConfigValidationError::DependencyTrackerSyncRequiresAddBlocker {
                    actual: self.tracker.kind,
                },
            );
        }
        Ok(())
    }

    /// `child_issue_policy: create_directly` means Symphony will create
    /// child tracker issues during decomposition and expects the tracker
    /// relation to be structural. Trackers that can only render advisory
    /// comments/text must use `propose_for_approval`, which keeps the
    /// local workflow truth explicit instead of pretending the tracker
    /// has a parent/child primitive.
    fn validate_decomposition_parent_link_capabilities(&self) -> Result<(), ConfigValidationError> {
        if self.decomposition.child_issue_policy == ChildIssuePolicy::CreateDirectly
            && !self.tracker.kind.supports_structural_parent_links()
        {
            return Err(
                ConfigValidationError::DecompositionDirectChildCreationRequiresParentLink {
                    actual: self.tracker.kind,
                },
            );
        }
        Ok(())
    }

    /// PR mutations require a GitHub-capable tracker — Linear and the
    /// in-memory mock cannot satisfy the SPEC v2 §5.11 surface.
    fn validate_pr_provider_pairing(&self) -> Result<(), ConfigValidationError> {
        if self.pull_requests.enabled && self.tracker.kind != TrackerKind::Github {
            return Err(ConfigValidationError::PullRequestsRequireGithubTracker {
                actual: self.tracker.kind,
            });
        }
        Ok(())
    }

    /// Workspace strategy and branching policy MUST agree on shared
    /// branch usage. SPEC v2 §5.9 rejects the config rather than
    /// guessing which side is authoritative.
    fn validate_shared_branch_consistency(&self) -> Result<(), ConfigValidationError> {
        if !self.branching.allow_same_branch_for_children {
            if let Some((name, _)) = self
                .workspace
                .strategies
                .iter()
                .find(|(_, s)| s.kind == WorkspaceStrategyKind::SharedBranch)
            {
                return Err(ConfigValidationError::ContradictorySharedBranchPolicy {
                    detail: format!(
                        "workspace.strategies.{name} is `shared_branch` but branching.allow_same_branch_for_children is false"
                    ),
                });
            }
            if self.integration.merge_strategy == MergeStrategy::SharedBranch {
                return Err(ConfigValidationError::ContradictorySharedBranchPolicy {
                    detail:
                        "integration.merge_strategy is `shared_branch` but branching.allow_same_branch_for_children is false"
                            .to_string(),
                });
            }
        }
        Ok(())
    }

    /// `workspace.default_strategy`, when set, MUST resolve to a
    /// configured strategy. Empty `strategies` plus `default_strategy:
    /// foo` is the most common typo path here.
    fn validate_workspace_default_strategy(&self) -> Result<(), ConfigValidationError> {
        if let Some(name) = &self.workspace.default_strategy
            && !self.workspace.strategies.contains_key(name)
        {
            return Err(ConfigValidationError::UnknownDefaultWorkspaceStrategy(
                name.clone(),
            ));
        }
        Ok(())
    }

    /// Resolve every workflow-level reference to a role or agent
    /// profile. SPEC v2 §5.4 / §5.5 / §5.6 / §5.7 / §5.10 / §5.11 /
    /// §5.12 / §5.13.
    fn validate_role_and_agent_references(&self) -> Result<(), ConfigValidationError> {
        // Routing
        if let Some(name) = &self.routing.default_role
            && !self.roles.contains_key(name)
        {
            return Err(ConfigValidationError::UnknownRoleReference {
                field: "routing.default_role".to_string(),
                role: name.clone(),
            });
        }
        for (i, rule) in self.routing.rules.iter().enumerate() {
            if !self.roles.contains_key(&rule.assign_role) {
                return Err(ConfigValidationError::UnknownRoleReference {
                    field: format!("routing.rules[{i}].assign_role"),
                    role: rule.assign_role.clone(),
                });
            }
        }

        // decomposition.owner_role: must resolve always; must be
        // integration_owner when decomposition is enabled.
        self.check_role_ref(
            "decomposition.owner_role",
            self.decomposition.owner_role.as_deref(),
            self.decomposition
                .enabled
                .then_some(RoleKind::IntegrationOwner),
        )?;

        // integration.owner_role: must be integration_owner when
        // `required_for` is non-empty.
        self.check_role_ref(
            "integration.owner_role",
            self.integration.owner_role.as_deref(),
            (!self.integration.required_for.is_empty()).then_some(RoleKind::IntegrationOwner),
        )?;

        // pull_requests.owner_role: must be integration_owner when
        // PR lifecycle is enabled.
        self.check_role_ref(
            "pull_requests.owner_role",
            self.pull_requests.owner_role.as_deref(),
            self.pull_requests
                .enabled
                .then_some(RoleKind::IntegrationOwner),
        )?;

        // qa.owner_role: must be qa_gate when `required` is true.
        self.check_role_ref(
            "qa.owner_role",
            self.qa.owner_role.as_deref(),
            self.qa.required.then_some(RoleKind::QaGate),
        )?;

        // qa.waiver_roles: must all resolve.
        for (i, name) in self.qa.waiver_roles.iter().enumerate() {
            if !self.roles.contains_key(name) {
                return Err(ConfigValidationError::UnknownRoleReference {
                    field: format!("qa.waiver_roles[{i}]"),
                    role: name.clone(),
                });
            }
        }

        // followups.approval_role: must resolve when set.
        self.check_role_ref(
            "followups.approval_role",
            self.followups.approval_role.as_deref(),
            None,
        )?;

        // role.agent must resolve in agents.
        for (role_name, role) in &self.roles {
            if let Some(agent) = &role.agent
                && !self.agents.contains_key(agent)
            {
                return Err(ConfigValidationError::UnknownAgentProfileReference {
                    role: role_name.clone(),
                    agent: agent.clone(),
                });
            }
        }

        // composite.lead/follower must resolve to concrete backend profiles.
        for (profile_name, profile) in &self.agents {
            if let AgentProfileConfig::Backend(backend) = profile {
                if backend.model.is_some() && matches!(backend.backend, AgentBackend::Mock) {
                    return Err(ConfigValidationError::UnsupportedAgentProfileModel {
                        profile: profile_name.clone(),
                        backend: backend.backend,
                    });
                }
                if backend
                    .command
                    .as_deref()
                    .is_some_and(|c| c.contains(char::is_whitespace))
                    && (!backend.args.is_empty() || !backend.extra_args.is_empty())
                {
                    return Err(ConfigValidationError::UnsupportedAgentArgsOrdering {
                        profile: profile_name.clone(),
                        command: backend.command.clone().unwrap_or_default(),
                    });
                }
            }
            if let AgentProfileConfig::Composite(c) = profile {
                for (seat, target) in [("lead", &c.lead), ("follower", &c.follower)] {
                    match self.agents.get(target) {
                        None => {
                            return Err(ConfigValidationError::UnknownCompositeAgentReference {
                                profile: profile_name.clone(),
                                seat: seat.to_string(),
                                target: target.clone(),
                            });
                        }
                        Some(AgentProfileConfig::Composite(_)) => {
                            return Err(ConfigValidationError::NestedCompositeAgentProfile {
                                profile: profile_name.clone(),
                                seat: seat.to_string(),
                                target: target.clone(),
                            });
                        }
                        Some(AgentProfileConfig::Backend(_)) => {}
                    }
                }
            }
        }

        Ok(())
    }

    /// Cross-cutting "at least one role of kind X" requirements. Run
    /// after [`Self::validate_role_and_agent_references`] so explicit
    /// owner_role typos surface first with a more specific error.
    fn validate_required_role_kinds(&self) -> Result<(), ConfigValidationError> {
        let need_integration_owner = if self.decomposition.enabled {
            Some("decomposition")
        } else if !self.integration.required_for.is_empty() {
            Some("integration")
        } else if self.pull_requests.enabled {
            Some("pull_requests")
        } else {
            None
        };
        if let Some(feature) = need_integration_owner {
            let has = self
                .roles
                .values()
                .any(|r| r.kind == RoleKind::IntegrationOwner);
            if !has {
                return Err(ConfigValidationError::MissingIntegrationOwnerRole {
                    feature: feature.to_string(),
                });
            }
        }
        if self.qa.required {
            let has = self.roles.values().any(|r| r.kind == RoleKind::QaGate);
            if !has {
                return Err(ConfigValidationError::MissingQaGateRole);
            }
        }
        Ok(())
    }

    /// Helper: confirm an `Option<&str>` role reference resolves and
    /// (optionally) matches an expected kind. Returns `Ok(())` for a
    /// `None` reference — required-when-X enforcement lives in the
    /// caller via the `expected_kind` argument plus the
    /// [`Self::validate_required_role_kinds`] sweep.
    fn check_role_ref(
        &self,
        field: &str,
        name: Option<&str>,
        expected_kind: Option<RoleKind>,
    ) -> Result<(), ConfigValidationError> {
        let Some(name) = name else { return Ok(()) };
        let Some(role) = self.roles.get(name) else {
            return Err(ConfigValidationError::UnknownRoleReference {
                field: field.to_string(),
                role: name.to_string(),
            });
        };
        if let Some(expected) = expected_kind
            && role.kind != expected
        {
            return Err(ConfigValidationError::RoleKindMismatch {
                field: field.to_string(),
                role: name.to_string(),
                actual: role.kind,
                expected,
            });
        }
        Ok(())
    }
}

impl TrackerKind {
    /// Product-level tracker capability map used by workflow validation.
    ///
    /// This mirrors the adapter capability contract without making
    /// `symphony-config` depend on runtime adapter crates. Linear has
    /// native issue relations; GitHub Issues and the fixture Mock do not.
    const fn supports_structural_blockers(self) -> bool {
        matches!(self, TrackerKind::Linear)
    }

    /// Product-level parent/child capability map used by workflow
    /// validation. Keep this separate from blocker support so a future
    /// tracker can support one structural relation without the other.
    const fn supports_structural_parent_links(self) -> bool {
        matches!(self, TrackerKind::Linear)
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
        assert_eq!(cfg.hooks.timeout_ms, 300_000);
        assert!(cfg.hooks.after_create.is_empty());
        assert!(cfg.hooks.before_run.is_empty());
        assert!(cfg.hooks.after_run.is_empty());
        assert!(cfg.hooks.before_remove.is_empty());
        assert_eq!(cfg.agent.kind, AgentKind::Codex);
        assert_eq!(cfg.agent.max_concurrent_agents, 10);
        assert_eq!(cfg.agent.max_turns, 20);
        assert_eq!(cfg.agent.max_retry_backoff_ms, 300_000);
        assert!(cfg.agent.max_concurrent_agents_by_state.is_empty());
        assert_eq!(cfg.codex.command, "codex app-server");
        assert_eq!(cfg.codex.turn_timeout_ms, 3_600_000);
        assert_eq!(cfg.codex.read_timeout_ms, 5_000);
        assert_eq!(cfg.codex.stall_timeout_ms, 300_000);
        assert!(cfg.observability.sse.enabled);
        assert_eq!(cfg.observability.sse.bind, "127.0.0.1:6280");
        assert_eq!(cfg.observability.sse.replay_buffer, 256);
        assert!(cfg.observability.event_bus);
        assert!(cfg.observability.tui.enabled);
        assert!(!cfg.observability.dashboard.enabled);
        assert_eq!(cfg.observability.logs.format, LogFormat::Json);
        assert!(cfg.workspace.root.is_none());
        assert!(cfg.workspace.default_strategy.is_none());
        assert!(cfg.workspace.strategies.is_empty());
        assert!(cfg.workspace.require_cwd_in_workspace);
        assert!(cfg.workspace.forbid_untracked_outside_workspace);
        assert_eq!(cfg.branching.default_base, "main");
        assert_eq!(
            cfg.branching.child_branch_template,
            "symphony/{{identifier}}"
        );
        assert_eq!(
            cfg.branching.integration_branch_template,
            "symphony/integration/{{identifier}}"
        );
        assert!(!cfg.branching.allow_same_branch_for_children);
        assert!(cfg.branching.require_clean_tree_before_run);
    }

    #[test]
    fn workspace_policy_parses_strategy_registry() {
        // Mirrors the SPEC v2 §5.8 worked example: a `git_worktree`
        // default plus a shared `existing_worktree` integration entry.
        let yaml = r#"
workspace:
  root: .symphony/workspaces
  default_strategy: issue_worktree
  strategies:
    issue_worktree:
      kind: git_worktree
      base: main
      branch_template: symphony/{{identifier}}
      cleanup: retain_until_done
    shared_integration:
      kind: existing_worktree
      path: ../project-integration
      require_branch: symphony/integration/{{parent_identifier}}
  require_cwd_in_workspace: true
  forbid_untracked_outside_workspace: true
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        assert_eq!(
            parsed.workspace.root.as_deref(),
            Some(std::path::Path::new(".symphony/workspaces"))
        );
        assert_eq!(
            parsed.workspace.default_strategy.as_deref(),
            Some("issue_worktree")
        );
        let issue = parsed.workspace.strategies.get("issue_worktree").unwrap();
        assert_eq!(issue.kind, WorkspaceStrategyKind::GitWorktree);
        assert_eq!(issue.base.as_deref(), Some("main"));
        assert_eq!(
            issue.branch_template.as_deref(),
            Some("symphony/{{identifier}}")
        );
        assert_eq!(issue.cleanup, WorkspaceCleanupPolicy::RetainUntilDone);
        let shared = parsed
            .workspace
            .strategies
            .get("shared_integration")
            .unwrap();
        assert_eq!(shared.kind, WorkspaceStrategyKind::ExistingWorktree);
        assert_eq!(
            shared.path.as_deref(),
            Some(std::path::Path::new("../project-integration"))
        );
        assert_eq!(
            shared.require_branch.as_deref(),
            Some("symphony/integration/{{parent_identifier}}")
        );
        assert!(parsed.workspace.require_cwd_in_workspace);
        assert!(parsed.workspace.forbid_untracked_outside_workspace);
    }

    #[test]
    fn workspace_policy_unknown_nested_key_is_rejected() {
        let yaml = r#"
workspace:
  strategies:
    issue:
      kind: git_worktree
      brnch_template: symphony/{{identifier}}
"#;
        let err = serde_yaml::from_str::<WorkflowConfig>(yaml).unwrap_err();
        assert!(
            err.to_string().contains("brnch_template") || err.to_string().contains("unknown field"),
            "expected unknown-field error, got: {err}"
        );
    }

    #[test]
    fn workspace_strategy_kind_enum_round_trips() {
        for kind in [
            WorkspaceStrategyKind::GitWorktree,
            WorkspaceStrategyKind::ExistingWorktree,
            WorkspaceStrategyKind::SharedBranch,
        ] {
            let yaml = serde_yaml::to_string(&kind).unwrap();
            let back: WorkspaceStrategyKind = serde_yaml::from_str(&yaml).unwrap();
            assert_eq!(back, kind);
        }
    }

    #[test]
    fn workspace_cleanup_policy_round_trips() {
        for policy in [
            WorkspaceCleanupPolicy::RetainUntilDone,
            WorkspaceCleanupPolicy::RemoveAfterRun,
            WorkspaceCleanupPolicy::RetainAlways,
        ] {
            let yaml = serde_yaml::to_string(&policy).unwrap();
            let back: WorkspaceCleanupPolicy = serde_yaml::from_str(&yaml).unwrap();
            assert_eq!(back, policy);
        }
    }

    #[test]
    fn branching_parses_spec_5_9_example() {
        let yaml = r#"
branching:
  default_base: main
  child_branch_template: symphony/{{identifier}}
  integration_branch_template: symphony/integration/{{identifier}}
  allow_same_branch_for_children: false
  require_clean_tree_before_run: true
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        assert_eq!(parsed.branching.default_base, "main");
        assert_eq!(
            parsed.branching.child_branch_template,
            "symphony/{{identifier}}"
        );
        assert_eq!(
            parsed.branching.integration_branch_template,
            "symphony/integration/{{identifier}}"
        );
        assert!(!parsed.branching.allow_same_branch_for_children);
        assert!(parsed.branching.require_clean_tree_before_run);
    }

    #[test]
    fn branching_unknown_nested_key_is_rejected() {
        let yaml = r#"
branching:
  defautl_base: main
"#;
        let err = serde_yaml::from_str::<WorkflowConfig>(yaml).unwrap_err();
        assert!(
            err.to_string().contains("defautl_base") || err.to_string().contains("unknown field"),
            "expected unknown-field error, got: {err}"
        );
    }

    #[test]
    fn branching_partial_override_keeps_defaults() {
        let yaml = r#"
branching:
  allow_same_branch_for_children: true
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        assert!(parsed.branching.allow_same_branch_for_children);
        assert_eq!(parsed.branching.default_base, "main");
        assert_eq!(
            parsed.branching.child_branch_template,
            "symphony/{{identifier}}"
        );
        assert!(parsed.branching.require_clean_tree_before_run);
    }

    #[test]
    fn workspace_and_branching_round_trip_through_yaml() {
        let mut cfg = WorkflowConfig::default();
        cfg.tracker.project_slug = Some("ENG".into());
        cfg.workspace.default_strategy = Some("issue_worktree".into());
        cfg.workspace.strategies.insert(
            "issue_worktree".into(),
            WorkspaceStrategyConfig {
                kind: WorkspaceStrategyKind::GitWorktree,
                base: Some("main".into()),
                branch_template: Some("symphony/{{identifier}}".into()),
                cleanup: WorkspaceCleanupPolicy::RetainUntilDone,
                path: None,
                require_branch: None,
            },
        );
        cfg.branching.allow_same_branch_for_children = true;
        let yaml = serde_yaml::to_string(&cfg).expect("serialises");
        let reparsed: WorkflowConfig = serde_yaml::from_str(&yaml).expect("re-parses");
        assert_eq!(cfg, reparsed);
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
    fn polling_lease_defaults_when_omitted() {
        let yaml = "tracker:\n  project_slug: ENG\npolling:\n  interval_ms: 1000\n";
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        assert_eq!(parsed.polling.lease.default_ttl_ms, 60_000);
        assert_eq!(parsed.polling.lease.renewal_margin_ms, 15_000);
    }

    #[test]
    fn polling_lease_roundtrip() {
        let yaml = r#"
tracker:
  project_slug: ENG
polling:
  interval_ms: 1000
  lease:
    default_ttl_ms: 30000
    renewal_margin_ms: 5000
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        assert_eq!(parsed.polling.lease.default_ttl_ms, 30_000);
        assert_eq!(parsed.polling.lease.renewal_margin_ms, 5_000);

        let reserialised = serde_yaml::to_string(&parsed).expect("serialises");
        let reparsed: WorkflowConfig = serde_yaml::from_str(&reserialised).expect("re-parses");
        assert_eq!(parsed, reparsed);
    }

    #[test]
    fn polling_lease_unknown_nested_key_is_rejected() {
        let yaml = r#"
tracker:
  project_slug: ENG
polling:
  lease:
    ttl_ms: 30000
"#;
        let err = serde_yaml::from_str::<WorkflowConfig>(yaml).unwrap_err();
        assert!(
            err.to_string().contains("ttl_ms") || err.to_string().contains("unknown field"),
            "expected unknown-field error, got: {err}"
        );
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
    fn yaml_roundtrip_preserves_observability_overrides() {
        let yaml = r#"
tracker:
  project_slug: ENG
observability:
  logs:
    format: pretty
  event_bus: false
  sse:
    enabled: false
    bind: 0.0.0.0:7000
    replay_buffer: 1024
  tui:
    enabled: false
  dashboard:
    enabled: true
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        assert_eq!(parsed.observability.logs.format, LogFormat::Pretty);
        assert!(!parsed.observability.event_bus);
        assert!(!parsed.observability.sse.enabled);
        assert_eq!(parsed.observability.sse.bind, "0.0.0.0:7000");
        assert_eq!(parsed.observability.sse.replay_buffer, 1024);
        assert!(!parsed.observability.tui.enabled);
        assert!(parsed.observability.dashboard.enabled);

        let reserialised = serde_yaml::to_string(&parsed).expect("serialises");
        let reparsed: WorkflowConfig = serde_yaml::from_str(&reserialised).expect("re-parses");
        assert_eq!(parsed, reparsed);
    }

    #[test]
    fn observability_defaults_when_omitted() {
        // SPEC v2 §5.14 promises that omitting the block keeps the
        // daemon runnable. Defaults must mirror the documented
        // worked example.
        let yaml = "tracker:\n  project_slug: ENG\n";
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        assert_eq!(parsed.observability, ObservabilityConfig::default());
        assert!(parsed.observability.sse.enabled);
        assert_eq!(parsed.observability.sse.bind, "127.0.0.1:6280");
        assert_eq!(parsed.observability.sse.replay_buffer, 256);
        assert!(parsed.observability.event_bus);
        assert!(parsed.observability.tui.enabled);
        assert!(!parsed.observability.dashboard.enabled);
        assert_eq!(parsed.observability.logs.format, LogFormat::Json);
    }

    #[test]
    fn legacy_top_level_status_block_is_rejected() {
        // Migration guard: SPEC v2 §5.14 moves `status:` under
        // `observability.sse`. A top-level `status:` block must fail
        // loudly with an unknown-field error rather than silently
        // dropping operator intent.
        let yaml = r#"
tracker:
  project_slug: ENG
status:
  enabled: false
  bind: 0.0.0.0:7000
"#;
        let err = serde_yaml::from_str::<WorkflowConfig>(yaml).unwrap_err();
        assert!(
            err.to_string().contains("status") || err.to_string().contains("unknown field"),
            "expected unknown-field error for legacy `status:`, got: {err}"
        );
    }

    #[test]
    fn observability_unknown_nested_key_is_rejected() {
        // Mirrors the polling test: typos inside a known section
        // must not silently default — operators expect feedback.
        let yaml = r#"
tracker:
  project_slug: ENG
observability:
  sse:
    enabld: true
"#;
        let err = serde_yaml::from_str::<WorkflowConfig>(yaml).unwrap_err();
        assert!(
            err.to_string().contains("enabld") || err.to_string().contains("unknown field"),
            "expected unknown-field error, got: {err}"
        );
    }

    #[test]
    fn observability_unknown_top_section_is_rejected() {
        let yaml = r#"
tracker:
  project_slug: ENG
observability:
  metrics:
    enabled: true
"#;
        let err = serde_yaml::from_str::<WorkflowConfig>(yaml).unwrap_err();
        assert!(
            err.to_string().contains("metrics") || err.to_string().contains("unknown field"),
            "expected unknown-field error, got: {err}"
        );
    }

    #[test]
    fn validate_rejects_zero_replay_buffer_when_sse_enabled() {
        let mut cfg = WorkflowConfig::default();
        cfg.tracker.project_slug = Some("ENG".into());
        cfg.observability.sse.replay_buffer = 0;
        assert_eq!(
            cfg.validate(),
            Err(ConfigValidationError::ObservabilitySseReplayBufferZero(0))
        );
    }

    #[test]
    fn validate_ignores_zero_replay_buffer_when_sse_disabled() {
        // A disabled SSE surface never constructs the broadcast
        // channel, so a zero buffer is harmless. Surfacing it as an
        // error would force operators to delete the (otherwise
        // illustrative) value just to start the daemon with SSE off.
        let mut cfg = WorkflowConfig::default();
        cfg.tracker.project_slug = Some("ENG".into());
        cfg.observability.sse.enabled = false;
        cfg.observability.sse.replay_buffer = 0;
        assert_eq!(cfg.validate(), Ok(()));
    }

    #[test]
    fn validate_rejects_unparseable_bind() {
        let mut cfg = WorkflowConfig::default();
        cfg.tracker.project_slug = Some("ENG".into());
        cfg.observability.sse.bind = "not-a-socket".into();
        assert_eq!(
            cfg.validate(),
            Err(ConfigValidationError::ObservabilitySseBindInvalid(
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
    command: codex
    args: [app-server]
    extra_args: [--profile, fast]
    model: o4
    tools: [git, github, tracker]
    mcp_tools: [get_issue_details, submit_handoff]
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
  fixture_agent:
    backend: mock
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        assert_eq!(parsed.agents.len(), 3);

        let lead = parsed
            .agents
            .get("lead_agent")
            .and_then(|a| a.as_backend())
            .expect("lead_agent is a backend profile");
        assert_eq!(lead.backend, AgentBackend::Codex);
        assert_eq!(lead.command.as_deref(), Some("codex"));
        assert_eq!(lead.args, vec!["app-server"]);
        assert_eq!(lead.extra_args, vec!["--profile", "fast"]);
        assert_eq!(lead.model.as_deref(), Some("o4"));
        assert_eq!(lead.tools, vec!["git", "github", "tracker"]);
        assert_eq!(
            lead.mcp_tools,
            vec![
                "get_issue_details".to_string(),
                "submit_handoff".to_string()
            ]
        );
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
            parsed.agents.get("fixture_agent").and_then(|a| a.backend()),
            Some(AgentBackend::Mock)
        );
    }

    #[test]
    fn agent_backend_serialises_lowercase() {
        for (variant, literal) in [
            (AgentBackend::Codex, "codex"),
            (AgentBackend::Claude, "claude"),
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
    fn agent_backend_rejects_hermes_with_v5_guidance() {
        let yaml = r#"
tracker:
  project_slug: ENG
agents:
  old:
    backend: hermes
"#;
        let err = serde_yaml::from_str::<WorkflowConfig>(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("hermes"), "{msg}");
        assert!(msg.contains("SPEC_v5.md"), "{msg}");
        assert!(msg.contains("§10"), "{msg}");
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
        assert_eq!(
            cfg.decomposition.dependency_policy,
            DependencyPolicy::default()
        );
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
        assert_eq!(
            parsed.decomposition.dependency_policy,
            DependencyPolicy::default()
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
  dependency_policy:
    materialize_edges: true
    tracker_sync: required
    dispatch_gate: true
    auto_resolve_on_terminal: true
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
        assert!(d.dependency_policy.materialize_edges);
        assert_eq!(
            d.dependency_policy.tracker_sync,
            DependencyTrackerSyncPolicy::Required
        );
        assert!(d.dependency_policy.dispatch_gate);
        assert!(d.dependency_policy.auto_resolve_on_terminal);
    }

    #[test]
    fn decomposition_dependency_policy_defaults_match_v3() {
        let yaml = r#"
tracker:
  project_slug: ENG
decomposition:
  dependency_policy: {}
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        assert_eq!(
            parsed.decomposition.dependency_policy,
            DependencyPolicy {
                materialize_edges: true,
                tracker_sync: DependencyTrackerSyncPolicy::BestEffort,
                dispatch_gate: true,
                auto_resolve_on_terminal: true,
            }
        );
    }

    #[test]
    fn decomposition_dispatch_gate_false_emits_warning_not_error() {
        let yaml = r#"
tracker:
  project_slug: ENG
decomposition:
  dependency_policy:
    dispatch_gate: false
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        assert_eq!(parsed.validate(), Ok(()));
        assert_eq!(
            parsed.validation_warnings(),
            vec![ConfigValidationWarning::DependencyDispatchGateDisabled]
        );
        assert!(
            parsed.validation_warnings()[0]
                .to_string()
                .contains("observability-only")
        );
    }

    #[test]
    fn default_dependency_dispatch_gate_has_no_warning() {
        let parsed: WorkflowConfig =
            serde_yaml::from_str("tracker:\n  project_slug: ENG\n").expect("yaml parses");
        assert!(parsed.validation_warnings().is_empty());
    }

    #[test]
    fn decomposition_dependency_policy_parses_all_sync_modes() {
        for (literal, expected) in [
            ("required", DependencyTrackerSyncPolicy::Required),
            ("best_effort", DependencyTrackerSyncPolicy::BestEffort),
            ("local_only", DependencyTrackerSyncPolicy::LocalOnly),
        ] {
            let yaml = format!(
                r#"
tracker:
  project_slug: ENG
decomposition:
  dependency_policy:
    tracker_sync: {literal}
"#
            );
            let parsed: WorkflowConfig = serde_yaml::from_str(&yaml).expect("yaml parses");
            assert_eq!(
                parsed.decomposition.dependency_policy.tracker_sync,
                expected
            );
        }
    }

    #[test]
    fn dependency_tracker_sync_policy_serialises_snake_case() {
        for (variant, literal) in [
            (DependencyTrackerSyncPolicy::Required, "required"),
            (DependencyTrackerSyncPolicy::BestEffort, "best_effort"),
            (DependencyTrackerSyncPolicy::LocalOnly, "local_only"),
        ] {
            let yaml = serde_yaml::to_string(&variant).expect("serialises");
            assert!(
                yaml.contains(literal),
                "expected {literal} in {yaml:?} for {variant:?}"
            );
            let reparsed: DependencyTrackerSyncPolicy =
                serde_yaml::from_str(&yaml).expect("re-parses");
            assert_eq!(reparsed, variant);
        }
    }

    #[test]
    fn dependency_policy_round_trips_all_fields_independently() {
        let policy = DependencyPolicy {
            materialize_edges: false,
            tracker_sync: DependencyTrackerSyncPolicy::LocalOnly,
            dispatch_gate: false,
            auto_resolve_on_terminal: false,
        };

        let yaml = serde_yaml::to_string(&policy).expect("serialises");
        let reparsed: DependencyPolicy = serde_yaml::from_str(&yaml).expect("re-parses");

        assert_eq!(reparsed, policy);
        assert!(yaml.contains("materialize_edges: false"));
        assert!(yaml.contains("tracker_sync: local_only"));
        assert!(yaml.contains("dispatch_gate: false"));
        assert!(yaml.contains("auto_resolve_on_terminal: false"));
    }

    #[test]
    fn decomposition_dependency_policy_rejects_unknown_sync_mode() {
        let yaml = r#"
tracker:
  project_slug: ENG
decomposition:
  dependency_policy:
    tracker_sync: eventually
"#;
        let err = serde_yaml::from_str::<WorkflowConfig>(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("eventually") || msg.contains("unknown variant"),
            "expected unknown-variant error, got: {msg}"
        );
    }

    #[test]
    fn decomposition_dependency_policy_rejects_unknown_key() {
        let yaml = r#"
tracker:
  project_slug: ENG
decomposition:
  dependency_policy:
    tracker_synk: required
"#;
        let err = serde_yaml::from_str::<WorkflowConfig>(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("tracker_synk") || msg.contains("unknown field"),
            "expected unknown-field error, got: {msg}"
        );
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
  dependency_policy:
    materialize_edges: false
    tracker_sync: local_only
    dispatch_gate: false
    auto_resolve_on_terminal: false
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

    #[test]
    fn qa_block_defaults_when_omitted() {
        // SPEC v2 §5.12 defaults: an omitted `qa:` block must produce
        // safe defaults — disabled, no owner, exhaustive, no static
        // shortcut, can file blockers/followups, blocks parent on
        // blocker, no waivers, all evidence categories required, and
        // auto-rerun after blockers resolve.
        let cfg = WorkflowConfig::default();
        let q = &cfg.qa;
        assert!(q.owner_role.is_none());
        assert!(!q.required);
        assert!(q.exhaustive);
        assert!(!q.allow_static_only);
        assert!(q.can_file_blockers);
        assert!(q.can_file_followups);
        assert_eq!(q.blocker_policy, BlockerPolicy::BlocksParent);
        assert!(q.waiver_roles.is_empty());
        assert!(q.evidence_required.tests);
        assert!(q.evidence_required.changed_files_review);
        assert!(q.evidence_required.acceptance_criteria_trace);
        assert!(
            q.evidence_required
                .visual_or_runtime_evidence_when_applicable
        );
        assert!(q.rerun_after_blockers_resolved);
    }

    #[test]
    fn qa_partial_block_inherits_documented_defaults() {
        // An operator who writes only `required` and `owner_role`
        // should still get exhaustive=true, blocker_policy=blocks_parent,
        // every evidence category required, and the auto-rerun.
        let yaml = r#"
tracker:
  project_slug: ENG
qa:
  owner_role: qa
  required: true
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        let q = &parsed.qa;
        assert_eq!(q.owner_role.as_deref(), Some("qa"));
        assert!(q.required);
        assert!(q.exhaustive);
        assert!(!q.allow_static_only);
        assert!(q.can_file_blockers);
        assert!(q.can_file_followups);
        assert_eq!(q.blocker_policy, BlockerPolicy::BlocksParent);
        assert!(q.waiver_roles.is_empty());
        assert!(q.evidence_required.tests);
        assert!(q.evidence_required.changed_files_review);
        assert!(q.evidence_required.acceptance_criteria_trace);
        assert!(
            q.evidence_required
                .visual_or_runtime_evidence_when_applicable
        );
        assert!(q.rerun_after_blockers_resolved);
    }

    #[test]
    fn qa_full_block_parses_spec_example() {
        // Mirrors the SPEC v2 §5.12 worked example verbatim.
        let yaml = r#"
tracker:
  project_slug: ENG
qa:
  owner_role: qa
  required: true
  exhaustive: true
  allow_static_only: false
  can_file_blockers: true
  can_file_followups: true
  blocker_policy: blocks_parent
  waiver_roles: [platform_lead]
  evidence_required:
    tests: true
    changed_files_review: true
    acceptance_criteria_trace: true
    visual_or_runtime_evidence_when_applicable: true
  rerun_after_blockers_resolved: true
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        let q = &parsed.qa;
        assert_eq!(q.owner_role.as_deref(), Some("qa"));
        assert!(q.required);
        assert!(q.exhaustive);
        assert!(!q.allow_static_only);
        assert!(q.can_file_blockers);
        assert!(q.can_file_followups);
        assert_eq!(q.blocker_policy, BlockerPolicy::BlocksParent);
        assert_eq!(q.waiver_roles, vec!["platform_lead".to_string()]);
        let e = &q.evidence_required;
        assert!(e.tests);
        assert!(e.changed_files_review);
        assert!(e.acceptance_criteria_trace);
        assert!(e.visual_or_runtime_evidence_when_applicable);
        assert!(q.rerun_after_blockers_resolved);
    }

    #[test]
    fn qa_blocker_policy_serialises_snake_case() {
        for (variant, literal) in [
            (BlockerPolicy::BlocksParent, "blocks_parent"),
            (BlockerPolicy::Advisory, "advisory"),
        ] {
            let yaml = serde_yaml::to_string(&variant).expect("serialises");
            assert!(
                yaml.contains(literal),
                "expected {literal} in {yaml:?} for {variant:?}"
            );
            let reparsed: BlockerPolicy = serde_yaml::from_str(&yaml).expect("re-parses");
            assert_eq!(reparsed, variant);
        }
    }

    #[test]
    fn qa_rejects_unknown_blocker_policy() {
        let yaml = r#"
tracker:
  project_slug: ENG
qa:
  blocker_policy: warns_only
"#;
        let err = serde_yaml::from_str::<WorkflowConfig>(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("warns_only")
                || msg.contains("unknown variant")
                || msg.contains("blocks_parent"),
            "expected unknown-variant error, got: {msg}"
        );
    }

    #[test]
    fn qa_rejects_unknown_top_level_key() {
        let yaml = r#"
tracker:
  project_slug: ENG
qa:
  enabled: true
"#;
        let err = serde_yaml::from_str::<WorkflowConfig>(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("enabled") || msg.contains("unknown field"),
            "expected unknown-field error, got: {msg}"
        );
    }

    #[test]
    fn qa_rejects_unknown_evidence_key() {
        // Typos inside `evidence_required:` must surface so a stray
        // `test:` (singular) does not silently produce a no-op flag.
        let yaml = r#"
tracker:
  project_slug: ENG
qa:
  evidence_required:
    test: true
"#;
        let err = serde_yaml::from_str::<WorkflowConfig>(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("test") || msg.contains("unknown field"),
            "expected unknown-field error, got: {msg}"
        );
    }

    #[test]
    fn qa_round_trips_through_yaml() {
        let yaml = r#"
tracker:
  project_slug: ENG
qa:
  owner_role: qa
  required: true
  exhaustive: false
  allow_static_only: true
  can_file_blockers: false
  can_file_followups: false
  blocker_policy: advisory
  waiver_roles: [platform_lead, release_captain]
  evidence_required:
    tests: false
    changed_files_review: true
    acceptance_criteria_trace: false
    visual_or_runtime_evidence_when_applicable: true
  rerun_after_blockers_resolved: false
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        let reserialised = serde_yaml::to_string(&parsed).expect("serialises");
        let reparsed: WorkflowConfig = serde_yaml::from_str(&reserialised).expect("re-parses");
        assert_eq!(parsed, reparsed);
        assert_eq!(parsed.qa.blocker_policy, BlockerPolicy::Advisory);
        assert_eq!(
            parsed.qa.waiver_roles,
            vec!["platform_lead".to_string(), "release_captain".to_string()]
        );
        assert!(!parsed.qa.exhaustive);
        assert!(parsed.qa.allow_static_only);
        assert!(!parsed.qa.evidence_required.tests);
        assert!(parsed.qa.evidence_required.changed_files_review);
        assert!(!parsed.qa.evidence_required.acceptance_criteria_trace);
        assert!(
            parsed
                .qa
                .evidence_required
                .visual_or_runtime_evidence_when_applicable
        );
        assert!(!parsed.qa.rerun_after_blockers_resolved);
    }

    #[test]
    fn followups_block_defaults_when_omitted() {
        // SPEC v2 §5.13 defaults: an omitted `followups:` block must
        // produce safe defaults — disabled, propose-for-approval,
        // recognisable label pair, and both reason/acceptance gates on.
        let cfg = WorkflowConfig::default();
        let f = &cfg.followups;
        assert!(!f.enabled);
        assert_eq!(f.default_policy, ChildIssuePolicy::ProposeForApproval);
        assert!(f.approval_role.is_none());
        assert!(f.initial_tracker_state.is_none());
        assert_eq!(f.non_blocking_label, "follow-up");
        assert_eq!(f.blocking_label, "blocker");
        assert!(f.require_reason);
        assert!(f.require_acceptance_criteria);
    }

    #[test]
    fn followups_partial_block_inherits_documented_defaults() {
        // An operator who writes only `enabled` and `approval_role`
        // should still get propose-for-approval, the SPEC label pair,
        // and both content gates on.
        let yaml = r#"
tracker:
  project_slug: ENG
followups:
  enabled: true
  approval_role: platform_lead
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        let f = &parsed.followups;
        assert!(f.enabled);
        assert_eq!(f.approval_role.as_deref(), Some("platform_lead"));
        assert!(f.initial_tracker_state.is_none());
        assert_eq!(f.default_policy, ChildIssuePolicy::ProposeForApproval);
        assert_eq!(f.non_blocking_label, "follow-up");
        assert_eq!(f.blocking_label, "blocker");
        assert!(f.require_reason);
        assert!(f.require_acceptance_criteria);
    }

    #[test]
    fn followups_full_block_parses_spec_example() {
        // Mirrors the SPEC v2 §5.13 worked example verbatim.
        let yaml = r#"
tracker:
  project_slug: ENG
followups:
  enabled: true
  default_policy: create_directly
  approval_role: platform_lead
  initial_tracker_state: Backlog
  non_blocking_label: follow-up
  blocking_label: blocker
  require_reason: true
  require_acceptance_criteria: true
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        let f = &parsed.followups;
        assert!(f.enabled);
        assert_eq!(f.default_policy, ChildIssuePolicy::CreateDirectly);
        assert_eq!(f.approval_role.as_deref(), Some("platform_lead"));
        assert_eq!(f.initial_tracker_state.as_deref(), Some("Backlog"));
        assert_eq!(f.non_blocking_label, "follow-up");
        assert_eq!(f.blocking_label, "blocker");
        assert!(f.require_reason);
        assert!(f.require_acceptance_criteria);
    }

    #[test]
    fn followups_share_child_issue_policy_enum_with_decomposition() {
        // SPEC v2 §5.13 explicitly shares the policy enum with §5.7.
        // Asserting both fields hold the same variant after a round
        // trip locks the shared-enum contract.
        let yaml = r#"
tracker:
  project_slug: ENG
decomposition:
  child_issue_policy: create_directly
followups:
  default_policy: create_directly
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        assert_eq!(
            parsed.decomposition.child_issue_policy,
            parsed.followups.default_policy,
        );
        assert_eq!(
            parsed.followups.default_policy,
            ChildIssuePolicy::CreateDirectly
        );
    }

    #[test]
    fn followups_rejects_unknown_default_policy() {
        let yaml = r#"
tracker:
  project_slug: ENG
followups:
  default_policy: file_silently
"#;
        let err = serde_yaml::from_str::<WorkflowConfig>(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("file_silently")
                || msg.contains("unknown variant")
                || msg.contains("create_directly"),
            "expected unknown-variant error, got: {msg}"
        );
    }

    #[test]
    fn followups_rejects_unknown_top_level_key() {
        // Typos like `aproval_role:` must surface so a misnamed key
        // does not silently fall back to the default.
        let yaml = r#"
tracker:
  project_slug: ENG
followups:
  aproval_role: platform_lead
"#;
        let err = serde_yaml::from_str::<WorkflowConfig>(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("aproval_role") || msg.contains("unknown field"),
            "expected unknown-field error, got: {msg}"
        );
    }

    #[test]
    fn followups_round_trips_through_yaml() {
        let yaml = r#"
tracker:
  project_slug: ENG
followups:
  enabled: true
  default_policy: create_directly
  approval_role: release_captain
  initial_tracker_state: Icebox
  non_blocking_label: chore
  blocking_label: hotfix
  require_reason: false
  require_acceptance_criteria: false
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        let reserialised = serde_yaml::to_string(&parsed).expect("serialises");
        let reparsed: WorkflowConfig = serde_yaml::from_str(&reserialised).expect("re-parses");
        assert_eq!(parsed, reparsed);
        let f = &parsed.followups;
        assert!(f.enabled);
        assert_eq!(f.default_policy, ChildIssuePolicy::CreateDirectly);
        assert_eq!(f.approval_role.as_deref(), Some("release_captain"));
        assert_eq!(f.initial_tracker_state.as_deref(), Some("Icebox"));
        assert_eq!(f.non_blocking_label, "chore");
        assert_eq!(f.blocking_label, "hotfix");
        assert!(!f.require_reason);
        assert!(!f.require_acceptance_criteria);
    }

    #[test]
    fn validate_rejects_followup_initial_tracker_state_that_is_active() {
        let mut cfg = WorkflowConfig::default();
        cfg.tracker.project_slug = Some("ENG".into());
        cfg.tracker.active_states = vec!["Todo".into(), "In Progress".into()];
        cfg.followups.initial_tracker_state = Some("todo".into());

        assert_eq!(
            cfg.validate(),
            Err(ConfigValidationError::FollowupInitialTrackerStateIsActive {
                state: "todo".into(),
            })
        );
    }

    /// SPEC v2 §5.17: each hook phase is a list of shell snippets, the
    /// timeout default is 5 minutes, and an empty list means "no hook
    /// configured". This pins the parsed shape from YAML.
    #[test]
    fn hooks_v2_list_form_round_trips() {
        let yaml = r#"
tracker:
  project_slug: ENG
hooks:
  timeout_ms: 120000
  after_create:
    - git config user.email "symphony@example.com"
    - git config user.name "Symphony"
  before_run:
    - git status --short
  after_run: []
  before_remove: []
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        let h = &parsed.hooks;
        assert_eq!(h.timeout_ms, 120_000);
        assert_eq!(
            h.after_create,
            vec![
                "git config user.email \"symphony@example.com\"".to_string(),
                "git config user.name \"Symphony\"".to_string(),
            ]
        );
        assert_eq!(h.before_run, vec!["git status --short".to_string()]);
        assert!(h.after_run.is_empty());
        assert!(h.before_remove.is_empty());

        let reserialised = serde_yaml::to_string(&parsed).expect("serialises");
        let reparsed: WorkflowConfig = serde_yaml::from_str(&reserialised).expect("re-parses");
        assert_eq!(parsed, reparsed);
    }

    /// The legacy v1 single-string form must be rejected — there is no
    /// silent migration. Operators wrap their snippet as `[<old>]`.
    #[test]
    fn hooks_v1_single_string_is_rejected() {
        let yaml = r#"
tracker:
  project_slug: ENG
hooks:
  after_create: "git status"
"#;
        let err = serde_yaml::from_str::<WorkflowConfig>(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("after_create") || msg.contains("sequence") || msg.contains("expected"),
            "expected sequence-shape error, got: {msg}"
        );
    }

    // -----------------------------------------------------------------
    // Phase 1 cross-cutting validation (CHECKLIST_v2 phase 1)
    // -----------------------------------------------------------------

    /// Build a minimal valid Linear-tracker config that the cross-cutting
    /// validation tests can mutate. Centralised so a future field
    /// requirement only has to be added in one place.
    fn minimal_linear_cfg() -> WorkflowConfig {
        let mut cfg = WorkflowConfig::default();
        cfg.tracker.project_slug = Some("ENG".into());
        cfg
    }

    fn role(kind: RoleKind) -> RoleConfig {
        RoleConfig {
            kind,
            description: None,
            agent: None,
            max_concurrent: None,
            can_decompose: None,
            can_assign: None,
            can_request_qa: None,
            can_close_parent: None,
            can_file_blockers: None,
            can_file_followups: None,
            required_for_done: None,
            instructions: RoleInstructionConfig::default(),
            assignment: RoleAssignmentMetadata::default(),
        }
    }

    #[test]
    fn role_instruction_and_assignment_metadata_round_trip() {
        let yaml = r#"
tracker:
  project_slug: ENG
roles:
  backend_engineer:
    kind: specialist
    description: Backend implementation
    agent: codex_fast
    instructions:
      role_prompt: .symphony/roles/backend_engineer/AGENTS.md
      soul: .symphony/roles/backend_engineer/SOUL.md
    assignment:
      owns:
        - Rust core and state changes
      does_not_own:
        - TUI polish
      requires:
        - acceptance criteria
      handoff_expectations:
        - tests_run
        - changed_files
      routing_hints:
        paths_any: [crates/**, tests/**]
        labels_any: [backend]
        issue_types_any: [implementation]
        domains_any: [state]
agents:
  codex_fast:
    backend: codex
    command: codex app-server
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        let role = parsed.roles.get("backend_engineer").unwrap();
        assert_eq!(
            role.instructions.role_prompt.as_deref(),
            Some(std::path::Path::new(
                ".symphony/roles/backend_engineer/AGENTS.md"
            ))
        );
        assert_eq!(
            role.instructions.soul.as_deref(),
            Some(std::path::Path::new(
                ".symphony/roles/backend_engineer/SOUL.md"
            ))
        );
        assert_eq!(role.assignment.owns, vec!["Rust core and state changes"]);
        assert_eq!(role.assignment.does_not_own, vec!["TUI polish"]);
        assert_eq!(role.assignment.requires, vec!["acceptance criteria"]);
        assert_eq!(
            role.assignment.handoff_expectations,
            vec!["tests_run", "changed_files"]
        );
        assert_eq!(
            role.assignment.routing_hints.paths_any,
            vec!["crates/**", "tests/**"]
        );
        assert_eq!(role.assignment.routing_hints.labels_any, vec!["backend"]);
        assert_eq!(
            role.assignment.routing_hints.issue_types_any,
            vec!["implementation"]
        );
        assert_eq!(role.assignment.routing_hints.domains_any, vec!["state"]);

        let reserialised = serde_yaml::to_string(&parsed).expect("serialises");
        let reparsed: WorkflowConfig = serde_yaml::from_str(&reserialised).expect("re-parses");
        assert_eq!(parsed, reparsed);
    }

    #[test]
    fn role_instruction_config_rejects_unknown_keys() {
        let yaml = r#"
tracker:
  project_slug: ENG
roles:
  backend_engineer:
    kind: specialist
    instructions:
      role_promtp: .symphony/roles/backend_engineer/AGENTS.md
"#;
        let err = serde_yaml::from_str::<WorkflowConfig>(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("role_promtp") || msg.contains("unknown field"),
            "expected unknown-field error, got: {msg}"
        );
    }

    #[test]
    fn role_assignment_metadata_warnings_surface_thin_catalog_entries() {
        let yaml = r#"
tracker:
  project_slug: ENG
roles:
  platform_lead:
    kind: integration_owner
  backend_engineer:
    kind: specialist
    description: Backend implementation
  frontend_engineer:
    kind: specialist
    description: Frontend implementation
decomposition:
  enabled: true
  owner_role: platform_lead
"#;
        let cfg: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        let warnings = cfg.validation_warnings();
        assert!(warnings.contains(
            &ConfigValidationWarning::SpecialistRoleMissingAssignmentMetadata {
                role: "backend_engineer".into()
            }
        ));
        assert!(warnings.contains(
            &ConfigValidationWarning::SpecialistRoleMissingAssignmentMetadata {
                role: "frontend_engineer".into()
            }
        ));
        assert!(warnings.contains(&ConfigValidationWarning::DecompositionCatalogMostlyFallback));
    }

    #[test]
    fn validate_rejects_decomposition_without_child_owning_roles() {
        let yaml = r#"
tracker:
  project_slug: ENG
roles:
  platform_lead:
    kind: integration_owner
decomposition:
  enabled: true
  owner_role: platform_lead
"#;
        let cfg: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        assert_eq!(
            cfg.validate(),
            Err(ConfigValidationError::MissingEligibleChildOwningRole)
        );
    }

    #[test]
    fn validate_rejects_overlapping_tracker_states() {
        let mut cfg = minimal_linear_cfg();
        cfg.tracker.active_states = vec!["Todo".into(), "In Progress".into()];
        cfg.tracker.terminal_states = vec!["Done".into(), "in progress".into()];
        assert_eq!(
            cfg.validate(),
            Err(ConfigValidationError::DuplicateTrackerState {
                state: "in progress".into(),
            })
        );
    }

    #[test]
    fn validate_rejects_empty_branch_template() {
        let mut cfg = minimal_linear_cfg();
        cfg.branching.child_branch_template = String::new();
        let err = cfg.validate().unwrap_err();
        match err {
            ConfigValidationError::InvalidBranchTemplate { field, reason, .. } => {
                assert_eq!(field, "child_branch_template");
                assert!(reason.contains("empty"), "got reason: {reason}");
            }
            other => panic!("expected InvalidBranchTemplate, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_branch_template_missing_identifier_placeholder() {
        let mut cfg = minimal_linear_cfg();
        cfg.branching.integration_branch_template = "symphony/integration".into();
        let err = cfg.validate().unwrap_err();
        match err {
            ConfigValidationError::InvalidBranchTemplate { field, reason, .. } => {
                assert_eq!(field, "integration_branch_template");
                assert!(reason.contains("{{identifier}}"), "got reason: {reason}");
            }
            other => panic!("expected InvalidBranchTemplate, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_zero_decomposition_max_depth_when_enabled() {
        let mut cfg = minimal_linear_cfg();
        cfg.roles
            .insert("lead".into(), role(RoleKind::IntegrationOwner));
        cfg.decomposition.enabled = true;
        cfg.decomposition.owner_role = Some("lead".into());
        cfg.decomposition.max_depth = 0;
        assert_eq!(
            cfg.validate(),
            Err(ConfigValidationError::DecompositionMaxDepthZero(0))
        );
    }

    #[test]
    fn validate_ignores_zero_decomposition_max_depth_when_disabled() {
        // When decomposition is disabled the depth field is inert; a
        // zero value should not fail validation.
        let mut cfg = minimal_linear_cfg();
        cfg.decomposition.enabled = false;
        cfg.decomposition.max_depth = 0;
        assert_eq!(cfg.validate(), Ok(()));
    }

    #[test]
    fn validate_accepts_required_dependency_tracker_sync_for_linear() {
        let mut cfg = minimal_linear_cfg();
        cfg.decomposition.dependency_policy.tracker_sync = DependencyTrackerSyncPolicy::Required;
        assert_eq!(cfg.validate(), Ok(()));
    }

    #[test]
    fn validate_rejects_required_dependency_tracker_sync_for_github() {
        let mut cfg = WorkflowConfig::default();
        cfg.tracker.kind = TrackerKind::Github;
        cfg.tracker.repository = Some("foglet-io/rust-symphony".into());
        cfg.decomposition.dependency_policy.tracker_sync = DependencyTrackerSyncPolicy::Required;
        assert_eq!(
            cfg.validate(),
            Err(
                ConfigValidationError::DependencyTrackerSyncRequiresAddBlocker {
                    actual: TrackerKind::Github,
                }
            )
        );
    }

    #[test]
    fn validate_rejects_required_dependency_tracker_sync_from_github_yaml() {
        let yaml = r#"
tracker:
  kind: github
  repository: foglet-io/rust-symphony
decomposition:
  dependency_policy:
    tracker_sync: required
"#;
        let cfg: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");

        assert_eq!(
            cfg.validate(),
            Err(
                ConfigValidationError::DependencyTrackerSyncRequiresAddBlocker {
                    actual: TrackerKind::Github,
                }
            )
        );
    }

    #[test]
    fn validate_rejects_required_dependency_tracker_sync_for_mock() {
        let mut cfg = WorkflowConfig::default();
        cfg.tracker.kind = TrackerKind::Mock;
        cfg.tracker.fixtures = Some(PathBuf::from("issues.yaml"));
        cfg.decomposition.dependency_policy.tracker_sync = DependencyTrackerSyncPolicy::Required;
        assert_eq!(
            cfg.validate(),
            Err(
                ConfigValidationError::DependencyTrackerSyncRequiresAddBlocker {
                    actual: TrackerKind::Mock,
                }
            )
        );
    }

    #[test]
    fn validate_accepts_local_only_dependency_tracker_sync_for_github_yaml() {
        let yaml = r#"
tracker:
  kind: github
  repository: foglet-io/rust-symphony
decomposition:
  dependency_policy:
    tracker_sync: local_only
"#;
        let cfg: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");

        assert_eq!(cfg.validate(), Ok(()));
    }

    #[test]
    fn validate_accepts_best_effort_dependency_tracker_sync_for_github_yaml() {
        let yaml = r#"
tracker:
  kind: github
  repository: foglet-io/rust-symphony
decomposition:
  dependency_policy:
    tracker_sync: best_effort
"#;
        let cfg: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");

        assert_eq!(cfg.validate(), Ok(()));
    }

    #[test]
    fn validate_accepts_direct_child_creation_for_linear_parent_links() {
        let mut cfg = minimal_linear_cfg();
        cfg.decomposition.child_issue_policy = ChildIssuePolicy::CreateDirectly;
        assert_eq!(cfg.validate(), Ok(()));
    }

    #[test]
    fn validate_rejects_direct_child_creation_for_github_without_parent_links() {
        let mut cfg = WorkflowConfig::default();
        cfg.tracker.kind = TrackerKind::Github;
        cfg.tracker.repository = Some("foglet-io/rust-symphony".into());
        cfg.decomposition.child_issue_policy = ChildIssuePolicy::CreateDirectly;
        assert_eq!(
            cfg.validate(),
            Err(
                ConfigValidationError::DecompositionDirectChildCreationRequiresParentLink {
                    actual: TrackerKind::Github,
                }
            )
        );
    }

    #[test]
    fn validate_rejects_direct_child_creation_from_github_yaml() {
        let yaml = r#"
tracker:
  kind: github
  repository: foglet-io/rust-symphony
decomposition:
  child_issue_policy: create_directly
"#;
        let cfg: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");

        assert_eq!(
            cfg.validate(),
            Err(
                ConfigValidationError::DecompositionDirectChildCreationRequiresParentLink {
                    actual: TrackerKind::Github,
                }
            )
        );
    }

    #[test]
    fn validate_accepts_github_parent_links_in_advisory_approval_mode() {
        let mut cfg = WorkflowConfig::default();
        cfg.tracker.kind = TrackerKind::Github;
        cfg.tracker.repository = Some("foglet-io/rust-symphony".into());
        cfg.decomposition.child_issue_policy = ChildIssuePolicy::ProposeForApproval;
        assert_eq!(cfg.validate(), Ok(()));
    }

    #[test]
    fn validate_rejects_direct_child_creation_for_mock_without_parent_links() {
        let mut cfg = WorkflowConfig::default();
        cfg.tracker.kind = TrackerKind::Mock;
        cfg.tracker.fixtures = Some(PathBuf::from("issues.yaml"));
        cfg.decomposition.child_issue_policy = ChildIssuePolicy::CreateDirectly;
        assert_eq!(
            cfg.validate(),
            Err(
                ConfigValidationError::DecompositionDirectChildCreationRequiresParentLink {
                    actual: TrackerKind::Mock,
                }
            )
        );
    }

    #[test]
    fn validate_rejects_pull_requests_without_github_tracker() {
        let mut cfg = minimal_linear_cfg();
        cfg.roles
            .insert("lead".into(), role(RoleKind::IntegrationOwner));
        cfg.pull_requests.enabled = true;
        cfg.pull_requests.owner_role = Some("lead".into());
        assert_eq!(
            cfg.validate(),
            Err(ConfigValidationError::PullRequestsRequireGithubTracker {
                actual: TrackerKind::Linear,
            })
        );
    }

    #[test]
    fn validate_accepts_pull_requests_with_github_tracker() {
        let mut cfg = WorkflowConfig::default();
        cfg.tracker.kind = TrackerKind::Github;
        cfg.tracker.repository = Some("foglet-io/rust-symphony".into());
        cfg.roles
            .insert("lead".into(), role(RoleKind::IntegrationOwner));
        cfg.pull_requests.enabled = true;
        cfg.pull_requests.owner_role = Some("lead".into());
        assert_eq!(cfg.validate(), Ok(()));
    }

    #[test]
    fn validate_rejects_shared_branch_strategy_when_branching_disallows_it() {
        let mut cfg = minimal_linear_cfg();
        cfg.workspace.strategies.insert(
            "shared".into(),
            WorkspaceStrategyConfig {
                kind: WorkspaceStrategyKind::SharedBranch,
                base: None,
                branch_template: None,
                cleanup: WorkspaceCleanupPolicy::default(),
                path: None,
                require_branch: Some("symphony/integration/main".into()),
            },
        );
        let err = cfg.validate().unwrap_err();
        let detail = match err {
            ConfigValidationError::ContradictorySharedBranchPolicy { detail } => detail,
            other => panic!("expected ContradictorySharedBranchPolicy, got {other:?}"),
        };
        assert!(detail.contains("shared"), "got detail: {detail}");
    }

    #[test]
    fn validate_rejects_shared_branch_merge_strategy_when_branching_disallows_it() {
        let mut cfg = minimal_linear_cfg();
        cfg.integration.merge_strategy = MergeStrategy::SharedBranch;
        let err = cfg.validate().unwrap_err();
        match err {
            ConfigValidationError::ContradictorySharedBranchPolicy { detail } => {
                assert!(
                    detail.contains("integration.merge_strategy"),
                    "got detail: {detail}"
                );
            }
            other => panic!("expected ContradictorySharedBranchPolicy, got {other:?}"),
        }
    }

    #[test]
    fn validate_accepts_shared_branch_strategy_when_branching_allows_it() {
        let mut cfg = minimal_linear_cfg();
        cfg.branching.allow_same_branch_for_children = true;
        cfg.workspace.strategies.insert(
            "shared".into(),
            WorkspaceStrategyConfig {
                kind: WorkspaceStrategyKind::SharedBranch,
                base: None,
                branch_template: None,
                cleanup: WorkspaceCleanupPolicy::default(),
                path: None,
                require_branch: Some("symphony/integration/main".into()),
            },
        );
        cfg.integration.merge_strategy = MergeStrategy::SharedBranch;
        assert_eq!(cfg.validate(), Ok(()));
    }

    #[test]
    fn validate_rejects_unknown_workspace_default_strategy() {
        let mut cfg = minimal_linear_cfg();
        cfg.workspace.default_strategy = Some("issue_worktree".into());
        assert_eq!(
            cfg.validate(),
            Err(ConfigValidationError::UnknownDefaultWorkspaceStrategy(
                "issue_worktree".into()
            ))
        );
    }

    #[test]
    fn validate_rejects_unknown_routing_default_role() {
        let mut cfg = minimal_linear_cfg();
        cfg.routing.default_role = Some("ghost".into());
        assert_eq!(
            cfg.validate(),
            Err(ConfigValidationError::UnknownRoleReference {
                field: "routing.default_role".into(),
                role: "ghost".into(),
            })
        );
    }

    #[test]
    fn validate_rejects_unknown_routing_rule_assign_role() {
        let mut cfg = minimal_linear_cfg();
        cfg.routing.rules.push(RoutingRule {
            priority: None,
            when: RoutingMatch::default(),
            assign_role: "ghost".into(),
        });
        assert_eq!(
            cfg.validate(),
            Err(ConfigValidationError::UnknownRoleReference {
                field: "routing.rules[0].assign_role".into(),
                role: "ghost".into(),
            })
        );
    }

    #[test]
    fn validate_rejects_decomposition_owner_role_typo() {
        let mut cfg = minimal_linear_cfg();
        cfg.roles
            .insert("lead".into(), role(RoleKind::IntegrationOwner));
        cfg.decomposition.enabled = true;
        cfg.decomposition.owner_role = Some("led".into());
        assert_eq!(
            cfg.validate(),
            Err(ConfigValidationError::UnknownRoleReference {
                field: "decomposition.owner_role".into(),
                role: "led".into(),
            })
        );
    }

    #[test]
    fn validate_rejects_decomposition_owner_role_with_wrong_kind() {
        let mut cfg = minimal_linear_cfg();
        cfg.roles
            .insert("worker".into(), role(RoleKind::Specialist));
        cfg.roles
            .insert("lead".into(), role(RoleKind::IntegrationOwner));
        cfg.decomposition.enabled = true;
        cfg.decomposition.owner_role = Some("worker".into());
        assert_eq!(
            cfg.validate(),
            Err(ConfigValidationError::RoleKindMismatch {
                field: "decomposition.owner_role".into(),
                role: "worker".into(),
                actual: RoleKind::Specialist,
                expected: RoleKind::IntegrationOwner,
            })
        );
    }

    #[test]
    fn validate_rejects_qa_owner_role_with_wrong_kind() {
        let mut cfg = minimal_linear_cfg();
        cfg.roles
            .insert("worker".into(), role(RoleKind::Specialist));
        cfg.qa.required = true;
        cfg.qa.owner_role = Some("worker".into());
        assert_eq!(
            cfg.validate(),
            Err(ConfigValidationError::RoleKindMismatch {
                field: "qa.owner_role".into(),
                role: "worker".into(),
                actual: RoleKind::Specialist,
                expected: RoleKind::QaGate,
            })
        );
    }

    #[test]
    fn validate_rejects_unknown_qa_waiver_role() {
        let mut cfg = minimal_linear_cfg();
        cfg.roles.insert("qa".into(), role(RoleKind::QaGate));
        cfg.qa.required = true;
        cfg.qa.owner_role = Some("qa".into());
        cfg.qa.waiver_roles = vec!["ghost".into()];
        assert_eq!(
            cfg.validate(),
            Err(ConfigValidationError::UnknownRoleReference {
                field: "qa.waiver_roles[0]".into(),
                role: "ghost".into(),
            })
        );
    }

    #[test]
    fn validate_rejects_missing_integration_owner_when_decomposition_enabled() {
        // No role of kind: integration_owner; decomposition is enabled
        // with no owner_role set, so the owner_role typo check passes
        // and the cross-cutting "must have at least one" error fires.
        let mut cfg = minimal_linear_cfg();
        cfg.roles
            .insert("worker".into(), role(RoleKind::Specialist));
        cfg.decomposition.enabled = false; // owner-role check off
        cfg.integration.required_for = vec![IntegrationRequirement::DecomposedParent];
        // need_int_owner is true via `integration.required_for`.
        assert_eq!(
            cfg.validate(),
            Err(ConfigValidationError::MissingIntegrationOwnerRole {
                feature: "integration".into(),
            })
        );
    }

    #[test]
    fn validate_rejects_missing_qa_gate_when_qa_required() {
        let mut cfg = minimal_linear_cfg();
        cfg.roles
            .insert("worker".into(), role(RoleKind::Specialist));
        cfg.qa.required = true;
        // owner_role unset — required-kind sweep fires.
        assert_eq!(
            cfg.validate(),
            Err(ConfigValidationError::MissingQaGateRole)
        );
    }

    #[test]
    fn validate_rejects_unknown_role_agent_reference() {
        let mut cfg = minimal_linear_cfg();
        let mut r = role(RoleKind::Specialist);
        r.agent = Some("ghost".into());
        cfg.roles.insert("worker".into(), r);
        assert_eq!(
            cfg.validate(),
            Err(ConfigValidationError::UnknownAgentProfileReference {
                role: "worker".into(),
                agent: "ghost".into(),
            })
        );
    }

    #[test]
    fn validate_rejects_unknown_composite_agent_reference() {
        let mut cfg = minimal_linear_cfg();
        cfg.agents.insert(
            "tandem".into(),
            AgentProfileConfig::Composite(AgentCompositeProfile {
                strategy: AgentStrategy::Tandem,
                lead: "lead".into(),
                follower: "ghost".into(),
                mode: TandemMode::DraftReview,
                description: None,
            }),
        );
        cfg.agents.insert(
            "lead".into(),
            AgentProfileConfig::Backend(Box::new(AgentBackendProfile {
                backend: AgentBackend::Mock,
                description: None,
                command: None,
                args: Vec::new(),
                extra_args: Vec::new(),
                model: None,
                system_prompt: None,
                tools: Vec::new(),
                mcp_tools: Vec::new(),
                env: BTreeMap::new(),
                memory: None,
                approval_policy: None,
                sandbox_policy: None,
                max_turns: None,
                turn_timeout_ms: None,
                stall_timeout_ms: None,
                token_budget: None,
                cost_budget_usd: None,
            })),
        );
        let err = cfg.validate().unwrap_err();
        match err {
            ConfigValidationError::UnknownCompositeAgentReference {
                profile,
                seat,
                target,
            } => {
                assert_eq!(profile, "tandem");
                assert_eq!(seat, "follower");
                assert_eq!(target, "ghost");
            }
            other => panic!("expected UnknownCompositeAgentReference, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_nested_composite_agent_profile() {
        let backend = AgentProfileConfig::Backend(Box::new(AgentBackendProfile {
            backend: AgentBackend::Mock,
            description: None,
            command: None,
            args: Vec::new(),
            extra_args: Vec::new(),
            model: None,
            system_prompt: None,
            tools: Vec::new(),
            mcp_tools: Vec::new(),
            env: BTreeMap::new(),
            memory: None,
            approval_policy: None,
            sandbox_policy: None,
            max_turns: None,
            turn_timeout_ms: None,
            stall_timeout_ms: None,
            token_budget: None,
            cost_budget_usd: None,
        }));
        let inner = AgentProfileConfig::Composite(AgentCompositeProfile {
            strategy: AgentStrategy::Tandem,
            lead: "real".into(),
            follower: "real".into(),
            mode: TandemMode::DraftReview,
            description: None,
        });
        let outer = AgentProfileConfig::Composite(AgentCompositeProfile {
            strategy: AgentStrategy::Tandem,
            lead: "inner".into(),
            follower: "real".into(),
            mode: TandemMode::DraftReview,
            description: None,
        });
        let mut cfg = minimal_linear_cfg();
        cfg.agents.insert("real".into(), backend);
        cfg.agents.insert("inner".into(), inner);
        cfg.agents.insert("outer".into(), outer);
        let err = cfg.validate().unwrap_err();
        match err {
            ConfigValidationError::NestedCompositeAgentProfile {
                profile,
                seat,
                target,
            } => {
                assert_eq!(profile, "outer");
                assert_eq!(seat, "lead");
                assert_eq!(target, "inner");
            }
            other => panic!("expected NestedCompositeAgentProfile, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_unsupported_model_override_for_backend() {
        let mut cfg = minimal_linear_cfg();
        cfg.agents.insert(
            "mock_model".into(),
            AgentProfileConfig::Backend(Box::new(AgentBackendProfile {
                backend: AgentBackend::Mock,
                description: None,
                command: None,
                args: Vec::new(),
                extra_args: Vec::new(),
                model: Some("pretend-model".into()),
                system_prompt: None,
                tools: Vec::new(),
                mcp_tools: Vec::new(),
                env: BTreeMap::new(),
                memory: None,
                approval_policy: None,
                sandbox_policy: None,
                max_turns: None,
                turn_timeout_ms: None,
                stall_timeout_ms: None,
                token_budget: None,
                cost_budget_usd: None,
            })),
        );
        assert_eq!(
            cfg.validate(),
            Err(ConfigValidationError::UnsupportedAgentProfileModel {
                profile: "mock_model".into(),
                backend: AgentBackend::Mock,
            })
        );
    }

    #[test]
    fn validate_rejects_structured_args_on_legacy_whitespace_command() {
        let mut cfg = minimal_linear_cfg();
        cfg.agents.insert(
            "bad_codex".into(),
            AgentProfileConfig::Backend(Box::new(AgentBackendProfile {
                backend: AgentBackend::Codex,
                description: None,
                command: Some("codex app-server".into()),
                args: vec!["--profile".into(), "fast".into()],
                extra_args: Vec::new(),
                model: None,
                system_prompt: None,
                tools: Vec::new(),
                mcp_tools: Vec::new(),
                env: BTreeMap::new(),
                memory: None,
                approval_policy: None,
                sandbox_policy: None,
                max_turns: None,
                turn_timeout_ms: None,
                stall_timeout_ms: None,
                token_budget: None,
                cost_budget_usd: None,
            })),
        );
        assert_eq!(
            cfg.validate(),
            Err(ConfigValidationError::UnsupportedAgentArgsOrdering {
                profile: "bad_codex".into(),
                command: "codex app-server".into(),
            })
        );
    }

    #[test]
    fn validate_accepts_full_v2_workflow_with_all_features() {
        // Smoke test: a workflow that turns on every feature at once
        // and wires every cross-reference correctly should validate
        // cleanly. Catches regressions where a new check is too eager.
        let mut cfg = WorkflowConfig::default();
        cfg.tracker.kind = TrackerKind::Github;
        cfg.tracker.repository = Some("foglet-io/rust-symphony".into());
        cfg.roles.insert("lead".into(), {
            let mut r = role(RoleKind::IntegrationOwner);
            r.agent = Some("codex_default".into());
            r
        });
        cfg.roles.insert("qa".into(), {
            let mut r = role(RoleKind::QaGate);
            r.agent = Some("codex_default".into());
            r
        });
        cfg.roles.insert("worker".into(), {
            let mut r = role(RoleKind::Specialist);
            r.agent = Some("codex_default".into());
            r
        });
        cfg.agents.insert(
            "codex_default".into(),
            AgentProfileConfig::Backend(Box::new(AgentBackendProfile {
                backend: AgentBackend::Mock,
                description: None,
                command: None,
                args: Vec::new(),
                extra_args: Vec::new(),
                model: None,
                system_prompt: None,
                tools: Vec::new(),
                mcp_tools: Vec::new(),
                env: BTreeMap::new(),
                memory: None,
                approval_policy: None,
                sandbox_policy: None,
                max_turns: None,
                turn_timeout_ms: None,
                stall_timeout_ms: None,
                token_budget: None,
                cost_budget_usd: None,
            })),
        );
        cfg.routing.default_role = Some("worker".into());
        cfg.routing.rules.push(RoutingRule {
            priority: None,
            when: RoutingMatch::default(),
            assign_role: "lead".into(),
        });
        cfg.decomposition.enabled = true;
        cfg.decomposition.owner_role = Some("lead".into());
        cfg.integration.owner_role = Some("lead".into());
        cfg.integration.required_for = vec![IntegrationRequirement::DecomposedParent];
        cfg.pull_requests.enabled = true;
        cfg.pull_requests.owner_role = Some("lead".into());
        cfg.qa.required = true;
        cfg.qa.owner_role = Some("qa".into());
        cfg.qa.waiver_roles = vec!["lead".into()];
        cfg.followups.enabled = true;
        cfg.followups.approval_role = Some("lead".into());
        assert_eq!(cfg.validate(), Ok(()));
    }

    // -----------------------------------------------------------------
    // Phase 1 negative tests: unknown nested keys, dangling role
    // references, and invalid strategy combinations (CHECKLIST_v2
    // phase 1 final item). These cover sections whose deny_unknown_fields
    // and validate() branches did not yet have explicit negative coverage.
    // -----------------------------------------------------------------

    fn assert_unknown_field(yaml: &str, needle: &str) {
        let err = serde_yaml::from_str::<WorkflowConfig>(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains(needle) || msg.contains("unknown field"),
            "expected unknown-field error mentioning `{needle}`, got: {msg}"
        );
    }

    #[test]
    fn tracker_unknown_nested_key_is_rejected() {
        assert_unknown_field(
            r#"
tracker:
  kind: linear
  proj_slug: ENG
"#,
            "proj_slug",
        );
    }

    #[test]
    fn hooks_unknown_nested_key_is_rejected() {
        assert_unknown_field(
            r#"
hooks:
  after_created:
    - "echo hi"
"#,
            "after_created",
        );
    }

    #[test]
    fn routing_rule_unknown_nested_key_is_rejected() {
        assert_unknown_field(
            r#"
routing:
  rules:
    - assign_role: worker
      whne:
        labels_any: [bug]
"#,
            "whne",
        );
    }

    #[test]
    fn workspace_strategy_entry_unknown_nested_key_is_rejected() {
        assert_unknown_field(
            r#"
workspace:
  strategies:
    issue_worktree:
      kind: git_worktree
      branch_tmpl: "symphony/{{identifier}}"
"#,
            "branch_tmpl",
        );
    }

    #[test]
    fn agent_top_level_unknown_nested_key_is_rejected() {
        assert_unknown_field(
            r#"
agent:
  kindd: codex
"#,
            "kindd",
        );
    }

    #[test]
    fn validate_rejects_unknown_integration_owner_role() {
        let mut cfg = minimal_linear_cfg();
        cfg.roles
            .insert("lead".into(), role(RoleKind::IntegrationOwner));
        cfg.integration.required_for = vec![IntegrationRequirement::DecomposedParent];
        cfg.integration.owner_role = Some("ghost".into());
        assert_eq!(
            cfg.validate(),
            Err(ConfigValidationError::UnknownRoleReference {
                field: "integration.owner_role".into(),
                role: "ghost".into(),
            })
        );
    }

    #[test]
    fn validate_rejects_unknown_pull_requests_owner_role() {
        let mut cfg = WorkflowConfig::default();
        cfg.tracker.kind = TrackerKind::Github;
        cfg.tracker.repository = Some("foglet-io/rust-symphony".into());
        cfg.roles
            .insert("lead".into(), role(RoleKind::IntegrationOwner));
        cfg.pull_requests.enabled = true;
        cfg.pull_requests.owner_role = Some("ghost".into());
        assert_eq!(
            cfg.validate(),
            Err(ConfigValidationError::UnknownRoleReference {
                field: "pull_requests.owner_role".into(),
                role: "ghost".into(),
            })
        );
    }

    #[test]
    fn validate_rejects_pull_requests_owner_role_with_wrong_kind() {
        let mut cfg = WorkflowConfig::default();
        cfg.tracker.kind = TrackerKind::Github;
        cfg.tracker.repository = Some("foglet-io/rust-symphony".into());
        cfg.roles
            .insert("lead".into(), role(RoleKind::IntegrationOwner));
        cfg.roles
            .insert("worker".into(), role(RoleKind::Specialist));
        cfg.pull_requests.enabled = true;
        cfg.pull_requests.owner_role = Some("worker".into());
        assert_eq!(
            cfg.validate(),
            Err(ConfigValidationError::RoleKindMismatch {
                field: "pull_requests.owner_role".into(),
                role: "worker".into(),
                actual: RoleKind::Specialist,
                expected: RoleKind::IntegrationOwner,
            })
        );
    }

    #[test]
    fn validate_rejects_unknown_qa_owner_role() {
        let mut cfg = minimal_linear_cfg();
        cfg.roles.insert("qa".into(), role(RoleKind::QaGate));
        cfg.qa.required = true;
        cfg.qa.owner_role = Some("ghost".into());
        assert_eq!(
            cfg.validate(),
            Err(ConfigValidationError::UnknownRoleReference {
                field: "qa.owner_role".into(),
                role: "ghost".into(),
            })
        );
    }

    #[test]
    fn validate_rejects_unknown_followups_approval_role() {
        let mut cfg = minimal_linear_cfg();
        cfg.followups.enabled = true;
        cfg.followups.approval_role = Some("ghost".into());
        assert_eq!(
            cfg.validate(),
            Err(ConfigValidationError::UnknownRoleReference {
                field: "followups.approval_role".into(),
                role: "ghost".into(),
            })
        );
    }

    #[test]
    fn validate_rejects_missing_integration_owner_when_pull_requests_enabled() {
        // Decomposition off, integration.required_for empty, but PR
        // lifecycle is on — the cross-cutting sweep must still demand an
        // integration_owner role and report `pull_requests` as the
        // triggering feature.
        let mut cfg = WorkflowConfig::default();
        cfg.tracker.kind = TrackerKind::Github;
        cfg.tracker.repository = Some("foglet-io/rust-symphony".into());
        cfg.roles
            .insert("worker".into(), role(RoleKind::Specialist));
        cfg.pull_requests.enabled = true;
        // owner_role unset so the role-ref check passes and the sweep
        // surfaces the missing-kind error.
        assert_eq!(
            cfg.validate(),
            Err(ConfigValidationError::MissingIntegrationOwnerRole {
                feature: "pull_requests".into(),
            })
        );
    }

    fn mock_backend_profile() -> AgentProfileConfig {
        AgentProfileConfig::Backend(Box::new(AgentBackendProfile {
            backend: AgentBackend::Mock,
            description: None,
            command: None,
            args: Vec::new(),
            extra_args: Vec::new(),
            model: None,
            system_prompt: None,
            tools: Vec::new(),
            mcp_tools: Vec::new(),
            env: BTreeMap::new(),
            memory: None,
            approval_policy: None,
            sandbox_policy: None,
            max_turns: None,
            turn_timeout_ms: None,
            stall_timeout_ms: None,
            token_budget: None,
            cost_budget_usd: None,
        }))
    }

    #[test]
    fn concurrency_defaults_are_empty() {
        let cfg = WorkflowConfig::default();
        assert_eq!(cfg.concurrency.global_max, None);
        assert!(cfg.concurrency.per_agent_profile.is_empty());
        assert!(cfg.concurrency.per_repository.is_empty());
        // Resolver falls back to the legacy alias when `global_max` is unset.
        assert_eq!(
            cfg.resolved_global_concurrency(),
            cfg.agent.max_concurrent_agents
        );
    }

    #[test]
    fn concurrency_global_max_overrides_legacy_alias() {
        // The deprecation contract: when both `concurrency.global_max`
        // and `agent.max_concurrent_agents` are set, the typed field
        // wins. This is the precedence rule the CHECKLIST_v2 subtask
        // calls out.
        let mut cfg = WorkflowConfig::default();
        cfg.tracker.project_slug = Some("ENG".into());
        cfg.agent.max_concurrent_agents = 4;
        cfg.concurrency.global_max = Some(7);
        assert_eq!(cfg.resolved_global_concurrency(), 7);
        assert_eq!(cfg.validate(), Ok(()));
    }

    #[test]
    fn concurrency_legacy_alias_used_when_typed_field_unset() {
        let mut cfg = WorkflowConfig::default();
        cfg.tracker.project_slug = Some("ENG".into());
        cfg.agent.max_concurrent_agents = 3;
        assert_eq!(cfg.resolved_global_concurrency(), 3);
    }

    #[test]
    fn concurrency_block_roundtrips() {
        let yaml = r#"
schema_version: 1
tracker:
  kind: github
  repository: foglet-io/rust-symphony
agents:
  fast:
    backend: mock
concurrency:
  global_max: 6
  per_agent_profile:
    fast: 2
  per_repository:
    foglet-io/rust-symphony: 4
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        assert_eq!(parsed.concurrency.global_max, Some(6));
        assert_eq!(
            parsed.concurrency.per_agent_profile.get("fast").copied(),
            Some(2),
        );
        assert_eq!(
            parsed
                .concurrency
                .per_repository
                .get("foglet-io/rust-symphony")
                .copied(),
            Some(4),
        );
        assert_eq!(parsed.validate(), Ok(()));
        let reserialised = serde_yaml::to_string(&parsed).expect("serialises");
        let reparsed: WorkflowConfig = serde_yaml::from_str(&reserialised).expect("re-parses");
        assert_eq!(parsed, reparsed);
    }

    #[test]
    fn concurrency_unknown_nested_key_is_rejected() {
        // `deny_unknown_fields` coverage: typos under `concurrency:`
        // must surface, not silently default.
        let yaml = r#"
tracker:
  project_slug: ENG
concurrency:
  globalmax: 4
"#;
        let err = serde_yaml::from_str::<WorkflowConfig>(yaml).unwrap_err();
        assert!(
            err.to_string().contains("globalmax") || err.to_string().contains("unknown field"),
            "expected unknown-field error, got: {err}"
        );
    }

    #[test]
    fn concurrency_global_max_zero_is_rejected_by_validate() {
        let mut cfg = WorkflowConfig::default();
        cfg.tracker.project_slug = Some("ENG".into());
        cfg.concurrency.global_max = Some(0);
        assert_eq!(
            cfg.validate(),
            Err(ConfigValidationError::ConcurrencyGlobalMaxZero(0)),
        );
    }

    #[test]
    fn concurrency_zero_per_agent_profile_cap_is_rejected() {
        let mut cfg = WorkflowConfig::default();
        cfg.tracker.project_slug = Some("ENG".into());
        cfg.agents.insert("fast".into(), mock_backend_profile());
        cfg.concurrency.per_agent_profile.insert("fast".into(), 0);
        assert_eq!(
            cfg.validate(),
            Err(ConfigValidationError::ConcurrencyZeroCap {
                field: "concurrency.per_agent_profile".into(),
                key: "fast".into(),
            }),
        );
    }

    #[test]
    fn concurrency_unknown_per_agent_profile_key_is_rejected() {
        let mut cfg = WorkflowConfig::default();
        cfg.tracker.project_slug = Some("ENG".into());
        cfg.concurrency.per_agent_profile.insert("ghost".into(), 2);
        assert_eq!(
            cfg.validate(),
            Err(ConfigValidationError::UnknownConcurrencyAgentProfile {
                profile: "ghost".into(),
            }),
        );
    }

    #[test]
    fn concurrency_zero_per_repository_cap_is_rejected() {
        let mut cfg = WorkflowConfig::default();
        cfg.tracker.kind = TrackerKind::Github;
        cfg.tracker.repository = Some("foglet-io/rust-symphony".into());
        cfg.concurrency
            .per_repository
            .insert("foglet-io/rust-symphony".into(), 0);
        assert_eq!(
            cfg.validate(),
            Err(ConfigValidationError::ConcurrencyZeroCap {
                field: "concurrency.per_repository".into(),
                key: "foglet-io/rust-symphony".into(),
            }),
        );
    }

    #[test]
    fn concurrency_unknown_per_repository_key_is_rejected() {
        let mut cfg = WorkflowConfig::default();
        cfg.tracker.kind = TrackerKind::Github;
        cfg.tracker.repository = Some("foglet-io/rust-symphony".into());
        cfg.concurrency
            .per_repository
            .insert("foglet-io/something-else".into(), 2);
        assert_eq!(
            cfg.validate(),
            Err(ConfigValidationError::UnknownConcurrencyRepository {
                repository: "foglet-io/something-else".into(),
            }),
        );
    }

    // -----------------------------------------------------------------
    // budgets
    // -----------------------------------------------------------------

    #[test]
    fn budgets_defaults_match_spec_5_15() {
        // The SPEC v2 §5.15 example block is the documented contract.
        // If any of these defaults drift, operators reading the SPEC
        // will get a different runtime than the doc promises — pin
        // them down here.
        let cfg = BudgetsConfig::default();
        assert_eq!(cfg.max_parallel_runs, 6);
        assert_eq!(cfg.max_cost_per_issue_usd, 10.00);
        assert_eq!(cfg.max_turns_per_run, 20);
        assert_eq!(cfg.max_retries, 3);
        assert_eq!(cfg.pause_policy, BudgetPausePolicy::BlockWorkItem);
    }

    #[test]
    fn budgets_omitted_block_is_default() {
        let yaml = r#"
tracker:
  project_slug: ENG
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        assert_eq!(parsed.budgets, BudgetsConfig::default());
        assert_eq!(parsed.validate(), Ok(()));
    }

    #[test]
    fn budgets_block_roundtrips_with_overrides() {
        // Per-field override + pause_policy round-trip in one fixture.
        let yaml = r#"
tracker:
  project_slug: ENG
budgets:
  max_parallel_runs: 12
  max_cost_per_issue_usd: 25.5
  max_turns_per_run: 40
  max_retries: 5
  pause_policy: block_run
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        assert_eq!(parsed.budgets.max_parallel_runs, 12);
        assert_eq!(parsed.budgets.max_cost_per_issue_usd, 25.5);
        assert_eq!(parsed.budgets.max_turns_per_run, 40);
        assert_eq!(parsed.budgets.max_retries, 5);
        assert_eq!(parsed.budgets.pause_policy, BudgetPausePolicy::BlockRun);
        assert_eq!(parsed.validate(), Ok(()));
        let reserialised = serde_yaml::to_string(&parsed).expect("serialises");
        let reparsed: WorkflowConfig = serde_yaml::from_str(&reserialised).expect("re-parses");
        assert_eq!(parsed, reparsed);
    }

    #[test]
    fn budgets_pause_policy_block_work_item_roundtrips() {
        // The default variant on its own — explicitly set so we
        // exercise the `block_work_item` arm of the snake_case rename.
        let yaml = r#"
tracker:
  project_slug: ENG
budgets:
  pause_policy: block_work_item
"#;
        let parsed: WorkflowConfig = serde_yaml::from_str(yaml).expect("yaml parses");
        assert_eq!(
            parsed.budgets.pause_policy,
            BudgetPausePolicy::BlockWorkItem
        );
        let reserialised = serde_yaml::to_string(&parsed.budgets).expect("serialises");
        assert!(
            reserialised.contains("block_work_item"),
            "expected snake_case wire form, got: {reserialised}"
        );
    }

    #[test]
    fn budgets_unknown_nested_key_is_rejected() {
        // `deny_unknown_fields` coverage: typos under `budgets:` must
        // surface, not silently default.
        let yaml = r#"
tracker:
  project_slug: ENG
budgets:
  maxretries: 5
"#;
        let err = serde_yaml::from_str::<WorkflowConfig>(yaml).unwrap_err();
        assert!(
            err.to_string().contains("maxretries") || err.to_string().contains("unknown field"),
            "expected unknown-field error, got: {err}"
        );
    }

    #[test]
    fn budgets_unknown_pause_policy_is_rejected() {
        let yaml = r#"
tracker:
  project_slug: ENG
budgets:
  pause_policy: pause_everything
"#;
        let err = serde_yaml::from_str::<WorkflowConfig>(yaml).unwrap_err();
        assert!(
            err.to_string().contains("pause_everything")
                || err.to_string().to_lowercase().contains("unknown variant"),
            "expected unknown-variant error, got: {err}"
        );
    }

    #[test]
    fn budgets_zero_max_parallel_runs_is_rejected() {
        let mut cfg = WorkflowConfig::default();
        cfg.tracker.project_slug = Some("ENG".into());
        cfg.budgets.max_parallel_runs = 0;
        assert_eq!(
            cfg.validate(),
            Err(ConfigValidationError::BudgetZero {
                field: "budgets.max_parallel_runs".into(),
            }),
        );
    }

    #[test]
    fn budgets_zero_max_cost_per_issue_usd_is_rejected() {
        let mut cfg = WorkflowConfig::default();
        cfg.tracker.project_slug = Some("ENG".into());
        cfg.budgets.max_cost_per_issue_usd = 0.0;
        assert_eq!(
            cfg.validate(),
            Err(ConfigValidationError::BudgetZero {
                field: "budgets.max_cost_per_issue_usd".into(),
            }),
        );
    }

    #[test]
    fn budgets_negative_max_cost_per_issue_usd_is_rejected() {
        // f64 lets the deserialiser admit negatives; validate() must
        // catch them since a negative ceiling cannot be enforced.
        let mut cfg = WorkflowConfig::default();
        cfg.tracker.project_slug = Some("ENG".into());
        cfg.budgets.max_cost_per_issue_usd = -1.5;
        assert_eq!(
            cfg.validate(),
            Err(ConfigValidationError::BudgetZero {
                field: "budgets.max_cost_per_issue_usd".into(),
            }),
        );
    }

    #[test]
    fn budgets_zero_max_turns_per_run_is_rejected() {
        let mut cfg = WorkflowConfig::default();
        cfg.tracker.project_slug = Some("ENG".into());
        cfg.budgets.max_turns_per_run = 0;
        assert_eq!(
            cfg.validate(),
            Err(ConfigValidationError::BudgetZero {
                field: "budgets.max_turns_per_run".into(),
            }),
        );
    }

    #[test]
    fn budgets_zero_max_retries_is_rejected() {
        let mut cfg = WorkflowConfig::default();
        cfg.tracker.project_slug = Some("ENG".into());
        cfg.budgets.max_retries = 0;
        assert_eq!(
            cfg.validate(),
            Err(ConfigValidationError::BudgetZero {
                field: "budgets.max_retries".into(),
            }),
        );
    }
}
