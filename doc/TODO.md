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
- [ ] Bench: `cargo bench` comparing `fs.read` via daemon vs. `cat` fork.

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

## Language backends (M3)

- [x] `LanguageBackend` trait + registry. `crates/daemon/src/backends/`
      defines the trait and `BackendRegistry`; the daemon registers a
      `TreeSitterBackend` (wrapping the existing `ParseCache` + outline
      queries) by default. `code.outline` / `code.symbols` handlers
      dispatch through the registry — specialist backends can be
      registered ahead of tree-sitter for languages they cover.
- [ ] Rust backend: spawn `rust-analyzer` once, speak LSP, cache by
      `ChangeLog` version.
- [ ] C++ backend: spawn `clangd`, parse `compile_commands.json`.
- [ ] Backend health: detect crashes, auto-respawn, surface errors as RPC
      errors instead of dropping the connection.

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
      tree-sitter parse + context assembly).
- [ ] Extend the buffer pool to additional hot paths (response
      serialization, parse_cache source reads).
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
kernel overhead. Lives under `bench/codex-forkexec/`.

- [ ] `bench/codex-forkexec/run.sh` orchestrator: clone Codex at
      its latest release tag into a tempdir, run the analysis prompt
      twice (with and without the mcp-cli plugin), capture per-run
      strace / dtruss output + Codex stdout.
- [ ] `bench/codex-forkexec/prompt.md`: the analysis task asking
      Codex to identify three concrete performance enhancements in
      its own source tree, with file/line citations.
- [ ] `bench/codex-forkexec/parse_trace.py`: count `execve` events
      per binary from a trace file; emit JSON.
- [ ] `bench/codex-forkexec/compare.py`: tabulate baseline vs
      with-mcp counts (overall + per-binary delta) and absolute
      wall-clock + token deltas.
- [ ] Per-tool daemon-side latency counters (`metrics.tool_latency`)
      so the benchmark can also assert no per-call regression — a
      fork/exec saved that costs the same wall-clock isn't a win.
- [ ] CI job (manual / weekly) that runs the benchmark on a
      controlled runner with Codex pre-installed and posts the
      comparison table as a PR comment.
- [ ] macOS support beyond the Linux baseline: `dtruss` requires
      root, document the workflow and gate the script on
      `id -u == 0` for the trace step.

## Lifecycle (M6)

(Auto-spawn + per-cwd socket routing moved to M3 under "Drop-in
install". What remains here is hardening + optional system integration.)

- [x] Bridge: detect daemon-dead mid-session, drop the stale stream,
      fall through to the M3 auto-spawn path, retry the call once.
      `DaemonClient` now owns its `ConnectConfig` so it can reconnect
      on its own. Regression test in
      `crates/mcp-bridge/tests/reconnect.rs` kills the daemon
      mid-session and asserts the next call still succeeds.
- [ ] Multi-bridge contention test: N bridges driving one daemon,
      verify fair scheduling and no per-bridge starvation.
- [ ] systemd user-service unit + launchd plist examples for users
      who prefer an always-on daemon over demand-spawn.

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
- [ ] `code.outline` `signatures-only` formatter (rtk
      `read --aggressive` equivalent) reusing the existing tree-sitter
      parse cache.
- [ ] `fs.read` `?strip-noise` flag for license headers, long base64
      blobs, generated-file markers.
- [ ] New `tool.run` family (one MCP method per wrapped tool, dispatched
      to a `ToolBackend` trait that mirrors `LanguageBackend`):
    - [ ] `tool.cargo_test`, `tool.cargo_clippy`, `tool.cargo_build`
          consuming `--message-format=json`. Failures + warnings only.
    - [ ] `tool.test_runner` adapter — `pytest --json-report`,
          `jest --json`, `go test -json`, `vitest --reporter=json`.
    - [ ] `tool.lint` adapter — `eslint --format json`,
          `tsc --pretty false`, `ruff check --output-format=json`,
          `golangci-lint run --out-format json`.
    - [ ] `tool.gh` adapter for `pr list`, `pr view`, `issue list`,
          `run list` (via `gh ... --json ...`).
- [ ] Per-`(command, cwd, file-mtime-fingerprint)` LRU cache so a
      re-run with no source changes returns the cached structured
      result, the same way `search.grep` caches by ChangeLog version.
- [x] `metrics.gain` RPC (`crates/daemon/src/metrics.rs`): per-tool
      counters of (raw_bytes, compacted_bytes, calls). Backed by
      atomics on the daemon side; cheap to keep and read. Bridge
      tool name `metrics_gain`.
- [ ] Bridge tool definitions for the new MCP surface, plus
      `doc/INTEGRATION.md` snippets showing the agent which tool to
      prefer over raw `Bash(...)` for each common workflow.

## Integration strategies (parallel tracks)

Tracked separately from the MCP milestones — these are alternative
mounting surfaces, not sequential work.

- [ ] **LSP proxy prototype**: expose `outline`/`definition`/`search` over
      LSP so the daemon can ride an editor's existing persistent
      connection and reuse its open-buffer text / ASTs.
- [ ] **WASI build target**: compile a subset of the tool surface (grep,
      scan, outline) to `wasm32-wasi` so agent runtimes that support WASI
      can load it in-process and skip the UDS hop.
- [ ] Decide scope: specialist (Rust + C++ deep via LSP backends) vs
      generalist (tree-sitter + text tools across many languages). Gates
      priority of M3 backend work and the WASI module surface.

## Docs / polish

- [ ] Architecture diagram in `doc/` (currently only ASCII in the README).
- [ ] `doc/PROTOCOL.md` listing every RPC method, params, result, error codes.
- [ ] `doc/INTEGRATION.md` with copy-pastable Claude Code / Codex configs.
