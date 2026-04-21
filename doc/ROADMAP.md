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

* **`execve` count** — tracer-counted. Linux uses
  `strace -e trace=execve -f`; macOS uses the PATH-shadow shim
  mode, since the system-wide tracer (`dtruss`) requires root and
  that's deliberately out of scope. The per-tool breakdown
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
on `PATH` and optionally `strace` on Linux (macOS always uses the
no-root shim backend).

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

## M7 - Token-killer compaction layer (pending)

Inspired by [`rtk`](https://github.com/rtk-ai/rtk) — every byte the
agent sees costs context window. Today's daemon already wins by
keeping state hot; M7 wins by *shrinking the responses themselves*
before they cross the bridge. rtk reports 60–90 % savings on a
typical agent session by filtering, grouping, truncating, and
deduplicating tool output. We can do the same — and better, because
we already own the structured form (libgit2 statuses, ripgrep hits,
tree-sitter outlines) and don't have to re-parse formatted text.

Where rtk works as a Bash-hook proxy that rewrites shell commands,
mcp-cli's surface is MCP tool calls. Same compression strategies,
different mounting point: we expose new MCP tools (and tighter
formatters on existing ones) so an agent that prefers `grep_compact`
over raw `Bash("rg ...")` gets a token-budget win for free.

### Compaction primitives

A `Compact` trait in `crates/daemon/src/compact/` codifies rtk's four
strategies so individual tool handlers compose them rather than each
inventing their own format:

* `filter` — drop boilerplate (whitespace, banner lines, ASCII art,
  progress bars, license headers).
* `group` — fold many similar items into one bucket (search hits by
  file/dir, lint errors by rule, dependency tree by top-level module).
* `truncate` — keep head + tail with a "… N more" marker; never the
  raw middle that the agent will just discard.
* `dedupe` — collapse repeated lines with a count prefix, the way
  `uniq -c` would.

### Tighter formatters on existing tools

Existing handlers should grow a `?compact: bool` (or default-on)
mode that emits the rtk-style summary instead of the raw structure.
First targets:

* `git.status` — group by status class, show counts per directory,
  drop ignored/clean entries entirely. Today's per-file dump is the
  pre-rtk baseline.
* `search.grep` — bucket hits by file (one line per file with a
  match count + first / last line numbers), full-detail mode behind
  an explicit flag for when the agent really needs every line.
* `code.outline` — already structurally compact; add a `signatures-only`
  formatter (rtk `read --aggressive` equivalent) that pairs with
  `fs.read` for the "show me this file but not the bodies" workflow.
* `fs.read` — auto-strip noise for known formats (license headers,
  long base64 blobs, generated files) behind a `?strip-noise: bool`.

### New external-command wrappers

The big rtk wins are on commands the daemon doesn't own today: test
runners, linters, builders. M6 adds a `tool.run` family that shells
out (once, not per-call) to the underlying tool, parses its
structured output, and returns the compacted view:

* `tool.cargo_test`, `tool.cargo_clippy`, `tool.cargo_build` —
  consume cargo's JSON output (`--message-format=json`), surface
  failures + warnings only.
* `tool.test_runner` — generic adapter for `pytest --json-report`,
  `jest --json`, `go test -json`, `vitest --reporter=json`. Failures
  + summary counts, never the passing-test noise.
* `tool.lint` — `eslint --format json`, `tsc --pretty false`,
  `ruff check --output-format=json`, `golangci-lint run --out-format json`.
  Group by file → rule → line so the agent gets a histogram, not a
  log dump.
* `tool.gh` — bridge GitHub CLI for `pr list`, `pr view`, `issue list`,
  `run list`. Parses the JSON, drops avatar URLs / timestamps the
  agent doesn't need.

External commands stream into the daemon, so the bridge sees the
compacted form only — and the daemon can cache the parsed form keyed
on `(command, cwd, file-mtime-fingerprint)` the same way `search.grep`
caches by ChangeLog version.

### Token-savings telemetry

A `metrics.gain` RPC mirrors `rtk gain`: per-tool counters of raw
output bytes (what the underlying command would have sent) vs.
compacted bytes (what we actually serialized). Cheap to maintain
(two atomics per tool), surfaces to the user how much context budget
the daemon is buying back.

### Why this is the right fit for mcp-cli (and not just "shell out to rtk")

* We already own the structured form for the highest-frequency tools
  (`fs.read`, `search.grep`, `git.status`, `code.outline`). Compacting
  in-process is cheaper and more correct than re-parsing formatted text.
* `tool.run` shares the daemon's parse / file-watch / cache layers,
  so a `cargo test` whose dependency graph hasn't changed can return
  the cached structured failure list without re-running.
* The compaction layer is symmetric with the M3 backend layer:
  `LanguageBackend` plugs in deeper semantics; `Compact` plugs in
  tighter output. Both keep the RPC surface stable.

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
