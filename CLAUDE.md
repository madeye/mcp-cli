# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

CI gates (must pass before pushing — see user global rules):

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```

Other common commands:

```sh
cargo build --release                                   # build both binaries
cargo run -p daemon -- --root /path/to/project          # run daemon (default socket: /tmp/mcp-cli.sock)
cargo run -p mcp-bridge                                 # run stdio bridge
cargo test -p daemon changelog::tests::push_advances_version   # single test by path
cargo test -p daemon -- --nocapture                     # see tracing output from tests
RUST_LOG=debug cargo run -p daemon                      # bump log level (daemon defaults to info, bridge to warn)
```

Daemon flags worth knowing: `--socket`, `--root`, `--changelog-capacity` (ring size, default 4096), `--search-cache-capacity` (LRU size, default 64; 0 disables), `--parse-cache-capacity` (tree-sitter parse LRU, default 256; 0 disables), `--no-prewarm` (skip startup page-cache walk — useful in tests/benchmarks).

## Architecture

Three-crate Cargo workspace implementing a **sidecar daemon + thin MCP bridge** pattern. The point is to amortize kernel overhead: agents make hundreds of small `fs.read` / `git.status` / `search.grep` calls, and each one hits a long-lived in-process daemon instead of forking `cat`/`git`/`rg`.

```
MCP client (Claude Code / Codex) ──stdio──> mcp-cli-bridge ──UDS──> mcp-cli-daemon
                                            (crates/mcp-bridge)      (crates/daemon)
                                                                     ↑ owns project state:
                                                                       mmap, libgit2, grep-searcher,
                                                                       notify-rs watcher + ChangeLog
```

### Crates

- **`crates/protocol`** — pure serde types shared by bridge and daemon. `lib.rs` defines `Request`/`Response`/`RpcError`, all params/result structs (`FsReadParams`, `FsSnapshotResult`, `FsChangesParams`, `FsScanParams`, `GitStatusResult`, `SearchGrepParams`, `CodeOutlineParams`/`Entry`/`Result`, `CodeSymbolsParams`/`Result`, …), and the `methods::*` constants (`ping`, `fs.read`, `fs.snapshot`, `fs.changes`, `fs.scan`, `git.status`, `search.grep`, `code.outline`, `code.symbols`). Add new RPC methods here first; both sides depend on this crate.
- **`crates/daemon`** — the `mcp-cli-daemon` binary. Owns the hot state. Modules:
  - `main.rs` — clap args, tracing setup, calls `server::serve`.
  - `server.rs` — binds the UDS, spawns `watcher` + optional `prewarm`, accepts connections, dispatches frames to `handlers`.
  - `framing.rs` — length-prefixed JSON frame codec on top of tokio `UnixStream`.
  - `handlers.rs` — per-method logic: `fs.read` (mmap via `memmap2`), `git.status` (libgit2), `search.grep` (grep-searcher + grep-regex), `fs.scan` (ignore::WalkBuilder), `fs.snapshot`/`fs.changes` (read from `ChangeLog`), `code.outline`/`code.symbols` (tree-sitter via `ParseCache` + `outline` module).
  - `changelog.rs` — bounded ring buffer with a monotonic `version` cursor. `fs.snapshot` returns the current version + `oldest_retained`; `fs.changes(since)` returns coalesced events or sets `overflowed: true` when the client fell off the ring.
  - `watcher.rs` — notify-rs (`FSEvent` on macOS / inotify on Linux) feeding the `ChangeLog` and evicting stale entries from the `ParseCache`. Gitignore-aware, hard-excludes `.git/`.
  - `search_cache.rs` — LRU of `search.grep` results keyed by query. **Invalidated by `ChangeLog` version bumps**, so entries only live through a quiescent window. Cap via `--search-cache-capacity`.
  - `parse_cache.rs` — per-file tree-sitter `Tree` LRU, validated by `(mtime_ns, size)` stat on every access. Independent of `ChangeLog` so a change to `a.rs` doesn't invalidate `b.rs`'s cached parse. Cap via `--parse-cache-capacity`.
  - `languages.rs` — extension → `Language` detection plus per-language tree-sitter grammars and outline query strings (rust, python, c, cpp, typescript, tsx, go). Adding a language means adding a variant, an extension case, the grammar constructor, and the query string.
  - `outline.rs` — executes the language's outline query against a `ParsedFile` and returns structured `CodeOutlineEntry` values. `symbols` is a flat, de-duplicated view on top.
  - `prewarm.rs` — one-shot background walker that pages source files into the OS cache at startup. Non-blocking; disabled with `--no-prewarm`.
- **`crates/mcp-bridge`** — the `mcp-cli-bridge` binary. Tiny. `mcp.rs` implements the MCP stdio server and translates `tools/call` into JSON-RPC; `daemon_client.rs` holds the UDS connection. The bridge is cheap to spawn per agent session — all heavy state lives in the daemon.

### Invariants & gotchas

- **Protocol changes are cross-crate.** If you touch `protocol/src/lib.rs`, rebuild both `daemon` and `mcp-bridge` — they deserialize the same structs.
- **`search.grep` cache coherence** relies on the watcher advancing the `ChangeLog` version for every mutation in the tree. A watcher regression silently makes the LRU stale. Tests in `search_cache.rs` and `changelog.rs` cover this — run them when editing either module.
- **`fs.changes` clients must handle `overflowed: true`** by re-snapshotting. Slow clients + small `--changelog-capacity` can trip this.
- **Socket cleanup**: `server::serve` unlinks the socket path on startup (stale socket) and on clean shutdown (ctrl-c). A crashed daemon can leave a stale socket; the next start removes it.
- The workspace pins `edition = "2021"` and `channel = "stable"` (see `rust-toolchain.toml`). `git2` is built with `default-features = false` (no vendored OpenSSL).

## Roadmap context

See `doc/roadmap.md` and `doc/todo.md`. M0 (skeleton), M1 (incremental sync), and M2 (indexing — pre-warm, `search.grep` LRU, tree-sitter parse cache, `code.outline`/`code.symbols`) are done. M3 is next: pluggable language backends (rust-analyzer, clangd) behind a `LanguageBackend` trait. New functionality should slot into one of those milestones rather than grow ad-hoc handlers.
