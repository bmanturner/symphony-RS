//! Per-run socket server lifecycle.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio_util::sync::CancellationToken;

use crate::handler::{ToolError, ToolResult};
use crate::registry::ToolRegistry;

/// Parameters for spawning one per-run MCP socket.
#[derive(Debug, Clone)]
pub struct SpawnParams {
    /// Unix-domain socket path for this run.
    pub socket_path: PathBuf,
    /// Tool registry visible to this run.
    pub registry: ToolRegistry,
}

/// Server facade.
pub struct Server;

impl Server {
    /// Spawn a per-run socket server. Existing stale sockets at the
    /// same path are removed before bind.
    pub async fn spawn(params: SpawnParams) -> Result<McpHandle, ServerError> {
        bind_socket(&params.socket_path)?;
        let listener =
            UnixListener::bind(&params.socket_path).map_err(|source| ServerError::Bind {
                path: params.socket_path.clone(),
                source,
            })?;
        let cancel = CancellationToken::new();
        let registry = Arc::new(params.registry);
        let socket_path = params.socket_path.clone();
        let cancel_for_task = cancel.clone();
        let task = tokio::spawn(async move {
            serve(listener, registry, cancel_for_task).await;
        });
        Ok(McpHandle {
            socket_path,
            cancel,
            task: Some(task),
        })
    }

    /// Return the registry for a role profile. Phase 1 has no
    /// role-specific defaults yet, so this is a pass-through hook for
    /// Phase 2+ to specialize.
    pub fn tools_for(_role: &str, registry: &ToolRegistry) -> ToolRegistry {
        registry.clone()
    }
}

/// RAII guard for a spawned MCP socket.
pub struct McpHandle {
    socket_path: PathBuf,
    cancel: CancellationToken,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl McpHandle {
    /// Socket path clients should connect to.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Shut down the server and remove its socket.
    pub async fn shutdown(mut self) -> Result<(), ServerError> {
        self.cancel.cancel();
        let _ = UnixStream::connect(&self.socket_path).await;
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
        remove_socket(&self.socket_path)
    }
}

impl Drop for McpHandle {
    fn drop(&mut self) {
        self.cancel.cancel();
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

/// Socket server errors.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    /// Parent directory could not be created.
    #[error("failed to create MCP socket parent for {path}: {source}")]
    CreateParent {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// Existing socket could not be removed.
    #[error("failed to remove stale MCP socket {path}: {source}")]
    RemoveStale {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// Binding the unix socket failed.
    #[error("failed to bind MCP socket {path}: {source}")]
    Bind {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolCallEnvelope {
    tool: String,
    #[serde(default)]
    input: Value,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum ToolResponseEnvelope {
    Ok { result: ToolResult },
    Err { error: ToolError },
}

async fn serve(listener: UnixListener, registry: Arc<ToolRegistry>, cancel: CancellationToken) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            accepted = listener.accept() => {
                let Ok((stream, _addr)) = accepted else {
                    continue;
                };
                let registry = registry.clone();
                tokio::spawn(async move {
                    let _ = handle_stream(stream, registry).await;
                });
            }
        }
    }
}

async fn handle_stream(
    stream: UnixStream,
    registry: Arc<ToolRegistry>,
) -> Result<(), std::io::Error> {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    while let Some(line) = lines.next_line().await? {
        let response = dispatch_line(&line, &registry).await;
        let encoded = serde_json::to_vec(&response).unwrap_or_else(|_| {
            br#"{"status":"err","error":{"kind":"internal","0":"encode"}}"#.to_vec()
        });
        write.write_all(&encoded).await?;
        write.write_all(b"\n").await?;
    }
    Ok(())
}

async fn dispatch_line(line: &str, registry: &ToolRegistry) -> ToolResponseEnvelope {
    let call: ToolCallEnvelope = match serde_json::from_str(line) {
        Ok(call) => call,
        Err(err) => {
            return ToolResponseEnvelope::Err {
                error: ToolError::InvalidPayload {
                    field: "$".into(),
                    message: err.to_string(),
                },
            };
        }
    };
    let Some(handler) = registry.get(&call.tool) else {
        return ToolResponseEnvelope::Err {
            error: ToolError::CapabilityUnsupported {
                capability: call.tool,
            },
        };
    };
    match handler.call(call.input).await {
        Ok(result) => ToolResponseEnvelope::Ok { result },
        Err(error) => ToolResponseEnvelope::Err { error },
    }
}

fn bind_socket(path: &Path) -> Result<(), ServerError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| ServerError::CreateParent {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    if path.exists() {
        std::fs::remove_file(path).map_err(|source| ServerError::RemoveStale {
            path: path.to_path_buf(),
            source,
        })?;
    }
    Ok(())
}

fn remove_socket(path: &Path) -> Result<(), ServerError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(ServerError::RemoveStale {
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use serde_json::json;

    use super::*;
    use crate::handler::ToolHandler;
    use crate::registry::ToolRegistration;

    #[derive(Debug)]
    struct EchoTool;

    #[async_trait]
    impl ToolHandler for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }

        async fn call(&self, input: Value) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::json(json!({ "echo": input })))
        }
    }

    #[tokio::test]
    async fn socket_lifecycle_removes_stale_socket_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("mcp.sock");
        std::fs::write(&socket_path, b"stale").unwrap();
        let registry = ToolRegistry::new(Vec::new()).unwrap();

        let handle = Server::spawn(SpawnParams {
            socket_path: socket_path.clone(),
            registry,
        })
        .await
        .unwrap();

        assert!(socket_path.exists());
        handle.shutdown().await.unwrap();
        assert!(!socket_path.exists());
    }

    #[tokio::test]
    async fn echo_tool_round_trips_over_socket() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("mcp.sock");
        let registry = ToolRegistry::new(vec![ToolRegistration {
            name: "echo".into(),
            handler: Arc::new(EchoTool),
        }])
        .unwrap();
        let handle = Server::spawn(SpawnParams {
            socket_path: socket_path.clone(),
            registry,
        })
        .await
        .unwrap();

        let mut stream = UnixStream::connect(&socket_path).await.unwrap();
        stream
            .write_all(br#"{"tool":"echo","input":{"hello":"world"}}"#)
            .await
            .unwrap();
        stream.write_all(b"\n").await.unwrap();

        let mut lines = BufReader::new(stream).lines();
        let line = lines.next_line().await.unwrap().unwrap();
        assert!(line.contains(r#""status":"ok""#), "{line}");
        assert!(line.contains(r#""hello":"world""#), "{line}");

        handle.shutdown().await.unwrap();
    }
}
