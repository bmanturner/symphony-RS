//! Per-session tool registry.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::handler::ToolHandler;

/// One named tool registration.
#[derive(Clone)]
pub struct ToolRegistration {
    /// Tool name exposed to the MCP client.
    pub name: String,
    /// Handler invoked for calls to `name`.
    pub handler: Arc<dyn ToolHandler>,
}

impl std::fmt::Debug for ToolRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistration")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// Errors raised while building a tool registry.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ToolRegistryError {
    /// Two handlers attempted to register the same MCP tool name.
    #[error("duplicate MCP tool registration `{0}`")]
    Duplicate(String),
}

/// Deterministic registry for tools enabled in one run/session.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn ToolHandler>>,
    enabled: Option<BTreeSet<String>>,
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("tools", &self.tool_names())
            .field("enabled", &self.enabled)
            .finish()
    }
}

impl ToolRegistry {
    /// Build a registry from registrations, rejecting duplicates.
    pub fn new(registrations: Vec<ToolRegistration>) -> Result<Self, ToolRegistryError> {
        let mut tools = BTreeMap::new();
        for registration in registrations {
            if tools
                .insert(registration.name.clone(), registration.handler)
                .is_some()
            {
                return Err(ToolRegistryError::Duplicate(registration.name));
            }
        }
        Ok(Self {
            tools,
            enabled: None,
        })
    }

    /// Return a clone with only the named tools enabled.
    pub fn enable_only(mut self, names: impl IntoIterator<Item = String>) -> Self {
        self.enabled = Some(names.into_iter().collect());
        self
    }

    /// Fetch a handler by name, respecting the per-session enabled set.
    pub fn get(&self, name: &str) -> Option<Arc<dyn ToolHandler>> {
        if let Some(enabled) = &self.enabled
            && !enabled.contains(name)
        {
            return None;
        }
        self.tools.get(name).cloned()
    }

    /// Deterministic list of visible tool names.
    pub fn tool_names(&self) -> Vec<String> {
        self.tools
            .keys()
            .filter(|name| {
                self.enabled
                    .as_ref()
                    .is_none_or(|enabled| enabled.contains(*name))
            })
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use serde_json::Value;

    use super::*;
    use crate::handler::{ToolError, ToolResult};

    #[derive(Debug)]
    struct NoopTool(&'static str);

    #[async_trait]
    impl ToolHandler for NoopTool {
        fn name(&self) -> &str {
            self.0
        }

        async fn call(&self, _input: Value) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::json(Value::Null))
        }
    }

    #[test]
    fn duplicate_registrations_are_rejected() {
        let err = ToolRegistry::new(vec![
            ToolRegistration {
                name: "echo".into(),
                handler: Arc::new(NoopTool("echo")),
            },
            ToolRegistration {
                name: "echo".into(),
                handler: Arc::new(NoopTool("echo")),
            },
        ])
        .unwrap_err();

        assert_eq!(err, ToolRegistryError::Duplicate("echo".into()));
    }

    #[test]
    fn enable_only_filters_visible_tools() {
        let registry = ToolRegistry::new(vec![
            ToolRegistration {
                name: "a".into(),
                handler: Arc::new(NoopTool("a")),
            },
            ToolRegistration {
                name: "b".into(),
                handler: Arc::new(NoopTool("b")),
            },
        ])
        .unwrap()
        .enable_only(["b".to_string()]);

        assert_eq!(registry.tool_names(), vec!["b".to_string()]);
        assert!(registry.get("a").is_none());
        assert!(registry.get("b").is_some());
    }
}
