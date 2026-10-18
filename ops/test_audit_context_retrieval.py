"""Regression tests for the aggregate tool-result recovery audit."""
import unittest

from audit_context_retrieval import summarize


class AuditContextRetrievalTests(unittest.TestCase):
    def test_counts_externalized_and_recovery_calls_without_claiming_one_to_one(self):
        rows = [
            {"event": "tool_result_bytes", "timestamp_ms": 1_760_000_000_000,
             "rpc_tool": "read", "class": "externalized", "raw_bytes": 300_000,
             "inline_bytes": 4_096},
            {"event": "tool_result_bytes", "timestamp_ms": 1_760_000_000_001,
             "rpc_tool": "read", "class": "small", "raw_bytes": 32,
             "inline_bytes": 32},
            {"event": "tool_result_bytes", "timestamp_ms": 1_760_000_000_002,
             "rpc_tool": "read_result", "class": "small", "raw_bytes": 120_000,
             "inline_bytes": 120_000},
            {"event": "tool_result_bytes", "timestamp_ms": 1_760_000_000_003,
             "rpc_tool": "read_result", "class": "small", "raw_bytes": 120_000,
             "inline_bytes": 120_000},
            {"event": "mcp_request", "timestamp_ms": 1_760_000_000_004, "rpc_tool": "read_result"},
        ]
        total = summarize(rows)
        self.assertEqual(total["read_calls"], 2)
        self.assertEqual(total["read_externalized"], 1)
        self.assertEqual(total["retrieval_calls"], 2)
        self.assertEqual(total["externalized_raw_bytes"], 300_000)
        self.assertEqual(total["externalized_inline_bytes"], 4_096)


if __name__ == "__main__":
    unittest.main()
