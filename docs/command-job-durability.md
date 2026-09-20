# Command job durability

Design for making `start_command` jobs honest about how they end. Today a
command job lives only in the RAM of the running CatDesk process: an app
restart kills every running job's process tree (`cancel_all` on exit) and
erases the registry, so `poll_command` answers `unknown or expired command
job` and the agent that started the job never learns what happened. Jobs
longer than the gap between two app restarts simply disappear without an
exit code. Observed in practice: ten app restarts during one day, and MCP
requests cancelled mid-flight (`http_cancelled` in the connections log),
with long builds stranded between them.

The fix persists one small record per job to disk. Results of finished jobs
survive restarts, and jobs that were running when the app died come back as
an explicit terminal state instead of vanishing. Process trees still die
with the app — ownership (kill-on-drop, kill-on-exit) is intentional and
stays.

## Goals

- Exit code and terminal state of every job survive an app restart within
  the existing retention window.
- A job that was running when CatDesk exited is reported to agents as
  `interrupted`, with a clear reason, instead of `unknown or expired`.
- No orphaned processes: restarting CatDesk still terminates running
  command trees. We persist records, not processes.
- Degrade gracefully: an unwritable or corrupt store never blocks starting
  or running jobs.

## Non-goals

- Process survival across restarts (no detached execution, no re-attach).
- Persisting output events. Output of an interrupted job is unrecoverable
  anyway (it dies with the process); output of a finished job is readable
  live, and after a restart the agent gets state and exit code without the
  log.
- Changes to the foreground `run_command` path, including its 30 s default
  and kill-on-drop semantics.
- Any CLI or UI to browse historical jobs.

## Store

- Directory: `~/.catdesk/jobs/` (same convention and resolution as
  `~/.catdesk/logs`, via `user_home_dir()`). One JSON file per job,
  named `<job_id>.json`.
- Records are written with `std::fs` (small payloads, two writes per job):
  atomically — temp file in the same directory, then rename. On Unix the
  store directory is forced to `0700` and record files to `0600`, because a
  command line can contain sensitive arguments.
- Record schema:

  ```json
  {
    "schema_version": 1,
    "job_id": "…",
    "command": "…",
    "cwd": "…",
    "workspace_root": "…",
    "timeout_ms": 7200000,
    "started_at_ms": 1789598787563,
    "state": "running",
    "exit_code": null,
    "finished_at_ms": null,
    "elapsed_ms": null
  }
  ```

  Timestamps are Unix milliseconds, matching the connections log. `state`
  uses the same snake_case spelling as the MCP snapshots. For terminal
  records `exit_code`, `finished_at_ms` and `elapsed_ms` are filled
  (`exit_code` is numeric for live terminal outcomes, including conventional
  `128 + signal` values such as `137` for `SIGKILL`; recovered `interrupted`
  jobs have no process exit status and therefore keep it `null`).
- Writes happen at exactly two points: when a job starts (`"running"`) and
  when `finish()` records its terminal state. Nothing is written on output
  events or polls.
- File lifecycle follows the in-memory lifecycle one-to-one: when `cleanup()`
  evicts a job (terminal TTL of one hour, the 64-job cap, or the global
  output budget), its file is deleted. A recovered terminal job has zero
  retained output bytes, so the global output budget never evicts it.

## States

`CommandJobState` gains one terminal variant: `interrupted` (serialized
`"interrupted"`). Semantics:

| State | Meaning | Set by |
|---|---|---|
| `running` | process tree alive | spawn |
| `succeeded` / `failed` | exited, numeric exit code recorded; Unix signals use `128 + signal` | runner |
| `cancelled` | a cancel request reached the runner in time (user cancel, or `cancel_all` during graceful shutdown) | runner |
| `timed_out` | timeout hit, tree terminated | runner |
| `interrupted` | the CatDesk process died before the job reached a terminal state | recovery at next startup |

`interrupted` is never produced by a live runner — only by recovery.

## Recovery

The manager loads the store directory when it is created (app startup):

- Terminal records (`succeeded`, `failed`, `cancelled`, `timed_out`) are
  restored into the registry: polls return their state, exit code and
  elapsed time, with empty events, `hasMoreOutput: false`, and text that
  does not imply the output log was drained.
- Records in state `running` are marked `interrupted`, with
  `finished_at` set to load time so the normal one-hour terminal TTL
  applies from the restart, not from the original start.
- A record whose JSON is corrupt or has an unknown `schema_version` is
  renamed aside (`.corrupt` suffix) and skipped, with a diagnostics event;
  recovery continues.
- Recovery does not apply timeout logic: a job whose `timeout_ms` elapsed
  while the app was dead is still `interrupted` — the app death is what
  actually happened to it.
- Recovered jobs are terminal, so they never count against the
  eight-active-jobs limit.
- The store is per-user and CatDesk is a single-instance desktop app.
  If a second instance ever starts against the same directory, it will
  mark the first instance's running jobs as `interrupted`; accepted as a
  documented limitation.

## Shutdown interplay

Graceful shutdown (`cancel_all` on app exit) sends cancel to every running
job. Runners that process it within the existing five-second wait record
`cancelled` and persist it through the normal finish path. Only jobs whose
runners did not get a chance to finish (crash, `kill -9`, or the deadline
expiring) remain `running` on disk and become `interrupted` at the next
startup. No new shutdown behavior is introduced.

## Failure handling

Persistence is best-effort. If the store directory cannot be created or a
write fails, the manager logs a diagnostics event (connections log) and
continues exactly as today, in memory. Starting a job must never fail
because of the store. Reads during recovery treat an unreadable directory
as empty.

## Manager construction

`CommandJobManager::new()` (and `Default`) keeps today's behavior: an
in-memory-only manager with a disabled store, so the ~60 existing test
call sites stay hermetic without a sweep. Persistence and recovery opt in
explicitly: `CommandJobManager::with_store(dir)` opens the store, runs
recovery once at construction, and is used by the single production
construction site in `main.rs` with `~/.catdesk/jobs` (resolved via
`user_home_dir()`; a missing home directory falls back to `new()`).

## MCP surface

- Structured outputs keep their shape; `state` can now be
  `"interrupted"`.
- Tool descriptions for `start_command` / `poll_command` /
  `cancel_command` document the new state and the fact that results
  survive restarts; `start_command` states the default and maximum
  timeout.
- `catdesk_instruction` gains one sentence: finished results survive a
  CatDesk restart, and a job the app took down surfaces as `interrupted`
  rather than disappearing.
- `CURRENT_CHATGPT_CONNECTOR_REVISION` bumps 6 → 7 (tool surface change).

## Default timeout

`DEFAULT_JOB_TIMEOUT_MS` rises from 30 minutes to 2 hours (maximum stays
24 hours). With restarts visible as `interrupted`, the 30-minute default
was the remaining silent killer of long-but-forgotten jobs; a fat-LTO
build on this machine can spend over ten minutes in the link step alone.
An explicitly passed `timeout` keeps priority as today.

## Testing

New tests (store in a temp dir throughout):

- Roundtrip: start persists a `running` record; natural completion persists
  `succeeded` with exit code; failure, cancel, and timeout paths persist
  their states.
- Recovery: a `running` record becomes `interrupted` with `finished_at` at
  load; terminal records restore as pollable with empty events and
  `hasMoreOutput: false`.
- Cleanup evicts the file together with the in-memory job.
- No temp-file residue after writes; corrupt record is skipped without
  blocking recovery.
- `cancel_all` during shutdown persists `cancelled` for runners that
  finish in time.
- An unwritable store directory degrades to in-memory behavior without
  failing job starts.

The existing `command_jobs` suite keeps passing unchanged except where the
default timeout constant is asserted.

## Release

Fork-local patch release: `Cargo.toml` version 0.8.0 → 0.8.1. Deploy is
`cargo build --release` and an app restart, since the launcher runs the
locally built binary. Restarting the app kills any command jobs running at
that moment — done deliberately, outside working agent sessions.

## Idle reaping

Time bounds alone do not tell an abandoned job from a wanted one: an agent
can start a dev server with a 24-hour timeout and never come back. Polling
is the honest signal of interest, so it doubles as a keep-alive heartbeat.

- Every job tracks `last_poll`, refreshed at the start of each
  `poll_command` (including the first poll after `start_command`).
- A running job whose quiet window — `DEFAULT_ABANDON_AFTER_MS`, 30 minutes
  — elapses with no poll is terminated by its own runner: process tree
  torn down, one stderr line explaining why (`no poll for … ms`), terminal
  state `abandoned`, synthetic exit code `131` (`EXIT_CODE_ABANDONED`).
- The deadline slides: polls push it out, so a long build polled every
  half minute runs to completion and a chained long-poll keeps a needed
  server alive indefinitely. The runner recomputes the deadline after
  every wake-up so a poll landing inside the check gap is never reaped.
- `abandoned` is terminal like the others: recovered records restore as
  pollable across restarts, the record schema is unchanged, and the state
  never counts against the active-job limit.
- The MCP surface documents the contract on `start_command` /
  `poll_command` and in `catdesk_instruction`; the widget title mapping
  renders it as "Command Abandoned" (failed).
  `CURRENT_CHATGPT_CONNECTOR_REVISION` bumps 7 → 8 for the tool-surface
  change.
