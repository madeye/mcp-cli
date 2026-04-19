//! Minimal MCP stdio server. Implements just enough of the protocol
//! (initialize / tools/list / tools/call) to expose the daemon's tools to
//! Claude Code, Codex, and other MCP-compatible clients.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::daemon_client::DaemonClient;

pub async fn run(socket: PathBuf) -> Result<()> {
    let client = Arc::new(DaemonClient::connect(&socket).await?);
    tracing::info!(socket = %socket.display(), "bridge connected to daemon");

    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut lines = BufReader::new(stdin).lines();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let err = json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": {"code": -32700, "message": format!("parse error: {e}")},
                });
                write_message(&mut stdout, &err).await?;
                continue;
            }
        };

        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("").to_string();
        let params = req.get("params").cloned().unwrap_or(Value::Null);

        // Notifications (no id): handle without responding.
        if id.is_null() && method.starts_with("notifications/") {
            continue;
        }

        let response = match handle(&client, &method, params).await {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(err) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32000, "message": err.to_string()},
            }),
        };
        write_message(&mut stdout, &response).await?;
    }
    Ok(())
}

async fn write_message<W: AsyncWriteExt + Unpin>(w: &mut W, msg: &Value) -> Result<()> {
    let bytes = serde_json::to_vec(msg)?;
    w.write_all(&bytes).await?;
    w.write_all(b"\n").await?;
    w.flush().await?;
    Ok(())
}

async fn handle(client: &DaemonClient, method: &str, params: Value) -> Result<Value> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "mcp-cli-bridge", "version": env!("CARGO_PKG_VERSION")},
        })),
        "tools/list" => Ok(json!({"tools": tool_definitions()})),
        "tools/call" => tools_call(client, params).await,
        "ping" => Ok(json!({})),
        other => Err(anyhow::anyhow!("method not supported: {other}")),
    }
}

async fn tools_call(client: &DaemonClient, params: Value) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing tool name"))?;
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    let daemon_method = match name {
        "fs_read" => protocol::methods::FS_READ,
        "git_status" => protocol::methods::GIT_STATUS,
        "search_grep" => protocol::methods::SEARCH_GREP,
        other => return Err(anyhow::anyhow!("unknown tool: {other}")),
    };

    let daemon_result = client.call(daemon_method, args).await?;
    let text = serde_json::to_string_pretty(&daemon_result)?;
    Ok(json!({
        "content": [{"type": "text", "text": text}],
        "isError": false,
    }))
}

fn tool_definitions() -> Value {
    json!([
        {
            "name": "fs_read",
            "description": "Read a file from the project root via the daemon's mmap-backed VFS.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path relative to project root."},
                    "offset": {"type": "integer", "minimum": 0, "default": 0},
                    "length": {"type": "integer", "minimum": 1, "description": "Bytes to read; default 256 KiB."}
                },
                "required": ["path"]
            }
        },
        {
            "name": "git_status",
            "description": "Return git status entries for the project (in-process libgit2, no fork/exec).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": {"type": "string", "description": "Optional repo path relative to project root."}
                }
            }
        },
        {
            "name": "search_grep",
            "description": "Run a regex search using ripgrep's library (grep-searcher) over the project tree.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pattern": {"type": "string"},
                    "path": {"type": "string", "description": "Subdirectory relative to project root."},
                    "glob": {"type": "string", "description": "Override glob (e.g. '*.rs')."},
                    "max_results": {"type": "integer", "minimum": 1, "default": 200},
                    "case_insensitive": {"type": "boolean", "default": false}
                },
                "required": ["pattern"]
            }
        }
    ])
}
