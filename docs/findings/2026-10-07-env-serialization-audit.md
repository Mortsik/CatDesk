# Env serialization audit: tests that spawn child processes (2026-10-07)

## Problem

`PATH` is process-global and rewritten by several `linux_sandbox` tests while
they run (`EnvGuards`, which hold `crate::test_serialization::lock_env()`).
Every test that spawns a child whose resolution depends on `PATH` — the shell
itself, `sleep`/`head`/`tr` inside `-c` scripts, `rg`/`grep`, `git`,
`systemd-run`, `unshare`/`mount` — races those rewrites. A spawn that lands in
a rewrite window gets the stub-only `PATH` and fails with ENOENT
("ripgrep disappeared while running search", "sleep: command not found",
"background command did not finish", `timed_out == false`).

Observed in full `cargo test --release` runs: random failing sets changed
between runs (572/3, 568/7, 571/4, 573/2 failures) while the same tests passed
in isolation. A control run on a commit without the sandbox preflight tests
still failed for a different, deterministic reason — the race predates it;
the preflight tests only added one more (longer) window.

## Rule

Every test whose flow can reach a child-process spawn takes the env lock as
its first statement:

```rust
let _env = env_lock(); // per-file helper, = crate::test_serialization::lock_env()
```

- Files with an existing helper reuse it (`mcp/tests.rs`, `workspace_tools.rs`,
  `command_jobs.rs`, `handoff.rs`, `change_tracking/snapshot.rs`,
  `linux_sandbox.rs` — the latter uses its own `EnvGuards::read()` idiom,
  which takes the same lock). Other files call
  `crate::test_serialization::lock_env()` inline.
- `#[tokio::test]` defaults to a current-thread runtime, so holding the std
  `MutexGuard` across `.await` cannot deadlock (established idiom in
  `mcp/tests.rs`).
- The lock serializes against the `linux_sandbox` env rewrites, so a spawn can
  never observe a stub-only `PATH` (or an NVM/Playwright rewrite).

## Per-file audit (sweep of commit a65ee7e)

| File | Locked before | Locked after | Still unlocked (audited: no spawn reach) |
|---|---|---|---|
| `command_jobs.rs` | 1 | 25 | 4 validation-only tests (no `.start(`) |
| `mcp/tests.rs` | 1 | 35 | ~44: schema/argument-rejection (`*_rejects_*`, `read_only_mode_blocks_*`), instruction/result-tool/payload-only tests — flow stops before any spawn |
| `server.rs` | 0 | 7 | ~29: health/ping/usage/widget and label/classification unit tests — no command execution on the path |
| `soak.rs` | 0 | 5 | — (every scenario drives real jobs through the spawned server) |
| `process_runner.rs` | 0 | 8 | `capture_reader_*` (in-process reader unit), `spawn_shell_command_denies_*` (denied before spawn) |
| `workspace_tools.rs` | 8 | 9 | builtin (in-process) search tests — no external binary; `search_text_grep`/`search_files` tests spawn `grep`/`rg` |
| `linux_sandbox.rs` | 10 | 11 | ~12 tests without `run_git`/`Command::new` fixtures |
| `handoff.rs` | 4 | 5 | 3: `render_handoff_*` renders from input structs, two create-handoff path tests without git |
| `change_tracking/snapshot.rs` | 2 | 2 | mount-boundary tests (`unshare`/`mount` spawns) were already locked; remaining are in-process |
| `diagnostics.rs` | 0 | 3 | pure log-file test; `real_http_requests`/`perf_metrics_observe` locked defensively — their flow reaches `CommandJobManager` |
| `devtools.rs` | 0 | 6 | — (`peer()`/`restartable_peer()` fixtures spawn real `sh` children) |
| `command_policy.rs`, `tests.rs` | 2 | 2 | already locked |

Totals: 163 spawn-flow tests identified by the audit, 26 locked before the
sweep, 113 locked after (87 added). The remaining ~50 tests were audited and
reach no child-process spawn; the table records the reason per file.

## Known compromises

1. **`soak.rs` holds the lock for the whole test** (sleeps of 1–2 s inside
   `start_command` jobs), adding roughly 10 s of serialization to the suite.
   The spawns happen inside the HTTP handler on the shared current-thread
   runtime, so a narrower window is not extractable without refactoring the
   helpers; judged acceptable versus per-test lock choreography.
2. **Payload/shape tests that go through `handle_tools_call` with a command
   tool in their fixture are locked defensively** even when the asserted
   property is a payload shape (e.g. `command_job_widget_state_matrix_*`).
   Triage of "does this exact call spawn?" per test is fragile — the handler
   owns the spawn decision, not the test.
3. **`server.rs` lock coverage is marker-based**: tests whose bodies mention a
   command tool (`start_command`/`run_command`/`poll_command`/
   `cancel_command`) are locked. A test that reaches command execution through
   a future helper without naming the tool would need its lock added by hand —
   hence the rule below.

## Rule for new tests

Before writing a test, ask: does anything on its call chain spawn a process?
If yes (command tools, search, git, sandbox, manager/job starts, devtools
peer), the test's first statement is the env lock. A test that only validates
arguments/schemas/payload shapes without invoking the flow does not need it.
When in doubt, take the lock: it costs serialization only, never correctness.
