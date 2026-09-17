# Command Job Durability Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Persist one record per `start_command` job so results survive CatDesk restarts and restart-killed jobs surface as an explicit `interrupted` state.

**Architecture:** A new `job_store` module owns atomic JSON records under `~/.catdesk/jobs/`. `CommandJobManager::with_store(dir)` opts into persistence + lazy recovery; `new()`/`Default` keep today's in-memory behavior so the ~60 existing test call sites stay hermetic. Job elapsed time moves from `Instant` to Unix milliseconds so recovered jobs can report honest durations.

**Tech Stack:** Rust 2024, tokio, serde/serde_json, std::fs. No new dependencies.

**Spec:** `docs/command-job-durability.md` (same worktree).

## Global Constraints

- Comments, docs, and commit messages in English (upstream-facing repo); imperative commit subjects.
- Store uses `std::fs` only; atomic writes via temp file + rename in the same directory.
- Persistence is best-effort: store failures never fail job start/run; they record a fixed-name diagnostics event (`job_store_write_failed`, `job_store_record_corrupt`) through `crate::diagnostics::event`.
- `CommandJobManager::new()` and `Default` must remain in-memory-only (disabled store).
- Record `schema_version` is `1`; records with any other version or unparseable JSON are renamed `*.json.corrupt` and skipped.
- `DEFAULT_JOB_TIMEOUT_MS` becomes `2 * 60 * 60 * 1_000` (2 h); `MAX_JOB_TIMEOUT_MS` stays 24 h.
- `CURRENT_CHATGPT_CONNECTOR_REVISION` (src/state.rs:73) becomes `7`; `Cargo.toml` version becomes `0.8.1`.
- Every task ends with `cargo test` green and a commit on `fix/command-job-durability`.

---

### Task 1: `interrupted` state + Unix-millisecond elapsed base

**Files:**
- Modify: `src/command_jobs.rs` (enum, `CommandJob`, `JobRuntime`, `finish`, `snapshot`, constructors)
- Modify: `src/mcp.rs:1379-1405` (`command_job_output_text`), `src/mcp.rs:1407-1432` (`command_job_structured`), `src/mcp.rs:3033-3046` (label table), `src/mcp.rs:4378-4387` (label test cases)

**Interfaces:**
- Consumes: nothing new.
- Produces: `CommandJobState::Interrupted` (serializes `"interrupted"`, terminal, `Deserialize`); `CommandJob { started_at_unix_ms: u64, .. }` (no `started_at: Instant`); `JobRuntime { final_elapsed_ms: Option<u64>, .. }`; `fn unix_now_ms() -> u64` (private, command_jobs.rs).

- [ ] **Step 1: Write the failing tests**

In `src/command_jobs.rs` tests module:

```rust
#[test]
fn interrupted_state_serializes_snake_case_and_roundtrips() {
    assert_eq!(
        serde_json::to_string(&CommandJobState::Interrupted).expect("serialize"),
        "\"interrupted\""
    );
    let state: CommandJobState =
        serde_json::from_str("\"interrupted\"").expect("deserialize");
    assert!(state.is_terminal());
}
```

In `src/mcp.rs` tests module:

```rust
#[test]
fn interrupted_snapshot_text_explains_restart() {
    let snapshot = CommandJobSnapshot {
        job_id: "j".into(),
        command: "sleep 1".into(),
        cwd: "/w".into(),
        state: CommandJobState::Interrupted,
        elapsed_ms: 5,
        exit_code: None,
        events: Vec::new(),
        next_cursor: 0,
        has_more_output: false,
        output_truncated: false,
        timeout_ms: 1_000,
    };
    assert!(command_job_output_text(&snapshot).contains("interrupted"));
}
```

Extend the label cases table in the existing structured-output test (`src/mcp.rs:4378`):

```rust
("poll_command", "interrupted", "Command Interrupted", "failed"),
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test interrupted`
Expected: FAIL — `Interrupted` variant does not exist; `command_job_output_text` returns "(no new output)".

- [ ] **Step 3: Implement**

`src/command_jobs.rs`:

```rust
use std::time::{SystemTime, UNIX_EPOCH};

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}
```

Enum (add `Deserialize` to the existing `Serialize` derive, add variant before the closing brace):

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandJobState {
    Running,
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
    Interrupted,
}
```

`as_str` gains `Self::Interrupted => "interrupted"`.

`CommandJob`: replace field `started_at: Instant` with `started_at_unix_ms: u64`; `new_with_change_session` sets `started_at_unix_ms: unix_now_ms()`.

`JobRuntime::default` gains `final_elapsed_ms: None` (add the field `final_elapsed_ms: Option<u64>` to the struct).

`finish`:

```rust
async fn finish(&self, state: CommandJobState, exit_code: Option<i32>) {
    let mut runtime = self.runtime.lock().await;
    if runtime.state.is_terminal() {
        return;
    }
    runtime.state = state;
    runtime.exit_code = exit_code;
    runtime.finished_at = Some(Instant::now());
    runtime.final_elapsed_ms = Some(unix_now_ms().saturating_sub(self.started_at_unix_ms));
    drop(runtime);
    self.changed.notify_waiters();
}
```

`snapshot` elapsed line becomes:

```rust
elapsed_ms: runtime
    .final_elapsed_ms
    .unwrap_or_else(|| unix_now_ms().saturating_sub(self.started_at_unix_ms)),
```

`src/mcp.rs` — `command_job_structured` match gains:

```rust
CommandJobState::Interrupted => Some(false),
```

`command_job_output_text` empty-events match gains, before the `_` arm:

```rust
CommandJobState::Interrupted => {
    "(command interrupted: CatDesk exited before the command finished; output was not retained)"
        .to_string()
}
```

Label table (mcp.rs:3040 area) gains, before the `_` arm:

```rust
"interrupted" => ("Command Interrupted", "failed"),
```

Fix any remaining exhaustive matches the compiler reports (there is at least one more in the widget/dashboard mapping paths).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test`
Expected: PASS, all 261+ tests (the three new ones included).

- [ ] **Step 5: Commit**

```bash
git add src/command_jobs.rs src/mcp.rs
git commit -m "feat(jobs): add interrupted terminal state and unix-ms elapsed base"
```

---

### Task 2: `job_store` module

**Files:**
- Create: `src/job_store.rs`
- Modify: `src/main.rs` (add `mod job_store;` next to `mod command_jobs;` at line 5)

**Interfaces:**
- Consumes: `CommandJobState` (Task 1), `crate::state::user_home_dir`, `crate::diagnostics::event`.
- Produces:

```rust
pub struct JobRecord { /* schema below */ }
#[derive(Clone, Default)]
pub struct JobStore { /* dir: Option<Arc<PathBuf>> */ }
impl JobStore {
    pub fn open(dir: PathBuf) -> Self;
    pub fn disabled() -> Self;
    pub fn default_dir() -> Option<PathBuf>;   // ~/.catdesk/jobs
    pub fn write(&self, record: &JobRecord);   // best-effort, atomic
    pub fn read_all(&self) -> Vec<JobRecord>;  // skips+renames corrupt
    pub fn remove(&self, job_id: &str);
}
```

- [ ] **Step 1: Write the failing tests**

`src/job_store.rs`, bottom of file:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (JobStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!("catdesk-jobstore-{}", uuid::Uuid::new_v4()));
        (JobStore::open(dir.clone()), dir)
    }

    fn sample_record(state: CommandJobState) -> JobRecord {
        JobRecord {
            schema_version: 1,
            job_id: "job-1".into(),
            command: "sleep 1".into(),
            cwd: "/w".into(),
            workspace_root: "/w".into(),
            timeout_ms: 60_000,
            started_at_ms: 1_000,
            state,
            exit_code: Some(0),
            finished_at_ms: Some(2_000),
            elapsed_ms: Some(1_000),
        }
    }

    #[test]
    fn record_roundtrip_through_disk() {
        let (store, dir) = temp_store();
        store.write(&sample_record(CommandJobState::Succeeded));
        let records = store.read_all();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].job_id, "job-1");
        assert_eq!(records[0].state, CommandJobState::Succeeded);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn write_leaves_no_temp_file() {
        let (store, dir) = temp_store();
        store.write(&sample_record(CommandJobState::Running));
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .expect("read dir")
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left behind");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn corrupt_record_is_renamed_aside_and_skipped() {
        let (store, dir) = temp_store();
        std::fs::create_dir_all(&dir).expect("create dir");
        std::fs::write(dir.join("bad.json"), b"{ not json").expect("write bad record");
        store.write(&sample_record(CommandJobState::Running));
        let records = store.read_all();
        assert_eq!(records.len(), 1);
        assert!(dir.join("bad.json.corrupt").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn disabled_store_is_noop() {
        let store = JobStore::disabled();
        store.write(&sample_record(CommandJobState::Running));
        assert!(store.read_all().is_empty());
        store.remove("job-1"); // must not panic
    }

    #[test]
    fn missing_directory_reads_empty() {
        let dir = std::env::temp_dir().join(format!("catdesk-jobstore-{}", uuid::Uuid::new_v4()));
        let store = JobStore::open(dir.clone());
        assert!(store.read_all().is_empty());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test job_store`
Expected: FAIL — module does not compile/exist yet (add `mod job_store;` to main.rs first so the test target sees it).

- [ ] **Step 3: Implement**

`src/job_store.rs`:

```rust
//! Durable records for command jobs (docs/command-job-durability.md).
//!
//! One JSON file per job, written atomically at job start and at the
//! terminal transition. Best-effort by design: a failing store must never
//! take job execution down with it, so errors degrade to a diagnostics
//! event and in-memory-only behavior.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::command_jobs::CommandJobState;

const JOB_RECORD_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
pub struct JobRecord {
    pub schema_version: u32,
    pub job_id: String,
    pub command: String,
    pub cwd: String,
    pub workspace_root: String,
    pub timeout_ms: u64,
    pub started_at_ms: u64,
    pub state: CommandJobState,
    pub exit_code: Option<i32>,
    pub finished_at_ms: Option<u64>,
    pub elapsed_ms: Option<u64>,
}

/// Best-effort, cheap-to-clone job record store. `disabled()` reproduces
/// the pre-durability in-memory-only behavior; `open(dir)` persists.
#[derive(Clone, Default)]
pub struct JobStore {
    dir: Option<Arc<PathBuf>>,
}

impl JobStore {
    pub fn open(dir: PathBuf) -> Self {
        Self {
            dir: Some(Arc::new(dir)),
        }
    }

    pub fn disabled() -> Self {
        Self { dir: None }
    }

    pub fn default_dir() -> Option<PathBuf> {
        crate::state::user_home_dir()
            .ok()
            .map(|home| home.join(".catdesk").join("jobs"))
    }

    pub fn write(&self, record: &JobRecord) {
        let Some(dir) = self.dir.as_deref() else {
            return;
        };
        let result = std::fs::create_dir_all(dir).and_then(|()| {
            let final_path = dir.join(format!("{}.json", record.job_id));
            let tmp_path = dir.join(format!("{}.json.tmp", record.job_id));
            let payload = serde_json::to_vec(record)
                .map_err(std::io::Error::other)?;
            std::fs::write(&tmp_path, &payload)?;
            std::fs::rename(&tmp_path, &final_path)
        });
        if result.is_err() {
            crate::diagnostics::event("job_store_write_failed");
        }
    }

    pub fn read_all(&self) -> Vec<JobRecord> {
        let Some(dir) = self.dir.as_deref() else {
            return Vec::new();
        };
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(_) => return Vec::new(),
        };
        let mut records = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            match Self::parse(&path) {
                Some(record) => records.push(record),
                None => {
                    let _ = std::fs::rename(&path, path.with_extension("json.corrupt"));
                    crate::diagnostics::event("job_store_record_corrupt");
                }
            }
        }
        records
    }

    pub fn remove(&self, job_id: &str) {
        let Some(dir) = self.dir.as_deref() else {
            return;
        };
        let _ = std::fs::remove_file(dir.join(format!("{job_id}.json")));
    }

    fn parse(path: &Path) -> Option<JobRecord> {
        let bytes = std::fs::read(path).ok()?;
        let record: JobRecord = serde_json::from_slice(&bytes).ok()?;
        (record.schema_version == JOB_RECORD_SCHEMA_VERSION).then_some(record)
    }
}
```

`src/main.rs`, next to `mod command_jobs;` (line 5):

```rust
mod job_store;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test job_store`
Expected: PASS (5 tests).

- [ ] **Step 5: Commit**

```bash
git add src/job_store.rs src/main.rs
git commit -m "feat(jobs): add best-effort job record store"
```

---

### Task 3: Manager persistence and recovery

**Files:**
- Modify: `src/command_jobs.rs` (manager struct/constructors, `CommandJob` constructors, `finish`, `start_with_change_session`, `cleanup`)

**Interfaces:**
- Consumes: `JobRecord`, `JobStore` (Task 2); `CommandJobState::Interrupted`, `unix_now_ms`, `started_at_unix_ms`, `final_elapsed_ms` (Task 1).
- Produces: `CommandJobManager::with_store(dir: PathBuf) -> Self` (sync ctor; recovery runs lazily on the first `cleanup()`); manager fields `store: JobStore`, `recovered: Arc<AtomicBool>`; `CommandJob::new_with_change_session(..., store: JobStore)`; private `CommandJob::to_record(&self, &JobRuntime) -> JobRecord`, `CommandJob::persist(&self)`, `CommandJob::recovered(record: JobRecord, state, final_elapsed_ms, store) -> (Arc<Self>, watch::Receiver<bool>)`.

- [ ] **Step 1: Write the failing tests**

In `src/command_jobs.rs` tests module:

```rust
fn manager_with_store() -> (CommandJobManager, PathBuf) {
    let dir = std::env::temp_dir().join(format!("catdesk-jobstore-{}", Uuid::new_v4()));
    (CommandJobManager::with_store(dir.clone()), dir)
}

#[tokio::test]
async fn start_persists_running_record_and_finish_updates_it() {
    let (manager, dir) = manager_with_store();
    let started = manager
        .start("printf 'done\\n'".into(), dir.clone(), 5_000, None)
        .await
        .expect("start job");
    let records = manager.store.read_all();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].state, CommandJobState::Running);

    let terminal = wait_terminal(&manager, &started.snapshot.job_id).await;
    assert_eq!(terminal.state, CommandJobState::Succeeded);
    let records = manager.store.read_all();
    assert_eq!(records[0].state, CommandJobState::Succeeded);
    assert_eq!(records[0].exit_code, Some(0));
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn recovery_marks_running_records_interrupted_and_rewrites_them() {
    let (manager, dir) = manager_with_store();
    let started = manager
        .start("sleep 30".into(), dir.clone(), 60_000, None)
        .await
        .expect("start job");

    // A second manager over the same store simulates an app restart.
    let restarted = CommandJobManager::with_store(dir.clone());
    let snapshot = restarted
        .poll(&started.snapshot.job_id, 0, 0)
        .await
        .expect("poll recovered job");
    assert_eq!(snapshot.state, CommandJobState::Interrupted);
    assert_eq!(snapshot.exit_code, None);
    assert!(snapshot.events.is_empty());
    assert!(!snapshot.has_more_output);

    let records = restarted.store.read_all();
    assert_eq!(records[0].state, CommandJobState::Interrupted);

    let _ = manager.cancel(&started.snapshot.job_id).await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn recovery_restores_terminal_records_as_pollable() {
    let (manager, dir) = manager_with_store();
    let started = manager
        .start("printf 'x'".into(), dir.clone(), 5_000, None)
        .await
        .expect("start job");
    let terminal = wait_terminal(&manager, &started.snapshot.job_id).await;
    assert_eq!(terminal.state, CommandJobState::Succeeded);

    let restarted = CommandJobManager::with_store(dir.clone());
    let snapshot = restarted
        .poll(&started.snapshot.job_id, 0, 0)
        .await
        .expect("poll restored job");
    assert_eq!(snapshot.state, CommandJobState::Succeeded);
    assert_eq!(snapshot.exit_code, Some(0));
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn cleanup_eviction_removes_store_file() {
    let (manager, dir) = manager_with_store();
    let started = manager
        .start("printf 'x'".into(), dir.clone(), 5_000, None)
        .await
        .expect("start job");
    let job_id = started.snapshot.job_id.clone();
    let _ = wait_terminal(&manager, &job_id).await;
    assert!(dir.join(format!("{job_id}.json")).exists());

    {
        let jobs = manager.inner.write().await;
        let job = jobs.jobs.get(&job_id).expect("job present");
        job.runtime.lock().await.finished_at =
            Some(Instant::now() - TERMINAL_JOB_TTL - StdDuration::from_secs(1));
    }
    manager.inner.write().await.last_cleanup = None;
    manager.cleanup().await;
    assert!(!dir.join(format!("{job_id}.json")).exists());
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn cancel_all_persists_cancelled_state() {
    let (manager, dir) = manager_with_store();
    let started = manager
        .start("sleep 5".into(), dir.clone(), 60_000, None)
        .await
        .expect("start job");
    manager.cancel_all().await;
    let records = manager.store.read_all();
    assert_eq!(records[0].state, CommandJobState::Cancelled);
    let _ = std::fs::remove_dir_all(dir);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test job_store` plus the new test names (`cargo test persists`, `cargo test recovery`, `cargo test eviction_removes`, `cargo test cancel_all_persists`)
Expected: FAIL — `with_store` and `manager.store` do not exist.

- [ ] **Step 3: Implement**

Manager struct gains (keep `#[derive(Clone, Default)]`; `JobStore: Default` is disabled):

```rust
#[derive(Clone, Default)]
pub struct CommandJobManager {
    inner: Arc<RwLock<ManagerState>>,
    start_lock: Arc<Mutex<()>>,
    shutting_down: Arc<AtomicBool>,
    store: JobStore,
    // Recovery runs once, lazily, on the first cleanup() after startup.
    recovered: Arc<AtomicBool>,
}
```

Import `use crate::job_store::{JobRecord, JobStore};`.

Constructors:

```rust
pub fn new() -> Self {
    Self::default()
}

pub fn with_store(dir: PathBuf) -> Self {
    Self {
        store: JobStore::open(dir),
        ..Self::default()
    }
}
```

`CommandJob` gains `store: JobStore`; `new_with_change_session` gains a `store: JobStore` parameter (last position) and stores it; the `#[cfg(test)]` `new` passes `JobStore::disabled()`. `start_with_change_session` passes `self.store.clone()` and, after the registry insert + request-key insert block, adds:

```rust
job.persist().await;
```

New `CommandJob` methods:

```rust
fn to_record(&self, runtime: &JobRuntime) -> JobRecord {
    JobRecord {
        schema_version: 1,
        job_id: self.id.clone(),
        command: self.command.clone(),
        cwd: self.cwd.to_string_lossy().into_owned(),
        workspace_root: self.workspace_root.to_string_lossy().into_owned(),
        timeout_ms: self.timeout_ms,
        started_at_ms: self.started_at_unix_ms,
        state: runtime.state,
        exit_code: runtime.exit_code,
        finished_at_ms: runtime.finished_at.map(|_| unix_now_ms()),
        elapsed_ms: runtime.final_elapsed_ms,
    }
}

async fn persist(&self) {
    let record = {
        let runtime = self.runtime.lock().await;
        self.to_record(&runtime)
    };
    self.store.write(&record);
}

/// Build a job from a persisted record. Only recovery constructs these;
/// `state` is already terminal (`running` records arrive as
/// `Interrupted` from the caller).
fn recovered(
    record: JobRecord,
    state: CommandJobState,
    final_elapsed_ms: Option<u64>,
    store: JobStore,
) -> (Arc<Self>, watch::Receiver<bool>) {
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let runtime = JobRuntime {
        state,
        exit_code: record.exit_code,
        finished_at: Some(Instant::now()),
        final_elapsed_ms,
        ..JobRuntime::default()
    };
    (
        Arc::new(Self {
            id: record.job_id,
            command: record.command,
            workspace_root: PathBuf::from(record.workspace_root),
            cwd: PathBuf::from(record.cwd),
            started_at_unix_ms: record.started_at_ms,
            timeout_ms: record.timeout_ms,
            change_session: None,
            runtime: Mutex::new(runtime),
            changed: Notify::new(),
            cancel_tx,
            store,
        }),
        cancel_rx,
    )
}
```

`finish` becomes (record built under the lock, written after):

```rust
async fn finish(&self, state: CommandJobState, exit_code: Option<i32>) {
    let record = {
        let mut runtime = self.runtime.lock().await;
        if runtime.state.is_terminal() {
            return;
        }
        runtime.state = state;
        runtime.exit_code = exit_code;
        runtime.finished_at = Some(Instant::now());
        runtime.final_elapsed_ms = Some(unix_now_ms().saturating_sub(self.started_at_unix_ms));
        self.to_record(&runtime)
    };
    self.store.write(&record);
    self.changed.notify_waiters();
}
```

Recovery — first lines of `cleanup()`, before the rate-limit check:

```rust
if !self.recovered.swap(true, Ordering::AcqRel) {
    self.recover().await;
}
```

and the method:

```rust
/// Load persisted records once per process. Terminal records come back
/// pollable; `running` records mean the previous process died with the
/// job's process tree — surface them as `interrupted` and rewrite the
/// record so a later restart sees the terminal state.
async fn recover(&self) {
    for record in self.store.read_all() {
        let was_running = record.state == CommandJobState::Running;
        let state = if was_running {
            CommandJobState::Interrupted
        } else {
            record.state
        };
        let final_elapsed_ms = Some(match record.elapsed_ms {
            Some(elapsed) => elapsed,
            None => unix_now_ms().saturating_sub(record.started_at_ms),
        });
        let (job, _cancel_rx) =
            CommandJob::recovered(record, state, final_elapsed_ms, self.store.clone());
        if was_running {
            job.persist().await;
        }
        let mut manager = self.inner.write().await;
        manager.jobs.insert(job.id.clone(), job.clone());
    }
}
```

Eviction in `cleanup()` — inside the final write-lock block:

```rust
let mut manager = self.inner.write().await;
for id in &expired {
    manager.jobs.remove(id);
    self.store.remove(id);
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test`
Expected: PASS, all tests including the five new ones.

- [ ] **Step 5: Commit**

```bash
git add src/command_jobs.rs
git commit -m "feat(jobs): persist job records and recover them across restarts"
```

---

### Task 4: Production wiring, MCP surface, release bump

**Files:**
- Modify: `src/state.rs:73` (revision), `src/state.rs:972` (production manager), `src/command_jobs.rs:16` (default timeout), `src/mcp.rs:862-916` (tool descriptions), `src/mcp.rs:2289-2297` (instruction), `Cargo.toml:3` (version)

**Interfaces:**
- Consumes: `CommandJobManager::with_store`, `JobStore::default_dir` (Task 3/2).
- Produces: released surface — new default timeout, documented `interrupted`, connector revision 7, version 0.8.1.

- [ ] **Step 1: Production wiring (src/state.rs:972)**

Replace:

```rust
command_jobs: CommandJobManager::new(),
```

with:

```rust
command_jobs: crate::job_store::JobStore::default_dir()
    .map(CommandJobManager::with_store)
    .unwrap_or_else(CommandJobManager::new),
```

- [ ] **Step 2: Constants and version**

`src/command_jobs.rs:16`:

```rust
pub const DEFAULT_JOB_TIMEOUT_MS: u64 = 2 * 60 * 60 * 1_000;
```

`src/state.rs:73`:

```rust
pub const CURRENT_CHATGPT_CONNECTOR_REVISION: u32 = 7;
```

`Cargo.toml:3`: `version = "0.8.1"`.

- [ ] **Step 3: Tool descriptions (src/mcp.rs)**

`start_command` timeout param description (mcp.rs ~877):

```rust
"maximum command runtime in milliseconds. Defaults to {} ms; maximum is {} ms. The job's state and exit code survive a CatDesk restart; a job the restart took down reports state \"interrupted\"."
```

`poll_command` description (mcp.rs ~886) — append one sentence:

```
A job that was running when CatDesk exited reports state "interrupted" with no further output.
```

`catdesk_instruction` (mcp.rs ~2293, after the poll_command line) — add:

```rust
lines.push(
    "Command results survive a CatDesk restart: finished jobs keep their state and exit code, and a job that was running when CatDesk exited reports \"interrupted\" — start it again if its work is still needed.".to_string(),
);
```

- [ ] **Step 4: Verify everything**

Run: `cargo test`
Expected: PASS (the default-timeout boundary test reads the constant, so it stays green).

Run: `cargo build --release 2>&1 | tail -3`
Expected: `Finished \`release\` profile` with no errors (this takes several minutes — fat LTO).

Run: `grep -n 'CURRENT_CHATGPT_CONNECTOR_REVISION: u32 = 7' src/state.rs && grep -n '^version = "0.8.1"' Cargo.toml && grep -n 'DEFAULT_JOB_TIMEOUT_MS: u64 = 2' src/command_jobs.rs`
Expected: all three lines match.

- [ ] **Step 5: Commit**

```bash
git add src/state.rs src/command_jobs.rs src/mcp.rs Cargo.toml Cargo.lock
git commit -m "feat(jobs): wire durable job store, 2h default timeout, connector revision 7"
```

---

## Final verification (after Task 4)

- `cargo test` — full suite green.
- `git log --oneline main..fix/command-job-durability` — spec, plan, and one commit per task.
- Manual smoke (optional, needs running app): start a `sleep 300` job via the MCP endpoint, kill the catdesk process, restart, poll the job id — expect `interrupted`.
- Deployment (user decision, outside this plan): merge to main, keep the release binary, restart CatDesk.
