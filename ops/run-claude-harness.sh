#!/usr/bin/env bash
# Drive a real Claude Code session against a live CatDesk (bead catdesk-ojt.10
# harness-side measurement).
#
# Starts the release binary headless (pty, throwaway HOME/workspace/port,
# multiTools profile), points Claude Code at it through an MCP HTTP config in
# an ISOLATED CLAUDE_CONFIG_DIR (real OAuth credentials copied in), and runs
# `claude -p` with the same fixture prompt as the Codex runner.
#
# ojt.8 established that Claude Code has historically spoken the
# initialize-era protocol while CatDesk implements only modern MCP
# 2026-07-28 (no initialize handler, GET SSE disabled) — so this run is
# expected to either connect through a modern-capable client or fail loudly
# at the transport probe; BOTH outcomes are measurements, and the server's
# connections.jsonl records exactly which requests arrived.
#
# Leaves behind (in --out, default /tmp/catdesk-ojt10-claude):
#   result.json    claude -p --output-format json result (usage included)
#   session.jsonl  the session transcript from CLAUDE_CONFIG_DIR/projects
#   connections.jsonl  CatDesk diagnostics records for the run
#   mcp.log / stderr  client-side MCP diagnostics
#   meta.json      wall clock, exit code, claude version
#
# Usage: ops/run-claude-harness.sh [--bin PATH] [--out DIR]
set -euo pipefail

REAL_HOME="$HOME"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SLUG="bench"
BIN=""
OUT="/tmp/catdesk-ojt10-claude"
while [[ $# -gt 0 ]]; do
    case "$1" in
        --bin) BIN="$2"; shift 2 ;;
        --out) OUT="$2"; shift 2 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done
if [[ -z "$BIN" ]]; then
    echo "== building release binary =="
    (cd "$REPO_ROOT" && cargo build --release)
    BIN="$REPO_ROOT/target/release/catdesk"
fi
command -v claude >/dev/null || { echo "claude CLI is required" >&2; exit 2; }
command -v python3 >/dev/null || { echo "python3 is required" >&2; exit 2; }

mkdir -p "$OUT"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/catdesk-claude.XXXXXX")"
PTQ_LOG="$TMP/pty.log"
QUIT_FILE="$TMP/quit"
EXITED_FILE="$TMP/exited"

cleanup() {
    touch "$QUIT_FILE" 2>/dev/null || true
    for _ in $(seq 1 5); do
        [[ -f "$EXITED_FILE" ]] && break
        sleep 1
    done
    if [[ -n "${DRIVER_PID:-}" ]]; then
        kill "$DRIVER_PID" 2>/dev/null || true
    fi
    rm -rf "$TMP"
}
trap cleanup EXIT

cat >"$TMP/drive.py" <<'EOF'
import fcntl, os, pty, select, struct, sys, termios, time

bin_path, quit_file, pty_log, exited = sys.argv[1:5]
pid, fd = pty.fork()
if pid == 0:
    os.execv(bin_path, [bin_path])
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
print(pid, flush=True)
mode_presses = 0
with open(pty_log, "wb") as log:
    end = time.time() + 3600
    while time.time() < end:
        if os.path.exists(quit_file):
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

HOME_DIR="$TMP/home"
WORKSPACE="$TMP/workspace"
CLAUDE_DIR="$TMP/claude-home"
mkdir -p "$HOME_DIR/.catdesk" "$WORKSPACE" "$CLAUDE_DIR"

PORT="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')"
BASE="http://127.0.0.1:$PORT"
cat >"$HOME_DIR/.catdesk/config.toml" <<EOF
mcpSlug = "$SLUG"
ngrokAuthtoken = "bench-dummy-never-used"
ngrokDomain = "bench.invalid"
chatgptConnectorRevision = 999999
partnerBinagotchySeed = "00000000000000ff"
theme = "concise"
mode = "computer"
toolMode = "multiTools"
EOF

echo "== starting CatDesk under a pty (port $PORT) =="
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
    echo "FAIL: CatDesk did not become healthy on port $PORT" >&2
    exit 1
}

# Isolated Claude config dir: real OAuth credentials, benchmark-only settings.
if [[ -f "$REAL_HOME/.claude/.credentials.json" ]]; then
    cp "$REAL_HOME/.claude/.credentials.json" "$CLAUDE_DIR/.credentials.json"
fi
cat >"$TMP/mcp.json" <<EOF
{"mcpServers": {"catdesk": {"type": "http", "url": "$BASE/$SLUG/mcp"}}}
EOF

PROMPT='Work only through the "catdesk" MCP server tools (never local shell). Do these steps in order, without asking anything:
1. Call the catdesk instruction tool (catdesk_instruction) first, as required.
2. Call run_command with EXACTLY this command:
awk '"'"'BEGIN { for (i = 0; i < 24000; i++) printf "S %06d - gate filler line\n", i }'"'"'
The output is very large, so the server externalizes it. After the call, print the value of the responseBudget field of the raw tool result (especially outputRef and originalBytes) — do not print the whole stdout.
3. If step 2 returned a responseBudget.outputRef: call read_result with result_id = that outputRef, offset 0, max_bytes 2000 (one bounded peek) and print the first line of the decoded data.
4. Call run_command with EXACTLY this command:
mkdir -p gate && awk '"'"'BEGIN { for (i = 0; i < 17000; i++) printf "F %06d - gate filler line\n", i }'"'"' > gate/big.txt
5. Call the catdesk read tool with paths ["gate/big.txt"]. Again print only the responseBudget field (outputRef, originalBytes), never the whole file text.
6. If step 5 returned a responseBudget.outputRef: call read_result with result_id = that outputRef, offset 0, max_bytes 2000 (one bounded peek) and print the first line of the decoded data.
7. Finish with a short summary: how many read_result calls you made, and the first line of each peek.'

echo "== running claude -p against the live server =="
STARTED="$(date +%s)"
set +e
( cd "$WORKSPACE" && \
  CLAUDE_CONFIG_DIR="$CLAUDE_DIR" claude -p "$PROMPT" \
      --output-format json \
      --mcp-config "$TMP/mcp.json" \
      --allowedTools "mcp__catdesk__*" \
      --max-turns 15 \
      --dangerously-skip-permissions \
      >"$OUT/result.json" 2>"$OUT/stderr.log" )
CLAUDE_EXIT=$?
set -e
ENDED="$(date +%s)"

touch "$QUIT_FILE"
for _ in $(seq 1 10); do
    [[ -f "$EXITED_FILE" ]] && break
    sleep 1
done
if [[ -f "$TMP/child.pid" ]]; then
    child_pid="$(cat "$TMP/child.pid")"
    rm -f "$TMP/child.pid"
    kill "$child_pid" 2>/dev/null || true
fi
kill "$DRIVER_PID" 2>/dev/null || true
wait "$DRIVER_PID" 2>/dev/null || true

# Newest session transcript from the isolated config dir.
SESSION_FILE="$(python3 - "$CLAUDE_DIR/projects" <<'PYEOF'
import glob, os, sys
root = sys.argv[1]
candidates = glob.glob(os.path.join(root, "**", "*.jsonl"), recursive=True)
print(max(candidates, key=os.path.getmtime) if candidates else "")
PYEOF
)"
[[ -n "$SESSION_FILE" ]] && cp "$SESSION_FILE" "$OUT/session.jsonl"

# Server-side request records for the run (which MCP methods arrived).
for LOG in "$HOME_DIR/.catdesk/logs/connections.jsonl" \
           "$HOME_DIR/.catdesk/logs/connections.jsonl.1" \
           "$HOME_DIR/.catdesk/logs/connections.jsonl.2"; do
    [[ -f "$LOG" ]] && cat "$LOG" >>"$OUT/connections.jsonl"
done
[[ -f "$OUT/connections.jsonl" ]] || : >"$OUT/connections.jsonl"

python3 - "$OUT" "$CLAUDE_EXIT" "$STARTED" "$ENDED" "$BASE" <<'PYEOF'
import json, subprocess, sys

out, code, started, ended, base = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4]), sys.argv[5]
meta = {
    "claude_exit_code": code,
    "wall_seconds": ended - started,
    "endpoint": base,
    "claude_version": subprocess.run(
        ["claude", "--version"], capture_output=True, text=True
    ).stdout.strip(),
}
json.dump(meta, open(f"{out}/meta.json", "w"), indent=2)
print(json.dumps(meta, indent=2))
PYEOF

echo "== stderr tail =="
tail -5 "$OUT/stderr.log" | cut -c1-300 || true
[[ $CLAUDE_EXIT == 0 ]] || {
    echo "NOTE: claude -p exited $CLAUDE_EXIT (see $OUT/stderr.log, $OUT/result.json)" >&2
    exit 0  # a harness-side failure is a measurement, not a script failure
}
echo "CLAUDE HARNESS OK"
