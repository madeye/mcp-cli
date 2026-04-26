# TODO

Concrete, actionable items. Group headers track milestones in
[`ROADMAP.md`](./ROADMAP.md).

## Hardening (M0/M1) — Done

* [x] Make `ChangeLog` capacity configurable via `--changelog-capacity`.
* [x] Suppress `created_then_removed` pairs.
* [x] Add `fs.scan` for fresh full enumeration.
* [x] Honour nested `.gitignore` files.
* [x] Tests: `ChangeLog`, `resolve_within`, `framing`.
* [x] Bench: `cargo bench` for `fs.read` vs `cat`.

## Indexing (M2) — Done

* [x] Pre-warm walker for page cache priming.
* [x] `tree-sitter` integration (Rust, Python, C, C++, TS, TSX, Go).
* [x] `code.outline` and `code.symbols` RPCs.
* [x] `ParseCache` with proactive eviction.
* [x] LRU for `search.grep` results.

## Language Backends & Install (M3) — Done

* [x] `LanguageBackend` trait + registry.
* [x] `mcp-cli install` / `uninstall` / `status`.
* [x] Bridge auto-spawn with backoff.
* [x] Per-cwd socket path routing.
* [x] Daemon idle-timeout.
* [x] End-to-end autospawn smoke test.

## I/O Ceiling (M4) — In Progress

* [x] Switch global allocator to `mimalloc`.
* [x] Recyclable `Vec<u8>` `BufferPool` for request frames.
* [x] Extend buffer pool to response serialization.
* [ ] Per-request arena allocator (`bumpalo`) for response building.
* [ ] Linux: `io_uring` for `fs.read` and walker I/O (`--io-uring`).
* [ ] Thread-per-core tokio runtime with per-worker `io_uring` rings.
* [ ] Binary `fs.read` mode (raw bytes side-channel for large files).
* [ ] Zero-copy `splice` path for large-response writes.

## Benchmark (M5) — Done

* [x] `bench/codex-forkexec/run.sh` orchestrator.
* [x] `bench/claudecode-forkexec/` twin benchmark.
* [x] `parse_trace.py` and `compare.py` for results.
* [x] `metrics.tool_latency` RPC for daemon-side instrumentation.
* [x] README headline updates with −44% (Codex) and −82% (Claude) wins.
* [x] Claude Code twin under `bench/claudecode-forkexec/`: three-pass `baseline` / `cold mcp-cli` / `warm mcp-cli` shape.

## Lifecycle & Contention (M6) — Done

* [x] Bridge: reconnect-on-dead mid-session.
* [x] Multi-bridge contention test (`multibridge.rs`).
* [x] `systemd` / `launchd` example units in `doc/services/`.

## Token-Killer Compaction (M7) — Done

* [x] `crates/daemon/src/compact/` foundation.
* [x] `git.status ?compact` (grouped by status class).
* [x] `search.grep ?compact` (bucketed by file).
* [x] `code.outline ?signatures_only` (declaration headers).
* [x] `fs.read ?strip_noise` (license/base64/generated stripping).
* [x] `fs.scan ?compact` (directory roll-ups).
* [x] `metrics.gain` RPC (raw vs. compacted bytes).
* [x] `git.log` RPC (compact one-liners).
* [x] `git.diff` RPC (condensed patches).
* [x] `tool.run` RPC (tee-on-failure, truncation, caching).
* [x] `tool.gh` adapter for `pr` / `issue` views.

## Write Path & Concurrency (M8) — Done

* [x] `fs.apply_patch` RPC with Optimistic Concurrency Control (OCC).
* [x] `fs.replace_all` RPC with OCC.

## Advanced Structural Tools (M9) — Done

* [x] `code.imports` / `code.dependencies` RPC.
* [x] Resident bi-directional dependency graph.
* [x] `code.find_occurrences` (Smart Grep).
* [x] `fs.read_skeleton` (Dynamic folding).

## Deep Git & Process Management (M10) — Pending

* [ ] `git.blame` (Compact) RPC.
* [ ] `git.history` (File-specific) RPC.
* [ ] `tool.spawn` / `tool.read_logs` / `tool.kill` (Background jobs).
