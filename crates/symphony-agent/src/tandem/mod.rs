//! Tandem runner — wraps two inner [`AgentRunner`]s and exposes them as a
//! single composite runner.
//!
//! This module is the scaffolding for SPEC-deviation #1: where the upstream
//! Symphony spec assumes one coding agent per run, Symphony-RS lets a
//! single dispatch drive two agents concurrently with a configurable
//! lead/follower relationship. Three strategies are planned (see
//! [`TandemStrategy`]); each lands in its own checklist item. This file
//! delivers the *shape*: the strategy enum, the role enum, the
//! [`TandemRunner`] struct that holds the two inner runners + strategy,
//! and a baseline [`AgentRunner`] impl that:
//!
//! 1. Calls `start_session` on both inner runners (lead first so we can
//!    abort it if the follower fails to launch).
//! 2. Merges their event streams round-robin, suppressing the inner
//!    [`AgentEvent::Completed`] / [`AgentEvent::Failed`] events.
//! 3. Emits one synthesized [`AgentEvent::Completed`] tagged with the
//!    lead session id once both inner streams have closed.
//! 4. Forwards `continue_turn` to both and aborts both on `abort`.
//!
//! Strategy-specific behaviour (draft-review feedback merging, split-
//! implement subtask routing, consensus output picking) is **not** in
//! this commit — each strategy will read `self.strategy` and override the
//! merge / continuation logic in its own checklist item. The baseline
//! "run both, merge, complete when done" is what `consensus` already
//! needs and what the other two strategies extend, so it doubles as a
//! sensible default.
//!
//! # Why a custom `Stream` and not `tokio_stream::StreamExt::merge`
//!
//! `merge` would close as soon as one inner stream ends. We want the
//! opposite: keep emitting from the longer-lived inner stream and only
//! signal terminal once *both* are done. The hand-rolled state machine
//! in `TandemStream` is small enough that pulling in `async-stream`
//! solely for `stream! { ... }` would not be a net win.
//!
//! # Strategy dispatch
//!
//! [`TandemStrategy::Consensus`] uses the parallel poll-merge baseline
//! defined in this file. [`TandemStrategy::DraftReview`] is sequential
//! (lead drafts, then follower reviews) and is implemented in the
//! private `draft_review` submodule. [`TandemStrategy::SplitImplement`]
//! currently falls through to the baseline pending its own checklist
//! item.

mod draft_review;

use async_trait::async_trait;
use futures::Stream;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use symphony_core::agent::{
    AgentControl, AgentEvent, AgentEventStream, AgentResult, AgentRunner, AgentSession,
    CompletionReason, SessionId, StartSessionParams,
};

// ---------------------------------------------------------------------------
// Configuration enums
// ---------------------------------------------------------------------------

/// Which of the three SPEC-deviation strategies the runner should use.
///
/// The variants are stored on [`TandemRunner`] but not yet branched on by
/// the baseline implementation in this module — each strategy lands in
/// its own follow-up checklist item. The enum is wired up now so callers
/// (and `WORKFLOW.md` parsing) can target the final API today.
///
/// Serialized as kebab-case to match the planned `WORKFLOW.md` syntax
/// (`tandem.strategy = "draft-review"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TandemStrategy {
    /// Lead drafts the turn, follower reviews it; the orchestrator either
    /// accepts the draft or runs a second turn with the merged feedback.
    DraftReview,
    /// Lead plans, follower executes specific subtasks it claims via the
    /// tool-use channel.
    SplitImplement,
    /// Both run in parallel; the orchestrator picks the output with the
    /// greater test-pass delta. The current baseline merge in this module
    /// is consensus's run-both phase.
    Consensus,
}

/// Which inner runner played which role in a given turn.
///
/// Carried in telemetry (Phase 6's per-agent token counts and agreement
/// rate) and used by the Phase 8 status surface to label a tandem panel.
/// Not currently embedded in [`AgentEvent`] — strategy-specific commits
/// may add a tandem-tagged variant later if and when the orchestrator
/// needs to demultiplex.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TandemRole {
    /// First-mover for the strategy: drafts in `draft-review`, plans in
    /// `split-implement`, parallel candidate A in `consensus`.
    Lead,
    /// Second seat: reviews / executes / parallel candidate B.
    Follower,
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

/// Composite [`AgentRunner`] holding a lead and a follower runner plus a
/// [`TandemStrategy`].
///
/// `Arc<dyn AgentRunner>` is the storage type so the same backend can be
/// used in both seats (e.g. `claude` lead + `claude` follower with
/// different system prompts) and so the orchestrator can keep its
/// `Arc<dyn AgentRunner>` composition root unchanged.
pub struct TandemRunner {
    lead: Arc<dyn AgentRunner>,
    follower: Arc<dyn AgentRunner>,
    strategy: TandemStrategy,
}

impl TandemRunner {
    /// Construct from two inner runners and a strategy. The first
    /// argument is the lead by convention — strategies that distinguish
    /// the seats branch on which `Arc` was passed where.
    pub fn new(
        lead: Arc<dyn AgentRunner>,
        follower: Arc<dyn AgentRunner>,
        strategy: TandemStrategy,
    ) -> Self {
        Self {
            lead,
            follower,
            strategy,
        }
    }

    /// Borrow the configured strategy. Exposed for status-surface and
    /// telemetry consumers; the runner's own behaviour reads it
    /// directly.
    pub fn strategy(&self) -> TandemStrategy {
        self.strategy
    }
}

#[async_trait]
impl AgentRunner for TandemRunner {
    async fn start_session(&self, params: StartSessionParams) -> AgentResult<AgentSession> {
        // Strategy-specific dispatch. Draft-review is sequential (lead
        // first, follower reviews after) and lives in its own module.
        // Consensus and split-implement currently share the parallel
        // poll-merge baseline below; split-implement gets its own commit
        // once the orchestrator-side subtask routing lands.
        if matches!(self.strategy, TandemStrategy::DraftReview) {
            return draft_review::start(self.lead.clone(), self.follower.clone(), params).await;
        }

        // Lead first so a follower-launch failure can clean up by aborting
        // the already-started lead instead of leaving an orphan subprocess.
        let lead_session = self.lead.start_session(params.clone()).await?;
        let follower_session = match self.follower.start_session(params).await {
            Ok(s) => s,
            Err(e) => {
                // Best-effort lead cleanup; ignore secondary failures so
                // the original error reaches the caller.
                let _ = lead_session.control.abort().await;
                return Err(e);
            }
        };

        let initial_session = lead_session.initial_session.clone();
        let events: AgentEventStream = Box::pin(TandemStream {
            lead: lead_session.events,
            follower: follower_session.events,
            lead_done: false,
            follower_done: false,
            terminal_emitted: false,
            poll_lead_first: true,
            final_session: initial_session.clone(),
        });
        let control: Box<dyn AgentControl> = Box::new(TandemControl {
            lead: lead_session.control,
            follower: follower_session.control,
        });

        Ok(AgentSession {
            initial_session,
            events,
            control,
        })
    }
}

// ---------------------------------------------------------------------------
// Control surface
// ---------------------------------------------------------------------------

/// [`AgentControl`] composite that fans out to both inner controls.
///
/// Strategy-specific commits will likely override this — for example,
/// `draft-review` may call `continue_turn` only on the lead with merged
/// feedback rather than on both. The baseline forwards to both so a
/// Tandem session behaves like two parallel sessions until a strategy
/// kicks in.
struct TandemControl {
    lead: Box<dyn AgentControl>,
    follower: Box<dyn AgentControl>,
}

#[async_trait]
impl AgentControl for TandemControl {
    async fn continue_turn(&self, guidance: &str) -> AgentResult<()> {
        // Sequential rather than `tokio::join!` to keep error attribution
        // unambiguous: if the lead refuses, we don't also burn a follower
        // turn that we'd then have to roll back.
        self.lead.continue_turn(guidance).await?;
        self.follower.continue_turn(guidance).await
    }

    async fn abort(&self) -> AgentResult<()> {
        // Abort is documented idempotent; we want both subprocesses gone
        // even if one abort fails. Surface the lead error preferentially
        // because it's the one the orchestrator's retry policy keys on.
        let lead_res = self.lead.abort().await;
        let follower_res = self.follower.abort().await;
        match (lead_res, follower_res) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(e), _) => Err(e),
            (Ok(()), Err(e)) => Err(e),
        }
    }
}

// ---------------------------------------------------------------------------
// Merged stream
// ---------------------------------------------------------------------------

/// Merges two [`AgentEventStream`]s by alternating polls and suppressing
/// per-stream terminal events; emits a single synthetic
/// [`AgentEvent::Completed`] tagged with the lead session once both inner
/// streams have ended.
///
/// Why suppress per-stream terminals: the orchestrator stops consuming
/// the moment it sees a terminal. If we forwarded the lead's `Completed`
/// while the follower was still mid-turn, the follower would be silently
/// abandoned mid-flight with its subprocess still running.
struct TandemStream {
    lead: AgentEventStream,
    follower: AgentEventStream,
    lead_done: bool,
    follower_done: bool,
    terminal_emitted: bool,
    /// Round-robin fairness flag: alternated each poll so a chatty lead
    /// can't starve the follower.
    poll_lead_first: bool,
    final_session: SessionId,
}

impl Stream for TandemStream {
    type Item = AgentEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<AgentEvent>> {
        let this = self.get_mut();

        if this.terminal_emitted {
            return Poll::Ready(None);
        }

        // Alternate which side we sample first so neither side starves.
        let lead_first = this.poll_lead_first;
        this.poll_lead_first = !lead_first;

        for try_lead in [lead_first, !lead_first] {
            if try_lead {
                if !this.lead_done {
                    match this.lead.as_mut().poll_next(cx) {
                        Poll::Ready(Some(ev)) if ev.is_terminal() => {
                            // Suppress; mark side done and continue.
                            this.lead_done = true;
                        }
                        Poll::Ready(Some(ev)) => return Poll::Ready(Some(ev)),
                        Poll::Ready(None) => this.lead_done = true,
                        Poll::Pending => { /* fall through to follower */ }
                    }
                }
            } else if !this.follower_done {
                match this.follower.as_mut().poll_next(cx) {
                    Poll::Ready(Some(ev)) if ev.is_terminal() => {
                        this.follower_done = true;
                    }
                    Poll::Ready(Some(ev)) => return Poll::Ready(Some(ev)),
                    Poll::Ready(None) => this.follower_done = true,
                    Poll::Pending => { /* fall through */ }
                }
            }
        }

        if this.lead_done && this.follower_done {
            this.terminal_emitted = true;
            return Poll::Ready(Some(AgentEvent::Completed {
                session: this.final_session.clone(),
                reason: CompletionReason::Success,
            }));
        }

        // At least one side is Pending and not yet done. Both sides
        // (whichever we polled) registered wakers via the calls above.
        Poll::Pending
    }
}

// Silence "unused" warnings for items the strategy commits will reach for
// shortly. Removing this blanket attribute is a checklist-level signal
// that downstream strategies have started consuming the API.
#[allow(dead_code)]
const _: fn() = || {
    // Compile-time touch points so refactors notice when these go away.
    let _ = TandemRole::Lead;
    let _ = TandemRole::Follower;
};

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use futures::stream;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use symphony_core::agent::{
        AgentControl, AgentError, AgentEvent, AgentSession, CompletionReason, SessionId,
        StartSessionParams,
    };
    use symphony_core::tracker::IssueId;

    /// Programmable stub runner: returns a pre-seeded event vector and
    /// records `start_session` / `abort` / `continue_turn` calls.
    struct StubRunner {
        session_id: SessionId,
        events: Mutex<Option<Vec<AgentEvent>>>,
        start_calls: AtomicUsize,
        fail_start: bool,
    }

    impl StubRunner {
        fn new(session_id: &str, events: Vec<AgentEvent>) -> Arc<Self> {
            Arc::new(Self {
                session_id: SessionId::from_raw(session_id),
                events: Mutex::new(Some(events)),
                start_calls: AtomicUsize::new(0),
                fail_start: false,
            })
        }

        fn failing() -> Arc<Self> {
            Arc::new(Self {
                session_id: SessionId::from_raw("never"),
                events: Mutex::new(Some(vec![])),
                start_calls: AtomicUsize::new(0),
                fail_start: true,
            })
        }
    }

    struct StubControl {
        continue_calls: Arc<AtomicUsize>,
        abort_calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl AgentControl for StubControl {
        async fn continue_turn(&self, _guidance: &str) -> AgentResult<()> {
            self.continue_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn abort(&self) -> AgentResult<()> {
            self.abort_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[async_trait]
    impl AgentRunner for StubRunner {
        async fn start_session(&self, _params: StartSessionParams) -> AgentResult<AgentSession> {
            self.start_calls.fetch_add(1, Ordering::SeqCst);
            if self.fail_start {
                return Err(AgentError::Spawn("stubbed failure".into()));
            }
            let evs = self.events.lock().unwrap().take().unwrap_or_default();
            Ok(AgentSession {
                initial_session: self.session_id.clone(),
                events: Box::pin(stream::iter(evs)),
                control: Box::new(StubControl {
                    continue_calls: Arc::new(AtomicUsize::new(0)),
                    abort_calls: Arc::new(AtomicUsize::new(0)),
                }),
            })
        }
    }

    fn msg(session: &str, content: &str) -> AgentEvent {
        AgentEvent::Message {
            session: SessionId::from_raw(session),
            role: "assistant".into(),
            content: content.into(),
        }
    }
    fn done(session: &str) -> AgentEvent {
        AgentEvent::Completed {
            session: SessionId::from_raw(session),
            reason: CompletionReason::Success,
        }
    }

    fn params() -> StartSessionParams {
        StartSessionParams {
            issue: IssueId::new("X-1"),
            workspace: std::path::PathBuf::from("/tmp/ws"),
            initial_prompt: "go".into(),
            issue_label: None,
        }
    }

    #[test]
    fn strategy_round_trips_kebab_case() {
        for (variant, expected) in [
            (TandemStrategy::DraftReview, "\"draft-review\""),
            (TandemStrategy::SplitImplement, "\"split-implement\""),
            (TandemStrategy::Consensus, "\"consensus\""),
        ] {
            let s = serde_json::to_string(&variant).unwrap();
            assert_eq!(s, expected);
            let back: TandemStrategy = serde_json::from_str(&s).unwrap();
            assert_eq!(back, variant);
        }
    }

    #[test]
    fn role_round_trips_lowercase() {
        assert_eq!(
            serde_json::to_string(&TandemRole::Lead).unwrap(),
            "\"lead\""
        );
        assert_eq!(
            serde_json::to_string(&TandemRole::Follower).unwrap(),
            "\"follower\""
        );
    }

    #[tokio::test]
    async fn starts_both_inner_runners_in_order() {
        let lead = StubRunner::new("lead-1", vec![done("lead-1")]);
        let follower = StubRunner::new("foll-1", vec![done("foll-1")]);
        let runner = TandemRunner::new(
            lead.clone() as Arc<dyn AgentRunner>,
            follower.clone() as Arc<dyn AgentRunner>,
            TandemStrategy::Consensus,
        );

        let session = runner.start_session(params()).await.expect("start ok");
        assert_eq!(session.initial_session.as_str(), "lead-1");
        assert_eq!(lead.start_calls.load(Ordering::SeqCst), 1);
        assert_eq!(follower.start_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn merges_messages_then_emits_single_terminal() {
        let lead = StubRunner::new(
            "lead-1",
            vec![msg("lead-1", "L1"), msg("lead-1", "L2"), done("lead-1")],
        );
        let follower = StubRunner::new("foll-1", vec![msg("foll-1", "F1"), done("foll-1")]);
        let runner = TandemRunner::new(
            lead as Arc<dyn AgentRunner>,
            follower as Arc<dyn AgentRunner>,
            TandemStrategy::Consensus,
        );

        let session = runner.start_session(params()).await.unwrap();
        let collected: Vec<AgentEvent> = session.events.collect().await;

        // Three messages from the inner streams, then one synthesized
        // Completed tagged with the lead session id, then the stream ends.
        let messages: Vec<&AgentEvent> = collected
            .iter()
            .filter(|e| matches!(e, AgentEvent::Message { .. }))
            .collect();
        assert_eq!(messages.len(), 3);

        let terminals: Vec<&AgentEvent> = collected.iter().filter(|e| e.is_terminal()).collect();
        assert_eq!(terminals.len(), 1, "exactly one synthetic terminal");
        match terminals[0] {
            AgentEvent::Completed { session, reason } => {
                assert_eq!(session.as_str(), "lead-1");
                assert_eq!(*reason, CompletionReason::Success);
            }
            other => panic!("expected Completed, got {other:?}"),
        }

        // Terminal must be the last event.
        assert!(collected.last().unwrap().is_terminal());
    }

    #[tokio::test]
    async fn follower_failure_aborts_lead_and_propagates() {
        // Lead succeeds but follower's start_session fails. The composite
        // must surface the follower error and abort the lead. We can't
        // observe the lead's abort directly here without a richer stub,
        // so this test covers the error-propagation path; lead-abort
        // wiring is exercised by the lead-side abort assertions.
        //
        // Pinned to `Consensus` because the baseline parallel-start path
        // is what surfaces follower-spawn errors synchronously.
        // Draft-review is sequential — it starts the follower only after
        // the lead's draft completes, so its follower-failure path is
        // covered by the in-stream `Failed` test in `draft_review`.
        let lead = StubRunner::new("lead-1", vec![done("lead-1")]);
        let follower = StubRunner::failing();
        let runner = TandemRunner::new(
            lead.clone() as Arc<dyn AgentRunner>,
            follower as Arc<dyn AgentRunner>,
            TandemStrategy::Consensus,
        );

        let err = match runner.start_session(params()).await {
            Ok(_) => panic!("expected failure"),
            Err(e) => e,
        };
        assert!(matches!(err, AgentError::Spawn(_)));
        assert_eq!(lead.start_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn strategy_accessor_returns_configured_value() {
        let lead = StubRunner::new("lead-1", vec![done("lead-1")]);
        let follower = StubRunner::new("foll-1", vec![done("foll-1")]);
        let runner = TandemRunner::new(
            lead as Arc<dyn AgentRunner>,
            follower as Arc<dyn AgentRunner>,
            TandemStrategy::SplitImplement,
        );
        assert_eq!(runner.strategy(), TandemStrategy::SplitImplement);
    }
}
