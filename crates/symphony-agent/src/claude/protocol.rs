//! Claude Code `stream-json` protocol decoder.
//!
//! `claude -p <prompt> --output-format stream-json --verbose` emits one
//! JSON object per line on stdout. Four top-level event types appear,
//! discriminated by a `type` field:
//!
//! 1. **`system`** — initialization event, sent once at the start.
//!    Carries the `session_id` Claude Code allocated (which matches the
//!    UUID we pass via `--session-id` if we supplied one), the model
//!    name, the cwd, and the available tool list.
//! 2. **`assistant`** — assistant turn output. The `message.content`
//!    field is an array of content blocks (`text`, `tool_use`, …).
//!    Multiple `assistant` events may appear within one turn as Claude
//!    streams its response in chunks; each carries a partial `usage`
//!    snapshot.
//! 3. **`user`** — synthetic user-role event carrying `tool_result`
//!    blocks the harness produced after running tools Claude requested.
//! 4. **`result`** — terminal event for the turn. Carries `is_error`,
//!    `subtype` (`success`, `error_max_turns`, `error_during_execution`,
//!    …), the final `usage` payload, `total_cost_usd`, and the
//!    surface-level `result` text.
//!
//! # Stability stance
//!
//! Per SPEC §10 we do not schema-lock more tightly than necessary.
//! Concretely:
//!
//! - The known top-level event types are decoded into named
//!   [`ClaudeEvent`] variants so callers can pattern-match.
//! - Any unrecognised `type` value lands in [`ClaudeEvent::Other`]
//!   carrying the raw JSON, never an error.
//! - Inside `assistant` content, only the blocks we care about
//!   (`text`, `tool_use`, `tool_result`) are typed; everything else
//!   becomes [`ContentBlock::Other`].
//! - Unknown fields on known events are silently ignored
//!   (`#[serde(default)]` + non-strict structs) rather than rejected.
//!
//! Translation from [`ClaudeEvent`] to
//! [`symphony_core::agent::AgentEvent`] lives in
//! [`crate::claude::runner`] so this module is a pure decoder with no
//! async or runtime dependencies.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// Failure to decode a Claude stream-json line.
///
/// The runner promotes these to `tracing::warn` rather than terminating
/// the session — a single malformed line shouldn't kill an in-flight
/// turn.
#[derive(Debug, Error)]
pub enum DecodeError {
    /// Line wasn't valid JSON at all.
    #[error("claude protocol: invalid JSON: {0}")]
    InvalidJson(#[from] serde_json::Error),
}

/// One decoded Claude stream-json event.
///
/// Variants are tagged by the `type` field per Claude Code's wire format.
/// Unknown event types deserialize to [`ClaudeEvent::Other`] preserving
/// the raw JSON so a future Claude release adding new event kinds
/// doesn't break this decoder.
#[derive(Debug, Clone, PartialEq)]
pub enum ClaudeEvent {
    /// Initialization event emitted once at the start of a turn.
    System(SystemPayload),
    /// Assistant turn output (one or more per turn as content streams).
    Assistant(AssistantPayload),
    /// Synthetic user-role event carrying tool results back to Claude.
    User(UserPayload),
    /// Terminal event for the turn.
    Result(ResultPayload),
    /// Any event whose `type` we don't recognise. Preserved verbatim.
    Other(Value),
}

/// Payload of a `system` event. `subtype` is typically `"init"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemPayload {
    /// Session UUID Claude allocated. Matches the value we passed via
    /// `--session-id` when one was supplied.
    pub session_id: String,
    /// `"init"` for the initialization event; reserved for future
    /// system event subtypes.
    #[serde(default)]
    pub subtype: Option<String>,
    /// Model name Claude resolved (e.g. `claude-opus-4-7`).
    #[serde(default)]
    pub model: Option<String>,
    /// Current working directory the harness was launched in.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Permission mode Claude is operating under.
    #[serde(default)]
    pub permission_mode: Option<String>,
}

/// Payload of an `assistant` event. The `message` field mirrors a slice
/// of the Anthropic Messages API response shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantPayload {
    /// Session id this event belongs to.
    pub session_id: String,
    /// The assistant message itself.
    pub message: AssistantMessage,
}

/// Anthropic-style assistant message body extracted from an `assistant`
/// event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessage {
    /// Anthropic message id (`msg_…`). Stable within a single
    /// assistant turn; multiple `assistant` events within one turn
    /// share the same id as Claude streams content blocks.
    #[serde(default)]
    pub id: Option<String>,
    /// Always `"assistant"` for these events.
    #[serde(default)]
    pub role: Option<String>,
    /// One or more content blocks (text, tool_use, …).
    #[serde(default)]
    pub content: Vec<ContentBlock>,
    /// Partial usage snapshot. Final usage arrives in the `result`
    /// event.
    #[serde(default)]
    pub usage: Option<UsagePayload>,
    /// Anthropic stop reason (`end_turn`, `tool_use`, …) when set.
    #[serde(default)]
    pub stop_reason: Option<String>,
}

/// Payload of a `user` event. Carries tool_result blocks the harness
/// produced after running a tool Claude invoked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserPayload {
    /// Session id this event belongs to.
    pub session_id: String,
    /// Anthropic-style user message body.
    pub message: UserMessage,
}

/// User-role message body for tool results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserMessage {
    /// Always `"user"` for these events.
    #[serde(default)]
    pub role: Option<String>,
    /// Content blocks — typically `tool_result` entries.
    #[serde(default)]
    pub content: Vec<ContentBlock>,
}

/// Payload of a terminal `result` event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResultPayload {
    /// Session id this event belongs to.
    pub session_id: String,
    /// `"success"`, `"error_max_turns"`, `"error_during_execution"`,
    /// or any other Claude-defined subtype.
    #[serde(default)]
    pub subtype: Option<String>,
    /// True when the turn ended in an error condition.
    #[serde(default)]
    pub is_error: bool,
    /// Wall-clock duration in milliseconds.
    #[serde(default)]
    pub duration_ms: Option<u64>,
    /// Final usage snapshot.
    #[serde(default)]
    pub usage: Option<UsagePayload>,
    /// Cumulative cost in USD across the turn.
    #[serde(default)]
    pub total_cost_usd: Option<f64>,
    /// Final surface-level result text (the assistant's last reply).
    #[serde(default)]
    pub result: Option<String>,
    /// Number of turns Claude actually used (may differ from configured
    /// max).
    #[serde(default)]
    pub num_turns: Option<u64>,
}

/// One content block inside an `assistant` or `user` message.
#[derive(Debug, Clone, PartialEq)]
pub enum ContentBlock {
    /// Plain assistant text.
    Text {
        /// The text body.
        text: String,
    },
    /// Tool invocation Claude wants the harness to perform.
    ToolUse {
        /// Anthropic tool-use id used to correlate with the matching
        /// tool_result.
        id: String,
        /// Tool name as advertised to Claude.
        name: String,
        /// Raw tool input JSON. Schema is tool-defined; preserved
        /// verbatim.
        input: Value,
    },
    /// Result of a tool the harness ran for Claude.
    ToolResult {
        /// Echoed `tool_use.id` correlating with the original call.
        tool_use_id: String,
        /// Tool output. Claude allows either a string or a structured
        /// content array; both are normalized to a JSON Value here so
        /// downstream code doesn't have to special-case.
        content: Value,
        /// True when the tool reported an error.
        #[allow(dead_code)]
        is_error: bool,
    },
    /// Any block whose `type` we don't recognise. Preserved verbatim.
    Other(Value),
}

/// Anthropic-style usage payload extracted from `assistant.usage` or
/// `result.usage`.
///
/// Claude reports cache hits/creates separately from base prompt input;
/// the runner sums them into [`symphony_core::agent::TokenUsage::input`]
/// while preserving the cache-read count under `cached_input`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsagePayload {
    /// Base prompt tokens billed at standard rate.
    #[serde(default)]
    pub input_tokens: u64,
    /// Tokens generated by the model.
    #[serde(default)]
    pub output_tokens: u64,
    /// Tokens written to the prompt cache this turn.
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    /// Tokens served from the prompt cache (billed at the cache-read
    /// rate).
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}

/// Decode one NDJSON line into a [`ClaudeEvent`].
///
/// Empty/whitespace-only lines should be filtered upstream; this fn
/// returns [`DecodeError::InvalidJson`] for them.
pub fn decode_line(line: &str) -> Result<ClaudeEvent, DecodeError> {
    let raw: Value = serde_json::from_str(line.trim_end())?;
    Ok(decode_value(raw))
}

/// Inner decoder operating on an already-parsed JSON value. Public so
/// tests and the runner's `app-server`-style fixtures can reuse it.
pub fn decode_value(raw: Value) -> ClaudeEvent {
    let kind = raw
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    match kind.as_str() {
        "system" => match serde_json::from_value::<SystemPayload>(raw.clone()) {
            Ok(p) => ClaudeEvent::System(p),
            Err(_) => ClaudeEvent::Other(raw),
        },
        "assistant" => match serde_json::from_value::<AssistantPayload>(raw.clone()) {
            Ok(p) => ClaudeEvent::Assistant(p),
            Err(_) => ClaudeEvent::Other(raw),
        },
        "user" => match serde_json::from_value::<UserPayload>(raw.clone()) {
            Ok(p) => ClaudeEvent::User(p),
            Err(_) => ClaudeEvent::Other(raw),
        },
        "result" => match serde_json::from_value::<ResultPayload>(raw.clone()) {
            Ok(p) => ClaudeEvent::Result(p),
            Err(_) => ClaudeEvent::Other(raw),
        },
        _ => ClaudeEvent::Other(raw),
    }
}

// `ContentBlock` needs a custom Deserialize because the discriminator
// field is `type` and the structured variants share differently-shaped
// payloads. We hand-roll rather than `#[serde(tag = "type")]` so unknown
// block types fall through to `Other` instead of failing the whole
// `assistant` event.
impl<'de> Deserialize<'de> for ContentBlock {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = Value::deserialize(deserializer)?;
        let kind = raw.get("type").and_then(Value::as_str).unwrap_or("");
        Ok(match kind {
            "text" => {
                let text = raw
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                ContentBlock::Text { text }
            }
            "tool_use" => {
                let id = raw
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let name = raw
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let input = raw.get("input").cloned().unwrap_or(Value::Null);
                ContentBlock::ToolUse { id, name, input }
            }
            "tool_result" => {
                let tool_use_id = raw
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let content = raw.get("content").cloned().unwrap_or(Value::Null);
                let is_error = raw
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                }
            }
            _ => ContentBlock::Other(raw),
        })
    }
}

impl Serialize for ContentBlock {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        // Round-tripping the original JSON shape isn't important for
        // Symphony — we never re-emit Claude protocol — but a
        // Serialize impl makes the type usable in tests that assert on
        // serde_json::Value.
        let v = match self {
            ContentBlock::Text { text } => serde_json::json!({"type": "text", "text": text}),
            ContentBlock::ToolUse { id, name, input } => {
                serde_json::json!({"type": "tool_use", "id": id, "name": name, "input": input})
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => serde_json::json!({
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": content,
                "is_error": is_error,
            }),
            ContentBlock::Other(v) => v.clone(),
        };
        v.serialize(serializer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decodes_system_init_event() {
        let line = r#"{"type":"system","subtype":"init","session_id":"u-1","model":"claude-opus","cwd":"/w","permission_mode":"bypassPermissions","tools":["Bash"]}"#;
        match decode_line(line).unwrap() {
            ClaudeEvent::System(p) => {
                assert_eq!(p.session_id, "u-1");
                assert_eq!(p.subtype.as_deref(), Some("init"));
                assert_eq!(p.model.as_deref(), Some("claude-opus"));
                assert_eq!(p.permission_mode.as_deref(), Some("bypassPermissions"));
            }
            other => panic!("expected System, got {other:?}"),
        }
    }

    #[test]
    fn decodes_assistant_text_content() {
        let line = r#"{"type":"assistant","session_id":"u-1","message":{"id":"msg_1","role":"assistant","content":[{"type":"text","text":"hello"}],"usage":{"input_tokens":10,"output_tokens":3}}}"#;
        match decode_line(line).unwrap() {
            ClaudeEvent::Assistant(p) => {
                assert_eq!(p.session_id, "u-1");
                assert_eq!(p.message.id.as_deref(), Some("msg_1"));
                assert_eq!(p.message.content.len(), 1);
                match &p.message.content[0] {
                    ContentBlock::Text { text } => assert_eq!(text, "hello"),
                    other => panic!("expected Text, got {other:?}"),
                }
                let usage = p.message.usage.unwrap();
                assert_eq!(usage.input_tokens, 10);
                assert_eq!(usage.output_tokens, 3);
            }
            other => panic!("expected Assistant, got {other:?}"),
        }
    }

    #[test]
    fn decodes_assistant_tool_use_block() {
        let line = r#"{"type":"assistant","session_id":"u-1","message":{"content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls"}}]}}"#;
        let ev = decode_line(line).unwrap();
        let ClaudeEvent::Assistant(p) = ev else {
            panic!("expected Assistant");
        };
        match &p.message.content[0] {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "toolu_1");
                assert_eq!(name, "Bash");
                assert_eq!(input["command"], json!("ls"));
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn decodes_user_tool_result() {
        let line = r#"{"type":"user","session_id":"u-1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"ok","is_error":false}]}}"#;
        let ClaudeEvent::User(p) = decode_line(line).unwrap() else {
            panic!("expected User");
        };
        match &p.message.content[0] {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                assert_eq!(tool_use_id, "toolu_1");
                assert_eq!(content, &json!("ok"));
                assert!(!*is_error);
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn decodes_result_with_full_usage_payload() {
        let line = r#"{"type":"result","subtype":"success","session_id":"u-1","is_error":false,"duration_ms":1234,"total_cost_usd":0.0125,"num_turns":1,"result":"done","usage":{"input_tokens":100,"output_tokens":40,"cache_creation_input_tokens":20,"cache_read_input_tokens":50}}"#;
        let ClaudeEvent::Result(p) = decode_line(line).unwrap() else {
            panic!("expected Result");
        };
        assert_eq!(p.subtype.as_deref(), Some("success"));
        assert!(!p.is_error);
        assert_eq!(p.total_cost_usd, Some(0.0125));
        let u = p.usage.unwrap();
        assert_eq!(u.input_tokens, 100);
        assert_eq!(u.output_tokens, 40);
        assert_eq!(u.cache_creation_input_tokens, 20);
        assert_eq!(u.cache_read_input_tokens, 50);
    }

    #[test]
    fn unknown_top_level_type_lands_in_other() {
        let line = r#"{"type":"future_event","payload":{"x":1}}"#;
        match decode_line(line).unwrap() {
            ClaudeEvent::Other(v) => assert_eq!(v["type"], json!("future_event")),
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn unknown_content_block_lands_in_other_block() {
        let line = r#"{"type":"assistant","session_id":"u","message":{"content":[{"type":"future_block","weird":1}]}}"#;
        let ClaudeEvent::Assistant(p) = decode_line(line).unwrap() else {
            panic!("expected Assistant");
        };
        match &p.message.content[0] {
            ContentBlock::Other(v) => assert_eq!(v["type"], json!("future_block")),
            other => panic!("expected Other block, got {other:?}"),
        }
    }

    #[test]
    fn malformed_json_returns_decode_error() {
        let err = decode_line("{not json").unwrap_err();
        matches!(err, DecodeError::InvalidJson(_));
    }

    #[test]
    fn missing_required_session_id_falls_through_to_other() {
        // System event without session_id can't deserialize; we keep it
        // as Other rather than failing the whole stream.
        let line = r#"{"type":"system","subtype":"init"}"#;
        let ev = decode_line(line).unwrap();
        assert!(matches!(ev, ClaudeEvent::Other(_)));
    }

    #[test]
    fn assistant_with_no_content_array_decodes_with_empty_content() {
        let line = r#"{"type":"assistant","session_id":"u","message":{}}"#;
        let ClaudeEvent::Assistant(p) = decode_line(line).unwrap() else {
            panic!("expected Assistant");
        };
        assert!(p.message.content.is_empty());
        assert!(p.message.usage.is_none());
    }
}
