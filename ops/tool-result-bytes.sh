#!/usr/bin/env bash
# Per-tool inline-byte reduction report from connections.jsonl captures
# (bead catdesk-ojt.6).
#
# For every tools/call result the server emits one numeric `tool_result_bytes`
# record into diagnostics (src/tool_result_metrics.rs): whitelisted rpc_tool,
# response class (small | compacted | externalized), and raw / inline /
# externalized byte totals. Records are aggregates only — payload text is
# never written to the log, so this report is safe to share.
#
# Byte semantics per class:
#   small        raw == inline (sent byte-for-byte)
#   compacted    inline < raw  (bytes dropped in-band, nothing stored)
#   externalized inline is the bounded preview, raw/externalized are the
#                full payload parked in the large-result store
#
# Usage:
#   ops/tool-result-bytes.sh capture.jsonl
#       Per-tool totals from one capture: call counts, class breakdown and
#       how much of the raw payload stayed off the wire per tool.
#
#   ops/tool-result-bytes.sh before.jsonl after.jsonl
#       Before/after comparison (e.g. a capture from a build without inline
#       compaction vs the current build): per-tool inline totals, the saved
#       bytes and the reduction percentage.
#
#   Add --json (before the files) for machine-readable output.
#
# Captures are the diagnostics log of a run (default
# ~/.catdesk/logs/connections.jsonl, including rotated .1/.2 files); any
# JSONL file containing tool_result_bytes records works. The script never
# talks to the MCP server and drives no traffic: produce captures by
# exercising the server however you like, then report offline.
#
# Requirements: python3.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

usage() {
    echo "usage: ops/tool-result-bytes.sh [--json] capture.jsonl [before.jsonl after.jsonl]" >&2
    echo "       (exactly one capture, or a before/after pair)" >&2
}

MODE="text"
POSITIONAL=()
for arg in "$@"; do
    case "$arg" in
        --json) MODE="json" ;;
        -h|--help) usage; exit 0 ;;
        -*) usage; exit 2 ;;
        *) POSITIONAL+=("$arg") ;;
    esac
done

if [[ ${#POSITIONAL[@]} -lt 1 || ${#POSITIONAL[@]} -gt 2 ]]; then
    usage
    exit 2
fi
for path in "${POSITIONAL[@]}"; do
    [[ -f "$path" ]] || { echo "not a file: $path" >&2; exit 2; }
done

python3 - "$MODE" "${POSITIONAL[@]}" <<'PY'
import json
import sys

FIELDS = ("calls", "errors", "small", "compacted", "externalized",
          "raw_bytes", "inline_bytes", "externalized_bytes")


def aggregate(path):
    tools = {}

    def row(tool):
        return tools.setdefault(tool, dict.fromkeys(FIELDS, 0))

    with open(path, "r", encoding="utf-8") as handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            try:
                record = json.loads(line)
            except ValueError:
                continue  # torn line from log rotation
            if record.get("event") != "tool_result_bytes":
                continue
            entry = row(record.get("rpc_tool") or "other")
            entry["calls"] += 1
            if record.get("is_error"):
                entry["errors"] += 1
            response_class = record.get("class")
            if response_class in ("small", "compacted", "externalized"):
                entry[response_class] += 1
            entry["raw_bytes"] += int(record.get("raw_bytes") or 0)
            entry["inline_bytes"] += int(record.get("inline_bytes") or 0)
            entry["externalized_bytes"] += int(record.get("externalized_bytes") or 0)
    return tools


def total(tools):
    sums = dict.fromkeys(FIELDS, 0)
    for entry in tools.values():
        for field in FIELDS:
            sums[field] += entry[field]
    return sums


def saved_bytes(entry):
    return max(entry["raw_bytes"] - entry["inline_bytes"], 0)


def saved_ratio(entry):
    if entry["raw_bytes"] == 0:
        return None
    return saved_bytes(entry) * 100.0 / entry["raw_bytes"]


def fmt_ratio(ratio):
    return "—" if ratio is None else f"{ratio:.1f}%"


def render_text(tools, before_tools=None):
    lines = []
    names = sorted(tools)
    if before_tools is None:
        lines.append("== tool_result_bytes totals by tool ==")
        header = (f"{'tool':<20}{'calls':>6}{'errs':>6}{'small':>7}{'compc':>7}"
                  f"{'ext':>6}{'raw B':>14}{'inline B':>14}{'saved':>8}")
        lines.append(header)
        for name in names:
            entry = tools[name]
            lines.append(
                f"{name:<20}{entry['calls']:>6}{entry['errors']:>6}{entry['small']:>7}"
                f"{entry['compacted']:>7}{entry['externalized']:>6}"
                f"{entry['raw_bytes']:>14}{entry['inline_bytes']:>14}"
                f"{fmt_ratio(saved_ratio(entry)):>8}"
            )
        sums = total(tools)
        lines.append(
            f"{'TOTAL':<20}{sums['calls']:>6}{sums['errors']:>6}{sums['small']:>7}"
            f"{sums['compacted']:>7}{sums['externalized']:>6}"
            f"{sums['raw_bytes']:>14}{sums['inline_bytes']:>14}"
            f"{fmt_ratio(saved_ratio(sums)):>8}"
        )
        return "\n".join(lines)

    lines.append("== inline-byte reduction by tool (before -> after) ==")
    header = (f"{'tool':<20}{'inline before':>15}{'inline after':>15}"
              f"{'saved B':>12}{'saved %':>9}")
    lines.append(header)
    names = sorted(set(before_tools) | set(tools))
    for name in names:
        before = before_tools.get(name, dict.fromkeys(FIELDS, 0))
        after = tools.get(name, dict.fromkeys(FIELDS, 0))
        saved = before["inline_bytes"] - after["inline_bytes"]
        ratio = (saved * 100.0 / before["inline_bytes"]
                 if before["inline_bytes"] else None)
        lines.append(
            f"{name:<20}{before['inline_bytes']:>15}{after['inline_bytes']:>15}"
            f"{saved:>12}{fmt_ratio(ratio):>9}"
        )
    before_sums = total(before_tools)
    after_sums = total(tools)
    saved = before_sums["inline_bytes"] - after_sums["inline_bytes"]
    ratio = (saved * 100.0 / before_sums["inline_bytes"]
             if before_sums["inline_bytes"] else None)
    lines.append(
        f"{'TOTAL':<20}{before_sums['inline_bytes']:>15}{after_sums['inline_bytes']:>15}"
        f"{saved:>12}{fmt_ratio(ratio):>9}"
    )
    return "\n".join(lines)


mode = sys.argv[1]
paths = sys.argv[2:]
tools = aggregate(paths[-1])
if mode == "json":
    payload = {"capture": paths[-1], "tools": tools, "total": total(tools)}
    if len(paths) == 2:
        payload["before"] = {"capture": paths[0], "tools": aggregate(paths[0])}
    print(json.dumps(payload, indent=2, sort_keys=True))
elif len(paths) == 2:
    print(render_text(tools, before_tools=aggregate(paths[0])))
else:
    if not tools:
        print(f"no tool_result_bytes records found in {paths[0]}; "
              "see the script header for how captures are produced",
              file=sys.stderr)
        sys.exit(1)
    print(render_text(tools))
PY
