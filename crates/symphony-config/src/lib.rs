//! Layered configuration for Symphony-RS.
//!
//! Responsibilities (mirrors SPEC §3.1 "Workflow Loader" and "Config
//! Layer"):
//!
//! - Parse `WORKFLOW.md` into `{front_matter, prompt_template}` using
//!   `gray_matter`.
//! - Layer configuration sources in this priority order: built-in defaults
//!   → environment variables → `WORKFLOW.md` front matter. Higher
//!   precedence wins (`figment` handles the merge).
//! - Hot-reload `WORKFLOW.md` via `notify` so operators can iterate on the
//!   prompt without restarting the daemon.
//!
//! This crate intentionally has no async surface; the orchestrator owns
//! the runtime and feeds parsed config in.

pub mod adapter_scope;
pub mod config;
pub mod layered;
pub mod loader;
pub mod role_catalog;

pub use adapter_scope::{
    SUPPORTED_PRODUCT_AGENT_BACKENDS, SUPPORTED_PRODUCT_TRACKERS, SUPPORTED_SOURCE_CONTROL_ADAPTER,
    is_product_agent_backend, is_product_tracker,
};

pub use config::{
    AgentBackend, AgentBackendProfile, AgentCompositeProfile, AgentConfig, AgentKind,
    AgentProfileConfig, AgentStrategy, BlockerPolicy, BranchPolicyConfig, ChildIssuePolicy,
    CodexConfig, ConfigValidationError, ConfigValidationWarning, ConflictPolicy, DashboardConfig,
    DecompositionConfig, DecompositionTriggers, DependencyPolicy, DependencyTrackerSyncPolicy,
    FollowupConfig, HermesAgentConfig, HooksConfig, IntegrationConfig, IntegrationRequirement,
    LeaseConfig, LogFormat, LogsConfig, MergeStrategy, ObservabilityConfig, PollingConfig,
    PrInitialState, PrMarkReadyStage, PrOpenStage, PrProvider, PullRequestConfig, QaConfig,
    QaEvidenceRequired, RoleAssignmentMetadata, RoleConfig, RoleInstructionConfig, RoleKind,
    RoleRoutingHints, RoutingConfig, RoutingMatch, RoutingMatchMode, RoutingRule,
    SUPPORTED_SCHEMA_VERSION, SseConfig, TandemMode, TrackerConfig, TrackerKind, TuiConfig,
    WorkflowConfig, WorkspaceCleanupPolicy, WorkspacePolicyConfig, WorkspaceStrategyConfig,
    WorkspaceStrategyKind,
};
pub use layered::{LayeredLoadError, LayeredLoader};
pub use loader::{
    InstructionKind, InstructionPackBundle, InstructionSource, LoadedRoleInstruction,
    LoadedRoleInstructionPack, LoadedWorkflow, WorkflowLoadError, WorkflowLoader,
    redact_instruction_content,
};
pub use role_catalog::{
    InstructionPackSummary, RoleCatalog, RoleCatalogBuilder, RoleCatalogEntry,
    RoleCatalogSourceSummary, RoutingRuleSummary,
};
