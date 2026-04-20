#!/usr/bin/env bash
# Codex fork/exec reduction benchmark — see README.md for design.
#
# Runs the same prompt under Codex twice (with and without the
# mcp-cli MCP plugin), traces execve, and prints a comparison table.
# Designed for Linux+strace and macOS+dtruss; macOS path needs root.

set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)

CODEX=${CODEX:-codex}
MCP_CLI=${MCP_CLI:-mcp-cli}
TARGET_REPO=${TARGET_REPO:-https://github.com/openai/codex}
TARGET_REF=${TARGET_REF:-}
TARGET_DIR=${TARGET_DIR:-}
OUT_DIR=${OUT_DIR:-$(mktemp -d -t mcpcli-bench-XXXXXX)}

log() { printf '[bench] %s\n' "$*" >&2; }

require() {
    command -v "$1" >/dev/null 2>&1 || {
        log "missing prerequisite: $1"
        exit 1
    }
}

require git
require python3
require "$CODEX"
require "$MCP_CLI"

# Pick a tracer up front so we fail fast if it isn't installed.
case "$(uname -s)" in
Linux)
    require strace
    TRACER=(strace -e trace=execve -f -qq -o)
    ;;
Darwin)
    require dtruss
    if [[ "$(id -u)" -ne 0 ]]; then
        log "macOS dtruss requires root; re-run with sudo"
        exit 1
    fi
    # dtruss has no -o; pipe stderr to file instead.
    TRACER=(dtruss -f -t execve)
    ;;
*)
    log "unsupported platform: $(uname -s)"
    exit 1
    ;;
esac

# Resolve the target ref. Default to the most recent semver-ish tag
# on the remote so the benchmark is deterministic against a release,
# not a moving HEAD.
if [[ -z "$TARGET_REF" && -z "$TARGET_DIR" ]]; then
    TARGET_REF=$(
        git ls-remote --tags --refs "$TARGET_REPO" |
            awk -F/ '{print $NF}' |
            grep -E '^v?[0-9]+\.[0-9]+\.[0-9]+$' |
            sort -V |
            tail -n1
    )
    [[ -n "$TARGET_REF" ]] || {
        log "could not resolve a release tag from $TARGET_REPO"
        exit 1
    }
fi

# Clone (or reuse) the target tree.
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

# Run helpers ------------------------------------------------------

# Codex's headless invocation differs slightly between releases.
# Override with CODEX_EXEC if your codex binary uses a different
# subcommand. Default tries `codex exec` then falls back to stdin.
codex_invoke() {
    local home_dir=$1
    local stdout=$2
    local stderr=$3
    if [[ -n "${CODEX_EXEC:-}" ]]; then
        # User-supplied invocation. They get $PROMPT and $home_dir.
        PROMPT_FILE="$PROMPT_FILE" CODEX_HOME="$home_dir" \
            bash -c "$CODEX_EXEC" >"$stdout" 2>"$stderr" || true
        return
    fi
    CODEX_HOME="$home_dir" "$CODEX" exec --skip-git-repo-check \
        "$(cat "$PROMPT_FILE")" >"$stdout" 2>"$stderr" || true
}

run_traced() {
    local label=$1
    local home_dir=$2
    local trace=$OUT_DIR/$label.trace
    local stdout=$OUT_DIR/$label.stdout
    local stderr=$OUT_DIR/$label.stderr
    local timing=$OUT_DIR/$label.timing

    log "running [$label]: trace=$trace stdout=$stdout"
    pushd "$SRC" >/dev/null

    local start end
    start=$(python3 -c 'import time; print(time.monotonic())')

    case "$(uname -s)" in
    Linux)
        "${TRACER[@]}" "$trace" -- env CODEX_HOME="$home_dir" \
            "$CODEX" exec --skip-git-repo-check "$(cat "$PROMPT_FILE")" \
            >"$stdout" 2>"$stderr" || true
        ;;
    Darwin)
        # dtruss writes to stderr; capture it as the trace.
        "${TRACER[@]}" env CODEX_HOME="$home_dir" \
            "$CODEX" exec --skip-git-repo-check "$(cat "$PROMPT_FILE")" \
            >"$stdout" 2>"$trace" || true
        ;;
    esac

    end=$(python3 -c 'import time; print(time.monotonic())')
    python3 -c "print(f'{float('$end') - float('$start'):.3f}')" >"$timing"

    popd >/dev/null
    log "[$label] wall_clock_s=$(cat "$timing")"
}

# Baseline ---------------------------------------------------------
BASELINE_HOME="$OUT_DIR/codex-home-baseline"
mkdir -p "$BASELINE_HOME"
# Empty config dir so no MCP servers are mounted.
run_traced baseline "$BASELINE_HOME"

# With mcp-cli mounted -------------------------------------------
MCP_HOME="$OUT_DIR/codex-home-mcp"
mkdir -p "$MCP_HOME"
log "installing mcp-cli into CODEX_HOME=$MCP_HOME"
CODEX_HOME="$MCP_HOME" "$MCP_CLI" install --target codex >/dev/null
run_traced mcp "$MCP_HOME"

# Compare ---------------------------------------------------------
log "computing comparison"
python3 "$HERE/compare.py" \
    --out-dir "$OUT_DIR" \
    --target-repo "$TARGET_REPO" \
    --target-ref "${TARGET_REF:-(reused $TARGET_DIR)}"

log "raw artifacts: $OUT_DIR"
