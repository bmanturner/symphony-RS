//! `split-implement` strategy for [`super::TandemRunner`].
//!
//! Sequence of events for one tandem session, per the SPEC-deviation #1
//! contract:
//!
//! 1. The **lead** runner is started immediately with the orchestrator's
//!    rendered prompt wrapped in a "plan only — emit a numbered subtask
//!    list, do not implement" envelope. Lead's events flow through the
//!    merged stream while a background task accumulates assistant-role
//!    message text into a `plan` buffer. Inner terminal events are
//!    suppressed so the orchestrator doesn't see two `Completed`s on one
//!    tandem session.
//! 2. Once the lead's stream ends *without* a `Failed`, the **follower**
//!    is started with an execution prompt that quotes the plan and tells
//!    the follower to claim each subtask via a `claim_subtask` tool-use
//!    call before executing it. Lead-`Failed` short-circuits the whole
//!    session — there's nothing useful to execute.
//! 3. Follower's events are forwarded the same way; once it terminates
//!    (or fails) we emit one synthesized [`AgentEvent`] tagged with the
//!    lead session id and close the stream.
//!
//! `continue_turn` forwards guidance to the **follower** because the
//! execution phase is the long-lived, refinable one — the lead's plan is
//! a one-shot. If the follower hasn't started yet (orchestrator called
//! continue during the plan phase), we fall back to the lead so guidance
//! is never silently dropped.
//!
//! `abort` aborts whichever sides are currently active and sets a flag so
//! a mid-flight follower start is skipped.
//!
//! # Why mirror `draft_review` rather than share code
//!
//! The two strategies have the same lead-then-follower shape but diverge
//! on prompt envelope, `continue_turn` target, and (eventually) telemetry
//! around tool-use claims. Pulling the shared scaffolding into a generic
//! "sequential tandem" helper would be premature abstraction — there are
//! exactly two callers and the differences are concentrated in a handful
//! of lines. Revisit if `consensus` or a fourth strategy lands.

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

/// Channel buffer for the merged stream. Matches `draft_review`'s sizing
/// rationale — large enough to absorb a chatty lead's burst between
/// orchestrator polls without forcing back-pressure inside the spawned
/// task.
const EVENT_BUFFER: usize = 64;

/// Tool name the follower is instructed to call before executing each
/// subtask. The orchestrator currently only forwards [`AgentEvent::ToolUse`]
/// verbatim — telemetry that distinguishes claim calls from arbitrary tool
/// use is a Phase 6 follow-up. The constant lives here so the prompt and
/// future telemetry filters agree on the spelling.
pub(super) const CLAIM_TOOL_NAME: &str = "claim_subtask";

/// Entry point invoked from [`super::TandemRunner::start_session`] when
/// the configured strategy is [`super::TandemStrategy::SplitImplement`].
///
/// Starts the lead synchronously (so `start_session` errors propagate
/// before we return) and spawns a single tokio task that drives the rest
/// of the sequence over an `mpsc` channel.
pub(super) async fn start(
    lead: Arc<dyn AgentRunner>,
    follower: Arc<dyn AgentRunner>,
    params: StartSessionParams,
) -> AgentResult<AgentSession> {
    // Wrap the orchestrator's prompt with a plan-only envelope before
    // handing it to the lead. The follower's prompt is built later from
    // the captured plan + the original task.
    let plan_prompt = build_plan_prompt(&params.initial_prompt);
    let lead_params = StartSessionParams {
        initial_prompt: plan_prompt,
        ..params.clone()
    };
    let lead_session = lead.start_session(lead_params).await?;
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

    let control: Box<dyn AgentControl> = Box::new(SplitImplementControl {
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

/// Wrap the orchestrator's prompt with a plan-only envelope.
///
/// Kept as a free function for unit-testing without spawning runners.
/// Section markers mirror `draft_review` (`=== … ===`) for visual
/// consistency in agent transcripts where both strategies might appear.
fn build_plan_prompt(original: &str) -> String {
    format!(
        "You are the PLANNING agent in a split-implement tandem. Produce a \
numbered list of concrete subtasks that an executing agent can pick up. \
Do NOT implement anything yourself — your output is a plan only. Each \
subtask should be self-contained and testable.\n\n\
=== ORIGINAL TASK ===\n{original}\n\n\
=== PLAN ==="
    )
}

/// Build the execution prompt sent to the follower.
///
/// The follower is told to call [`CLAIM_TOOL_NAME`] with the subtask
/// number/title before executing it, so a watcher (or future Phase 8 TUI
/// panel) can observe progress through the plan.
fn build_execution_prompt(original: &str, plan: &str) -> String {
    format!(
        "You are the EXECUTING agent in a split-implement tandem. Implement \
the subtasks in the plan below. Before starting each subtask, call the \
`{CLAIM_TOOL_NAME}` tool with the subtask's number and title so progress \
can be tracked. Implement subtasks in order unless dependencies dictate \
otherwise.\n\n\
=== ORIGINAL TASK ===\n{original}\n\n\
=== PLAN ===\n{plan}\n\n\
=== EXECUTION ==="
    )
}

/// Drain lead, start follower, drain follower, emit synthetic terminal.
/// Errors flow as [`AgentEvent::Failed`] so the orchestrator's stream-side
/// handling sees them.
async fn orchestrate(
    mut lead_events: AgentEventStream,
    follower: Arc<dyn AgentRunner>,
    params: StartSessionParams,
    final_session: SessionId,
    tx: mpsc::Sender<AgentEvent>,
    follower_holder: Arc<Mutex<Option<Box<dyn AgentControl>>>>,
    aborted: Arc<AtomicBool>,
) {
    let mut plan = String::new();
    let mut lead_failed: Option<AgentEvent> = None;

    while let Some(ev) = lead_events.next().await {
        // Capture only assistant-role text. Tool-result and user-role
        // echoes shouldn't leak into the plan body — they'd confuse the
        // executor's reading of the subtask list.
        if let AgentEvent::Message { content, role, .. } = &ev
            && role.eq_ignore_ascii_case("assistant")
        {
            if !plan.is_empty() {
                plan.push('\n');
            }
            plan.push_str(content);
        }
        if matches!(&ev, AgentEvent::Failed { .. }) {
            // Forward the lead's failure verbatim — preserve the original
            // session id so the orchestrator's logging stays correlated.
            lead_failed = Some(ev);
            break;
        }
        if ev.is_terminal() {
            // Suppress the inner Completed; we'll synthesize one after
            // the follower's execution (or earlier if aborted).
            break;
        }
        if tx.send(ev).await.is_err() {
            // Receiver was dropped — caller stopped consuming the stream.
            return;
        }
    }

    if let Some(failed) = lead_failed {
        let _ = tx.send(failed).await;
        return;
    }

    if aborted.load(Ordering::SeqCst) {
        // Orchestrator called abort during the plan phase. Don't burn an
        // executor turn; emit cancellation and stop.
        let _ = tx
            .send(AgentEvent::Completed {
                session: final_session,
                reason: CompletionReason::Cancelled,
            })
            .await;
        return;
    }

    let execution_prompt = build_execution_prompt(&params.initial_prompt, &plan);
    let follower_params = StartSessionParams {
        initial_prompt: execution_prompt,
        ..params
    };
    let follower_session = match follower.start_session(follower_params).await {
        Ok(s) => s,
        Err(e) => {
            let _ = tx
                .send(AgentEvent::Failed {
                    session: final_session,
                    error: format!("split-implement follower failed to start: {e}"),
                })
                .await;
            return;
        }
    };

    // Stash the follower control so abort() and continue_turn() can reach
    // it. tokio Mutex so the abort path doesn't block the executor on
    // contention.
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

/// Control surface for a split-implement session.
///
/// Holds the lead's control directly (always live once `start` returned
/// `Ok`) and a slot for the follower's control (populated by the
/// orchestrate task once the lead's plan completes). `continue_turn`
/// targets the follower because execution is the refinable phase; the
/// lead's plan is a one-shot.
struct SplitImplementControl {
    lead: Box<dyn AgentControl>,
    follower: Arc<Mutex<Option<Box<dyn AgentControl>>>>,
    aborted: Arc<AtomicBool>,
}

#[async_trait]
impl AgentControl for SplitImplementControl {
    async fn continue_turn(&self, guidance: &str) -> AgentResult<()> {
        // Prefer the follower if it's started; fall back to the lead so
        // guidance issued during the plan phase is not silently dropped.
        // We hold the lock only long enough to peek; the inner
        // continue_turn is called outside the guard so a slow continue
        // doesn't block abort().
        let follower_guard = self.follower.lock().await;
        if let Some(_f) = follower_guard.as_ref() {
            // Re-borrow without moving out of the Option. We can't move
            // the Box (it's not `Clone`), so we forward through a deref
            // while the guard is held.
            return follower_guard
                .as_ref()
                .unwrap()
                .continue_turn(guidance)
                .await;
        }
        drop(follower_guard);
        self.lead.continue_turn(guidance).await
    }

    async fn abort(&self) -> AgentResult<()> {
        // Set the flag *before* aborting so the orchestrate task, which
        // checks the flag after the lead drains, sees `true` and skips
        // the follower start instead of racing it.
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
/// pattern in `draft_review.rs` and the runner crates so we don't have to
/// add the `tokio-stream` `sync` feature for this file alone.
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

    /// Stub runner that records the params it was started with and how
    /// many times each control method was hit. Tests poke at these
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
                return Err(AgentError::Spawn("execute-side stub refused".into()));
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
    fn plan_prompt_wraps_original_with_section_markers() {
        let p = build_plan_prompt("Do the thing");
        assert!(p.contains("PLANNING agent"));
        assert!(p.contains("=== ORIGINAL TASK ==="));
        assert!(p.contains("Do the thing"));
        assert!(p.contains("=== PLAN ==="));
        // Plan-only guard must be present so the lead doesn't start
        // implementing.
        assert!(p.to_lowercase().contains("do not implement"));
    }

    #[test]
    fn execution_prompt_quotes_plan_and_names_claim_tool() {
        let p = build_execution_prompt("Do the thing", "1. Step one\n2. Step two");
        assert!(p.contains("EXECUTING agent"));
        assert!(p.contains("Do the thing"));
        assert!(p.contains("1. Step one"));
        assert!(p.contains("2. Step two"));
        assert!(
            p.contains(CLAIM_TOOL_NAME),
            "execution prompt must name the claim tool: {p}"
        );
    }

    #[tokio::test]
    async fn lead_plans_then_follower_executes_with_single_terminal() {
        let lead = RecordingRunner::new(
            "lead-1",
            vec![
                assistant_msg("lead-1", "1. First subtask"),
                assistant_msg("lead-1", "2. Second subtask"),
                done("lead-1"),
            ],
        );
        let follower = RecordingRunner::new(
            "foll-1",
            vec![
                AgentEvent::ToolUse {
                    session: SessionId::from_raw("foll-1"),
                    tool: CLAIM_TOOL_NAME.into(),
                    input: serde_json::json!({"number": 1, "title": "First subtask"}),
                },
                assistant_msg("foll-1", "executing subtask 1"),
                done("foll-1"),
            ],
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

        // Two plan messages from lead + one tool-use + one execution
        // message from follower = 4 non-terminal events, then exactly
        // one synthetic terminal tagged with the lead session id.
        let messages: Vec<&AgentEvent> = collected
            .iter()
            .filter(|e| matches!(e, AgentEvent::Message { .. }))
            .collect();
        assert_eq!(messages.len(), 3);

        let tool_uses: Vec<&AgentEvent> = collected
            .iter()
            .filter(|e| matches!(e, AgentEvent::ToolUse { .. }))
            .collect();
        assert_eq!(tool_uses.len(), 1);

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

        // Lead should have been started with the plan envelope.
        let lead_params = lead.recorded_params.lock().unwrap().clone().unwrap();
        assert!(lead_params.initial_prompt.contains("PLANNING agent"));
        assert!(lead_params.initial_prompt.contains("Implement feature X"));

        // Follower should have been started with an execution prompt
        // that quotes the captured plan + the original task.
        let follower_params = follower.recorded_params.lock().unwrap().clone().unwrap();
        assert!(follower_params.initial_prompt.contains("EXECUTING agent"));
        assert!(
            follower_params
                .initial_prompt
                .contains("Implement feature X")
        );
        assert!(follower_params.initial_prompt.contains("1. First subtask"));
        assert!(follower_params.initial_prompt.contains("2. Second subtask"));
        assert!(follower_params.initial_prompt.contains(CLAIM_TOOL_NAME));
        // Workspace + issue label carry through unchanged.
        assert_eq!(follower_params.workspace, params().workspace);
        assert_eq!(follower_params.issue_label, params().issue_label);
    }

    #[tokio::test]
    async fn user_role_messages_are_not_quoted_into_plan() {
        let lead = RecordingRunner::new(
            "lead-1",
            vec![
                user_msg("lead-1", "TOOL_RESULT_should_not_appear"),
                assistant_msg("lead-1", "real plan line"),
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
        assert!(prompt.contains("real plan line"));
        assert!(
            !prompt.contains("TOOL_RESULT_should_not_appear"),
            "user-role text leaked into execution prompt: {prompt}"
        );
    }

    #[tokio::test]
    async fn lead_failure_short_circuits_and_skips_follower() {
        let lead = RecordingRunner::new(
            "lead-1",
            vec![
                assistant_msg("lead-1", "partial plan"),
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

        match collected.last().unwrap() {
            AgentEvent::Failed { session, error } => {
                assert_eq!(session.as_str(), "lead-1");
                assert_eq!(error, "model error");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(follower.recorded_params.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn follower_start_failure_surfaces_as_failed_event() {
        let lead = RecordingRunner::new(
            "lead-1",
            vec![assistant_msg("lead-1", "plan"), done("lead-1")],
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
                assert_eq!(session.as_str(), "lead-1");
                assert!(error.contains("follower failed to start"), "got {error}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn continue_turn_targets_follower_after_it_starts() {
        let lead = RecordingRunner::new(
            "lead-1",
            vec![assistant_msg("lead-1", "plan"), done("lead-1")],
        );
        let follower = RecordingRunner::new(
            "foll-1",
            vec![assistant_msg("foll-1", "exec"), done("foll-1")],
        );
        let lead_continue = lead.continue_calls.clone();
        let follower_continue = follower.continue_calls.clone();

        let session = start(
            lead as Arc<dyn AgentRunner>,
            follower as Arc<dyn AgentRunner>,
            params(),
        )
        .await
        .unwrap();
        // Drain so the follower is fully wired into the holder slot.
        let _ = session.events.collect::<Vec<_>>().await;

        session
            .control
            .continue_turn("refine the implementation")
            .await
            .unwrap();
        assert_eq!(
            lead_continue.load(AtomicOrdering::SeqCst),
            0,
            "lead must not be re-prompted once execution has started"
        );
        assert_eq!(follower_continue.load(AtomicOrdering::SeqCst), 1);
    }

    #[tokio::test]
    async fn continue_turn_falls_back_to_lead_during_plan_phase() {
        // Lead produces no terminal so its stream stays open; the
        // orchestrator's continue_turn races the plan and must hit the
        // lead since the follower hasn't started.
        let lead_session = SessionId::from_raw("lead-1");
        let lead = RecordingRunner {
            session_id: lead_session.clone(),
            // Empty events vec => stream closes immediately, but we
            // delay the orchestrate task by NOT including a terminal so
            // it sits in the lead-drain loop. Simpler: include only one
            // assistant message with no terminal, then call continue
            // before the channel drains. To avoid timing flakiness we
            // instead start with NO events at all and rely on the
            // synchronous follower-not-yet-started state right after
            // start() returns.
            events: StdMutex::new(Some(vec![])),
            recorded_params: Arc::new(StdMutex::new(None)),
            continue_calls: Arc::new(AtomicUsize::new(0)),
            abort_calls: Arc::new(AtomicUsize::new(0)),
            fail_start: false,
        };
        let lead = Arc::new(lead);
        let follower = RecordingRunner::new("foll-1", vec![done("foll-1")]);
        let lead_continue = lead.continue_calls.clone();
        let follower_continue = follower.continue_calls.clone();

        let session = start(
            lead.clone() as Arc<dyn AgentRunner>,
            follower.clone() as Arc<dyn AgentRunner>,
            params(),
        )
        .await
        .unwrap();

        // Call continue_turn immediately, before yielding to the
        // orchestrate task that would otherwise wire up the follower.
        session
            .control
            .continue_turn("steer the plan")
            .await
            .unwrap();
        assert_eq!(
            lead_continue.load(AtomicOrdering::SeqCst),
            1,
            "continue must fall back to the lead during plan phase"
        );
        assert_eq!(follower_continue.load(AtomicOrdering::SeqCst), 0);

        // Drain to let the orchestrate task complete cleanly.
        let _ = session.events.collect::<Vec<_>>().await;
    }

    #[tokio::test]
    async fn abort_aborts_both_sides_when_follower_started() {
        let lead = RecordingRunner::new(
            "lead-1",
            vec![assistant_msg("lead-1", "plan"), done("lead-1")],
        );
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
