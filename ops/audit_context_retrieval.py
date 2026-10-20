#!/usr/bin/env python3
"""Summarize CatDesk preview/retrieval usage without logging file contents.

The diagnostics stream carries counts, not the opaque outputRef on each call.
Retrieval calls may read multiple chunks of one result, so this is an
aggregate observability signal, NOT proof that a particular file was read.
"""
import argparse
import glob
import json
from pathlib import Path


def summarize(records):
    report = {
        "read_calls": 0,
        "read_externalized": 0,
        "externalized_calls": 0,
        "externalized_raw_bytes": 0,
        "externalized_inline_bytes": 0,
        "retrieval_calls": 0,
        "retrieval_response_bytes": 0,
        "search_result_calls": 0,
        "read_usage_calls": 0,
        "full_requests": 0,
        "ranged_requests": 0,
        "full_files": 0,
        "ranged_files": 0,
        "failed_files": 0,
        "returned_bytes": 0,
    }
    for row in records:
        if row.get("event") == "read_usage":
            report["read_usage_calls"] += 1
            for field in ("full_requests", "ranged_requests", "full_files",
                          "ranged_files", "failed_files", "returned_bytes"):
                report[field] += row.get(field, 0)
            continue
        if row.get("event") != "tool_result_bytes":
            continue
        tool = row.get("rpc_tool")
        if tool == "read":
            report["read_calls"] += 1
        if tool == "read_result":
            report["retrieval_calls"] += 1
            report["retrieval_response_bytes"] += row.get("raw_bytes", 0)
        if tool == "search_result":
            report["search_result_calls"] += 1
        if row.get("class") == "externalized":
            report["externalized_calls"] += 1
            report["externalized_raw_bytes"] += row.get("raw_bytes", 0)
            report["externalized_inline_bytes"] += row.get("inline_bytes", 0)
            if tool == "read":
                report["read_externalized"] += 1
    return report


def read_logs(paths):
    for pattern in paths:
        for filename in sorted(glob.glob(pattern)):
            try:
                with open(filename, encoding="utf-8") as stream:
                    for line in stream:
                        try:
                            yield json.loads(line)
                        except json.JSONDecodeError:
                            continue
            except OSError as error:
                print(f"Cannot read {filename}: {error}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("logs", nargs="*", default=[
        str(Path.home() / ".catdesk/logs/connections*.jsonl")
    ], help="Diagnostics JSONL paths or glob patterns")
    args = parser.parse_args()
    print(json.dumps(summarize(read_logs(args.logs)), indent=2))
    print("Full/ranged input counters include duplicate requested paths; output "
          "counters exclude deduplicated entries. Logs before the read_usage "
          "event was introduced cannot classify historical read calls.")
    print("Retrieval calls count chunks, not recovered files/results. "
          "Correlating individual outputRef values requires additional tracking.")


if __name__ == "__main__":
    main()
