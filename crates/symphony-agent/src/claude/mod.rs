//! Claude Code stream-json backend for
//! [`AgentRunner`](symphony_core::agent::AgentRunner).
//!
//! Two pieces, mirroring the shape of [`crate::codex`]:
//!
//! - [`protocol`] — pure decoder for `claude -p --output-format
//!   stream-json` events. No async, no runtime dependencies, just
//!   `serde`. Forward-compatible: unknown `type` values land in
//!   [`protocol::ClaudeEvent::Other`] rather than failing.
//! - [`runner`] — [`runner::ClaudeRunner`], which spawns the `claude`
//!   binary per-turn (one subprocess per turn, with `--session-id` on
//!   the first turn and `--resume` thereafter), translates each
//!   stream-json event into a normalized
//!   [`AgentEvent`](symphony_core::agent::AgentEvent), and stitches
//!   per-turn subprocesses into a single session by holding a stable
//!   thread-id and incrementing the turn-id counter.

pub mod protocol;
pub mod runner;

pub use protocol::{
    AssistantMessage, AssistantPayload, ClaudeEvent, ContentBlock, DecodeError, ResultPayload,
    SystemPayload, UsagePayload, UserMessage, UserPayload, decode_line, decode_value,
};
pub use runner::{ClaudeAgentControl, ClaudeConfig, ClaudeRunner, DEFAULT_CLAUDE_COMMAND};
