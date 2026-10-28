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

    def test_read_usage_counts_full_ranged_and_failed_without_inventing_historical_data(self):
        rows = [
            {"event": "tool_result_bytes", "rpc_tool": "read", "class": "small", "raw_bytes": 500, "inline_bytes": 500},
            {"event": "read_usage", "full_requests": 3, "ranged_requests": 2,
             "full_files": 2, "ranged_files": 1, "failed_files": 1,
             "budget_cut_files": 1, "returned_bytes": 9000},
            {"event": "read_usage", "full_requests": 1, "ranged_requests": 0,
             "full_files": 1, "ranged_files": 0, "failed_files": 0,
             "returned_bytes": 120},
        ]
        result = summarize(rows)
        self.assertEqual(result["read_calls"], 1)
        self.assertEqual(result["read_usage_calls"], 2)
        self.assertEqual(result["full_requests"], 4)
        self.assertEqual(result["ranged_requests"], 2)
        self.assertEqual(result["full_files"], 3)
        self.assertEqual(result["ranged_files"], 1)
        self.assertEqual(result["failed_files"], 1)
        # Older records lack the counter; they contribute zero, not an error.
        self.assertEqual(result["budget_cut_files"], 1)
        self.assertEqual(result["returned_bytes"], 9120)

    def test_command_usage_counts_categories_and_programs_per_tool(self):
        rows = [
            {"event": "command_usage", "tool": "run_command", "category": "file_view", "program": "sed"},
            {"event": "command_usage", "tool": "run_command", "category": "file_view", "program": "cat"},
            {"event": "command_usage", "tool": "start_command", "category": "build_test", "program": "cargo"},
        ]
        result = summarize(rows)
        self.assertEqual(result["command_usage_calls"], 3)
        self.assertEqual(result["command_categories"],
                         {"run_command:file_view": 2, "start_command:build_test": 1})
        self.assertEqual(result["command_programs"]["run_command:sed"], 1)


if __name__ == "__main__":
    unittest.main()
