# Env-lock audit: residual PATH-spawning tests without the lock (2026-10-08)

## Problem

The dr6 sweep (2026-10-07-env-serialization-audit.md) closed the class of
PATH-racing test spawns, but two holes remained open:

1. **Tests added after the sweep** inherit none of its coverage; the reviewer
   stress-run surfaced one directly:
   `mcp::tests::oversized_run_command_with_both_streams_full_is_reduced_not_sent_inline`
   (spawns `printf/yes/head/tr` through `sh`/`PATH`) — a broken spawn yields a
   small error result that is never externalized, so the `outputRef`
   assertions lose their subject. The transcript gate needed the same lock
   post-dr6 (88393dd).
2. **Direct-helper flows the dr6 marker lock missed**: dr6's server.rs
   coverage was marker-based (test bodies naming a command tool). A test that
   calls `CommandJobManager::start*` directly — without `run_command`/
   `start_command` appearing in its body — was never locked
   (`deleting_one_named_session_cancels_only_its_jobs…`, exactly the
   "known compromise 3" the dr6 audit predicted). `command.rs` was absent
   from the dr6 sweep's file list entirely.

## Rule (unchanged from dr6)

Every test whose flow can reach a child-process spawn takes the env lock as
its first statement (`crate::test_serialization::lock_env()`, or the file's
`env_lock()` helper where one exists). A test that only validates
arguments/schemas/payload shapes, or whose spawn-shaped strings are literals
never dispatched, does not need it. When in doubt, take the lock.

## Sweep method

Two automated passes over all 647 test functions in the 40 test-bearing
files: brace-matched test bodies, then marker matching (`Command::new`,
`sh`/`/bin/sh`, `python`, `"git"`, `grep`/`rg`, job starts — including
`start_with_change_session`, which the naive `\.start\(` pattern misses —
`.spawn()/.output()/.status()`, `peer()`, `run_git`, command-tool dispatches
through `handle_tools_call`/`post_tools_call`) crossed with lock presence
(`lock_env()`/`env_lock()`/`EnvGuards`). Every unlocked hit was triaged by
reading the body: live-spawn subject → lock; otherwise → documented no-op
below. The fixed sweep (after adding the `start_with` pattern) is the
regression check: it reports only the no-op list.

## Locked by this audit (11)

| File:line | Test | Spawn | Why the subject dies on a broken spawn |
|---|---|---|---|
| `mcp/tests.rs:6562` | `oversized_run_command_with_both_streams_full_is_reduced_not_sent_inline` | `printf`/`yes`/`head`/`tr` via `sh` (`run_command`) | small error result is never externalized → `outputRef`/sentinel assertions lose their subject (the reported residual) |
| `mcp/tests.rs:6854` | `sub_budget_entry_cap_reduction_discloses_loss_inline_without_a_manifest` | `printf`/`head`/`tr` via `sh` (`run_command`) | result lands below the cap → `entryCapTruncated`/`entryCapOriginalBytes` assertions lose their subject |
| `diagnostics.rs:1145` | `client_disconnection_is_recorded_as_cancelled_with_a_reason` | `sleep 3` via `start_command` over live HTTP | job fails before the poll can block → no mid-request disconnect → `http_cancelled` assertions lose their subject (sibling `concurrent_scheduled_requests…` was already locked) |
| `diagnostics.rs:1407` | `deadline_timeout_is_recorded_through_real_http` | `sleep 2` via `run_command` over live HTTP | fast failure → `504`/`elapsed_ms >= 1400`/`scheduler_deadline_stage == "execution"` lose their subject |
| `command_jobs.rs:1294` | `recovery_marks_running_records_interrupted_and_rewrites_them` | `sleep 30` | job never reaches `Running` → `Interrupted` recovery assertions lose their subject |
| `command_jobs.rs:1451` | `background_change_report_is_deferred_and_cached_until_terminal` | `sleep 0.5` | job terminal immediately → "running jobs defer change scans" loses its subject |
| `command_jobs.rs:1955` | `cancel_session_signals_only_jobs_owned_by_that_session` | `sleep 5` ×2 | both jobs fail before the cancel → `cancel_session == 1` loses its subject |
| `server.rs:3452` | `deleting_one_named_session_cancels_only_its_jobs_and_keeps_other_session_connected` | `sleep 5` ×2 via `CommandJobManager::start_with_change_session` | direct-manager flow with no command-tool name in the body — the dr6 marker lock skipped it; broken spawn fails the only-session-a-cancelled assertions |
| `command.rs:1114` | `run_command_uses_platform_shell_and_cwd` | platform shell + `basename` | `success`/`stdout == leaf` lose their subject |
| `command.rs:1143` | `run_command_preserves_both_ends_of_stdout_larger_than_one_mibibyte` | `printf`/`head`/`tr` via `sh` | `success`/both-ends/`stdout_truncated == false` lose their subject |
| `command.rs:1176` | `run_command_preserves_both_ends_of_stderr_larger_than_one_mibibyte` | `printf`/`head`/`tr` via `sh` | same on the stderr stream |

Lock-only changes (the dr6 invariant): no assertion touched, no fixture
rewritten. `command.rs` was outside the dr6 file list — it was swept this
time because the acceptance criterion is repo-wide, and its three capture
tests are live `run_command` dispatches.

## Audited no-ops (unlocked, no live-spawn subject)

| File:line | Test | Spawn-shaped marker | Why it is a no-op |
|---|---|---|---|
| `mcp/tests.rs:1009` | `read_only_mode_blocks_all_command_job_calls_even_if_invoked_directly` | `start_command`/`poll_command`/`cancel_command` requests | `ToolMode::ReadOnly` rejects before any dispatch; `echo blocked` never spawns |
| `mcp/tests.rs:1268` | `run_command_rejects_long_timeout_and_points_to_start_command` | `run_command` request | validation rejects `timeout > MAX` before dispatch; `echo short` never spawns |
| `mcp/tests.rs:1941` | `local_tools_list_exposes_output_schemas_except_multimodal_read_image` | `run_command` in schema table | static tool-catalog schema assertions, no call flow |
| `mcp/tests.rs:3173` | `catdesk_instruction_steers_long_commands_to_start_and_poll` | `start_command`/`run_command` | instruction *text* literal assertions |
| `mcp/tests.rs:6689` | `entry_cap_reduction_bounds_pathological_escaping_and_keeps_ends` | `run_command` in `toolName` field | pure `reduce_result_to_entry_cap` unit on a `json!` literal |
| `mcp/tests.rs:4846` | `poll_command_rejects_wait_above_stream_safe_ceiling` | `poll_command` request | argument validation rejects before any job lookup |
| `diagnostics.rs:38` | `request_metadata_does_not_persist_client_secrets` | `start_command` name string | pure `request_metadata` mapping on `json!` inputs |
| `diagnostics.rs:114` | `request_metadata_records_only_safe_requested_timing_values` | `run_command`/`poll_command` names | same |
| `diagnostics.rs:159` | `tool_result_bytes_records_numbers_only_aggregates` | `"run_command"` slot name | synthetic diagnostics channel; no process flow |
| `diagnostics.rs:229` | `tool_result_bytes_survive_blocking_pool_dispatch` | `"run_command"` slot name | same (blocking-pool scope plumbing, no spawn) |
| `diagnostics.rs:281` | `tool_result_bytes_from_an_unscoped_blocking_future_stay_silent` | `"run_command"` slot name | same |
| `server.rs:1564` | `tool_flow_label_includes_selected_argument_summary` | `run_command` label fixture | pure `request_flow_label` on `json!` |
| `server.rs:1615` | `request_classification_keeps_control_plane_separate_from_heavy_work` | `run_command`/`start_command` bodies | pure `request_class` classification, no handler |
| `server.rs:1798` | `detailed_mcp_log_summaries_include_bootstrap_context` | `toolName=run_command` URI literal | log-summary formatting |
| `server.rs:1964`, `:2014`, `:2149`, `:2439`, `:2489`, `:2640`, `:2741`, `:3245`, `:3353`, `:3622`, `:3671`, `:4388`, `:4436` | `post_mcp_*`, `ping_*`, `initialize_*`, `public_routes_*`, `instruction_*`, `openai_*`, `authoritative_*` | `.status()` on HTTP responses | instruction/read/ping/widget flows only — no command tool dispatched (dr6 no-command-path class, re-verified per body) |
| `command_jobs.rs` (validation-only set, dr6) | `*_rejects_*` argument tests | — | reject before `.start*` (unchanged from dr6) |
| `process_runner.rs:1042` | `spawn_shell_command_denies_wsl_shutdown_before_spawn` | `spawn_shell_command` | denied before spawn (dr6) |
| `workspace_tools.rs:2065` | `named_backend_probe_is_cached_once` | `"rg"` | `cached_command_available` with a stub closure — OnceLock caching logic, no real probe |
| `linux_sandbox.rs:2135` | `bubblewrap_executable_skips_workspace_symlink_and_uses_later_candidate` | `#!/bin/sh` file content | pure candidate-path scan over explicit dirs; nothing executed, no env read |
| `linux_sandbox.rs` (`runtime_read_paths_*`, `sandbox_*`, `helper_*`, `bubblewrap_argv_*`, `workspace_git_paths_*`) | — | `EnvGuards` | already hold the lock via `EnvGuards::set_many` → `env_lock()` (dr6 idiom) |
| `state.rs:2365`, `:2893`, `:2947`, `:3098` | bootstrap/flow records | `"run_command"`/`"start_command"` strings | widget-name/flow-label fixtures, in-process |
| `tests.rs:458`, `:531`, `:586`, `:767` | timeout-streak/dashboard renders | `"run_command"` in streak fixtures | synthetic `ToolTimeoutStreak` snapshots + render assertions |
| `tool_result_metrics.rs:215` | `observe_accumulates_per_tool_class_byte_and_error_totals` | `"run_command"` slot | synthetic `observe` calls |
| `mcp/response_budget.rs:682`, `:736`, `:854`, `:1051`, `:1100`, `:1145` | budget/store/entry-cap tests | `"toolName": "run_command"` | `json!` literals through in-process budget/store plumbing — file not previously swept; no dispatch on any path |

Files with tests but no spawn potential at all (search, git, sandbox, soak,
handoff, devtools, e2e gate) matched dr6's coverage: every live-spawn test in
them already holds the lock (`EnvGuards`, per-file `env_lock()` helpers, or
the 88393dd gate lock).

## Regression check

The sweep script (brace-matched bodies × markers × lock presence) is the
permanent tripwire: on the post-fix tree it reports only the no-op rows
above. A new spawn-dependent test without the lock reappears in its output
on the first run.

## Verification

5× full `cargo test -j 8` (656 tests) under parallel compile load — a fresh
scratch build of the same tree cycling `cargo clean && cargo build -j 8` in
the background throughout (load average ~34 on 32 cores). All five runs
green, `0 failed`:

| Run | Result | Wall |
|---|---|---|
| 1 | ok. 656 passed; 0 failed | 41.72 s |
| 2 | ok. 656 passed; 0 failed | 44.99 s |
| 3 | ok. 656 passed; 0 failed | 41.48 s |
| 4 | ok. 656 passed; 0 failed | 43.13 s |
| 5 | ok. 656 passed; 0 failed | 41.33 s |

`cargo fmt --check` clean.
