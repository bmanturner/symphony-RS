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
    if enabled_tools.is_empty() {
        return Ok((None, existing_args.to_vec()));
    }

    let symphony_dir = workspace.join(".symphony");
    tokio::fs::create_dir_all(&symphony_dir)
        .await
        .map_err(|err| AgentError::Spawn(format!("create MCP dir: {err}")))?;
    let socket_path = symphony_dir.join("mcp.sock");
    let config_path = symphony_dir.join("mcp-config.json");

    let registry = ToolRegistry::new(Vec::new())
        .map_err(|err| AgentError::Spawn(format!("build MCP registry: {err}")))?
        .enable_only(enabled_tools.iter().cloned());
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
    use super::*;

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
}
