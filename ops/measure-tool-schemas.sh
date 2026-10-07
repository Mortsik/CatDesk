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
#   PROFILE   MultiTools, ReadOnly (CatDesk tools/list under a headless pty,
#             computer mode), DevTools (direct stdio measurement of
#             chrome-devtools-mcp@latest — the set CatDesk forwards verbatim
#             in browser mode). Default: all three.
#   --bin     use an existing catdesk binary instead of cargo build --release.
#   --out     also write per-profile tools/list JSON + a machine-readable
#             summary.json into DIR.
#
# The script also prints Combined (Mode::Both + MultiTools) as a COMPUTED SUM
# of the MultiTools and DevTools profiles. Driving CatDesk's full Both path
# headlessly needs a detected browser plus a multi-step TUI wizard, so the
# combined figure is labeled as computed, never as a single tools/list
# capture.
#
# Sizes are UTF-8 bytes; tokens use the compact-JSON chars/4 heuristic (a
# GPT-family tokenizer lands within roughly ±10% of it; the char counts are
# reported alongside).
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
        MultiTools | ReadOnly | DevTools)
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
    PROFILES=(MultiTools ReadOnly DevTools)
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

def utf8(text):
    # Sizes reported in UTF-8 bytes; JSON escaped non-ASCII inflates bytes
    # relative to the char count the tokenizer sees.
    return len(text.encode("utf-8"))

def field_size(tool, field):
    if field not in tool:
        return 0
    return utf8(compact(tool[field]))

def summarize(tools):
    rows = []
    for tool in tools:
        full_text = compact(tool)
        full = utf8(full_text)
        rows.append({
            "name": tool.get("name", "?"),
            "bytes": full,
            "chars": len(full_text),
            "tokens_chars_over_4": round(len(full_text) / 4.0, 1),
            "description_bytes": field_size(tool, "description"),
            "title_bytes": field_size(tool, "title"),
            "inputSchema_bytes": field_size(tool, "inputSchema"),
            "outputSchema_bytes": field_size(tool, "outputSchema"),
            "annotations_bytes": field_size(tool, "annotations"),
            "_meta_bytes": field_size(tool, "_meta"),
        })
    rows.sort(key=lambda r: r["bytes"], reverse=True)
    total = sum(r["bytes"] for r in rows)
    total_chars = sum(r["chars"] for r in rows)
    return {
        "tools": rows,
        "total_bytes": total,
        "total_chars": total_chars,
        "total_tokens_chars_over_4": round(total_chars / 4.0, 1),
        "total_description_bytes": sum(r["description_bytes"] for r in rows),
        "total_inputSchema_bytes": sum(r["inputSchema_bytes"] for r in rows),
        "total_outputSchema_bytes": sum(r["outputSchema_bytes"] for r in rows),
        "total_annotations_bytes": sum(r["annotations_bytes"] for r in rows),
        "total_meta_bytes": sum(r["_meta_bytes"] for r in rows),
    }

payload = json.load(open(sys.argv[1]))
if payload.get("error") is not None:
    print(f"MEASURE FAIL: tools/list returned a JSON-RPC error: {payload['error']}", file=sys.stderr)
    sys.exit(1)
result = payload.get("result")
if not isinstance(result, dict) or not isinstance(result.get("tools"), list) or not result["tools"]:
    print("MEASURE FAIL: tools/list returned no non-empty result.tools array "
          "(HTTP 200 with an empty or malformed catalog is a failure)", file=sys.stderr)
    sys.exit(1)
tools = result["tools"]
summary = summarize(tools)
summary["result_envelope_bytes"] = utf8(compact(result)) - summary["total_bytes"]
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

# Direct measurement of the browser toolset CatDesk forwards verbatim: speak
# stdio JSON-RPC to npx chrome-devtools-mcp@latest and record the resolved
# package version from the initialize serverInfo.
cat >"$TMP/devtools_measure.py" <<'PYEOF'
import json, os, subprocess, sys

def compact(value):
    return json.dumps(value, separators=(",", ":"), ensure_ascii=False)

def utf8(text):
    return len(text.encode("utf-8"))

proc = subprocess.Popen(
    ["npx", "-y", "chrome-devtools-mcp@latest"],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
    env={**os.environ, "NO_COLOR": "1"},
    text=True, bufsize=1,
)

def send(obj):
    proc.stdin.write(json.dumps(obj) + "\n")
    proc.stdin.flush()

def readline_json():
    while True:
        line = proc.stdout.readline()
        if not line:
            raise RuntimeError("chrome-devtools-mcp closed stdout before replying")
        line = line.strip()
        if line:
            return json.loads(line)

send({"jsonrpc": "2.0", "id": 1, "method": "initialize",
      "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                 "clientInfo": {"name": "catdesk-measure", "version": "1.0"}}})
init = readline_json()
server_info = init.get("result", {}).get("serverInfo", {})
version = server_info.get("version", "unknown")
send({"jsonrpc": "2.0", "method": "notifications/initialized"})
send({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}})
tools = None
while tools is None:
    msg = readline_json()
    if msg.get("id") == 2:
        if msg.get("error") is not None:
            print(f"MEASURE FAIL: DevTools tools/list error: {msg['error']}", file=sys.stderr)
            sys.exit(1)
        tools = msg.get("result", {}).get("tools", [])
if not tools:
    print("MEASURE FAIL: DevTools tools/list returned an empty catalog", file=sys.stderr)
    sys.exit(1)

rows = []
for tool in tools:
    text = compact(tool)
    rows.append({
        "name": tool.get("name", "?"),
        "bytes": utf8(text),
        "chars": len(text),
        "tokens_chars_over_4": round(len(text) / 4.0, 1),
    })
rows.sort(key=lambda r: r["bytes"], reverse=True)
total = sum(r["bytes"] for r in rows)
total_chars = sum(r["chars"] for r in rows)
summary = {
    "source": "npx chrome-devtools-mcp@latest (stdio)",
    "resolved_version": version,
    "tools": rows,
    "tool_count": len(tools),
    "total_bytes": total,
    "total_chars": total_chars,
    "total_tokens_chars_over_4": round(total_chars / 4.0, 1),
}
json.dump(summary, open(sys.argv[1], "w"), indent=2, ensure_ascii=False)
print(f"chrome-devtools-mcp resolved version: {version}")
print(f"tools: {summary['tool_count']}")
print(f"{'tool':<28} {'bytes':>7} {'~tokens':>8}")
for r in rows:
    print(f"{r['name']:<28} {r['bytes']:>7} {r['tokens_chars_over_4']:>8.0f}")
print(f"{'TOTAL':<28} {total:>7} {summary['total_tokens_chars_over_4']:>8.0f}")
proc.terminate()
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
    if [[ "$PROFILE" == "DevTools" ]]; then
        echo "== profile $PROFILE: measuring chrome-devtools-mcp@latest over stdio =="
        DEVTOOLS_SUMMARY="$TMP/summary-DevTools.json"
        timeout 180 python3 "$TMP/devtools_measure.py" "$DEVTOOLS_SUMMARY" \
            || fail "DevTools measurement failed"
        if [[ -n "$OUT" ]]; then
            mkdir -p "$OUT"
            cp "$DEVTOOLS_SUMMARY" "$OUT/summary-DevTools.json"
        fi
        echo
        continue
    fi
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

# Combined default-session footprint (Mode::Both + MultiTools) as a computed
# sum: CatDesk in Both mode with a live DevTools bridge could not be driven
# headlessly in every environment (it needs a detected browser and a
# multi-step TUI wizard), so when both inputs were measured, report the sum
# explicitly as computed, not as a single tools/list capture.
SUM_FILE="$TMP/summary-Combined.json"
if python3 - "$SUM_FILE" "${OUT:-}" "$TMP" <<'PYEOF'
import json, os, sys

sum_path, out_dir, tmp_dir = sys.argv[1], sys.argv[2], sys.argv[3]
summaries = {}
for name in ("MultiTools", "DevTools"):
    for candidate in (os.path.join(out_dir, f"summary-{name}.json") if out_dir else None,
                      os.path.join(tmp_dir, f"summary-{name}.json")):
        if candidate and os.path.exists(candidate):
            summaries[name] = json.load(open(candidate))
            break
if len(summaries) != 2:
    sys.exit(1)
mt, dt = summaries["MultiTools"], summaries["DevTools"]
combined = {
    "note": "computed sum of measured MultiTools and DevTools profiles, "
            "not a single tools/list capture",
    "tool_count": mt["tool_count"] + dt["tool_count"],
    "total_bytes": mt["total_bytes"] + dt["total_bytes"],
    "total_chars": mt["total_chars"] + dt["total_chars"],
    "total_tokens_chars_over_4": round(mt["total_tokens_chars_over_4"]
                                       + dt["total_tokens_chars_over_4"], 1),
    "devtools_resolved_version": dt.get("resolved_version", "unknown"),
}
json.dump(combined, open(sum_path, "w"), indent=2, ensure_ascii=False)
print(f"== Combined (computed sum: MultiTools + DevTools) ==")
print(f"tools: {combined['tool_count']}  bytes: {combined['total_bytes']}  "
      f"~tokens: {combined['total_tokens_chars_over_4']:,.0f}  "
      f"(chrome-devtools-mcp {combined['devtools_resolved_version']})")
if out_dir:
    json.dump(combined, open(os.path.join(out_dir, "summary-Combined.json"), "w"),
              indent=2, ensure_ascii=False)
sys.exit(0)
PYEOF
then
    :
else
    echo "note: Combined summary skipped (needs measured MultiTools and DevTools profiles)"
fi

echo "MEASURE OK"
