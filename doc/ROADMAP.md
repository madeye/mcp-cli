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

## M3 - Language backends + drop-in install (done)

Two independent tracks, both landed in M3. The language work makes the
daemon *smarter*; the install work makes it something a user can actually
adopt in under 30 seconds.

### Language backends

Generalist scope: tree-sitter + text tools across many languages, with
a pluggable `trait LanguageBackend` + `BackendRegistry` so a specialist
(rust-analyzer, clangd, …) could be dropped in later if a use case
justifies it — but none are planned today.

* Trait + registry + tree-sitter backend: **landed**
  (`crates/daemon/src/backends/`). The handlers no longer touch the
  `ParseCache` directly — they go through `Daemon::backends`. Tree-sitter
  covers the `outline` / `symbols` surface we ship today; `definition`,
  `references`, and `diagnostics` would live on the same trait if the
  RPC surface ever grows into them.
* Supported languages: rust, python, c, cpp, typescript, tsx, go.
  Adding a language is a variant + grammar crate + query string in
  `languages.rs` — no per-language backend plumbing required.

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

## M5 - Codex fork/exec reduction benchmark (done)

The whole project's premise is "make per-call kernel overhead go to
zero by replacing fork/exec with a daemon round-trip." M5 is the
load-bearing measurement of that premise: a reproducible benchmark
that runs an identical agent task under Codex and Claude Code — once with
vanilla tooling (shelling out to `cat`/`grep`/`git` for every call)
and once with the mcp-cli MCP plugin loaded — and reports the delta
in `execve` syscall count, wall-clock, and tokens consumed.

* **Codex:** Measured −44 % wall-clock and −66 % `execve` count on
  a representative repo analysis task.
* **Claude Code:** Measured −82 % `execve` reduction, beating
  baseline wall-clock even on cold passes.
* **Warm-cache wins:** Measured −650k to −950k cached token
  savings on warm passes due to `search_cache` and `parse_cache`
  re-use.

The benchmark driver lives under `bench/codex-forkexec/` and
`bench/claudecode-forkexec/`.

## M6 - Multi-client + lifecycle (done)

Daemon auto-spawn and per-cwd socket routing have landed. The daemon
now handles multiple concurrent bridges and manages its own lifecycle
gracefully.

* **Multi-bridge support:** Multiple MCP bridges can connect to one
  daemon; tested under contention with `multibridge.rs`.
* **Health check + reconnect:** `DaemonClient` detects transport
  failures and automatically reconnects/retries on a dead stream.
* **Idle-exit:** Daemon tracks activity and exits cleanly after a
  configurable idle timeout (default 30m).
* **System integration:** Example `systemd` and `launchd` units
  provided in `doc/services/`.

## M7 - Token-killer compaction layer (done)

Inspired by [`rtk`](https://github.com/rtk-ai/rtk) — shrinking tool
responses before they cross the bridge to save context budget.

* [x] **Compaction foundation:** `crates/daemon/src/compact/` module
  provides grouping and filtering primitives.
* [x] **`git.status ?compact`**: Groups by status class, per-dir counts.
* [x] **`search.grep ?compact`**: Buckets hits by file with counts
  and line-range summaries.
* [x] **`code.outline ?signatures_only`**: Emits declaration headers
  without bodies.
* [x] **`fs.read ?strip_noise`**: Strips license headers, base64
  blobs, and generated-file boilerplate.
* [x] **`metrics.gain`**: Telemetry reporting raw vs. compacted
  byte counts.
* [x] **`git.log` / `git.diff`**: Specialized compact RPCs for
  git history and patches.
* [x] **`tool.run`**: Generic argv-based process wrapper with
  tee-on-failure, truncation, and result caching.
* [x] **`tool.gh`**: Compact GitHub CLI adapter for PR and issue views.


## M8 - Write path & Optimistic Concurrency (done)

The current roadmap focuses heavily on reads. But agents *write* code, and that's where they often break things. The daemon's `ChangeLog` makes it uniquely qualified to safely handle writes.

* [x] **`fs.apply_patch` / `fs.replace_all`** — Structured RPCs
  to apply unified diffs or literal search-and-replace. Because the
  daemon tracks `mtime` and its internal `ChangeLog` version, callers
  can pass `expected_mtime_ns` and/or `expected_version` for Optimistic
  Concurrency Control. If the file changed between the agent reading
  it and patching it, the daemon rejects the write before touching the
  file, preventing clobbers of user/concurrent edits.

## M9 - Advanced Structural Tools (pending)

Leveraging the resident `ParseCache` and tree-sitter to give agents instant architectural understanding without LSP overhead.

* **`code.imports` / `code.dependencies`** — Tree-sitter query to extract import/use statements. Builds a lightweight, resident, bi-directional dependency graph. Agents can instantly answer "What files import `src/auth.ts`?".
* **`code.find_occurrences` (Smart Grep)** — Tree-sitter powered search that only matches actual identifiers or function calls, ignoring matches in comments or strings.
* **`fs.read_skeleton` (Dynamic Folding)** — A hybrid of `fs.read` and `code.outline`. The agent requests a file but specifies a target line or symbol. The daemon returns the file with all irrelevant function bodies dynamically folded/elided (e.g., `// ... 50 lines elided ...`), preserving the file structure but drastically cutting tokens.

## M10 - Deep Git & Process Management (pending)

Expanding the capabilities of `libgit2` and expanding `tool.run` to handle async workflows.

* **`git.blame` (Compact)** — In-process blame via libgit2. Instead of token-heavy line-by-line output, it groups spans: "Lines 10-50 were modified in commit `abc123` by X (fixes #42)".
* **`git.history` (File-specific)** — Return the last N commit messages and authors that touched a specific file without flooding context.
* **`tool.background_job`** — Allow the agent to start a background process (`tool.spawn`), poll its ring-buffered output (`tool.read_logs`), and terminate it (`tool.kill`). The daemon manages the PTY and buffer pool, ensuring the agent doesn't get blocked on watch tasks or dev servers.

## Integration strategy

* **Plan.** Implement an MCP-compatible server; the agent (Claude Code,
  Codex, …) mounts it over stdio.
* **Integration.** `mcp-cli-bridge` speaks MCP on stdin/stdout and forwards
  `tools/call` to the daemon over UDS. Zero editor modification; any
  MCP-aware host picks it up.
* **Perf win.** Replaces the shell-wrapper cost (`fork` + `exec` + dynamic
  linker + per-call syscall stream) with a UDS round-trip on a pre-warmed
  socket. Heavy state (mmap, libgit2, tree-sitter, change ring) stays
  resident across calls instead of being rebuilt per invocation.

## Non-goals

* **Specialist language backends.** Scope is deliberately generalist
  (tree-sitter + text tools across many languages). Deep per-language
  features via `rust-analyzer` / `clangd` / similar would belong in the
  editor's own LSP, not here; the `LanguageBackend` trait leaves the
  door open without committing the project to the maintenance surface.
* **LSP proxy.** We don't expose our RPCs over LSP — agents mount us
  as an MCP plugin and editors already have their own LSPs.
* **WASI extension.** No in-process `.wasm` build of the tool surface;
  the UDS round-trip is cheap enough that the sandbox-entry win
  doesn't pay for the build + distribution complexity.
* **Network transport.** UDS only; remote agents talk to a remote daemon,
  not this one.
* **Sandboxing.** The daemon trusts its caller; the agent's safety story
  is handled by the host (Claude Code, Codex, etc.).
