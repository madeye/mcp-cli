#!/usr/bin/env python3
"""Tabulate the baseline vs. with-mcp-cli Claude Code benchmark runs.

Twin of bench/codex-forkexec/compare.py — same 6-col layout
(baseline | cold | cold Δ | warm | Δ vs cold), adapted for the
Claude Code `claude -p --output-format stream-json` event stream.

Reads `<out_dir>/{baseline,mcp,mcp_warm}.{trace,counters,timing,stdout}`
and prints a fixed-width comparison table. Any missing input becomes
a '?' cell in the output rather than aborting the run.
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

# Token extraction.
#
# Claude Code's stream-json `result` event carries usage like:
#   {"type":"result","usage":{"input_tokens":42,
#     "cache_read_input_tokens":100,
#     "cache_creation_input_tokens":0,
#     "output_tokens":23}}
#
# We match the plain `input_tokens` / `output_tokens` keys and also
# recognise Claude's `cache_read_input_tokens` — normalised to
# `cached_input` for column-header parity with the codex twin.
_TOKEN_KINDS = (
    ("input", r"input_tokens"),
    ("output", r"output_tokens"),
    ("cached_input", r"cache_read_input_tokens"),
    ("cache_creation", r"cache_creation_input_tokens"),
)
_TOKEN_RES = [(kind, re.compile(rf'"{key}"\s*:\s*(\d+)')) for kind, key in _TOKEN_KINDS]

# Claude Code emits tool calls as `content_block_start` events wrapping
# a `tool_use` block:
#   {"type":"content_block_start","content_block":
#     {"type":"tool_use","name":"mcp__mcp-cli__fs_read",...}}
# MCP server tools follow the `mcp__<server>__<tool>` naming
# convention; everything else is a built-in (Bash, Read, Grep, …).
_TOOL_USE_RE = re.compile(r'"type":"tool_use"[^{}]*?"name":"([^"]+)"')


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
    """Best-effort token extraction from Claude Code's stdout.

    Claude emits cumulative usage blocks inside `message_delta` events
    *and* a final `result` event. We take the max observed value for
    each kind so partial streams still report the largest number seen.
    """
    if not stdout.exists():
        return {}
    counts: dict[str, int] = {}
    for line in stdout.read_text(errors="replace").splitlines():
        for kind, rx in _TOKEN_RES:
            for m in rx.finditer(line):
                n = int(m.group(1))
                counts[kind] = max(counts.get(kind, 0), n)
    return counts


def parse_tool_uses(stdout: Path) -> dict[str, int]:
    """Count `tool_use` events grouped by tool name.

    MCP tools surface as `mcp__<server>__<tool>`; built-ins as their
    bare name (`Bash`, `Read`, `Grep`, …). The mix of the two is the
    routing evidence — a with-mcp-cli run that shows zero
    `mcp__mcp-cli__*` calls means the integration didn't bind.
    """
    if not stdout.exists():
        return {}
    counts: dict[str, int] = {}
    for line in stdout.read_text(errors="replace").splitlines():
        for m in _TOOL_USE_RE.finditer(line):
            name = m.group(1)
            counts[name] = counts.get(name, 0) + 1
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
    system = platform.system()
    if system == "Linux":
        return "strace"
    if system == "Darwin":
        return "dtruss" if os.geteuid() == 0 else "shim"
    return "shim"


def row(label: str, baseline, mcp, width: int = 30) -> str:
    return f"{label:<{width}}{fmt(baseline):>14}{fmt(mcp):>16}{fmt_delta(baseline, mcp):>14}"


def row_warm(label: str, baseline, cold, warm, width: int = 30) -> str:
    """Six-column row: metric | baseline | cold | cold Δ | warm | Δ vs cold."""
    return (
        f"{label:<{width}}"
        f"{fmt(baseline):>14}"
        f"{fmt(cold):>16}"
        f"{fmt_delta(baseline, cold):>14}"
        f"{fmt(warm):>16}"
        f"{fmt_delta(cold, warm):>14}"
    )


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--out-dir", required=True)
    ap.add_argument("--target-repo", default="?")
    ap.add_argument("--target-ref", default="?")
    ap.add_argument(
        "--tracer",
        choices=("strace", "dtruss", "shim"),
        default=None,
        help="Override the auto-detected tracer.",
    )
    args = ap.parse_args()

    out = Path(args.out_dir)
    tracer = args.tracer or select_tracer()

    if tracer == "shim":
        baseline_input = out / "baseline.counters"
        mcp_input = out / "mcp.counters"
        warm_input = out / "mcp_warm.counters"
    else:
        baseline_input = out / "baseline.trace"
        mcp_input = out / "mcp.trace"
        warm_input = out / "mcp_warm.trace"
    baseline_counts = (
        parse_trace.parse(str(baseline_input), tracer)
        if baseline_input.exists()
        else None
    )
    mcp_counts = (
        parse_trace.parse(str(mcp_input), tracer) if mcp_input.exists() else None
    )
    warm_counts = (
        parse_trace.parse(str(warm_input), tracer) if warm_input.exists() else None
    )
    has_warm = warm_counts is not None or (out / "mcp_warm.timing").exists()

    baseline_total = sum(baseline_counts.values()) if baseline_counts is not None else None
    mcp_total = sum(mcp_counts.values()) if mcp_counts is not None else None
    warm_total = sum(warm_counts.values()) if warm_counts is not None else None

    baseline_wall = read_float(out / "baseline.timing")
    mcp_wall = read_float(out / "mcp.timing")
    warm_wall = read_float(out / "mcp_warm.timing")

    baseline_tokens = parse_tokens(out / "baseline.stdout")
    mcp_tokens = parse_tokens(out / "mcp.stdout")
    warm_tokens = parse_tokens(out / "mcp_warm.stdout") if has_warm else {}

    def _row(label, base, cold, warm):
        if has_warm:
            return row_warm(label, base, cold, warm)
        return row(label, base, cold)

    print(
        f"Claude Code fork/exec benchmark — target={args.target_repo}@{args.target_ref}"
    )
    width = 104 if has_warm else 74
    print("=" * width)
    if has_warm:
        header = (
            f"{'metric':<30}{'baseline':>14}"
            f"{'cold mcp-cli':>16}{'cold Δ':>14}"
            f"{'warm mcp-cli':>16}{'Δ vs cold':>14}"
        )
    else:
        header = f"{'metric':<30}{'baseline':>14}{'with mcp-cli':>16}{'delta':>14}"
    print(header)
    print("-" * width)
    print(_row("execve total", baseline_total, mcp_total, warm_total))

    # Per-binary breakdown.
    if baseline_counts is not None or mcp_counts is not None or warm_counts is not None:
        all_keys: set[str] = set()
        for bag in (baseline_counts, mcp_counts, warm_counts):
            if bag is not None:
                all_keys.update(bag.keys())

        def _get(bag, k):
            return bag.get(k) if bag is not None else None

        seen: set[str] = set()
        for b in DAEMON_REPLACED:
            if b not in all_keys:
                continue
            base = _get(baseline_counts, b)
            cold = _get(mcp_counts, b)
            warm = _get(warm_counts, b)
            if (base or 0) == 0 and (cold or 0) == 0 and (warm or 0) == 0:
                continue
            print(_row(f"  {b}", base or 0, cold or 0, warm or 0 if has_warm else None))
            seen.add(b)

        leftovers = [k for k in all_keys if k not in seen]
        leftovers.sort(
            key=lambda k: -(
                (baseline_counts or {}).get(k, 0)
                + (mcp_counts or {}).get(k, 0)
                + (warm_counts or {}).get(k, 0)
            )
        )
        for b in leftovers:
            base = _get(baseline_counts, b)
            cold = _get(mcp_counts, b)
            warm = _get(warm_counts, b)
            if (base or 0) == 0 and (cold or 0) == 0 and (warm or 0) == 0:
                continue
            print(_row(f"  {b}", base or 0, cold or 0, warm or 0 if has_warm else None))

    for marker in ("mcp-cli-bridge", "mcp-cli-daemon"):
        base = baseline_counts.get(marker) if baseline_counts is not None else None
        cold = mcp_counts.get(marker) if mcp_counts is not None else None
        warm = warm_counts.get(marker) if warm_counts is not None else None
        if (base or 0) == 0 and (cold or 0) == 0 and (warm or 0) == 0:
            continue
        print(_row(f"  {marker}", base or 0, cold or 0, warm or 0 if has_warm else None))

    print(_row("wall clock (s)", baseline_wall, mcp_wall, warm_wall))
    print(
        _row(
            "input tokens",
            baseline_tokens.get("input"),
            mcp_tokens.get("input"),
            warm_tokens.get("input") if has_warm else None,
        )
    )
    print(
        _row(
            "  cached input tokens",
            baseline_tokens.get("cached_input"),
            mcp_tokens.get("cached_input"),
            warm_tokens.get("cached_input") if has_warm else None,
        )
    )
    print(
        _row(
            "  cache creation tokens",
            baseline_tokens.get("cache_creation"),
            mcp_tokens.get("cache_creation"),
            warm_tokens.get("cache_creation") if has_warm else None,
        )
    )
    print(
        _row(
            "output tokens",
            baseline_tokens.get("output"),
            mcp_tokens.get("output"),
            warm_tokens.get("output") if has_warm else None,
        )
    )

    # Tool-use routing — surfaces whether Claude actually used mcp-cli's
    # tools or stayed on its built-in Bash/Read/Grep. Keys prefixed
    # `mcp__mcp-cli__` are the daemon routes; unprefixed names are
    # built-ins (which should drop to zero in cold/warm if
    # --disallowed-tools did its job).
    baseline_tools = parse_tool_uses(out / "baseline.stdout")
    mcp_tools = parse_tool_uses(out / "mcp.stdout")
    warm_tools = parse_tool_uses(out / "mcp_warm.stdout") if has_warm else {}
    if baseline_tools or mcp_tools or warm_tools:
        keys = sorted(set(baseline_tools) | set(mcp_tools) | set(warm_tools))
        print()
        print("Claude Code tool_use events (name)")
        print("-" * width)
        for k in keys:
            base = baseline_tools.get(k, 0)
            cold = mcp_tools.get(k, 0)
            warm = warm_tools.get(k, 0) if has_warm else None
            print(_row(f"  {k}", base, cold, warm))

    # Daemon-side per-tool latency — same as codex twin.
    latency = parse_latency_dump(out / "mcp.metrics.tool_latency.json")
    if latency:
        print()
        print("daemon-side per-tool latency (with mcp-cli)")
        print("-" * width)
        print(f"{'tool':<30}{'calls':>10}{'mean us':>14}{'max us':>14}")
        for entry in latency:
            print(
                f"{entry['tool']:<30}{entry['calls']:>10}{entry['mean_us']:>14}{entry['max_us']:>14}"
            )

    print()
    print(f"Raw artifacts: {out}")
    return 0


def parse_latency_dump(path: Path) -> list[dict]:
    if not path.exists():
        return []
    try:
        import json
        return list(json.loads(path.read_text()).get("per_tool") or [])
    except (OSError, ValueError):
        return []


if __name__ == "__main__":
    raise SystemExit(main())
