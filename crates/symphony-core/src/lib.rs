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
pub mod tracker;
pub mod tracker_trait;

pub use agent::{
    AgentControl, AgentError, AgentEvent, AgentEventStream, AgentResult, AgentRunner, AgentSession,
    CompletionReason, RateLimitSnapshot, SessionId, StartSessionParams, ThreadId, TokenUsage,
    TurnId,
};
pub use tracker::{BlockerRef, Issue, IssueId, IssueState};
pub use tracker_trait::{IssueTracker, TrackerError, TrackerResult};
