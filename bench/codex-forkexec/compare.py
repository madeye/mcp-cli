#!/usr/bin/env python3
"""Tabulate the baseline vs. with-mcp-cli benchmark runs.

Reads `<out_dir>/baseline.trace`, `<out_dir>/mcp.trace`, the
`.timing` files written by `run.sh`, and the Codex stdout files for
token counts. Prints a fixed-width comparison table to stdout.

The script is deliberately permissive — any missing input becomes a
'?' cell in the output rather than aborting the run. The benchmark
is more useful with partial data than with no data.
"""

from __future__ import annotations

import argparse
import os
import platform
import re
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import parse_trace  # type: ignore

# Binaries the daemon should make redundant when the plugin is on.
# Anything in this list collapsing to (near) zero is the headline
# proof that mcp-cli is doing its job.
DAEMON_REPLACED = (
    "cat",
    "head",
    "tail",
    "grep",
    "rg",
    "ripgrep",
    "find",
    "ls",
    "git",
)

# Loose pattern matches both human-readable lines (`prompt tokens: 42`)
# and the JSON keys codex exec --json emits (`"input_tokens":42`,
# `"cached_input_tokens":...`, `"output_tokens":...`). Cached tokens
# are tracked separately so the report can distinguish "model billed
# them as cache hits" from "fresh prompt bytes".
TOKEN_RE = re.compile(
    r'"?(input|output|prompt|completion|cached_input|reasoning_output)"?[_ ]?tokens?"?\s*[:=]\s*(\d+)',
    re.IGNORECASE,
)

# `mcp_tool_call` events fire when codex routes through an MCP server.
# Counting them tells us whether codex actually used mcp-cli's tools
# vs. just fell back to its built-in Bash/Read.
_MCP_TOOL_RE = re.compile(r'"type":"mcp_tool_call"[^{]*"server":"([^"]+)"[^{]*"tool":"([^"]+)"')


def read_int(path: Path) -> int | None:
    try:
        return int(path.read_text().strip())
    except (OSError, ValueError):
        return None


def read_float(path: Path) -> float | None:
    try:
        return float(path.read_text().strip())
    except (OSError, ValueError):
        return None


def parse_tokens(stdout: Path) -> dict[str, int]:
    """Best-effort token extraction from Codex stdout."""
    if not stdout.exists():
        return {}
    counts: dict[str, int] = {}
    for line in stdout.read_text(errors="replace").splitlines():
        for m in TOKEN_RE.finditer(line):
            kind = m.group(1).lower()
            n = int(m.group(2))
            # Normalise so the column header is stable regardless of
            # which codex / non-codex source we're parsing.
            kind = {"prompt": "input", "completion": "output"}.get(kind, kind)
            counts[kind] = max(counts.get(kind, 0), n)
    return counts


def parse_mcp_tool_calls(stdout: Path) -> dict[str, int]:
    """Count codex `mcp_tool_call` events grouped by `server/tool`.

    Used to expose whether codex actually routed work through
    mcp-cli's MCP tools or stuck with its built-in Bash. A bench
    where the with-mcp-cli run shows zero `mcp-cli/*` calls is a
    headline finding — the integration didn't bind, the win is
    illusory.
    """
    if not stdout.exists():
        return {}
    counts: dict[str, int] = {}
    for line in stdout.read_text(errors="replace").splitlines():
        for m in _MCP_TOOL_RE.finditer(line):
            key = f"{m.group(1)}/{m.group(2)}"
            counts[key] = counts.get(key, 0) + 1
    return counts


def fmt(n) -> str:
    if n is None:
        return "?"
    if isinstance(n, float):
        return f"{n:,.2f}"
    return f"{n:,}"


def fmt_delta(a, b) -> str:
    if a is None or b is None:
        return "?"
    if isinstance(a, float) or isinstance(b, float):
        return f"{(b - a):+,.2f}"
    return f"{(b - a):+,}"


def select_tracer() -> str:
    """Mirror the auto-detection logic in run.sh.

    `run.sh` is authoritative; this matches its preferences for the
    case where someone calls `compare.py` directly without re-running.
    """
    system = platform.system()
    if system == "Linux":
        return "strace"
    if system == "Darwin":
        # dtruss only works as root; assume the common case (no root)
        # and pick the shim format.
        return "dtruss" if os.geteuid() == 0 else "shim"
    return "shim"


def row(label: str, baseline, mcp, width: int = 30) -> str:
    return f"{label:<{width}}{fmt(baseline):>14}{fmt(mcp):>16}{fmt_delta(baseline, mcp):>14}"


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--out-dir", required=True)
    ap.add_argument("--target-repo", default="?")
    ap.add_argument("--target-ref", default="?")
    ap.add_argument(
        "--tracer",
        choices=("strace", "dtruss", "shim"),
        default=None,
        help="Override the auto-detected tracer (defaults to strace on "
        "Linux, dtruss on macOS root, shim otherwise).",
    )
    args = ap.parse_args()

    out = Path(args.out_dir)
    tracer = args.tracer or select_tracer()

    # Shim mode reads counter directories instead of trace files; the
    # other tracers read flat files. Same downstream code either way.
    if tracer == "shim":
        baseline_input = out / "baseline.counters"
        mcp_input = out / "mcp.counters"
    else:
        baseline_input = out / "baseline.trace"
        mcp_input = out / "mcp.trace"
    baseline_counts = (
        parse_trace.parse(str(baseline_input), tracer)
        if baseline_input.exists()
        else None
    )
    mcp_counts = (
        parse_trace.parse(str(mcp_input), tracer) if mcp_input.exists() else None
    )

    baseline_total = sum(baseline_counts.values()) if baseline_counts is not None else None
    mcp_total = sum(mcp_counts.values()) if mcp_counts is not None else None

    baseline_wall = read_float(out / "baseline.timing")
    mcp_wall = read_float(out / "mcp.timing")

    baseline_tokens = parse_tokens(out / "baseline.stdout")
    mcp_tokens = parse_tokens(out / "mcp.stdout")

    print(
        f"Codex fork/exec benchmark — target={args.target_repo}@{args.target_ref}"
    )
    print("=" * 74)
    header = f"{'metric':<30}{'baseline':>14}{'with mcp-cli':>16}{'delta':>14}"
    print(header)
    print("-" * 74)
    print(row("execve total", baseline_total, mcp_total))

    # Per-binary breakdown. Show every binary that was seen in
    # *either* run — the headline DAEMON_REPLACED list goes first for
    # quick scanning, then any leftover binaries (sed, awk, nl, jq,
    # …) sorted by combined call count desc.
    if baseline_counts is not None or mcp_counts is not None:
        all_keys: set[str] = set()
        if baseline_counts is not None:
            all_keys.update(baseline_counts.keys())
        if mcp_counts is not None:
            all_keys.update(mcp_counts.keys())

        # First the canonical "expected to be replaced" list, in its
        # declaration order, for stable diffing.
        seen: set[str] = set()
        for b in DAEMON_REPLACED:
            if b not in all_keys:
                continue
            base = baseline_counts.get(b) if baseline_counts is not None else None
            with_mcp = mcp_counts.get(b) if mcp_counts is not None else None
            if (base or 0) == 0 and (with_mcp or 0) == 0:
                continue
            print(row(f"  {b}", base or 0, with_mcp or 0))
            seen.add(b)

        # Then everything else (sed, awk, jq, nl, …), sorted by
        # combined-count desc so the heaviest binary is first.
        leftovers = [k for k in all_keys if k not in seen]
        leftovers.sort(
            key=lambda k: -(
                (baseline_counts or {}).get(k, 0) + (mcp_counts or {}).get(k, 0)
            )
        )
        for b in leftovers:
            base = baseline_counts.get(b) if baseline_counts is not None else None
            with_mcp = mcp_counts.get(b) if mcp_counts is not None else None
            if (base or 0) == 0 and (with_mcp or 0) == 0:
                continue
            print(row(f"  {b}", base or 0, with_mcp or 0))

    # Bridge / daemon should each show up exactly once when on.
    for marker in ("mcp-cli-bridge", "mcp-cli-daemon"):
        base = baseline_counts.get(marker) if baseline_counts is not None else None
        with_mcp = mcp_counts.get(marker) if mcp_counts is not None else None
        if (base or 0) == 0 and (with_mcp or 0) == 0:
            continue
        print(row(f"  {marker}", base or 0, with_mcp or 0))

    print(row("wall clock (s)", baseline_wall, mcp_wall))
    print(row("input tokens", baseline_tokens.get("input"), mcp_tokens.get("input")))
    print(
        row(
            "  cached input tokens",
            baseline_tokens.get("cached_input"),
            mcp_tokens.get("cached_input"),
        )
    )
    print(row("output tokens", baseline_tokens.get("output"), mcp_tokens.get("output")))

    # MCP tool-call routing — proves whether codex actually used the
    # mounted MCP server's tools or fell back to its built-in Bash.
    baseline_mcp_calls = parse_mcp_tool_calls(out / "baseline.stdout")
    mcp_mcp_calls = parse_mcp_tool_calls(out / "mcp.stdout")
    if baseline_mcp_calls or mcp_mcp_calls:
        keys = sorted(set(baseline_mcp_calls) | set(mcp_mcp_calls))
        print()
        print("codex MCP tool calls (server/tool)")
        print("-" * 74)
        for k in keys:
            base = baseline_mcp_calls.get(k, 0)
            with_mcp = mcp_mcp_calls.get(k, 0)
            print(row(f"  {k}", base, with_mcp))

    # Daemon-side per-tool latency, written by run.sh after the with-mcp
    # run via `metrics.tool_latency`. Only the with-mcp run has a daemon
    # to query, so we just print the single column.
    latency = parse_latency_dump(out / "mcp.metrics.tool_latency.json")
    if latency:
        print()
        print("daemon-side per-tool latency (with mcp-cli)")
        print("-" * 74)
        print(f"{'tool':<30}{'calls':>10}{'mean us':>14}{'max us':>14}")
        for entry in latency:
            print(
                f"{entry['tool']:<30}{entry['calls']:>10}{entry['mean_us']:>14}{entry['max_us']:>14}"
            )

    print()
    print(f"Raw artifacts: {out}")
    return 0


def parse_latency_dump(path: Path) -> list[dict]:
    """Read the metrics.tool_latency JSON dump if present.

    Returns the per_tool array sorted by latency_sum_us desc (the
    daemon already sorts this way; we don't re-sort). Missing or
    malformed file → empty list.
    """
    if not path.exists():
        return []
    try:
        import json
        return list(json.loads(path.read_text()).get("per_tool") or [])
    except (OSError, ValueError):
        return []


if __name__ == "__main__":
    raise SystemExit(main())
