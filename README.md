# mcp-cli

A sidecar-daemon + MCP bridge designed to give CLI/IDE-based AI agents
(Claude Code, Codex, etc.) deep access to a project's filesystem, git state,
and source code **without paying the per-call fork/exec tax** of traditional
shell-tool wrappers.

## Headline Numbers

Significant wins across both major agents measured on 2026-04-21 (analysis of `openai/codex@rust-v0.122.0`).

### [Codex](bench/codex-forkexec/results/2026-04-21-rust-v0.122.0-sandbox-ablation.md)
* **−44 %** Wall-clock (209s → 117s)
* **−66 %** `execve` calls (64 → 22)
* **−64 %** Input tokens (1.9M → 0.7M)

### [Claude Code](bench/claudecode-forkexec/results/2026-04-21-rust-v0.122.0.md)
* **−5 %** Wall-clock (even on cold pass)
* **−82 %** `execve` calls (85 → 15)
* **−14 %** Output tokens

**Warm-cache bonus:** Re-running tasks on a resident daemon saves an additional **650k–950k tokens** per pass via tree-sitter parse and grep result caching.

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

Artifacts under `target/release/`: `mcp-cli-daemon`, `mcp-cli-bridge`,
and the `mcp-cli` installer.

## Install

One-command registration for the agents the installer knows about:

```sh
# Claude Code + Codex at once (auto-spawns the daemon on first tool
# call; no always-on process needed).
target/release/mcp-cli install
```

For **codex specifically**, also pass `--prefer-mcp` so codex
actually routes through the daemon instead of preferring its own
built-in Bash (this is the configuration the headline numbers were
measured under):

```sh
target/release/mcp-cli install --target codex --prefer-mcp
```

`--prefer-mcp` writes `[features] shell_tool = false` to
`~/.codex/config.toml` (so codex stops emitting Bash tool calls for
`cat` / `rg` / `git` / etc.) and sets per-tool
`approval_mode = "approve"` for every mcp-cli tool (so codex doesn't
prompt for approval in non-interactive `codex exec`).

Without `--prefer-mcp`, codex still *mounts* mcp-cli but keeps
reaching for Bash — see the v1 benchmark writeup for evidence.

The daemon auto-spawns per project root on the bridge's first
connect and idle-exits after 30 min of inactivity; you don't need
to run anything by hand. To always keep one resident, see
[`doc/services/`](./doc/services/).

## Status

See [`doc/ROADMAP.md`](./doc/ROADMAP.md) and [`doc/TODO.md`](./doc/TODO.md).

* **Done (M0–M6 + parts of M7)** — Daemon/Bridge skeleton, incremental watch sync, tree-sitter indexing (`rust`, `python`, `c`, `cpp`, `ts`, `go`), drop-in `mcp-cli install`, per-cwd auto-spawn, reconnect-on-dead, `mimalloc` + buffer pooling, full M5 performance benchmarks, and `git.log` / `git.diff` RPCs.
* **In Progress (M7)** — Token-killer compaction (`?compact` on `git.status`, `search.grep`, `fs.scan`; `?strip_noise` on `fs.read`), metrics telemetry, and the generic `tool.run` wrapper.
* **Open (M8–M10)** — Write-path with optimistic concurrency (`fs.apply_patch`), advanced structural tools (dependency graphs, dynamic folding), and deep git/process management.
