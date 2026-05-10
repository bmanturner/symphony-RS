//! Agent-side Symphony MCP wiring helpers.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use symphony_core::agent::{AgentError, AgentResult};
use symphony_mcp::{Server, SpawnParams, ToolRegistry};

/// Runtime MCP session resources owned by an agent control handle.
pub(crate) struct AgentMcpSession {
    pub(crate) _config_path: PathBuf,
    _handle: symphony_mcp::McpHandle,
}

/// Prepare a per-run Symphony MCP server/config when tools are enabled.
pub(crate) async fn prepare_mcp_session(
    workspace: &Path,
    existing_args: &[String],
    enabled_tools: &[String],
) -> AgentResult<(Option<AgentMcpSession>, Vec<String>)> {
    let registry = ToolRegistry::new(Vec::new())
        .map_err(|err| AgentError::Spawn(format!("build MCP registry: {err}")))?;
    prepare_mcp_session_with_registry(workspace, existing_args, enabled_tools, registry).await
}

pub(crate) async fn prepare_mcp_session_with_registry(
    workspace: &Path,
    existing_args: &[String],
    enabled_tools: &[String],
    registry: ToolRegistry,
) -> AgentResult<(Option<AgentMcpSession>, Vec<String>)> {
    if enabled_tools.is_empty() {
        return Ok((None, existing_args.to_vec()));
    }

    let symphony_dir = workspace.join(".symphony");
    tokio::fs::create_dir_all(&symphony_dir)
        .await
        .map_err(|err| AgentError::Spawn(format!("create MCP dir: {err}")))?;
    let socket_path = symphony_dir.join("mcp.sock");
    let config_path = symphony_dir.join("mcp-config.json");

    let registry = registry.enable_only(enabled_tools.iter().cloned());
    let handle = Server::spawn(SpawnParams {
        socket_path: socket_path.clone(),
        registry,
    })
    .await
    .map_err(|err| AgentError::Spawn(format!("spawn Symphony MCP server: {err}")))?;

    let merged = merged_mcp_config(existing_args, &socket_path, enabled_tools)?;
    let encoded = serde_json::to_vec_pretty(&merged)
        .map_err(|err| AgentError::Spawn(format!("encode MCP config: {err}")))?;
    tokio::fs::write(&config_path, encoded)
        .await
        .map_err(|err| AgentError::Spawn(format!("write MCP config: {err}")))?;

    let mut args = strip_operator_mcp_config(existing_args);
    args.push("--mcp-config".into());
    args.push(config_path.display().to_string());
    Ok((
        Some(AgentMcpSession {
            _config_path: config_path,
            _handle: handle,
        }),
        args,
    ))
}

fn merged_mcp_config(
    args: &[String],
    socket_path: &Path,
    enabled_tools: &[String],
) -> AgentResult<Value> {
    let mut servers = serde_json::Map::new();
    if let Some(path) = operator_mcp_config_path(args) {
        let raw = std::fs::read_to_string(path)
            .map_err(|err| AgentError::Spawn(format!("read operator MCP config {path}: {err}")))?;
        let doc: Value = serde_json::from_str(&raw)
            .map_err(|err| AgentError::Spawn(format!("parse operator MCP config {path}: {err}")))?;
        if let Some(existing) = doc.get("mcpServers").and_then(Value::as_object) {
            for (name, value) in existing {
                if name == "symphony" {
                    return Err(AgentError::Spawn(
                        "operator MCP config defines reserved server `symphony`".into(),
                    ));
                }
                if let Some(conflicts) = value.get("tools").and_then(Value::as_array) {
                    for tool in conflicts.iter().filter_map(Value::as_str) {
                        if enabled_tools.iter().any(|enabled| enabled == tool) {
                            return Err(AgentError::Spawn(format!(
                                "operator MCP config tool `{tool}` conflicts with Symphony registry"
                            )));
                        }
                    }
                }
                servers.insert(name.clone(), value.clone());
            }
        }
    }
    servers.insert(
        "symphony".into(),
        json!({
            "transport": "unix",
            "socket": socket_path.display().to_string(),
            "tools": enabled_tools,
        }),
    );
    Ok(json!({ "mcpServers": servers }))
}

fn operator_mcp_config_path(args: &[String]) -> Option<&str> {
    args.windows(2)
        .find(|pair| pair[0] == "--mcp-config")
        .map(|pair| pair[1].as_str())
}

fn strip_operator_mcp_config(args: &[String]) -> Vec<String> {
    let mut stripped = Vec::new();
    let mut skip_next = false;
    for arg in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "--mcp-config" {
            skip_next = true;
            continue;
        }
        stripped.push(arg.clone());
    }
    stripped
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    use super::*;
    use symphony_mcp::{ToolError, ToolHandler, ToolRegistration, ToolResult};

    struct StaticTool;

    #[async_trait]
    impl ToolHandler for StaticTool {
        fn name(&self) -> &str {
            "propose_decomposition"
        }

        async fn call(&self, _input: Value) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::json(json!({
                "decomposition_id": 1,
                "child_count": 1,
                "status": "applied"
            })))
        }
    }

    #[test]
    fn merges_no_operator_config() {
        let doc =
            merged_mcp_config(&[], Path::new("/tmp/mcp.sock"), &["submit_handoff".into()]).unwrap();
        assert_eq!(
            doc["mcpServers"]["symphony"]["socket"],
            Value::String("/tmp/mcp.sock".into())
        );
    }

    #[test]
    fn preserves_operator_config_without_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        let operator = dir.path().join("operator.json");
        std::fs::write(
            &operator,
            r#"{"mcpServers":{"linear":{"command":"linear-mcp","tools":["search"]}}}"#,
        )
        .unwrap();
        let doc = merged_mcp_config(
            &["--mcp-config".into(), operator.display().to_string()],
            Path::new("/tmp/mcp.sock"),
            &["submit_handoff".into()],
        )
        .unwrap();
        assert_eq!(
            doc["mcpServers"]["linear"]["command"],
            Value::String("linear-mcp".into())
        );
        assert!(doc["mcpServers"]["symphony"].is_object());
    }

    #[test]
    fn rejects_operator_tool_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        let operator = dir.path().join("operator.json");
        std::fs::write(
            &operator,
            r#"{"mcpServers":{"other":{"tools":["submit_handoff"]}}}"#,
        )
        .unwrap();
        let err = merged_mcp_config(
            &["--mcp-config".into(), operator.display().to_string()],
            Path::new("/tmp/mcp.sock"),
            &["submit_handoff".into()],
        )
        .unwrap_err();
        assert!(err.to_string().contains("conflicts"));
    }

    #[tokio::test]
    async fn prepared_session_binds_socket_and_cleans_up_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let (session, args) = prepare_mcp_session(dir.path(), &[], &["submit_handoff".to_string()])
            .await
            .unwrap();
        let session = session.expect("MCP session should be enabled");
        assert!(dir.path().join(".symphony/mcp.sock").exists());
        assert!(session._config_path.exists());
        assert!(args.iter().any(|arg| arg == "--mcp-config"));
        drop(session);
        assert!(!dir.path().join(".symphony/mcp.sock").exists());
    }

    async fn call_tool(socket_path: &Path, tool: &str) -> Value {
        let stream = UnixStream::connect(socket_path).await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        let encoded = serde_json::to_string(&json!({
            "tool": tool,
            "input": { "summary": "ok", "children": [] }
        }))
        .unwrap();
        write.write_all(encoded.as_bytes()).await.unwrap();
        write.write_all(b"\n").await.unwrap();
        serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn claude_and_codex_configs_surface_propose_decomposition_and_serialize_tool_calls() {
        let mut claude = crate::claude::ClaudeConfig::default();
        claude.mcp_tools = vec!["propose_decomposition".into()];
        let mut codex = crate::codex::CodexConfig::default();
        codex.mcp_tools = vec!["propose_decomposition".into()];

        for (backend, tools) in [("claude", claude.mcp_tools), ("codex", codex.mcp_tools)] {
            let dir = tempfile::tempdir().unwrap();
            let registry = ToolRegistry::new(vec![ToolRegistration {
                name: "propose_decomposition".into(),
                handler: std::sync::Arc::new(StaticTool),
            }])
            .unwrap();
            let (session, args) =
                prepare_mcp_session_with_registry(dir.path(), &[], &tools, registry)
                    .await
                    .unwrap();
            let session = session.expect("MCP session");
            let config: Value =
                serde_json::from_slice(&tokio::fs::read(&session._config_path).await.unwrap())
                    .unwrap();
            assert_eq!(
                config["mcpServers"]["symphony"]["tools"],
                json!(["propose_decomposition"]),
                "{backend} should expose propose_decomposition"
            );
            assert!(args.iter().any(|arg| arg == "--mcp-config"));

            let response = call_tool(
                dir.path().join(".symphony/mcp.sock").as_path(),
                "propose_decomposition",
            )
            .await;
            assert_eq!(response["status"], "ok");
            assert_eq!(
                response["result"]["content"]["status"], "applied",
                "{backend} should serialize a successful tool result"
            );
        }
    }
}
