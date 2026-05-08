//! `consensus` strategy for [`super::TandemRunner`].
//!
//! Sequence of events for one tandem session, per the SPEC-deviation #1
//! contract ("both run, pick the output with greater test-pass delta"):
//!
//! 1. **Lead** and **follower** are started in parallel with the
//!    orchestrator's rendered prompt — they get the *same* task. A
//!    follower-launch failure aborts the already-started lead and
//!    propagates the error so the orchestrator's retry queue keys on a
//!    real spawn failure rather than a half-started session.
//! 2. Both event streams are drained concurrently into per-side buffers.
//!    Nothing is forwarded to the orchestrator yet — until both runners
//!    terminate we don't know whose transcript to keep, and forwarding
//!    interleaved partial events would force the watcher to demultiplex
//!    something we're going to throw half of away anyway.
//! 3. Once both sides terminate we score each transcript with the
//!    configured [`ConsensusScorer`]. The higher score wins; a tie
//!    breaks toward the lead (deterministic, and consistent with the
//!    other strategies' "lead is canonical" convention). A side that
//!    emitted [`AgentEvent::Failed`] forfeits regardless of score; if
//!    both failed, the lead's failure is propagated.
//! 4. The winner's buffered events (minus its inner terminal) are
//!    replayed through the merged stream in original order, followed by
//!    one synthesized [`AgentEvent::Completed`] tagged with the lead
//!    session id (or [`AgentEvent::Failed`] when both sides failed).
//!
//! Why we buffer instead of streaming: the orchestrator stops consuming
//! the moment it sees a terminal, and a status surface that rendered
//! both sides' interleaved messages would be misleading because half of
//! it is about to be discarded. Buffering trades wall-clock latency
//! (the user sees nothing until the slower side finishes) for a clean,
//! single-transcript view of "what consensus chose." A streaming variant
//! that emits both sides under a tandem-tagged variant is a future
//! refinement once an `AgentEvent` carries role/seat metadata.
//!
//! # `continue_turn` and `abort`
//!
//! Continue forwards to both inner controls — both sessions are still
//! alive until the orchestrator releases them, and we don't know yet
//! which transcript future continuations should extend. Abort fans out
//! to both and sets a flag so the scoring task exits with a cancelled
//! terminal instead of finishing the score-and-replay dance.

use async_trait::async_trait;
use futures::{Stream, StreamExt};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use tokio::sync::mpsc;

use symphony_core::agent::{
    AgentControl, AgentEvent, AgentEventStream, AgentResult, AgentRunner, AgentSession,
    CompletionReason, SessionId, StartSessionParams,
};

/// Channel buffer for the replayed stream. Smaller than the sequential
/// strategies' buffer because the replay phase is a tight loop over an
/// already-collected `Vec` and the orchestrator drains as fast as it can
/// — back-pressure here is cheap.
const EVENT_BUFFER: usize = 64;

/// Pluggable scoring function the orchestrator supplies to break ties
/// between the two consensus candidates.
///
/// The contract: given a side's full event transcript (excluding any
/// terminal event, which is consumed by the strategy), return an `i64`
/// "test-pass delta" or other quality measure where higher is better.
/// The strategy itself doesn't interpret the number — it only compares
/// the two sides' scores. Choosing the right metric is a deployment
/// concern; we ship a heuristic default and let `WORKFLOW.md`-driven
/// configuration override it.
///
/// Wrapped in `Arc` so callers can clone the scorer cheaply across
/// turns; `Send + Sync` so it crosses the spawned scoring task.
pub type ConsensusScorer = Arc<dyn Fn(&[AgentEvent]) -> i64 + Send + Sync>;

/// Default scorer used when the caller doesn't supply one.
///
/// Counts the number of assistant-role [`AgentEvent::Message`] and
/// [`AgentEvent::ToolUse`] events whose textual surface mentions a
/// passing test (`"passed"`, `"ok"` next to a test count, `"test result:
/// ok"`). The intent is a cheap, agent-agnostic proxy for "this side
/// reported more green tests" — it's not a substitute for actually
/// running `cargo test` against each candidate's worktree, which is the
/// next refinement once consensus has its own per-side workspace clone.
///
/// The heuristic is intentionally conservative: false positives merely
/// nudge the tie-break, and the deterministic lead-wins fallback covers
/// the common "neither side ran tests" case (both score 0).
pub fn default_consensus_scorer() -> ConsensusScorer {
    Arc::new(|events: &[AgentEvent]| -> i64 {
        let mut score: i64 = 0;
        for ev in events {
            match ev {
                AgentEvent::Message { content, .. } => {
                    let lower = content.to_ascii_lowercase();
                    // "test result: ok" is cargo's success line; "passed"
                    // catches pytest / jest / rspec / generic phrasing.
                    if lower.contains("test result: ok") {
                        score += 2;
                    }
                    // Each "passed" mention is worth one point. We don't
                    // try to parse counts because outputs vary wildly
                    // across test frameworks.
                    score += lower.matches("passed").count() as i64;
                }
                // A side that actually invoked a test runner gets a
                // small bump even before its output arrives — agents
                // that *try* to verify are a weak positive signal.
                AgentEvent::ToolUse { tool, .. }
                    if tool.to_ascii_lowercase().contains("test") =>
                {
                    score += 1;
                }
                _ => {}
            }
        }
        score
    })
}

/// Entry point invoked from [`super::TandemRunner::start_session`] when
/// the configured strategy is [`super::TandemStrategy::Consensus`].
///
/// Starts both inner runners synchronously (parallel: a follower spawn
/// failure aborts the lead and propagates) and spawns a single tokio
/// task that drains, scores, and replays.
pub(super) async fn start(
    lead: Arc<dyn AgentRunner>,
    follower: Arc<dyn AgentRunner>,
    params: StartSessionParams,
    scorer: ConsensusScorer,
) -> AgentResult<AgentSession> {
    // Lead first so a follower-launch failure has something to abort.
    // Mirrors the baseline merge path in `mod.rs`.
    let lead_session = lead.start_session(params.clone()).await?;
    let follower_session = match follower.start_session(params).await {
        Ok(s) => s,
        Err(e) => {
            let _ = lead_session.control.abort().await;
            return Err(e);
        }
    };

    let initial_session = lead_session.initial_session.clone();
    let aborted = Arc::new(AtomicBool::new(false));

    let (tx, rx) = mpsc::channel::<AgentEvent>(EVENT_BUFFER);

    tokio::spawn(orchestrate(
        lead_session.events,
        follower_session.events,
        initial_session.clone(),
        scorer,
        tx,
        aborted.clone(),
    ));

    let control: Box<dyn AgentControl> = Box::new(ConsensusControl {
        lead: lead_session.control,
        follower: follower_session.control,
        aborted,
    });

    Ok(AgentSession {
        initial_session,
        events: Box::pin(ReceiverStream { rx }),
        control,
    })
}

/// Drain both sides into buffers, score, replay the winner, terminate.
///
/// Errors flow as `AgentEvent::Failed` so the orchestrator's stream-side
/// handling sees them — same convention as `draft_review` and
/// `split_implement`.
async fn orchestrate(
    lead_events: AgentEventStream,
    follower_events: AgentEventStream,
    final_session: SessionId,
    scorer: ConsensusScorer,
    tx: mpsc::Sender<AgentEvent>,
    aborted: Arc<AtomicBool>,
) {
    // Drain both streams concurrently. We use `tokio::join!` so a slow
    // side can't block the fast side from making progress; the buffers
    // are independent.
    let drain = |mut stream: AgentEventStream| async move {
        let mut buf: Vec<AgentEvent> = Vec::new();
        let mut failure: Option<AgentEvent> = None;
        while let Some(ev) = stream.next().await {
            if matches!(&ev, AgentEvent::Failed { .. }) {
                failure = Some(ev);
                break;
            }
            if ev.is_terminal() {
                // Drop the inner Completed; the strategy emits its own.
                break;
            }
            buf.push(ev);
        }
        SideTranscript {
            events: buf,
            failure,
        }
    };

    let (lead_tx, follower_tx) = tokio::join!(drain(lead_events), drain(follower_events));

    if aborted.load(Ordering::SeqCst) {
        // Orchestrator pulled the rug. Don't replay — just emit a
        // cancelled terminal so the orchestrator's state machine moves
        // on cleanly.
        let _ = tx
            .send(AgentEvent::Completed {
                session: final_session,
                reason: CompletionReason::Cancelled,
            })
            .await;
        return;
    }

    // Failure handling: a side that emitted Failed forfeits. If both
    // failed, surface the lead's failure (stable correlation key).
    let winner = match (&lead_tx.failure, &follower_tx.failure) {
        (Some(lead_fail), Some(_)) => {
            let _ = tx.send(lead_fail.clone()).await;
            return;
        }
        (Some(_), None) => Side::Follower,
        (None, Some(_)) => Side::Lead,
        (None, None) => {
            let lead_score = (scorer)(&lead_tx.events);
            let follower_score = (scorer)(&follower_tx.events);
            // Tie breaks toward the lead — consistent with the other
            // strategies' "lead is canonical" convention.
            if follower_score > lead_score {
                Side::Follower
            } else {
                Side::Lead
            }
        }
    };

    let chosen = match winner {
        Side::Lead => &lead_tx.events,
        Side::Follower => &follower_tx.events,
    };
    for ev in chosen {
        if tx.send(ev.clone()).await.is_err() {
            return;
        }
    }

    let reason = if aborted.load(Ordering::SeqCst) {
        CompletionReason::Cancelled
    } else {
        CompletionReason::Success
    };
    let _ = tx
        .send(AgentEvent::Completed {
            session: final_session,
            reason,
        })
        .await;
}

/// Per-side drain result. `failure` is `Some` iff the side emitted a
/// terminal `Failed`; in that case `events` holds whatever non-terminal
/// events arrived before the failure (kept for diagnostics, but not
/// replayed because Failed is a forfeit).
struct SideTranscript {
    events: Vec<AgentEvent>,
    failure: Option<AgentEvent>,
}

/// Which side won consensus. Internal to this module.
enum Side {
    Lead,
    Follower,
}

/// Control surface for a consensus session. Forwards continue/abort to
/// both inner controls; abort additionally sets the cancellation flag.
struct ConsensusControl {
    lead: Box<dyn AgentControl>,
    follower: Box<dyn AgentControl>,
    aborted: Arc<AtomicBool>,
}

#[async_trait]
impl AgentControl for ConsensusControl {
    async fn continue_turn(&self, guidance: &str) -> AgentResult<()> {
        // Sequential rather than `tokio::join!` so an early refusal from
        // the lead doesn't waste a follower turn we'd then have to roll
        // back. Mirrors the baseline `TandemControl` reasoning.
        self.lead.continue_turn(guidance).await?;
        self.follower.continue_turn(guidance).await
    }

    async fn abort(&self) -> AgentResult<()> {
        // Set first so the scoring task observes the flag the moment it
        // wakes from `tokio::join!`, before it commits to a replay.
        self.aborted.store(true, Ordering::SeqCst);
        let lead_res = self.lead.abort().await;
        let follower_res = self.follower.abort().await;
        match (lead_res, follower_res) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(e), _) => Err(e),
            (Ok(()), Err(e)) => Err(e),
        }
    }
}

/// Hand-rolled `Stream` over an `mpsc::Receiver<AgentEvent>`. Mirrors
/// the pattern used in `draft_review` and `split_implement` so we don't
/// have to add the `tokio-stream` `sync` feature just for this file.
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
    use futures::stream;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use symphony_core::agent::{
        AgentControl, AgentError, AgentEvent, AgentSession, CompletionReason, SessionId,
        StartSessionParams,
    };
    use symphony_core::tracker::IssueId;

    struct StubRunner {
        session_id: SessionId,
        events: StdMutex<Option<Vec<AgentEvent>>>,
        continue_calls: Arc<AtomicUsize>,
        abort_calls: Arc<AtomicUsize>,
        fail_start: bool,
    }

    impl StubRunner {
        fn new(session_id: &str, events: Vec<AgentEvent>) -> Arc<Self> {
            Arc::new(Self {
                session_id: SessionId::from_raw(session_id),
                events: StdMutex::new(Some(events)),
                continue_calls: Arc::new(AtomicUsize::new(0)),
                abort_calls: Arc::new(AtomicUsize::new(0)),
                fail_start: false,
            })
        }
        fn failing() -> Arc<Self> {
            Arc::new(Self {
                session_id: SessionId::from_raw("never"),
                events: StdMutex::new(Some(vec![])),
                continue_calls: Arc::new(AtomicUsize::new(0)),
                abort_calls: Arc::new(AtomicUsize::new(0)),
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
            self.continue_calls.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(())
        }
        async fn abort(&self) -> AgentResult<()> {
            self.abort_calls.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(())
        }
    }

    #[async_trait]
    impl AgentRunner for StubRunner {
        async fn start_session(&self, _params: StartSessionParams) -> AgentResult<AgentSession> {
            if self.fail_start {
                return Err(AgentError::Spawn("consensus stub refused".into()));
            }
            let evs = self.events.lock().unwrap().take().unwrap_or_default();
            Ok(AgentSession {
                initial_session: self.session_id.clone(),
                events: Box::pin(stream::iter(evs)),
                control: Box::new(StubControl {
                    continue_calls: self.continue_calls.clone(),
                    abort_calls: self.abort_calls.clone(),
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
    fn failed(session: &str, err: &str) -> AgentEvent {
        AgentEvent::Failed {
            session: SessionId::from_raw(session),
            error: err.into(),
        }
    }
    fn tool_use(session: &str, tool: &str) -> AgentEvent {
        AgentEvent::ToolUse {
            session: SessionId::from_raw(session),
            tool: tool.into(),
            input: serde_json::Value::Null,
        }
    }

    fn params() -> StartSessionParams {
        StartSessionParams {
            issue: IssueId::new("X-1"),
            workspace: std::path::PathBuf::from("/tmp/ws"),
            initial_prompt: "do the task".into(),
            issue_label: None,
        }
    }

    #[test]
    fn default_scorer_rewards_test_pass_phrasing_and_test_tools() {
        let scorer = default_consensus_scorer();
        let no_signal = vec![msg("a", "I am thinking about it.")];
        let pass_signal = vec![msg(
            "a",
            "ran cargo test, test result: ok. 5 passed; 0 failed",
        )];
        let tool_signal = vec![tool_use("a", "run_tests"), msg("a", "looks good")];
        assert_eq!((scorer)(&no_signal), 0);
        // "test result: ok" => +2, "passed" => +1.
        assert_eq!((scorer)(&pass_signal), 3);
        // Tool name contains "test" => +1.
        assert_eq!((scorer)(&tool_signal), 1);
    }

    #[tokio::test]
    async fn higher_scoring_side_wins_and_replays_in_order() {
        // Lead emits filler; follower emits "passed" twice.
        let lead = StubRunner::new(
            "lead-1",
            vec![
                msg("lead-1", "thinking..."),
                msg("lead-1", "still thinking"),
                done("lead-1"),
            ],
        );
        let follower = StubRunner::new(
            "foll-1",
            vec![
                msg("foll-1", "step one passed"),
                msg("foll-1", "step two passed"),
                done("foll-1"),
            ],
        );
        let session = start(
            lead as Arc<dyn AgentRunner>,
            follower as Arc<dyn AgentRunner>,
            params(),
            default_consensus_scorer(),
        )
        .await
        .unwrap();
        // Synthesized terminal still tagged with the lead session id.
        assert_eq!(session.initial_session.as_str(), "lead-1");
        let collected: Vec<AgentEvent> = session.events.collect().await;

        // Follower's two messages replayed verbatim, in order, plus one
        // synthesized Completed at the end. No lead messages should leak.
        let messages: Vec<&AgentEvent> = collected
            .iter()
            .filter(|e| matches!(e, AgentEvent::Message { .. }))
            .collect();
        assert_eq!(messages.len(), 2);
        match messages[0] {
            AgentEvent::Message {
                session, content, ..
            } => {
                assert_eq!(session.as_str(), "foll-1");
                assert_eq!(content, "step one passed");
            }
            _ => unreachable!(),
        }
        match messages[1] {
            AgentEvent::Message { content, .. } => assert_eq!(content, "step two passed"),
            _ => unreachable!(),
        }
        // Single terminal at the end, tagged with lead session id.
        let terminals: Vec<&AgentEvent> = collected.iter().filter(|e| e.is_terminal()).collect();
        assert_eq!(terminals.len(), 1);
        match terminals[0] {
            AgentEvent::Completed { session, reason } => {
                assert_eq!(session.as_str(), "lead-1");
                assert_eq!(*reason, CompletionReason::Success);
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ties_break_toward_lead() {
        // Both sides get score 0 from the default scorer.
        let lead = StubRunner::new("lead-1", vec![msg("lead-1", "lead text"), done("lead-1")]);
        let follower = StubRunner::new(
            "foll-1",
            vec![msg("foll-1", "follower text"), done("foll-1")],
        );
        let session = start(
            lead as Arc<dyn AgentRunner>,
            follower as Arc<dyn AgentRunner>,
            params(),
            default_consensus_scorer(),
        )
        .await
        .unwrap();
        let collected: Vec<AgentEvent> = session.events.collect().await;
        let first_msg = collected
            .iter()
            .find_map(|e| match e {
                AgentEvent::Message {
                    content, session, ..
                } => Some((session.clone(), content.clone())),
                _ => None,
            })
            .unwrap();
        assert_eq!(first_msg.0.as_str(), "lead-1");
        assert_eq!(first_msg.1, "lead text");
    }

    #[tokio::test]
    async fn injected_scorer_overrides_default() {
        // Lead would lose under the default heuristic but our injected
        // scorer always picks the lead.
        let lead = StubRunner::new("lead-1", vec![msg("lead-1", "neutral"), done("lead-1")]);
        let follower = StubRunner::new(
            "foll-1",
            vec![
                msg("foll-1", "5 tests passed passed passed"),
                done("foll-1"),
            ],
        );
        let custom: ConsensusScorer = Arc::new(|_events: &[AgentEvent]| 0);
        let session = start(
            lead as Arc<dyn AgentRunner>,
            follower as Arc<dyn AgentRunner>,
            params(),
            custom,
        )
        .await
        .unwrap();
        let collected: Vec<AgentEvent> = session.events.collect().await;
        // Tie under custom scorer => lead wins.
        let first = collected
            .iter()
            .find_map(|e| match e {
                AgentEvent::Message {
                    session, content, ..
                } => Some((session.clone(), content.clone())),
                _ => None,
            })
            .unwrap();
        assert_eq!(first.0.as_str(), "lead-1");
        assert_eq!(first.1, "neutral");
    }

    #[tokio::test]
    async fn failed_side_forfeits_to_other() {
        let lead = StubRunner::new(
            "lead-1",
            vec![msg("lead-1", "partial"), failed("lead-1", "boom")],
        );
        let follower = StubRunner::new("foll-1", vec![msg("foll-1", "all good"), done("foll-1")]);
        let session = start(
            lead as Arc<dyn AgentRunner>,
            follower as Arc<dyn AgentRunner>,
            params(),
            default_consensus_scorer(),
        )
        .await
        .unwrap();
        let collected: Vec<AgentEvent> = session.events.collect().await;

        // Follower's transcript wins despite scoring zero — the lead
        // failed and forfeits.
        let messages: Vec<&AgentEvent> = collected
            .iter()
            .filter(|e| matches!(e, AgentEvent::Message { .. }))
            .collect();
        assert_eq!(messages.len(), 1);
        match messages[0] {
            AgentEvent::Message {
                session, content, ..
            } => {
                assert_eq!(session.as_str(), "foll-1");
                assert_eq!(content, "all good");
            }
            _ => unreachable!(),
        }
        match collected.last().unwrap() {
            AgentEvent::Completed { reason, .. } => {
                assert_eq!(*reason, CompletionReason::Success);
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn both_failed_propagates_lead_failure_verbatim() {
        let lead = StubRunner::new("lead-1", vec![failed("lead-1", "lead boom")]);
        let follower = StubRunner::new("foll-1", vec![failed("foll-1", "foll boom")]);
        let session = start(
            lead as Arc<dyn AgentRunner>,
            follower as Arc<dyn AgentRunner>,
            params(),
            default_consensus_scorer(),
        )
        .await
        .unwrap();
        let collected: Vec<AgentEvent> = session.events.collect().await;
        match collected.last().unwrap() {
            AgentEvent::Failed { session, error } => {
                assert_eq!(session.as_str(), "lead-1");
                assert_eq!(error, "lead boom");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn follower_start_failure_aborts_lead_and_propagates() {
        let lead = StubRunner::new("lead-1", vec![done("lead-1")]);
        let lead_aborts = lead.abort_calls.clone();
        let follower = StubRunner::failing();
        let err = start(
            lead as Arc<dyn AgentRunner>,
            follower as Arc<dyn AgentRunner>,
            params(),
            default_consensus_scorer(),
        )
        .await;
        assert!(matches!(err, Err(AgentError::Spawn(_))));
        // Lead's abort must have been called as cleanup.
        assert_eq!(lead_aborts.load(AtomicOrdering::SeqCst), 1);
    }

    #[tokio::test]
    async fn continue_turn_forwards_to_both() {
        let lead = StubRunner::new("lead-1", vec![done("lead-1")]);
        let follower = StubRunner::new("foll-1", vec![done("foll-1")]);
        let lead_continue = lead.continue_calls.clone();
        let follower_continue = follower.continue_calls.clone();
        let session = start(
            lead as Arc<dyn AgentRunner>,
            follower as Arc<dyn AgentRunner>,
            params(),
            default_consensus_scorer(),
        )
        .await
        .unwrap();
        // Drain so both sides finish before we exercise control.
        let _ = session.events.collect::<Vec<_>>().await;
        session.control.continue_turn("more").await.unwrap();
        assert_eq!(lead_continue.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(follower_continue.load(AtomicOrdering::SeqCst), 1);
    }

    #[tokio::test]
    async fn abort_aborts_both_and_emits_cancelled_terminal() {
        // Use never-ending streams so the orchestrate task is still
        // mid-drain when we call abort.
        let lead = StubRunner::new(
            "lead-1",
            vec![msg("lead-1", "tick"), msg("lead-1", "tick"), done("lead-1")],
        );
        let follower = StubRunner::new(
            "foll-1",
            vec![msg("foll-1", "tick"), msg("foll-1", "tick"), done("foll-1")],
        );
        let lead_abort = lead.abort_calls.clone();
        let follower_abort = follower.abort_calls.clone();
        let session = start(
            lead as Arc<dyn AgentRunner>,
            follower as Arc<dyn AgentRunner>,
            params(),
            default_consensus_scorer(),
        )
        .await
        .unwrap();
        session.control.abort().await.unwrap();
        // Drain — orchestrate task observes the abort flag and emits
        // a cancelled terminal regardless of replay.
        let collected: Vec<AgentEvent> = session.events.collect().await;
        assert_eq!(lead_abort.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(follower_abort.load(AtomicOrdering::SeqCst), 1);
        match collected.last().unwrap() {
            AgentEvent::Completed { reason, .. } => {
                assert_eq!(*reason, CompletionReason::Cancelled);
            }
            other => panic!("expected cancelled Completed, got {other:?}"),
        }
    }
}
