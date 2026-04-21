# `claudecode-forkexec` — measure fork/exec reduction with the mcp-cli plugin (Claude Code)

Twin of [`bench/codex-forkexec`](../codex-forkexec/README.md) — same
workload, same three-pass shape, same comparison table — driven by
the `claude` CLI instead of `codex exec`.

Running both benches on the same `TARGET_REF` lets you compare how
much each agent wins from mcp-cli: Codex already showed a 79 %
execve drop with a wall-clock regression (atomic MCP calls >
bash-pipeline turns); Claude Code has a different tool palette
(`Read`, `Grep`, `Glob`, `Bash`) and a different tool-use event
stream, so its payoff curve won't be identical.

## What it does

1. Clones a target repo (default: **the Codex repo itself**, same as
   the codex twin — same corpus = comparable numbers across agents)
   into a fresh tempdir.
2. Runs Claude Code **three times** with the same prompt:
   * **Baseline** — vanilla claude with its full built-in tool
     palette (`Bash`, `Read`, `Grep`, `Glob`, …), no MCP servers.
   * **Cold mcp-cli** — the bench writes a throwaway
     `$OUT_DIR/mcp-config.json` pointing at the mcp-cli bridge
     and passes it via `--mcp-config`. Daemon is freshly spawned;
     `search_cache` + `parse_cache` are empty. `--disallowed-tools`
     strips `Bash Read Grep Glob Edit Write NotebookEdit` so the
     agent must route through the daemon for all I/O and search.
   * **Warm mcp-cli** — same `--mcp-config`, so the bridge resolves
     the same canonical socket and reconnects to the already-running
     daemon. The config pins `--daemon-arg=--idle-timeout=30m` so
     the daemon survives between the cold and warm passes.
3. Wraps each run with an `execve` tracer (auto-picked):
   * Linux: `strace -e trace=execve -f -o trace.log claude -p ...`
   * macOS root: `dtruss -f -t execve claude -p ...`
   * Otherwise: **shim mode** — PATH-shadow wrappers that bump a
     per-binary counter file before delegating to the real binary.
     Same mechanism as the codex twin.
4. Parses each trace / counter dir via `parse_trace.py` (shared with
   codex-forkexec via symlink), then `compare.py` tabulates
   per-binary `execve` count, wall-clock, Claude Code token usage
   (including `cache_read_input_tokens` — Claude's prompt-cache
   counter), and the `tool_use` events grouped by tool name so you
   can see whether the agent actually exercised the daemon's MCP
   tools vs. its built-ins.

## Why no `mcp-cli install --target claude-code`?

The codex twin runs `mcp-cli install --target codex` into an isolated
`CODEX_HOME`. Claude Code's `install` equivalent shells out to
`claude mcp add`, which edits the **user's real `~/.claude.json`** —
there's no supported `CLAUDE_HOME` knob. To keep the bench
reproducible and non-invasive we use `--mcp-config <json>` instead,
which loads the server definition for just this one invocation
without touching any on-disk config. That file path is also how we
embed the `--daemon-arg=--idle-timeout=30m` override the warm pass
relies on.

`--bare` is the companion flag: it strips hooks, plugins, auto-memory
reads, CLAUDE.md auto-discovery, and background keychain reads so the
bench numbers don't drift based on whatever happens to be in the
user's environment. `ANTHROPIC_API_KEY` must be set (or configured
via `--settings`) — `--bare` will not read OAuth state from the
keychain.

## Workload

[`prompt.md`](./prompt.md) is a **symlink** to
[`../codex-forkexec/prompt.md`](../codex-forkexec/prompt.md). Same
text, same target repo, same constraints — changing one flows to
both so the two benches stay strictly comparable.

## Running it

Prereqs:

* `claude` on `PATH` (or set `CLAUDE=/path/to/claude`). Tested
  against the npm `claude-code` CLI.
* `ANTHROPIC_API_KEY` in the environment (or configured via
  `--settings`, but passing it inline is simplest under `--bare`).
* `mcp-cli`, `mcp-cli-bridge`, and `mcp-cli-daemon` on `PATH` —
  `cargo install --path crates/mcp-cli` from the repo root, plus
  symlinks for the bridge and daemon binaries from `target/release/`.
* Linux: `strace` installed, no extra privileges needed.
* macOS: must run as root for `dtruss` to attach, otherwise shim
  mode kicks in.
* `git`, `python3`.

```sh
# From the repo root.
bench/claudecode-forkexec/run.sh

# Override the target repo / ref:
TARGET_REPO=https://github.com/openai/codex \
TARGET_REF=v0.42.0 \
bench/claudecode-forkexec/run.sh

# Skip the clone if you already have it:
TARGET_DIR=/path/to/checkout bench/claudecode-forkexec/run.sh
```

The script writes everything to a tempdir under `$TMPDIR`, prints
the comparison table, and leaves the raw traces + stream-json stdout
behind for inspection (path is logged on exit).

If you already have an artifact dir from an earlier run and just
want it re-tabulated:

```sh
python3 bench/claudecode-forkexec/compare.py \
    --out-dir /path/to/run-artifacts \
    --tracer strace      # or dtruss; auto-detects from `uname -s`
```

## Output

```
Claude Code fork/exec benchmark — target=openai/codex@v0.42.0
========================================================================================================
metric                              baseline    cold mcp-cli        cold Δ    warm mcp-cli     Δ vs cold
execve total                            <N0>           <N1>         <N1-N0>           <N2>     <N2-N1>
  cat                                    <c0>           <c1>               …           <c2>           …
  grep                                   <g0>           <g1>               …           <g2>           …
  ...
  mcp-cli-daemon                         0              1                 +1            0               -1
wall clock (s)                          <t0>           <t1>               …            <t2>     <t2-t1>
input tokens                           <ti0>          <ti1>               …           <ti2>           …
  cached input tokens                   <ci0>          <ci1>               …           <ci2>           …
output tokens                          <to0>          <to1>               …           <to2>           …
```

Two headline numbers:

* `execve total → cold Δ` — the fork/exec win. Binaries the daemon
  obviates (`cat`, `grep`, `rg`, `git`, `find`, `head`, `tail`, `ls`)
  should collapse to near zero in the cold column. Claude Code
  doesn't shell out as aggressively as Codex by default (it has its
  own `Read` / `Grep` / `Glob` tools), so the pre-`--disallowed-tools`
  execve count may already be lower than codex's baseline — the
  interesting number is the combined (disallowed + mcp) pass relative
  to vanilla claude.
* `wall clock (s) → Δ vs cold` — the in-memory-cache payoff. The
  warm pass reuses the daemon, so `mcp-cli-daemon` should execve
  zero times (daemon already running) and the in-process caches
  amortize parse/search work.

There's also a **tool_use** block at the bottom showing every tool
claude invoked, broken out by name. MCP tools appear as
`mcp__mcp-cli__<tool>`; built-ins appear as their bare name. A
healthy cold run shows `mcp__mcp-cli__fs_read`, `mcp__mcp-cli__search_grep`,
etc. at non-zero with `Bash` / `Read` / `Grep` at zero.

## Caveats and known limitations

* **Claude Code non-determinism**: model temperature is not set in
  the prompt; tool choices vary between runs. Trust trends across
  N≥3 runs, not single-run numbers.
* **`claude mcp add` pollutes `~/.claude.json`**: we deliberately
  avoid it and use `--mcp-config` instead. If you want to measure
  the user-config path, run `mcp-cli install --target claude-code`
  manually beforehand and drop `--mcp-config` from `run.sh` — but
  that makes the bench stateful across runs.
* **`--bare` skips hooks and auto-memory**: results don't reflect
  a "full claude" session with the user's normal plugins. This is
  intentional (reproducibility > realism for a micro-benchmark).
  For a realistic-session measurement, remove `--bare` and
  re-run — expect noisier numbers.
* **Per-tool latency**: the `metrics.tool_latency` dump at the end
  is the same as the codex twin — daemon-side per-tool counters,
  best-effort snapshot before the daemon idle-exits.
