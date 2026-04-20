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

TOKEN_RE = re.compile(
    r"(input|output|prompt|completion)\s+tokens?\s*[:=]\s*(\d+)",
    re.IGNORECASE,
)


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
            # Normalise prompt → input, completion → output so the
            # column header is stable regardless of which Codex
            # version we're parsing.
            if kind == "prompt":
                kind = "input"
            elif kind == "completion":
                kind = "output"
            counts[kind] = max(counts.get(kind, 0), n)
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
    return "dtruss" if platform.system() == "Darwin" else "strace"


def row(label: str, baseline, mcp, width: int = 30) -> str:
    return f"{label:<{width}}{fmt(baseline):>14}{fmt(mcp):>16}{fmt_delta(baseline, mcp):>14}"


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--out-dir", required=True)
    ap.add_argument("--target-repo", default="?")
    ap.add_argument("--target-ref", default="?")
    ap.add_argument(
        "--tracer",
        choices=("strace", "dtruss"),
        default=None,
        help="Override the auto-detected tracer (defaults to strace on "
        "Linux, dtruss on macOS).",
    )
    args = ap.parse_args()

    out = Path(args.out_dir)
    tracer = args.tracer or select_tracer()

    baseline_trace = out / "baseline.trace"
    mcp_trace = out / "mcp.trace"
    baseline_counts = (
        parse_trace.parse(str(baseline_trace), tracer) if baseline_trace.exists() else None
    )
    mcp_counts = parse_trace.parse(str(mcp_trace), tracer) if mcp_trace.exists() else None

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

    # Per-binary breakdown for the binaries we expect to vanish.
    if baseline_counts is not None or mcp_counts is not None:
        for b in DAEMON_REPLACED:
            base = baseline_counts.get(b) if baseline_counts is not None else None
            with_mcp = mcp_counts.get(b) if mcp_counts is not None else None
            # Skip rows where both runs saw zero — they only add noise.
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
    print(row("output tokens", baseline_tokens.get("output"), mcp_tokens.get("output")))

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
