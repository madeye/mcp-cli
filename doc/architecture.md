# Architecture

Three crates and one shape: agent → bridge → daemon, with the
daemon doing all the heavy lifting and the bridge staying tiny so
spawn cost is irrelevant.

```
+------------------------+      stdio (MCP, JSONL)      +-----------------------+
|  Agent                 |  ◄────────────────────────► |  mcp-cli-bridge       |
|  (Claude Code, Codex,  |                              |  (crates/mcp-bridge)  |
|   IDE plugin, …)       |                              |                       |
+------------------------+                              +-----------+-----------+
                                                                    │
                                                  UDS, length-prefixed JSON-RPC
                                                  one socket per project root
                                                  ($XDG_RUNTIME_DIR/mcp-cli/<hash>.sock)
                                                                    │
                                                        +-----------v-----------+
                                                        |  mcp-cli-daemon       |
                                                        |  (crates/daemon)      |
                                                        |                       |
                                                        |  • mmap-backed VFS    |
                                                        |  • libgit2 status     |
                                                        |  • grep-searcher      |
                                                        |  • tree-sitter cache  |
                                                        |  • notify-rs watcher  |
                                                        |  • compact + metrics  |
                                                        |  • backends registry  |
                                                        +-----------------------+
```

## Crates

* **`crates/protocol`** — pure serde types shared by bridge and
  daemon. `Request` / `Response` / `RpcError`, every params /
  result struct, and the `methods::*` constants. **Add new RPC
  methods here first**; both sides depend on this crate.

* **`crates/daemon`** — the `mcp-cli-daemon` binary. Owns the hot
  state. Single tokio runtime, one task per accepted connection.
  Modules:
  * `main.rs` — clap args, tracing, calls `server::serve`.
  * `server.rs` — binds the UDS, spawns watcher + (optional)
    prewarm, accepts connections, dispatches frames to
    `handlers`. Times every dispatch into the metrics module.
  * `framing.rs` — length-prefixed JSON frame codec on top of
    tokio `UnixStream`.
  * `handlers.rs` — per-method logic. Each non-trivial method
    has a `*_inner` core so single-shot and `*_batch` variants
    share semantics.
  * `changelog.rs` — bounded ring buffer with a monotonic
    `version` cursor. Backs `fs.snapshot` / `fs.changes`.
  * `watcher.rs` — notify-rs (FSEvent / inotify). Feeds the
    ChangeLog and evicts stale entries from the ParseCache.
    Gitignore-aware, hard-excludes `.git/`.
  * `search_cache.rs` — LRU of `search.grep` results keyed by
    `(pattern, glob, path, max_results, case_insensitive,
    context, version)`. Flushed on ChangeLog version bump so
    cache coherence is *physically* impossible to drift.
  * `parse_cache.rs` — per-file tree-sitter `Tree` LRU,
    validated by `(mtime_ns, size)` stat on every access.
  * `backends/` — `LanguageBackend` trait + `BackendRegistry`.
    `TreeSitterBackend` is the default generalist; future
    rust-analyzer / clangd backends register ahead of it.
  * `compact/` — primitives (`git_status_compact`,
    `search_grep_compact`) that reshape responses into
    bucket form when callers pass `?compact: true`.
  * `metrics.rs` — per-tool counters: `(raw, compacted, calls)`
    surfaced via `metrics.gain`, plus `(calls, sum, mean, max)`
    latency surfaced via `metrics.tool_latency`.
  * `buffer_pool.rs` — bounded recycled `Vec<u8>` pool used by
    the per-connection frame reader and response serializer.
  * `prewarm.rs` — startup walker that pages source files into
    the OS cache. Disable with `--no-prewarm` for benchmarks.

* **`crates/mcp-bridge`** — the `mcp-cli-bridge` binary. Tiny.
  `mcp.rs` implements the MCP stdio server and translates
  `tools/call` into JSON-RPC; `daemon_client.rs` holds the UDS
  connection and reconnects on transport-level failures (broken
  pipe, EOF, ECONNREFUSED) by re-spawning the daemon through the
  same auto-spawn path used at startup.

* **`crates/mcp-cli`** — installer wrapper. Writes the agent
  config (Claude Code via `claude mcp add`, Codex via
  `toml_edit` upserts on `~/.codex/config.toml`). `--prefer-mcp`
  also writes `[features] shell_tool = false` and per-tool
  `approval_mode = "approve"` for Codex (see
  [`INTEGRATION.md`](./INTEGRATION.md)).

## Lifecycle

* The bridge is cheap to spawn per agent session — heavy state
  lives in the daemon.
* On startup the bridge tries to connect to the per-cwd socket.
  On `ENOENT`/`ECONNREFUSED` it `fork+exec`s
  `mcp-cli-daemon --root <cwd> --socket <derived>`, detaches
  with `setsid`, redirects stdio to a per-socket `.log`, and
  retry-connects with 25 ms→320 ms backoff up to 2 s.
* The daemon idle-exits after `--idle-timeout` (default `30 m`,
  `0` disables) with no active bridges. The bridge auto-respawns
  it on the next call if needed.
* Multi-bridge: N bridges can share one daemon. Each connection
  is its own tokio task; the daemon's per-tool caches are
  shared. Tested in `crates/mcp-bridge/tests/multibridge.rs`.

## Invariants worth knowing

* **Protocol changes are cross-crate.** Touch `protocol/` first;
  rebuild bridge + daemon together.
* **`search.grep` cache coherence** depends on the watcher
  advancing the ChangeLog version on every mutation. A watcher
  regression silently makes the LRU stale. Tests in
  `search_cache.rs` and `changelog.rs` cover this.
* **`fs.changes` clients must handle `overflowed: true`** by
  re-snapshotting. Slow clients + small `--changelog-capacity`
  can trip this.
* **Socket cleanup**: `server::serve` unlinks the socket on
  startup (in case of a stale leftover) and on clean shutdown.
  A crashed daemon leaves a stale socket; the next start
  removes it.
* **Per-cwd socket derivation** uses an FNV-1a hash of the
  canonical project root so the path is stable across Rust
  versions. Parent dir created mode 0700.

## Where to add new things

| You want to … | Add it to … |
|---|---|
| New RPC method | `protocol/src/lib.rs` (struct + method constant), then daemon `handlers.rs` and `server.rs` dispatch, then bridge `mcp.rs` (dispatch + tool definition). |
| New language for `code_outline` / `code_symbols` | `daemon/src/languages.rs` — variant + extension case + grammar + outline query string. |
| New compact / context formatter on an existing tool | Add a primitive to `daemon/src/compact/` if reusable; otherwise inline in the handler. |
| New language backend (rust-analyzer, clangd, …) | Implement `LanguageBackend` in `daemon/src/backends/<name>.rs`; register ahead of `TreeSitterBackend` in `server.rs`. |
| New install target (Cursor, Windsurf, …) | New module in `crates/mcp-cli/`, mirror `claude_code.rs` / `codex.rs`. Wire into `Target` enum + `install`/`uninstall`/`status`. |

For the full RPC reference see [`PROTOCOL.md`](./PROTOCOL.md).
For agent-side integration see [`INTEGRATION.md`](./INTEGRATION.md).
For the daemon's measured fork/exec / token wins on a real
workload see [`bench/codex-forkexec/`](../bench/codex-forkexec/).
