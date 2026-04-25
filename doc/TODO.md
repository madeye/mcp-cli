# TODO

Concrete, actionable items. Group headers track milestones in
[`ROADMAP.md`](./ROADMAP.md). Check items off as commits land.

## Hardening (M0/M1 follow-ups)

- [x] Make `ChangeLog` capacity configurable via `--changelog-capacity`.
- [x] Suppress `created_then_removed` pairs (file briefly existed, gone before
      next snapshot) so they don't show up as `removed`.
- [x] Add `fs.scan` so a client that sees `overflowed: true` can do a fresh
      full enumeration in one RPC instead of falling back to host-side `find`.
- [x] Honour nested `.gitignore` files (walk deepest-to-shallowest per event,
      invalidate the cached `.gitignore` for a directory when the file itself
      changes).
- [x] Tests:
    - [x] `ChangeLog`: ordering, coalescing, overflow watermark.
    - [x] `resolve_within`: rejects `..` traversal, rejects symlinks
          escaping root, accepts absolute paths inside root.
    - [x] `framing`: max-frame, EOF mid-frame, oversize length.
- [x] Bench: `cargo bench` comparing `fs.read` via daemon vs. `cat` fork.
      `crates/daemon/benches/fs_read.rs` — macOS baseline: daemon ~67 µs vs
      cat ~1.15 ms (~17× faster). Run with `cargo bench -p daemon --bench fs_read`.

## Indexing (M2)

- [x] Pre-warm walker that respects gitignore and pages source files in.
- [x] `tree-sitter` integration; wired for `rust`, `python`, `c`, `cpp`,
      `typescript` (+ `tsx`), `go` in `crates/daemon/src/languages.rs`.
- [x] `code.outline` RPC: file -> top-level definitions (`fn`, `struct`,
      `class`, `def`, etc.) with byte ranges and 1-based line numbers.
- [x] `code.symbols` RPC: flat, de-duplicated top-level symbol names.
- [x] `ParseCache` keyed on `(path, mtime_ns, size)`, with proactive
      eviction from the watcher on change events.
- [x] LRU for `search.grep` results keyed on `(pattern, glob, version)`.

## Language backends (M3) — done

Scope settled as generalist (tree-sitter + text tools across many
languages). Specialist backends (rust-analyzer, clangd) aren't on the
roadmap; the `LanguageBackend` trait leaves the door open if a future
use case ever justifies one.

- [x] `LanguageBackend` trait + registry. `crates/daemon/src/backends/`
      defines the trait and `BackendRegistry`; the daemon registers a
      `TreeSitterBackend` (wrapping the existing `ParseCache` + outline
      queries) by default. `code.outline` / `code.symbols` handlers
      dispatch through the registry.
- [x] Languages: rust, python, c, cpp, typescript, tsx, go. Adding a
      new one is a variant + grammar crate + query string in
      `languages.rs` — no per-language backend plumbing required.

## Drop-in install + per-cwd auto-spawn (M3) — done

- [x] `mcp-cli install` subcommand lives in a new `crates/mcp-cli`
      wrapper binary alongside `mcp-cli-daemon`/`mcp-cli-bridge`.
      Flags: `--target claude-code | codex | all` (default `all`),
      `--bridge-path`, `--dry-run`.
    - [x] Claude Code: shells out to `claude mcp add mcp-cli <path>`;
          skip if `claude mcp list` already has it. Surfaces the CLI's
          exit code on failure; degrades gracefully when `claude` is
          not on PATH.
    - [x] Codex: reads `~/.codex/config.toml`, merges a
          `[mcp_servers.mcp-cli]` entry via `toml_edit` to preserve
          user-authored keys, comments, and formatting. Idempotent.
    - [x] `mcp-cli uninstall` inverse (removes the registration).
    - [x] `mcp-cli status` reports per-target registration state.
- [x] Bridge: `--root` defaults to `std::env::current_dir()` when not
      passed. The agent's cwd becomes the project root.
- [x] Per-cwd socket path via `protocol::paths::socket_path_for`:
      `$XDG_RUNTIME_DIR/mcp-cli/<hash>.sock` on Linux,
      `/tmp/mcp-cli-<user>-<hash>.sock` elsewhere. FNV-1a hash keeps
      the derivation stable across Rust versions. Parent dir is
      created mode 0700.
- [x] Bridge auto-spawn: on `ENOENT`/`ECONNREFUSED`, `fork+exec`
      `mcp-cli-daemon --root <cwd> --socket <derived>`, detach with
      `setsid`, redirect stdout/stderr to a per-socket `.log`, and
      retry-connect with 25ms→320ms backoff up to 2s.
- [x] Daemon `--idle-timeout <duration>` (default `30m`; `0` or empty
      disables). Tracks last-idle timestamp via `IdleTracker`; exits
      cleanly when the timer elapses with no active bridges.
- [x] Bridge `--daemon-arg` passthrough (repeatable) lets callers
      forward flags like `--idle-timeout 5m` to the spawned daemon.
- [x] End-to-end smoke test (`crates/mcp-bridge/tests/autospawn.rs`):
      starts the bridge in a tempdir cwd with an isolated
      `XDG_RUNTIME_DIR`, drives `initialize`, `tools/list`,
      `tools/call fs_read`, and waits for the daemon to idle-exit.

## I/O ceiling (M4)

- [x] Switch global allocator to `mimalloc` behind a default-on feature
      flag (`crates/daemon/Cargo.toml`). Opt out with
      `cargo build -p daemon --no-default-features` for valgrind /
      heaptrack / ASan runs.
- [x] Recyclable `Vec<u8>` `BufferPool` (`crates/daemon/src/buffer_pool.rs`)
      wired into the per-connection frame reader so request frame
      buffers are reused across calls instead of allocated per request.
- [ ] Per-request arena allocator for response building (hot path:
      tree-sitter parse + context assembly). Deferred — substantial
      lifetime threading required for `bumpalo` integration.

### Wall-clock regression follow-ups (deferred from PR #21)

The M5 benchmark v5 run brought the regression vs pure-bash baseline
down from +76 % to +61 % via two server-side reductions
(`fs_read_batch`, `search_grep ?context`). Two more leverage points
identified but not pursued in this iteration:

- [x] Compound `code_symbols_batch` / `code_outline_batch` taking
      `requests: Vec<{path}>` so a multi-file structural pass is
      one MCP turn. Same shape as `fs_read_batch`: per-item
      `result` / `error`; per-request failures don't abort.
      Integration test exercises both real-source and missing-path
      paths through the bridge → daemon → tree-sitter loop.
- [x] Cross-session warm cache measurement. Bench now runs a third
      `mcp_warm` pass against the still-running daemon from the cold
      pass (`--daemon-arg=--idle-timeout=30m` pinned into the
      generated config); `compare.py` renders a 6-col table with a
      `Δ vs cold` column. Claude-Code twin (`bench/claudecode-forkexec`)
      built on the same shape. See PRs #27, #28, #33, #34.
- [x] Re-bench and update the README headline. 2026-04-21 results
      under `bench/codex-forkexec/results/2026-04-21-rust-v0.122.0-sandbox-ablation.md`
      and `bench/claudecode-forkexec/results/2026-04-21-rust-v0.122.0.md`;
      top-level `README.md` now lists both agents' cold mcp-cli wins
      (Codex −44 %/−64 %, Claude −82 %/−5 %).
- [x] Extend the buffer pool to response serialization. New
      `BufferPool::acquire_with_capacity(min)` method; `handle_conn`'s
      response-write path now goes through `write_response_pooled`
      which serializes JSON directly into a recycled buffer (1 KiB
      starting capacity covers most responses without growth
      reallocations). `parse_cache` source reads remain on
      `std::fs::read` because the source `Vec` is held by the cache
      entry for the file's lifetime — no recycling opportunity there.
- [ ] Linux: experiment with `io_uring` for `fs.read` and walker I/O. Gate
      behind `--io-uring`.
- [ ] Thread-per-core tokio runtime with per-worker `io_uring` rings
      (no cross-core SQ contention). Depends on the `io_uring` item above.
- [ ] Binary `fs.read` mode: return raw bytes via a side channel for files
      above N KiB instead of JSON-encoded `String`.
- [ ] Zero-copy large-response path: `splice` file bytes directly into the
      socket for responses above the threshold, skipping the JSON
      round-trip entirely.

## Codex fork/exec reduction benchmark (M5)

Reproducible measurement that the daemon actually erases per-call
kernel overhead. The current headline run still lives under
`bench/codex-forkexec/`; five result files now sit under
`bench/codex-forkexec/results/`, and the
[v5 run](../bench/codex-forkexec/results/2026-04-20-rust-v0.121.0-search-ctx.md)
is the README headline today. There is now also a Claude Code twin
under `bench/claudecode-forkexec/`: same target workload, but a
three-pass `baseline` / `cold mcp-cli` / `warm mcp-cli` shape so the
daemon's cache reuse shows up explicitly.

- [x] `bench/codex-forkexec/run.sh` orchestrator: clone target at
      its latest release tag, run the analysis prompt twice
      (baseline + mcp-cli-plugin), capture per-run trace +
      stdout. Two tracer backends (strace / shim); shim is the
      macOS path (PATH-shadow shim wrappers + ZDOTDIR override +
      sandbox `--add-dir` whitelist) and the Linux fallback when
      `strace` isn't installed. An earlier `dtruss` backend was
      dropped because it required root on macOS.
- [x] `bench/codex-forkexec/prompt.md`: the analysis task.
- [x] `bench/codex-forkexec/parse_trace.py`: count `execve`
      events per binary from each tracer's output.
- [x] `bench/codex-forkexec/compare.py`: tabulate baseline vs
      with-mcp deltas (per-binary, wall clock, tokens, MCP tool
      calls grouped by `server/tool`).
- [x] Per-tool daemon-side latency counters via the M7 metrics module
      (`crates/daemon/src/metrics.rs`). New `metrics.tool_latency` RPC
      + bridge tool `metrics_tool_latency` returns calls / sum / mean /
      max microseconds per dispatched method. `dispatch` records the
      elapsed wall-clock for every call (success or error), so latency
      tracking is automatic with no per-handler instrumentation.
      Bench `run.sh` snapshots the counters into
      `mcp.metrics.tool_latency.json` after the with-mcp run and
      `compare.py` renders a per-tool latency table when present.
- [x] macOS support beyond the Linux baseline (dropped).
      The original plan was to gate a `dtruss` trace step on
      `id -u == 0` and document the sudo workflow. Decision:
      `dtruss` requires root, which is out of scope for this
      bench — we don't want users running an agent benchmark
      under sudo, and the shim backend already covers the
      headline binaries without root. `dtruss` was removed from
      `run.sh`, `compare.py`, and `parse_trace.py` across both
      `bench/codex-forkexec/` and `bench/claudecode-forkexec/`;
      macOS now always uses shim mode. Limitation noted in both
      READMEs under "Caveats": shim mode only counts the binaries
      in its allowlist.

## Lifecycle (M6)

(Auto-spawn + per-cwd socket routing moved to M3 under "Drop-in
install". What remains here is hardening + optional system integration.)

- [x] Bridge: detect daemon-dead mid-session, drop the stale stream,
      fall through to the M3 auto-spawn path, retry the call once.
      `DaemonClient` now owns its `ConnectConfig` so it can reconnect
      on its own. Regression test in
      `crates/mcp-bridge/tests/reconnect.rs` kills the daemon
      mid-session and asserts the next call still succeeds.
- [x] Multi-bridge contention test: 4 bridges driving one daemon
      concurrently, each running 8 fs_read calls.
      `crates/mcp-bridge/tests/multibridge.rs` asserts no
      cross-talk (each bridge sees its own file's content), no
      per-bridge starvation, and that the daemon idle-exits cleanly
      after all bridges disconnect.
- [x] systemd user-service unit + launchd plist examples
      (`doc/services/`) for users who prefer an always-on daemon
      over demand-spawn. Includes a README explaining when the
      always-on shape earns its keep.

## Token-killer compaction layer (M7)

Inspired by [`rtk`](https://github.com/rtk-ai/rtk). Goal: shrink
tool-output bytes 60–90 % so the agent burns less context per call.

- [x] `crates/daemon/src/compact/` module with grouping primitives
      (`git_status_compact`, `search_grep_compact`); table-driven unit
      tests cover the status-class precedence rules and per-file
      bucketing. `filter` / `truncate` / `dedupe` primitives still
      pending — added on demand as future formatters need them.
- [x] `git.status` compact mode: group by status class, per-directory
      counts, drop `clean`. `?compact: bool` param defaults off; flip
      default later once we've measured parity in the M5 benchmark.
- [x] `search.grep` compact mode: bucket by file with match count +
      first/last line numbers; full-detail still emitted when
      `compact: false` (default).
- [x] `code.outline` `signatures-only` formatter. `CodeOutlineParams`
      grows `signatures_only: bool` (default `false`); when set, each
      entry carries a `signature` field with the declaration header
      (body stripped, whitespace collapsed). Body detection walks the
      tree-sitter node's `body` field first, then `BODY_KINDS`
      descendants up to depth 4 — handles the Go `type_declaration ->
      type_spec -> struct_type` shape without per-language code.
      Bodiless declarations (constants, type aliases, unit structs)
      fall back to the first line. Bridge schemas + `doc/PROTOCOL.md`
      updated; unit tests cover rust/python/ts/go/c.
- [x] `fs.read` `strip_noise` flag. `FsReadParams` grows
      `strip_noise: bool` (default `false`); when set and `offset == 0`,
      the daemon runs three detectors over the content: (a) leading
      license-header comments (Copyright / SPDX / etc., ≥ 3 lines),
      (b) runs of ≥ 5 base64-ish lines (≥ 60 chars each, alnum
      + `+/=-_`), and (c) bodies of files tagged `@generated` / `DO NOT
      EDIT` in the first 10 lines and ≥ 50 lines long. Each stripped
      region becomes a single `[[mcp-cli: stripped N-line <kind>]]`
      marker; `FsReadResult::stripped_regions` reports the original
      1-based line range so callers can request specific lines back.
      Bridge schemas + `doc/PROTOCOL.md` + `doc/INTEGRATION.md`
      updated; 13 unit tests cover license / base64 / generated /
      shebang preservation / overlap resolution.
- [ ] Generic `tool.run` RPC. Takes a shell command + cwd and
      applies three tool-agnostic primitives:
    - **Tee-on-failure** — raw stdout/stderr to
      `${XDG_CACHE}/mcp-cli/tee/<hash>.log` on non-zero exit; the
      path comes back in the response so the agent can `fs.read` the
      full output only when the summary isn't enough.
    - **Byte-cap truncation** — head N lines + tail N lines with a
      `(X lines elided)` marker, configurable cap.
    - **`(command, cwd, file-mtime-fingerprint)` LRU cache** —
      reuses the `search_cache` pattern. Re-running with no source
      changes returns the cached result instead of re-executing.

      Deliberately language-agnostic. Per-framework JSON-format
      adapters (`cargo --message-format=json`, `pytest --json-report`,
      `jest --json`, `ruff --output-format=json`, etc.) are specialist
      work that scales poorly (N×M adapters to track upstream format
      drift) — same reasoning as the M3 rust-analyzer/clangd pivot.
      Callers who want tighter per-tool output can wrap the command
      themselves before calling `tool.run`.
- [ ] `tool.gh` adapter for `pr list`, `pr view`, `issue list`,
      `run list` (via `gh ... --json ...`). Kept as a named tool
      because `gh` is one binary everywhere and the JSON shape is
      stable — the usual specialist-maintenance argument doesn't
      apply.
- [x] `fs.scan ?compact` — directory tree roll-up with per-directory
      counts (`src/ (8 files)`) instead of the flat path list. Same
      compaction primitive shape as `git.status ?compact` and
      `search.grep ?compact`. `FsScanParams` grows `compact: bool`
      (default `false`); in compact mode the response carries a
      `{by_dir: [{dir, count}], total}` roll-up keyed by immediate
      parent directory, ordered by count descending, with the tail
      collapsed into a synthetic `(other)` row beyond 32 rows.
      Top-level files bucket as `"."`.
- [ ] `git.log` RPC — one-line commits, optional `since` / `author`
      / `max_count`. Small handler; fills the gap agents currently
      patch with raw `Bash("git log …")`.
- [ ] `git.diff` RPC — condensed patch (unchanged file-header chrome
      stripped, context lines shrunk). Larger than `git.log`; useful
      for PR-review flows.
- [x] `metrics.gain` RPC (`crates/daemon/src/metrics.rs`): per-tool
      counters of (raw_bytes, compacted_bytes, calls). Backed by
      atomics on the daemon side; cheap to keep and read. Bridge
      tool name `metrics_gain`.
- [ ] Bridge tool definitions for the new MCP surface, plus
      `doc/INTEGRATION.md` snippets showing the agent which tool to
      prefer over raw `Bash(...)` for each common workflow.

## Docs / polish

- [x] Architecture overview in [`doc/architecture.md`](./architecture.md)
      — ASCII diagram, per-crate module map, lifecycle
      invariants, "where to add new X" cheat sheet.
- [x] `doc/PROTOCOL.md` listing every RPC method, params, result,
      error codes, plus the bridge → daemon name mapping and
      design rules for adding new methods.
- [x] `doc/INTEGRATION.md` with copy-pastable Claude Code / Codex
      configs, the `--prefer-mcp` recipe, and the full MCP tool
      surface table.
