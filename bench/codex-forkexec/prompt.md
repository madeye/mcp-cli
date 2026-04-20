You are analysing this Codex source tree (you are running inside a
checkout of `openai/codex`).

Your task: identify the **three** highest-impact performance
improvements you can make to the Codex agent runtime — places where
agent throughput, latency, or memory footprint suffers from a
specific implementation choice that has a tractable fix.

For each finding, deliver:

1. **Symptom** — one sentence describing what slows down or wastes
   resources.
2. **Location** — exact file path(s) and line number(s) where the
   issue lives. Use `path/to/file.rs:LINE` form.
3. **Root cause** — what the code is doing today and why that hurts.
4. **Proposed fix** — a concrete patch sketch (a few lines of
   pseudo-Rust or a clear english description of the data-structure
   / algorithm change). Prefer fixes that are local and reviewable
   over speculative rewrites.
5. **Expected gain** — your honest estimate of the win in latency,
   throughput, or memory, with a brief justification.

Constraints:

* Cite real files. If you cannot find evidence in the tree, say so
  rather than guessing.
* Prefer issues that show up under the daily-use workload of an
  agent CLI: prompt round-trips, tool-call dispatch, file I/O,
  subprocess management, JSON serialization. Skip cosmetic
  refactors and "we should add tests."
* Do not modify any files. This is a read-only analysis.
* **Prefer the `mcp-cli` MCP tools when available.** If your
  toolkit exposes `mcp__mcp-cli__fs_read`, `mcp__mcp-cli__search_grep`,
  `mcp__mcp-cli__code_outline`, `mcp__mcp-cli__code_symbols`, or
  `mcp__mcp-cli__git_status`, use them instead of shelling out to
  `cat` / `rg` / `tree-sitter` / `git`. They return the same data
  more compactly and avoid per-call fork/exec.

When you are done, print the three findings as a numbered list and
exit. No follow-up questions.
