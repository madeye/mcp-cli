# mcp-cli

A sidecar-daemon + MCP bridge designed to give CLI/IDE-based AI agents
(Claude Code, Codex, etc.) deep access to a project's filesystem, git state,
and source code **without paying the per-call fork/exec tax** of traditional
shell-tool wrappers.

## Headline numbers

Both tables below come from single-sample runs on 2026-04-21 with
the agent analysing `openai/codex@rust-v0.122.0` (identical prompt,
same host). Full writeups linked in each section; earlier iterations
live alongside under
[`bench/codex-forkexec/results/`](./bench/codex-forkexec/results/) and
[`bench/claudecode-forkexec/results/`](./bench/claudecode-forkexec/results/).

### Codex

With `--sandbox danger-full-access` (the Seatbelt overhead on
baseline's shell calls was concealing the win; full analysis in
[`bench/codex-forkexec/results/2026-04-21-rust-v0.122.0-sandbox-ablation.md`](./bench/codex-forkexec/results/2026-04-21-rust-v0.122.0-sandbox-ablation.md)):

| metric | baseline | cold mcp-cli | delta |
|---|---:|---:|---:|
| `execve` total | 64 | 22 | **−66 %** |
| wall clock (s) | 209 | 117 | **−44 %** |
| input tokens | 1 931 010 | 694 446 | **−64 %** |
| cached input tokens | 1 811 072 | 571 776 | −68 % |
| MCP calls on the daemon | 0 | 66 | *`search_grep` ×30, `fs_read` ×20, `code_outline` ×12, `fs_scan` ×2, `git_status` ×2* |

Warm pass cuts cached input another 649 k tokens cold → warm even
as the agent makes *more* calls — `search_cache` + `parse_cache` +
prewarm amortising work as intended.

### Claude Code

Full writeup in
[`bench/claudecode-forkexec/results/2026-04-21-rust-v0.122.0.md`](./bench/claudecode-forkexec/results/2026-04-21-rust-v0.122.0.md):

| metric | baseline | cold mcp-cli | delta |
|---|---:|---:|---:|
| `execve` total | 85 | 15 | **−82 %** |
| wall clock (s) | 219 | 208 | **−5 %** |
| output tokens | 13 584 | 11 747 | −14 % |
| MCP calls on the daemon | 0 | 29 | *`fs_read` ×17, `search_grep` ×9, `fs_scan` ×3* |

Claude shells out harder than Codex by default (85 vs 64 execves),
so the fork/exec win is bigger. Unlike Codex, Claude's cold pass
already *beats* baseline wall-clock. Warm pass drops cached input
−952 k tokens cold → warm despite making ~2× more calls.

Each step of progress is a result file under the two `results/`
dirs; they're worth reading as a sequence.

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
  buffer pool, M5 codex-forkexec + M5-twin claudecode-forkexec
  benches (three-pass baseline / cold / warm with full 6-col
  comparison), M7 compaction foundation (`?compact` on
  `git.status` / `search.grep`, `metrics.gain` +
  `metrics.tool_latency`), the `--prefer-mcp` path, and compound /
  batch MCP tools (`fs_read_batch`, `code_outline_batch`,
  `code_symbols_batch`, `search_grep ?context`) — which together
  closed the cold wall-clock regression so that cold mcp-cli is now
  faster than baseline on both Codex and Claude Code (see tables
  above).
* **Open** — rust-analyzer / clangd language backends, further
  compound/batch tool opportunities flagged by warm-pass call
  patterns, `io_uring` I/O, per-request arenas, optional LSP /
  WASI mounting surfaces.
