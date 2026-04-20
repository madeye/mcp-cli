# Roadmap

The thesis: stop treating agent tooling as a **discrete toolchain** invoked
per call, and start treating it as a **persistent code database** the agent
queries. Replace the per-call `fork`/`exec` cost of shell-wrapped tools
with a single hot user-space process that holds project state in RAM, and
shift the mental model from "Tool Call" (spawn, work, die) to "Service
Call" (round-trip to a resident service). Every milestone below is judged
against one question — does it move the kernel-overhead-per-tool-call
closer to zero without giving up correctness?

## M0 - Working skeleton (done)

* Cargo workspace: `protocol`, `daemon`, `mcp-bridge`.
* UDS + length-prefixed JSON-RPC framing.
* `fs.read` via `mmap`.
* `git.status` via `libgit2` (no `git` fork/exec).
* `search.grep` via `grep-searcher` (ripgrep's library).
* MCP stdio bridge that exposes those as `tools/call`.
* GitHub Actions CI: fmt + clippy + test on every PR.

## M1 - Incremental sync (done)

* `inotify` (Linux) / `FSEvent` (macOS) via `notify-rs`.
* Bounded ring buffer + monotonic version cursor (`ChangeLog`).
* `fs.snapshot` returns the cursor; `fs.changes(since: u64)` returns
  per-path coalesced events. `overflowed: true` when the client fell behind.
* Gitignore-aware filter, hard exclusion of `.git/`.

## M2 - Indexing (done)

Goal: make `search.grep` and future semantic queries cheap on cold caches.

* [x] Background pre-warm: walk the tree at startup, prime the page cache
  for source files (`prewarm.rs`). Path → blob-hash table still TODO
  (deferred; LRU + tree-sitter cover the query-latency case for now).
* [x] Result deduplication LRU for `search.grep`, keyed on query and
  invalidated on `ChangeLog` version bump (`search_cache.rs`).
* [x] Tree-sitter parse cache (`parse_cache.rs`). Per-file cache keyed
  on path and validated by `(mtime_ns, size)`; the watcher also
  evicts on change events to release memory proactively.
* [x] `code.outline` + `code.symbols` RPCs exposed as MCP tools, backed
  by per-language tree-sitter queries (`languages.rs` / `outline.rs`).
  Supported: rust, python, c, cpp, typescript, tsx, go.

## M3 - Language backends + drop-in install (in progress)

Two independent tracks, both landing in M3. The language work makes the
daemon *smarter*; the install work makes it something a user can actually
adopt in under 30 seconds. The install track has shipped; language
backends remain.

### Language backends

Plug-in shape: a backend is `trait LanguageBackend` with `outline`,
`symbols` today and `definition`, `references`, `diagnostics` to come.
The daemon owns a `BackendRegistry` and routes each request to the
first registered backend that claims the file's language.

* Trait + registry + tree-sitter backend: **landed**
  (`crates/daemon/src/backends/`). The handlers no longer touch the
  `ParseCache` directly — they go through `Daemon::backends`.
* Rust: shell out (once, long-lived) to `rust-analyzer` over its LSP, cache
  responses keyed on `ChangeLog` version. (pending)
* C++: same pattern with `clangd`, plus auto-discover `compile_commands.json`.
  (pending)
* Pure-text languages: tree-sitter only (already landed in M2).

The cost of `rust-analyzer` startup is paid once per project, not once per
agent turn — that is the whole point of the daemon.

### Drop-in install + per-project auto-spawn (done)

One-command install + per-project auto-spawn have shipped. New
`crates/mcp-cli` installer binary, bridge auto-spawn with backoff,
daemon idle-exit timer. End state already achieved:

    mcp-cli install               # once, globally
    claude                         # any project → warm daemon on demand

Details, all implemented:

* **One-command registration** via `mcp-cli install [--target claude-code|codex|all]`.
  * Claude Code — shells out to `claude mcp add mcp-cli <bridge>` (idempotent; checks `claude mcp list`).
  * Codex — `toml_edit` upsert of `[mcp_servers.mcp-cli]` in `~/.codex/config.toml`, preserving user comments.
  * `uninstall` + `status` subcommands for the inverse and a reporting view. `--dry-run` on both write paths.
* **Per-cwd socket** in `protocol::paths`: FNV-1a hash of the canonical cwd → `$XDG_RUNTIME_DIR/mcp-cli/<hash>.sock` or `/tmp/mcp-cli-<user>-<hash>.sock`. Parent dir created mode 0700.
* **Bridge auto-spawn**: on `ENOENT`/`ECONNREFUSED`, fork+exec the daemon, `setsid` to detach, redirect stdio to a per-socket `.log`, retry-connect with 25ms→320ms backoff up to 2s. Daemon binary resolved next to the bridge (then PATH).
* **Default `--root = $PWD`**: bridge and daemon both default to cwd when `--root` is omitted.
* **Idle-exit on the daemon**: `--idle-timeout` (default `30m`, `0` disables). Humantime-parsed; tracked via an `IdleTracker` that fires a clean shutdown after the timeout elapses with no active bridges.
* **Forwarding passthrough**: bridge `--daemon-arg=<flag>` (repeatable) forwards to the spawned daemon, enabling tests and power users to tune the daemon without hand-editing source.

End-to-end smoke test in `crates/mcp-bridge/tests/autospawn.rs` exercises the full path.

## M4 - I/O ceiling (pending)

Squeeze out the last per-syscall overhead. Two axes: reduce time spent in
the kernel, and reduce time spent in the allocator.

**Kernel side — thread-per-core, zero-syscall I/O.**

* Linux: `io_uring` submission queue for batched `fs.read` and tree walks.
  Submission + completion queues shared with the kernel give us
  effectively zero syscalls on the hot path for file reads.
* Pin one tokio runtime worker per core, with per-thread `io_uring`
  instances — no cross-core contention on the submission queue.
* Zero-copy frame writes for `fs.read` responses larger than a threshold
  (send `splice`d bytes directly, skip the JSON `String` round-trip).

**Allocator side — pooled buffers, per-request arenas.**

* `mimalloc` as the global allocator. **landed** behind the daemon's
  default-on `mimalloc` feature flag; `--no-default-features` falls
  back to the system allocator for profiling tools.
* Pre-allocated, reusable buffer pool (`buffer_pool.rs`). **landed**
  for per-connection request frames; future work extends it to the
  response-serialization path and parse-cache source reads.
* Arena allocators per request, freed in a single drop at end of dispatch
  — particularly important for tree-sitter parses and large context
  assembly, where per-object `free` pressure dominates otherwise. (pending)

## M5 - Codex fork/exec reduction benchmark (pending)

The whole project's premise is "make per-call kernel overhead go to
zero by replacing fork/exec with a daemon round-trip." M5 is the
load-bearing measurement of that premise: a reproducible benchmark
that runs an identical agent task under Codex twice — once with
vanilla Codex tooling (`Bash` shells out to `cat`/`grep`/`git` for
every call) and once with the mcp-cli MCP plugin loaded
(`fs.read`/`search.grep`/`git.status` served by the daemon, no
fork/exec) — and reports the delta in `execve` syscall count,
wall-clock, and tokens consumed.

The task itself is deliberately self-referential: ask Codex to
analyze the **Codex repository at its own latest release tag** and
propose three concrete performance enhancements. Picking Codex's
own source has two benefits — the workload is realistic
(grep-heavy, git-heavy, lots of small file reads, the exact shape
mcp-cli is tuned for) and the prompt naturally re-uses the same
files as the agent walks the repo.

### What the benchmark measures

* **`execve` count** — tracer-counted (`strace -e trace=execve -f`
  on Linux, `dtruss -f -t execve` on macOS). The per-tool breakdown
  separates the binaries the daemon obviates (`cat`, `grep`, `rg`,
  `git`, `find`, `ls`, `head`, `tail`) from the rest, so a regression
  in mcp-cli coverage shows up as a binary that crept back into the
  trace.
* **Wall-clock** — total time from prompt accept to terminal `exit`.
  Captured per run for both modes.
* **Token consumption** — Codex's own usage stats (parsed from its
  stdout / `~/.codex/sessions/`).
* **Per-tool latency** (optional) — daemon-side instrumentation that
  records p50/p99 of `fs.read`, `search.grep`, `git.status`. Lets us
  catch a daemon-side regression that would otherwise hide behind
  "we still saved fork/exec, but each call got slower."

### Where it lives

`bench/codex-forkexec/` outside the cargo workspace. Driver is bash
+ python (no need to drag in a benchmarking crate); requires Codex
on `PATH` and either `strace` (Linux) or root + `dtruss` (macOS).

### Why this is a milestone, not just a script

* It is the shipping criterion for every other milestone. M3 / M4 /
  M6 each claim "fewer fork/execs" or "fewer bytes per call" — the
  benchmark is the only way to put a number on those claims.
* Running it in CI on every PR (eventually) lets us regression-gate
  on the kernel-overhead curve. A new feature that quietly shells
  out behind the scenes will show up immediately.
* The output table is the artifact we point at when explaining
  what mcp-cli buys you.

## M6 - Multi-client + lifecycle (pending)

Daemon auto-spawn and per-cwd socket routing move up into M3 (see the
*Drop-in install* track above). What remains here is hardening under
contention and optional system-integration surface.

* Multiple MCP bridges connected to one daemon (already supported by the
  per-connection task; needs explicit testing under contention).
* Health check + reconnect: **landed**. `DaemonClient` owns its
  connect config and detects transport-layer failures
  (`BrokenPipe` / `UnexpectedEof` / `ConnectionRefused` / `NotFound`
  / friends). On a dead-stream error mid-call it drops the stream,
  reconnects via the same M3 auto-spawn path, and retries the call
  once before surfacing the error.
* Optional systemd / launchd unit files for users who prefer an always-on
  daemon over demand-spawn.

## Integration strategies

Three ways to wire this daemon into an agent, in increasing order of
invasiveness and performance headroom. M0–M5 above assume the MCP path;
the LSP and WASI paths are parallel tracks, not sequential milestones.

### MCP plugin (current)

* **Plan.** Implement an MCP-compatible server; the agent (Claude Code,
  Codex, …) mounts it over stdio.
* **Integration.** `mcp-cli-bridge` speaks MCP on stdin/stdout and forwards
  `tools/call` to the daemon over UDS. Zero editor modification; any
  MCP-aware host picks it up.
* **Perf win.** Replaces the shell-wrapper cost (`fork` + `exec` + dynamic
  linker + per-call syscall stream) with a UDS round-trip on a pre-warmed
  socket. Heavy state (mmap, libgit2, tree-sitter, change ring) stays
  resident across calls instead of being rebuilt per invocation.

### LSP proxy

* **Plan.** Expose the same capabilities behind a Language Server
  interface, so the daemon attaches to the editor the way `rust-analyzer`
  or `clangd` does.
* **Integration.** Register as an LSP for the project's languages (or as a
  generic text LSP). Reuse the editor's already-open persistent connection;
  ride on top of `textDocument/didOpen` + `didChange` streams instead of
  running our own `notify-rs` watcher for open buffers.
* **Perf win.** Share the editor's parsed AST and open-buffer contents
  instead of re-reading from disk and re-parsing — the editor has already
  paid that cost. The watcher only has to cover *unopened* files, which
  collapses duplicate work on the hot set of files the user is actively
  editing.

### WASI extension

* **Plan.** Compile the daemon's tool surface (grep, scan, outline) to
  WebAssembly and load it inside the agent runtime's WASI sandbox.
* **Integration.** Ship a `.wasm` artifact the agent host instantiates in
  process. Calls are intra-process function invocations against a sandboxed
  module; no socket, no framing, no serde round-trip for large buffers.
* **Perf win.** Eliminates the IPC hop entirely — even a UDS round-trip
  costs a context switch per call, and WASM sandbox entry is cheaper than
  that. Linear memory is bounded and predictable, so the host can cap
  resource use without relying on OS-level limits.

## Open questions

* **Scope: specialist vs generalist.** Should the daemon be tuned for a
  small set of languages where we can go deep (Rust, C++ via
  rust-analyzer / clangd) and accept degraded behavior elsewhere, or stay
  language-agnostic (tree-sitter + text tools only) and let language
  backends be opt-in plugins? This decision shapes M3 and M5 priorities.

## Non-goals

* Becoming a general LSP. We sit *next to* LSPs and reuse their state.
* Network transport. UDS only; remote agents talk to a remote daemon, not
  this one.
* Sandboxing. The daemon trusts its caller; the agent's safety story is
  handled by the host (Claude Code, Codex, etc.).
