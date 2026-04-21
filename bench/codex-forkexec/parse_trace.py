#!/usr/bin/env python3
"""Count execve events per binary from a strace or shim trace.

Output: JSON object {"total": <int>, "by_binary": {"basename": <int>}}
to stdout. Designed to be cheap and tolerant of malformed lines —
better to undercount one binary than blow up on a corrupted trace.

Usage:
    parse_trace.py --tracer strace --input baseline.trace
    parse_trace.py --tracer shim   --input baseline.counters/

The shim form expects `--input` to point at a *directory* containing
`<binary>.count` files written by the PATH-shadow shim wrappers (one
line per invocation). Used on macOS (no root-gated tracer) and as a
fallback on Linux where strace isn't installed.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
from collections import Counter
from pathlib import Path

# strace -e trace=execve -f lines look like (PID prefix optional):
#   [pid 12345] execve("/usr/bin/cat", ["cat", "foo"], ...) = 0
# We only count successful execs (= 0); failed ones are typically
# the kernel walking PATH and tell us nothing about agent behaviour.
_STRACE_RE = re.compile(r'execve\("([^"]+)"[^=]*=\s*0\b')


def basename(path: str) -> str:
    name = os.path.basename(path)
    return name or path


def parse(path: str, tracer: str) -> Counter:
    if tracer == "shim":
        return _parse_shim(path)
    if tracer == "strace":
        rx = _STRACE_RE
    else:
        raise SystemExit(f"unknown tracer: {tracer}")

    counts: Counter = Counter()
    with open(path, "r", errors="replace") as f:
        for line in f:
            m = rx.search(line)
            if not m:
                continue
            counts[basename(m.group(1))] += 1
    return counts


def _parse_shim(directory: str) -> Counter:
    """Count `.count` files in the shim counter directory.

    Each `<binary>.count` file holds one line per invocation, so the
    line count == fork/exec count for that binary. Missing or empty
    directory → empty counter (treated as 'no data').
    """
    counts: Counter = Counter()
    p = Path(directory)
    if not p.is_dir():
        return counts
    for entry in p.iterdir():
        if entry.suffix != ".count":
            continue
        try:
            with entry.open("rb") as f:
                # Counting newlines is faster than splitlines() for
                # large traces, and tolerant of a stray trailing line
                # without a newline.
                n = sum(1 for _ in f)
        except OSError:
            continue
        counts[entry.stem] = n
    return counts


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument(
        "--input",
        required=True,
        help="trace file (strace) or counter directory (shim)",
    )
    ap.add_argument(
        "--tracer",
        required=True,
        choices=("strace", "shim"),
        help="which tracer produced the input",
    )
    args = ap.parse_args()

    counts = parse(args.input, args.tracer)
    payload = {
        "total": sum(counts.values()),
        "by_binary": dict(sorted(counts.items(), key=lambda kv: (-kv[1], kv[0]))),
    }
    json.dump(payload, sys.stdout, indent=2)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
