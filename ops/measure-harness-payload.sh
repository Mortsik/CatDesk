#!/usr/bin/env bash
# Long-session payload benchmark per harness profile (bead catdesk-ojt.10).
#
# Starts the real release binary headless (under a pty) against a throwaway
# HOME/workspace/port and drives the SAME workflow families as the ojt.9
# transcript gate (src/mcp/e2e_transcript_gate.rs) through the REAL HTTP
# transport — the exact JSON-RPC payloads a harness receives:
#
#   schema        tools/list footprint (per-tool bytes + envelope)
#   instruction   catdesk_instruction tool-result bytes (paid once/session)
#   (a) run_command with multi-MB stdout+stderr -> externalized, then
#       reconstructed through read_result (retrieval-call count measured)
#   (b) file workflow: write big file, needle search, full read-back with
#       byte-for-byte reconstruction
#   (d) failing command keeps its diagnostic inline and bounded
#   (e) repeated polling stays inside the inline budget
#
# For every step the driver records: serialized JSON-RPC result bytes (what
# a transcript pays for), the full HTTP body bytes (transport envelope),
# wall-clock seconds around the whole request, responseBudget.originalBytes
# and outputRef presence (externalization), and for every externalized
# result the read_result call count and retrieved bytes until EOF.
#
# Read-only profiles expose no command or write tools by construction
# (ToolMode::read_only), so their fixture degrades to the read/search subset
# over a file prepared directly in the workspace with identical bytes; the
# skipped families are recorded under "not_applicable" with the reason.
#
# Workflow (c) (browser/DevTools) runs as its own DevTools profile over the
# SAME real HTTP/MCP endpoint: a deterministic mock peer replaces
# chrome-devtools-mcp behind an npx PATH shim, and a stub "chromium"
# executable (answers the /json/version remote-debug probe only) satisfies
# browser detection — no real browser, no CDP, no network. The mock peer
# shares its byte-identical large-body generator with the ojt.9 gate
# (src/mcp/e2e_transcript_gate.rs), so gate and HTTP numbers are like-for-
# like. Provenance is explicit (mock peer, real transport) and a peer that
# fails to provision is an ERROR, not a skip.
#
# The mascot seed is pinned in the generated config so the catdesk_instruction
# widget payload (mascot cards) is deterministic across runs; without it the
# instruction result size drifts with rand::random::<u64>() (src/state.rs).
#
# Usage:
#   ops/measure-harness-payload.sh [--bin PATH] [--out DIR] [PROFILE...]
#
#   PROFILE   MultiTools, ReadOnly, DevTools (CatDesk tools/list + tools/call
#             under a headless pty; MultiTools/ReadOnly run computer mode,
#             DevTools runs mode=both with the mock peer provisioned).
#             Default: MultiTools + ReadOnly.
#   --bin     use an existing catdesk binary instead of cargo build --release.
#   --out     write the per-profile machine-readable summary JSON into DIR.
#
# Requirements: cargo, python3, curl. The only outside writes are cargo's
# own target dir.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SLUG="bench"
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
    PROFILES=(MultiTools ReadOnly)
fi
if [[ -z "$BIN" ]]; then
    echo "== building release binary =="
    (cd "$REPO_ROOT" && cargo build --release)
    BIN="$REPO_ROOT/target/release/catdesk"
fi

command -v python3 >/dev/null || { echo "python3 is required" >&2; exit 2; }
command -v curl >/dev/null || { echo "curl is required" >&2; exit 2; }

TMP="$(mktemp -d "${TMPDIR:-/tmp}/catdesk-bench.XXXXXX")"
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
    if [[ -n "${DRIVER_PID:-}" ]]; then
        kill "$DRIVER_PID" 2>/dev/null || true
    fi
    if [[ -n "${BENCH_KEEP_TMP:-}" ]]; then
        echo "bench: keeping $TMP (BENCH_KEEP_TMP set)" >&2
    else
        rm -rf "$TMP"
    fi
}
trap cleanup EXIT

cat >"$TMP/bench.py" <<'PYEOF'
"""Drive the ojt.9 gate fixture through CatDesk's real HTTP transport.

Wall clock wraps the whole POST; bytes are UTF-8, serialized exactly as the
transport serializes them (compact JSON, no spaces).
"""
import base64
import json
import os
import sys
import time
import urllib.error
import urllib.request

BASE = sys.argv[1]
SLUG = sys.argv[2]
OUT_PATH = sys.argv[3]
PROFILE = sys.argv[4]
WORKSPACE = sys.argv[5]
READ_ONLY = PROFILE == "ReadOnly"

MAX_RANGE_BYTES = 128 * 1024  # result_store DEFAULT_MAX_RANGE_BYTES
MAX_RECONSTRUCT_RANGES = 256  # same cap as the ojt.9 gate
INLINE_BUDGET_BYTES = 64 * 1024  # response_budget DEFAULT_INLINE_RESPONSE_BYTES

# Large DevTools network body — byte-identical generator with the ojt.9
# gate's fake peer (src/mcp/e2e_transcript_gate.rs) so gate rows and HTTP
# rows stay like-for-like. Keep the two in lockstep.
DEVTOOLS_BODY = "GATE-NET-HEAD\n" + ("0123456789abcdef" * 8192) + "\nGATE-NET-TAIL"


def compact(value):
    return json.dumps(value, separators=(",", ":"), ensure_ascii=False)


def utf8(text):
    return len(text.encode("utf-8"))


def post(params, rpc_method):
    """POST one modern-MCP request; return (parsed response, wall seconds,
    HTTP body bytes)."""
    body = compact({
        "jsonrpc": "2.0",
        "id": "bench",
        "method": rpc_method,
        "params": {
            **params,
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {},
            },
        },
    })
    headers = {
        "Content-Type": "application/json",
        "MCP-Protocol-Version": "2026-07-28",
        "Mcp-Method": rpc_method,
    }
    # tools/call and prompts/get must also carry Mcp-Name matching
    # params.name (src/server.rs validates the header against the body).
    target_name = params.get("name") if isinstance(params, dict) else None
    if rpc_method in ("tools/call", "prompts/get") and target_name:
        headers["Mcp-Name"] = target_name
    request = urllib.request.Request(
        f"{BASE}/{SLUG}/mcp",
        data=body.encode("utf-8"),
        headers=headers,
        method="POST",
    )
    started = time.perf_counter()
    try:
        with urllib.request.urlopen(request, timeout=600) as response:
            raw = response.read()
    except urllib.error.HTTPError as error:
        detail = error.read().decode("utf-8", "replace")
        raise RuntimeError(f"{rpc_method} returned HTTP {error.code}: {detail}") from error
    wall = time.perf_counter() - started
    payload = json.loads(raw)
    if payload.get("error") is not None:
        raise RuntimeError(f"{rpc_method} failed: {payload['error']}")
    return payload, wall, len(raw)


def call_tool(name, arguments):
    payload, wall, http_bytes = post(
        {"name": name, "arguments": arguments}, "tools/call"
    )
    result = payload["result"]
    return {
        "tool": name,
        "wall_seconds": round(wall, 4),
        "http_body_bytes": http_bytes,
        "result_bytes": utf8(compact(result)),
        "result": result,
    }


def require_externalized(step):
    node = step["result"].get("responseBudget") or {}
    output_ref = node.get("outputRef")
    original = node.get("originalBytes")
    if output_ref is None or original is None:
        raise RuntimeError(
            f"expected {step['tool']} to externalize; responseBudget={node!r}"
        )
    return str(output_ref), int(original)


def reconstruct(output_ref):
    """Pull the whole stored payload through read_result until EOF; return
    (retrieval_call_count, retrieved_bytes, wall_seconds_total, rebuilt)."""
    calls = 0
    wall_total = 0.0
    rebuilt = bytearray()
    offset = 0
    for _ in range(MAX_RECONSTRUCT_RANGES):
        step = call_tool(
            "read_result",
            {
                "result_id": output_ref,
                "offset": offset,
                "max_bytes": MAX_RANGE_BYTES,
            },
        )
        calls += 1
        wall_total += step["wall_seconds"]
        structured = step["result"].get("structuredContent")
        if structured is None:
            raise RuntimeError(
                f"read_result range missing structuredContent: {step['result']!r}"
            )
        rebuilt.extend(base64.b64decode(structured["dataBase64"]))
        offset = int(structured["nextOffset"])
        if structured.get("eof") is True:
            return calls, len(rebuilt), wall_total, bytes(rebuilt)
    raise RuntimeError(f"reconstruct exceeded {MAX_RECONSTRUCT_RANGES} read_result ranges")


report = {}

# ── schema: tools/list footprint ──────────────────────────────────────────
payload, wall, http_bytes = post({}, "tools/list")
tools = payload["result"]["tools"]
tool_rows = sorted(
    ((t.get("name", "?"), utf8(compact(t))) for t in tools),
    key=lambda row: row[1],
    reverse=True,
)
report["schema"] = {
    "tool_count": len(tools),
    "tool_objects_bytes": sum(size for _, size in tool_rows),
    "tools_array_bytes": utf8(compact(tools)),
    "result_envelope_bytes": utf8(compact(payload["result"]))
    - utf8(compact(tools)),
    "wall_seconds": round(wall, 4),
    "http_body_bytes": http_bytes,
    "per_tool": [
        {"name": name, "bytes": size, "tokens_chars_over_4": round(size / 4.0, 1)}
        for name, size in tool_rows
    ],
}

# ── instruction: catdesk_instruction (mandatory first call) ──────────────
step = call_tool("catdesk_instruction", {})
structured = step["result"].get("structuredContent", {})
instruction_text = structured.get("instructionText") or ""
report["instruction"] = {
    "text_bytes": utf8(instruction_text),
    "result_bytes": step["result_bytes"],
    "http_body_bytes": step["http_body_bytes"],
    "wall_seconds": step["wall_seconds"],
}

fixture = {}

NEEDLE = "GATE-NEEDLE-9f3ab2"
CONTENT = (
    "GATE-FILE-HEAD\n"
    + "".join(f"F {i:06d} - gate filler line\n" for i in range(17000))
    + f"{NEEDLE}: the needle lives here\n"
    "GATE-FILE-TAIL\n"
)

# Read-only profiles have no write/command tools; the file for the
# read/search subset is prepared directly in the workspace with bytes
# identical to the (b) write below.
if READ_ONLY:
    gate_dir = os.path.join(WORKSPACE, "gate")
    os.makedirs(gate_dir, exist_ok=True)
    with open(os.path.join(gate_dir, "big.txt"), "w") as handle:
        handle.write(CONTENT)


def workflow_large_command():
    command = (
        "printf 'GATE-STDOUT-HEAD\\n'; "
        "awk 'BEGIN { for (i = 0; i < 24000; i++) "
        "printf \"S %06d - gate filler line\\n\", i }'; "
        "printf 'GATE-STDOUT-TAIL\\n'; "
        "{ printf 'GATE-STDERR-HEAD\\n'; "
        "awk 'BEGIN { for (i = 0; i < 24000; i++) "
        "printf \"E %06d - gate filler line\\n\", i }'; "
        "printf 'GATE-STDERR-TAIL\\n'; } >&2"
    )
    step = call_tool("run_command", {"command": command})
    if step["result"].get("isError") is True:
        raise RuntimeError(f"(a) run_command failed: {step['result']!r}")
    output_ref, original = require_externalized(step)
    row = {
        "result_bytes": step["result_bytes"],
        "http_body_bytes": step["http_body_bytes"],
        "externalized": True,
        "original_bytes": original,
        "wall_seconds": step["wall_seconds"],
    }
    calls, retrieved, retrieval_wall, rebuilt = reconstruct(output_ref)
    stored = json.loads(rebuilt)
    assert stored["structuredContent"]["stdout"].startswith("GATE-STDOUT-HEAD\n")
    assert stored["structuredContent"]["stdout"].endswith("GATE-STDOUT-TAIL\n")
    assert stored["structuredContent"]["stderr"].startswith("GATE-STDERR-HEAD\n")
    assert stored["structuredContent"]["stderr"].endswith("GATE-STDERR-TAIL\n")
    row["retrieval"] = {
        "read_result_calls": calls,
        "retrieved_bytes": retrieved,
        "wall_seconds": round(retrieval_wall, 4),
    }
    return row



def workflow_write_big():
    write_step = call_tool(
        "write",
        {"path": "gate/big.txt", "content": CONTENT, "create_dirs": True},
    )
    if write_step["result"].get("isError") is True:
        raise RuntimeError(f"(b) write failed: {write_step['result']!r}")
    return {
        "result_bytes": write_step["result_bytes"],
        "http_body_bytes": write_step["http_body_bytes"],
        "externalized": False,
        "wall_seconds": write_step["wall_seconds"],
    }


def workflow_needle_search():
    search_step = call_tool("search", {"pattern": NEEDLE, "path": "gate"})
    search_text = compact(search_step["result"])
    if "big.txt" not in search_text or NEEDLE not in search_text:
        raise RuntimeError("(b) search did not locate the needle in big.txt")
    return {
        "result_bytes": search_step["result_bytes"],
        "http_body_bytes": search_step["http_body_bytes"],
        "externalized": False,
        "wall_seconds": search_step["wall_seconds"],
    }


def workflow_read_big():
    read_step = call_tool("read", {"paths": ["gate/big.txt"]})
    read_ref, read_original = require_externalized(read_step)
    row = {
        "result_bytes": read_step["result_bytes"],
        "http_body_bytes": read_step["http_body_bytes"],
        "externalized": True,
        "original_bytes": read_original,
        "wall_seconds": read_step["wall_seconds"],
    }
    calls, retrieved, retrieval_wall, rebuilt = reconstruct(read_ref)
    stored = json.loads(rebuilt)
    got = stored["structuredContent"]["files"][0]["text"]
    assert got == CONTENT, "(b) read content must reconstruct byte-for-byte"
    row["retrieval"] = {
        "read_result_calls": calls,
        "retrieved_bytes": retrieved,
        "wall_seconds": round(retrieval_wall, 4),
    }
    return row


def workflow_error_diagnostics():
    step = call_tool(
        "run_command", {"command": "echo gate-boom-diagnostic >&2; exit 7"}
    )
    structured = step["result"].get("structuredContent", {})
    if structured.get("exitCode") != 7:
        raise RuntimeError(f"(d) exit code lost: {structured!r}")
    if "gate-boom-diagnostic" not in compact(step["result"]):
        raise RuntimeError("(d) diagnostic text must stay visible")
    return {
        "result_bytes": step["result_bytes"],
        "http_body_bytes": step["http_body_bytes"],
        "externalized": False,
        "wall_seconds": step["wall_seconds"],
    }


def workflow_repeated_polling():
    start_step = call_tool("start_command", {"command": "printf gate-poll-done"})
    start_structured = start_step["result"].get("structuredContent", {})
    job_id = start_structured.get("jobId")
    cursor = start_structured.get("nextCursor")
    if job_id is None or cursor is None:
        raise RuntimeError(f"(e) start_command shape unexpected: {start_structured!r}")
    polls = 0
    max_poll_bytes = 0
    sum_poll_bytes = 0
    poll_wall = 0.0
    collected = ""
    terminal = None
    while terminal is None:
        polls += 1
        if polls > 500:
            raise RuntimeError("(e) job never reached a terminal state")
        poll_step = call_tool(
            "poll_command",
            {"job_id": job_id, "after": cursor, "wait_ms": 0},
        )
        poll_wall += poll_step["wall_seconds"]
        max_poll_bytes = max(max_poll_bytes, poll_step["result_bytes"])
        sum_poll_bytes += poll_step["result_bytes"]
        poll_structured = poll_step["result"].get("structuredContent", {})
        cursor = poll_structured.get("nextCursor")
        for event in poll_structured.get("events") or []:
            if event.get("stream") == "stdout":
                collected += event.get("text") or ""
        state = poll_structured.get("state")
        if state in ("succeeded", "failed"):
            terminal = state
    if collected != "gate-poll-done":
        raise RuntimeError(f"(e) job output incorrect: {collected!r}")
    return {
        "poll_count": polls,
        "terminal_state": terminal,
        "max_poll_result_bytes": max_poll_bytes,
        "sum_poll_result_bytes": sum_poll_bytes,
        "wall_seconds": round(poll_wall, 4),
    }


def require_devtools_provisioned():
    """The mock peer must be visible through the REAL catalog: a missing
    DevTools tool means the bridge/peer failed to provision, which is an
    error, not a skip (catdesk-ytk acceptance)."""
    names = {row["name"] for row in report["schema"]["per_tool"]}
    required = {"take_snapshot", "list_console_messages", "get_network_request"}
    missing = sorted(required - names)
    if missing:
        raise RuntimeError(
            "DevTools peer not provisioned (tools/list is missing "
            f"{missing}); the (c) fixture requires the mock peer behind the "
            "real HTTP transport — check the server log for bridge startup"
        )


def echoed_arguments(step):
    # Over HTTP the server moves the peer's content text into
    # structuredContent.text (transport decoration); the raw handler shape
    # (content[0].text, as the gate sees it) stays the fallback.
    structured = step["result"].get("structuredContent") or {}
    text = structured.get("text")
    if text is None:
        content = step["result"].get("content") or []
        if content and "text" in content[0]:
            text = content[0]["text"]
    if text is None:
        raise RuntimeError(
            f"{step['tool']} returned no echoable text: {step['result']!r}"
        )
    return json.loads(text)


def workflow_devtools_input_bounds():
    # Input-side bounding must reach the peer as exact arguments over the
    # real transport (same asserts as the ojt.9 gate).
    snapshot = call_tool("take_snapshot", {})
    snapshot_args = echoed_arguments(snapshot)
    assert snapshot_args["verbose"] is False, snapshot_args
    listing = call_tool("list_console_messages", {"pageIdx": 2})
    listing_args = echoed_arguments(listing)
    assert listing_args["pageSize"] == 100, listing_args
    assert listing_args["pageIdx"] == 2, listing_args
    return {
        "result_bytes": listing["result_bytes"],
        "http_body_bytes": listing["http_body_bytes"],
        "externalized": False,
        "wall_seconds": listing["wall_seconds"],
        "provenance": "mock-peer",
        "echoed_defaults": {
            "take_snapshot.verbose": snapshot_args["verbose"],
            "list_console_messages.pageSize": listing_args["pageSize"],
            "list_console_messages.pageIdx": listing_args["pageIdx"],
        },
    }


def stored_body_text(full):
    # The store persists the result as the handler produced it
    # (content[0].text, the shape the gate reconstructs); the HTTP-decorated
    # variant carries structuredContent.text instead — accept both.
    structured = full.get("structuredContent") or {}
    if isinstance(structured.get("text"), str):
        return structured["text"]
    content = full.get("content") or []
    if content and "text" in content[0]:
        return content[0]["text"]
    raise RuntimeError(f"stored result carries no body text: {sorted(full)}")


def workflow_devtools_network_body():
    network = call_tool("get_network_request", {"reqid": 41})
    output_ref, original = require_externalized(network)
    row = {
        "result_bytes": network["result_bytes"],
        "http_body_bytes": network["http_body_bytes"],
        "externalized": True,
        "original_bytes": original,
        "inline_budget_bytes": INLINE_BUDGET_BYTES,
        "outputRef": True,
        "wall_seconds": network["wall_seconds"],
        "provenance": "mock-peer",
    }
    calls, retrieved, retrieval_wall, rebuilt = reconstruct(output_ref)
    full = json.loads(rebuilt)
    body = stored_body_text(full)
    assert body == DEVTOOLS_BODY, "network body must reconstruct byte-for-byte"
    row["retrieval"] = {
        "read_result_calls": calls,
        "retrieved_bytes": retrieved,
        "wall_seconds": round(retrieval_wall, 4),
    }
    row["lossless_reconstruction"] = True
    return row


if PROFILE == "DevTools":
    require_devtools_provisioned()
    report["devtools_provenance"] = {
        "peer": "deterministic mock (npx PATH shim; large-body generator "
        "byte-identical with the ojt.9 gate's fake peer)",
        "transport": "real HTTP/MCP (same endpoint, result store, and "
        "response budget as every other row)",
        "browser": "stub chromium executable answering the /json/version "
        "remote-debug probe only — no real browser, no CDP, no network",
        "note": "figures qualify the CatDesk transport/budget path; they are "
        "NOT chrome-devtools-mcp or real-browser measurements, and no client "
        "token or live-browser latency may be inferred from them",
    }
    fixture["c_input_bounds"] = workflow_devtools_input_bounds()
    fixture["c_network_body"] = workflow_devtools_network_body()
    report["not_applicable"] = {
        "a_run_command": "measured in the MultiTools profile over the same "
        "transport; mode=both changes none of the local tool handlers",
        "b_write": "measured in the MultiTools profile over the same "
        "transport; mode=both changes none of the local tool handlers",
        "b_needle_search": "measured in the MultiTools profile over the same "
        "transport; mode=both changes none of the local tool handlers",
        "b_read_big": "measured in the MultiTools profile over the same "
        "transport; mode=both changes none of the local tool handlers",
        "d_error_diagnostics": "measured in the MultiTools profile over the "
        "same transport; mode=both changes none of the local tool handlers",
        "e_polling": "measured in the MultiTools profile over the same "
        "transport; mode=both changes none of the local tool handlers",
    }
elif READ_ONLY:
    fixture["b_needle_search"] = workflow_needle_search()
    fixture["b_read_big"] = workflow_read_big()
    report["not_applicable"] = {
        "a_run_command": "run_command is not part of the read-only tool set "
        "(ToolMode::read_only); the server answers isError 'Tool run_command "
        "is disabled in read-only mode'",
        "b_write": "write is not part of the read-only tool set; the file for "
        "read/search was prepared directly in the workspace with identical "
        "bytes",
        "d_error_diagnostics": "requires run_command (absent in read-only)",
        "e_polling": "requires start_command/poll_command (absent in read-only)",
    }
else:
    fixture["a_run_command"] = workflow_large_command()
    fixture["b_write"] = workflow_write_big()
    fixture["b_needle_search"] = workflow_needle_search()
    fixture["b_read_big"] = workflow_read_big()
    fixture["d_error_diagnostics"] = workflow_error_diagnostics()
    fixture["e_polling"] = workflow_repeated_polling()

report["fixture"] = fixture

# Aggregates the acceptance criteria ask for.
externalizing = [key for key in fixture if fixture[key].get("externalized")]
bounded = [
    key for key in fixture
    if "poll_count" not in fixture[key] and not fixture[key].get("externalized")
]
report["summary"] = {
    "inline_tool_result_bytes": sum(
        fixture[key]["result_bytes"] for key in bounded
    )
    + fixture.get("e_polling", {}).get("sum_poll_result_bytes", 0),
    "externalized_original_bytes": sum(
        fixture[key]["original_bytes"] for key in externalizing
    ),
    "externalized_inline_bytes": sum(
        fixture[key]["result_bytes"] for key in externalizing
    ),
    "retrieval_calls": sum(
        fixture[key].get("retrieval", {}).get("read_result_calls", 0)
        for key in externalizing
    ),
    "retrieved_bytes": sum(
        fixture[key].get("retrieval", {}).get("retrieved_bytes", 0)
        for key in externalizing
    ),
    "fixture_wall_seconds": round(
        sum(
            fixture[key]["wall_seconds"]
            + fixture[key].get("retrieval", {}).get("wall_seconds", 0)
            for key in fixture
        ),
        4,
    ),
    "instruction_bytes": report["instruction"]["text_bytes"],
    "schema_tool_objects_bytes": report["schema"]["tool_objects_bytes"],
}

json.dump(report, open(OUT_PATH, "w"), indent=2, ensure_ascii=False)

print(f"tools: {report['schema']['tool_count']}"
      f"  schema objects: {report['schema']['tool_objects_bytes']} B"
      f"  array: {report['schema']['tools_array_bytes']} B")
print(f"instruction: {report['instruction']['text_bytes']} B"
      f"  (result {report['instruction']['result_bytes']} B)")
if PROFILE == "DevTools":
    print("devtools provenance: mock peer behind the real HTTP transport "
          "(no real browser / CDP / network)")
for key, row in fixture.items():
    if "poll_count" in row:
        print(f"{key:<20} polls {row['poll_count']}  max {row['max_poll_result_bytes']} B"
              f"  sum {row['sum_poll_result_bytes']} B  wall {row['wall_seconds']:.3f}s")
        continue
    line = (f"{key:<20} result {row['result_bytes']:>7} B  http {row['http_body_bytes']:>7} B"
            f"  wall {row['wall_seconds']:.3f}s")
    if row.get("externalized"):
        line += f"  raw {row['original_bytes']} B"
    if "retrieval" in row:
        line += (f"  retrieval {row['retrieval']['read_result_calls']} calls /"
                 f" {row['retrieval']['retrieved_bytes']} B")
    print(line)
print(f"summary: {compact(report['summary'])}")
print("BENCH OK")
PYEOF

cat >"$TMP/drive.py" <<'EOF'
import fcntl, os, pty, select, struct, sys, termios, time

bin_path, quit_file, pty_log, exited = sys.argv[1:5]
# Startup mode-select key: "1" Computer (default), "3" Both (DevTools
# profile — the bridge only starts in a browser-enabled mode). The intro
# splash runs ~1.85s and eats one "any key" skip first; the mode key is
# sent after a short delay so it lands on the mode-select screen. Later
# "1" presses confirm the first supported browser in the picker and are
# harmless everywhere else.
mode_key = sys.argv[5] if len(sys.argv) > 5 else "1"
pid, fd = pty.fork()
if pid == 0:
    os.execv(bin_path, [bin_path])
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
print(pid, flush=True)
sent_mode_key = False
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
        if not sent_mode_key:
            try:
                os.write(fd, b"s")  # any key skips the intro splash
                time.sleep(0.7)
                os.write(fd, mode_key.encode())
                sent_mode_key = True
            except OSError:
                break
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

fail() {
    echo "BENCH FAIL: $*" >&2
    exit 1
}

for PROFILE in "${PROFILES[@]}"; do
    EXTRA_PATH=""
    case "$PROFILE" in
        MultiTools) TOOL_MODE_VALUE="multiTools"; MODE_VALUE="computer" ;;
        ReadOnly) TOOL_MODE_VALUE="readOnly"; MODE_VALUE="computer" ;;
        DevTools) TOOL_MODE_VALUE="multiTools"; MODE_VALUE="both" ;;
    esac
    HOME_DIR="$TMP/home-$PROFILE"
    WORKSPACE="$TMP/workspace-$PROFILE"
    mkdir -p "$HOME_DIR/.catdesk" "$WORKSPACE"

    if [[ "$PROFILE" == "DevTools" ]]; then
        # Deterministic mock-peer provisioning (catdesk-ytk): an npx PATH
        # shim stands in for "npx -y chrome-devtools-mcp@latest" and a stub
        # "chromium" satisfies browser detection plus the /json/version
        # remote-debug probe. No real browser, no CDP, no network. A peer
        # that still fails to provision is an ERROR in the driver
        # (require_devtools_provisioned), never a silent skip.
        SHIM_BIN="$TMP/devtools-shim"
        mkdir -p "$SHIM_BIN"
        cat >"$SHIM_BIN/peer.py" <<'PEEREOF'
"""Deterministic DevTools mock peer (catdesk-ytk).

Speaks the chrome-devtools-mcp stdio contract CatDesk's DevtoolsBridge
expects: line-delimited JSON-RPC. The large network body uses the SAME
byte-identical generator as the ojt.9 gate's fake peer
(src/mcp/e2e_transcript_gate.rs) — keep the two in lockstep.
"""
import json
import sys

BODY = "GATE-NET-HEAD\n" + ("0123456789abcdef" * 8192) + "\nGATE-NET-TAIL"

TOOLS = [
    {
        "name": "take_snapshot",
        "description": "mock peer: page snapshot; echoes arguments back",
        "inputSchema": {
            "type": "object",
            "properties": {"verbose": {"type": "boolean"}},
        },
        "annotations": {"readOnlyHint": True},
    },
    {
        "name": "list_console_messages",
        "description": "mock peer: console listing; echoes arguments back",
        "inputSchema": {
            "type": "object",
            "properties": {
                "pageIdx": {"type": "integer"},
                "pageSize": {"type": "integer"},
            },
        },
        "annotations": {"readOnlyHint": True},
    },
    {
        "name": "get_network_request",
        "description": "mock peer: network body by request id; deterministic large body",
        "inputSchema": {
            "type": "object",
            "properties": {"reqid": {}},
            "required": ["reqid"],
        },
        "annotations": {"readOnlyHint": True},
    },
]

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    if "id" not in req:
        continue  # notification (e.g. notifications/initialized): no reply
    method = req.get("method", "")
    if method == "initialize":
        result = {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "serverInfo": {"name": "catdesk-mock-devtools", "version": "1.0.0"},
        }
    elif method == "tools/list":
        result = {"tools": TOOLS}
    elif method == "tools/call":
        name = req.get("params", {}).get("name", "")
        args = req.get("params", {}).get("arguments", {})
        text = BODY if name == "get_network_request" else json.dumps(args, sort_keys=True)
        result = {"content": [{"type": "text", "text": text}]}
    else:
        print(json.dumps({
            "jsonrpc": "2.0",
            "id": req["id"],
            "error": {"code": -32601, "message": f"method not found: {method}"},
        }), flush=True)
        continue
    print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": result}), flush=True)
PEEREOF
        printf '#!/bin/sh\n# catdesk-ytk: deterministic npx shim — replaces the\n# "npx -y chrome-devtools-mcp@latest ..." network fetch with the local\n# mock peer. Arguments are deliberately ignored.\nexec python3 "%s/peer.py" "$@"\n' \
            "$SHIM_BIN" >"$SHIM_BIN/npx"
        cat >"$SHIM_BIN/chromium" <<'STUBEOF'
#!/usr/bin/env python3
"""Stub Chromium for the DevTools bench profile (catdesk-ytk).

CatDesk startup detects a Chromium binary through PATH and, with remote
debugging not yet active, launches it with --remote-debugging-port=N, then
probes http://127.0.0.1:N/json/version. This stub answers that probe (HTTP
200) and nothing else — no browser, no CDP, no network. Self-terminates so
a crashed run leaks nothing past the deadline.
"""
import json
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, HTTPServer

PORT = 9222
for arg in sys.argv[1:]:
    if arg.startswith("--remote-debugging-port="):
        PORT = int(arg.split("=", 1)[1])


class Probe(BaseHTTPRequestHandler):
    def do_GET(self):
        body = json.dumps({
            "Browser": "catdesk-bench-stub/1.0",
            "webSocketDebuggerUrl": "",
        }).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_args):
        pass


server = HTTPServer(("127.0.0.1", PORT), Probe)
threading.Thread(target=server.serve_forever, daemon=True).start()
time.sleep(900)
STUBEOF
        chmod +x "$SHIM_BIN/npx" "$SHIM_BIN/chromium"
        EXTRA_PATH="$SHIM_BIN"
    fi

    PORT="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')"
    BASE="http://127.0.0.1:$PORT"
    cat >"$HOME_DIR/.catdesk/config.toml" <<EOF
mcpSlug = "$SLUG"
ngrokAuthtoken = "bench-dummy-never-used"
ngrokDomain = "bench.invalid"
chatgptConnectorRevision = 999999
partnerBinagotchySeed = "00000000000000ff"
theme = "concise"
mode = "$MODE_VALUE"
toolMode = "$TOOL_MODE_VALUE"
EOF

    if [[ "$PROFILE" == "DevTools" ]]; then
        # catdesk-ytk: headless startup has no TUI to pick a browser, so pin
        # the stub as the selected browser (config field shape mirrors
        # DetectedBrowser in src/browser.rs).
        cat >>"$HOME_DIR/.catdesk/config.toml" <<EOF

[selectedBrowser]
name = "Chromium"
binary = "chromium"
path = "$SHIM_BIN/chromium"
remote_debugging = true
remote_debug_hint = "--remote-debugging-port=<port>"
mcp_supported = true
support_note = "Chromium (supported)"
remote_debug_active = false
EOF
    fi

    echo "== profile $PROFILE: starting CatDesk under a pty (port $PORT) =="
    if [[ -n "$EXTRA_PATH" ]]; then
        # The server must resolve the shimmed npx (DevTools bridge) and the
        # stub chromium (browser detection) BEFORE system PATH entries.
        export PATH="$EXTRA_PATH:$PATH"
    fi
    export HOME="$HOME_DIR"
    export WORKSPACE_ROOT="$WORKSPACE"
    export PORT
    export TERM="${TERM:-xterm-256color}"
    MODE_KEY=1
    [[ "$PROFILE" == "DevTools" ]] && MODE_KEY=3
    python3 "$TMP/drive.py" "$BIN" "$QUIT_FILE" "$PTQ_LOG" "$EXITED_FILE" "$MODE_KEY" \
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

    echo "== profile $PROFILE: driving the gate fixture over HTTP =="
    SUMMARY="$TMP/bench-$PROFILE.json"
    python3 "$TMP/bench.py" "$BASE" "$SLUG" "$SUMMARY" "$PROFILE" "$WORKSPACE" \
        || {
            tail -c 3000 "$PTQ_LOG" || true
            fail "fixture run failed (profile $PROFILE)"
        }

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
    rm -f "$QUIT_FILE" "$EXITED_FILE"

    if [[ -n "$OUT" ]]; then
        mkdir -p "$OUT"
        cp "$SUMMARY" "$OUT/bench-$PROFILE.json"
    fi
    echo
done

echo "BENCH OK"
