//! Minimal MCP stdio server. Implements just enough of the protocol
//! (initialize / tools/list / tools/call) to expose the daemon's tools to
//! Claude Code, Codex, and other MCP-compatible clients.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::daemon_client::{ConnectConfig, DaemonClient};

pub struct RunConfig {
    pub socket: PathBuf,
    pub root: PathBuf,
    pub daemon_bin: Option<PathBuf>,
    pub autospawn: bool,
    pub daemon_extra_args: Vec<String>,
}

pub async fn run(cfg: RunConfig) -> Result<()> {
    let socket_display = cfg.socket.display().to_string();
    let client = Arc::new(
        DaemonClient::connect(ConnectConfig {
            socket: cfg.socket,
            root: cfg.root,
            daemon_bin: cfg.daemon_bin,
            autospawn: cfg.autospawn,
            daemon_extra_args: cfg.daemon_extra_args,
        })
        .await?,
    );
    tracing::info!(socket = %socket_display, "bridge connected to daemon");

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
        let method = req
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();
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
        "fs_read_batch" => protocol::methods::FS_READ_BATCH,
        "fs_apply_patch" => protocol::methods::FS_APPLY_PATCH,
        "fs_replace_all" => protocol::methods::FS_REPLACE_ALL,
        "fs_snapshot" => protocol::methods::FS_SNAPSHOT,
        "fs_changes" => protocol::methods::FS_CHANGES,
        "fs_scan" => protocol::methods::FS_SCAN,
        "git_status" => protocol::methods::GIT_STATUS,
        "git_log" => protocol::methods::GIT_LOG,
        "git_diff" => protocol::methods::GIT_DIFF,
        "search_grep" => protocol::methods::SEARCH_GREP,
        "code_outline" => protocol::methods::CODE_OUTLINE,
        "code_outline_batch" => protocol::methods::CODE_OUTLINE_BATCH,
        "code_symbols" => protocol::methods::CODE_SYMBOLS,
        "code_symbols_batch" => protocol::methods::CODE_SYMBOLS_BATCH,
        "tool_run" => protocol::methods::TOOL_RUN,
        "tool_gh" => protocol::methods::TOOL_GH,
        "metrics_gain" => protocol::methods::METRICS_GAIN,
        "metrics_tool_latency" => protocol::methods::METRICS_TOOL_LATENCY,
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
            "description": "Read a file from the project root via the daemon's mmap-backed VFS. Returns `version` and `mtime_ns` tokens that can be echoed into write RPCs for optimistic concurrency. Set `strip_noise: true` to collapse license headers, long base64 blobs, and `@generated` bodies into short `[[mcp-cli: stripped …]]` markers — original line ranges are reported in `stripped_regions` so callers can ask for specific lines back if needed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path relative to project root."},
                    "offset": {"type": "integer", "minimum": 0, "default": 0},
                    "length": {"type": "integer", "minimum": 1, "description": "Bytes to read; default 256 KiB."},
                    "strip_noise": {"type": "boolean", "default": false, "description": "Elide license/base64/generated boilerplate from the returned content. Only applied when offset is 0."}
                },
                "required": ["path"]
            }
        },
        {
            "name": "fs_read_batch",
            "description": "Read many files (or regions) in one MCP call. Response is a parallel list of {result?, error?}; per-request failures do not abort the batch. Each request may opt into `strip_noise` independently.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "requests": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": {"type": "string"},
                                "offset": {"type": "integer", "minimum": 0, "default": 0},
                                "length": {"type": "integer", "minimum": 1},
                                "strip_noise": {"type": "boolean", "default": false}
                            },
                            "required": ["path"]
                        },
                        "minItems": 1
                    }
                },
                "required": ["requests"]
            }
        },
        {
            "name": "fs_apply_patch",
            "description": "Apply unified diff hunks to one existing file. Supports optimistic concurrency with `expected_version` from fs_read/fs_snapshot and `expected_mtime_ns` from fs_read; stale writes are rejected before touching the file.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Existing file path relative to project root."},
                    "patch": {"type": "string", "description": "Unified diff hunks for this file."},
                    "expected_version": {"type": "integer", "minimum": 0, "description": "Optional ChangeLog version returned by fs_read or fs_snapshot."},
                    "expected_mtime_ns": {"type": "integer", "minimum": 0, "description": "Optional mtime_ns returned by fs_read."}
                },
                "required": ["path", "patch"]
            }
        },
        {
            "name": "fs_replace_all",
            "description": "Replace every literal occurrence of a string in one existing file. Supports the same optimistic concurrency fields as fs_apply_patch and can fail instead of writing when `max_replacements` would be exceeded.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Existing file path relative to project root."},
                    "search": {"type": "string"},
                    "replacement": {"type": "string"},
                    "expected_version": {"type": "integer", "minimum": 0, "description": "Optional ChangeLog version returned by fs_read or fs_snapshot."},
                    "expected_mtime_ns": {"type": "integer", "minimum": 0, "description": "Optional mtime_ns returned by fs_read."},
                    "max_replacements": {"type": "integer", "minimum": 0, "description": "Fail without writing if the occurrence count exceeds this cap."}
                },
                "required": ["path", "search", "replacement"]
            }
        },
        {
            "name": "fs_snapshot",
            "description": "Return the current monotonic version cursor for the watched tree. Pair with fs_changes to do incremental syncs.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "fs_changes",
            "description": "Return file changes (created/modified/removed, coalesced per path) since the given version. If `overflowed` is true, the client must do a full re-scan.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "since": {"type": "integer", "minimum": 0}
                },
                "required": ["since"]
            }
        },
        {
            "name": "fs_scan",
            "description": "Enumerate all tracked files in the project (gitignore-aware, .git excluded). Returns the version cursor captured at the start of the walk, so a follow-up fs_changes(since: version) closes any race with events that landed during the scan. Use when fs_changes returned `overflowed: true`. Set `compact: true` for a directory roll-up (per-dir file counts) instead of the flat path list — usually 10-100× smaller for 'what's in this repo' exploration.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Optional subdirectory relative to project root."},
                    "max_results": {"type": "integer", "minimum": 1, "description": "Cap on returned entries."},
                    "compact": {"type": "boolean", "default": false, "description": "Return a `{by_dir: [{dir, count}], total}` roll-up instead of `files`."}
                }
            }
        },
        {
            "name": "git_status",
            "description": "Return git status entries for the project (in-process libgit2, no fork/exec). Set `compact: true` to get a roll-up by status class (modified / untracked / deleted / etc.) with per-directory counts instead of every file — much smaller on large dirty trees.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": {"type": "string", "description": "Optional repo path relative to project root."},
                    "compact": {"type": "boolean", "default": false, "description": "Return a class+directory roll-up instead of the full per-file list."}
                }
            }
        },
        {
            "name": "git_log",
            "description": "Return a compact one-liner git log (SHA, author, date, summary). Default 50 commits from HEAD. Supports revision and path filtering.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": {"type": "string", "description": "Optional repo path relative to project root."},
                    "max_count": {"type": "integer", "minimum": 1, "default": 50},
                    "revision": {"type": "string", "description": "Optional revision to start from (branch, tag, or SHA)."},
                    "path": {"type": "string", "description": "Optional path to filter commits."}
                }
            }
        },
        {
            "name": "git_diff",
            "description": "Return a unified diff between two revisions or between a revision and the working tree.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": {"type": "string", "description": "Optional repo path relative to project root."},
                    "base": {"type": "string", "description": "Base revision (default: HEAD)."},
                    "target": {"type": "string", "description": "Target revision (if omitted, compares base against working tree)."},
                    "path": {"type": "string", "description": "Optional path filter."}
                }
            }
        },
        {
            "name": "search_grep",
            "description": "Regex search over the project tree (grep-searcher). Each hit carries `path`, `line_number`, `line`; set `context: N` to attach N lines before and after each match as `hit.context[]`. `compact: true` returns per-file buckets instead of per-line hits.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pattern": {"type": "string"},
                    "path": {"type": "string", "description": "Subdirectory relative to project root."},
                    "glob": {"type": "string", "description": "Override glob (e.g. '*.rs')."},
                    "max_results": {"type": "integer", "minimum": 1, "default": 200},
                    "case_insensitive": {"type": "boolean", "default": false},
                    "compact": {"type": "boolean", "default": false, "description": "Bucket hits per file instead of returning every matching line."},
                    "context": {"type": "integer", "minimum": 0, "maximum": 20, "default": 0, "description": "Lines of context before and after each match."}
                },
                "required": ["pattern"]
            }
        },
        {
            "name": "code_outline",
            "description": "Return the structural outline (top-level functions, types, classes, etc.) of a single source file via tree-sitter. Results are cached per file, invalidated by mtime + size. Supports rust, python, c, cpp, typescript, tsx, go. Unsupported extensions return an empty `entries` list with `language: null`. Set `signatures_only: true` to attach a compact declaration header (e.g. `fn foo(x: u32) -> bool`) to each entry — useful when the agent wants signatures but not bodies.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path relative to project root."},
                    "signatures_only": {"type": "boolean", "default": false, "description": "Populate each entry's `signature` with the declaration header up to the body."}
                },
                "required": ["path"]
            }
        },
        {
            "name": "code_outline_batch",
            "description": "code_outline for many files in one MCP call. Response is a parallel list of {path, result?, error?}; per-request failures do not abort the batch. Each request may opt into `signatures_only` independently.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "requests": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": {"type": "string"},
                                "signatures_only": {"type": "boolean", "default": false}
                            },
                            "required": ["path"]
                        },
                        "minItems": 1
                    }
                },
                "required": ["requests"]
            }
        },
        {
            "name": "code_symbols",
            "description": "Return a flat, de-duplicated list of top-level symbol names in a source file (function names, type names, etc.). Cheaper than code_outline when only names are needed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path relative to project root."}
                },
                "required": ["path"]
            }
        },
        {
            "name": "code_symbols_batch",
            "description": "code_symbols for many files in one MCP call. Response is a parallel list of {path, result?, error?}; per-request failures do not abort the batch.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "requests": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {"path": {"type": "string"}},
                            "required": ["path"]
                        },
                        "minItems": 1
                    }
                },
                "required": ["requests"]
            }
        },
        {
            "name": "tool_run",
            "description": "Run a local command with argv semantics from the project root or an in-root cwd. Output is capped per stream, failures include a combined stdout/stderr tail, and successful results can be cached until the watched tree version changes.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Executable name or absolute path. Use command 'sh' with args ['-lc', '...'] if shell behavior is required."},
                    "args": {"type": "array", "items": {"type": "string"}, "default": []},
                    "cwd": {"type": "string", "description": "Optional working directory relative to project root."},
                    "env": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "name": {"type": "string"},
                                "value": {"type": "string"}
                            },
                            "required": ["name", "value"]
                        },
                        "default": []
                    },
                    "max_output_bytes": {"type": "integer", "minimum": 1, "default": 65536, "description": "Per-stream output cap; keeps the last N bytes."},
                    "cache": {"type": "boolean", "default": false, "description": "Cache successful result while the daemon changelog version is unchanged."}
                },
                "required": ["command"]
            }
        },
        {
            "name": "tool_gh",
            "description": "Compact GitHub CLI adapter for `gh pr view` and `gh issue view`. Returns parsed JSON for the requested PR or issue.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "kind": {"type": "string", "enum": ["pr", "issue"]},
                    "selector": {"type": "string", "description": "Number, URL, or branch accepted by gh view. Omit for gh's contextual default where supported."},
                    "repo": {"type": "string", "description": "Optional OWNER/REPO override."},
                    "fields": {"type": "array", "items": {"type": "string"}, "description": "Optional gh JSON fields. Defaults to compact PR or issue fields."}
                },
                "required": ["kind"]
            }
        },
        {
            "name": "metrics_gain",
            "description": "Per-tool byte-savings counters: how many bytes the daemon would have shipped (raw_bytes) vs. how many it actually serialized (compacted_bytes), with a session-wide savings ratio. Useful for the agent to verify its compact-mode requests are paying off.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "metrics_tool_latency",
            "description": "Per-tool daemon-side wall-clock counters (calls, sum / mean / max in microseconds) across every RPC dispatched this session. Paired with `metrics_gain` so agents and the M5 benchmark can check that fork/exec saved actually translates to wall-clock saved per call.",
            "inputSchema": {"type": "object", "properties": {}}
        }
    ])
}
