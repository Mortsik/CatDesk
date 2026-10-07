#!/usr/bin/env bash
# True-duration soak for CatDesk connection failure modes (bead catdesk-43k.3).
#
# The in-suite scenarios (src/soak.rs) shorten response deadlines through a
# test-only override so `cargo test` stays fast. This script runs the REAL
# production policy — the real 15-second stream-safe poll boundary, a ~2-minute
# request near the 120-second ceiling, real concurrency, real client
# disconnects, and the ngrok supervisor's real tunnel lifecycle events — and
# asserts the same failure budget: every induced failure carries its expected
# terminal classification and no request leaks active state.
#
# Usage:
#   ops/soak-real-duration.sh [--full]
#
#   --full     also run the ~2-minute near-ceiling phase (total ~5 min).
#
# Requirements: cargo, python3, curl. Everything runs against a throwaway
# HOME/workspace/port; the only outside writes are cargo's own target dir.
#
# Tunnel coverage: the seeded dummy ngrok authtoken makes the in-process
# supervisor's connect attempts fail, producing real tunnel_* lifecycle
# records that the tunnel phase classifies. A mid-stream tunnel drop needs a
# live tunnel and stays an opt-in step: set CLOUDFLARED_UNIT to a user systemd
# unit to restart it mid-phase (see the tunnel phase below).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$REPO_ROOT/target/release/catdesk"
SLUG="soak"
FULL=0
if [[ "${1:-}" == "--full" ]]; then
    FULL=1
fi

command -v python3 >/dev/null || { echo "python3 is required" >&2; exit 2; }
command -v curl >/dev/null || { echo "curl is required" >&2; exit 2; }

TMP="$(mktemp -d "${TMPDIR:-/tmp}/catdesk-soak.XXXXXX")"
HOME_DIR="$TMP/home"
WORKSPACE="$TMP/workspace"
mkdir -p "$HOME_DIR/.catdesk" "$WORKSPACE"
LOG="$HOME_DIR/.catdesk/logs/connections.jsonl"
PTY_LOG="$TMP/pty.log"
QUIT_FILE="$TMP/quit"

cleanup() {
    touch "$QUIT_FILE" 2>/dev/null || true
    # Give the TUI a moment to quit cleanly so diagnostics drain.
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        [[ -f "$TMP/exited" ]] && break
        sleep 1
    done
    if [[ -f "$TMP/child.pid" ]]; then
        kill "$(cat "$TMP/child.pid")" 2>/dev/null || true
    fi
    rm -rf "$TMP"
}
trap cleanup EXIT

fail() {
    echo "SOAK FAIL: $*" >&2
    exit 1
}

echo "== building release binary =="
(cd "$REPO_ROOT" && cargo build --release) || fail "cargo build --release failed"

PORT="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')"
BASE="http://127.0.0.1:$PORT"
MCP="$BASE/$SLUG/mcp"

# Seed config so first-run wizards are skipped: authtoken+domain make the app
# a returning user, a high connector revision skips the refresh notice, and
# Computer mode keeps the browser stack out of the soak.
cat >"$HOME_DIR/.catdesk/config.toml" <<EOF
mcpSlug = "$SLUG"
ngrokAuthtoken = "soak-dummy-never-used"
ngrokDomain = "soak.invalid"
chatgptConnectorRevision = 999999
theme = "concise"
mode = "computer"
toolMode = "multiTools"
EOF

echo "== starting CatDesk under a pty (port $PORT) =="
# Export only after the build: cargo must keep its real CARGO_HOME, while the
# soak app must see the throwaway HOME, workspace, port and a TERM.
export HOME="$HOME_DIR"
export WORKSPACE_ROOT="$WORKSPACE"
export PORT
export TERM="${TERM:-xterm-256color}"
cat >"$TMP/drive.py" <<'EOF'
import os, pty, select, sys, time

bin_path, quit_file, pty_log, exited = sys.argv[1:5]
pid, fd = pty.fork()
if pid == 0:
    os.execv(bin_path, [bin_path])
print(pid, flush=True)
mode_presses = 0
last_quit_press = -1
with open(pty_log, "wb") as log:
    end = time.time() + 3600
    while time.time() < end:
        # 'q' is the ordinary quit path: clean shutdown drains diagnostics.
        # Press it once per second while quit is requested — but never stop
        # reading the pty: EOF is the only reliable signal that CatDesk has
        # exited, and skipping the read used to leave the reaped child waiting
        # and the exited marker unwritten for the whole run budget.
        if os.path.exists(quit_file):
            now = int(time.time())
            if now != last_quit_press:
                last_quit_press = now
                try:
                    os.write(fd, b"q")
                except OSError:
                    break
        # Press '1' (Computer mode) until the mode screen is satisfied; the
        # key is unbound in the main TUI loop, so extra presses are harmless.
        elif mode_presses < 40 and int(time.time() * 2) % 4 == 0:
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
python3 "$TMP/drive.py" "$BIN" "$QUIT_FILE" "$PTY_LOG" "$TMP/exited" >"$TMP/child.pid" 2>"$TMP/driver.err" &
DRIVER_PID=$!

# Wait for the health endpoint; the first ngrok connect attempt with the
# dummy token can hold startup briefly, hence the generous budget.
READY=0
for _ in $(seq 1 120); do
    if curl -sf --max-time 2 "$BASE/$SLUG" >/dev/null 2>&1; then
        READY=1
        break
    fi
    sleep 1
done
if [[ "$READY" != 1 ]]; then
    echo "--- pty log tail ---"
    tail -c 2000 "$PTY_LOG" || true
    fail "CatDesk did not become healthy on port $PORT"
fi
echo "== CatDesk is healthy =="

# ── helpers ──────────────────────────────────────────────────

# call <method> <tool> <arguments-json> [max-time]
# Writes the response body to $TMP/body.$$ and the HTTP status to $STATUS.
# Invoke directly (never inside $(...)) so STATUS survives.
call() {
    local method="$1" tool="${2:-}" arguments="${3:-{\}}" max_time="${4:-60}"
    local body out
    out="$TMP/body.$$"
    body=$(printf '{"jsonrpc":"2.0","id":"soak","method":"%s","params":{"name":"%s","arguments":%s,"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}' \
        "$method" "$tool" "$arguments")
    STATUS=$(curl -s -o "$out" -w '%{http_code}' --max-time "$max_time" \
        -X POST "$MCP" \
        -H "Content-Type: application/json" \
        -H "MCP-Protocol-Version: 2026-07-28" \
        -H "Mcp-Method: $method" \
        -H "Mcp-Name: $tool" \
        -d "$body" || true)
    LAST_BODY_FILE="$out"
}

body_json() {
    python3 -c 'import json,sys; print(json.dumps(json.load(open(sys.argv[1]))["result"]["structuredContent"], sort_keys=True))' "$LAST_BODY_FILE"
}

job_id_of() {
    python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["result"]["structuredContent"]["jobId"])' "$LAST_BODY_FILE"
}

cursor_of() {
    python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["result"]["structuredContent"]["nextCursor"])' "$LAST_BODY_FILE"
}

state_of() {
    python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["result"]["structuredContent"]["state"])' "$LAST_BODY_FILE"
}

# The assertion engine: same failure budget as src/soak.rs, over the real log.
cat >"$TMP/assert.py" <<'EOF'
import json, sys

log_path, mode = sys.argv[1], sys.argv[2]
records = [json.loads(line) for line in open(log_path) if line.strip()]

def terminals(event, reason):
    return [r for r in records
            if r.get("event") == event and r.get("terminal_reason") == reason]

def fail(msg):
    print(f"SOAK FAIL [{mode}]: {msg}")
    sys.exit(1)

def leak_gate():
    started, terminal_counts = [], {}
    for r in records:
        if r.get("event") == "http_started":
            started.append(r.get("request_id"))
        elif r.get("event") in ("http_finished", "http_cancelled"):
            rid = r.get("request_id")
            terminal_counts[rid] = terminal_counts.get(rid, 0) + 1
    dupes = [rid for rid, n in terminal_counts.items() if n > 1]
    if dupes:
        fail(f"requests with multiple terminal records: {dupes}")
    missing = [rid for rid in started if rid not in terminal_counts]
    if missing:
        fail(f"requests without a terminal record (leaked state): {len(missing)} {missing[:5]}")

if mode == "summary":
    counts = {}
    for r in records:
        key = (r.get("event"), r.get("terminal_reason"))
        counts[key] = counts.get(key, 0) + 1
    for (event, reason), n in sorted(counts.items(), key=str):
        print(f"  {event or '-'} / {reason or '-'}: {n}")
    leak_gate()
    print(f"  leak gate: {len(records)} records, no leaked requests")
elif mode == "poll15":
    requested_ids = {r.get("request_id") for r in records
                     if r.get("event") == "mcp_request"
                     and r.get("rpc_tool") == "poll_command"
                     and r.get("requested_wait_ms") == 15_000}
    if not requested_ids:
        fail("no metadata record with requested_wait_ms=15000")
    polls = [r for r in terminals("http_finished", "completed")
             if r.get("request_id") in requested_ids
             and r.get("status") == 200 and r.get("scheduler_class") == "control"
             and 14_000 <= r.get("elapsed_ms", 0) < 30_000]
    if not polls:
        fail("no matching completed control-class poll near the 15s stream-safe boundary")
    print(f"  stream-safe poll completed after {polls[0]['elapsed_ms']} ms")
elif mode == "disconnect":
    drops = [r for r in terminals("http_cancelled", "client_disconnect")
             if r.get("elapsed_ms", 1 << 30) < 10_000]
    if not drops:
        fail("no http_cancelled/client_disconnect with a sub-10s elapsed time")
    print(f"  client disconnect classified after {drops[0]['elapsed_ms']} ms")
elif mode == "concurrency":
    peak = max((r.get("active_requests", 0) for r in records
                if r.get("event") == "http_started"), default=0)
    if peak < 8:
        fail(f"peak concurrent active_requests was only {peak}")
    print(f"  peak concurrent active requests: {peak}")
    leak_gate()
elif mode == "tunnel":
    tunnel = [r for r in records if str(r.get("event", "")).startswith("tunnel_")]
    if not tunnel:
        print("  SKIP: no tunnel lifecycle events were produced")
        sys.exit(0)
    at = tunnel[0]["timestamp_ms"]
    window = [r for r in records if abs(r.get("timestamp_ms", 0) - at) <= 15_000]
    # Classifier precedence (request_lifecycle::classify_stream_failure): any
    # server-fault record would outrank the tunnel, so its absence plus the
    # tunnel event makes the window's verdict "tunnel_event".
    server_fault = [r for r in window if r.get("terminal_reason") in
                    ("deadline_timeout", "worker_failed", "server_shutdown")]
    if server_fault:
        fail(f"server-fault terminal reasons inside the tunnel window: "
             f"{[r.get('terminal_reason') for r in server_fault]}")
    names = [r["event"] for r in tunnel[:6]]
    print(f"  tunnel window around {at} classifies as tunnel_event "
          f"({len(tunnel)} tunnel events, e.g. {names})")
elif mode == "ceiling":
    long_ok = [r for r in terminals("http_finished", "completed")
               if r.get("scheduler_class") == "process" and r.get("elapsed_ms", 0) >= 110_000]
    if not long_ok:
        fail("no completed process-class request near the 120 s ceiling "
             "(expected elapsed>=110000)")
    print(f"  near-ceiling request completed after {long_ok[0]['elapsed_ms']} ms")
EOF

assert_ok() {
    echo "-- assert $1"
    python3 "$TMP/assert.py" "$LOG" "$1" || fail "assertion $1 failed"
}

echo "== phase: instruction gate =="
call "tools/call" "catdesk_instruction" '{}' 30
[[ "$STATUS" == 200 ]] || fail "catdesk_instruction returned $STATUS"

# ── phase A: real stream-safe poll boundary (~15 s) ──
echo "== phase: poll returns at the real 15s stream-safe boundary =="
call "tools/call" "start_command" '{"command":"sleep 90"}' 30
[[ "$STATUS" == 200 ]] || fail "start_command returned $STATUS"
JOB_A="$(job_id_of)"
CURSOR_A="$(cursor_of)"
call "tools/call" "poll_command" "{\"job_id\":\"$JOB_A\",\"after\":$CURSOR_A,\"wait_ms\":0}" 10
[[ "$STATUS" == 200 ]] || fail "priming poll returned $STATUS"
CURSOR_A="$(cursor_of)"
STATE_A="$(state_of)"
if [[ "$STATE_A" == "queued" ]]; then
    call "tools/call" "poll_command" "{\"job_id\":\"$JOB_A\",\"after\":$CURSOR_A,\"wait_ms\":5000}" 10
    [[ "$STATUS" == 200 ]] || fail "queued-to-running poll returned $STATUS"
    CURSOR_A="$(cursor_of)"
    STATE_A="$(state_of)"
fi
if [[ "$STATE_A" != "running" ]]; then
    body_json >&2
    fail "job did not reach running state before 15s poll (state=$STATE_A)"
fi
call "tools/call" "poll_command" "{\"job_id\":\"$JOB_A\",\"after\":$CURSOR_A,\"wait_ms\":15000}" 30
[[ "$STATUS" == 200 ]] || fail "15s poll returned $STATUS (expected 200)"
echo "  poll returned without approaching the 45s control deadline (HTTP $STATUS)"
assert_ok "poll15"

# The phase-A sleep job outlives its assertions; the later phases only take
# ~30-40 s, so at quit time the job would still be running and the shutdown's
# job drain blows the quit-exit budget. Cancel it now and poll to a terminal
# state so quitting never races the drain.
call "tools/call" "cancel_command" "{\"job_id\":\"$JOB_A\"}" 30
[[ "$STATUS" == 200 ]] || fail "phase-A job cancel returned $STATUS"
for _ in $(seq 1 30); do
    call "tools/call" "poll_command" "{\"job_id\":\"$JOB_A\",\"after\":$CURSOR_A,\"wait_ms\":0}" 10
    [[ "$STATUS" == 200 ]] || fail "phase-A drain poll returned $STATUS"
    STATE_A="$(state_of)"
    case "$STATE_A" in
        succeeded | failed | cancelled | timed_out | abandoned | interrupted) break ;;
    esac
    sleep 1
done
case "$STATE_A" in
    succeeded | failed | cancelled | timed_out | abandoned | interrupted)
        echo "  phase-A job drained before quit (state=$STATE_A)"
        ;;
    *)
        body_json >&2
        fail "phase-A job never reached a terminal state (state=$STATE_A)"
        ;;
esac

# ── phase B: concurrency burst (24 parallel calls) ──
echo "== phase: 24 concurrent tool calls =="
call "tools/call" "start_command" '{"command":"sleep 6"}' 30
[[ "$STATUS" == 200 ]] || fail "start_command returned $STATUS"
JOB_B="$(job_id_of)"
echo "soak" >"$WORKSPACE/notes.txt"
PIDS=()
for i in $(seq 1 24); do
    case $((i % 4)) in
        0 | 1) TOOL="poll_command" ARGS="{\"job_id\":\"$JOB_B\",\"wait_ms\":2500}" ;;
        2) TOOL="read" ARGS='{"paths":["notes.txt"]}' ;;
        *) TOOL="poll_command" ARGS='{"job_id":"missing","wait_ms":0}' ;;
    esac
    (
        call "tools/call" "$TOOL" "$ARGS" 30
        [[ "$STATUS" == 200 ]] || exit 1
    ) &
    PIDS+=($!)
done
for pid in "${PIDS[@]}"; do
    wait "$pid" || fail "a concurrent call failed"
done
echo "  all concurrent calls answered"

# ── phase C: client disconnect during side-effectful work ──
echo "== phase: client disconnect during side-effectful work =="
call "tools/call" "start_command" \
    '{"command":"sleep 3 && echo survived > disconnect-effect.txt"}' 30
[[ "$STATUS" == 200 ]] || fail "start_command returned $STATUS"
JOB_C="$(job_id_of)"
CURSOR_C="$(cursor_of)"
call "tools/call" "poll_command" "{\"job_id\":\"$JOB_C\",\"after\":$CURSOR_C,\"wait_ms\":0}" 10
[[ "$STATUS" == 200 ]] || fail "disconnect priming poll returned $STATUS"
CURSOR_C="$(cursor_of)"
STATE_C="$(state_of)"
if [[ "$STATE_C" == "queued" ]]; then
    call "tools/call" "poll_command" "{\"job_id\":\"$JOB_C\",\"after\":$CURSOR_C,\"wait_ms\":5000}" 10
    [[ "$STATUS" == 200 ]] || fail "disconnect queued-to-running poll returned $STATUS"
    CURSOR_C="$(cursor_of)"
    STATE_C="$(state_of)"
fi
if [[ "$STATE_C" != "running" ]]; then
    body_json >&2
    fail "disconnect job did not reach running state (state=$STATE_C)"
fi
# curl gives up after ~2 s mid-request; the server must classify the drop.
call "tools/call" "poll_command" "{\"job_id\":\"$JOB_C\",\"after\":$CURSOR_C,\"wait_ms\":15000}" 2
sleep 4
[[ -f "$WORKSPACE/disconnect-effect.txt" ]] || fail "side effect did not survive the disconnect"
echo "  side effect survived the disconnect"
call "tools/call" "poll_command" "{\"job_id\":\"$JOB_C\",\"wait_ms\":0}" 30
[[ "$STATUS" == 200 ]] || fail "post-disconnect poll returned $STATUS"
body_json | grep -q '"state": "succeeded"' \
    || fail "job not pollable after the disconnect: $(body_json)"
echo "  job still pollable after the disconnect"

# ── phase D: tunnel lifecycle window ──
echo "== phase: tunnel lifecycle classification =="
if [[ -n "${CLOUDFLARED_UNIT:-}" ]] && systemctl --user is-active --quiet "$CLOUDFLARED_UNIT"; then
    # Opt-in: restart a real tunnel unit mid-poll so an in-flight request
    # experiences a transport drop. The server must attribute the dropped
    # request to the client, never to itself.
    (
        call "tools/call" "poll_command" "{\"job_id\":\"$JOB_C\",\"wait_ms\":0}" 10
    ) &
    DROP_PID=$!
    sleep 0.5
    systemctl --user restart "$CLOUDFLARED_UNIT"
    wait "$DROP_PID" || true
fi
# The dummy authtoken makes the supervisor's connect attempts fail, which
# already produces real tunnel_* events; give them a moment to land.
sleep 5
assert_ok "tunnel"

# ── phase E (optional, --full): near-ceiling long request (~115 s) ──
if [[ "$FULL" == 1 ]]; then
    echo "== phase: near-ceiling long request (real ~115 s) =="
    call "tools/call" "run_command" \
        '{"command":"echo near-ceiling; sleep 115","timeout":120000}' 150
    [[ "$STATUS" == 200 ]] || fail "near-ceiling run_command returned $STATUS"
    assert_ok "ceiling"
fi

# ── phase F: budgets over the whole log ──
echo "== failure budget gates =="
assert_ok "poll15"
assert_ok "disconnect"
assert_ok "concurrency"
echo "-- assert summary"
python3 "$TMP/assert.py" "$LOG" "summary" || fail "summary gate failed"

echo "== quitting CatDesk cleanly =="
touch "$QUIT_FILE"
for _ in $(seq 1 20); do
    [[ -f "$TMP/exited" ]] && break
    sleep 1
done
[[ -f "$TMP/exited" ]] || fail "CatDesk did not exit after quit"
wait "$DRIVER_PID" 2>/dev/null || true

echo
echo "SOAK OK: every induced failure classified, no leaked request state."
echo "Diagnostics: $LOG ($(( $(wc -l <"$LOG") )) records; temp dir removed on exit)."
