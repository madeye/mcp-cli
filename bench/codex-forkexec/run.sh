#!/usr/bin/env bash
# Codex fork/exec reduction benchmark — see README.md for design.
#
# Runs the same prompt under Codex twice (with and without the
# mcp-cli MCP plugin), counts per-binary fork/exec, and prints a
# comparison table.
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

CODEX=${CODEX:-codex}
MCP_CLI=${MCP_CLI:-mcp-cli}
TARGET_REPO=${TARGET_REPO:-https://github.com/openai/codex}
TARGET_REF=${TARGET_REF:-}
TARGET_DIR=${TARGET_DIR:-}
OUT_DIR=${OUT_DIR:-$(mktemp -d -t mcpcli-bench-XXXXXX)}
TRACER=${TRACER:-}

# Binaries the daemon obviates. Used both as the shim allowlist and
# as the headline rows in compare.py.
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
require "$CODEX"
require "$MCP_CLI"

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

# Resolve the target ref. Default to the latest non-pre-release
# tag for a deterministic snapshot, not a moving HEAD. Try `gh
# release list` first (handles repos like openai/codex that use
# release tags like `rust-v0.121.0` rather than bare `v0.121.0`),
# then fall back to a relaxed semver-ish tag scan.
if [[ -z "$TARGET_REF" && -z "$TARGET_DIR" ]]; then
    repo_slug=${TARGET_REPO#https://github.com/}
    repo_slug=${repo_slug%.git}
    if command -v gh >/dev/null 2>&1 && [[ "$repo_slug" != "$TARGET_REPO" ]]; then
        TARGET_REF=$(gh release list \
            -R "$repo_slug" --exclude-pre-releases --limit 1 \
            --json tagName --jq '.[].tagName' 2>/dev/null || true)
    fi
    if [[ -z "$TARGET_REF" ]]; then
        # Fallback: grep tags for *anything* ending in vN.N.N(.N)?
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

# Generate per-run shim wrappers when in shim mode. Each shim is a
# bash one-liner that appends to a per-binary counter file then
# execs the real binary (whose path was resolved at setup time, so
# the shim doesn't have to re-walk PATH).
make_shims() {
    local label=$1
    local shim_dir=$OUT_DIR/$label.shim
    local counter_dir=$OUT_DIR/$label.counters
    mkdir -p "$shim_dir" "$counter_dir"
    for b in "${SHIMMED_BINS[@]}"; do
        local real
        real=$(command -v "$b" || true)
        [[ -z "$real" ]] && continue
        # Skip if the resolved binary is itself in our shim dir
        # (shouldn't happen with a fresh dir, but defensive).
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

# When codex shells out via `zsh -lc "…"`, the login shell re-sources
# the user's profile and prepends `/opt/homebrew/bin` (etc.) to PATH,
# shadowing our shim dir if it lives further back. Workaround: point
# `$ZDOTDIR` at a per-run init dir whose files source the real ones
# first and then re-prepend the shim dir, so the shim wins on every
# zsh startup regardless of -l. Returns the path on stdout.
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

run_codex() {
    local label=$1
    local home_dir=$2
    local extra_path=${3:-}
    local zdotdir=${4:-}
    local stdout=$OUT_DIR/$label.stdout
    local stderr=$OUT_DIR/$label.stderr
    local timing=$OUT_DIR/$label.timing
    local trace=$OUT_DIR/$label.trace

    log "running [$label]: stdout=$stdout"
    pushd "$SRC" >/dev/null

    local start end
    start=$(python3 -c 'import time; print(time.monotonic())')

    # PATH-prepend the shim dir for shim mode; harmless otherwise.
    local path_prefix=""
    [[ -n "$extra_path" ]] && path_prefix="$extra_path:"

    # `--add-dir "$OUT_DIR"` whitelists the bench output directory in
    # codex's sandbox, so the shim wrappers' counter writes (and our
    # auto-spawned daemon's per-cwd socket / log) work even under
    # macOS Seatbelt or Linux sandboxing. Without it the shim writes
    # silently EPERM and per-binary counts come back as zero.
    local codex_args=(
        exec
        --skip-git-repo-check
        --json
        --sandbox workspace-write
        --add-dir "$OUT_DIR"
    )

    # Build the env layer. We always inherit the parent env; ZDOTDIR
    # is set only in shim mode so the per-run zsh init can re-prepend
    # the shim dir after the user's profile has had its turn.
    local -a env_args=(env CODEX_HOME="$home_dir" PATH="${path_prefix}$PATH")
    [[ -n "$zdotdir" ]] && env_args+=(ZDOTDIR="$zdotdir")

    case "$TRACER" in
    strace)
        "${env_args[@]}" \
            strace -e trace=execve -f -qq -o "$trace" -- \
            "$CODEX" "${codex_args[@]}" "$(cat "$PROMPT_FILE")" \
            >"$stdout" 2>"$stderr" || true
        ;;
    dtruss)
        "${env_args[@]}" \
            dtruss -f -t execve \
            "$CODEX" "${codex_args[@]}" "$(cat "$PROMPT_FILE")" \
            >"$stdout" 2>"$trace" || true
        ;;
    shim)
        "${env_args[@]}" \
            "$CODEX" "${codex_args[@]}" "$(cat "$PROMPT_FILE")" \
            >"$stdout" 2>"$stderr" || true
        ;;
    esac

    end=$(python3 -c 'import time; print(time.monotonic())')
    python3 -c "print(f'{float('$end') - float('$start'):.3f}')" >"$timing"

    popd >/dev/null
    log "[$label] wall_clock_s=$(cat "$timing")"
}

run_one() {
    local label=$1
    local home_dir=$2
    if [[ "$TRACER" == "shim" ]]; then
        local shim_dir zdotdir
        shim_dir=$(make_shims "$label")
        zdotdir=$(make_zdotdir "$label" "$shim_dir")
        run_codex "$label" "$home_dir" "$shim_dir" "$zdotdir"
    else
        run_codex "$label" "$home_dir" "" ""
    fi
}

# Bootstrap a per-run CODEX_HOME. We want isolation (no inherited
# `mcp_servers` config from the real home) AND a working session
# (auth.json + model config). Strategy: copy auth.json from the real
# home if present, leave config.toml empty so the daemon-default
# applies. Override SOURCE_CODEX_HOME to point at a different home.
SOURCE_CODEX_HOME=${SOURCE_CODEX_HOME:-$HOME/.codex}

bootstrap_codex_home() {
    local target=$1
    mkdir -p "$target"
    if [[ -r "$SOURCE_CODEX_HOME/auth.json" ]]; then
        cp "$SOURCE_CODEX_HOME/auth.json" "$target/auth.json"
    else
        log "warning: no auth.json under $SOURCE_CODEX_HOME — codex will likely 401"
    fi
}

# Baseline ---------------------------------------------------------
BASELINE_HOME="$OUT_DIR/codex-home-baseline"
bootstrap_codex_home "$BASELINE_HOME"
# Baseline mounts no MCP servers — empty config.toml means the agent
# only has its built-in tools.
run_one baseline "$BASELINE_HOME"

# With mcp-cli mounted -------------------------------------------
MCP_HOME="$OUT_DIR/codex-home-mcp"
bootstrap_codex_home "$MCP_HOME"
log "installing mcp-cli into CODEX_HOME=$MCP_HOME"
CODEX_HOME="$MCP_HOME" "$MCP_CLI" install --target codex >/dev/null
run_one mcp "$MCP_HOME"

# Snapshot daemon-side latency counters before the daemon idle-exits.
# Best-effort: if codex left the bridge running, this hits a live
# daemon and pulls per-tool counters; if the daemon already shut down
# it just times out and we skip the section in compare.py.
DUMP_FILE="$OUT_DIR/mcp.metrics.tool_latency.json"
log "dumping daemon metrics.tool_latency to $DUMP_FILE"
LATENCY_REQ='{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"metrics_tool_latency","arguments":{}}}'
if echo "$LATENCY_REQ" | timeout 5 "$(dirname "$MCP_CLI")/mcp-cli-bridge" \
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

log "raw artifacts: $OUT_DIR"
