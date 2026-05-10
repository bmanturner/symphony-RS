//! Symphony's internal MCP server scaffolding.
//!
//! The public surface intentionally starts smaller than the full MCP
//! protocol. Phase 1 establishes the handler trait, registry, per-run
//! socket lifecycle, and a deterministic JSON round-trip harness. Later
//! phases adapt this to `rmcp` tool/result types as concrete Symphony
//! tools land.

pub mod decomposition_tools;
pub mod handler;
pub mod read_tools;
pub mod registry;
pub mod server;

pub use handler::{ToolError, ToolHandler, ToolResult};
pub use registry::{ToolRegistration, ToolRegistry, ToolRegistryError};
pub use server::{McpHandle, Server, ServerError, SpawnParams};
