//! Codex `app-server` backend for [`AgentRunner`](symphony_core::agent::AgentRunner).
//!
//! Two pieces:
//!
//! - [`protocol`] — pure decoder for the JSON-RPC framing and the
//!   `codex/event` notification payloads. No async, no runtime
//!   dependencies, just `serde`. Forward-compatible: unknown event
//!   types decode as [`protocol::CodexMsg::Other`] rather than failing.
//! - [`runner`] — [`runner::CodexRunner`], which spawns
//!   `bash -lc <codex.command>`, drives the JSON-RPC handshake, and
//!   translates each Codex event into a normalized
//!   [`AgentEvent`](symphony_core::agent::AgentEvent).

pub mod protocol;
pub mod runner;

pub use protocol::{
    CodexEventEnvelope, CodexMsg, DecodeError, JsonRpcError, JsonRpcFrame, ResponseOutcome,
    decode_event_params, decode_line,
};
pub use runner::{CodexAgentControl, CodexConfig, CodexRunner, DEFAULT_CODEX_COMMAND};
