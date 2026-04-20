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

- [ ] `LanguageBackend` trait + registry.
- [ ] Rust backend: spawn `rust-analyzer` once, speak LSP, cache by
      `ChangeLog` version.
- [ ] C++ backend: spawn `clangd`, parse `compile_commands.json`.
- [ ] Backend health: detect crashes, auto-respawn, surface errors as RPC
      errors instead of dropping the connection.

## Drop-in install + per-cwd auto-spawn (M3)

- [ ] `mcp-cli install` subcommand (probably a new `mcp-cli` wrapper
      binary alongside `mcp-cli-daemon`/`mcp-cli-bridge`). Flags:
      `--target claude-code | codex | all` (default `all`).
    - [ ] Claude Code: shell out to `claude mcp add mcp-cli <path>`;
          skip if `claude mcp list` already has it. Surface the CLI's
          exit code on failure.
    - [ ] Codex: read `~/.codex/config.toml`, merge a
          `[mcp_servers.mcp-cli]` entry, write back. Preserve
          user-authored keys; idempotent on re-run.
    - [ ] Print a diff of what changed in each target's config.
    - [ ] `--uninstall` inverse (remove the registration).
- [ ] Bridge: default `--root` to `std::env::current_dir()` when not
      passed, so the agent's cwd becomes the project root.
- [ ] Derive per-cwd socket path. Hash the canonicalized cwd; use
      `$XDG_RUNTIME_DIR/mcp-cli/<hash>.sock` on Linux, fall back to
      `/tmp/mcp-cli-<user>-<hash>.sock` elsewhere. Mode 0600.
- [ ] Bridge auto-spawn: on `ENOENT`/`ECONNREFUSED`, fork+exec
      `mcp-cli-daemon --root <cwd> --socket <derived>`, detach with
      `setsid`, and retry-connect with ~25ms backoff up to ~2s before
      erroring. Background stdout/stderr to a per-cwd log under the
      same directory as the socket.
- [ ] Daemon `--idle-timeout <duration>` (default 30m; `0` disables).
      Track last-connected time; exit cleanly when idle elapsed.
- [ ] End-to-end smoke test: start bridge from a tempdir cwd, issue a
      `tools/call fs_read`, verify a daemon was spawned with the right
      `--root`, kill it, confirm the socket is cleaned up.

## I/O ceiling (M4)

- [ ] Switch global allocator to `mimalloc` behind a feature flag.
- [ ] Per-request arena allocator for response building (hot path:
      tree-sitter parse + context assembly).
- [ ] Pre-allocated, reusable source-read buffer pool to stop
      re-requesting small pages from the kernel on every `fs.read`.
- [ ] Linux: experiment with `io_uring` for `fs.read` and walker I/O. Gate
      behind `--io-uring`.
- [ ] Thread-per-core tokio runtime with per-worker `io_uring` rings
      (no cross-core SQ contention). Depends on the `io_uring` item above.
- [ ] Binary `fs.read` mode: return raw bytes via a side channel for files
      above N KiB instead of JSON-encoded `String`.
- [ ] Zero-copy large-response path: `splice` file bytes directly into the
      socket for responses above the threshold, skipping the JSON
      round-trip entirely.

## Lifecycle (M5)

(Auto-spawn + per-cwd socket routing moved to M3 under "Drop-in
install". What remains here is hardening + optional system integration.)

- [ ] Bridge: detect daemon-dead mid-session, retry-connect with backoff,
      fall through to the M3 auto-spawn path on `ECONNREFUSED` rather
      than failing the call.
- [ ] Multi-bridge contention test: N bridges driving one daemon,
      verify fair scheduling and no per-bridge starvation.
- [ ] systemd user-service unit + launchd plist examples for users
      who prefer an always-on daemon over demand-spawn.

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
