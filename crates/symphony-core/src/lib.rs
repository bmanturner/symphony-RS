//! Symphony orchestrator core.
//!
//! This crate defines the *composition root* of Symphony-RS: the poll loop,
//! the per-issue state machine (SPEC §4.1.8), and the abstract trait surface
//! that decouples the orchestrator from any concrete tracker or agent
//! backend. Concrete adapters live in sibling crates (`symphony-tracker`,
//! `symphony-agent`, `symphony-workspace`).
//!
//! Architectural invariant: nothing in this crate may depend on
//! Linear/Codex/Claude protocol details. The orchestrator only sees the
//! normalized `Issue` model and the `AgentEvent` enum (defined in later
//! phases).

pub mod agent;
pub mod blocker;
pub mod event_bus;
pub mod events;
pub mod poll_loop;
pub mod retry;
pub mod role;
pub mod state_machine;
pub mod tracker;
pub mod tracker_trait;
pub mod work_item;

pub use agent::{
    AgentControl, AgentError, AgentEvent, AgentEventStream, AgentResult, AgentRunner, AgentSession,
    CompletionReason, RateLimitSnapshot, SessionId, StartSessionParams, ThreadId, TokenUsage,
    TurnId,
};
pub use blocker::{
    Blocker, BlockerError, BlockerId, BlockerOrigin, BlockerSeverity, BlockerStatus, RunRef,
};
pub use event_bus::{DEFAULT_REPLAY_BUFFER, EventBus};
pub use events::OrchestratorEvent;
pub use poll_loop::{Dispatcher, PollLoop, PollLoopConfig, TickReport};
pub use retry::{RetryConfig, RetryEntry, RetryQueue, RetryReason, ScheduleRequest, backoff_for};
pub use role::{RoleAuthority, RoleAuthorityOverrides, RoleContext, RoleKind, RoleName};
pub use state_machine::{ClaimState, ReleaseReason, StateMachine, TransitionError};
pub use tracker::{BlockerRef, Issue, IssueId, IssueState};
pub use tracker_trait::{IssueTracker, TrackerError, TrackerResult};
pub use work_item::{
    ClassifyError, StatusClassifier, TrackerStatus, UnknownStatePolicy, WorkItem, WorkItemId,
    WorkItemStatusClass,
};
