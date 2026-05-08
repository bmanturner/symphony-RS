//! `draft-review` strategy for [`super::TandemRunner`].
//!
//! Sequence of events for one tandem session, per the SPEC-deviation #1
//! contract:
//!
//! 1. The **lead** runner is started immediately with the orchestrator's
//!    rendered prompt. Lead's events flow through the merged stream
//!    while a background task accumulates assistant-role message text
//!    into a `draft` buffer. Inner terminal events are suppressed so the
//!    orchestrator doesn't see two `Completed`s on one tandem session.
//! 2. Once the lead's stream ends *without* a `Failed`, the **follower**
//!    is started with a synthesized review prompt that quotes both the
//!    original task and the captured draft. Lead-`Failed` short-circuits
//!    the whole session — there's nothing useful to review.
//! 3. Follower's events are forwarded the same way; once it terminates
//!    (or fails) we emit one synthesized [`AgentEvent`] tagged with the
//!    lead session id and close the stream.
//!
//! `continue_turn` forwards guidance to the **lead only** because the
//! SPEC-deviation contract says the orchestrator either accepts the
//! draft or "runs a second turn with the merged feedback" — that second
//! turn is a re-draft, not a re-review. `abort` aborts whichever sides
//! are currently active and sets a flag so a mid-flight follower start
//! is skipped.
//!
//! # Why a spawned task and channel rather than the baseline
//! [`super::TandemStream`] poll machine
//!
//! The baseline runs both inner streams concurrently and merges. Draft-
//! review is sequential — follower can't start until the lead's draft is
//! complete — so a custom poll machine would have to keep an
//! "awaiting_follower_start" state plus a boxed future for the start
//! call. Using `tokio::spawn` + `mpsc` keeps the orchestration as
//! straight-line `async` code at the cost of one extra task. That's a
//! good trade for readability when the merge logic is non-trivial.

use async_trait::async_trait;
use futures::{Stream, StreamExt};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use tokio::sync::{Mutex, mpsc};

use symphony_core::agent::{
    AgentControl, AgentEvent, AgentEventStream, AgentResult, AgentRunner, AgentSession,
    CompletionReason, SessionId, StartSessionParams,
};

/// Channel buffer for the merged stream. Big enough to absorb a chatty
/// lead's burst of messages between orchestrator polls without forcing
/// the spawned task to back-pressure on send. Sized empirically against
/// the existing CodexRunner / ClaudeRunner emission rates.
const EVENT_BUFFER: usize = 64;

/// Entry point invoked from [`super::TandemRunner::start_session`] when
/// the configured strategy is [`super::TandemStrategy::DraftReview`].
///
/// Starts the lead synchronously (so `start_session` errors propagate
/// before we return) and spawns a single tokio task that drives the
/// rest of the sequence over an `mpsc` channel.
pub(super) async fn start(
    lead: Arc<dyn AgentRunner>,
    follower: Arc<dyn AgentRunner>,
    params: StartSessionParams,
) -> AgentResult<AgentSession> {
    // Start the lead first. A spawn failure here is the orchestrator's
    // problem (no stream gets created) — return synchronously so the
    // retry queue can key on it.
    let lead_session = lead.start_session(params.clone()).await?;
    let initial_session = lead_session.initial_session.clone();

    let (tx, rx) = mpsc::channel::<AgentEvent>(EVENT_BUFFER);
    let follower_holder: Arc<Mutex<Option<Box<dyn AgentControl>>>> = Arc::new(Mutex::new(None));
    let aborted = Arc::new(AtomicBool::new(false));

    tokio::spawn(orchestrate(
        lead_session.events,
        follower,
        params,
        initial_session.clone(),
        tx,
        follower_holder.clone(),
        aborted.clone(),
    ));

    let control: Box<dyn AgentControl> = Box::new(DraftReviewControl {
        lead: lead_session.control,
        follower: follower_holder,
        aborted,
    });

    Ok(AgentSession {
        initial_session,
        events: Box::pin(ReceiverStream { rx }),
        control,
    })
}

/// Build the review prompt sent to the follower.
///
/// Kept as a free function rather than a method on the strategy enum so
/// it can be unit-tested without spawning runners. The format is
/// deliberately plain text — both Codex and Claude accept multi-section
/// markdown-style prompts and we don't want to assume one frontend's
/// templating rules.
fn build_review_prompt(original: &str, draft: &str) -> String {
    // Section markers (`=== … ===`) are agent-agnostic and stable enough
    // for prompt-tuning experiments. If the draft is empty (lead emitted
    // no assistant messages) we still send a review prompt so the
    // follower's session id is a real id rather than a synthetic one;
    // the follower will typically respond with "no draft to review."
    format!(
        "You are reviewing a draft response to the task below. Identify \
issues with correctness, missing edge cases, untested assumptions, and \
any hallucinations. Be concrete and concise.\n\n\
=== ORIGINAL TASK ===\n{original}\n\n\
=== DRAFT ===\n{draft}\n\n\
=== REVIEW ==="
    )
}

/// The actual sequencing task: drain lead, start follower, drain follower,
/// emit synthetic terminal. Errors flow as `AgentEvent::Failed` so the
/// orchestrator's stream-side handling sees them.
async fn orchestrate(
    mut lead_events: AgentEventStream,
    follower: Arc<dyn AgentRunner>,
    params: StartSessionParams,
    final_session: SessionId,
    tx: mpsc::Sender<AgentEvent>,
    follower_holder: Arc<Mutex<Option<Box<dyn AgentControl>>>>,
    aborted: Arc<AtomicBool>,
) {
    let mut draft = String::new();
    let mut lead_failed: Option<AgentEvent> = None;

    while let Some(ev) = lead_events.next().await {
        // Capture only assistant-role text. Tool-result messages and
        // user-role echoes shouldn't leak into the draft body — they'd
        // confuse the follower's review.
        if let AgentEvent::Message { content, role, .. } = &ev
            && role.eq_ignore_ascii_case("assistant")
        {
            if !draft.is_empty() {
                draft.push('\n');
            }
            draft.push_str(content);
        }
        if matches!(&ev, AgentEvent::Failed { .. }) {
            // Forward the lead's failure verbatim — keeps the original
            // session id so the orchestrator's logging stays correlated.
            lead_failed = Some(ev);
            break;
        }
        if ev.is_terminal() {
            // Suppress the inner Completed; we'll synthesize one after
            // the follower's review (or earlier if aborted).
            break;
        }
        if tx.send(ev).await.is_err() {
            // Receiver was dropped — caller stopped consuming the stream.
            // Nothing useful to do; bail.
            return;
        }
    }

    if let Some(failed) = lead_failed {
        let _ = tx.send(failed).await;
        return;
    }

    if aborted.load(Ordering::SeqCst) {
        // Orchestrator called abort during the draft phase. Don't burn
        // a follower turn; emit the cancellation completion and stop.
        let _ = tx
            .send(AgentEvent::Completed {
                session: final_session,
                reason: CompletionReason::Cancelled,
            })
            .await;
        return;
    }

    // Build the review prompt and start the follower with the SAME
    // workspace + issue label so logs stay attributable to one issue.
    let review_prompt = build_review_prompt(&params.initial_prompt, &draft);
    let follower_params = StartSessionParams {
        initial_prompt: review_prompt,
        ..params
    };
    let follower_session = match follower.start_session(follower_params).await {
        Ok(s) => s,
        Err(e) => {
            let _ = tx
                .send(AgentEvent::Failed {
                    session: final_session,
                    error: format!("draft-review follower failed to start: {e}"),
                })
                .await;
            return;
        }
    };

    // Stash the follower control so abort() can reach it. Stored under a
    // tokio Mutex so the abort path's `.lock().await` doesn't block the
    // current task's executor thread on contention.
    *follower_holder.lock().await = Some(follower_session.control);

    let mut follower_events = follower_session.events;
    let mut follower_failed: Option<AgentEvent> = None;
    while let Some(ev) = follower_events.next().await {
        if matches!(&ev, AgentEvent::Failed { .. }) {
            follower_failed = Some(ev);
            break;
        }
        if ev.is_terminal() {
            break;
        }
        if tx.send(ev).await.is_err() {
            return;
        }
    }

    if let Some(failed) = follower_failed {
        let _ = tx.send(failed).await;
        return;
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

/// Control surface for a draft-review session.
///
/// Holds the lead's control directly (always live once `start` returned
/// `Ok`) and a slot for the follower's control (populated by the
/// orchestrate task once the lead's draft completes).
struct DraftReviewControl {
    lead: Box<dyn AgentControl>,
    follower: Arc<Mutex<Option<Box<dyn AgentControl>>>>,
    aborted: Arc<AtomicBool>,
}

#[async_trait]
impl AgentControl for DraftReviewControl {
    async fn continue_turn(&self, guidance: &str) -> AgentResult<()> {
        // Continuation in draft-review is a re-draft on the lead. The
        // follower is review-only; a fresh review will be initiated by
        // the next continuation cycle's draft phase, not by re-prompting
        // the prior follower turn.
        self.lead.continue_turn(guidance).await
    }

    async fn abort(&self) -> AgentResult<()> {
        // Set the flag *before* aborting so the orchestrate task, which
        // checks the flag after lead drains, sees `true` and skips the
        // follower start instead of racing it.
        self.aborted.store(true, Ordering::SeqCst);

        let lead_res = self.lead.abort().await;
        // Only abort the follower if it actually started. We `take` so a
        // second abort call doesn't double-abort.
        let follower_opt = self.follower.lock().await.take();
        let follower_res = match follower_opt {
            Some(f) => f.abort().await,
            None => Ok(()),
        };
        match (lead_res, follower_res) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(e), _) => Err(e),
            (Ok(()), Err(e)) => Err(e),
        }
    }
}

/// Hand-rolled `Stream` over an `mpsc::Receiver<AgentEvent>`. Mirrors the
/// pattern in `claude/runner.rs` and `codex/runner.rs` so we don't have
/// to add the `tokio-stream` `sync` feature just for this file.
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

    /// Stub runner that records the params it was started with and the
    /// number of times each control method was hit. Tests poke at these
    /// counters to assert ordering and prompt construction.
    struct RecordingRunner {
        session_id: SessionId,
        events: StdMutex<Option<Vec<AgentEvent>>>,
        recorded_params: Arc<StdMutex<Option<StartSessionParams>>>,
        continue_calls: Arc<AtomicUsize>,
        abort_calls: Arc<AtomicUsize>,
        fail_start: bool,
    }

    impl RecordingRunner {
        fn new(session_id: &str, events: Vec<AgentEvent>) -> Arc<Self> {
            Arc::new(Self {
                session_id: SessionId::from_raw(session_id),
                events: StdMutex::new(Some(events)),
                recorded_params: Arc::new(StdMutex::new(None)),
                continue_calls: Arc::new(AtomicUsize::new(0)),
                abort_calls: Arc::new(AtomicUsize::new(0)),
                fail_start: false,
            })
        }

        fn failing() -> Arc<Self> {
            Arc::new(Self {
                session_id: SessionId::from_raw("never"),
                events: StdMutex::new(Some(vec![])),
                recorded_params: Arc::new(StdMutex::new(None)),
                continue_calls: Arc::new(AtomicUsize::new(0)),
                abort_calls: Arc::new(AtomicUsize::new(0)),
                fail_start: true,
            })
        }
    }

    struct RecordingControl {
        continue_calls: Arc<AtomicUsize>,
        abort_calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl AgentControl for RecordingControl {
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
    impl AgentRunner for RecordingRunner {
        async fn start_session(&self, params: StartSessionParams) -> AgentResult<AgentSession> {
            *self.recorded_params.lock().unwrap() = Some(params);
            if self.fail_start {
                return Err(AgentError::Spawn("review-side stub refused".into()));
            }
            let evs = self.events.lock().unwrap().take().unwrap_or_default();
            Ok(AgentSession {
                initial_session: self.session_id.clone(),
                events: Box::pin(stream::iter(evs)),
                control: Box::new(RecordingControl {
                    continue_calls: self.continue_calls.clone(),
                    abort_calls: self.abort_calls.clone(),
                }),
            })
        }
    }

    fn assistant_msg(session: &str, content: &str) -> AgentEvent {
        AgentEvent::Message {
            session: SessionId::from_raw(session),
            role: "assistant".into(),
            content: content.into(),
        }
    }
    fn user_msg(session: &str, content: &str) -> AgentEvent {
        AgentEvent::Message {
            session: SessionId::from_raw(session),
            role: "user".into(),
            content: content.into(),
        }
    }
    fn done(session: &str) -> AgentEvent {
        AgentEvent::Completed {
            session: SessionId::from_raw(session),
            reason: CompletionReason::Success,
        }
    }
    fn failed(session: &str, msg: &str) -> AgentEvent {
        AgentEvent::Failed {
            session: SessionId::from_raw(session),
            error: msg.into(),
        }
    }

    fn params() -> StartSessionParams {
        StartSessionParams {
            issue: IssueId::new("X-1"),
            workspace: std::path::PathBuf::from("/tmp/ws"),
            initial_prompt: "Implement feature X".into(),
            issue_label: Some("X-1: feature X".into()),
        }
    }

    #[test]
    fn review_prompt_quotes_task_and_draft_with_section_markers() {
        let p = build_review_prompt("Do the thing", "Here is my draft.");
        assert!(p.contains("=== ORIGINAL TASK ==="));
        assert!(p.contains("Do the thing"));
        assert!(p.contains("=== DRAFT ==="));
        assert!(p.contains("Here is my draft."));
        assert!(p.contains("=== REVIEW ==="));
    }

    #[tokio::test]
    async fn lead_drafts_then_follower_reviews_with_single_terminal() {
        let lead = RecordingRunner::new(
            "lead-1",
            vec![
                assistant_msg("lead-1", "draft line A"),
                assistant_msg("lead-1", "draft line B"),
                done("lead-1"),
            ],
        );
        let follower = RecordingRunner::new(
            "foll-1",
            vec![assistant_msg("foll-1", "review note"), done("foll-1")],
        );

        let session = start(
            lead.clone() as Arc<dyn AgentRunner>,
            follower.clone() as Arc<dyn AgentRunner>,
            params(),
        )
        .await
        .expect("start ok");

        assert_eq!(session.initial_session.as_str(), "lead-1");
        let collected: Vec<AgentEvent> = session.events.collect().await;

        // Two lead drafts + one follower review = 3 messages, then a
        // single synthetic terminal tagged with the lead session id.
        let messages: Vec<&AgentEvent> = collected
            .iter()
            .filter(|e| matches!(e, AgentEvent::Message { .. }))
            .collect();
        assert_eq!(messages.len(), 3);

        let terminals: Vec<&AgentEvent> = collected.iter().filter(|e| e.is_terminal()).collect();
        assert_eq!(terminals.len(), 1);
        match terminals[0] {
            AgentEvent::Completed { session, reason } => {
                assert_eq!(session.as_str(), "lead-1");
                assert_eq!(*reason, CompletionReason::Success);
            }
            other => panic!("expected Completed, got {other:?}"),
        }
        assert!(collected.last().unwrap().is_terminal());

        // Follower should have been started with a prompt that contains
        // the captured draft lines and the original task.
        let follower_params = follower.recorded_params.lock().unwrap().clone().unwrap();
        assert!(
            follower_params
                .initial_prompt
                .contains("Implement feature X")
        );
        assert!(follower_params.initial_prompt.contains("draft line A"));
        assert!(follower_params.initial_prompt.contains("draft line B"));
        // Workspace + issue label carry through unchanged.
        assert_eq!(follower_params.workspace, params().workspace);
        assert_eq!(follower_params.issue_label, params().issue_label);
    }

    #[tokio::test]
    async fn user_role_messages_are_not_quoted_into_draft() {
        let lead = RecordingRunner::new(
            "lead-1",
            vec![
                user_msg("lead-1", "TOOL_RESULT_should_not_appear"),
                assistant_msg("lead-1", "real draft"),
                done("lead-1"),
            ],
        );
        let follower = RecordingRunner::new("foll-1", vec![done("foll-1")]);

        let session = start(
            lead as Arc<dyn AgentRunner>,
            follower.clone() as Arc<dyn AgentRunner>,
            params(),
        )
        .await
        .unwrap();
        let _ = session.events.collect::<Vec<_>>().await;

        let prompt = follower
            .recorded_params
            .lock()
            .unwrap()
            .clone()
            .unwrap()
            .initial_prompt;
        assert!(prompt.contains("real draft"));
        assert!(
            !prompt.contains("TOOL_RESULT_should_not_appear"),
            "user-role text leaked into review prompt: {prompt}"
        );
    }

    #[tokio::test]
    async fn lead_failure_short_circuits_and_skips_follower() {
        let lead = RecordingRunner::new(
            "lead-1",
            vec![
                assistant_msg("lead-1", "partial draft"),
                failed("lead-1", "model error"),
            ],
        );
        let follower = RecordingRunner::new("foll-1", vec![done("foll-1")]);

        let session = start(
            lead as Arc<dyn AgentRunner>,
            follower.clone() as Arc<dyn AgentRunner>,
            params(),
        )
        .await
        .unwrap();
        let collected: Vec<AgentEvent> = session.events.collect().await;

        // Last event must be the lead's Failed, propagated verbatim.
        match collected.last().unwrap() {
            AgentEvent::Failed { session, error } => {
                assert_eq!(session.as_str(), "lead-1");
                assert_eq!(error, "model error");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        // Follower's start_session must not have been called.
        assert!(follower.recorded_params.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn follower_start_failure_surfaces_as_failed_event() {
        let lead = RecordingRunner::new(
            "lead-1",
            vec![assistant_msg("lead-1", "draft"), done("lead-1")],
        );
        let follower = RecordingRunner::failing();

        let session = start(
            lead as Arc<dyn AgentRunner>,
            follower as Arc<dyn AgentRunner>,
            params(),
        )
        .await
        .unwrap();
        let collected: Vec<AgentEvent> = session.events.collect().await;

        match collected.last().unwrap() {
            AgentEvent::Failed { session, error } => {
                // Synthesized failure carries the lead session id (stable
                // correlation key for the orchestrator).
                assert_eq!(session.as_str(), "lead-1");
                assert!(error.contains("follower failed to start"), "got {error}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn continue_turn_forwards_to_lead_only() {
        let lead =
            RecordingRunner::new("lead-1", vec![assistant_msg("lead-1", "x"), done("lead-1")]);
        let follower = RecordingRunner::new("foll-1", vec![done("foll-1")]);
        let lead_continue = lead.continue_calls.clone();
        let follower_continue = follower.continue_calls.clone();

        let session = start(
            lead as Arc<dyn AgentRunner>,
            follower as Arc<dyn AgentRunner>,
            params(),
        )
        .await
        .unwrap();
        // Drain to ensure both sides have completed their work before we
        // poke at controls.
        let _ = session.events.collect::<Vec<_>>().await;

        session
            .control
            .continue_turn("merged feedback")
            .await
            .unwrap();
        assert_eq!(lead_continue.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(
            follower_continue.load(AtomicOrdering::SeqCst),
            0,
            "follower must not be re-prompted on continue"
        );
    }

    #[tokio::test]
    async fn abort_aborts_both_sides_when_follower_started() {
        let lead =
            RecordingRunner::new("lead-1", vec![assistant_msg("lead-1", "x"), done("lead-1")]);
        let follower = RecordingRunner::new("foll-1", vec![done("foll-1")]);
        let lead_abort = lead.abort_calls.clone();
        let follower_abort = follower.abort_calls.clone();

        let session = start(
            lead as Arc<dyn AgentRunner>,
            follower as Arc<dyn AgentRunner>,
            params(),
        )
        .await
        .unwrap();
        // Drain so the follower's control is in the holder slot.
        let _ = session.events.collect::<Vec<_>>().await;

        session.control.abort().await.unwrap();
        assert_eq!(lead_abort.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(follower_abort.load(AtomicOrdering::SeqCst), 1);
    }
}
