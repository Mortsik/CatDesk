#!/usr/bin/env bash
# Measure the eager MCP schema footprint CatDesk exposes via tools/list
# (evaluation for catdesk-ojt.8).
#
# Starts the real release binary headless (under a pty) against a throwaway
# HOME/workspace, issues a modern-MCP tools/list request for each requested
# tool-mode profile, and reports per-tool byte and token sizes with a
# per-field breakdown (description, inputSchema, outputSchema, annotations,
# _meta). Numbers come from the live server, so format!()-interpolated
# descriptions and constants are measured as shipped.
#
# Usage:
#   ops/measure-tool-schemas.sh [--bin PATH] [--out DIR] [PROFILE...]
#
#   PROFILE   one of: MultiTools, ReadOnly (default: both).
#   --bin     use an existing catdesk binary instead of cargo build --release.
#   --out     also write per-profile tools/list JSON + a machine-readable
#             summary.json into DIR.
#
# Token approximation: compact JSON characters / 4 (the usual ASCII JSON
# heuristic; a GPT-family tokenizer lands within roughly ±10% of it).
#
# Requirements: cargo, python3, curl. Everything runs against a throwaway
# HOME/workspace/port; the only outside writes are cargo's own target dir.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SLUG="measure"
BIN=""
OUT=""
PROFILES=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        --bin)
            BIN="$2"
            shift 2
            ;;
        --out)
            OUT="$2"
            shift 2
            ;;
        MultiTools | ReadOnly)
            PROFILES+=("$1")
            shift
            ;;
        *)
            echo "unknown argument: $1" >&2
            exit 2
            ;;
    esac
done
if [[ ${#PROFILES[@]} -eq 0 ]]; then
    PROFILES=(MultiTools ReadOnly)
fi
if [[ -z "$BIN" ]]; then
    echo "== building release binary =="
    (cd "$REPO_ROOT" && cargo build --release)
    BIN="$REPO_ROOT/target/release/catdesk"
fi

command -v python3 >/dev/null || { echo "python3 is required" >&2; exit 2; }
command -v curl >/dev/null || { echo "curl is required" >&2; exit 2; }

TMP="$(mktemp -d "${TMPDIR:-/tmp}/catdesk-measure.XXXXXX")"
PTQ_LOG="$TMP/pty.log"
QUIT_FILE="$TMP/quit"
EXITED_FILE="$TMP/exited"
mkdir -p "$TMP"

cleanup() {
    touch "$QUIT_FILE" 2>/dev/null || true
    for _ in $(seq 1 5); do
        [[ -f "$EXITED_FILE" ]] && break
        sleep 1
    done
    [[ -f "$TMP/child.pid" ]] && kill "$(cat "$TMP/child.pid")" 2>/dev/null || true
    rm -rf "$TMP"
}
trap cleanup EXIT

cat >"$TMP/measure.py" <<'PYEOF'
import json, sys

def compact(value):
    return json.dumps(value, separators=(",", ":"), ensure_ascii=False)

def field_size(tool, field):
    if field not in tool:
        return 0
    return len(compact(tool[field]))

def summarize(tools):
    rows = []
    for tool in tools:
        full = len(compact(tool))
        rows.append({
            "name": tool.get("name", "?"),
            "bytes": full,
            "tokens_chars_over_4": round(full / 4.0, 1),
            "description_bytes": field_size(tool, "description"),
            "title_bytes": field_size(tool, "title"),
            "inputSchema_bytes": field_size(tool, "inputSchema"),
            "outputSchema_bytes": field_size(tool, "outputSchema"),
            "annotations_bytes": field_size(tool, "annotations"),
            "_meta_bytes": field_size(tool, "_meta"),
        })
    rows.sort(key=lambda r: r["bytes"], reverse=True)
    total = sum(r["bytes"] for r in rows)
    return {
        "tools": rows,
        "total_bytes": total,
        "total_tokens_chars_over_4": round(total / 4.0, 1),
        "total_description_bytes": sum(r["description_bytes"] for r in rows),
        "total_inputSchema_bytes": sum(r["inputSchema_bytes"] for r in rows),
        "total_outputSchema_bytes": sum(r["outputSchema_bytes"] for r in rows),
        "total_annotations_bytes": sum(r["annotations_bytes"] for r in rows),
        "total_meta_bytes": sum(r["_meta_bytes"] for r in rows),
    }

payload = json.load(open(sys.argv[1]))
result = payload.get("result", {})
tools = result.get("tools", [])
summary = summarize(tools)
summary["result_envelope_bytes"] = len(compact(result)) - summary["total_bytes"]
summary["tool_count"] = len(tools)
json.dump(summary, open(sys.argv[2], "w"), indent=2, ensure_ascii=False)

print(f"tools: {summary['tool_count']}")
print(f"{'tool':<20} {'bytes':>7} {'~tokens':>8} {'desc':>6} {'in':>6} {'out':>6} {'anno':>5} {'meta':>5}")
for r in summary["tools"]:
    print(
        f"{r['name']:<20} {r['bytes']:>7} {r['tokens_chars_over_4']:>8.0f} "
        f"{r['description_bytes']:>6} {r['inputSchema_bytes']:>6} "
        f"{r['outputSchema_bytes']:>6} {r['annotations_bytes']:>5} {r['_meta_bytes']:>5}"
    )
print(
    f"{'TOTAL':<20} {summary['total_bytes']:>7} "
    f"{summary['total_tokens_chars_over_4']:>8.0f} "
    f"{summary['total_description_bytes']:>6} {summary['total_inputSchema_bytes']:>6} "
    f"{summary['total_outputSchema_bytes']:>6} {summary['total_annotations_bytes']:>5} "
    f"{summary['total_meta_bytes']:>5}"
)
print(f"result envelope beyond the tools array: {summary['result_envelope_bytes']} bytes")
PYEOF

cat >"$TMP/drive.py" <<'EOF'
import fcntl, os, pty, select, struct, sys, termios, time

bin_path, quit_file, pty_log, exited = sys.argv[1:5]
pid, fd = pty.fork()
if pid == 0:
    os.execv(bin_path, [bin_path])
# Give the TUI a real size: a 0x0 pty renders nothing useful.
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
print(pid, flush=True)
# The app blocks on the mode-select screen (keys 1/2/3) before it binds the
# HTTP server; press '1' (Computer) until the main TUI takes over. The key is
# unbound in the main loop, so extra presses are harmless.
mode_presses = 0
with open(pty_log, "wb") as log:
    end = time.time() + 3600
    while time.time() < end:
        if os.path.exists(quit_file):
            # 'q' is the ordinary quit path: clean shutdown drains diagnostics.
            try:
                os.write(fd, b"q")
            except OSError:
                break
            time.sleep(1)
            continue
        if mode_presses < 40 and int(time.time() * 2) % 4 == 0:
            try:
                os.write(fd, b"1")
                mode_presses += 1
            except OSError:
                break
        ready, _, _ = select.select([fd], [], [], 0.5)
        if ready:
            try:
                data = os.read(fd, 65536)
            except OSError:
                break
            if not data:
                break
            log.write(data)
    try:
        os.close(fd)
    except OSError:
        pass
    _, status = os.waitpid(pid, 0)
    open(exited, "w").write(f"{status}\n")
EOF

fail() {
    echo "MEASURE FAIL: $*" >&2
    exit 1
}

for PROFILE in "${PROFILES[@]}"; do
    # Config enum values are serde camelCase: "multiTools" / "readOnly".
    case "$PROFILE" in
        MultiTools) TOOL_MODE_VALUE="multiTools" ;;
        ReadOnly) TOOL_MODE_VALUE="readOnly" ;;
    esac
    HOME_DIR="$TMP/home-$PROFILE"
    WORKSPACE="$TMP/workspace-$PROFILE"
    mkdir -p "$HOME_DIR/.catdesk" "$WORKSPACE"

    PORT="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')"
    BASE="http://127.0.0.1:$PORT"
    cat >"$HOME_DIR/.catdesk/config.toml" <<EOF
mcpSlug = "$SLUG"
ngrokAuthtoken = "measure-dummy-never-used"
ngrokDomain = "measure.invalid"
chatgptConnectorRevision = 999999
theme = "concise"
mode = "computer"
toolMode = "$TOOL_MODE_VALUE"
EOF

    echo "== profile $PROFILE: starting CatDesk under a pty (port $PORT) =="
    export HOME="$HOME_DIR"
    export WORKSPACE_ROOT="$WORKSPACE"
    export PORT
    export TERM="${TERM:-xterm-256color}"
    python3 "$TMP/drive.py" "$BIN" "$QUIT_FILE" "$PTQ_LOG" "$EXITED_FILE" \
        >"$TMP/child.pid" 2>"$TMP/driver.err" &
    DRIVER_PID=$!

    READY=0
    for _ in $(seq 1 120); do
        if curl -sf --max-time 2 "$BASE/$SLUG" >/dev/null 2>&1; then
            READY=1
            break
        fi
        sleep 1
    done
    [[ "$READY" == 1 ]] || {
        tail -c 2000 "$PTQ_LOG" || true
        fail "CatDesk did not become healthy on port $PORT (profile $PROFILE)"
    }

    BODY_FILE="$TMP/tools-list-$PROFILE.json"
    STATUS=$(curl -s -o "$BODY_FILE" -w '%{http_code}' --max-time 30 \
        -X POST "$BASE/$SLUG/mcp" \
        -H "Content-Type: application/json" \
        -H "MCP-Protocol-Version: 2026-07-28" \
        -H "Mcp-Method: tools/list" \
        -d '{"jsonrpc":"2.0","id":"measure","method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}') \
        || fail "tools/list request failed (profile $PROFILE)"
    [[ "$STATUS" == 200 ]] || fail "tools/list returned HTTP $STATUS (profile $PROFILE)"

    touch "$QUIT_FILE"
    for _ in $(seq 1 10); do
        [[ -f "$EXITED_FILE" ]] && break
        sleep 1
    done
    # The driver's quit branch can outlive the app (it writes 'q' on a fixed
    # cadence); stop the app (child.pid holds the pty child) and the driver
    # itself, then reap the driver so the next profile starts clean.
    if [[ -f "$TMP/child.pid" ]]; then
        kill "$(cat "$TMP/child.pid")" 2>/dev/null || true
    fi
    kill "$DRIVER_PID" 2>/dev/null || true
    wait "$DRIVER_PID" 2>/dev/null || true
    rm -f "$QUIT_FILE" "$EXITED_FILE"

    if [[ -n "$OUT" ]]; then
        mkdir -p "$OUT"
        cp "$BODY_FILE" "$OUT/tools-list-$PROFILE.json"
    fi

    echo "== profile $PROFILE: tools/list schema footprint =="
    if [[ -n "$OUT" ]]; then
        python3 "$TMP/measure.py" "$BODY_FILE" "$OUT/summary-$PROFILE.json"
    else
        python3 "$TMP/measure.py" "$BODY_FILE" "$TMP/summary-$PROFILE.json"
    fi
    echo
done

echo "MEASURE OK"
