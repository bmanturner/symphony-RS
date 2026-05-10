//! [`ClaudeRunner`] — `AgentRunner` impl that drives the `claude` CLI in
//! `--output-format stream-json` mode.
//!
//! Unlike Codex's app-server (one long-lived subprocess per session),
//! Claude Code's `-p` mode runs one subprocess **per turn**: every turn
//! starts a fresh `claude -p <prompt> --session-id <uuid>` (or
//! `--resume <uuid>` for continuation turns), reads stream-json from
//! stdout until EOF, and exits. Symphony stitches the per-turn
//! subprocesses into a single logical session by:
//!
//! 1. Generating one `thread_id` (`SessionId`'s left half) — a v4 UUID
//!    we pass via `--session-id` on the very first turn so Claude
//!    persists it as the session id we resume against later.
//! 2. Tracking a monotonic `turn_id` counter (`"1"`, `"2"`, …) so the
//!    composite `<thread>-<turn>` `SessionId` advances per turn even
//!    though Claude itself reports the same session uuid throughout.
//! 3. Synthesising [`AgentEvent::Started`] at spawn time (Claude has no
//!    `task_started` equivalent) and emitting [`AgentEvent::Completed`]
//!    on the terminal `result` event.
//! 4. Keeping the [`AgentEventStream`] open across turns. Each
//!    `continue_turn` call spawns a fresh subprocess and a fresh reader
//!    task that shares the channel — the stream only closes on
//!    [`AgentControl::abort`] or when the [`ClaudeAgentControl`] is
//!    dropped.
//!
//! ## Test scope of THIS file
//!
//! This iteration lands the runner code path and exercises the pure
//! helpers (event translation, command building) directly. End-to-end
//! subprocess wiring is deferred to the next checklist item, which
//! provides a deterministic fake-binary fixture both Claude and Codex
//! runners exercise.

use crate::claude::protocol::{
    AssistantPayload, ClaudeEvent, ContentBlock, ResultPayload, SystemPayload, UsagePayload,
    UserPayload, decode_line,
};
use async_trait::async_trait;
use futures::Stream;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use symphony_core::agent::{
    AgentControl, AgentError, AgentEvent, AgentEventStream, AgentResult, AgentRunner, AgentSession,
    CompletionReason, SessionId, StartSessionParams, ThreadId, TokenUsage, TurnId,
};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStdout, Command};
use tokio::sync::{Mutex, mpsc};
use tracing::warn;
use uuid::Uuid;

/// Default binary name. Resolved via `$PATH`; runners that need a
/// pinned absolute path should set [`ClaudeConfig::command`].
pub const DEFAULT_CLAUDE_COMMAND: &str = "claude";

/// Configuration controlling how [`ClaudeRunner`] launches the `claude`
/// CLI.
///
/// Fields are exposed because the Claude CLI flag surface has churned
/// across releases (`--permission-mode`, `--print` vs `-p`, the addition
/// of `--verbose`); defaulting them in config rather than hard-coding
/// lets a deployment opt into the flags of its installed Claude without
/// code changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeConfig {
    /// Binary or shell command to launch. Defaults to `"claude"`.
    /// Always invoked directly (no `bash -lc`) so flag arguments
    /// pass through verbatim without shell interpolation.
    #[serde(default = "default_command")]
    pub command: String,

    /// Optional model name passed via `--model`. `None` lets Claude
    /// pick its default.
    #[serde(default)]
    pub model: Option<String>,

    /// Permission mode passed via `--permission-mode`. SPEC §10.5's
    /// high-trust profile uses `bypassPermissions`.
    #[serde(default = "default_permission_mode")]
    pub permission_mode: String,

    /// Whether to pass `--verbose`. Required by Claude for `-p` +
    /// `stream-json` to emit per-block events; without it the harness
    /// only emits the final `result` event.
    #[serde(default = "default_verbose")]
    pub verbose: bool,

    /// Extra command-line arguments appended after the canonical flags
    /// but before the prompt. Lets operators wire Claude features
    /// Symphony doesn't model (e.g. `--add-dir`, `--mcp-config`)
    /// without code changes.
    #[serde(default)]
    pub extra_args: Vec<String>,

    /// Symphony MCP tools to expose for this session. Empty disables
    /// Symphony MCP injection.
    #[serde(default)]
    pub mcp_tools: Vec<String>,
}

fn default_command() -> String {
    DEFAULT_CLAUDE_COMMAND.to_string()
}

fn default_permission_mode() -> String {
    "bypassPermissions".to_string()
}

fn default_verbose() -> bool {
    true
}

impl Default for ClaudeConfig {
    fn default() -> Self {
        Self {
            command: default_command(),
            model: None,
            permission_mode: default_permission_mode(),
            verbose: default_verbose(),
            extra_args: Vec::new(),
            mcp_tools: Vec::new(),
        }
    }
}

/// `AgentRunner` implementation backed by `claude -p`.
#[derive(Debug, Clone, Default)]
pub struct ClaudeRunner {
    config: ClaudeConfig,
}

impl ClaudeRunner {
    /// Build a runner with the given config.
    pub fn new(config: ClaudeConfig) -> Self {
        Self { config }
    }

    /// Borrow the configuration (used by tests).
    pub fn config(&self) -> &ClaudeConfig {
        &self.config
    }
}

#[async_trait]
impl AgentRunner for ClaudeRunner {
    async fn start_session(&self, params: StartSessionParams) -> AgentResult<AgentSession> {
        spawn_session(&self.config, params).await
    }
}

// ---------------------------------------------------------------------------
// Argument building
// ---------------------------------------------------------------------------

/// Build the argv for the *first* turn: `claude -p <prompt>
/// --session-id <uuid> --output-format stream-json [--verbose]
/// --permission-mode <mode> [--model <m>] [extra…]`.
///
/// `--session-id` is what lets us resume against the same session
/// uuid on continuation turns. SPEC §10.2 requires a stable
/// `thread_id` across the run; we synthesize it here and surface it as
/// the [`ThreadId`] left half of every emitted [`SessionId`].
pub(crate) fn build_initial_args(
    config: &ClaudeConfig,
    session_uuid: &str,
    prompt: &str,
) -> Vec<String> {
    let mut args = base_args(config);
    args.push("--session-id".into());
    args.push(session_uuid.into());
    args.push("-p".into());
    args.push(prompt.into());
    args
}

/// Build the argv for a continuation turn: `claude -p <guidance>
/// --resume <uuid> --output-format stream-json [--verbose]
/// --permission-mode <mode> [--model <m>] [extra…]`.
pub(crate) fn build_continue_args(
    config: &ClaudeConfig,
    session_uuid: &str,
    guidance: &str,
) -> Vec<String> {
    let mut args = base_args(config);
    args.push("--resume".into());
    args.push(session_uuid.into());
    args.push("-p".into());
    args.push(guidance.into());
    args
}

/// Common prefix for both initial and continuation argv vectors.
fn base_args(config: &ClaudeConfig) -> Vec<String> {
    let mut args = vec![
        "--output-format".into(),
        "stream-json".into(),
        "--permission-mode".into(),
        config.permission_mode.clone(),
    ];
    if config.verbose {
        args.push("--verbose".into());
    }
    if let Some(model) = &config.model {
        args.push("--model".into());
        args.push(model.clone());
    }
    for extra in &config.extra_args {
        args.push(extra.clone());
    }
    args
}

// ---------------------------------------------------------------------------
// Subprocess wiring
// ---------------------------------------------------------------------------

/// Spawn the first-turn Claude subprocess and wire up its stdout reader.
async fn spawn_session(
    config: &ClaudeConfig,
    params: StartSessionParams,
) -> AgentResult<AgentSession> {
    if !params.workspace.is_absolute() {
        return Err(AgentError::Spawn(format!(
            "workspace must be absolute, got {}",
            params.workspace.display()
        )));
    }
    if !params.workspace.exists() {
        return Err(AgentError::Spawn(format!(
            "workspace does not exist: {}",
            params.workspace.display()
        )));
    }

    let session_uuid = Uuid::new_v4().to_string();
    let turn_counter = Arc::new(AtomicU64::new(1));
    let (event_tx, event_rx) = mpsc::channel::<AgentEvent>(64);

    let turn_id = TurnId::new(turn_counter.fetch_add(1, Ordering::Relaxed).to_string());
    let thread_id = ThreadId::new(session_uuid.clone());
    let initial_session = SessionId::new(&thread_id, &turn_id);

    let (mcp_session, extra_args) =
        crate::mcp::prepare_mcp_session(&params.workspace, &config.extra_args, &config.mcp_tools)
            .await?;
    let mut launch_config = config.clone();
    launch_config.extra_args = extra_args;

    let args = build_initial_args(&launch_config, &session_uuid, &params.initial_prompt);
    let child = spawn_child(&launch_config.command, &args, &params.workspace)?;

    // Synthesise Started immediately so the orchestrator sees a
    // dispatch event without waiting for stdout.
    let _ = event_tx
        .send(AgentEvent::Started {
            session: initial_session.clone(),
            issue: params.issue.clone(),
        })
        .await;

    let child_handle = Arc::new(Mutex::new(Some(child)));
    let reader_handle = spawn_reader(
        child_handle.clone(),
        event_tx.clone(),
        params.issue.clone(),
        thread_id.clone(),
        turn_id,
    );

    let control = ClaudeAgentControl {
        config: launch_config,
        workspace: params.workspace.clone(),
        issue: params.issue,
        session_uuid,
        thread: thread_id,
        turn_counter,
        event_tx,
        current_child: child_handle,
        current_reader: Arc::new(Mutex::new(Some(reader_handle))),
        _mcp_session: mcp_session,
    };

    Ok(AgentSession {
        initial_session,
        events: stream_from_receiver(event_rx),
        control: Box::new(control),
    })
}

/// Spawn a child process with the configured command + args, cwd'd into
/// the per-issue workspace, with stdout piped (we don't need stdin —
/// `-p` carries the prompt as an argv).
fn spawn_child(command: &str, args: &[String], cwd: &std::path::Path) -> AgentResult<Child> {
    Command::new(command)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| AgentError::Spawn(format!("spawn `{command}`: {e}")))
}

/// Take the child's stdout and spawn a tokio task that pumps lines
/// through the decoder and translator into the shared event channel.
fn spawn_reader(
    child: Arc<Mutex<Option<Child>>>,
    tx: mpsc::Sender<AgentEvent>,
    issue: symphony_core::tracker::IssueId,
    thread: ThreadId,
    turn: TurnId,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let stdout = {
            let mut guard = child.lock().await;
            guard.as_mut().and_then(|c| c.stdout.take())
        };
        let Some(stdout) = stdout else {
            warn!("claude child stdout missing");
            return;
        };
        reader_loop(stdout, tx, issue, thread, turn).await;
    })
}

/// Pump stdout: decode each line, translate, send. On EOF without a
/// terminal `result` event, synthesize a `Completed{SubprocessExit}`.
async fn reader_loop(
    stdout: ChildStdout,
    tx: mpsc::Sender<AgentEvent>,
    issue: symphony_core::tracker::IssueId,
    thread: ThreadId,
    turn: TurnId,
) {
    let mut state = TranslateState::new(issue, thread, turn);
    let mut lines = BufReader::new(stdout).lines();

    loop {
        match lines.next_line().await {
            Ok(Some(line)) if line.trim().is_empty() => continue,
            Ok(Some(line)) => match decode_line(&line) {
                Ok(event) => {
                    for ev in state.translate(event) {
                        if tx.send(ev).await.is_err() {
                            return;
                        }
                    }
                }
                Err(e) => warn!(error = %e, line = %line, "malformed claude line"),
            },
            Ok(None) => {
                if !state.terminal_sent {
                    let _ = tx
                        .send(AgentEvent::Completed {
                            session: state.session(),
                            reason: CompletionReason::SubprocessExit,
                        })
                        .await;
                }
                return;
            }
            Err(e) => {
                warn!(error = %e, "claude stdout read failed");
                let _ = tx
                    .send(AgentEvent::Failed {
                        session: state.session(),
                        error: format!("stdout read: {e}"),
                    })
                    .await;
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Event translation
// ---------------------------------------------------------------------------

/// Per-turn translation state.
///
/// One instance lives for the lifetime of a single subprocess (turn).
/// Continuation turns get a fresh `TranslateState` because `TurnId`
/// changes; the `ThreadId` is preserved by the runner across instances.
#[derive(Debug)]
pub(crate) struct TranslateState {
    thread: ThreadId,
    turn: TurnId,
    /// Set true after we emit a terminal `Completed`/`Failed` so the
    /// EOF handler doesn't double-emit.
    pub(crate) terminal_sent: bool,
    /// Cumulative usage so we emit a final TokenUsage on `result`
    /// regardless of whether `assistant` events carried partials.
    tokens: TokenUsage,
}

impl TranslateState {
    pub(crate) fn new(
        _issue: symphony_core::tracker::IssueId,
        thread: ThreadId,
        turn: TurnId,
    ) -> Self {
        // `_issue` is accepted for API symmetry with codex's
        // TranslateState — the runner emits `AgentEvent::Started`
        // synthetically at spawn time so the issue id never needs to
        // flow through the per-turn translator.
        Self {
            thread,
            turn,
            terminal_sent: false,
            tokens: TokenUsage::default(),
        }
    }

    fn session(&self) -> SessionId {
        SessionId::new(&self.thread, &self.turn)
    }

    /// Translate one Claude event into zero or more normalized
    /// [`AgentEvent`]s.
    ///
    /// `Started` is emitted by the runner at spawn time, not here, so
    /// the caller never sees a `Started` from this function. `result`
    /// becomes both a `TokenUsage` and a terminal `Completed`/`Failed`.
    pub(crate) fn translate(&mut self, event: ClaudeEvent) -> Vec<AgentEvent> {
        match event {
            ClaudeEvent::System(SystemPayload { .. }) => {
                // System init carries no information the orchestrator
                // needs (we already know the thread id). Suppress.
                Vec::new()
            }
            ClaudeEvent::Assistant(p) => self.translate_assistant(p),
            ClaudeEvent::User(p) => self.translate_user(p),
            ClaudeEvent::Result(p) => self.translate_result(p),
            ClaudeEvent::Other(_) => Vec::new(),
        }
    }

    fn translate_assistant(&mut self, p: AssistantPayload) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        // Flatten text blocks into a single Message; emit ToolUse
        // separately so observability surfaces tool calls verbatim.
        let mut text = String::new();
        for block in p.message.content {
            match block {
                ContentBlock::Text { text: t } => {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&t);
                }
                ContentBlock::ToolUse { name, input, .. } => {
                    out.push(AgentEvent::ToolUse {
                        session: self.session(),
                        tool: name,
                        input,
                    });
                }
                ContentBlock::ToolResult { .. } | ContentBlock::Other(_) => {}
            }
        }
        if !text.is_empty() {
            out.insert(
                0,
                AgentEvent::Message {
                    session: self.session(),
                    role: "assistant".into(),
                    content: text,
                },
            );
        }
        if let Some(u) = p.message.usage {
            self.tokens = merge_usage(&self.tokens, &u);
            out.push(AgentEvent::TokenUsage {
                session: self.session(),
                usage: self.tokens.clone(),
            });
        }
        out
    }

    fn translate_user(&mut self, p: UserPayload) -> Vec<AgentEvent> {
        // Tool results aren't useful as standalone Messages — they're
        // output of tools we don't run anyway (Claude runs them
        // in-process when bypassPermissions is set). Surface only as a
        // single `Message{role=user}` per event when there's text-like
        // content; otherwise drop. Keeps the orchestrator's stream
        // small without losing observability.
        let mut content = String::new();
        for block in p.message.content {
            if let ContentBlock::ToolResult {
                content: c,
                is_error,
                ..
            } = block
            {
                let s = match &c {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                if !content.is_empty() {
                    content.push('\n');
                }
                if is_error {
                    content.push_str("[error] ");
                }
                content.push_str(&s);
            }
        }
        if content.is_empty() {
            Vec::new()
        } else {
            vec![AgentEvent::Message {
                session: self.session(),
                role: "user".into(),
                content,
            }]
        }
    }

    fn translate_result(&mut self, p: ResultPayload) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        if let Some(u) = p.usage {
            self.tokens = merge_usage(&self.tokens, &u);
            out.push(AgentEvent::TokenUsage {
                session: self.session(),
                usage: self.tokens.clone(),
            });
        }
        self.terminal_sent = true;
        if p.is_error {
            out.push(AgentEvent::Failed {
                session: self.session(),
                error: p
                    .subtype
                    .unwrap_or_else(|| "claude reported is_error".into()),
            });
        } else {
            out.push(AgentEvent::Completed {
                session: self.session(),
                reason: CompletionReason::Success,
            });
        }
        out
    }
}

/// Merge a Claude `UsagePayload` into the cumulative `TokenUsage`
/// snapshot we report to the orchestrator.
///
/// Claude reports cache creation and cache reads as separate buckets
/// from base prompt input. We sum all three into `input` (so cost
/// estimators see the full input footprint) and surface cache reads
/// separately under `cached_input` per
/// [`symphony_core::agent::TokenUsage`]'s contract.
fn merge_usage(prev: &TokenUsage, u: &UsagePayload) -> TokenUsage {
    let input = u.input_tokens + u.cache_creation_input_tokens + u.cache_read_input_tokens;
    let output = u.output_tokens;
    let cached = u.cache_read_input_tokens;
    // Claude resends cumulative usage on each assistant event during
    // streaming, so `max` is the safe merge regardless of partial vs.
    // total semantics.
    TokenUsage {
        input: prev.input.max(input),
        output: prev.output.max(output),
        cached_input: prev.cached_input.max(cached),
        total: prev.total.max(input + output),
    }
}

// ---------------------------------------------------------------------------
// Stream wiring
// ---------------------------------------------------------------------------

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
// Control surface
// ---------------------------------------------------------------------------

/// `AgentControl` returned by [`ClaudeRunner::start_session`].
///
/// Owns the per-session uuid, the turn counter, and the currently-live
/// child handle. `continue_turn` spawns a fresh subprocess (via
/// `--resume <uuid>`) with the next turn id; `abort` kills whatever
/// child is current.
pub struct ClaudeAgentControl {
    config: ClaudeConfig,
    workspace: std::path::PathBuf,
    issue: symphony_core::tracker::IssueId,
    session_uuid: String,
    thread: ThreadId,
    turn_counter: Arc<AtomicU64>,
    event_tx: mpsc::Sender<AgentEvent>,
    current_child: Arc<Mutex<Option<Child>>>,
    current_reader: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    _mcp_session: Option<crate::mcp::AgentMcpSession>,
}

#[async_trait]
impl AgentControl for ClaudeAgentControl {
    async fn continue_turn(&self, guidance: &str) -> AgentResult<()> {
        // Wait for any in-flight reader to finish before starting the
        // next turn so events stay ordered. The previous child should
        // already have exited (Claude `-p` is one-shot per turn) but
        // we await defensively in case the orchestrator races.
        if let Some(handle) = self.current_reader.lock().await.take() {
            let _ = handle.await;
        }

        let turn_id = TurnId::new(
            self.turn_counter
                .fetch_add(1, Ordering::Relaxed)
                .to_string(),
        );
        let session = SessionId::new(&self.thread, &turn_id);

        let args = build_continue_args(&self.config, &self.session_uuid, guidance);
        let child = spawn_child(&self.config.command, &args, &self.workspace)?;

        let _ = self
            .event_tx
            .send(AgentEvent::Started {
                session: session.clone(),
                issue: self.issue.clone(),
            })
            .await;

        let mut child_slot = self.current_child.lock().await;
        *child_slot = Some(child);
        drop(child_slot);

        let handle = spawn_reader(
            self.current_child.clone(),
            self.event_tx.clone(),
            self.issue.clone(),
            self.thread.clone(),
            turn_id,
        );
        *self.current_reader.lock().await = Some(handle);
        Ok(())
    }

    async fn abort(&self) -> AgentResult<()> {
        // Kill the child if alive; reader will then see EOF and emit
        // SubprocessExit. The orchestrator can choose to interpret that
        // as cancellation.
        let mut child_slot = self.current_child.lock().await;
        if let Some(mut child) = child_slot.take() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        if let Some(handle) = self.current_reader.lock().await.take() {
            handle.abort();
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Errors (parity with CodexRunnerError)
// ---------------------------------------------------------------------------

/// Errors specific to the Claude runner. Currently a thin wrapper over
/// [`AgentError`]; preserved as a public type so a future operator-facing
/// CLI can pattern-match without depending on `symphony-core`.
#[derive(Debug, Error)]
pub enum ClaudeRunnerError {
    /// Bubbled-up `AgentError` from the trait.
    #[error(transparent)]
    Agent(#[from] AgentError),
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;
    use symphony_core::tracker::IssueId;

    fn fresh_state() -> TranslateState {
        TranslateState::new(
            IssueId::new("X-1"),
            ThreadId::new("uuid-thread"),
            TurnId::new("1"),
        )
    }

    #[test]
    fn build_initial_args_includes_session_id_and_prompt() {
        let cfg = ClaudeConfig::default();
        let args = build_initial_args(&cfg, "uuid-1", "do the thing");
        assert!(args.iter().any(|a| a == "--session-id"));
        assert!(args.iter().any(|a| a == "uuid-1"));
        assert!(args.iter().any(|a| a == "-p"));
        assert!(args.iter().any(|a| a == "do the thing"));
        assert!(args.iter().any(|a| a == "--output-format"));
        assert!(args.iter().any(|a| a == "stream-json"));
        assert!(args.iter().any(|a| a == "--verbose"));
        assert!(args.iter().any(|a| a == "--permission-mode"));
        assert!(args.iter().any(|a| a == "bypassPermissions"));
    }

    #[test]
    fn build_continue_args_uses_resume_not_session_id() {
        let cfg = ClaudeConfig::default();
        let args = build_continue_args(&cfg, "uuid-1", "press on");
        assert!(args.iter().any(|a| a == "--resume"));
        assert!(!args.iter().any(|a| a == "--session-id"));
        assert!(args.iter().any(|a| a == "press on"));
    }

    #[test]
    fn config_model_and_extra_args_threaded_through() {
        let cfg = ClaudeConfig {
            model: Some("claude-opus-4-7".into()),
            extra_args: vec!["--add-dir".into(), "/data".into()],
            ..Default::default()
        };
        let args = build_initial_args(&cfg, "u", "p");
        let joined = args.join(" ");
        assert!(joined.contains("--model claude-opus-4-7"));
        assert!(joined.contains("--add-dir /data"));
    }

    #[test]
    fn verbose_false_omits_flag() {
        let cfg = ClaudeConfig {
            verbose: false,
            ..Default::default()
        };
        let args = build_initial_args(&cfg, "u", "p");
        assert!(!args.iter().any(|a| a == "--verbose"));
    }

    #[test]
    fn translate_assistant_text_emits_message_and_usage() {
        let mut state = fresh_state();
        let event = decode_line(
            r#"{"type":"assistant","session_id":"u","message":{"id":"m1","content":[{"type":"text","text":"hello"}],"usage":{"input_tokens":10,"output_tokens":3,"cache_read_input_tokens":2}}}"#,
        )
        .unwrap();
        let out = state.translate(event);
        assert_eq!(out.len(), 2);
        match &out[0] {
            AgentEvent::Message { role, content, .. } => {
                assert_eq!(role, "assistant");
                assert_eq!(content, "hello");
            }
            other => panic!("expected Message, got {other:?}"),
        }
        match &out[1] {
            AgentEvent::TokenUsage { usage, .. } => {
                // input = 10 + 0 (cache_creation) + 2 (cache_read) = 12
                assert_eq!(usage.input, 12);
                assert_eq!(usage.output, 3);
                assert_eq!(usage.cached_input, 2);
                assert_eq!(usage.total, 15);
            }
            other => panic!("expected TokenUsage, got {other:?}"),
        }
    }

    #[test]
    fn translate_assistant_tool_use_emits_tooluse_event() {
        let mut state = fresh_state();
        let event = decode_line(
            r#"{"type":"assistant","session_id":"u","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}"#,
        )
        .unwrap();
        let out = state.translate(event);
        assert_eq!(out.len(), 1);
        match &out[0] {
            AgentEvent::ToolUse { tool, input, .. } => {
                assert_eq!(tool, "Bash");
                assert_eq!(input["command"], json!("ls"));
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn translate_result_success_emits_usage_then_completed() {
        let mut state = fresh_state();
        let event = decode_line(
            r#"{"type":"result","subtype":"success","session_id":"u","is_error":false,"usage":{"input_tokens":100,"output_tokens":40}}"#,
        )
        .unwrap();
        let out = state.translate(event);
        assert_eq!(out.len(), 2);
        assert!(matches!(out[0], AgentEvent::TokenUsage { .. }));
        match &out[1] {
            AgentEvent::Completed { reason, .. } => {
                assert_eq!(*reason, CompletionReason::Success);
            }
            other => panic!("expected Completed, got {other:?}"),
        }
        assert!(state.terminal_sent);
    }

    #[test]
    fn translate_result_is_error_emits_failed() {
        let mut state = fresh_state();
        let event = decode_line(
            r#"{"type":"result","subtype":"error_max_turns","session_id":"u","is_error":true}"#,
        )
        .unwrap();
        let out = state.translate(event);
        match out.last().unwrap() {
            AgentEvent::Failed { error, .. } => assert_eq!(error, "error_max_turns"),
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(state.terminal_sent);
    }

    #[test]
    fn translate_user_tool_result_emits_user_message() {
        let mut state = fresh_state();
        let event = decode_line(
            r#"{"type":"user","session_id":"u","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"file listing","is_error":false}]}}"#,
        )
        .unwrap();
        let out = state.translate(event);
        assert_eq!(out.len(), 1);
        match &out[0] {
            AgentEvent::Message { role, content, .. } => {
                assert_eq!(role, "user");
                assert_eq!(content, "file listing");
            }
            other => panic!("expected Message, got {other:?}"),
        }
    }

    #[test]
    fn translate_user_error_tool_result_prefixes_marker() {
        let mut state = fresh_state();
        let event = decode_line(
            r#"{"type":"user","session_id":"u","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"perm denied","is_error":true}]}}"#,
        )
        .unwrap();
        let out = state.translate(event);
        match &out[0] {
            AgentEvent::Message { content, .. } => assert!(content.starts_with("[error]")),
            other => panic!("expected Message, got {other:?}"),
        }
    }

    #[test]
    fn translate_system_init_emits_nothing() {
        let mut state = fresh_state();
        let event = decode_line(
            r#"{"type":"system","subtype":"init","session_id":"u","model":"x","cwd":"/w"}"#,
        )
        .unwrap();
        assert!(state.translate(event).is_empty());
    }

    #[test]
    fn translate_unknown_event_is_silent() {
        let mut state = fresh_state();
        let event = decode_line(r#"{"type":"future_event","x":1}"#).unwrap();
        assert!(state.translate(event).is_empty());
    }

    #[test]
    fn merge_usage_tracks_running_max_across_partials() {
        let prev = TokenUsage {
            input: 10,
            output: 5,
            cached_input: 0,
            total: 15,
        };
        let small = UsagePayload {
            input_tokens: 5,
            output_tokens: 2,
            ..Default::default()
        };
        let merged = merge_usage(&prev, &small);
        // Don't regress the larger prior snapshot.
        assert_eq!(merged.input, 10);
        assert_eq!(merged.output, 5);
        assert_eq!(merged.total, 15);

        let bigger = UsagePayload {
            input_tokens: 100,
            output_tokens: 40,
            cache_read_input_tokens: 30,
            ..Default::default()
        };
        let merged = merge_usage(&prev, &bigger);
        assert_eq!(merged.input, 130);
        assert_eq!(merged.output, 40);
        assert_eq!(merged.cached_input, 30);
        assert_eq!(merged.total, 170);
    }

    #[tokio::test]
    async fn start_session_rejects_relative_workspace() {
        let runner = ClaudeRunner::default();
        let res = runner
            .start_session(StartSessionParams {
                issue: IssueId::new("X-1"),
                workspace: PathBuf::from("relative/path"),
                initial_prompt: "p".into(),
                issue_label: None,
            })
            .await;
        assert!(matches!(res, Err(AgentError::Spawn(_))));
    }

    #[tokio::test]
    async fn start_session_rejects_missing_workspace() {
        let runner = ClaudeRunner::default();
        let res = runner
            .start_session(StartSessionParams {
                issue: IssueId::new("X-1"),
                workspace: PathBuf::from("/this/does/not/exist/symphony-claude-test"),
                initial_prompt: "p".into(),
                issue_label: None,
            })
            .await;
        assert!(matches!(res, Err(AgentError::Spawn(_))));
    }
}
