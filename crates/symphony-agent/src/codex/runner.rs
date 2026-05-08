//! [`CodexRunner`] — `AgentRunner` impl that drives `codex app-server`.
//!
//! The runner does five jobs:
//!
//! 1. **Spawn**. Per SPEC §10.1 we launch the configured command via
//!    `bash -lc <codex.command>`, with the per-issue workspace as cwd
//!    and stdio piped so we own the JSON-RPC channel.
//! 2. **Initialize / open conversation**. We send the JSON-RPC handshake
//!    Codex's app-server expects so the server is ready to accept a
//!    turn. The exact method names are read from config (with sane
//!    defaults) so a future Codex version that renames them only
//!    requires a config-level patch — SPEC §10 explicitly disclaims
//!    schema-locking.
//! 3. **Translate**. Each `codex/event` notification's typed `msg`
//!    becomes a normalized [`AgentEvent`]. We accumulate cumulative
//!    token counts so consumers get a sensible final
//!    [`AgentEvent::TokenUsage`] even if Codex only reports running
//!    totals.
//! 4. **Reply to server requests**. Codex may send approval requests or
//!    other server-initiated requests mid-turn; SPEC §10.5's high-trust
//!    profile says auto-approve known kinds and reject everything else
//!    so the session never stalls.
//! 5. **Continuation / abort**. The [`AgentControl`] handle the runner
//!    returns lets the orchestrator request another turn on the same
//!    thread or cancel via `interruptConversation`.
//!
//! ## Test scope of THIS file
//!
//! This iteration lands the runner code path but exercises only the
//! pure helpers (event translation, request building) directly.
//! End-to-end subprocess wiring lands behind the fake-binary fixture
//! in the next checklist item — that's where we exercise spawn /
//! interrupt / continuation against a deterministic stand-in.

use crate::codex::protocol::{
    AgentMessagePayload, CodexEventEnvelope, CodexMsg, ErrorPayload, ExecCommandPayload,
    JsonRpcFrame, McpToolCallPayload, SessionConfiguredPayload, TaskStartedPayload,
    TokenCountPayload, decode_event_params, decode_line,
};
use async_trait::async_trait;
use futures::Stream;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;
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
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, mpsc};
use tracing::{debug, warn};

/// Default Codex command (SPEC §5.3.6 / §10.1).
pub const DEFAULT_CODEX_COMMAND: &str = "codex app-server";

/// Configuration controlling how [`CodexRunner`] launches Codex and
/// names its protocol methods.
///
/// `method_*` fields are exposed because Codex's app-server has churned
/// method names across releases (`newConversation` ↔ `addConversation`,
/// `sendUserTurn` ↔ `userInput`). Defaulting them in config rather than
/// hard-coding lets a deployment opt into the names of its installed
/// Codex without code changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexConfig {
    /// Shell command to launch. Defaults to `"codex app-server"`. The
    /// runner always invokes this via `bash -lc` per SPEC §10.1.
    #[serde(default = "default_command")]
    pub command: String,

    /// JSON-RPC method to call for the initial handshake.
    #[serde(default = "default_method_initialize")]
    pub method_initialize: String,

    /// JSON-RPC method to open a new conversation/thread.
    #[serde(default = "default_method_new_conversation")]
    pub method_new_conversation: String,

    /// JSON-RPC method to send a user turn (initial prompt or
    /// continuation guidance).
    #[serde(default = "default_method_send_user_turn")]
    pub method_send_user_turn: String,

    /// JSON-RPC method to interrupt the current turn (used by
    /// [`AgentControl::abort`]).
    #[serde(default = "default_method_interrupt")]
    pub method_interrupt: String,

    /// Optional model name to advertise on `newConversation`. Codex
    /// uses the server-side default when this is `None`.
    #[serde(default)]
    pub model: Option<String>,

    /// Pass-through Codex `approval_policy` value (SPEC §5.3.6).
    #[serde(default)]
    pub approval_policy: Option<Value>,

    /// Pass-through Codex `thread_sandbox` value (SPEC §5.3.6).
    #[serde(default)]
    pub thread_sandbox: Option<Value>,

    /// Pass-through Codex `turn_sandbox_policy` value (SPEC §5.3.6).
    #[serde(default)]
    pub turn_sandbox_policy: Option<Value>,
}

fn default_command() -> String {
    DEFAULT_CODEX_COMMAND.to_string()
}
fn default_method_initialize() -> String {
    "initialize".to_string()
}
fn default_method_new_conversation() -> String {
    "newConversation".to_string()
}
fn default_method_send_user_turn() -> String {
    "sendUserTurn".to_string()
}
fn default_method_interrupt() -> String {
    "interruptConversation".to_string()
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            command: default_command(),
            method_initialize: default_method_initialize(),
            method_new_conversation: default_method_new_conversation(),
            method_send_user_turn: default_method_send_user_turn(),
            method_interrupt: default_method_interrupt(),
            model: None,
            approval_policy: None,
            thread_sandbox: None,
            turn_sandbox_policy: None,
        }
    }
}

/// `AgentRunner` implementation backed by `codex app-server`.
///
/// Stateless across sessions — every `start_session` call spawns a
/// fresh subprocess. Per-session state is owned by the
/// [`CodexAgentControl`] returned alongside the event stream.
#[derive(Debug, Clone, Default)]
pub struct CodexRunner {
    config: CodexConfig,
}

impl CodexRunner {
    /// Build a runner with the given config.
    pub fn new(config: CodexConfig) -> Self {
        Self { config }
    }

    /// Borrow the configuration (used by tests).
    pub fn config(&self) -> &CodexConfig {
        &self.config
    }
}

#[async_trait]
impl AgentRunner for CodexRunner {
    async fn start_session(&self, params: StartSessionParams) -> AgentResult<AgentSession> {
        spawn_session(&self.config, params).await
    }
}

// ---------------------------------------------------------------------------
// Subprocess wiring
// ---------------------------------------------------------------------------

/// Spawn the Codex subprocess and wire up its I/O.
///
/// Errors here surface as `AgentError::Spawn` — they all happen before
/// the event stream exists, so the orchestrator never sees them in-band.
async fn spawn_session(
    config: &CodexConfig,
    params: StartSessionParams,
) -> AgentResult<AgentSession> {
    validate_workspace(&params.workspace)?;

    let mut child = Command::new("bash")
        .arg("-lc")
        .arg(&config.command)
        .current_dir(&params.workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| AgentError::Spawn(format!("spawn `{}`: {e}", config.command)))?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| AgentError::Spawn("child stdin not captured".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AgentError::Spawn("child stdout not captured".into()))?;

    let (event_tx, event_rx) = mpsc::channel::<AgentEvent>(64);
    let next_request_id = Arc::new(AtomicU64::new(1));
    let stdin = Arc::new(Mutex::new(stdin));

    // Send the initial bootstrap requests synchronously so any startup
    // failure surfaces as `AgentError::Spawn` before we hand back the
    // stream.
    bootstrap_session(config, &stdin, &next_request_id, &params).await?;

    // The reader task owns stdout and forwards normalized AgentEvents
    // to the orchestrator's stream.
    let reader_handle = tokio::spawn(reader_task(
        stdout,
        event_tx.clone(),
        params.issue.clone(),
        stdin.clone(),
        next_request_id.clone(),
    ));

    let initial_session = SessionId::from_raw("pending"); // replaced by Started events
    let control = CodexAgentControl {
        config: config.clone(),
        stdin,
        next_request_id,
        child: Arc::new(Mutex::new(Some(child))),
        reader_handle: Arc::new(Mutex::new(Some(reader_handle))),
    };

    Ok(AgentSession {
        initial_session,
        events: stream_from_receiver(event_rx),
        control: Box::new(control),
    })
}

/// Reject obviously-invalid workspaces up front so the user gets a
/// clean `AgentError::Spawn` rather than a confusing exec failure.
fn validate_workspace(workspace: &Path) -> AgentResult<()> {
    if !workspace.is_absolute() {
        return Err(AgentError::Spawn(format!(
            "workspace must be absolute, got {}",
            workspace.display()
        )));
    }
    if !workspace.exists() {
        return Err(AgentError::Spawn(format!(
            "workspace does not exist: {}",
            workspace.display()
        )));
    }
    Ok(())
}

/// Send `initialize` + `newConversation` + the first user turn, in that
/// order. We don't await replies here — replies flow through the reader
/// task — but we do log them at debug level when they arrive.
async fn bootstrap_session(
    config: &CodexConfig,
    stdin: &Arc<Mutex<ChildStdin>>,
    next_id: &Arc<AtomicU64>,
    params: &StartSessionParams,
) -> AgentResult<()> {
    let init = build_initialize(config, next_id);
    let new_conv = build_new_conversation(config, &params.workspace, next_id);
    let first_turn = build_send_user_turn(
        config,
        &params.initial_prompt,
        params.issue_label.as_deref(),
        next_id,
    );

    let mut guard = stdin.lock().await;
    write_frame(&mut guard, &init).await?;
    write_frame(&mut guard, &new_conv).await?;
    write_frame(&mut guard, &first_turn).await?;
    Ok(())
}

/// Serialise + write a single JSON-RPC frame, terminated by `\n`.
async fn write_frame(stdin: &mut ChildStdin, frame: &Value) -> AgentResult<()> {
    let mut bytes = serde_json::to_vec(frame)
        .map_err(|e| AgentError::Protocol(format!("encode frame: {e}")))?;
    bytes.push(b'\n');
    stdin
        .write_all(&bytes)
        .await
        .map_err(|e| AgentError::Spawn(format!("write to codex stdin: {e}")))?;
    stdin
        .flush()
        .await
        .map_err(|e| AgentError::Spawn(format!("flush codex stdin: {e}")))?;
    Ok(())
}

/// Build the `initialize` JSON-RPC request.
pub(crate) fn build_initialize(config: &CodexConfig, next_id: &Arc<AtomicU64>) -> Value {
    let id = next_id.fetch_add(1, Ordering::Relaxed);
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": &config.method_initialize,
        "params": {
            "clientName": "symphony-rs",
            "clientVersion": env!("CARGO_PKG_VERSION"),
        }
    })
}

/// Build the `newConversation` JSON-RPC request, splicing in the
/// configured pass-through Codex policy fields.
pub(crate) fn build_new_conversation(
    config: &CodexConfig,
    workspace: &Path,
    next_id: &Arc<AtomicU64>,
) -> Value {
    let id = next_id.fetch_add(1, Ordering::Relaxed);
    let mut params = serde_json::Map::new();
    params.insert("cwd".into(), json!(workspace.to_string_lossy()));
    if let Some(ref m) = config.model {
        params.insert("model".into(), json!(m));
    }
    if let Some(ref v) = config.approval_policy {
        params.insert("approvalPolicy".into(), v.clone());
    }
    if let Some(ref v) = config.thread_sandbox {
        params.insert("sandbox".into(), v.clone());
    }
    if let Some(ref v) = config.turn_sandbox_policy {
        params.insert("turnSandboxPolicy".into(), v.clone());
    }
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": &config.method_new_conversation,
        "params": Value::Object(params),
    })
}

/// Build a `sendUserTurn` JSON-RPC request carrying the prompt body.
pub(crate) fn build_send_user_turn(
    config: &CodexConfig,
    prompt: &str,
    title: Option<&str>,
    next_id: &Arc<AtomicU64>,
) -> Value {
    let id = next_id.fetch_add(1, Ordering::Relaxed);
    let mut params = serde_json::Map::new();
    params.insert("input".into(), json!(prompt));
    if let Some(t) = title {
        params.insert("title".into(), json!(t));
    }
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": &config.method_send_user_turn,
        "params": Value::Object(params),
    })
}

/// Build an `interruptConversation` JSON-RPC request.
pub(crate) fn build_interrupt(config: &CodexConfig, next_id: &Arc<AtomicU64>) -> Value {
    let id = next_id.fetch_add(1, Ordering::Relaxed);
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": &config.method_interrupt,
        "params": {},
    })
}

/// Build a JSON-RPC error reply for an unhandled server-initiated
/// request. SPEC §10.5: never stall the session on an unsupported
/// request; reply with a clean error and let the agent continue.
pub(crate) fn build_error_reply(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message},
    })
}

/// Reader task: pumps stdout lines through the decoder, translates each
/// `codex/event` into an [`AgentEvent`], and replies to server-initiated
/// requests via the shared stdin handle.
async fn reader_task(
    stdout: ChildStdout,
    tx: mpsc::Sender<AgentEvent>,
    issue: symphony_core::tracker::IssueId,
    stdin: Arc<Mutex<ChildStdin>>,
    next_id: Arc<AtomicU64>,
) {
    let mut state = TranslateState::new(issue);
    let mut lines = BufReader::new(stdout).lines();

    loop {
        match lines.next_line().await {
            Ok(Some(line)) if line.trim().is_empty() => continue,
            Ok(Some(line)) => match decode_line(&line) {
                Ok(JsonRpcFrame::Notification { method, params }) if method == "codex/event" => {
                    match decode_event_params(&params) {
                        Ok(env) => {
                            for ev in state.translate(env) {
                                if tx.send(ev).await.is_err() {
                                    return;
                                }
                            }
                        }
                        Err(e) => {
                            warn!(error = %e, "codex/event decode failed");
                        }
                    }
                }
                Ok(JsonRpcFrame::Notification { method, .. }) => {
                    debug!(method, "ignoring non-codex notification");
                }
                Ok(JsonRpcFrame::Request { id, method, .. }) => {
                    // SPEC §10.5: high-trust default — refuse unknown
                    // server-initiated requests rather than block.
                    let reply =
                        build_error_reply(id, -32601, &format!("unsupported method: {method}"));
                    let mut guard = stdin.lock().await;
                    if let Err(e) = write_frame(&mut guard, &reply).await {
                        warn!(error = %e, "failed to reply to server request");
                    }
                }
                Ok(JsonRpcFrame::Response { id, outcome }) => {
                    debug!(?id, ?outcome, "response from codex");
                }
                Err(e) => {
                    warn!(error = %e, line = %line, "malformed codex frame");
                }
            },
            Ok(None) => {
                // EOF — emit Completed{SubprocessExit} unless we already
                // sent a terminal event.
                if !state.terminal_sent {
                    let session = state.current_session();
                    let _ = tx
                        .send(AgentEvent::Completed {
                            session,
                            reason: CompletionReason::SubprocessExit,
                        })
                        .await;
                }
                let _ = next_id; // suppress unused-var warning when no requests are sent
                return;
            }
            Err(e) => {
                warn!(error = %e, "stdout read failed");
                let session = state.current_session();
                let _ = tx
                    .send(AgentEvent::Failed {
                        session,
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

/// Per-session translation state.
///
/// Tracks the current `<thread, turn>` pair so every emitted
/// [`AgentEvent`] carries a [`SessionId`] that matches the SPEC §4.2
/// `<thread>-<turn>` shape, and remembers cumulative tokens so we can
/// emit a sensible final [`AgentEvent::TokenUsage`] even if Codex only
/// reports running totals.
#[derive(Debug)]
pub(crate) struct TranslateState {
    issue: symphony_core::tracker::IssueId,
    thread: Option<ThreadId>,
    turn: Option<TurnId>,
    tokens: TokenUsage,
    /// True once we've emitted a Started event for the current turn so
    /// later turns know to emit a fresh one.
    started_emitted: bool,
    /// True once a terminal event has been sent for the current turn.
    pub(crate) terminal_sent: bool,
}

impl TranslateState {
    pub(crate) fn new(issue: symphony_core::tracker::IssueId) -> Self {
        Self {
            issue,
            thread: None,
            turn: None,
            tokens: TokenUsage::default(),
            started_emitted: false,
            terminal_sent: false,
        }
    }

    /// Best-effort SessionId reflecting current state. Used for
    /// post-EOF terminal events when we haven't seen a `task_started`.
    fn current_session(&self) -> SessionId {
        match (&self.thread, &self.turn) {
            (Some(t), Some(u)) => SessionId::new(t, u),
            _ => SessionId::from_raw("unknown"),
        }
    }

    /// Translate one Codex event into zero, one, or two normalized
    /// [`AgentEvent`]s. (`task_started` produces a `Started` *and* may
    /// be paired with a follow-up usage emission on `task_complete`.)
    pub(crate) fn translate(&mut self, env: CodexEventEnvelope) -> Vec<AgentEvent> {
        match env.msg {
            CodexMsg::SessionConfigured(SessionConfiguredPayload { session_id, .. }) => {
                self.thread = Some(ThreadId::new(session_id));
                Vec::new()
            }
            CodexMsg::TaskStarted(TaskStartedPayload { turn_id }) => {
                self.turn = Some(TurnId::new(turn_id));
                self.tokens = TokenUsage::default();
                self.started_emitted = true;
                self.terminal_sent = false;
                let session = self.current_session();
                vec![AgentEvent::Started {
                    session,
                    issue: self.issue.clone(),
                }]
            }
            CodexMsg::AgentMessage(AgentMessagePayload { message }) => {
                vec![AgentEvent::Message {
                    session: self.current_session(),
                    role: "assistant".into(),
                    content: message,
                }]
            }
            CodexMsg::AgentMessageDelta(_) | CodexMsg::AgentReasoningDelta(_) => {
                // Observability-only: streaming deltas don't appear in
                // the normalized event surface.
                Vec::new()
            }
            CodexMsg::McpToolCallBegin(McpToolCallPayload {
                tool, arguments, ..
            }) => {
                vec![AgentEvent::ToolUse {
                    session: self.current_session(),
                    tool,
                    input: arguments,
                }]
            }
            CodexMsg::ExecCommandBegin(ExecCommandPayload { command, .. }) => {
                vec![AgentEvent::ToolUse {
                    session: self.current_session(),
                    tool: "exec".into(),
                    input: json!({"command": command}),
                }]
            }
            CodexMsg::TokenCount(p) => {
                self.tokens = merge_tokens(&self.tokens, &p);
                vec![AgentEvent::TokenUsage {
                    session: self.current_session(),
                    usage: self.tokens.clone(),
                }]
            }
            CodexMsg::TaskComplete(_) => {
                self.terminal_sent = true;
                vec![AgentEvent::Completed {
                    session: self.current_session(),
                    reason: CompletionReason::Success,
                }]
            }
            CodexMsg::Error(ErrorPayload { message }) => {
                self.terminal_sent = true;
                vec![AgentEvent::Failed {
                    session: self.current_session(),
                    error: message,
                }]
            }
            CodexMsg::TurnAborted(_) => {
                self.terminal_sent = true;
                vec![AgentEvent::Completed {
                    session: self.current_session(),
                    reason: CompletionReason::Cancelled,
                }]
            }
            CodexMsg::McpToolCallEnd(_) | CodexMsg::ExecCommandEnd(_) | CodexMsg::Other => {
                Vec::new()
            }
        }
    }
}

/// Merge a Codex running token count into our cumulative snapshot.
///
/// Codex generally reports running totals so a naive `=` would be
/// correct — but some app-server versions emit deltas. The "max" here
/// is defensive: it works whether the server sends totals (idempotent)
/// or deltas (we'd undercount, but not double-count).
fn merge_tokens(prev: &TokenUsage, p: &TokenCountPayload) -> TokenUsage {
    let input = prev.input.max(p.input_tokens);
    let output = prev.output.max(p.output_tokens);
    let cached = prev.cached_input.max(p.cached_input_tokens);
    let total = if p.total_tokens > 0 {
        p.total_tokens.max(prev.total)
    } else {
        input + output
    };
    TokenUsage {
        input,
        output,
        cached_input: cached,
        total,
    }
}

// ---------------------------------------------------------------------------
// Stream wiring
// ---------------------------------------------------------------------------

/// Adapt an `mpsc::Receiver<AgentEvent>` into the boxed stream the trait
/// expects.
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

/// `AgentControl` impl returned from `CodexRunner::start_session`.
#[derive(Clone)]
pub struct CodexAgentControl {
    config: CodexConfig,
    stdin: Arc<Mutex<ChildStdin>>,
    next_request_id: Arc<AtomicU64>,
    child: Arc<Mutex<Option<Child>>>,
    reader_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

#[async_trait]
impl AgentControl for CodexAgentControl {
    async fn continue_turn(&self, guidance: &str) -> AgentResult<()> {
        let frame = build_send_user_turn(&self.config, guidance, None, &self.next_request_id);
        let mut guard = self.stdin.lock().await;
        write_frame(&mut guard, &frame).await
    }

    async fn abort(&self) -> AgentResult<()> {
        // 1. Ask Codex to interrupt politely.
        let frame = build_interrupt(&self.config, &self.next_request_id);
        {
            let mut guard = self.stdin.lock().await;
            // If stdin is already closed the child is gone — that's a
            // valid no-op, not an error.
            let _ = write_frame(&mut guard, &frame).await;
        }
        // 2. Kill the child if it's still around. `kill_on_drop` is
        // belt-and-suspenders; doing it eagerly here unblocks the
        // reader task so the stream closes promptly.
        let mut child_slot = self.child.lock().await;
        if let Some(mut child) = child_slot.take() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        // 3. Drop the reader handle — reader will exit on EOF.
        let mut handle_slot = self.reader_handle.lock().await;
        if let Some(h) = handle_slot.take() {
            h.abort();
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Spawn-error type (used by tests)
// ---------------------------------------------------------------------------

/// Errors specific to the runner that wrap [`AgentError`] for symmetry
/// with other crates' error types. Currently unused by callers but kept
/// public so a future operator-facing CLI can pattern-match.
#[derive(Debug, Error)]
pub enum CodexRunnerError {
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

    fn id_counter() -> Arc<AtomicU64> {
        Arc::new(AtomicU64::new(1))
    }

    #[test]
    fn build_initialize_emits_jsonrpc_v2_with_unique_ids() {
        let cfg = CodexConfig::default();
        let counter = id_counter();
        let a = build_initialize(&cfg, &counter);
        let b = build_initialize(&cfg, &counter);
        assert_eq!(a["jsonrpc"], json!("2.0"));
        assert_eq!(a["method"], json!("initialize"));
        assert_eq!(a["params"]["clientName"], json!("symphony-rs"));
        assert_ne!(a["id"], b["id"], "ids must be monotonic");
    }

    #[test]
    fn build_new_conversation_passes_through_config_fields() {
        let cfg = CodexConfig {
            model: Some("gpt-5".into()),
            approval_policy: Some(json!("never")),
            thread_sandbox: Some(json!("read-only")),
            turn_sandbox_policy: Some(json!({"network": false})),
            ..Default::default()
        };
        let counter = id_counter();
        let frame = build_new_conversation(&cfg, &PathBuf::from("/work/X-1"), &counter);
        assert_eq!(frame["method"], json!("newConversation"));
        assert_eq!(frame["params"]["cwd"], json!("/work/X-1"));
        assert_eq!(frame["params"]["model"], json!("gpt-5"));
        assert_eq!(frame["params"]["approvalPolicy"], json!("never"));
        assert_eq!(frame["params"]["sandbox"], json!("read-only"));
        assert_eq!(
            frame["params"]["turnSandboxPolicy"]["network"],
            json!(false)
        );
    }

    #[test]
    fn build_send_user_turn_includes_optional_title() {
        let cfg = CodexConfig::default();
        let counter = id_counter();
        let with_title =
            build_send_user_turn(&cfg, "do the thing", Some("X-1: do the thing"), &counter);
        let without_title = build_send_user_turn(&cfg, "do the thing", None, &counter);
        assert_eq!(with_title["params"]["title"], json!("X-1: do the thing"));
        assert!(without_title["params"].get("title").is_none());
    }

    #[test]
    fn build_error_reply_echoes_id_and_code() {
        let frame = build_error_reply(json!(7), -32601, "no");
        assert_eq!(frame["id"], json!(7));
        assert_eq!(frame["error"]["code"], json!(-32601));
        assert_eq!(frame["error"]["message"], json!("no"));
    }

    #[test]
    fn translate_session_then_task_then_message_then_complete() {
        let mut state = TranslateState::new(IssueId::new("X-1"));
        // session_configured → no event, but thread is now known
        let none = state.translate(parse(
            json!({"id":"e0","msg":{"type":"session_configured","session_id":"th_42"}}),
        ));
        assert!(none.is_empty());

        let started = state.translate(parse(
            json!({"id":"e1","msg":{"type":"task_started","turn_id":"tn_7"}}),
        ));
        assert_eq!(started.len(), 1);
        match &started[0] {
            AgentEvent::Started { session, issue } => {
                assert_eq!(session.as_str(), "th_42-tn_7");
                assert_eq!(issue.as_str(), "X-1");
            }
            other => panic!("expected Started, got {other:?}"),
        }

        let msg = state.translate(parse(
            json!({"id":"e2","msg":{"type":"agent_message","message":"hi"}}),
        ));
        assert!(matches!(
            &msg[0],
            AgentEvent::Message { role, content, .. } if role == "assistant" && content == "hi"
        ));

        let done = state.translate(parse(json!({"id":"e3","msg":{"type":"task_complete"}})));
        assert!(matches!(
            &done[0],
            AgentEvent::Completed { reason, .. } if *reason == CompletionReason::Success
        ));
        assert!(state.terminal_sent);
    }

    #[test]
    fn translate_token_count_emits_usage_with_total_fallback() {
        let mut state = TranslateState::new(IssueId::new("X-1"));
        state.translate(parse(
            json!({"id":"e0","msg":{"type":"session_configured","session_id":"th"}}),
        ));
        state.translate(parse(
            json!({"id":"e1","msg":{"type":"task_started","turn_id":"tn"}}),
        ));
        let out = state.translate(parse(json!({
            "id":"e2",
            "msg":{"type":"token_count","input_tokens":40,"output_tokens":60}
        })));
        match &out[0] {
            AgentEvent::TokenUsage { usage, .. } => {
                assert_eq!(usage.input, 40);
                assert_eq!(usage.output, 60);
                // total field absent in payload → fallback to input+output
                assert_eq!(usage.total, 100);
            }
            other => panic!("expected TokenUsage, got {other:?}"),
        }
    }

    #[test]
    fn translate_error_emits_failed_and_marks_terminal() {
        let mut state = TranslateState::new(IssueId::new("X-1"));
        state.translate(parse(
            json!({"id":"e0","msg":{"type":"session_configured","session_id":"th"}}),
        ));
        state.translate(parse(
            json!({"id":"e1","msg":{"type":"task_started","turn_id":"tn"}}),
        ));
        let out = state.translate(parse(
            json!({"id":"e2","msg":{"type":"error","message":"boom"}}),
        ));
        assert!(matches!(&out[0], AgentEvent::Failed { error, .. } if error == "boom"));
        assert!(state.terminal_sent);
    }

    #[test]
    fn translate_turn_aborted_yields_cancelled_completion() {
        let mut state = TranslateState::new(IssueId::new("X-1"));
        state.translate(parse(
            json!({"id":"e0","msg":{"type":"session_configured","session_id":"th"}}),
        ));
        state.translate(parse(
            json!({"id":"e1","msg":{"type":"task_started","turn_id":"tn"}}),
        ));
        let out = state.translate(parse(json!({"id":"e2","msg":{"type":"turn_aborted"}})));
        assert!(matches!(
            &out[0],
            AgentEvent::Completed { reason, .. } if *reason == CompletionReason::Cancelled
        ));
    }

    #[test]
    fn translate_exec_begin_normalises_to_tooluse() {
        let mut state = TranslateState::new(IssueId::new("X-1"));
        state.translate(parse(
            json!({"id":"e0","msg":{"type":"session_configured","session_id":"th"}}),
        ));
        state.translate(parse(
            json!({"id":"e1","msg":{"type":"task_started","turn_id":"tn"}}),
        ));
        let out = state.translate(parse(json!({
            "id":"e2",
            "msg":{"type":"exec_command_begin","call_id":"c1","command":["ls","-la"]}
        })));
        match &out[0] {
            AgentEvent::ToolUse { tool, input, .. } => {
                assert_eq!(tool, "exec");
                assert_eq!(input["command"], json!(["ls", "-la"]));
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn translate_message_deltas_are_silent() {
        let mut state = TranslateState::new(IssueId::new("X-1"));
        state.translate(parse(
            json!({"id":"e0","msg":{"type":"session_configured","session_id":"th"}}),
        ));
        state.translate(parse(
            json!({"id":"e1","msg":{"type":"task_started","turn_id":"tn"}}),
        ));
        let out = state.translate(parse(
            json!({"id":"e2","msg":{"type":"agent_message_delta","delta":"par"}}),
        ));
        assert!(out.is_empty());
    }

    #[test]
    fn translate_unknown_msg_is_silent_not_an_error() {
        let mut state = TranslateState::new(IssueId::new("X-1"));
        let out = state.translate(parse(
            json!({"id":"e0","msg":{"type":"future_unknown_event","weird":1}}),
        ));
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn validate_workspace_rejects_relative_paths() {
        let res = validate_workspace(&PathBuf::from("relative/path"));
        assert!(matches!(res, Err(AgentError::Spawn(_))));
    }

    #[tokio::test]
    async fn validate_workspace_rejects_missing_dir() {
        let res = validate_workspace(&PathBuf::from("/this/does/not/exist/symphony-test"));
        assert!(matches!(res, Err(AgentError::Spawn(_))));
    }

    fn parse(v: Value) -> CodexEventEnvelope {
        decode_event_params(&v).unwrap()
    }
}
