# `codex-forkexec` — measure fork/exec reduction with the mcp-cli plugin

This benchmark is the load-bearing measurement for the project's
core claim: **mounting mcp-cli as an MCP plugin in Codex eliminates
the per-call `fork`/`exec` of `cat`, `grep`, `git`, …** Every other
milestone (M3 backends, M4 I/O ceiling, M7 compaction) is a
no-op if the agent never actually stops shelling out — so we
measure that directly.

> **Headline finding from the first real run** ([results/2026-04-20-rust-v0.121.0.md](./results/2026-04-20-rust-v0.121.0.md)):
> codex did **not** route any work through `mcp-cli`'s MCP tools.
> Per-binary fork/exec deltas were within model-variance noise
> (-5 over 93 calls); the only `mcp_tool_call` events that fired
> were codex's own startup discovery (`list_mcp_resources`).
> Closing this gap needs either a codex-side preference rule or a
> Bash-rewriting hook (rtk-style) — neither lives in mcp-cli today.

## What it does

1. Clones a target repo (default: **the Codex repo itself, at its
   latest release tag**) into a fresh tempdir.
2. Runs Codex twice with the same prompt:
   * **Baseline** — vanilla Codex, no MCP servers configured.
   * **mcp-cli** — same Codex install, but with `mcp-cli install
     --target codex` already applied so the bridge is mounted.
3. Wraps each run with an `execve` tracer (auto-picked):
   * Linux: `strace -e trace=execve -f -o trace.log codex exec ...`
   * macOS root: `dtruss -f -t execve codex exec ...`
   * Otherwise: **shim mode** — PATH-shadow wrappers that bump a
     per-binary counter file before delegating to the real binary.
     Works without root by setting `$ZDOTDIR` so codex's `zsh -lc`
     re-prepends the shim dir after the user's profile, and by
     passing `--add-dir "$OUT_DIR"` so codex's sandbox lets the
     counter writes through.
4. Parses each trace (or counter dir), tabulates per-binary
   `execve` count, wall-clock, codex token usage, and the codex
   `mcp_tool_call` events grouped by `server/tool` so you can see
   whether the daemon was actually exercised.

## Workload

[`prompt.md`](./prompt.md) — Codex is asked to analyze its own
source tree and propose three concrete performance enhancements
with file:line citations. The prompt is deliberately self-referential
so the benchmark is reproducible against a stable target without
us having to maintain a separate corpus repo.

## Running it

Prereqs:

* `codex` on `PATH` (or set `CODEX=/path/to/codex`).
* `mcp-cli`, `mcp-cli-bridge`, and `mcp-cli-daemon` on `PATH` —
  `cargo install --path crates/mcp-cli` from the repo root, plus
  symlinks for the bridge and daemon binaries from `target/release/`.
* Linux: `strace` installed, no extra privileges needed.
* macOS: must run as root for `dtruss` to attach.
* `git`, `python3`, `jq`.

```sh
# From the repo root.
bench/codex-forkexec/run.sh

# Override the target repo / ref:
TARGET_REPO=https://github.com/openai/codex \
TARGET_REF=v0.42.0 \
bench/codex-forkexec/run.sh

# Skip the clone if you already have it:
TARGET_DIR=/path/to/checkout bench/codex-forkexec/run.sh
```

The script writes everything to a tempdir under `$TMPDIR`, prints
the comparison table, and leaves the raw traces + Codex stdout
behind for inspection (path is logged on exit).

If you have an existing trace pair you just want re-tabulated, call
`compare.py` directly:

```sh
python3 bench/codex-forkexec/compare.py \
    --out-dir /path/to/run-artifacts \
    --tracer strace      # or dtruss; auto-detects from `uname -s`
```

## Output

```
Codex fork/exec benchmark — target=openai/codex@v0.42.0
====================================================================
                           baseline      with mcp-cli      delta
execve total                   <N0>             <N1>      <N1-N0>
  cat                          <c0>             <c1>      …
  grep                         <g0>             <g1>      …
  git                          <git0>           <git1>    …
  rg                           <rg0>            <rg1>     …
wall clock (s)                 <t0>             <t1>      …
input tokens                   <ti0>            <ti1>     …
output tokens                  <to0>            <to1>     …
```

The headline number is `execve total → delta`. A healthy
mcp-cli-equipped run shows the binaries the daemon obviates
(`cat`, `grep`, `rg`, `git`, `find`, `head`, `tail`, `ls`)
collapsing to near zero, with `mcp-cli-bridge` / `mcp-cli-daemon`
showing up exactly once per session (the auto-spawn) instead of
hundreds of per-call `cat` / `grep` invocations.

## Caveats and known limitations

* **Codex non-determinism**: model temperature defaults to 0 in the
  prompt, but the agent's tool choices still vary slightly between
  runs. Treat single-run numbers as indicative; trust the trend
  across N≥3 runs.
* **macOS root requirement**: `dtruss` needs root. The script
  refuses to start the trace step on macOS unless `id -u == 0`.
* **Codex-version-locked**: the parsing of Codex's stdout for token
  counts is fragile against Codex CLI updates. Re-check `compare.py`
  if the headline numbers look off after a Codex bump.
* **Per-tool latency** is not yet measured here — the benchmark
  reports counts only. Adding daemon-side `metrics.tool_latency` is
  a follow-up so we can prove fork/exec saved actually translates
  into wall-clock saved per call (see `doc/todo.md`).

## Why Codex's own source as the target?

* It's a real-world agent codebase: lots of small Rust files, a
  non-trivial `git log`, plenty of `grep`-able call sites.
* It's a stable target: Codex tags releases, so pinning
  `TARGET_REF=$(latest tag)` gives reproducible numbers.
* It dogfoods the project — a Codex performance regression that
  hurts mcp-cli's own benchmark hits the same code path Codex
  users care about.
