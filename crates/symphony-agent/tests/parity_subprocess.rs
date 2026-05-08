//! Stream-shape parity test: a single logical "say hello and finish"
//! turn must produce the same normalized [`AgentEvent`] sequence under
//! both [`CodexRunner`] and [`ClaudeRunner`], modulo backend-only
//! events.
//!
//! Why this matters: the orchestrator (Phase 5) is written against the
//! `AgentEvent` enum and MUST behave identically regardless of which
//! backend produced the events. If one backend emits a `Started`
//! between every message while the other doesn't, the orchestrator's
//! state machine will diverge in subtle ways. Locking the shape down at
//! the integration level — with real subprocesses on both sides —
//! catches that class of bug before it reaches the orchestrator.
//!
//! What "modulo backend-only events" means in practice:
//!
//! - Claude's `result` event always carries a `usage` block, so
//!   [`AgentEvent::TokenUsage`] is emitted at least twice per turn
//!   (once on the `assistant` event, once on `result`). The Codex
//!   JSON-RPC protocol carries token telemetry on a separate
//!   `task_token_count` event that our fake binary doesn't emit
//!   because nothing about the orchestrator's correctness depends on
//!   per-message token deltas. SPEC §10.1 explicitly allows
//!   `TokenUsage` to be optional ("adapters MAY emit incremental
//!   updates"), so we filter it out before comparing shapes.
//! - Other variants (`ToolUse`, `RateLimit`, `Failed`) are not
//!   exercised here — they each get backend-specific coverage in the
//!   fake-binary tests.
//!
//! The remaining required sequence per turn is therefore:
//!
//!     Started → Message → Completed{Success}
//!
//! Both backends MUST produce exactly that, in that order, for a
//! happy-path "say hello" turn.

#![cfg(unix)]

mod common;

use common::{next_event, write_script};
use symphony_agent::claude::{ClaudeConfig, ClaudeRunner};
use symphony_agent::codex::{CodexConfig, CodexRunner};
use symphony_core::agent::{AgentEvent, AgentRunner, CompletionReason, StartSessionParams};
use symphony_core::tracker::IssueId;
use tempfile::TempDir;

/// Variant tag for shape comparison. We deliberately drop fields here:
/// session ids, message text, and token counts all differ between
/// backends in the *content* dimension, but the orchestrator only
/// dispatches off the variant. Equating on the tag is the right level
/// of abstraction for parity.
#[derive(Debug, PartialEq, Eq)]
enum Shape {
    Started,
    Message,
    Completed,
}

/// Map an [`AgentEvent`] to its parity-relevant [`Shape`]. Backend-only
/// variants ([`AgentEvent::TokenUsage`] in particular) return `None`
/// and get filtered out by the caller.
fn shape_of(ev: &AgentEvent) -> Option<Shape> {
    match ev {
        AgentEvent::Started { .. } => Some(Shape::Started),
        AgentEvent::Message { .. } => Some(Shape::Message),
        AgentEvent::Completed { .. } => Some(Shape::Completed),
        // SPEC §10.1: optional, not part of the parity contract.
        AgentEvent::TokenUsage { .. } => None,
        // Unused in this scripted scenario; surface a clear failure if
        // a future fake-binary change starts emitting one unexpectedly.
        other => panic!("parity test saw an unexpected event variant: {other:?}"),
    }
}

/// Drain events from `stream` until a `Completed` arrives, returning
/// the parity-relevant shapes in order plus the final reason. Bounds
/// the loop so a misbehaving fake binary fails the test rather than
/// hanging forever.
async fn collect_turn<S>(stream: &mut S) -> (Vec<Shape>, CompletionReason)
where
    S: futures::Stream<Item = AgentEvent> + Unpin,
{
    let mut shapes = Vec::new();
    // Generous cap: a real turn is 3 events, our scripts at most 5.
    // Anything past 16 means the script is wedged and we'd rather fail
    // loudly than spin.
    for _ in 0..16 {
        let ev = next_event(stream).await;
        if let AgentEvent::Completed { reason, .. } = &ev {
            let r = reason.clone();
            if let Some(s) = shape_of(&ev) {
                shapes.push(s);
            }
            return (shapes, r);
        }
        if let Some(s) = shape_of(&ev) {
            shapes.push(s);
        }
    }
    panic!("collect_turn exhausted its event budget without seeing Completed");
}

/// Bash fake for Codex: emits one happy-path turn — same shape as
/// `fake_codex_subprocess.rs` but trimmed to the parity contract.
const CODEX_PARITY_SCRIPT: &str = r#"#!/usr/bin/env bash
set -u
emit_turn() {
  printf '{"jsonrpc":"2.0","method":"codex/event","params":{"id":"e0","msg":{"type":"session_configured","session_id":"th-parity"}}}\n'
  printf '{"jsonrpc":"2.0","method":"codex/event","params":{"id":"e1","msg":{"type":"task_started","turn_id":"t1"}}}\n'
  printf '{"jsonrpc":"2.0","method":"codex/event","params":{"id":"e2","msg":{"type":"agent_message","message":"hello"}}}\n'
  printf '{"jsonrpc":"2.0","method":"codex/event","params":{"id":"e3","msg":{"type":"task_complete"}}}\n'
}
while IFS= read -r line; do
  case "$line" in
    *sendUserTurn*) emit_turn ;;
    *interruptConversation*) exit 0 ;;
  esac
done
"#;

/// Bash fake for Claude: same logical scenario. The runner synthesises
/// `Started` itself; the `system` event is silent; `assistant` and
/// `result` carry the parity-visible bits.
const CLAUDE_PARITY_SCRIPT: &str = r#"#!/usr/bin/env bash
set -u
sid="parity-claude-uuid"
printf '{"type":"system","subtype":"init","session_id":"%s","model":"fake","cwd":"/x"}\n' "$sid"
printf '{"type":"assistant","session_id":"%s","message":{"id":"m1","content":[{"type":"text","text":"hello"}],"usage":{"input_tokens":1,"output_tokens":1}}}\n' "$sid"
printf '{"type":"result","subtype":"success","session_id":"%s","is_error":false,"usage":{"input_tokens":1,"output_tokens":1}}\n' "$sid"
"#;

fn params(workspace: std::path::PathBuf) -> StartSessionParams {
    StartSessionParams {
        issue: IssueId::new("PAR-1"),
        workspace,
        initial_prompt: "say hello".into(),
        issue_label: Some("PAR-1: parity".into()),
    }
}

#[tokio::test]
async fn happy_path_turn_has_identical_shape_across_backends() {
    // --- Codex side ---
    let codex_tmp = TempDir::new().expect("codex tempdir");
    let codex_script = write_script(codex_tmp.path(), "fake_codex.sh", CODEX_PARITY_SCRIPT);
    let codex = CodexRunner::new(CodexConfig {
        command: codex_script.to_string_lossy().into_owned(),
        ..Default::default()
    });
    let mut codex_session = codex
        .start_session(params(codex_tmp.path().to_path_buf()))
        .await
        .expect("codex start_session");
    let (codex_shapes, codex_reason) = collect_turn(&mut codex_session.events).await;
    // Tear down so the still-running subprocess doesn't outlive the test.
    codex_session.control.abort().await.unwrap();

    // --- Claude side ---
    let claude_tmp = TempDir::new().expect("claude tempdir");
    let claude_script = write_script(claude_tmp.path(), "fake_claude.sh", CLAUDE_PARITY_SCRIPT);
    let claude = ClaudeRunner::new(ClaudeConfig {
        command: claude_script.to_string_lossy().into_owned(),
        ..Default::default()
    });
    let mut claude_session = claude
        .start_session(params(claude_tmp.path().to_path_buf()))
        .await
        .expect("claude start_session");
    let (claude_shapes, claude_reason) = collect_turn(&mut claude_session.events).await;

    // The parity contract: same shape sequence, same completion reason.
    let expected = vec![Shape::Started, Shape::Message, Shape::Completed];
    assert_eq!(
        codex_shapes, expected,
        "codex shape sequence drifted from the parity contract"
    );
    assert_eq!(
        claude_shapes, expected,
        "claude shape sequence drifted from the parity contract"
    );
    assert_eq!(codex_reason, CompletionReason::Success);
    assert_eq!(claude_reason, CompletionReason::Success);
    // Tautology in the assertion, but it documents the *contract*: if
    // either side ever changes, this comparison fails first and the
    // diff names the offender.
    assert_eq!(codex_shapes, claude_shapes);
    assert_eq!(codex_reason, claude_reason);
}
