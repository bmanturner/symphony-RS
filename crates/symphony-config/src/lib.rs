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

pub mod config;
pub mod loader;

pub use config::{
    AgentConfig, AgentKind, CodexConfig, ConfigValidationError, HooksConfig, PollingConfig,
    TrackerConfig, TrackerKind, WorkflowConfig, WorkspaceConfig,
};
pub use loader::{LoadedWorkflow, WorkflowLoadError, WorkflowLoader};
