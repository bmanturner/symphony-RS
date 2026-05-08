//! Subprocess-level tests for [`CodexRunner`] driven by a fake bash
//! "codex app-server" that speaks just enough JSON-RPC to exercise the
//! runner's spawn / stdin-write / stdout-read / abort paths end to end.
//!
//! What this gives us that the in-tree unit tests don't:
//!
//! - Real `tokio::process` spawning, real pipe plumbing, real EOF-on-kill.
//! - Real ordering between `bootstrap_session` (which writes initialize +
//!   newConversation + sendUserTurn before returning) and the reader task
//!   that pumps the script's stdout into the event stream.
//! - A continuation-turn round-trip via [`AgentControl::continue_turn`].
//! - Abort: the runner should kill the subprocess and the stream should
//!   close cleanly within a bounded window.
//!
//! The fake binary lives only on Unix because we rely on `chmod +x` and
//! `bash`. CI runs on macOS / Linux, which is sufficient for now;
//! Windows support would need a `.cmd` equivalent and is out of scope.

#![cfg(unix)]

mod common;

use common::{drain_until_closed, next_event, write_script};
use std::time::Duration;
use symphony_agent::codex::{CodexConfig, CodexRunner};
use symphony_core::agent::{AgentEvent, AgentRunner, CompletionReason, StartSessionParams};
use symphony_core::tracker::IssueId;
use tempfile::TempDir;

/// Bash script: pretend to be `codex app-server`.
///
/// Reads JSON-RPC frames line-by-line off stdin. We can't easily parse
/// JSON in pure bash, so we substring-match the `method` name — the
/// Symphony runner always serialises `"method": "..."` with a literal
/// substring we can grep. On each `sendUserTurn` we emit a turn cycle;
/// `interruptConversation` exits the script (mirrors how a real Codex
/// would terminate the conversation).
///
/// The first turn additionally emits a `session_configured` to seed the
/// runner's `ThreadId`. Subsequent turns reuse it.
const FAKE_CODEX_SCRIPT: &str = r#"#!/usr/bin/env bash
set -u
turn=0
emit_turn() {
  turn=$((turn+1))
  if [ "$turn" -eq 1 ]; then
    printf '{"jsonrpc":"2.0","method":"codex/event","params":{"id":"e0","msg":{"type":"session_configured","session_id":"th-fake"}}}\n'
  fi
  printf '{"jsonrpc":"2.0","method":"codex/event","params":{"id":"e%s-s","msg":{"type":"task_started","turn_id":"t%s"}}}\n' "$turn" "$turn"
  printf '{"jsonrpc":"2.0","method":"codex/event","params":{"id":"e%s-m","msg":{"type":"agent_message","message":"hi %s"}}}\n' "$turn" "$turn"
  printf '{"jsonrpc":"2.0","method":"codex/event","params":{"id":"e%s-c","msg":{"type":"task_complete"}}}\n' "$turn"
}
while IFS= read -r line; do
  case "$line" in
    *sendUserTurn*) emit_turn ;;
    *interruptConversation*) exit 0 ;;
  esac
done
"#;

/// Build a runner pointed at a freshly-written fake-codex script in a
/// throwaway tempdir. Returns the runner alongside the tempdir handle —
/// callers must keep the handle alive for the duration of the test or
/// the script (and workspace) get unlinked from under the subprocess.
fn fixture() -> (CodexRunner, TempDir) {
    let tmp = TempDir::new().expect("tempdir");
    let script = write_script(tmp.path(), "fake_codex.sh", FAKE_CODEX_SCRIPT);
    let cfg = CodexConfig {
        command: script.to_string_lossy().into_owned(),
        ..Default::default()
    };
    (CodexRunner::new(cfg), tmp)
}

/// Standard `StartSessionParams` for these tests: workspace = tempdir,
/// dummy issue id, trivial prompt.
fn params(workspace: std::path::PathBuf) -> StartSessionParams {
    StartSessionParams {
        issue: IssueId::new("X-1"),
        workspace,
        initial_prompt: "do the thing".into(),
        issue_label: Some("X-1: do the thing".into()),
    }
}

#[tokio::test]
async fn first_turn_yields_started_message_completed_in_order() {
    let (runner, tmp) = fixture();
    let mut session = runner
        .start_session(params(tmp.path().to_path_buf()))
        .await
        .expect("start_session");

    // session_configured is silent; task_started → Started.
    match next_event(&mut session.events).await {
        AgentEvent::Started { session: s, issue } => {
            assert_eq!(s.as_str(), "th-fake-t1");
            assert_eq!(issue.as_str(), "X-1");
        }
        other => panic!("expected Started, got {other:?}"),
    }
    match next_event(&mut session.events).await {
        AgentEvent::Message { role, content, .. } => {
            assert_eq!(role, "assistant");
            assert_eq!(content, "hi 1");
        }
        other => panic!("expected Message, got {other:?}"),
    }
    match next_event(&mut session.events).await {
        AgentEvent::Completed { reason, .. } => {
            assert_eq!(reason, CompletionReason::Success);
        }
        other => panic!("expected Completed, got {other:?}"),
    }

    // Clean up the still-running subprocess so the test exits promptly.
    session.control.abort().await.unwrap();
}

#[tokio::test]
async fn continue_turn_starts_a_second_turn_with_new_id() {
    let (runner, tmp) = fixture();
    let mut session = runner
        .start_session(params(tmp.path().to_path_buf()))
        .await
        .expect("start_session");

    // Drain turn 1.
    for _ in 0..3 {
        next_event(&mut session.events).await;
    }

    session
        .control
        .continue_turn("press on")
        .await
        .expect("continue_turn");

    match next_event(&mut session.events).await {
        AgentEvent::Started { session: s, .. } => {
            // Same thread, new turn — proves SPEC §10.2 thread reuse.
            assert_eq!(s.as_str(), "th-fake-t2");
        }
        other => panic!("expected Started for turn 2, got {other:?}"),
    }
    // Message + Completed for turn 2.
    next_event(&mut session.events).await;
    match next_event(&mut session.events).await {
        AgentEvent::Completed { reason, .. } => {
            assert_eq!(reason, CompletionReason::Success);
        }
        other => panic!("expected Completed for turn 2, got {other:?}"),
    }

    session.control.abort().await.unwrap();
}

#[tokio::test]
async fn abort_kills_subprocess_and_closes_stream() {
    let (runner, tmp) = fixture();
    let mut session = runner
        .start_session(params(tmp.path().to_path_buf()))
        .await
        .expect("start_session");

    // Drain turn 1 so we know the subprocess is fully wired.
    for _ in 0..3 {
        next_event(&mut session.events).await;
    }

    session.control.abort().await.expect("abort");
    // Idempotency check (SPEC §10.3): a second abort must succeed.
    session.control.abort().await.expect("second abort no-op");

    drain_until_closed(&mut session.events, Duration::from_secs(5)).await;
}
