//! The [`AgentRunner`] trait — the abstract seam between Symphony and any
//! concrete coding-agent backend (Codex app-server, Claude Code stream-json,
//! the Tandem composite, …).
//!
//! Like [`crate::tracker_trait::TrackerRead`], this trait lives in
//! `symphony-core` deliberately: the orchestrator never speaks Codex or
//! Claude protocol. It only sees the normalized [`AgentEvent`] stream
//! defined here. All backend-specific framing — Codex JSONL `app-server`
//! messages, Claude Code's `stream-json` events — gets translated by the
//! adapter inside `symphony-agent` before reaching the orchestrator.
//!
//! # Identifier shape (SPEC §4.2)
//!
//! Symphony pegs a session identifier to the coding agent's own thread/turn
//! IDs: `session_id = "<thread_id>-<turn_id>"`. The same `thread_id` is
//! reused across continuation turns within one worker run; `turn_id`
//! changes per turn. The [`SessionId`], [`ThreadId`], and [`TurnId`]
//! newtypes preserve that distinction at the type level so adapters can't
//! accidentally pass a thread id where a session id is wanted.
//!
//! # Event shape (SPEC §10.4 + Phase 8 stable wire format)
//!
//! [`AgentEvent`] is the normalized superset of what Codex's app-server and
//! Claude Code's stream-json emit. Every variant carries the [`SessionId`]
//! it pertains to so a Phase 8 SSE consumer can route events without
//! tracking sender state. The enum derives `Serialize` / `Deserialize` with
//! `#[serde(tag = "type")]` so it can be re-emitted as
//! `OrchestratorEvent::Agent` (Phase 8) without re-encoding once the
//! event bus lands.
//!
//! # Stream surface
//!
//! `start_session` returns an [`AgentSession`] bundling three things:
//!
//! 1. The initial [`SessionId`] (so the orchestrator can log a dispatch
//!    line before the first event arrives).
//! 2. An [`AgentEventStream`] — a pinned boxed `futures::Stream` of
//!    [`AgentEvent`]s. We box rather than expose an associated type so the
//!    trait stays object-safe and can be stored as `Arc<dyn AgentRunner>`
//!    in the orchestrator's composition root.
//! 3. An [`AgentControl`] handle the orchestrator uses to ask for a
//!    continuation turn or abort the run. Splitting control from the
//!    stream means a tokio task can poll the stream while a different
//!    task issues `continue_turn` / `abort` without lifetime gymnastics.
//!
//! # Why not a single fat method
//!
//! An earlier draft had `start_session` return only the stream and made
//! `continue_turn` / `abort` methods on the trait, threading a
//! `&SessionHandle` through. That forced every adapter to maintain a
//! session map keyed by handle, which Codex and Claude don't need
//! (they're inherently per-session subprocesses). The split here keeps
//! per-session state local to the [`AgentControl`] impl the adapter
//! returns.

use crate::tracker::IssueId;
use async_trait::async_trait;
use futures::Stream;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::pin::Pin;
use thiserror::Error;

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

/// Coding-agent thread identifier. Stable across continuation turns
/// within one worker run (SPEC §10.2: "Reuse the same `thread_id` for
/// all continuation turns inside one worker run").
///
/// Wrapped as a newtype so it can't be confused with [`TurnId`] or
/// [`SessionId`] at compile time. Internal representation is `String`
/// because both Codex and Claude treat the id opaquely; we never parse
/// them.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ThreadId(String);

impl ThreadId {
    /// Wrap a backend-supplied thread identifier.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Borrow the underlying string slice for logging.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Coding-agent turn identifier. Changes for every new turn (initial
/// dispatch, each continuation). Combined with [`ThreadId`] it forms a
/// [`SessionId`] per SPEC §4.2.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TurnId(String);

impl TurnId {
    /// Wrap a backend-supplied turn identifier.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Borrow the underlying string slice for logging.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Composite session id: `<thread_id>-<turn_id>` per SPEC §4.2 / §10.2.
///
/// The dash separator is part of the SPEC contract; downstream consumers
/// (status surfaces, log scrapers) MAY split on the last `-` to recover
/// turn id, so adapters must construct via [`SessionId::new`] rather than
/// hand-rolling the format.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(String);

impl SessionId {
    /// Compose from a thread + turn id. Uses the SPEC §4.2 separator.
    pub fn new(thread: &ThreadId, turn: &TurnId) -> Self {
        Self(format!("{}-{}", thread.0, turn.0))
    }

    /// Construct from a pre-formatted string. Adapter authors should
    /// prefer [`SessionId::new`]; this is the deserialization escape
    /// hatch (and is used by tests that don't care about the components).
    pub fn from_raw(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Borrow the underlying string slice for logging.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ---------------------------------------------------------------------------
// Telemetry payloads
// ---------------------------------------------------------------------------

/// Token usage snapshot from one agent turn.
///
/// Adapters fill the four fields they can extract from their backend's
/// telemetry. Claude reports `input_tokens` / `output_tokens` /
/// `cache_creation_input_tokens` / `cache_read_input_tokens` in its
/// `result` event; Codex reports `input_tokens` / `output_tokens` plus a
/// total in its `turn_completed` event. Adapters MAY leave fields at zero
/// when their backend genuinely doesn't report them — but they MUST NOT
/// fabricate.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Prompt + system + tool-result tokens consumed by the model.
    pub input: u64,
    /// Tokens generated by the model in its response.
    pub output: u64,
    /// Subset of `input` that was served from prompt cache (Anthropic
    /// reports this; OpenAI reports `cached_tokens` similarly).
    pub cached_input: u64,
    /// Backend-reported total. Adapters that only get input/output
    /// SHOULD set this to `input + output` so consumers don't have to
    /// special-case missing totals.
    pub total: u64,
}

/// Rate-limit snapshot. Symphony's orchestrator stores the *latest* one
/// per agent (SPEC §4.1.8 `codex_rate_limits`); intermediate snapshots
/// are observability only.
///
/// The structured fields capture the headers/fields most agents expose;
/// `raw` preserves the full backend payload so a future status surface
/// can render backend-specific extras (e.g. Anthropic's
/// `anthropic-ratelimit-tokens-remaining` vs OpenAI's window-based
/// counters) without us schema-locking too early.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RateLimitSnapshot {
    /// Requests remaining in the current window, if reported.
    pub remaining_requests: Option<u64>,
    /// Tokens remaining in the current window, if reported.
    pub remaining_tokens: Option<u64>,
    /// Unix timestamp at which the limit resets, if reported.
    pub reset_at_unix: Option<u64>,
    /// Raw backend payload, preserved verbatim for observability.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub raw: serde_json::Value,
}

/// Why a session ended. Mirrors SPEC §10.3 completion conditions.
///
/// Distinguished from `AgentEvent::Failed` because some terminal states
/// (`Cancelled`, `Timeout`) are orchestrator-driven rather than
/// agent-reported errors — keeping them in the variant lets the
/// orchestrator's retry policy branch cleanly without parsing error
/// strings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionReason {
    /// Backend reported a successful turn completion.
    Success,
    /// Backend reported a turn failure (model error, tool failure, etc.).
    Failure,
    /// Orchestrator (or operator) called [`AgentControl::abort`].
    Cancelled,
    /// `turn_timeout_ms` elapsed before completion.
    Timeout,
    /// Agent subprocess exited before the turn terminated.
    SubprocessExit,
}

// ---------------------------------------------------------------------------
// Normalized event stream
// ---------------------------------------------------------------------------

/// The normalized event the orchestrator consumes.
///
/// Encoded as `#[serde(tag = "type")]` so the Phase 8 SSE wire format can
/// switch on `event.type` without shape inference. New variants are
/// non-breaking for SSE consumers (they ignore unknown tags); removing or
/// renaming a variant requires a major version bump.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// Session has begun. Carries the freshly-allocated [`SessionId`] so
    /// downstream subscribers can correlate later events.
    Started {
        /// Composite `<thread>-<turn>` id this turn was assigned.
        session: SessionId,
        /// Which issue this run is for. Lets a TUI panel filter by issue
        /// without joining against orchestrator state.
        issue: IssueId,
    },

    /// Model produced a chat-style message (assistant or user-role tool
    /// result). `content` is the human-readable text; tool calls embedded
    /// in messages are surfaced separately as [`AgentEvent::ToolUse`].
    Message {
        session: SessionId,
        /// Speaker role as reported by the backend (`assistant`,
        /// `user`, `system`). Lower-cased by the adapter for stability.
        role: String,
        /// Plain-text rendering of the message. Adapters that receive
        /// structured content (Claude blocks, Codex multi-part) flatten
        /// to text here and emit any tool blocks as [`AgentEvent::ToolUse`].
        content: String,
    },

    /// Agent invoked a tool. Symphony does not interpret the input; this
    /// is observability so a watcher can render which tools fired.
    ToolUse {
        session: SessionId,
        /// Tool name as advertised by the backend.
        tool: String,
        /// Raw tool input JSON. Preserved verbatim because tool schemas
        /// are agent-defined and we don't want to leak that into the
        /// orchestrator.
        input: serde_json::Value,
    },

    /// Token usage update. Emitted at least once per turn (at completion);
    /// adapters MAY emit incremental updates if their backend supports it.
    TokenUsage {
        session: SessionId,
        usage: TokenUsage,
    },

    /// Rate-limit snapshot from the backend. The orchestrator keeps the
    /// latest one (SPEC §4.1.8); a status surface MAY render history.
    RateLimit {
        session: SessionId,
        snapshot: RateLimitSnapshot,
    },

    /// Terminal: the turn ended. Whichever stream carries this event
    /// SHOULD then close; adapters MUST NOT emit further events on this
    /// session id after `Completed` or [`AgentEvent::Failed`].
    Completed {
        session: SessionId,
        reason: CompletionReason,
    },

    /// Terminal: an unrecoverable error inside the adapter (subprocess
    /// crash, malformed protocol, etc.). The `error` string is for
    /// human consumption; structured handling is via the
    /// [`AgentError`] returned from the originating call.
    Failed { session: SessionId, error: String },
}

impl AgentEvent {
    /// Borrow the [`SessionId`] every variant carries. Useful when a
    /// status surface routes events into per-session ring buffers.
    pub fn session(&self) -> &SessionId {
        match self {
            AgentEvent::Started { session, .. }
            | AgentEvent::Message { session, .. }
            | AgentEvent::ToolUse { session, .. }
            | AgentEvent::TokenUsage { session, .. }
            | AgentEvent::RateLimit { session, .. }
            | AgentEvent::Completed { session, .. }
            | AgentEvent::Failed { session, .. } => session,
        }
    }

    /// Returns `true` if this variant terminates the stream — that is,
    /// the orchestrator should stop polling once it sees this event.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            AgentEvent::Completed { .. } | AgentEvent::Failed { .. }
        )
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Failures the runner surfaces as call-site errors (as opposed to
/// in-stream [`AgentEvent::Failed`] events).
///
/// The split is: anything that prevents the stream from existing at all
/// surfaces here; anything that occurs after the stream is established
/// flows through it. So a "binary not on PATH" is [`AgentError::Spawn`],
/// but a mid-turn subprocess crash arrives as
/// `AgentEvent::Completed { reason: SubprocessExit }`.
#[derive(Debug, Error)]
pub enum AgentError {
    /// Failed to spawn the agent subprocess (binary missing, exec
    /// permission denied, working directory unreadable, …).
    #[error("agent spawn failed: {0}")]
    Spawn(String),

    /// Protocol-level failure during session startup (handshake mismatch,
    /// unsupported app-server version, malformed bootstrap message).
    #[error("agent protocol failure: {0}")]
    Protocol(String),

    /// Operator config rejected by the runner (bad model name, unknown
    /// permission mode, …). Mirrors [`crate::tracker_trait::TrackerError::Misconfigured`].
    #[error("agent misconfigured: {0}")]
    Misconfigured(String),

    /// `continue_turn` / `abort` called against a session id the runner
    /// no longer tracks. Almost always a bug in the orchestrator state
    /// machine, but we surface it rather than panicking.
    #[error("agent session {0} not found")]
    SessionNotFound(SessionId),

    /// Catch-all for adapter-internal failures.
    #[error("agent error: {0}")]
    Other(String),
}

/// Convenience alias for runner-method results.
pub type AgentResult<T> = Result<T, AgentError>;

// ---------------------------------------------------------------------------
// Trait surface
// ---------------------------------------------------------------------------

/// Pinned boxed [`AgentEvent`] stream. Aliased so the trait surface stays
/// readable; consumers can pattern-match the inner stream type if they
/// need to.
pub type AgentEventStream = Pin<Box<dyn Stream<Item = AgentEvent> + Send>>;

/// Inputs for [`AgentRunner::start_session`].
///
/// Kept as a struct rather than positional arguments so adding fields
/// (Phase 6 will add `tandem_strategy`, Phase 7 may add log-redirect
/// handles) is non-breaking.
#[derive(Debug, Clone)]
pub struct StartSessionParams {
    /// Issue this session is dispatched for. Carried into the first
    /// [`AgentEvent::Started`] so subscribers can correlate.
    pub issue: IssueId,
    /// Absolute path the agent subprocess should `cwd` into. SPEC §10.1
    /// requires the per-issue workspace path here.
    pub workspace: PathBuf,
    /// Rendered prompt body for the first turn (SPEC §10.2: "Start the
    /// first turn with the rendered issue prompt").
    pub initial_prompt: String,
    /// Optional human label (e.g. `"<identifier>: <title>"`) advertised
    /// to the backend as a turn/session title when the protocol supports
    /// it (SPEC §10.2).
    pub issue_label: Option<String>,
}

/// Per-session control surface. Returned from [`AgentRunner::start_session`]
/// alongside the event stream so the orchestrator can request continuation
/// or abort without re-locking the runner.
///
/// Methods take `&self` because the implementation typically holds an
/// internal `Mutex` over its tokio child handle; concurrent
/// `continue_turn` calls on the same session are a programmer error and
/// should surface as [`AgentError::Other`] rather than block.
#[async_trait]
pub trait AgentControl: Send + Sync {
    /// Start another turn on the live thread with continuation guidance
    /// (SPEC §10.3 "Continuation processing"). Resending the original
    /// issue prompt is explicitly disallowed by the SPEC — pass only
    /// the new guidance.
    ///
    /// Emits a fresh [`AgentEvent::Started`] on the same event stream
    /// with a new turn id.
    async fn continue_turn(&self, guidance: &str) -> AgentResult<()>;

    /// Cancel the in-flight turn and end the session. Causes the stream
    /// to deliver `AgentEvent::Completed { reason: Cancelled }` and
    /// then close. Idempotent — calling twice is a no-op, not an error.
    async fn abort(&self) -> AgentResult<()>;
}

/// What [`AgentRunner::start_session`] returns.
///
/// A struct rather than a tuple so call sites can name the fields and so
/// later phases can grow it without breaking signatures.
pub struct AgentSession {
    /// The first [`SessionId`] the backend allocated. Subsequent turns
    /// may have different ids; the orchestrator updates from in-stream
    /// `Started` events.
    pub initial_session: SessionId,
    /// Stream of normalized events. Closes when the session is
    /// terminal. Consumed exactly once.
    pub events: AgentEventStream,
    /// Control surface for `continue_turn` / `abort`.
    pub control: Box<dyn AgentControl>,
}

/// Abstract coding-agent backend.
///
/// One implementation per backend (Codex, Claude, Tandem) plus
/// `MockRunner` for tests. The orchestrator stores its runner as
/// `Arc<dyn AgentRunner>` so the trait must stay object-safe.
#[async_trait]
pub trait AgentRunner: Send + Sync {
    /// Start a fresh session in the given workspace with the given
    /// initial prompt. Returns the bundle of (initial id, event stream,
    /// control handle) once the backend has accepted the launch.
    ///
    /// Failure modes that prevent the stream from existing surface as
    /// [`AgentError`]; failures that occur once the stream is live flow
    /// through it as [`AgentEvent::Failed`] / `Completed`.
    async fn start_session(&self, params: StartSessionParams) -> AgentResult<AgentSession>;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;
    use std::sync::Arc;

    #[test]
    fn session_id_uses_spec_separator() {
        let s = SessionId::new(&ThreadId::new("th_42"), &TurnId::new("tn_7"));
        assert_eq!(s.as_str(), "th_42-tn_7");
    }

    #[test]
    fn session_id_round_trips_through_serde() {
        let s = SessionId::new(&ThreadId::new("a"), &TurnId::new("b"));
        let j = serde_json::to_string(&s).unwrap();
        // transparent newtype → quoted string, no enclosing object
        assert_eq!(j, r#""a-b""#);
        let back: SessionId = serde_json::from_str(&j).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn agent_event_serializes_with_type_tag() {
        let ev = AgentEvent::Started {
            session: SessionId::from_raw("th-tn"),
            issue: IssueId::new("ABC-1"),
        };
        let j = serde_json::to_value(&ev).unwrap();
        assert_eq!(j["type"], "started");
        assert_eq!(j["session"], "th-tn");
        assert_eq!(j["issue"], "ABC-1");
    }

    #[test]
    fn agent_event_round_trips_every_variant() {
        let session = SessionId::from_raw("th-tn");
        let cases = vec![
            AgentEvent::Started {
                session: session.clone(),
                issue: IssueId::new("X-1"),
            },
            AgentEvent::Message {
                session: session.clone(),
                role: "assistant".into(),
                content: "hello".into(),
            },
            AgentEvent::ToolUse {
                session: session.clone(),
                tool: "bash".into(),
                input: serde_json::json!({"cmd": "ls"}),
            },
            AgentEvent::TokenUsage {
                session: session.clone(),
                usage: TokenUsage {
                    input: 10,
                    output: 20,
                    cached_input: 5,
                    total: 30,
                },
            },
            AgentEvent::RateLimit {
                session: session.clone(),
                snapshot: RateLimitSnapshot {
                    remaining_requests: Some(99),
                    remaining_tokens: Some(50_000),
                    reset_at_unix: Some(1_700_000_000),
                    raw: serde_json::Value::Null,
                },
            },
            AgentEvent::Completed {
                session: session.clone(),
                reason: CompletionReason::Success,
            },
            AgentEvent::Failed {
                session: session.clone(),
                error: "boom".into(),
            },
        ];
        for ev in cases {
            let j = serde_json::to_string(&ev).unwrap();
            let back: AgentEvent = serde_json::from_str(&j).unwrap();
            assert_eq!(back, ev, "round-trip mismatch for {ev:?}");
        }
    }

    #[test]
    fn agent_event_session_accessor_returns_each_variants_id() {
        let s = SessionId::from_raw("s");
        let cases = [
            AgentEvent::Started {
                session: s.clone(),
                issue: IssueId::new("I"),
            },
            AgentEvent::Completed {
                session: s.clone(),
                reason: CompletionReason::Cancelled,
            },
            AgentEvent::Failed {
                session: s.clone(),
                error: "e".into(),
            },
        ];
        for ev in &cases {
            assert_eq!(ev.session(), &s);
        }
    }

    #[test]
    fn is_terminal_only_for_completed_and_failed() {
        let s = SessionId::from_raw("s");
        assert!(
            AgentEvent::Completed {
                session: s.clone(),
                reason: CompletionReason::Success
            }
            .is_terminal()
        );
        assert!(
            AgentEvent::Failed {
                session: s.clone(),
                error: "x".into()
            }
            .is_terminal()
        );
        assert!(
            !AgentEvent::Message {
                session: s.clone(),
                role: "assistant".into(),
                content: "hi".into()
            }
            .is_terminal()
        );
        assert!(
            !AgentEvent::TokenUsage {
                session: s,
                usage: TokenUsage::default()
            }
            .is_terminal()
        );
    }

    #[test]
    fn agent_error_display_is_informative() {
        assert!(
            AgentError::Spawn("ENOENT".into())
                .to_string()
                .contains("spawn")
        );
        assert!(
            AgentError::Protocol("bad handshake".into())
                .to_string()
                .contains("protocol")
        );
        assert!(
            AgentError::Misconfigured("model required".into())
                .to_string()
                .contains("misconfigured")
        );
        assert!(
            AgentError::SessionNotFound(SessionId::from_raw("s"))
                .to_string()
                .contains("session s")
        );
    }

    #[test]
    fn agent_error_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<AgentError>();
    }

    /// Stub that proves the trait is object-safe and an `Arc<dyn
    /// AgentRunner>` round-trips through `start_session`. The real
    /// `MockRunner` lands in `symphony-agent::mock` once the dedicated
    /// checklist item arrives in Phase 4.
    struct StubRunner;

    struct StubControl;

    #[async_trait]
    impl AgentControl for StubControl {
        async fn continue_turn(&self, _guidance: &str) -> AgentResult<()> {
            Ok(())
        }
        async fn abort(&self) -> AgentResult<()> {
            Ok(())
        }
    }

    #[async_trait]
    impl AgentRunner for StubRunner {
        async fn start_session(&self, params: StartSessionParams) -> AgentResult<AgentSession> {
            let session = SessionId::new(&ThreadId::new("th"), &TurnId::new("tn"));
            let started = AgentEvent::Started {
                session: session.clone(),
                issue: params.issue,
            };
            let completed = AgentEvent::Completed {
                session: session.clone(),
                reason: CompletionReason::Success,
            };
            Ok(AgentSession {
                initial_session: session,
                events: Box::pin(stream::iter(vec![started, completed])),
                control: Box::new(StubControl),
            })
        }
    }

    #[tokio::test]
    async fn trait_is_object_safe_and_callable_through_arc_dyn() {
        use futures::StreamExt;
        let runner: Arc<dyn AgentRunner> = Arc::new(StubRunner);
        let session = runner
            .start_session(StartSessionParams {
                issue: IssueId::new("X-1"),
                workspace: PathBuf::from("/tmp/ws"),
                initial_prompt: "do the thing".into(),
                issue_label: Some("X-1: do the thing".into()),
            })
            .await
            .expect("start_session ok");

        assert_eq!(session.initial_session.as_str(), "th-tn");

        let events: Vec<AgentEvent> = session.events.collect().await;
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], AgentEvent::Started { .. }));
        assert!(events[1].is_terminal());

        // Control surface is reachable through the boxed trait object.
        session.control.continue_turn("press on").await.unwrap();
        session.control.abort().await.unwrap();
    }
}
