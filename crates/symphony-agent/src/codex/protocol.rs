//! Codex app-server JSON-RPC protocol decoder.
//!
//! The Codex app-server (`codex app-server`) speaks JSON-RPC 2.0 over
//! NDJSON on stdio. Each frame is one JSON object terminated by `\n`.
//! Three frame shapes appear:
//!
//! 1. **Request** — client → server. Carries `id`, `method`, `params`.
//! 2. **Response** — server → client. Carries the same `id` plus `result`
//!    or `error`. We send these as replies to server-initiated requests
//!    (e.g. approval prompts when not auto-approved).
//! 3. **Notification** — server → client. Carries `method` + `params`
//!    but no `id`. The bulk of the streaming protocol comes through here:
//!    the server emits `method = "codex/event"` notifications whose
//!    `params` envelope a typed `msg` payload describing what happened
//!    inside the agent's turn.
//!
//! # Stability stance
//!
//! SPEC §10 is explicit: the targeted Codex app-server version is the
//! source of truth for protocol shapes, and Symphony adapters must not
//! schema-lock too tightly. We implement that by:
//!
//! - Modelling the JSON-RPC envelope strictly (`jsonrpc`, `id`, `method`).
//! - Modelling the *known* `msg.type` values explicitly so callers can
//!   pattern-match without ad-hoc string compares.
//! - Surfacing everything else through an `Other { kind, raw }` variant
//!   that preserves the verbatim JSON. A future Codex release adding a
//!   new event type lands in `Other` without breaking this crate.
//!
//! The translation from [`CodexMsg`] to [`symphony_core::agent::AgentEvent`]
//! lives in [`crate::codex::runner`] so this module stays a pure decoder
//! with no async or runtime dependencies.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// Error decoding a Codex JSONL line.
#[derive(Debug, Error)]
pub enum DecodeError {
    /// Line wasn't valid JSON.
    #[error("codex protocol: invalid JSON: {0}")]
    InvalidJson(#[from] serde_json::Error),

    /// JSON parsed but didn't match a JSON-RPC 2.0 frame shape.
    #[error("codex protocol: not a JSON-RPC frame: {0}")]
    NotJsonRpc(String),
}

/// A single decoded JSON-RPC frame from the Codex app-server.
///
/// We split rather than collapse the three shapes because a runner
/// handles them on different code paths: notifications fan into the
/// event stream, responses unblock pending request futures, and incoming
/// requests typically need a synchronous reply (approvals, tool calls).
#[derive(Debug, Clone, PartialEq)]
pub enum JsonRpcFrame {
    /// Server-initiated request. The runner must reply with a Response
    /// using the same `id`. Symphony's high-trust default is to
    /// auto-approve known approval methods; everything else returns a
    /// JSON-RPC error so the session doesn't stall (SPEC §10.5).
    Request {
        /// Request id. Either an integer or a string per the JSON-RPC
        /// spec — we keep it as `Value` so we can echo it back verbatim
        /// on the reply.
        id: Value,
        /// JSON-RPC method name.
        method: String,
        /// Method params (any JSON shape).
        params: Value,
    },
    /// Reply to a client-initiated request.
    Response {
        /// Echoed id from the originating request.
        id: Value,
        /// Either the success result or the error payload.
        outcome: ResponseOutcome,
    },
    /// Server notification (no `id`). The Codex stream is mostly this.
    Notification {
        /// Notification method, e.g. `"codex/event"` for the streaming
        /// turn events; rarely something else (e.g. log notifications).
        method: String,
        /// Notification params.
        params: Value,
    },
}

/// Either a successful JSON-RPC result or a JSON-RPC error object.
#[derive(Debug, Clone, PartialEq)]
pub enum ResponseOutcome {
    /// Success payload.
    Result(Value),
    /// Error payload — preserved verbatim because Codex error payloads
    /// carry implementation-specific fields beyond the JSON-RPC spec.
    Error(JsonRpcError),
}

/// JSON-RPC 2.0 error object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcError {
    /// Numeric error code (negative integers are reserved by the spec).
    pub code: i64,
    /// Human-readable message.
    pub message: String,
    /// Optional structured payload.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub data: Value,
}

/// Decode a single NDJSON line into a [`JsonRpcFrame`].
///
/// This is the public entry point — runners pipe stdout lines through
/// here. Trailing newlines are tolerated; blank lines should be filtered
/// upstream (they aren't valid JSON-RPC frames).
pub fn decode_line(line: &str) -> Result<JsonRpcFrame, DecodeError> {
    let raw: Value = serde_json::from_str(line.trim_end())?;
    decode_value(raw)
}

/// Decode an already-parsed JSON value. Exposed so tests can pass
/// constructed values without re-serialising.
pub fn decode_value(raw: Value) -> Result<JsonRpcFrame, DecodeError> {
    let obj = raw
        .as_object()
        .ok_or_else(|| DecodeError::NotJsonRpc("frame is not a JSON object".into()))?;

    let has_method = obj.contains_key("method");
    let has_id = obj.contains_key("id");
    let has_result = obj.contains_key("result");
    let has_error = obj.contains_key("error");

    if has_method && has_id {
        // Request from server.
        let id = obj.get("id").cloned().unwrap_or(Value::Null);
        let method = obj
            .get("method")
            .and_then(Value::as_str)
            .ok_or_else(|| DecodeError::NotJsonRpc("method is not a string".into()))?
            .to_string();
        let params = obj.get("params").cloned().unwrap_or(Value::Null);
        Ok(JsonRpcFrame::Request { id, method, params })
    } else if has_method {
        // Notification (no id).
        let method = obj
            .get("method")
            .and_then(Value::as_str)
            .ok_or_else(|| DecodeError::NotJsonRpc("method is not a string".into()))?
            .to_string();
        let params = obj.get("params").cloned().unwrap_or(Value::Null);
        Ok(JsonRpcFrame::Notification { method, params })
    } else if has_id && (has_result || has_error) {
        let id = obj.get("id").cloned().unwrap_or(Value::Null);
        let outcome = if has_error {
            let err: JsonRpcError = serde_json::from_value(obj["error"].clone())?;
            ResponseOutcome::Error(err)
        } else {
            ResponseOutcome::Result(obj["result"].clone())
        };
        Ok(JsonRpcFrame::Response { id, outcome })
    } else {
        Err(DecodeError::NotJsonRpc(
            "frame has neither method nor result/error".into(),
        ))
    }
}

// ---------------------------------------------------------------------------
// codex/event payloads
// ---------------------------------------------------------------------------

/// Envelope of the `codex/event` notification.
///
/// Codex carries every streaming turn update inside this envelope:
/// `params = { "id": <event_id>, "msg": { "type": "...", ... } }`.
/// `id` is unrelated to the JSON-RPC request id — it's the monotonic
/// per-event sequence Codex assigns for replay/debug purposes.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CodexEventEnvelope {
    /// Codex-internal event sequence id. Logged for diagnostics; not
    /// load-bearing for orchestration.
    #[serde(default)]
    pub id: Option<String>,
    /// The typed payload describing what happened.
    pub msg: CodexMsg,
}

/// Typed Codex event payload.
///
/// We model the variants SPEC §10.4 enumerates plus the
/// session/turn-identity events the runner needs to construct
/// [`SessionId`](symphony_core::agent::SessionId)s. Anything we don't
/// recognise lands in [`CodexMsg::Other`] with the raw JSON intact so
/// the runner can ignore it without losing the data.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CodexMsg {
    /// Session has started; payload carries the `thread_id` (sometimes
    /// called `conversation_id` in older app-server builds).
    SessionConfigured(SessionConfiguredPayload),

    /// A new turn began on the live thread. Carries the `turn_id`.
    TaskStarted(TaskStartedPayload),

    /// Final assistant message text for the turn (post-streaming).
    /// Codex emits incremental `agent_message_delta` notifications; we
    /// surface the consolidated `agent_message` on completion.
    AgentMessage(AgentMessagePayload),

    /// Streaming delta of an assistant message. Adapters MAY accumulate
    /// these but Symphony only emits them on completion via
    /// `agent_message`; deltas land here for callers that want them.
    AgentMessageDelta(AgentMessageDeltaPayload),

    /// Reasoning/thinking content delta. Observability only.
    AgentReasoningDelta(AgentReasoningDeltaPayload),

    /// Agent invoked an MCP tool.
    McpToolCallBegin(McpToolCallPayload),

    /// MCP tool finished.
    McpToolCallEnd(McpToolCallEndPayload),

    /// Agent invoked a shell exec.
    ExecCommandBegin(ExecCommandPayload),

    /// Shell exec finished.
    ExecCommandEnd(ExecCommandEndPayload),

    /// Token-usage snapshot. Codex reports running totals.
    TokenCount(TokenCountPayload),

    /// Turn ended successfully.
    TaskComplete(TaskCompletePayload),

    /// Turn ended in failure.
    Error(ErrorPayload),

    /// Turn was cancelled (operator interrupt).
    TurnAborted(TurnAbortedPayload),

    /// Anything else. Serde's `#[serde(other)]` requires a unit variant
    /// here; the runner that wants the raw JSON should retain the
    /// original `Value` upstream of decoding (see
    /// [`decode_event_params`] callers).
    #[serde(other)]
    Other,
}

/// `session_configured` payload.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SessionConfiguredPayload {
    /// Codex thread / conversation identifier. Field is named
    /// `session_id` in some app-server versions and `thread_id` in
    /// others; we accept both.
    #[serde(alias = "thread_id", alias = "conversation_id")]
    pub session_id: String,
    /// Optional model name reported back by the server.
    #[serde(default)]
    pub model: Option<String>,
}

/// `task_started` payload.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct TaskStartedPayload {
    /// Per-turn identifier. Combined with the thread id from
    /// `session_configured` to form a `SessionId`.
    #[serde(alias = "task_id", alias = "submission_id")]
    pub turn_id: String,
}

/// `agent_message` payload.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct AgentMessagePayload {
    /// Final consolidated text from the assistant for this turn.
    pub message: String,
}

/// `agent_message_delta` payload.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct AgentMessageDeltaPayload {
    /// Incremental token chunk.
    pub delta: String,
}

/// `agent_reasoning_delta` payload.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct AgentReasoningDeltaPayload {
    /// Incremental reasoning chunk.
    pub delta: String,
}

/// `mcp_tool_call_begin` payload.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct McpToolCallPayload {
    /// Tool invocation id (matches the corresponding `_end` event).
    pub call_id: String,
    /// MCP server name owning the tool.
    pub server: String,
    /// Tool name.
    pub tool: String,
    /// Verbatim input JSON.
    #[serde(default)]
    pub arguments: Value,
}

/// `mcp_tool_call_end` payload.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct McpToolCallEndPayload {
    /// Matching call id from the `_begin` event.
    pub call_id: String,
    /// Tool result (any JSON shape).
    #[serde(default)]
    pub result: Value,
}

/// `exec_command_begin` payload.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ExecCommandPayload {
    /// Codex per-exec identifier.
    pub call_id: String,
    /// argv as Codex saw it.
    pub command: Vec<String>,
    /// Working directory for the exec.
    #[serde(default)]
    pub cwd: Option<String>,
}

/// `exec_command_end` payload.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ExecCommandEndPayload {
    /// Matching call id from the `_begin` event.
    pub call_id: String,
    /// Exit status, if the process was actually run.
    #[serde(default)]
    pub exit_code: Option<i32>,
}

/// `token_count` payload. Codex reports cumulative tokens for the turn.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct TokenCountPayload {
    /// Prompt tokens consumed so far.
    #[serde(default, alias = "input_tokens")]
    pub input_tokens: u64,
    /// Completion tokens emitted so far.
    #[serde(default, alias = "output_tokens")]
    pub output_tokens: u64,
    /// Subset of input tokens served from prompt cache, if reported.
    #[serde(default, alias = "cached_input_tokens")]
    pub cached_input_tokens: u64,
    /// Backend-reported total. Some Codex versions report this; others
    /// expect the consumer to add input + output. Default zero means
    /// "not reported"; the adapter will fall back to the sum.
    #[serde(default, alias = "total_tokens")]
    pub total_tokens: u64,
}

/// `task_complete` payload.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct TaskCompletePayload {
    /// Final last_message, if Codex chose to repeat it here. Optional
    /// because many Codex versions only carry it on `agent_message`.
    #[serde(default)]
    pub last_agent_message: Option<String>,
}

/// `error` payload.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ErrorPayload {
    /// Human-readable error message.
    pub message: String,
}

/// `turn_aborted` payload.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct TurnAbortedPayload {
    /// Optional reason string. Treated as observability only.
    #[serde(default)]
    pub reason: Option<String>,
}

/// Decode a `codex/event` notification's `params` JSON into the typed
/// envelope. The runner calls this for every notification whose method
/// is `"codex/event"`.
pub fn decode_event_params(params: &Value) -> Result<CodexEventEnvelope, DecodeError> {
    Ok(serde_json::from_value(params.clone())?)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decode_notification_frame() {
        let line = r#"{"jsonrpc":"2.0","method":"codex/event","params":{"id":"e1","msg":{"type":"task_started","turn_id":"t-7"}}}"#;
        let frame = decode_line(line).unwrap();
        match frame {
            JsonRpcFrame::Notification { method, params } => {
                assert_eq!(method, "codex/event");
                let env = decode_event_params(&params).unwrap();
                assert_eq!(env.id.as_deref(), Some("e1"));
                assert!(matches!(env.msg, CodexMsg::TaskStarted(_)));
            }
            other => panic!("expected Notification, got {other:?}"),
        }
    }

    #[test]
    fn decode_response_success_frame() {
        let line = r#"{"jsonrpc":"2.0","id":42,"result":{"ok":true}}"#;
        let frame = decode_line(line).unwrap();
        match frame {
            JsonRpcFrame::Response { id, outcome } => {
                assert_eq!(id, json!(42));
                match outcome {
                    ResponseOutcome::Result(v) => assert_eq!(v["ok"], json!(true)),
                    ResponseOutcome::Error(e) => panic!("unexpected error {e:?}"),
                }
            }
            other => panic!("expected Response, got {other:?}"),
        }
    }

    #[test]
    fn decode_response_error_frame() {
        let line = r#"{"jsonrpc":"2.0","id":"req-1","error":{"code":-32601,"message":"method not found"}}"#;
        let frame = decode_line(line).unwrap();
        let JsonRpcFrame::Response { outcome, .. } = frame else {
            panic!("expected response");
        };
        let ResponseOutcome::Error(e) = outcome else {
            panic!("expected error");
        };
        assert_eq!(e.code, -32601);
        assert_eq!(e.message, "method not found");
    }

    #[test]
    fn decode_request_frame() {
        let line =
            r#"{"jsonrpc":"2.0","id":7,"method":"approvalRequest","params":{"kind":"exec"}}"#;
        let frame = decode_line(line).unwrap();
        match frame {
            JsonRpcFrame::Request { id, method, params } => {
                assert_eq!(id, json!(7));
                assert_eq!(method, "approvalRequest");
                assert_eq!(params["kind"], json!("exec"));
            }
            other => panic!("expected Request, got {other:?}"),
        }
    }

    #[test]
    fn invalid_json_is_an_error() {
        assert!(matches!(
            decode_line("not json"),
            Err(DecodeError::InvalidJson(_))
        ));
    }

    #[test]
    fn frames_must_be_objects() {
        assert!(matches!(
            decode_line("[1,2,3]"),
            Err(DecodeError::NotJsonRpc(_))
        ));
    }

    #[test]
    fn frames_without_method_or_result_rejected() {
        assert!(matches!(
            decode_line(r#"{"jsonrpc":"2.0","id":1}"#),
            Err(DecodeError::NotJsonRpc(_))
        ));
    }

    #[test]
    fn session_configured_accepts_both_field_names() {
        // Older app-server: `session_id`. Newer: `thread_id`. Both must
        // round-trip through the same alias-bearing payload.
        let a = decode_event_params(&json!({
            "id": "e0",
            "msg": {"type": "session_configured", "session_id": "th_a"}
        }))
        .unwrap();
        let b = decode_event_params(&json!({
            "id": "e0",
            "msg": {"type": "session_configured", "thread_id": "th_b"}
        }))
        .unwrap();

        let CodexMsg::SessionConfigured(pa) = a.msg else {
            panic!("not session_configured")
        };
        let CodexMsg::SessionConfigured(pb) = b.msg else {
            panic!("not session_configured")
        };
        assert_eq!(pa.session_id, "th_a");
        assert_eq!(pb.session_id, "th_b");
    }

    #[test]
    fn token_count_payload_defaults_zero_when_absent() {
        let env = decode_event_params(&json!({
            "id": "e1",
            "msg": {"type": "token_count"}
        }))
        .unwrap();
        let CodexMsg::TokenCount(p) = env.msg else {
            panic!("not token_count")
        };
        assert_eq!(p, TokenCountPayload::default());
    }

    #[test]
    fn unknown_msg_type_falls_into_other() {
        let env = decode_event_params(&json!({
            "id": "e1",
            "msg": {"type": "future_event_we_dont_know", "weird": 99}
        }))
        .unwrap();
        assert!(matches!(env.msg, CodexMsg::Other));
    }

    #[test]
    fn agent_message_payload_round_trips() {
        let env = decode_event_params(&json!({
            "id": "e1",
            "msg": {"type": "agent_message", "message": "Hello, operator."}
        }))
        .unwrap();
        let CodexMsg::AgentMessage(p) = env.msg else {
            panic!()
        };
        assert_eq!(p.message, "Hello, operator.");
    }

    #[test]
    fn task_complete_with_no_last_message_is_valid() {
        let env = decode_event_params(&json!({
            "id": "e1",
            "msg": {"type": "task_complete"}
        }))
        .unwrap();
        let CodexMsg::TaskComplete(p) = env.msg else {
            panic!()
        };
        assert!(p.last_agent_message.is_none());
    }

    #[test]
    fn error_payload_carries_message() {
        let env = decode_event_params(&json!({
            "id": "e1",
            "msg": {"type": "error", "message": "model overloaded"}
        }))
        .unwrap();
        let CodexMsg::Error(p) = env.msg else {
            panic!()
        };
        assert_eq!(p.message, "model overloaded");
    }

    #[test]
    fn turn_aborted_payload_optional_reason() {
        let env = decode_event_params(&json!({
            "id": "e1",
            "msg": {"type": "turn_aborted"}
        }))
        .unwrap();
        let CodexMsg::TurnAborted(p) = env.msg else {
            panic!()
        };
        assert!(p.reason.is_none());
    }

    #[test]
    fn mcp_tool_call_begin_preserves_arguments() {
        let env = decode_event_params(&json!({
            "id": "e1",
            "msg": {
                "type": "mcp_tool_call_begin",
                "call_id": "c1",
                "server": "fs",
                "tool": "read",
                "arguments": {"path": "/etc/hostname"}
            }
        }))
        .unwrap();
        let CodexMsg::McpToolCallBegin(p) = env.msg else {
            panic!()
        };
        assert_eq!(p.tool, "read");
        assert_eq!(p.arguments["path"], json!("/etc/hostname"));
    }
}
