# TODO

Concrete, actionable items. Group headers track milestones in
[`ROADMAP.md`](./ROADMAP.md). Check items off as commits land.

## Hardening (M0/M1 follow-ups)

- [ ] Make `ChangeLog` capacity configurable via `--changelog-capacity`.
- [ ] Suppress `created_then_removed` pairs (file briefly existed, gone before
      next snapshot) so they don't show up as `removed`.
- [ ] Add `fs.scan` so a client that sees `overflowed: true` can do a fresh
      full enumeration in one RPC instead of falling back to host-side `find`.
- [ ] Honour nested `.gitignore` files (currently only the project-root
      `.gitignore` is loaded).
- [ ] Tests:
    - [ ] `ChangeLog`: ordering, coalescing, overflow watermark.
    - [ ] `resolve_within`: rejects `..` traversal, rejects symlinks
          escaping root, accepts absolute paths inside root.
    - [ ] `framing`: max-frame, EOF mid-frame, oversize length.
- [ ] Bench: `cargo bench` comparing `fs.read` via daemon vs. `cat` fork.

## Indexing (M2)

- [ ] Pre-warm walker that respects gitignore and pages source files in.
- [ ] `tree-sitter` integration crate; start with `rust`, `c`, `cpp`, `python`,
      `typescript`, `go`.
- [ ] `code.outline` RPC: file -> top-level definitions (`fn`, `struct`,
      `class`, `def`, etc.) with byte ranges.
- [ ] LRU for `search.grep` results keyed on `(pattern, glob, version)`.

## Language backends (M3)

- [ ] `LanguageBackend` trait + registry.
- [ ] Rust backend: spawn `rust-analyzer` once, speak LSP, cache by
      `ChangeLog` version.
- [ ] C++ backend: spawn `clangd`, parse `compile_commands.json`.
- [ ] Backend health: detect crashes, auto-respawn, surface errors as RPC
      errors instead of dropping the connection.

## I/O ceiling (M4)

- [ ] Switch global allocator to `mimalloc` behind a feature flag.
- [ ] Per-request arena allocator for response building.
- [ ] Linux: experiment with `io_uring` for `fs.read` and walker I/O. Gate
      behind `--io-uring`.
- [ ] Binary `fs.read` mode: return raw bytes via a side channel for files
      above N KiB instead of JSON-encoded `String`.

## Lifecycle (M5)

- [ ] Bridge: detect daemon-dead, retry-connect with backoff, surface a
      clean MCP error if the daemon is unreachable.
- [ ] Optional auto-spawn of the daemon from the bridge if the socket
      doesn't exist.
- [ ] systemd user-service unit + launchd plist examples.

## Docs / polish

- [ ] Architecture diagram in `doc/` (currently only ASCII in the README).
- [ ] `doc/PROTOCOL.md` listing every RPC method, params, result, error codes.
- [ ] `doc/INTEGRATION.md` with copy-pastable Claude Code / Codex configs.
