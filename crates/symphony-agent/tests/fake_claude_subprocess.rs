//! Subprocess-level tests for [`ClaudeRunner`] driven by a fake bash
//! "claude" CLI that emits a deterministic stream-json sequence.
//!
//! Claude's `-p` mode is one subprocess **per turn**: the runner spawns
//! a fresh child with `--session-id <uuid>` for the first turn and
//! `--resume <uuid>` for each continuation. Our fake binary keys off
//! the `--resume` argv to vary its output across turns so we can prove
//! the runner correctly stitches per-turn subprocesses into one
//! logical session with a stable [`ThreadId`] and a monotonic
//! [`TurnId`].

#![cfg(unix)]

mod common;

use common::{drain_until_closed, next_event, write_script};
use std::time::Duration;
use symphony_agent::claude::{ClaudeConfig, ClaudeRunner};
use symphony_core::agent::{AgentEvent, AgentRunner, CompletionReason, StartSessionParams};
use symphony_core::tracker::IssueId;
use tempfile::TempDir;

/// Fake `claude -p` binary.
///
/// Detects a continuation turn by scanning argv for `--resume`. Emits
/// `system` (silent), `assistant` (→ `Message` + `TokenUsage`), then
/// `result` (→ `TokenUsage` + `Completed`). The runner synthesises
/// `Started` itself before this script even runs, so we don't need to
/// emit anything for that.
const FAKE_CLAUDE_SCRIPT: &str = r#"#!/usr/bin/env bash
set -u
turn=1
for arg in "$@"; do
  if [ "$arg" = "--resume" ]; then
    turn=2
  fi
done
sid="fake-claude-uuid"
printf '{"type":"system","subtype":"init","session_id":"%s","model":"fake","cwd":"/x"}\n' "$sid"
printf '{"type":"assistant","session_id":"%s","message":{"id":"m%s","content":[{"type":"text","text":"hello %s"}],"usage":{"input_tokens":10,"output_tokens":3}}}\n' "$sid" "$turn" "$turn"
printf '{"type":"result","subtype":"success","session_id":"%s","is_error":false,"usage":{"input_tokens":10,"output_tokens":3}}\n' "$sid"
"#;

/// Long-running fake: emits one event then sleeps so we can exercise
/// abort against a still-live child. Without this the one-shot script
/// would exit before abort had anything to kill.
const FAKE_CLAUDE_HANG_SCRIPT: &str = r#"#!/usr/bin/env bash
sid="fake-claude-uuid"
printf '{"type":"system","subtype":"init","session_id":"%s","model":"fake","cwd":"/x"}\n' "$sid"
# `exec` replaces the bash shell with `sleep` so killing the spawned
# child actually closes the stdout pipe. Without this, killing bash
# leaves `sleep` orphaned (still holding the write end of the pipe) and
# the runner's reader never sees EOF.
exec sleep 60
"#;

fn fixture(script_body: &str) -> (ClaudeRunner, TempDir) {
    let tmp = TempDir::new().expect("tempdir");
    let script = write_script(tmp.path(), "fake_claude.sh", script_body);
    let cfg = ClaudeConfig {
        command: script.to_string_lossy().into_owned(),
        // verbose=false trims the argv slightly so the fake script's
        // arg-scan is faster, and it's irrelevant to our fake.
        verbose: true,
        ..Default::default()
    };
    (ClaudeRunner::new(cfg), tmp)
}

fn params(workspace: std::path::PathBuf) -> StartSessionParams {
    StartSessionParams {
        issue: IssueId::new("X-1"),
        workspace,
        initial_prompt: "do the thing".into(),
        issue_label: Some("X-1: do the thing".into()),
    }
}

#[tokio::test]
async fn first_turn_yields_started_message_usage_completed() {
    let (runner, tmp) = fixture(FAKE_CLAUDE_SCRIPT);
    let mut session = runner
        .start_session(params(tmp.path().to_path_buf()))
        .await
        .expect("start_session");

    // Synthesised by the runner itself — proves we don't depend on the
    // child for the dispatch event.
    match next_event(&mut session.events).await {
        AgentEvent::Started { session: s, issue } => {
            assert_eq!(issue.as_str(), "X-1");
            // turn id is the first allocation: "1"
            assert!(s.as_str().ends_with("-1"), "got {}", s.as_str());
        }
        other => panic!("expected Started, got {other:?}"),
    }
    match next_event(&mut session.events).await {
        AgentEvent::Message { role, content, .. } => {
            assert_eq!(role, "assistant");
            assert_eq!(content, "hello 1");
        }
        other => panic!("expected Message, got {other:?}"),
    }
    match next_event(&mut session.events).await {
        AgentEvent::TokenUsage { usage, .. } => {
            assert_eq!(usage.output, 3);
            assert_eq!(usage.input, 10);
        }
        other => panic!("expected TokenUsage, got {other:?}"),
    }
    // `result` carries the same usage (no growth) and Completed.
    let mut saw_completed = false;
    for _ in 0..3 {
        match next_event(&mut session.events).await {
            AgentEvent::TokenUsage { .. } => continue,
            AgentEvent::Completed { reason, .. } => {
                assert_eq!(reason, CompletionReason::Success);
                saw_completed = true;
                break;
            }
            other => panic!("unexpected event between usage and completed: {other:?}"),
        }
    }
    assert!(saw_completed, "never saw a Completed event");
}

#[tokio::test]
async fn continue_turn_keeps_thread_id_and_advances_turn() {
    let (runner, tmp) = fixture(FAKE_CLAUDE_SCRIPT);
    let mut session = runner
        .start_session(params(tmp.path().to_path_buf()))
        .await
        .expect("start_session");

    // Capture the thread id from the first Started event.
    let first_session = match next_event(&mut session.events).await {
        AgentEvent::Started { session: s, .. } => s.as_str().to_string(),
        other => panic!("expected Started, got {other:?}"),
    };
    let (thread, _turn) = first_session
        .rsplit_once('-')
        .expect("session id has the SPEC §4.2 separator");

    // Drain the rest of turn 1 until Completed.
    loop {
        let ev = next_event(&mut session.events).await;
        if matches!(ev, AgentEvent::Completed { .. }) {
            break;
        }
    }

    session
        .control
        .continue_turn("press on")
        .await
        .expect("continue_turn");

    // Turn 2's Started: same thread, new turn id.
    match next_event(&mut session.events).await {
        AgentEvent::Started { session: s, .. } => {
            let (thread2, turn2) = s.as_str().rsplit_once('-').unwrap();
            assert_eq!(thread2, thread, "thread id must be reused (SPEC §10.2)");
            assert_eq!(turn2, "2", "turn id must advance, got {turn2}");
        }
        other => panic!("expected Started for turn 2, got {other:?}"),
    }

    // Drain turn 2 until Completed.
    let mut saw_completed = false;
    for _ in 0..6 {
        if matches!(
            next_event(&mut session.events).await,
            AgentEvent::Completed { .. }
        ) {
            saw_completed = true;
            break;
        }
    }
    assert!(saw_completed, "turn 2 never completed");
}

#[tokio::test]
async fn abort_terminates_long_running_child_and_closes_stream() {
    let (runner, tmp) = fixture(FAKE_CLAUDE_HANG_SCRIPT);
    let session = runner
        .start_session(params(tmp.path().to_path_buf()))
        .await
        .expect("start_session");

    // Destructure so we can drop the control after abort. ClaudeAgentControl
    // holds a clone of the event-channel sender so it can synthesise
    // `AgentEvent::Started` on every continuation turn; the channel
    // therefore stays open until the control is dropped, even after the
    // reader task exits. Dropping it explicitly here is the documented
    // way to signal "I'm done with this session" to the stream.
    let symphony_core::agent::AgentSession {
        mut events,
        control,
        ..
    } = session;

    // First event is the synthesised Started.
    assert!(matches!(
        next_event(&mut events).await,
        AgentEvent::Started { .. }
    ));

    control.abort().await.expect("abort");
    // Idempotent (SPEC §10.3).
    control.abort().await.expect("second abort no-op");
    drop(control);

    drain_until_closed(&mut events, Duration::from_secs(5)).await;
}
