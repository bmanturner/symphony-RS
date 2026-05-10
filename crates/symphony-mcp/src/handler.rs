//! Tool handler abstraction for Symphony MCP tools.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Successful tool call result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolResult {
    /// Machine-readable payload returned to the caller.
    pub content: Value,
}

impl ToolResult {
    /// Build a result from any JSON value.
    pub fn json(content: impl Into<Value>) -> Self {
        Self {
            content: content.into(),
        }
    }
}

/// Structured tool-call error taxonomy.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolError {
    /// Payload failed schema or field-level validation.
    #[error("invalid payload at {field}: {message}")]
    InvalidPayload { field: String, message: String },

    /// The live tracker/backend cannot provide the requested capability.
    #[error("capability `{capability}` is unsupported")]
    CapabilityUnsupported { capability: String },

    /// Workflow policy refused an otherwise well-formed request.
    #[error("policy `{gate}` refused request: {message}")]
    PolicyViolation { gate: String, message: String },

    /// The calling role is not allowed to use this tool.
    #[error("role `{role}` is not allowed to call `{tool}`")]
    CapabilityRefused { role: String, tool: String },

    /// Generic internal failure; callers should treat it as retryable
    /// only when the tool documentation says so.
    #[error("internal tool error: {0}")]
    Internal(String),
}

/// Async handler for one MCP tool.
#[async_trait]
pub trait ToolHandler: Send + Sync {
    /// Stable MCP tool name.
    fn name(&self) -> &str;

    /// Invoke the tool with a JSON payload.
    async fn call(&self, input: Value) -> Result<ToolResult, ToolError>;
}
