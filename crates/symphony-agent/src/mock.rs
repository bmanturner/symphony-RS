//! In-process [`AgentRunner`] used by the Quickstart and CLI smoke tests.
//!
//! The Quickstart needs `symphony run` to dispatch end-to-end without
//! `codex` or `claude` on PATH. [`MockAgentRunner`] satisfies that by
//! emitting a deterministic, scripted event sequence per turn:
//!
//! 1. [`AgentEvent::Started`] with a freshly-allocated [`SessionId`]
//! 2. [`AgentEvent::Message`] echoing the configured reply
//! 3. [`AgentEvent::Completed`] with [`CompletionReason::Success`]
//!
//! Subsequent [`AgentControl::continue_turn`] calls re-run the same
//! script with a new turn id. [`AgentControl::abort`] flips the session
//! into a terminal `Cancelled` state and closes the stream.
//!
//! Why a real `AgentRunner` impl (and not a `#[cfg(test)]` stub)? The
//! Quickstart smoke test needs the same runner the operator gets when
//! they set `agent.kind: mock` in `WORKFLOW.md`. Lifting it into the
//! crate's public API lets one runner serve both production
//! demonstrations and the in-tree integration tests, so we never have
//! "tests pass but the demo path is dead."

use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll};
use symphony_core::agent::{
    AgentControl, AgentEvent, AgentEventStream, AgentResult, AgentRunner, AgentSession,
    CompletionReason, SessionId, StartSessionParams, ThreadId, TokenUsage, TurnId,
};
use tokio::sync::{Mutex, mpsc};

/// Default reply body emitted in the scripted [`AgentEvent::Message`].
pub const DEFAULT_MOCK_MESSAGE: &str = "mock agent: pretending to work the issue";

/// Configuration for [`MockAgentRunner`].
///
/// The fields are intentionally narrow — the mock is a fixture, not a
/// product. Extending it without a corresponding Quickstart change is
/// almost always a smell.
#[derive(Debug, Clone)]
pub struct MockAgentConfig {
    /// Body used for the scripted `Message` event each turn. Defaults to
    /// [`DEFAULT_MOCK_MESSAGE`]. Surfaced so tests can assert on a
    /// distinguishable string when more than one mock is in play.
    pub message: String,

    /// Optional [`TokenUsage`] emitted before the terminal event. `None`
    /// suppresses the `TokenUsage` event entirely so a Quickstart
    /// operator's cost panel reads "—" rather than a fabricated number.
    pub token_usage: Option<TokenUsage>,
}

impl Default for MockAgentConfig {
    fn default() -> Self {
        Self {
            message: DEFAULT_MOCK_MESSAGE.to_string(),
            token_usage: None,
        }
    }
}

/// `AgentRunner` that returns a scripted event sequence without spawning
/// any subprocess.
///
/// Stateless across sessions: each `start_session` call mints a fresh
/// thread id from a process-wide counter and routes events through a new
/// channel. Per-session state (turn counter, abort flag) lives on the
/// returned [`MockAgentControl`].
#[derive(Debug, Clone, Default)]
pub struct MockAgentRunner {
    config: MockAgentConfig,
    next_thread: Arc<AtomicU64>,
}

impl MockAgentRunner {
    /// Build a runner with the supplied config.
    pub fn new(config: MockAgentConfig) -> Self {
        Self {
            config,
            next_thread: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Borrow the config (used by tests and CLI factory wiring).
    pub fn config(&self) -> &MockAgentConfig {
        &self.config
    }
}

#[async_trait]
impl AgentRunner for MockAgentRunner {
    async fn start_session(&self, params: StartSessionParams) -> AgentResult<AgentSession> {
        // Allocate a stable thread id per session and start the turn
        // counter at 1. The combination feeds SessionId::new so consumers
        // can split on the SPEC §4.2 dash separator if they want to.
        let thread_n = self.next_thread.fetch_add(1, Ordering::SeqCst);
        let thread = ThreadId::new(format!("mock-th-{thread_n}"));
        let next_turn = Arc::new(AtomicU64::new(1));

        // Buffer of 8 is generous for the three-event script; the bound
        // exists only so a runaway abort spam can't grow unbounded.
        let (tx, rx) = mpsc::channel::<AgentEvent>(8);

        let initial_session = emit_turn(
            &tx,
            &thread,
            &next_turn,
            &params,
            &self.config,
            /*aborted=*/ false,
        )
        .await;

        let control = MockAgentControl {
            config: self.config.clone(),
            tx: Mutex::new(Some(tx)),
            thread,
            next_turn,
            params: params.clone(),
            aborted: AtomicBool::new(false),
        };

        Ok(AgentSession {
            initial_session,
            events: stream_from_receiver(rx),
            control: Box::new(control),
        })
    }
}

/// Per-session control returned alongside the event stream.
///
/// Holds the channel sender so [`continue_turn`](AgentControl::continue_turn)
/// can push another scripted turn into the live stream, and an
/// `aborted` flag so a second `abort` is a no-op rather than a double
/// `Completed`.
pub struct MockAgentControl {
    config: MockAgentConfig,
    /// `Some` while the session is live; cleared on `abort` so dropped
    /// senders close the stream.
    tx: Mutex<Option<mpsc::Sender<AgentEvent>>>,
    thread: ThreadId,
    next_turn: Arc<AtomicU64>,
    params: StartSessionParams,
    aborted: AtomicBool,
}

#[async_trait]
impl AgentControl for MockAgentControl {
    async fn continue_turn(&self, _guidance: &str) -> AgentResult<()> {
        if self.aborted.load(Ordering::SeqCst) {
            // Match the SPEC's idempotency rule: post-abort calls are
            // observable as no-ops, not errors.
            return Ok(());
        }
        let guard = self.tx.lock().await;
        let Some(tx) = guard.as_ref() else {
            return Ok(());
        };
        emit_turn(
            tx,
            &self.thread,
            &self.next_turn,
            &self.params,
            &self.config,
            false,
        )
        .await;
        Ok(())
    }

    async fn abort(&self) -> AgentResult<()> {
        if self.aborted.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let mut guard = self.tx.lock().await;
        if let Some(tx) = guard.take() {
            // Emit a terminal Cancelled event using the next turn id so
            // the session id is stable-shaped, then drop the sender so
            // the stream closes.
            let turn = TurnId::new(format!(
                "tn-{}",
                self.next_turn.fetch_add(1, Ordering::SeqCst)
            ));
            let session = SessionId::new(&self.thread, &turn);
            let _ = tx
                .send(AgentEvent::Completed {
                    session,
                    reason: CompletionReason::Cancelled,
                })
                .await;
        }
        Ok(())
    }
}

/// Push one full scripted turn into `tx` and return the [`SessionId`]
/// the `Started` event used. Errors during send are dropped on the
/// floor — the receiver may have hung up, which is the orchestrator's
/// signal that nobody is listening.
async fn emit_turn(
    tx: &mpsc::Sender<AgentEvent>,
    thread: &ThreadId,
    next_turn: &AtomicU64,
    params: &StartSessionParams,
    config: &MockAgentConfig,
    _aborted: bool,
) -> SessionId {
    let turn = TurnId::new(format!("tn-{}", next_turn.fetch_add(1, Ordering::SeqCst)));
    let session = SessionId::new(thread, &turn);

    let _ = tx
        .send(AgentEvent::Started {
            session: session.clone(),
            issue: params.issue.clone(),
        })
        .await;
    let _ = tx
        .send(AgentEvent::Message {
            session: session.clone(),
            role: "assistant".into(),
            content: config.message.clone(),
        })
        .await;
    if let Some(usage) = config.token_usage.clone() {
        let _ = tx
            .send(AgentEvent::TokenUsage {
                session: session.clone(),
                usage,
            })
            .await;
    }
    let _ = tx
        .send(AgentEvent::Completed {
            session: session.clone(),
            reason: CompletionReason::Success,
        })
        .await;
    session
}

/// Adapt a `tokio::sync::mpsc::Receiver` into the boxed
/// `Stream<Item = AgentEvent>` the trait surface expects.
///
/// We hand-roll this rather than pulling in `tokio_stream::wrappers`
/// because it's two lines of code and avoids growing the agent crate's
/// dep surface.
fn stream_from_receiver(rx: mpsc::Receiver<AgentEvent>) -> AgentEventStream {
    Box::pin(ReceiverStream { rx })
}

struct ReceiverStream {
    rx: mpsc::Receiver<AgentEvent>,
}

impl Stream for ReceiverStream {
    type Item = AgentEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use std::path::PathBuf;
    use symphony_core::tracker::IssueId;

    fn params() -> StartSessionParams {
        StartSessionParams {
            issue: IssueId::new("MOCK-1"),
            workspace: PathBuf::from("/tmp/mock-ws"),
            initial_prompt: "do the thing".into(),
            issue_label: Some("MOCK-1: do the thing".into()),
        }
    }

    async fn collect(mut stream: AgentEventStream) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        while let Some(ev) = stream.next().await {
            out.push(ev);
        }
        out
    }

    #[tokio::test]
    async fn first_turn_emits_started_message_completed() {
        let runner = MockAgentRunner::new(MockAgentConfig::default());
        let session = runner.start_session(params()).await.unwrap();
        // Drop the control to close the channel after the scripted turn
        // so the stream terminates and `collect` returns.
        drop(session.control);
        let events = collect(session.events).await;
        assert_eq!(events.len(), 3, "expected Started/Message/Completed");
        assert!(matches!(events[0], AgentEvent::Started { .. }));
        match &events[1] {
            AgentEvent::Message { role, content, .. } => {
                assert_eq!(role, "assistant");
                assert_eq!(content, DEFAULT_MOCK_MESSAGE);
            }
            other => panic!("expected Message, got {other:?}"),
        }
        assert!(matches!(
            events[2],
            AgentEvent::Completed {
                reason: CompletionReason::Success,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn continue_turn_emits_a_second_scripted_turn() {
        let runner = MockAgentRunner::new(MockAgentConfig::default());
        let session = runner.start_session(params()).await.unwrap();
        let symphony_core::agent::AgentSession {
            mut events,
            control,
            ..
        } = session;
        // Drain the first turn synchronously.
        for _ in 0..3 {
            events.next().await.expect("first turn event");
        }
        control.continue_turn("press on").await.unwrap();
        // Second turn arrives in the same shape.
        let started = events.next().await.expect("second turn started");
        assert!(matches!(started, AgentEvent::Started { .. }));
        let _msg = events.next().await.expect("second turn message");
        let done = events.next().await.expect("second turn completed");
        assert!(matches!(
            done,
            AgentEvent::Completed {
                reason: CompletionReason::Success,
                ..
            }
        ));
        // And the session ids advance the turn counter.
        if let (
            AgentEvent::Started { session: s1, .. },
            AgentEvent::Completed { session: s2, .. },
        ) = (&started, &done)
        {
            assert_eq!(s1, s2, "Started and Completed share the same session id");
            assert!(s1.as_str().ends_with("tn-2"), "got {}", s1.as_str());
        }
    }

    #[tokio::test]
    async fn abort_emits_cancelled_and_closes_stream() {
        let runner = MockAgentRunner::new(MockAgentConfig::default());
        let session = runner.start_session(params()).await.unwrap();
        let symphony_core::agent::AgentSession {
            mut events,
            control,
            ..
        } = session;
        for _ in 0..3 {
            events.next().await.unwrap();
        }
        control.abort().await.unwrap();
        let cancel = events.next().await.expect("cancellation event");
        assert!(matches!(
            cancel,
            AgentEvent::Completed {
                reason: CompletionReason::Cancelled,
                ..
            }
        ));
        // Stream closes after the cancellation event.
        assert!(events.next().await.is_none());
        // Second abort is a no-op, not an error.
        control.abort().await.unwrap();
    }

    #[tokio::test]
    async fn token_usage_is_emitted_only_when_configured() {
        let cfg = MockAgentConfig {
            message: "scripted".into(),
            token_usage: Some(TokenUsage {
                input: 4,
                output: 8,
                cached_input: 0,
                total: 12,
            }),
        };
        let runner = MockAgentRunner::new(cfg);
        let session = runner.start_session(params()).await.unwrap();
        drop(session.control);
        let events = collect(session.events).await;
        assert_eq!(events.len(), 4, "Started/Message/TokenUsage/Completed");
        match &events[2] {
            AgentEvent::TokenUsage { usage, .. } => {
                assert_eq!(usage.total, 12);
                assert_eq!(usage.input, 4);
                assert_eq!(usage.output, 8);
            }
            other => panic!("expected TokenUsage, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn each_session_uses_a_distinct_thread_id() {
        let runner = MockAgentRunner::new(MockAgentConfig::default());
        let s1 = runner.start_session(params()).await.unwrap();
        let s2 = runner.start_session(params()).await.unwrap();
        assert_ne!(
            s1.initial_session, s2.initial_session,
            "fresh sessions get distinct ids"
        );
    }
}
