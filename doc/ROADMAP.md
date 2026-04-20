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

## M3 - Language backends (next)

Plug-in shape: a backend is `trait LanguageBackend` with `outline`,
`definition`, `references`, `diagnostics`. The daemon owns one
backend instance per language and routes requests.

* Rust: shell out (once, long-lived) to `rust-analyzer` over its LSP, cache
  responses keyed on `ChangeLog` version.
* C++: same pattern with `clangd`, plus auto-discover `compile_commands.json`.
* Pure-text languages: tree-sitter only.

The cost of `rust-analyzer` startup is paid once per project, not once per
agent turn — that is the whole point of the daemon.

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

* `mimalloc` as the global allocator.
* Pre-allocated, reusable source-read buffer pool sized to cover common
  file sizes; avoids repeatedly asking the kernel for small pages on every
  `fs.read`.
* Arena allocators per request, freed in a single drop at end of dispatch
  — particularly important for tree-sitter parses and large context
  assembly, where per-object `free` pressure dominates otherwise.

## M5 - Multi-client + lifecycle (pending)

* Multiple MCP bridges connected to one daemon (already supported by the
  per-connection task; needs explicit testing under contention).
* Health check + auto-respawn from the bridge if the daemon went away.
* Optional systemd / launchd unit files.

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
