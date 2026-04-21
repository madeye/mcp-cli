# mcp-cli

A sidecar-daemon + MCP bridge designed to give CLI/IDE-based AI agents
(Claude Code, Codex, etc.) deep access to a project's filesystem, git state,
and source code **without paying the per-call fork/exec tax** of traditional
shell-tool wrappers.

## Headline numbers

Measured on codex (`rust-v0.121.0`) analysing its own source tree
— single-sample, full writeup in
[`bench/codex-forkexec/results/2026-04-20-rust-v0.121.0-search-ctx.md`](./bench/codex-forkexec/results/2026-04-20-rust-v0.121.0-search-ctx.md)
(latest), with the [first run](./bench/codex-forkexec/results/2026-04-20-rust-v0.121.0-prefer-mcp.md)
and three intermediate iterations alongside.

| metric | baseline | with mcp-cli | delta |
|---|---:|---:|---:|
| `execve` total | 83 | 22 | **−73 %** |
| `rg` invocations | 13 | 0 | −100 % |
| `sed` invocations | 50 | 2 | −96 % |
| MCP calls on the daemon | 0 | 73 | *`search_grep` ×32 (31 with context), `fs_read` ×21, `code_symbols` ×6, …* |
| wall clock (s) | 201 | 324 | +61 % |

The wall-clock regression — every MCP call is atomic where bash
is a pipeline — narrowed run-over-run as we collapsed two-call
patterns server-side: 124 MCP turns / 375 s in the first run
(`prefer-mcp`), 73 MCP turns / 324 s after `search_grep ?context`
landed (this run). Each step of progress is a result file under
[`bench/codex-forkexec/results/`](./bench/codex-forkexec/results/);
they're worth reading as a sequence.

There is now a matching Claude Code twin benchmark under
[`bench/claudecode-forkexec/`](./bench/claudecode-forkexec/): same
target workload, but three passes (`baseline`, `cold mcp-cli`, `warm
mcp-cli`) so the daemon's cross-session cache payoff is visible too.
The Codex run above is still the headline result in the README; the
Claude Code twin broadens the measurement surface beyond a single
agent and is the next place to fold into the top-level story.

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
