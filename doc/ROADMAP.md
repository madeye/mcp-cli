# Roadmap

The thesis: replace the per-call `fork`/`exec` cost of shell-wrapped agent
tools with a single hot user-space process that holds project state in RAM.
Every milestone below is judged against one question — does it move the
kernel-overhead-per-tool-call closer to zero without giving up correctness?

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

## M2 - Indexing (next)

Goal: make `search.grep` and future semantic queries cheap on cold caches.

* Background pre-warm: walk the tree at startup, prime the page cache for
  source files, build a path -> blob-hash table.
* Tree-sitter parse cache. Parse on first request per file, evict on
  `fs.changes`. Expose `code.outline` (top-level defs) and `code.symbols`
  (named identifiers) as MCP tools.
* Result deduplication: when an agent issues N back-to-back `search.grep`
  calls with the same pattern, serve from a tiny LRU. Invalidate on
  `ChangeLog` version bump.

## M3 - Language backends

Plug-in shape: a backend is `trait LanguageBackend` with `outline`,
`definition`, `references`, `diagnostics`. The daemon owns one
backend instance per language and routes requests.

* Rust: shell out (once, long-lived) to `rust-analyzer` over its LSP, cache
  responses keyed on `ChangeLog` version.
* C++: same pattern with `clangd`, plus auto-discover `compile_commands.json`.
* Pure-text languages: tree-sitter only.

The cost of `rust-analyzer` startup is paid once per project, not once per
agent turn — that is the whole point of the daemon.

## M4 - I/O ceiling

Squeeze out the last per-syscall overhead.

* Linux: `io_uring` submission queue for batched `fs.read` and tree walks.
* `mimalloc` global allocator + arena allocators per request, freed in one
  drop at end of dispatch.
* Zero-copy frame writes for `fs.read` responses larger than a threshold
  (send `splice`d bytes directly, skip the JSON `String` round-trip).

## M5 - Multi-client + lifecycle

* Multiple MCP bridges connected to one daemon (already supported by the
  per-connection task; needs explicit testing under contention).
* Health check + auto-respawn from the bridge if the daemon went away.
* Optional systemd / launchd unit files.

## Non-goals

* Becoming a general LSP. We sit *next to* LSPs and reuse their state.
* Network transport. UDS only; remote agents talk to a remote daemon, not
  this one.
* Sandboxing. The daemon trusts its caller; the agent's safety story is
  handled by the host (Claude Code, Codex, etc.).
