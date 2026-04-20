# mcp-cli

A sidecar-daemon + MCP bridge designed to give CLI/IDE-based AI agents
(Claude Code, Codex, etc.) deep access to a project's filesystem, git state,
and source code **without paying the per-call fork/exec tax** of traditional
shell-tool wrappers.

## Headline numbers

Measured on codex (`rust-v0.121.0`) analysing its own source tree —
single-sample, full writeup in
[`bench/codex-forkexec/results/2026-04-20-rust-v0.121.0-prefer-mcp.md`](./bench/codex-forkexec/results/2026-04-20-rust-v0.121.0-prefer-mcp.md):

| metric | baseline | with mcp-cli | delta |
|---|---:|---:|---:|
| `execve` total | 103 | 22 | **−79 %** |
| `rg` invocations | 24 | 0 | −100 % |
| `sed` invocations | 58 | 2 | −97 % |
| input tokens | 2,159,617 | 1,755,579 | **−19 %** |
| output tokens | 9,896 | 8,641 | −13 % |
| MCP calls on the daemon | 0 | 124 | *`fs_read` ×50, `search_grep` ×70, `fs_scan` ×4* |

Wall clock regressed 45 % on this run — codex made ~2.7× more turns
because each MCP call is atomic while a bash command can be a
pipeline. The bench writeup walks through which compound/batch MCP
tools would close the gap (kernel overhead and token budget are
already wins; wall-clock recovery needs MCP-tool-shape work, not
daemon-perf work). See [`bench/codex-forkexec/`](./bench/codex-forkexec/)
to reproduce.

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

See [`doc/roadmap.md`](./doc/roadmap.md) and
[`doc/todo.md`](./doc/todo.md).

* **Done** — skeleton, incremental sync (`fs.snapshot` / `fs.changes`
  / `fs.scan`), tree-sitter `code.outline` / `code.symbols` parse
  cache, drop-in install (`mcp-cli install`), per-cwd auto-spawn,
  reconnect-on-dead, multi-bridge contention test, mimalloc +
  buffer pool, first M5 benchmark numbers, M7 compaction foundation
  (`?compact` on `git.status` / `search.grep`, `metrics.gain` +
  `metrics.tool_latency`), and the `--prefer-mcp` path that made
  the headline numbers above actually move.
* **Open** — rust-analyzer / clangd language backends, compound /
  batch MCP tools to close the wall-clock regression from the
  benchmark, `io_uring` I/O, per-request arenas, optional LSP /
  WASI mounting surfaces.
