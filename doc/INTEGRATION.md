# Integrating mcp-cli with an agent

The bridge speaks MCP on stdin/stdout; the daemon serves it over a
per-cwd Unix socket. You don't normally have to know that — the
`mcp-cli` installer wires both ends into the agent of your choice.

## TL;DR

```sh
# Build (one-time)
cargo build --release

# Register with every agent the installer knows about
target/release/mcp-cli install
```

That's it for Claude Code. For Codex, the headline numbers in the
[README](../README.md) and the
[M5 benchmark results](../bench/codex-forkexec/results/) only
materialise if you also pass `--prefer-mcp`:

```sh
target/release/mcp-cli install --target codex --prefer-mcp
```

The rest of this doc explains what each agent target ends up with,
what `--prefer-mcp` does, and how to undo / verify everything.

## Codex

### What `mcp-cli install --target codex` writes

```toml
# Appended to ~/.codex/config.toml (or $CODEX_HOME/config.toml)
[mcp_servers.mcp-cli]
command = "/abs/path/to/mcp-cli-bridge"
args = []
```

The installer uses `toml_edit` so user-authored keys, comments, and
formatting in the rest of `config.toml` are preserved.

### What `--prefer-mcp` *additionally* writes

```toml
[features]
shell_tool = false

[mcp_servers.mcp-cli.tools.fs_read]
approval_mode = "approve"

[mcp_servers.mcp-cli.tools.fs_read_batch]
approval_mode = "approve"

# … one entry per mcp-cli tool: fs_snapshot, fs_changes, fs_scan,
# git_status, search_grep, code_outline, code_outline_batch,
# code_symbols, code_symbols_batch, metrics_gain, metrics_tool_latency.
```

* **`shell_tool = false`** disables codex's built-in `Bash` tool.
  Without this, codex prefers `Bash("rg ...")` / `Bash("cat ...")`
  for nearly every read or search and never reaches for the MCP
  surface — which made the v1 benchmark a clean negative result.
* **Per-tool `approval_mode = "approve"`** auto-approves each
  mcp-cli tool. Codex requires per-tool approval entries (no
  server-wide default in the schema), and `codex exec --json` has
  no interactive channel — without these, every MCP call cancels
  with `user cancelled MCP tool call`.

### Verify after install

```sh
grep -A1 'shell_tool\|mcp-cli\.tools' ~/.codex/config.toml | head
codex exec --skip-git-repo-check --json \
  "use the mcp-cli tools to list files in this directory" \
  | grep '"tool":"fs_scan"'
```

You should see a real `fs_scan` invocation (and zero
`/bin/zsh -lc` calls).

### Per-session isolation

The installer honours `$CODEX_HOME`, so per-session installs land
in the right place:

```sh
mkdir -p /tmp/my-isolated-codex-home
CODEX_HOME=/tmp/my-isolated-codex-home \
  target/release/mcp-cli install --target codex --prefer-mcp
CODEX_HOME=/tmp/my-isolated-codex-home codex exec --json "…"
```

The `bench/codex-forkexec/` runner uses exactly this pattern.

### Uninstall

```sh
target/release/mcp-cli uninstall --target codex
```

Removes the `[mcp_servers.mcp-cli]` block. Note: it does **not**
remove `[features] shell_tool = false` — drop that by hand if you
want codex's built-in Bash back.

## Claude Code

Claude Code has its own `claude mcp add` CLI; the installer just
shells out to it.

```sh
target/release/mcp-cli install --target claude-code
# under the hood: `claude mcp add mcp-cli /abs/path/to/mcp-cli-bridge`
```

`--prefer-mcp` is a no-op for the `claude-code` target today —
Claude Code's tool router doesn't have an analogous `shell_tool`
toggle, and per-tool approval modes live in a different
configuration shape.

### Verify

```sh
claude mcp list
# Should include `mcp-cli ➜ /…/mcp-cli-bridge`
```

### Uninstall

```sh
target/release/mcp-cli uninstall --target claude-code
# under the hood: `claude mcp remove mcp-cli`
```

## What the daemon actually exposes

Once mounted, the agent sees these MCP tools (full schemas in
`crates/mcp-bridge/src/mcp.rs`):

| tool | shape | notes |
|---|---|---|
| `fs_read` | `{path, offset?, length?, strip_noise?}` → `{content, stripped_regions?, …}` | mmap-backed; `strip_noise: true` elides license / base64 / generated boilerplate when reading from byte 0 |
| `fs_read_batch` | `{requests: [{path, offset?, length?, strip_noise?}]}` → `{responses: [{path, result?, error?}]}` | per-item errors don't abort the batch |
| `fs_snapshot` / `fs_changes` | version cursor + coalesced events | for incremental sync clients |
| `fs_scan` | optional subdir + max | gitignore-aware, `.git/` excluded |
| `git_status` | `{repo?, compact?}` | libgit2; `compact: true` rolls up by status class + per-dir |
| `search_grep` | `{pattern, glob?, path?, context?, compact?, …}` | grep-searcher; `context: N` attaches surrounding lines |
| `code_outline` | `{path, signatures_only?}` → `{entries: [{kind, name, signature?, …}]}` | tree-sitter, supports rust/python/c/cpp/ts/tsx/go; `signatures_only: true` drops bodies |
| `code_outline_batch` / `code_symbols_batch` | `{requests: [{path, signatures_only?}]}` → batched | per-item errors don't abort |
| `code_symbols` | flat de-duplicated symbol list | cheaper than `code_outline` when only names matter |
| `metrics_gain` | per-tool byte-savings counters | `(raw, compacted, calls)` |
| `metrics_tool_latency` | per-tool wall-clock counters (μs) | calls / sum / mean / max |

Two design rules to know about as a caller:

* **Batch tools (`*_batch`) are strongly preferred** when you know
  multiple paths up-front. One UDS round-trip beats N — and
  crucially, one *agent turn* beats N. The M5 benchmark trail
  shows that turn count, not per-call latency, dominates wall
  clock.
* **`?compact` and `?context` flags reshape the response, not the
  semantics.** They're cheap server-side. `git_status?compact` is
  5–10× smaller on a non-trivial dirty tree; `search_grep?context`
  collapses the "grep then read around the hit" two-call pattern.

## Always-on daemon

Demand-spawn (the default) is usually right — the daemon comes up
on the bridge's first call and idle-exits 30 minutes after the
last bridge disconnects. If you'd rather pin one daemon per
project, see [`doc/services/`](./services/) for systemd /
launchd unit examples.

## Reproducing the headline numbers

```sh
cargo build --release
PATH="$PWD/target/release:$PATH" \
  OUT_DIR=/tmp/mcpcli-bench \
  bench/codex-forkexec/run.sh
```

Codex must be installed and authenticated. The bench bootstraps
its own `CODEX_HOME` with a copy of `~/.codex/auth.json` so the
real config is left untouched.
