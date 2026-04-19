# mcp-cli

A sidecar-daemon + MCP bridge designed to give CLI/IDE-based AI agents
(Claude Code, Codex, etc.) deep access to a project's filesystem, git state,
and source code **without paying the per-call fork/exec tax** of traditional
shell-tool wrappers.

## Architecture

```
+---------------------+         stdio (MCP)         +----------------------+
|  Claude Code /      | <-------------------------> | mcp-cli-bridge       |
|  Codex / IDE        |                             | (MCP stdio server)   |
+---------------------+                             +----------+-----------+
                                                               |
                                              UDS + length-prefixed JSON-RPC
                                                               |
                                                    +----------v-----------+
                                                    | mcp-cli-daemon       |
                                                    |  - mmap VFS          |
                                                    |  - libgit2           |
                                                    |  - grep-searcher     |
                                                    |  - (future) tree-    |
                                                    |    sitter, LSP       |
                                                    +----------------------+
```

* **`crates/protocol`** - shared JSON-RPC request/response types between bridge
  and daemon.
* **`crates/daemon`** - long-lived `mcp-cli-daemon` process that owns the
  project. Listens on a Unix Domain Socket and exposes in-process tools:
  * `fs.read` - mmap-backed file read (no `read(2)` per call).
  * `fs.snapshot` / `fs.changes` - incremental sync. The daemon runs an
    inotify (Linux) / FSEvent (macOS) watcher with a gitignore-aware filter,
    maintains a monotonic version cursor, and returns coalesced
    created/modified/removed events since a client-supplied version. If the
    client falls too far behind, the response sets `overflowed: true` so the
    client can re-scan instead of silently missing events.
  * `git.status` - libgit2 status (no `git` fork/exec).
  * `search.grep` - ripgrep's `grep-searcher` library over the project tree.
* **`crates/mcp-bridge`** - small `mcp-cli-bridge` stdio binary that the agent
  spawns. Translates MCP `tools/call` requests into UDS calls. Cheap to spawn,
  but the heavy state stays in the daemon.

## Why this shape

Every traditional shell-wrapped tool call (`cat`, `git status`, `rg ...`) costs
a `fork` + `exec` + page-table setup + library loading + syscall stream + tear
down. For an agent that issues hundreds of small reads per task, that's
dominated by kernel overhead, not useful work. By keeping a single hot daemon:

* File reads come from an `mmap` already in the page cache - the kernel only
  pages in on misses; no per-call `read` syscall stream.
* Git, search, and (planned) parse operations stay in-process, so the kernel
  sees one long-lived process instead of thousands of short-lived ones.
* The bridge stays tiny and the per-call cost is a UDS round-trip on a single
  pre-warmed socket.

This is the foundation for later layers (incremental inotify diffs, tree-sitter
indexing, language-specific backends like clangd / rust-analyzer reuse).

## Build

```sh
cargo build --release
```

Artifacts:

* `target/release/mcp-cli-daemon`
* `target/release/mcp-cli-bridge`

## Run

Start the daemon for your project:

```sh
mcp-cli-daemon --socket /tmp/mcp-cli.sock --root /path/to/project
```

Register the bridge as an MCP server in your agent. Example for Claude Code's
`~/.config/claude/config.json`:

```json
{
  "mcpServers": {
    "mcp-cli": {
      "command": "/path/to/mcp-cli-bridge",
      "args": ["--socket", "/tmp/mcp-cli.sock"]
    }
  }
}
```

## Status

Skeleton. Working primitives: `fs_read`, `fs_snapshot`, `fs_changes`,
`fs_scan`, `git_status`, `search_grep`. Planned: tree-sitter indexing,
pluggable language backends, `io_uring` I/O path on Linux.
