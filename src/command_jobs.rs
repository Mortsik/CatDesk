use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration as StdDuration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::{Mutex, Notify, OnceCell, OwnedSemaphorePermit, RwLock, Semaphore, watch};
use tokio::time::{Duration, timeout};
use uuid::Uuid;

use crate::change_tracking::{ChangeSession, FileChange};
use crate::job_store::{JobRecord, JobStore};
use crate::process_exit::{
    EXIT_CODE_ABANDONED, EXIT_CODE_CANCELLED, EXIT_CODE_INTERNAL_ERROR, EXIT_CODE_TIMEOUT,
    exit_code_for_status,
};
use crate::process_runner;

pub const DEFAULT_JOB_TIMEOUT_MS: u64 = 2 * 60 * 60 * 1_000;
pub const MAX_JOB_TIMEOUT_MS: u64 = 24 * 60 * 60 * 1_000;
// A running job whose owner never polls is reaped after this window; polling
// is the heartbeat that keeps a still-needed job alive.
pub const DEFAULT_ABANDON_AFTER_MS: u64 = 30 * 60 * 1_000;
pub const MAX_POLL_WAIT_MS: u64 = 30_000;
pub const DEFAULT_POLL_WAIT_MS: u64 = 10_000;
const MAX_ACTIVE_JOBS: usize = 8;
const MAX_ACTIVE_PROCESSES: usize = 12;
const MAX_RETAINED_JOBS: usize = 64;
const TERMINAL_JOB_TTL: StdDuration = StdDuration::from_secs(60 * 60);
const IDEMPOTENCY_WINDOW: StdDuration = StdDuration::from_secs(30);
const MAX_OUTPUT_BYTES_PER_JOB: usize = 4 * 1024 * 1024;
const MAX_TERMINAL_OUTPUT_BYTES: usize = 32 * 1024 * 1024;
const MAX_POLL_OUTPUT_BYTES: usize = 128 * 1024;
const READ_CHUNK_BYTES: usize = 8 * 1024;
const CLEANUP_INTERVAL: StdDuration = StdDuration::from_secs(1);

// Elapsed time is Unix-milliseconds-based, not Instant-based, so jobs
// recovered from persisted records can report honest durations without
// reconstructing a monotonic clock across process boundaries.
fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandJobState {
    Running,
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
    Abandoned,
    Interrupted,
}

impl CommandJobState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed_out",
            Self::Abandoned => "abandoned",
            Self::Interrupted => "interrupted",
        }
    }

    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct CommandOutputEvent {
    pub seq: u64,
    pub stream: &'static str,
    pub text: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandJobSnapshot {
    pub job_id: String,
    pub command: String,
    pub cwd: String,
    pub state: CommandJobState,
    pub elapsed_ms: u64,
    pub exit_code: Option<i32>,
    pub events: Vec<CommandOutputEvent>,
    pub next_cursor: u64,
    pub has_more_output: bool,
    pub output_truncated: bool,
    pub timeout_ms: u64,
}

#[derive(Clone, Debug)]
pub struct StartCommandResult {
    pub snapshot: CommandJobSnapshot,
    pub deduplicated: bool,
}

#[derive(Debug)]
struct JobRuntime {
    state: CommandJobState,
    exit_code: Option<i32>,
    finished_at: Option<Instant>,
    final_elapsed_ms: Option<u64>,
    events: VecDeque<CommandOutputEvent>,
    retained_output_bytes: usize,
    next_seq: u64,
    output_truncated: bool,
}

impl Default for JobRuntime {
    fn default() -> Self {
        Self {
            state: CommandJobState::Running,
            exit_code: None,
            finished_at: None,
            final_elapsed_ms: None,
            events: VecDeque::new(),
            retained_output_bytes: 0,
            next_seq: 1,
            output_truncated: false,
        }
    }
}

#[derive(Debug)]
struct CommandJob {
    id: String,
    command: String,
    workspace_root: PathBuf,
    cwd: PathBuf,
    owner_session: Option<String>,
    started_at_unix_ms: u64,
    timeout_ms: u64,
    abandon_after_ms: u64,
    change_session: Option<ChangeSession>,
    store: JobStore,
    runtime: Mutex<JobRuntime>,
    // Heartbeat for idle reaping: the most recent poll_command instant. A
    // std mutex is enough — held only for non-async reads and writes.
    last_poll: std::sync::Mutex<Instant>,
    changed: Notify,
    cancel_tx: watch::Sender<bool>,
}

impl CommandJob {
    #[cfg(test)]
    fn new(command: String, cwd: PathBuf, timeout_ms: u64) -> (Arc<Self>, watch::Receiver<bool>) {
        Self::new_with_change_session(
            command,
            cwd.clone(),
            cwd,
            timeout_ms,
            DEFAULT_ABANDON_AFTER_MS,
            None,
            None,
            JobStore::disabled(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new_with_change_session(
        command: String,
        workspace_root: PathBuf,
        cwd: PathBuf,
        timeout_ms: u64,
        abandon_after_ms: u64,
        change_session: Option<ChangeSession>,
        owner_session: Option<String>,
        store: JobStore,
    ) -> (Arc<Self>, watch::Receiver<bool>) {
        let (cancel_tx, cancel_rx) = watch::channel(false);
        (
            Arc::new(Self {
                id: Uuid::new_v4().to_string(),
                command,
                workspace_root,
                cwd,
                owner_session,
                started_at_unix_ms: unix_now_ms(),
                timeout_ms,
                abandon_after_ms,
                change_session,
                store,
                runtime: Mutex::new(JobRuntime::default()),
                last_poll: std::sync::Mutex::new(Instant::now()),
                changed: Notify::new(),
                cancel_tx,
            }),
            cancel_rx,
        )
    }

    fn mark_polled(&self) {
        *self.last_poll.lock().expect("last_poll mutex poisoned") = Instant::now();
    }

    fn to_record(&self, runtime: &JobRuntime) -> JobRecord {
        JobRecord {
            schema_version: 1,
            job_id: self.id.clone(),
            command: self.command.clone(),
            cwd: self.cwd.to_string_lossy().into_owned(),
            workspace_root: self.workspace_root.to_string_lossy().into_owned(),
            owner_session: self.owner_session.clone(),
            timeout_ms: self.timeout_ms,
            started_at_ms: self.started_at_unix_ms,
            state: runtime.state,
            exit_code: runtime.exit_code,
            finished_at_ms: runtime
                .final_elapsed_ms
                .map(|elapsed| self.started_at_unix_ms.saturating_add(elapsed)),
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

    fn recovered(record: JobRecord, state: CommandJobState, store: JobStore) -> Arc<Self> {
        let now_ms = unix_now_ms();
        let was_running = record.state == CommandJobState::Running;
        let final_elapsed_ms = Some(
            record
                .elapsed_ms
                .unwrap_or_else(|| now_ms.saturating_sub(record.started_at_ms)),
        );
        let finished_at = if was_running {
            Instant::now()
        } else {
            let age = StdDuration::from_millis(
                now_ms.saturating_sub(record.finished_at_ms.unwrap_or(now_ms)),
            );
            Instant::now().checked_sub(age).unwrap_or_else(Instant::now)
        };
        let (cancel_tx, _) = watch::channel(false);
        Arc::new(Self {
            id: record.job_id,
            command: record.command,
            workspace_root: PathBuf::from(record.workspace_root),
            cwd: PathBuf::from(record.cwd),
            owner_session: record.owner_session,
            started_at_unix_ms: record.started_at_ms,
            timeout_ms: record.timeout_ms,
            abandon_after_ms: DEFAULT_ABANDON_AFTER_MS,
            change_session: None,
            store,
            runtime: Mutex::new(JobRuntime {
                state,
                exit_code: record.exit_code,
                finished_at: Some(finished_at),
                final_elapsed_ms,
                ..JobRuntime::default()
            }),
            last_poll: std::sync::Mutex::new(Instant::now()),
            changed: Notify::new(),
            cancel_tx,
        })
    }

    async fn append_output(&self, stream: &'static str, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let text = String::from_utf8_lossy(bytes).into_owned();
        let event_bytes = text.len();
        let mut runtime = self.runtime.lock().await;
        let seq = runtime.next_seq;
        runtime.next_seq = runtime.next_seq.saturating_add(1);
        runtime
            .events
            .push_back(CommandOutputEvent { seq, stream, text });
        runtime.retained_output_bytes = runtime.retained_output_bytes.saturating_add(event_bytes);

        while runtime.retained_output_bytes > MAX_OUTPUT_BYTES_PER_JOB {
            let Some(removed) = runtime.events.pop_front() else {
                break;
            };
            runtime.retained_output_bytes = runtime
                .retained_output_bytes
                .saturating_sub(removed.text.len());
            runtime.output_truncated = true;
        }
        drop(runtime);
        self.changed.notify_waiters();
    }

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

    async fn snapshot(&self, after: u64) -> CommandJobSnapshot {
        let runtime = self.runtime.lock().await;
        let first_retained_seq = runtime
            .events
            .front()
            .map(|event| event.seq)
            .unwrap_or(runtime.next_seq);
        let cursor_fell_behind = after.saturating_add(1) < first_retained_seq;
        let latest_cursor = runtime.next_seq.saturating_sub(1);
        let mut events = Vec::new();
        let mut response_bytes = 0usize;
        for event in runtime.events.iter().filter(|event| event.seq > after) {
            let event_bytes = event.text.len();
            if !events.is_empty()
                && response_bytes.saturating_add(event_bytes) > MAX_POLL_OUTPUT_BYTES
            {
                break;
            }
            response_bytes = response_bytes.saturating_add(event_bytes);
            events.push(event.clone());
        }
        let next_cursor = events
            .last()
            .map(|event| event.seq)
            .unwrap_or(latest_cursor);
        let has_more_output = next_cursor < latest_cursor;
        CommandJobSnapshot {
            job_id: self.id.clone(),
            command: self.command.clone(),
            cwd: self.cwd.to_string_lossy().into_owned(),
            state: runtime.state,
            elapsed_ms: runtime
                .final_elapsed_ms
                .unwrap_or_else(|| unix_now_ms().saturating_sub(self.started_at_unix_ms)),
            exit_code: runtime.exit_code,
            events,
            next_cursor,
            has_more_output,
            output_truncated: runtime.output_truncated || cursor_fell_behind,
            timeout_ms: self.timeout_ms,
        }
    }
}

#[derive(Default)]
struct ManagerState {
    jobs: HashMap<String, Arc<CommandJob>>,
    // Retry dedupe is intentionally short-lived. JSON-RPC request IDs are only
    // correlation IDs and may be reused later by a stateless client.
    request_jobs: HashMap<String, (String, Instant)>,
    last_cleanup: Option<Instant>,
}

#[derive(Clone)]
pub struct CommandJobManager {
    inner: Arc<RwLock<ManagerState>>,
    // Starting a job performs a dedupe lookup, active-job capacity check, and
    // registry insertion. Serialize that short critical section so concurrent
    // MCP requests cannot both pass the checks and create duplicate/overflow jobs.
    start_lock: Arc<Mutex<()>>,
    // App shutdown is terminal for this manager. Once set, no new background
    // command may be created even if an MCP request races with shutdown.
    shutting_down: Arc<AtomicBool>,
    // Foreground run_command and background start_command share this budget.
    // HTTP concurrency can stay high while the number of expensive process
    // trees remains bounded independently.
    process_budget: Arc<Semaphore>,
    process_limit: usize,
    abandon_after_ms: u64,
    store: JobStore,
    // Recovery is shared across clones and awaited by every concurrent first
    // request. Marking it complete before recover() finishes would let another
    // request observe an empty in-memory registry during startup.
    recovery: Arc<OnceCell<()>>,
}

impl Default for CommandJobManager {
    fn default() -> Self {
        Self::with_process_limit(MAX_ACTIVE_PROCESSES)
    }
}

impl CommandJobManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_store(dir: PathBuf) -> Self {
        Self {
            store: JobStore::open(dir),
            ..Self::with_process_limit(MAX_ACTIVE_PROCESSES)
        }
    }

    fn with_process_limit(limit: usize) -> Self {
        Self {
            inner: Arc::new(RwLock::new(ManagerState::default())),
            start_lock: Arc::new(Mutex::new(())),
            shutting_down: Arc::new(AtomicBool::new(false)),
            process_budget: Arc::new(Semaphore::new(limit)),
            process_limit: limit,
            abandon_after_ms: DEFAULT_ABANDON_AFTER_MS,
            store: JobStore::disabled(),
            recovery: Arc::new(OnceCell::new()),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_abandon_after_ms(mut self, ms: u64) -> Self {
        self.abandon_after_ms = ms;
        self
    }

    #[cfg(test)]
    pub(crate) fn new_with_process_limit(limit: usize) -> Self {
        Self::with_process_limit(limit)
    }

    pub(crate) fn try_acquire_process(&self) -> Result<OwnedSemaphorePermit, String> {
        self.process_budget
            .clone()
            .try_acquire_owned()
            .map_err(|_| {
                format!(
                    "too many active command processes; maximum combined foreground/background process trees is {}",
                    self.process_limit
                )
            })
    }

    pub fn normalize_timeout(timeout_ms: Option<u64>) -> Result<u64, String> {
        match timeout_ms {
            None => Ok(DEFAULT_JOB_TIMEOUT_MS),
            Some(0) => Err("timeout must be at least 1 ms".to_string()),
            Some(value) if value > MAX_JOB_TIMEOUT_MS => Err(format!(
                "timeout exceeds the maximum background command runtime of {MAX_JOB_TIMEOUT_MS} ms"
            )),
            Some(value) => Ok(value),
        }
    }

    #[cfg(test)]
    pub async fn start(
        &self,
        command: String,
        cwd: PathBuf,
        timeout_ms: u64,
        request_key: Option<String>,
    ) -> Result<StartCommandResult, String> {
        self.start_with_change_session(
            command,
            cwd.clone(),
            cwd,
            timeout_ms,
            request_key,
            None,
            None,
        )
        .await
    }

    pub async fn start_with_change_session(
        &self,
        command: String,
        workspace_root: PathBuf,
        cwd: PathBuf,
        timeout_ms: u64,
        request_key: Option<String>,
        change_session: Option<ChangeSession>,
        owner_session: Option<&str>,
    ) -> Result<StartCommandResult, String> {
        let _start_guard = self.start_lock.lock().await;
        if self.shutting_down.load(Ordering::Acquire) {
            return Err(
                "command job manager is shutting down; new commands are not accepted".to_string(),
            );
        }
        self.cleanup().await;

        if let Some(key) = request_key.as_deref() {
            let existing = {
                let manager = self.inner.read().await;
                manager
                    .request_jobs
                    .get(key)
                    .and_then(|(job_id, created_at)| {
                        if created_at.elapsed() <= IDEMPOTENCY_WINDOW {
                            manager.jobs.get(job_id).cloned()
                        } else {
                            None
                        }
                    })
            };
            if let Some(job) = existing {
                if job.command != command
                    || job.workspace_root != workspace_root
                    || job.cwd != cwd
                    || job.timeout_ms != timeout_ms
                {
                    return Err(
                        "the same MCP request id was reused with different start_command arguments"
                            .to_string(),
                    );
                }
                return Ok(StartCommandResult {
                    snapshot: job.snapshot(0).await,
                    deduplicated: true,
                });
            }
        }

        let active_count = {
            let jobs = {
                let manager = self.inner.read().await;
                manager.jobs.values().cloned().collect::<Vec<_>>()
            };
            let mut active = 0usize;
            for job in jobs {
                if job.runtime.lock().await.state == CommandJobState::Running {
                    active += 1;
                }
            }
            active
        };
        if active_count >= MAX_ACTIVE_JOBS {
            return Err(format!(
                "too many active command jobs ({active_count}); maximum is {MAX_ACTIVE_JOBS}. Poll or cancel an existing job before starting another"
            ));
        }

        let process_permit = self.try_acquire_process()?;
        let (job, cancel_rx) = CommandJob::new_with_change_session(
            command,
            workspace_root,
            cwd,
            timeout_ms,
            self.abandon_after_ms,
            change_session,
            owner_session.map(str::to_owned),
            self.store.clone(),
        );
        let job_id = job.id.clone();
        {
            let mut manager = self.inner.write().await;
            manager.jobs.insert(job_id.clone(), job.clone());
            if let Some(key) = request_key {
                manager
                    .request_jobs
                    .insert(key, (job_id.clone(), Instant::now()));
            }
        }

        job.persist().await;
        tokio::spawn(run_job(job.clone(), cancel_rx, Some(process_permit)));
        Ok(StartCommandResult {
            snapshot: job.snapshot(0).await,
            deduplicated: false,
        })
    }

    #[cfg(test)]
    pub async fn poll(
        &self,
        job_id: &str,
        after: u64,
        wait_ms: u64,
    ) -> Result<CommandJobSnapshot, String> {
        self.poll_for_session(job_id, after, wait_ms, None).await
    }

    pub async fn poll_for_session(
        &self,
        job_id: &str,
        after: u64,
        wait_ms: u64,
        owner_session: Option<&str>,
    ) -> Result<CommandJobSnapshot, String> {
        self.cleanup().await;
        let job = self.get_job_for_session(job_id, owner_session).await?;
        // Polling is the keep-alive heartbeat for a running job.
        job.mark_polled();
        let wait_ms = wait_ms.min(MAX_POLL_WAIT_MS);

        // `Notify::notified()` does not register with `notify_waiters()` until
        // the future is polled or explicitly enabled. Pin and enable it before
        // taking the snapshot so a change in the check/wait gap is retained.
        let notified = job.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let snapshot = job.snapshot(after).await;
        if snapshot.state.is_terminal() || !snapshot.events.is_empty() || wait_ms == 0 {
            return Ok(snapshot);
        }

        let _ = timeout(Duration::from_millis(wait_ms), &mut notified).await;
        Ok(job.snapshot(after).await)
    }

    #[cfg(test)]
    pub async fn current_changes(&self, job_id: &str) -> Result<Vec<FileChange>, String> {
        self.current_changes_for_session(job_id, None).await
    }

    pub async fn current_changes_for_session(
        &self,
        job_id: &str,
        owner_session: Option<&str>,
    ) -> Result<Vec<FileChange>, String> {
        self.cleanup().await;
        let job = self.get_job_for_session(job_id, owner_session).await?;
        Ok(job
            .change_session
            .as_ref()
            .map(ChangeSession::changes)
            .unwrap_or_default())
    }

    #[cfg(test)]
    pub async fn cancel(&self, job_id: &str) -> Result<CommandJobSnapshot, String> {
        self.cancel_for_session(job_id, None).await
    }

    pub async fn cancel_for_session(
        &self,
        job_id: &str,
        owner_session: Option<&str>,
    ) -> Result<CommandJobSnapshot, String> {
        self.cleanup().await;
        let job = self.get_job_for_session(job_id, owner_session).await?;

        let current = job.snapshot(0).await;
        if current.state.is_terminal() {
            return Ok(current);
        }
        let _ = job.cancel_tx.send(true);

        // Cancellation itself remains a short MCP operation, but ordinary
        // stdout/stderr notifications must not make cancel_command return a
        // misleading Running state. Wait until terminal or the bounded deadline.
        let deadline = Instant::now() + StdDuration::from_secs(5);
        loop {
            // Pin and explicitly enable the waiter before checking state so a
            // terminal transition cannot be lost in the check/wait gap.
            let notified = job.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let snapshot = job.snapshot(0).await;
            if snapshot.state.is_terminal() {
                return Ok(snapshot);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(snapshot);
            }
            if timeout(remaining, &mut notified).await.is_err() {
                return Ok(job.snapshot(0).await);
            }
        }
    }

    async fn get_job_for_session(
        &self,
        job_id: &str,
        owner_session: Option<&str>,
    ) -> Result<Arc<CommandJob>, String> {
        self.inner
            .read()
            .await
            .jobs
            .get(job_id)
            .filter(|job| job.owner_session.as_deref() == owner_session)
            .cloned()
            .ok_or_else(|| format!("unknown or expired command job: {job_id}"))
    }

    /// Signal every active command owned by one MCP session without waiting for
    /// process-tree teardown. DELETE /mcp stays responsive while the runners
    /// release their shared process permits asynchronously.
    pub async fn cancel_session(&self, owner_session: &str) -> usize {
        self.cleanup().await;
        let jobs = {
            let manager = self.inner.read().await;
            manager
                .jobs
                .values()
                .filter(|job| job.owner_session.as_deref() == Some(owner_session))
                .cloned()
                .collect::<Vec<_>>()
        };
        let mut cancelled = 0usize;
        for job in jobs {
            if job.runtime.lock().await.state == CommandJobState::Running {
                let _ = job.cancel_tx.send(true);
                cancelled += 1;
            }
        }
        cancelled
    }

    /// Cancel every command still owned by CatDesk and wait briefly for the
    /// runners to terminate their process trees. Used during application exit;
    /// ordinary MCP request completion deliberately does not call this.
    pub async fn cancel_all(&self) {
        // Serialize with start(): either a start completes before this guard and
        // is included below, or shutdown wins and that start is rejected.
        let _start_guard = self.start_lock.lock().await;
        self.shutting_down.store(true, Ordering::Release);

        let jobs = {
            let manager = self.inner.read().await;
            manager.jobs.values().cloned().collect::<Vec<_>>()
        };
        for job in &jobs {
            if job.runtime.lock().await.state == CommandJobState::Running {
                let _ = job.cancel_tx.send(true);
            }
        }

        let deadline = Instant::now() + StdDuration::from_secs(5);
        loop {
            let mut any_running = false;
            for job in &jobs {
                if job.runtime.lock().await.state == CommandJobState::Running {
                    any_running = true;
                    break;
                }
            }
            if !any_running || Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn recover(&self) {
        let records = self.store.read_all();
        if records.is_empty() {
            return;
        }

        let mut recovered_jobs = Vec::with_capacity(records.len());
        for record in records {
            let was_running = record.state == CommandJobState::Running;
            let state = if was_running {
                CommandJobState::Interrupted
            } else {
                record.state
            };
            let job = CommandJob::recovered(record, state, self.store.clone());
            if was_running {
                job.persist().await;
            }
            recovered_jobs.push(job);
        }

        let mut manager = self.inner.write().await;
        for job in recovered_jobs {
            manager.jobs.insert(job.id.clone(), job);
        }
    }

    pub async fn cleanup(&self) {
        self.recovery
            .get_or_init(|| async {
                self.recover().await;
            })
            .await;
        {
            let mut manager = self.inner.write().await;
            if manager
                .last_cleanup
                .is_some_and(|last| last.elapsed() < CLEANUP_INTERVAL)
            {
                return;
            }
            manager.last_cleanup = Some(Instant::now());
        }

        let jobs = {
            let manager = self.inner.read().await;
            manager
                .jobs
                .iter()
                .map(|(id, job)| (id.clone(), job.clone()))
                .collect::<Vec<_>>()
        };

        let mut expired = Vec::new();
        let mut terminal = Vec::new();
        for (id, job) in jobs {
            let runtime = job.runtime.lock().await;
            let Some(finished_at) = runtime.finished_at else {
                continue;
            };
            let age = finished_at.elapsed();
            if age >= TERMINAL_JOB_TTL {
                expired.push(id);
            } else {
                terminal.push((id, age, runtime.retained_output_bytes));
            }
        }

        // Oldest terminal jobs are the first eviction candidates both for the
        // retained-job count and for the global decoded-output memory budget.
        terminal.sort_by_key(|(_, age, _)| std::cmp::Reverse(*age));
        let retained_count = {
            let manager = self.inner.read().await;
            manager.jobs.len().saturating_sub(expired.len())
        };
        if retained_count > MAX_RETAINED_JOBS {
            let overflow = retained_count - MAX_RETAINED_JOBS;
            expired.extend(terminal.iter().take(overflow).map(|(id, _, _)| id.clone()));
        }

        let already_expired = expired.iter().cloned().collect::<HashSet<_>>();
        let mut terminal_output_bytes = terminal
            .iter()
            .filter(|(id, _, _)| !already_expired.contains(id))
            .map(|(_, _, bytes)| *bytes)
            .sum::<usize>();
        if terminal_output_bytes > MAX_TERMINAL_OUTPUT_BYTES {
            for (id, _, bytes) in &terminal {
                if terminal_output_bytes <= MAX_TERMINAL_OUTPUT_BYTES {
                    break;
                }
                if already_expired.contains(id) || expired.contains(id) {
                    continue;
                }
                expired.push(id.clone());
                terminal_output_bytes = terminal_output_bytes.saturating_sub(*bytes);
            }
        }

        let mut manager = self.inner.write().await;
        for id in &expired {
            manager.jobs.remove(id);
        }
        let live_job_ids = manager.jobs.keys().cloned().collect::<HashSet<_>>();
        manager.request_jobs.retain(|_, (job_id, created_at)| {
            created_at.elapsed() <= IDEMPOTENCY_WINDOW && live_job_ids.contains(job_id)
        });
        drop(manager);
        for id in &expired {
            self.store.remove(id);
        }
    }
}

fn decode_utf8_incremental(
    pending: &mut Vec<u8>,
    chunk: &[u8],
    end_of_stream: bool,
) -> Vec<String> {
    pending.extend_from_slice(chunk);
    let mut decoded = Vec::new();

    loop {
        match std::str::from_utf8(pending) {
            Ok(text) => {
                if !text.is_empty() {
                    decoded.push(text.to_string());
                }
                pending.clear();
                break;
            }
            Err(error) => {
                let valid_up_to = error.valid_up_to();
                if valid_up_to > 0 {
                    let text = String::from_utf8_lossy(&pending[..valid_up_to]).into_owned();
                    decoded.push(text);
                    pending.drain(..valid_up_to);
                    continue;
                }
                match error.error_len() {
                    Some(invalid_len) => {
                        decoded.push("�".to_string());
                        pending.drain(..invalid_len.min(pending.len()));
                    }
                    None => break, // incomplete UTF-8 sequence; keep it for the next read
                }
            }
        }
    }

    if end_of_stream && !pending.is_empty() {
        decoded.push(String::from_utf8_lossy(pending).into_owned());
        pending.clear();
    }
    decoded
}

async fn read_job_output<R>(job: Arc<CommandJob>, stream: &'static str, mut reader: R)
where
    R: AsyncRead + Unpin,
{
    let mut buffer = vec![0_u8; READ_CHUNK_BYTES];
    let mut pending_utf8 = Vec::with_capacity(4);
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => {
                for text in decode_utf8_incremental(&mut pending_utf8, &[], true) {
                    job.append_output(stream, text.as_bytes()).await;
                }
                break;
            }
            Ok(read) => {
                for text in decode_utf8_incremental(&mut pending_utf8, &buffer[..read], false) {
                    job.append_output(stream, text.as_bytes()).await;
                }
            }
            Err(error) => {
                for text in decode_utf8_incremental(&mut pending_utf8, &[], true) {
                    job.append_output(stream, text.as_bytes()).await;
                }
                job.append_output(
                    "stderr",
                    format!("CatDesk failed to read {stream}: {error}\n").as_bytes(),
                )
                .await;
                break;
            }
        }
    }
}

async fn run_job(
    job: Arc<CommandJob>,
    mut cancel_rx: watch::Receiver<bool>,
    _process_permit: Option<OwnedSemaphorePermit>,
) {
    let cancelled_before_spawn = *cancel_rx.borrow();
    if cancelled_before_spawn {
        job.finish(CommandJobState::Cancelled, Some(EXIT_CODE_CANCELLED))
            .await;
        return;
    }

    let mut process = match process_runner::spawn_shell_command(
        &job.command,
        &job.workspace_root,
        &job.cwd,
    )
    .await
    {
        Ok(process) => process,
        Err(error) => {
            job.append_output("stderr", format!("Failed to execute: {error}\n").as_bytes())
                .await;
            job.finish(CommandJobState::Failed, Some(EXIT_CODE_INTERNAL_ERROR))
                .await;
            return;
        }
    };

    let stdout_task = process
        .take_stdout()
        .map(|stdout| tokio::spawn(read_job_output(job.clone(), "stdout", stdout)));
    let stderr_task = process
        .take_stderr()
        .map(|stderr| tokio::spawn(read_job_output(job.clone(), "stderr", stderr)));

    enum Completion {
        Exited(std::io::Result<std::process::ExitStatus>),
        Cancelled,
        TimedOut,
        Abandoned,
    }

    let timeout_deadline =
        tokio::time::Instant::from_std(Instant::now() + StdDuration::from_millis(job.timeout_ms));
    let completion = loop {
        // The abandon deadline slides with every poll: a job lives as long as
        // its owner keeps asking about it.
        let idle_deadline = {
            let last_poll = *job.last_poll.lock().expect("last_poll mutex poisoned");
            tokio::time::Instant::from_std(
                last_poll + StdDuration::from_millis(job.abandon_after_ms),
            )
        };
        tokio::select! {
            status = process.wait() => break Completion::Exited(status),
            _ = cancel_rx.changed() => break Completion::Cancelled,
            _ = tokio::time::sleep_until(timeout_deadline) => break Completion::TimedOut,
            _ = tokio::time::sleep_until(idle_deadline) => {
                // A poll may have refreshed the deadline after it was captured;
                // only abandon once the quiet window has really elapsed.
                let last_poll = *job.last_poll.lock().expect("last_poll mutex poisoned");
                if last_poll + StdDuration::from_millis(job.abandon_after_ms) <= Instant::now() {
                    break Completion::Abandoned;
                }
            }
        }
    };

    let (state, exit_code) = match completion {
        Completion::Exited(Ok(status)) => {
            process.disarm().await;
            if status.success() {
                (
                    CommandJobState::Succeeded,
                    Some(exit_code_for_status(&status)),
                )
            } else {
                if let Some(signal) = process_runner::exit_status_signal_diagnostic(&status) {
                    job.append_output("stderr", format!("{signal}\n").as_bytes())
                        .await;
                }
                (CommandJobState::Failed, Some(exit_code_for_status(&status)))
            }
        }
        Completion::Exited(Err(error)) => {
            process.terminate_tree().await;
            let _ = process.wait().await;
            job.append_output(
                "stderr",
                format!("CatDesk failed while waiting for command: {error}\n").as_bytes(),
            )
            .await;
            (CommandJobState::Failed, Some(EXIT_CODE_INTERNAL_ERROR))
        }
        Completion::Cancelled => {
            process.terminate_tree().await;
            let status = process.wait().await.ok();
            let _ = status;
            (CommandJobState::Cancelled, Some(EXIT_CODE_CANCELLED))
        }
        Completion::TimedOut => {
            process.terminate_tree().await;
            let status = process.wait().await.ok();
            job.append_output(
                "stderr",
                format!("Command timed out after {} ms\n", job.timeout_ms).as_bytes(),
            )
            .await;
            let _ = status;
            (CommandJobState::TimedOut, Some(EXIT_CODE_TIMEOUT))
        }
        Completion::Abandoned => {
            process.terminate_tree().await;
            let status = process.wait().await.ok();
            job.append_output(
                "stderr",
                format!(
                    "CatDesk abandoned this job: no poll for {} ms\n",
                    job.abandon_after_ms
                )
                .as_bytes(),
            )
            .await;
            let _ = status;
            (CommandJobState::Abandoned, Some(EXIT_CODE_ABANDONED))
        }
    };

    if let Some(task) = stdout_task {
        let _ = task.await;
    }
    if let Some(task) = stderr_task {
        let _ = task.await;
    }
    job.finish(state, exit_code).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("catdesk-jobs-{name}-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create test workspace");
        path
    }

    async fn wait_terminal(manager: &CommandJobManager, job_id: &str) -> CommandJobSnapshot {
        let mut cursor = 0;
        for _ in 0..30 {
            let snapshot = manager.poll(job_id, cursor, 250).await.expect("poll job");
            cursor = snapshot.next_cursor;
            if snapshot.state.is_terminal() {
                // Fetch from zero once terminal so callers that assert on output
                // see the complete retained log rather than only the final delta.
                return manager.poll(job_id, 0, 0).await.expect("read terminal job");
            }
        }
        panic!("job did not reach terminal state");
    }

    async fn wait_for_file(path: &std::path::Path) {
        let deadline = Instant::now() + StdDuration::from_secs(5);
        while !path.exists() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            path.exists(),
            "command never reached ready state: {}",
            path.display()
        );
    }

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

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_first_polls_wait_for_recovery_to_finish() {
        use std::io::Write;

        let dir = workspace("recovery-race");
        let job_id = "recovery-race-job".to_string();
        let fifo = dir.join(format!("{job_id}.json"));
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("run mkfifo");
        assert!(
            status.success(),
            "mkfifo must be available for the race fixture"
        );

        let record = JobRecord {
            schema_version: 1,
            job_id: job_id.clone(),
            command: "sleep 1".into(),
            cwd: dir.to_string_lossy().into_owned(),
            workspace_root: dir.to_string_lossy().into_owned(),
            owner_session: None,
            timeout_ms: 60_000,
            started_at_ms: unix_now_ms().saturating_sub(1_000),
            state: CommandJobState::Running,
            exit_code: None,
            finished_at_ms: None,
            elapsed_ms: None,
        };
        let payload = serde_json::to_vec(&record).expect("serialize fixture");
        let (reader_open_tx, reader_open_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .open(&fifo)
                .expect("open fifo writer");
            reader_open_tx.send(()).expect("announce recovery read");
            release_rx
                .recv_timeout(StdDuration::from_secs(2))
                .expect("release fifo writer");
            file.write_all(&payload).expect("write persisted job");
        });

        let manager = CommandJobManager::with_store(dir.clone());
        let first_manager = manager.clone();
        let first_job_id = job_id.clone();
        let first = tokio::spawn(async move { first_manager.poll(&first_job_id, 0, 0).await });

        tokio::task::spawn_blocking(move || {
            reader_open_rx
                .recv_timeout(StdDuration::from_secs(2))
                .expect("first poll never started recovery")
        })
        .await
        .expect("wait for recovery read");

        let second_manager = manager.clone();
        let second_job_id = job_id.clone();
        let mut second =
            tokio::spawn(async move { second_manager.poll(&second_job_id, 0, 0).await });
        let premature = tokio::time::timeout(Duration::from_millis(100), &mut second).await;
        let completed_prematurely = premature.is_ok();

        release_tx.send(()).expect("release recovery read");
        let first_snapshot = first.await.expect("first poll task").expect("first poll");
        let second_result = match premature {
            Ok(joined) => joined.expect("second poll task"),
            Err(_) => second.await.expect("second poll task"),
        };
        writer.join().expect("fifo writer");

        assert!(
            !completed_prematurely,
            "a concurrent first poll bypassed in-progress recovery"
        );
        let second_snapshot = second_result.expect("second poll after recovery");
        assert_eq!(first_snapshot.state, CommandJobState::Interrupted);
        assert_eq!(second_snapshot.state, CommandJobState::Interrupted);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn recovery_marks_running_records_interrupted_and_rewrites_them() {
        let (manager, dir) = manager_with_store();
        let started = manager
            .start_with_change_session(
                "sleep 30".into(),
                dir.clone(),
                dir.clone(),
                60_000,
                None,
                None,
                Some("session-a"),
            )
            .await
            .expect("start job");

        let restarted = CommandJobManager::with_store(dir.clone());
        let snapshot = restarted
            .poll_for_session(&started.snapshot.job_id, 0, 0, Some("session-a"))
            .await
            .expect("poll recovered job");
        assert_eq!(snapshot.state, CommandJobState::Interrupted);
        assert_eq!(snapshot.exit_code, None);
        assert!(snapshot.events.is_empty());
        assert!(!snapshot.has_more_output);

        let records = restarted.store.read_all();
        assert_eq!(records[0].state, CommandJobState::Interrupted);

        let cross_session = restarted
            .poll_for_session(&started.snapshot.job_id, 0, 0, Some("session-b"))
            .await;
        assert!(
            cross_session.is_err(),
            "recovery must preserve job ownership"
        );

        let _ = manager
            .cancel_for_session(&started.snapshot.job_id, Some("session-a"))
            .await;
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
        assert!(snapshot.events.is_empty());
        assert!(!snapshot.has_more_output);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn recovery_restores_abandoned_records_as_pollable() {
        let root = workspace("abandon-recovery");
        let (manager, dir) = manager_with_store();
        let manager = manager.with_abandon_after_ms(150);
        let started = manager
            .start("sleep 3".into(), root.clone(), 10_000, None)
            .await
            .expect("start job");

        // Wait for the abandon decision without polling the job itself.
        let deadline = Instant::now() + StdDuration::from_secs(5);
        loop {
            if manager
                .store
                .read_all()
                .iter()
                .any(|record| record.state == CommandJobState::Abandoned)
            {
                break;
            }
            assert!(Instant::now() < deadline, "job was never abandoned");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let restarted = CommandJobManager::with_store(dir.clone());
        let snapshot = restarted
            .poll(&started.snapshot.job_id, 0, 0)
            .await
            .expect("poll recovered abandoned job");
        assert_eq!(snapshot.state, CommandJobState::Abandoned);
        assert_eq!(snapshot.exit_code, Some(EXIT_CODE_ABANDONED));
        assert!(snapshot.events.is_empty());
        assert!(!snapshot.has_more_output);
        let _ = std::fs::remove_dir_all(root);
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
            let manager_state = manager.inner.read().await;
            let job = manager_state
                .jobs
                .get(&job_id)
                .expect("job present")
                .clone();
            drop(manager_state);
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
        let _started = manager
            .start("sleep 5".into(), dir.clone(), 60_000, None)
            .await
            .expect("start job");
        manager.cancel_all().await;
        let records = manager.store.read_all();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].state, CommandJobState::Cancelled);
        assert_eq!(records[0].exit_code, Some(EXIT_CODE_CANCELLED));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn background_job_returns_immediately_and_completes() {
        let root = workspace("complete");
        let manager = CommandJobManager::new();
        let command = if cfg!(windows) {
            "Start-Sleep -Milliseconds 300; Write-Output done"
        } else {
            "sleep 0.3; printf 'done\\n'"
        };
        let started = Instant::now();
        let started_job = manager
            .start(command.to_string(), root.clone(), 5_000, None)
            .await
            .expect("start job");
        assert!(started.elapsed() < StdDuration::from_millis(250));
        assert_eq!(started_job.snapshot.state, CommandJobState::Running);

        let snapshot = wait_terminal(&manager, &started_job.snapshot.job_id).await;
        assert_eq!(snapshot.state, CommandJobState::Succeeded);
        let text = snapshot
            .events
            .iter()
            .map(|event| event.text.as_str())
            .collect::<String>();
        assert!(text.contains("done"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn polling_is_incremental_by_cursor() {
        let root = workspace("cursor");
        let manager = CommandJobManager::new();
        let command = if cfg!(windows) {
            "Write-Output first; Start-Sleep -Milliseconds 250; Write-Output second"
        } else {
            "printf 'first\\n'; sleep 0.25; printf 'second\\n'"
        };
        let started = manager
            .start(command.to_string(), root.clone(), 5_000, None)
            .await
            .expect("start job");

        let deadline = Instant::now() + StdDuration::from_secs(5);
        let first = loop {
            let snapshot = manager
                .poll(&started.snapshot.job_id, 0, 250)
                .await
                .expect("first poll");
            if !snapshot.events.is_empty() {
                break snapshot;
            }
            assert!(
                !snapshot.state.is_terminal(),
                "job completed without producing expected first output"
            );
            assert!(
                Instant::now() < deadline,
                "timed out waiting for first output"
            );
        };

        let first_cursor = first.next_cursor;
        let deadline = Instant::now() + StdDuration::from_secs(5);
        let second = loop {
            let snapshot = manager
                .poll(&started.snapshot.job_id, first_cursor, 250)
                .await
                .expect("second poll");
            if !snapshot.events.is_empty() || snapshot.state.is_terminal() {
                break snapshot;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for second output"
            );
        };
        assert!(
            second.events.iter().all(|event| event.seq > first_cursor),
            "incremental poll repeated an already-consumed event"
        );
        let _ = wait_terminal(&manager, &started.snapshot.job_id).await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cancellation_prevents_later_side_effects() {
        let root = workspace("cancel");
        let ready = root.join("ready.txt");
        let sentinel = root.join("sentinel.txt");
        let manager = CommandJobManager::new();
        let command = if cfg!(windows) {
            "Set-Content ready.txt ready; Start-Sleep -Seconds 3; Set-Content sentinel.txt survived"
        } else {
            "printf ready > ready.txt; sleep 3; printf survived > sentinel.txt"
        };
        let started = manager
            .start(command.to_string(), root.clone(), 10_000, None)
            .await
            .expect("start job");
        wait_for_file(&ready).await;
        let cancelled = manager
            .cancel(&started.snapshot.job_id)
            .await
            .expect("cancel job");
        assert!(matches!(
            cancelled.state,
            CommandJobState::Cancelled | CommandJobState::Running
        ));
        let terminal = wait_terminal(&manager, &started.snapshot.job_id).await;
        assert_eq!(terminal.state, CommandJobState::Cancelled);
        assert_eq!(terminal.exit_code, Some(EXIT_CODE_CANCELLED));
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert!(
            !sentinel.exists(),
            "cancelled process survived and wrote sentinel"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn pre_cancelled_job_does_not_spawn_command() {
        let root = workspace("pre-cancel");
        let sentinel = root.join("sentinel.txt");
        let command = if cfg!(windows) {
            "Set-Content sentinel.txt spawned; Start-Sleep -Seconds 2"
        } else {
            "printf spawned > sentinel.txt; sleep 2"
        };
        let (job, cancel_rx) = CommandJob::new(command.to_string(), root.clone(), 10_000);

        let _ = job.cancel_tx.send(true);
        run_job(job.clone(), cancel_rx, None).await;

        let snapshot = job.snapshot(0).await;
        assert_eq!(snapshot.state, CommandJobState::Cancelled);
        assert_eq!(snapshot.exit_code, Some(EXIT_CODE_CANCELLED));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !sentinel.exists(),
            "a job cancelled before the runner started still spawned its shell"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cancel_waits_for_terminal_state_despite_output_notifications() {
        let root = workspace("cancel-terminal");
        let ready = root.join("ready.txt");
        let manager = CommandJobManager::new();
        let command = if cfg!(windows) {
            "Set-Content ready.txt ready; 1..200 | ForEach-Object { Write-Output $_; Start-Sleep -Milliseconds 5 }; Start-Sleep -Seconds 3"
        } else {
            "printf ready > ready.txt; for i in $(seq 1 200); do printf '%s\\n' \"$i\"; sleep 0.005; done; sleep 3"
        };
        let started = manager
            .start(command.to_string(), root.clone(), 10_000, None)
            .await
            .expect("start noisy job");
        wait_for_file(&ready).await;
        let cancelled = manager
            .cancel(&started.snapshot.job_id)
            .await
            .expect("cancel noisy job");
        assert_eq!(
            cancelled.state,
            CommandJobState::Cancelled,
            "cancel_command should wait past output notifications for terminal acknowledgement"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cancellation_terminates_descendant_process_tree() {
        let root = workspace("descendant-cancel");
        let sentinel = root.join("descendant.txt");
        let manager = CommandJobManager::new();
        let command = if cfg!(windows) {
            "Start-Process powershell.exe -ArgumentList '-NoProfile','-Command','Start-Sleep -Milliseconds 800; Set-Content -Path descendant.txt -Value survived' -WorkingDirectory .; Start-Sleep -Seconds 5"
        } else {
            "(sleep 0.8; printf survived > descendant.txt) & sleep 5"
        };
        let started = manager
            .start(command.to_string(), root.clone(), 5_000, None)
            .await
            .expect("start descendant job");
        tokio::time::sleep(Duration::from_millis(150)).await;
        let _ = manager
            .cancel(&started.snapshot.job_id)
            .await
            .expect("cancel descendant job");
        let terminal = wait_terminal(&manager, &started.snapshot.job_id).await;
        assert_eq!(terminal.state, CommandJobState::Cancelled);
        tokio::time::sleep(Duration::from_millis(1_000)).await;
        assert!(
            !sentinel.exists(),
            "cancelled root shell left a descendant process alive"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn signalled_background_job_reports_conventional_exit_code_and_signal() {
        let root = workspace("signal-diagnostic");
        let manager = CommandJobManager::new();
        let started = manager
            .start("kill -KILL $$".to_string(), root.clone(), 5_000, None)
            .await
            .expect("start signalled job");
        let terminal = wait_terminal(&manager, &started.snapshot.job_id).await;
        assert_eq!(terminal.state, CommandJobState::Failed);
        assert_eq!(terminal.exit_code, Some(137));
        let text = terminal
            .events
            .iter()
            .map(|event| event.text.as_str())
            .collect::<String>();
        assert!(
            text.contains("SIGKILL") || text.contains("signal 9"),
            "missing signal diagnostic: {text:?}"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn job_timeout_terminates_process_tree() {
        let root = workspace("timeout");
        let sentinel = root.join("sentinel.txt");
        let manager = CommandJobManager::new();
        let command = if cfg!(windows) {
            "Start-Sleep -Milliseconds 800; Set-Content sentinel.txt survived"
        } else {
            "sleep 0.8; printf survived > sentinel.txt"
        };
        let started = manager
            .start(command.to_string(), root.clone(), 100, None)
            .await
            .expect("start job");
        let terminal = wait_terminal(&manager, &started.snapshot.job_id).await;
        assert_eq!(terminal.state, CommandJobState::TimedOut);
        assert_eq!(terminal.exit_code, Some(EXIT_CODE_TIMEOUT));
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert!(
            !sentinel.exists(),
            "timed-out job survived and wrote sentinel"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn idle_job_without_polls_is_abandoned_and_terminates_process_tree() {
        let root = workspace("abandoned");
        let sentinel = root.join("sentinel.txt");
        let (manager, store_dir) = manager_with_store();
        let manager = manager.with_abandon_after_ms(150);
        let command = if cfg!(windows) {
            "Start-Sleep -Milliseconds 2000; Set-Content sentinel.txt survived"
        } else {
            "sleep 2; printf survived > sentinel.txt"
        };
        let started = manager
            .start(command.to_string(), root.clone(), 10_000, None)
            .await
            .expect("start job");

        // Observe through the persisted record only: polling the job would
        // itself count as agent interest once heartbeat refresh exists.
        let deadline = Instant::now() + StdDuration::from_secs(5);
        loop {
            let abandoned = manager
                .store
                .read_all()
                .iter()
                .any(|record| record.state == CommandJobState::Abandoned);
            if abandoned {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "job was not abandoned without polls"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // A poll after the terminal transition must explain why the job ended.
        let terminal = manager
            .poll(&started.snapshot.job_id, 0, 0)
            .await
            .expect("poll abandoned job");
        assert_eq!(terminal.state, CommandJobState::Abandoned);
        assert_eq!(terminal.exit_code, Some(EXIT_CODE_ABANDONED));
        let events_text = terminal
            .events
            .iter()
            .map(|event| event.text.as_str())
            .collect::<String>();
        assert!(
            events_text.contains("no poll"),
            "expected abandon reason in output, got: {events_text}"
        );

        tokio::time::sleep(Duration::from_millis(2_100)).await;
        assert!(
            !sentinel.exists(),
            "abandoned job survived and wrote sentinel"
        );
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    #[tokio::test]
    async fn poll_refreshes_the_abandon_deadline() {
        let root = workspace("abandon-refresh");
        let (manager, store_dir) = manager_with_store();
        let manager = manager.with_abandon_after_ms(300);
        let command = if cfg!(windows) {
            "Start-Sleep -Seconds 3"
        } else {
            "sleep 3"
        };
        let started = manager
            .start(command.to_string(), root.clone(), 10_000, None)
            .await
            .expect("start job");

        // A poll halfway through the first quiet window must push the abandon
        // deadline out; the job still runs 100 ms after the original deadline.
        tokio::time::sleep(Duration::from_millis(150)).await;
        manager
            .poll(&started.snapshot.job_id, 0, 0)
            .await
            .expect("poll running job");
        tokio::time::sleep(Duration::from_millis(250)).await;
        let records = manager.store.read_all();
        assert_eq!(
            records[0].state,
            CommandJobState::Running,
            "poll did not extend the abandon deadline"
        );

        // Silence from here on: the job must still be reaped relative to the
        // refreshed deadline, not the original one.
        let deadline = Instant::now() + StdDuration::from_secs(5);
        loop {
            let state = manager.store.read_all()[0].state;
            if state == CommandJobState::Abandoned {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "job was not abandoned after the refreshed deadline passed"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let _ = manager.cancel(&started.snapshot.job_id).await;
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    #[tokio::test]
    async fn regular_polling_keeps_a_job_alive_until_it_finishes() {
        let root = workspace("abandon-heartbeat");
        let manager = CommandJobManager::new().with_abandon_after_ms(200);
        let command = if cfg!(windows) {
            "Start-Sleep -Milliseconds 1200"
        } else {
            "sleep 1.2"
        };
        let started = manager
            .start(command.to_string(), root.clone(), 10_000, None)
            .await
            .expect("start job");

        // Heartbeat: poll far more often than the abandon window.
        let mut cursor = 0;
        let terminal = loop {
            tokio::time::sleep(Duration::from_millis(80)).await;
            let snapshot = manager
                .poll(&started.snapshot.job_id, cursor, 0)
                .await
                .expect("poll job");
            cursor = snapshot.next_cursor;
            if snapshot.state.is_terminal() {
                break snapshot;
            }
        };
        assert_eq!(
            terminal.state,
            CommandJobState::Succeeded,
            "heartbeat polling must keep a needed job alive"
        );
        assert_eq!(terminal.exit_code, Some(0));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn incremental_utf8_decoder_preserves_split_multibyte_characters() {
        let bytes = "build ✓ 🚀".as_bytes();
        let split = bytes.len() - 2;
        let mut pending = Vec::new();
        let first = decode_utf8_incremental(&mut pending, &bytes[..split], false);
        let second = decode_utf8_incremental(&mut pending, &bytes[split..], false);
        let final_chunk = decode_utf8_incremental(&mut pending, &[], true);
        let decoded = first
            .into_iter()
            .chain(second)
            .chain(final_chunk)
            .collect::<String>();
        assert_eq!(decoded, "build ✓ 🚀");
        assert!(pending.is_empty());
    }

    #[tokio::test]
    async fn cancel_session_signals_only_jobs_owned_by_that_session() {
        let root = workspace("cancel-session");
        let manager = CommandJobManager::new();
        let command = if cfg!(windows) {
            "Start-Sleep -Seconds 5"
        } else {
            "sleep 5"
        };

        let first = manager
            .start_with_change_session(
                command.to_string(),
                root.clone(),
                root.clone(),
                10_000,
                None,
                None,
                Some("session-a"),
            )
            .await
            .expect("start session-a job");
        let second = manager
            .start_with_change_session(
                command.to_string(),
                root.clone(),
                root.clone(),
                10_000,
                None,
                None,
                Some("session-b"),
            )
            .await
            .expect("start session-b job");

        assert_eq!(manager.cancel_session("session-a").await, 1);
        let first_terminal = loop {
            let snapshot = manager
                .poll_for_session(&first.snapshot.job_id, 0, 100, Some("session-a"))
                .await
                .expect("poll session-a job");
            if snapshot.state.is_terminal() {
                break snapshot;
            }
        };
        assert_eq!(first_terminal.state, CommandJobState::Cancelled);

        let second_snapshot = manager
            .poll_for_session(&second.snapshot.job_id, 0, 0, Some("session-b"))
            .await
            .expect("poll session-b job");
        assert_eq!(second_snapshot.state, CommandJobState::Running);
        assert!(
            manager
                .poll_for_session(&second.snapshot.job_id, 0, 0, Some("session-a"))
                .await
                .is_err(),
            "another session must not observe the surviving job"
        );

        assert_eq!(manager.cancel_session("session-b").await, 1);
        let _ = manager.cancel_all().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cancel_all_terminates_active_jobs() {
        let root = workspace("cancel-all");
        let ready = root.join("ready.txt");
        let sentinel = root.join("sentinel.txt");
        let manager = CommandJobManager::new();
        let command = if cfg!(windows) {
            "Set-Content ready.txt ready; Start-Sleep -Seconds 3; Set-Content sentinel.txt survived"
        } else {
            "printf ready > ready.txt; sleep 3; printf survived > sentinel.txt"
        };
        let started = manager
            .start(command.to_string(), root.clone(), 10_000, None)
            .await
            .expect("start job");
        wait_for_file(&ready).await;
        manager.cancel_all().await;
        let terminal = manager
            .poll(&started.snapshot.job_id, 0, 0)
            .await
            .expect("poll cancelled job");
        assert_eq!(terminal.state, CommandJobState::Cancelled);
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert!(
            !sentinel.exists(),
            "shutdown cancellation left process alive"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cancel_all_permanently_rejects_future_starts() {
        let root = workspace("shutdown-reject");
        let manager = CommandJobManager::new();
        manager.cancel_all().await;

        let error = manager
            .start("echo should-not-run".to_string(), root.clone(), 5_000, None)
            .await
            .expect_err("shutdown manager must reject new jobs");
        assert!(error.contains("shutting down"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn start_racing_with_shutdown_cannot_escape_cancellation() {
        use tokio::sync::Barrier;

        let root = workspace("shutdown-race");
        let sentinel = root.join("escaped.txt");
        let manager = CommandJobManager::new();
        let barrier = Arc::new(Barrier::new(3));
        let command = if cfg!(windows) {
            "Start-Sleep -Seconds 2; Set-Content escaped.txt survived"
        } else {
            "sleep 2; printf survived > escaped.txt"
        };

        let starter_manager = manager.clone();
        let starter_root = root.clone();
        let starter_barrier = barrier.clone();
        let starter = tokio::spawn(async move {
            starter_barrier.wait().await;
            starter_manager
                .start(command.to_string(), starter_root, 10_000, None)
                .await
        });

        let shutdown_manager = manager.clone();
        let shutdown_barrier = barrier.clone();
        let shutdown = tokio::spawn(async move {
            shutdown_barrier.wait().await;
            shutdown_manager.cancel_all().await;
        });

        barrier.wait().await;
        let start_result = starter.await.expect("starter task");
        shutdown.await.expect("shutdown task");

        match start_result {
            Ok(started) => {
                let terminal = wait_terminal(&manager, &started.snapshot.job_id).await;
                assert_eq!(terminal.state, CommandJobState::Cancelled);
            }
            Err(error) => assert!(error.contains("shutting down")),
        }

        tokio::time::sleep(Duration::from_millis(2_200)).await;
        assert!(
            !sentinel.exists(),
            "a start racing with shutdown escaped manager ownership"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cleanup_prunes_expired_idempotency_keys_without_job_eviction() {
        let root = workspace("dedupe-cleanup");
        let manager = CommandJobManager::new();
        let command = if cfg!(windows) {
            "Write-Output done"
        } else {
            "printf 'done\\n'"
        };
        let started = manager
            .start(
                command.to_string(),
                root.clone(),
                5_000,
                Some("expired-request-key".into()),
            )
            .await
            .expect("start job");
        let _ = wait_terminal(&manager, &started.snapshot.job_id).await;

        {
            let mut state = manager.inner.write().await;
            let entry = state
                .request_jobs
                .get_mut("expired-request-key")
                .expect("request key exists before cleanup");
            entry.1 = Instant::now() - IDEMPOTENCY_WINDOW - StdDuration::from_secs(1);
            state.last_cleanup = None;
            assert!(state.jobs.contains_key(&started.snapshot.job_id));
        }

        manager.cleanup().await;
        let state = manager.inner.read().await;
        assert!(
            !state.request_jobs.contains_key("expired-request-key"),
            "expired idempotency metadata must be pruned even when no job is evicted"
        );
        assert!(
            state.jobs.contains_key(&started.snapshot.job_id),
            "cleanup should retain the still-fresh terminal job"
        );
        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn duplicate_request_key_reuses_existing_job() {
        let root = workspace("dedup");
        let manager = CommandJobManager::new();
        let command = if cfg!(windows) {
            "Start-Sleep -Milliseconds 300"
        } else {
            "sleep 0.3"
        };
        let first = manager
            .start(
                command.to_string(),
                root.clone(),
                5_000,
                Some("request-1".into()),
            )
            .await
            .expect("start first job");
        let second = manager
            .start(
                command.to_string(),
                root.clone(),
                5_000,
                Some("request-1".into()),
            )
            .await
            .expect("deduplicate job");
        assert!(!first.deduplicated);
        assert!(second.deduplicated);
        assert_eq!(first.snapshot.job_id, second.snapshot.job_id);
        let _ = manager.cancel(&first.snapshot.job_id).await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn interrupted_state_serializes_snake_case_and_roundtrips() {
        assert_eq!(
            serde_json::to_string(&CommandJobState::Interrupted).expect("serialize"),
            "\"interrupted\""
        );
        let state: CommandJobState = serde_json::from_str("\"interrupted\"").expect("deserialize");
        assert!(state.is_terminal());
    }

    #[test]
    fn abandoned_state_serializes_snake_case_and_roundtrips() {
        assert_eq!(
            serde_json::to_string(&CommandJobState::Abandoned).expect("serialize"),
            "\"abandoned\""
        );
        let state: CommandJobState = serde_json::from_str("\"abandoned\"").expect("deserialize");
        assert!(state.is_terminal());
    }

    #[test]
    fn background_timeout_validation_covers_boundaries() {
        assert_eq!(DEFAULT_JOB_TIMEOUT_MS, 2 * 60 * 60 * 1_000);
        assert_eq!(
            CommandJobManager::normalize_timeout(None).expect("default timeout"),
            DEFAULT_JOB_TIMEOUT_MS
        );
        assert!(CommandJobManager::normalize_timeout(Some(0)).is_err());
        assert_eq!(
            CommandJobManager::normalize_timeout(Some(1)).expect("minimum timeout"),
            1
        );
        assert_eq!(
            CommandJobManager::normalize_timeout(Some(MAX_JOB_TIMEOUT_MS))
                .expect("maximum timeout"),
            MAX_JOB_TIMEOUT_MS
        );
        assert!(CommandJobManager::normalize_timeout(Some(MAX_JOB_TIMEOUT_MS + 1)).is_err());
    }

    #[tokio::test]
    async fn shared_process_budget_is_held_by_background_jobs_until_termination() {
        let root = workspace("process-budget");
        let manager = CommandJobManager::with_process_limit(1);
        let command = if cfg!(windows) {
            "Start-Sleep -Seconds 5"
        } else {
            "sleep 5"
        };
        let started = manager
            .start(command.to_string(), root.clone(), 10_000, None)
            .await
            .expect("start background command");

        let busy = manager.try_acquire_process();
        assert!(
            busy.is_err(),
            "background job must consume shared process capacity"
        );

        manager
            .cancel(&started.snapshot.job_id)
            .await
            .expect("cancel background command");
        let _ = wait_terminal(&manager, &started.snapshot.job_id).await;

        let permit = tokio::time::timeout(StdDuration::from_secs(2), async {
            loop {
                if let Ok(permit) = manager.try_acquire_process() {
                    break permit;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("process capacity did not recover");
        drop(permit);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn active_job_limit_is_enforced_and_recovers_after_cancel() {
        let root = workspace("capacity");
        let manager = CommandJobManager::new();
        let command = if cfg!(windows) {
            "Start-Sleep -Seconds 5"
        } else {
            "sleep 5"
        };
        let mut ids = Vec::new();
        for _ in 0..MAX_ACTIVE_JOBS {
            let started = manager
                .start(command.to_string(), root.clone(), 10_000, None)
                .await
                .expect("start capacity job");
            ids.push(started.snapshot.job_id);
        }

        let overflow = manager
            .start(command.to_string(), root.clone(), 10_000, None)
            .await
            .expect_err("ninth active job must be rejected");
        assert!(overflow.contains("too many active command jobs"));

        manager.cancel(&ids[0]).await.expect("cancel one job");
        let _ = wait_terminal(&manager, &ids[0]).await;
        let replacement = manager
            .start(command.to_string(), root.clone(), 10_000, None)
            .await
            .expect("capacity should recover after cancellation");
        ids.push(replacement.snapshot.job_id);
        manager.cancel_all().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn oversized_output_is_bounded_and_marks_old_cursor_truncated() {
        let root = workspace("output-limit");
        let (job, _cancel_rx) = CommandJob::new("synthetic".into(), root.clone(), 5_000);
        let chunk = vec![b'x'; READ_CHUNK_BYTES];
        let chunks = (MAX_OUTPUT_BYTES_PER_JOB / READ_CHUNK_BYTES) + 8;
        for _ in 0..chunks {
            job.append_output("stdout", &chunk).await;
        }

        let snapshot = job.snapshot(0).await;
        let retained = snapshot
            .events
            .iter()
            .map(|event| event.text.len())
            .sum::<usize>();
        assert!(retained <= MAX_OUTPUT_BYTES_PER_JOB);
        assert!(snapshot.output_truncated);
        assert!(snapshot.events.first().is_some_and(|event| event.seq > 1));
        assert_eq!(
            snapshot.next_cursor,
            snapshot.events.last().expect("retained poll events").seq
        );
        assert!(snapshot.has_more_output);
        assert!(snapshot.next_cursor < chunks as u64);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn poll_output_is_bounded_and_cursor_drains_terminal_logs_without_gaps() {
        let root = workspace("poll-output-limit");
        let (job, _cancel_rx) = CommandJob::new("synthetic".into(), root.clone(), 5_000);
        let chunk = vec![b'x'; READ_CHUNK_BYTES];
        let chunks = 40usize;
        for _ in 0..chunks {
            job.append_output("stdout", &chunk).await;
        }
        job.finish(CommandJobState::Succeeded, Some(0)).await;

        let mut after = 0u64;
        let mut seen = Vec::new();
        let mut polls = 0usize;
        loop {
            let snapshot = job.snapshot(after).await;
            polls += 1;
            assert_eq!(snapshot.state, CommandJobState::Succeeded);
            let returned_bytes = snapshot
                .events
                .iter()
                .map(|event| event.text.len())
                .sum::<usize>();
            assert!(
                returned_bytes <= MAX_POLL_OUTPUT_BYTES,
                "poll returned {returned_bytes} bytes, limit is {MAX_POLL_OUTPUT_BYTES}"
            );
            for event in &snapshot.events {
                assert_eq!(event.seq, after + 1, "cursor skipped or repeated an event");
                after = event.seq;
                seen.push(event.seq);
            }
            assert_eq!(snapshot.next_cursor, after);
            if !snapshot.has_more_output {
                break;
            }
            assert!(
                !snapshot.events.is_empty(),
                "hasMoreOutput must make progress"
            );
        }

        assert!(
            polls > 1,
            "test must exercise multiple bounded poll responses"
        );
        assert_eq!(seen.len(), chunks);
        assert_eq!(seen, (1..=chunks as u64).collect::<Vec<_>>());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn unknown_job_ids_are_rejected_for_poll_and_cancel() {
        let manager = CommandJobManager::new();
        let poll_error = manager
            .poll("definitely-not-a-job", 0, 0)
            .await
            .expect_err("unknown poll must fail");
        assert!(poll_error.contains("unknown or expired command job"));
        let cancel_error = manager
            .cancel("definitely-not-a-job")
            .await
            .expect_err("unknown cancel must fail");
        assert!(cancel_error.contains("unknown or expired command job"));
    }

    #[tokio::test]
    async fn cancelling_terminal_job_is_idempotent() {
        let root = workspace("terminal-cancel");
        let manager = CommandJobManager::new();
        let command = if cfg!(windows) {
            "Write-Output done"
        } else {
            "printf 'done\\n'"
        };
        let started = manager
            .start(command.to_string(), root.clone(), 5_000, None)
            .await
            .expect("start job");
        let terminal = wait_terminal(&manager, &started.snapshot.job_id).await;
        assert_eq!(terminal.state, CommandJobState::Succeeded);
        let cancelled = manager
            .cancel(&started.snapshot.job_id)
            .await
            .expect("cancel terminal job");
        assert_eq!(cancelled.state, CommandJobState::Succeeded);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn poll_wait_returns_near_requested_deadline_when_nothing_changes() {
        let root = workspace("poll-wait");
        let manager = CommandJobManager::new();
        let command = if cfg!(windows) {
            "Start-Sleep -Milliseconds 500"
        } else {
            "sleep 0.5"
        };
        let started = manager
            .start(command.to_string(), root.clone(), 5_000, None)
            .await
            .expect("start job");
        let started_wait = Instant::now();
        let snapshot = manager
            .poll(&started.snapshot.job_id, 0, 100)
            .await
            .expect("poll job");
        let elapsed = started_wait.elapsed();
        assert_eq!(snapshot.state, CommandJobState::Running);
        assert!(snapshot.events.is_empty());
        assert!(
            elapsed >= StdDuration::from_millis(70),
            "poll returned too early: {elapsed:?}"
        );
        assert!(
            elapsed < StdDuration::from_millis(400),
            "poll waited too long: {elapsed:?}"
        );
        manager.cancel_all().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cleanup_enforces_global_terminal_output_budget() {
        let root = workspace("global-output-budget");
        let manager = CommandJobManager::new();

        for index in 0..9u64 {
            let (job, _cancel_rx) = CommandJob::new("synthetic".into(), root.clone(), 5_000);
            {
                let mut runtime = job.runtime.lock().await;
                runtime.state = CommandJobState::Succeeded;
                runtime.finished_at = Some(Instant::now() - StdDuration::from_millis(index));
                runtime.retained_output_bytes = MAX_OUTPUT_BYTES_PER_JOB;
            }
            manager.inner.write().await.jobs.insert(job.id.clone(), job);
        }

        manager.inner.write().await.last_cleanup = None;
        manager.cleanup().await;

        let jobs = {
            let state = manager.inner.read().await;
            state.jobs.values().cloned().collect::<Vec<_>>()
        };
        let mut retained_bytes = 0usize;
        for job in jobs {
            retained_bytes =
                retained_bytes.saturating_add(job.runtime.lock().await.retained_output_bytes);
        }
        assert!(retained_bytes <= MAX_TERMINAL_OUTPUT_BYTES);
        let _ = std::fs::remove_dir_all(root);
    }
}
