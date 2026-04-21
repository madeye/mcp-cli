#!/usr/bin/env bash
# Claude Code fork/exec reduction benchmark — twin of
# bench/codex-forkexec/run.sh, adapted for the `claude` CLI.
#
# Runs the same prompt under Claude Code three times (baseline,
# cold mcp-cli, warm mcp-cli), counts per-binary fork/exec, and
# prints a comparison table via compare.py.
#
# Three tracer backends, auto-picked in order of preference:
#   - strace   (Linux, requires strace on PATH)
#   - dtruss   (macOS, requires root)
#   - shim     (portable; PATH-shadows known binaries with bash
#              wrappers that bump a counter file before delegating
#              to the real binary)
#
# Override with TRACER={strace,dtruss,shim}.

set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)

CLAUDE=${CLAUDE:-claude}
MCP_CLI=${MCP_CLI:-mcp-cli}
TARGET_REPO=${TARGET_REPO:-https://github.com/openai/codex}
TARGET_REF=${TARGET_REF:-}
TARGET_DIR=${TARGET_DIR:-}
OUT_DIR=${OUT_DIR:-$(mktemp -d -t mcpcli-claude-bench-XXXXXX)}
TRACER=${TRACER:-}

# Binaries the daemon obviates. Same list as codex-forkexec — the
# whole point of the twin bench is that the payoff should show up
# across both agents.
SHIMMED_BINS=(cat head tail grep rg ripgrep find ls git jq sed awk wc)

log() { printf '[bench] %s\n' "$*" >&2; }

require() {
    command -v "$1" >/dev/null 2>&1 || {
        log "missing prerequisite: $1"
        exit 1
    }
}

require git
require python3
require "$CLAUDE"
require "$MCP_CLI"

# Resolve the absolute path of the mcp-cli-bridge binary — it lives
# next to the mcp-cli installer. We embed this absolute path in the
# --mcp-config JSON so claude can spawn it without depending on PATH
# once it's inside the sandbox.
MCP_CLI_ABS=$(command -v "$MCP_CLI")
BRIDGE_ABS="$(dirname "$MCP_CLI_ABS")/mcp-cli-bridge"
[[ -x "$BRIDGE_ABS" ]] || {
    log "expected mcp-cli-bridge next to $MCP_CLI_ABS, got $BRIDGE_ABS"
    exit 1
}

# Auto-pick a tracer if the user didn't force one.
if [[ -z "$TRACER" ]]; then
    case "$(uname -s)" in
    Linux) command -v strace >/dev/null 2>&1 && TRACER=strace || TRACER=shim ;;
    Darwin)
        if command -v dtruss >/dev/null 2>&1 && [[ "$(id -u)" -eq 0 ]]; then
            TRACER=dtruss
        else
            TRACER=shim
        fi
        ;;
    *) TRACER=shim ;;
    esac
fi
log "tracer: $TRACER"

case "$TRACER" in
strace) require strace ;;
dtruss)
    require dtruss
    [[ "$(id -u)" -eq 0 ]] || {
        log "dtruss requires root"
        exit 1
    }
    ;;
shim) ;; # Generated below per-run.
*)
    log "unknown tracer: $TRACER"
    exit 1
    ;;
esac

# Resolve the target ref. Default to the latest non-pre-release tag
# for a deterministic snapshot. Same logic as the codex twin.
if [[ -z "$TARGET_REF" && -z "$TARGET_DIR" ]]; then
    repo_slug=${TARGET_REPO#https://github.com/}
    repo_slug=${repo_slug%.git}
    if command -v gh >/dev/null 2>&1 && [[ "$repo_slug" != "$TARGET_REPO" ]]; then
        TARGET_REF=$(gh release list \
            -R "$repo_slug" --exclude-pre-releases --limit 1 \
            --json tagName --jq '.[].tagName' 2>/dev/null || true)
    fi
    if [[ -z "$TARGET_REF" ]]; then
        TARGET_REF=$(
            git ls-remote --tags --refs "$TARGET_REPO" |
                awk -F/ '{print $NF}' |
                grep -E 'v?[0-9]+\.[0-9]+\.[0-9]+(\.[0-9]+)?$' |
                grep -viE 'alpha|beta|rc|pre' |
                sort -V |
                tail -n1
        )
    fi
    [[ -n "$TARGET_REF" ]] || {
        log "could not resolve a release tag from $TARGET_REPO"
        exit 1
    }
fi

if [[ -n "$TARGET_DIR" ]]; then
    SRC="$TARGET_DIR"
    log "reusing target dir: $SRC"
else
    SRC="$OUT_DIR/src"
    log "cloning $TARGET_REPO@$TARGET_REF into $SRC"
    git clone --depth 1 --branch "$TARGET_REF" "$TARGET_REPO" "$SRC" >/dev/null 2>&1
fi

PROMPT_FILE="$HERE/prompt.md"
[[ -r "$PROMPT_FILE" ]] || {
    log "prompt file missing: $PROMPT_FILE"
    exit 1
}

# Generate per-run shim wrappers. Identical mechanics to the codex
# twin — kept in sync by copy rather than shared script so each bench
# is self-contained.
make_shims() {
    local label=$1
    local shim_dir=$OUT_DIR/$label.shim
    local counter_dir=$OUT_DIR/$label.counters
    mkdir -p "$shim_dir" "$counter_dir"
    for b in "${SHIMMED_BINS[@]}"; do
        local real
        real=$(command -v "$b" || true)
        [[ -z "$real" ]] && continue
        [[ "$real" == "$shim_dir/$b" ]] && continue
        cat >"$shim_dir/$b" <<EOF
#!/usr/bin/env bash
printf '1\n' >>"$counter_dir/$b.count"
exec "$real" "\$@"
EOF
        chmod +x "$shim_dir/$b"
    done
    printf '%s\n' "$shim_dir"
}

# When claude shells out via `zsh -lc "…"` (same pattern codex uses),
# the login shell re-sources the user's profile and prepends
# `/opt/homebrew/bin` (etc.) to PATH, shadowing our shim dir. Same
# ZDOTDIR workaround as the codex bench — source the real init files
# then re-prepend the shim dir so it wins on every zsh startup.
make_zdotdir() {
    local label=$1
    local shim_dir=$2
    local zdotdir=$OUT_DIR/$label.zdotdir
    mkdir -p "$zdotdir"
    for f in .zshenv .zprofile .zshrc .zlogin; do
        local real_home_file=$HOME/$f
        cat >"$zdotdir/$f" <<EOF
# Bench-generated zsh init. Source the real \$HOME/$f first if it
# exists, then re-prepend the bench shim dir so it wins over any
# PATH the user's profile sets.
[[ -r "$real_home_file" ]] && source "$real_home_file"
export PATH="$shim_dir:\$PATH"
EOF
    done
    printf '%s\n' "$zdotdir"
}

# Generate the --mcp-config JSON for the cold/warm passes. We embed
# --daemon-arg=--idle-timeout=30m so the daemon survives between cold
# and warm passes; the bridge forwards --daemon-arg to the auto-spawn.
# Same rationale as the codex bench's config-toml patch.
MCP_CONFIG_FILE="$OUT_DIR/mcp-config.json"
cat >"$MCP_CONFIG_FILE" <<EOF
{
  "mcpServers": {
    "mcp-cli": {
      "command": "$BRIDGE_ABS",
      "args": ["--daemon-arg=--idle-timeout=30m"]
    }
  }
}
EOF
log "wrote --mcp-config to $MCP_CONFIG_FILE"

# The tools we strip from claude's built-in palette for the cold/warm
# passes — the same set mcp-cli replaces. Leaving Edit/Write/NotebookEdit
# in place would let the agent accidentally modify files; the prompt
# says "read-only analysis" but belt-and-braces. TodoWrite / Task /
# agent-internal orchestration tools stay enabled.
#
# `--disallowed-tools` is declared variadic (`<tools...>`) in
# commander.js, so *any* positional after it (even with comma-joined
# values like "Bash,Read") gets consumed as another tool name —
# including the prompt. Two fixes are required:
#   1. Comma-joined values so the "list" collapses to one token.
#   2. Pipe the prompt via stdin (omit the positional prompt arg)
#      so there's nothing for the variadic to grab.
# Without (2) claude errors with "Input must be provided either
# through stdin or as a prompt argument when using --print".
MCP_DISALLOWED_TOOLS="Bash,Read,Grep,Glob,Edit,Write,NotebookEdit"

run_claude() {
    local label=$1
    local extra_path=${2:-}
    local zdotdir=${3:-}
    local use_mcp=${4:-0}

    local stdout=$OUT_DIR/$label.stdout
    local stderr=$OUT_DIR/$label.stderr
    local timing=$OUT_DIR/$label.timing
    local trace=$OUT_DIR/$label.trace

    log "running [$label]: stdout=$stdout"
    pushd "$SRC" >/dev/null

    local start end
    start=$(python3 -c 'import time; print(time.monotonic())')

    local path_prefix=""
    [[ -n "$extra_path" ]] && path_prefix="$extra_path:"

    # Claude Code equivalents of the codex flags:
    #   --dangerously-skip-permissions no interactive approval; matches
    #                                 `codex --sandbox workspace-write
    #                                 --prefer-mcp` non-interactive
    #                                 mode. Safe here because the
    #                                 prompt forbids mutations.
    #   --output-format stream-json   event stream; lets compare.py
    #                                 count tool_use events and
    #                                 extract usage counters
    #   --add-dir $OUT_DIR            lets shim counter writes and
    #                                 daemon sockets/logs land in
    #                                 the bench output dir
    #
    # NOTE on --bare: we intentionally DO NOT pass --bare. It blocks
    # keychain reads, which breaks macOS OAuth; in environments
    # without ANTHROPIC_API_KEY set explicitly, claude errors out with
    # "Not logged in" under --bare. The trade is that the user's
    # hooks / plugins / CLAUDE.md auto-discovery run in all three
    # passes — comparative deltas stay meaningful because the bias
    # is constant across baseline/cold/warm, but absolute numbers
    # reflect the user's environment. If you want the --bare-isolated
    # numbers, set ANTHROPIC_API_KEY and re-add --bare to claude_args.
    local claude_args=(
        -p
        --dangerously-skip-permissions
        --add-dir "$OUT_DIR"
        --output-format stream-json
        --verbose
    )
    if [[ "$use_mcp" == "1" ]]; then
        claude_args+=(
            --mcp-config "$MCP_CONFIG_FILE"
            --disallowed-tools "$MCP_DISALLOWED_TOOLS"
        )
    fi

    local -a env_args=(env PATH="${path_prefix}$PATH")
    [[ -n "$zdotdir" ]] && env_args+=(ZDOTDIR="$zdotdir")

    # Prompt via stdin, not positional. See the MCP_DISALLOWED_TOOLS
    # comment above — the variadic `--disallowed-tools` would otherwise
    # swallow the positional prompt.
    case "$TRACER" in
    strace)
        "${env_args[@]}" \
            strace -e trace=execve -f -qq -o "$trace" -- \
            "$CLAUDE" "${claude_args[@]}" \
            <"$PROMPT_FILE" >"$stdout" 2>"$stderr" || true
        ;;
    dtruss)
        "${env_args[@]}" \
            dtruss -f -t execve \
            "$CLAUDE" "${claude_args[@]}" \
            <"$PROMPT_FILE" >"$stdout" 2>"$trace" || true
        ;;
    shim)
        "${env_args[@]}" \
            "$CLAUDE" "${claude_args[@]}" \
            <"$PROMPT_FILE" >"$stdout" 2>"$stderr" || true
        ;;
    esac

    end=$(python3 -c 'import time; print(time.monotonic())')
    python3 -c "print(f'{float('$end') - float('$start'):.3f}')" >"$timing"

    popd >/dev/null
    log "[$label] wall_clock_s=$(cat "$timing")"
}

run_one() {
    local label=$1
    local use_mcp=$2
    if [[ "$TRACER" == "shim" ]]; then
        local shim_dir zdotdir
        shim_dir=$(make_shims "$label")
        zdotdir=$(make_zdotdir "$label" "$shim_dir")
        run_claude "$label" "$shim_dir" "$zdotdir" "$use_mcp"
    else
        run_claude "$label" "" "" "$use_mcp"
    fi
}

# Baseline ---------------------------------------------------------
# Vanilla claude with full built-in tool palette, no MCP config.
run_one baseline 0

# Cold mcp-cli -----------------------------------------------------
# Fresh daemon (idle-timeout 30m so it survives the wait for the warm
# pass). Built-in I/O tools disabled so claude has to route through
# mcp-cli's MCP tools.
run_one mcp 1

# Warm mcp-cli -----------------------------------------------------
# Same --mcp-config -> same bridge path -> same canonical socket, so
# the bridge reconnects to the already-running daemon. search_cache +
# parse_cache populated, prewarm long since finished.
run_one mcp_warm 1

# Snapshot daemon-side latency counters. After both passes so the
# per-tool counters include cold+warm calls.
DUMP_FILE="$OUT_DIR/mcp.metrics.tool_latency.json"
log "dumping daemon metrics.tool_latency to $DUMP_FILE"
LATENCY_REQ='{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"metrics_tool_latency","arguments":{}}}'
if echo "$LATENCY_REQ" | timeout 5 "$(dirname "$MCP_CLI_ABS")/mcp-cli-bridge" \
    --root "$SRC" \
    --no-autospawn 2>/dev/null |
    python3 -c '
import json, sys
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        msg = json.loads(line)
    except ValueError:
        continue
    text = (msg.get("result") or {}).get("content", [{}])[0].get("text")
    if text:
        print(text)
        break
' >"$DUMP_FILE" 2>/dev/null; then
    [[ -s "$DUMP_FILE" ]] || rm -f "$DUMP_FILE"
else
    rm -f "$DUMP_FILE"
fi

# Compare ---------------------------------------------------------
log "computing comparison"
python3 "$HERE/compare.py" \
    --out-dir "$OUT_DIR" \
    --tracer "$TRACER" \
    --target-repo "$TARGET_REPO" \
    --target-ref "${TARGET_REF:-(reused $TARGET_DIR)}"
