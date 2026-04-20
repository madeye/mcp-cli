#!/usr/bin/env python3
"""Count execve events per binary from a strace or dtruss trace file.

Output: JSON object {"total": <int>, "by_binary": {"basename": <int>}}
to stdout. Designed to be cheap and tolerant of malformed lines —
better to undercount one binary than blow up on a corrupted trace.

Usage:
    parse_trace.py --tracer strace --input baseline.trace
    parse_trace.py --tracer dtruss --input baseline.trace
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
from collections import Counter

# strace -e trace=execve -f lines look like (PID prefix optional):
#   [pid 12345] execve("/usr/bin/cat", ["cat", "foo"], ...) = 0
# We only count successful execs (= 0); failed ones are typically
# the kernel walking PATH and tell us nothing about agent behaviour.
_STRACE_RE = re.compile(r'execve\("([^"]+)"[^=]*=\s*0\b')

# dtruss -t execve lines look like:
#   12345/0x123:  execve("/usr/bin/cat\0", 0x..., 0x...)         = 0 0
_DTRUSS_RE = re.compile(r'execve\("([^"]+?)\\0?"[^=]*=\s*0\b')


def basename(path: str) -> str:
    name = os.path.basename(path)
    return name or path


def parse(path: str, tracer: str) -> Counter:
    if tracer == "strace":
        rx = _STRACE_RE
    elif tracer == "dtruss":
        rx = _DTRUSS_RE
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


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--input", required=True, help="path to trace file")
    ap.add_argument(
        "--tracer",
        required=True,
        choices=("strace", "dtruss"),
        help="which tracer produced the file",
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
