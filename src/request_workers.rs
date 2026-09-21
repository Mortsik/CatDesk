//! Keep synchronous filesystem/tool work off the async reactor while queuing temporary saturation fairly.
use crate::fair_queue::{FairAcquireError, FairGate, FairGateSnapshot, QueueBudget, SchedulingKey};
use std::{
    future::Future,
    sync::Arc,
    time::{Duration, Instant},
};

const CONTROL_QUEUE_BUDGET: usize = 512;
const HEAVY_QUEUE_BUDGET: usize = 8192;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RequestFailure {
    Busy,
    Deadline,
    Failed,
}

impl std::fmt::Display for RequestFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Busy => "CatDesk scheduler safety queue is full. No new work was started; retry after existing queued work drains.",
            Self::Deadline => "CatDesk response deadline exceeded. If execution had already started, the operation may still be running; do not repeat a write or command blindly. Check its result or poll the command job.",
            Self::Failed => "CatDesk request worker failed; inspect diagnostics before retrying.",
        })
    }
}

#[derive(Clone)]
pub(crate) struct RequestWorkers {
    gate: FairGate,
}

impl RequestWorkers {
    fn new(limit: usize, budget: Arc<QueueBudget>) -> Self {
        Self {
            gate: FairGate::new(limit, budget),
        }
    }

    async fn run<F>(
        &self,
        key: SchedulingKey,
        work: F,
        deadline: Duration,
    ) -> Result<F::Output, RequestFailure>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let absolute_deadline = Instant::now() + deadline;
        let permit =
            self.gate
                .acquire(key, absolute_deadline)
                .await
                .map_err(|error| match error {
                    FairAcquireError::Deadline => RequestFailure::Deadline,
                    FairAcquireError::Overloaded => RequestFailure::Busy,
                    FairAcquireError::Closed => RequestFailure::Failed,
                })?;
        let remaining = absolute_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            drop(permit);
            return Err(RequestFailure::Deadline);
        }

        let runtime = tokio::runtime::Handle::current();
        let task = tokio::task::spawn_blocking(move || {
            // Once synchronous work has started, a disconnected client or response timeout
            // cannot safely cancel it. Keep the physical slot until the real work ends.
            let _permit = permit;
            runtime.block_on(work)
        });
        match tokio::time::timeout(remaining, task).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(_)) => Err(RequestFailure::Failed),
            Err(_) => Err(RequestFailure::Deadline),
        }
    }

    fn snapshot(&self) -> FairGateSnapshot {
        self.gate.snapshot()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RequestClass {
    Control,
    Filesystem,
    Process,
    Browser,
    General,
}

impl RequestClass {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Control => "control",
            Self::Filesystem => "filesystem",
            Self::Process => "process",
            Self::Browser => "browser",
            Self::General => "general",
        }
    }
}

#[derive(Clone, Copy)]
struct RequestLimits {
    control: usize,
    filesystem: usize,
    process: usize,
    browser: usize,
    general: usize,
}

impl Default for RequestLimits {
    fn default() -> Self {
        Self {
            control: 8,
            filesystem: 16,
            process: 12,
            browser: 4,
            general: 8,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RequestSchedulerSnapshot {
    pub(crate) control: FairGateSnapshot,
    pub(crate) filesystem: FairGateSnapshot,
    pub(crate) process: FairGateSnapshot,
    pub(crate) browser: FairGateSnapshot,
    pub(crate) general: FairGateSnapshot,
}

#[derive(Clone)]
pub(crate) struct RequestScheduler {
    control: RequestWorkers,
    filesystem: RequestWorkers,
    process: RequestWorkers,
    browser: RequestWorkers,
    general: RequestWorkers,
}

impl RequestScheduler {
    pub(crate) fn new() -> Self {
        Self::with_limits(RequestLimits::default())
    }

    fn with_limits(limits: RequestLimits) -> Self {
        let control_budget = QueueBudget::new(CONTROL_QUEUE_BUDGET);
        let heavy_budget = QueueBudget::new(HEAVY_QUEUE_BUDGET);
        Self {
            control: RequestWorkers::new(limits.control, control_budget),
            filesystem: RequestWorkers::new(limits.filesystem, heavy_budget.clone()),
            process: RequestWorkers::new(limits.process, heavy_budget.clone()),
            browser: RequestWorkers::new(limits.browser, heavy_budget.clone()),
            general: RequestWorkers::new(limits.general, heavy_budget),
        }
    }

    fn workers(&self, class: RequestClass) -> &RequestWorkers {
        match class {
            RequestClass::Control => &self.control,
            RequestClass::Filesystem => &self.filesystem,
            RequestClass::Process => &self.process,
            RequestClass::Browser => &self.browser,
            RequestClass::General => &self.general,
        }
    }

    pub(crate) async fn run_keyed<F>(
        &self,
        class: RequestClass,
        key: SchedulingKey,
        work: F,
        deadline: Duration,
    ) -> Result<F::Output, RequestFailure>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.workers(class).run(key, work, deadline).await
    }

    pub(crate) fn snapshot(&self) -> RequestSchedulerSnapshot {
        RequestSchedulerSnapshot {
            control: self.control.snapshot(),
            filesystem: self.filesystem.snapshot(),
            process: self.process.snapshot(),
            browser: self.browser.snapshot(),
            general: self.general.snapshot(),
        }
    }
}

pub(crate) fn global_request_scheduler() -> &'static RequestScheduler {
    static SCHEDULER: std::sync::LazyLock<RequestScheduler> =
        std::sync::LazyLock::new(RequestScheduler::new);
    &SCHEDULER
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn key(session: &str) -> SchedulingKey {
        SchedulingKey::new(session, None)
    }

    async fn wait_until(mut check: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while !check() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("condition was not reached");
    }

    #[tokio::test]
    async fn synchronous_tool_does_not_starve_runtime_timer() {
        let workers = RequestWorkers::new(1, QueueBudget::new(32));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let watchdog_tx = release_tx.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(3));
            let _ = watchdog_tx.send(false);
        });

        let async_release = tokio::spawn(async move {
            started_rx.await.expect("synchronous fixture never started");
            tokio::time::sleep(Duration::from_millis(20)).await;
            let _ = release_tx.send(true);
        });

        let released_by_runtime = workers
            .run(
                key("session-a"),
                async move {
                    started_tx.send(()).unwrap();
                    release_rx
                        .recv_timeout(Duration::from_secs(4))
                        .expect("worker fixture was never released")
                },
                Duration::from_secs(5),
            )
            .await
            .expect("worker request failed");
        async_release.await.expect("runtime release task panicked");

        assert!(
            released_by_runtime,
            "synchronous work blocked the runtime timer until the watchdog intervened"
        );
    }

    #[tokio::test]
    async fn response_timeout_keeps_capacity_reserved_until_started_work_really_finishes() {
        let workers = RequestWorkers::new(1, QueueBudget::new(32));
        let (release, wait) = std::sync::mpsc::channel();
        let watchdog = release.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(2));
            let _ = watchdog.send(());
        });
        let result = workers
            .run(
                key("session-a"),
                async move {
                    wait.recv().unwrap();
                },
                Duration::from_millis(20),
            )
            .await;
        assert_eq!(result, Err(RequestFailure::Deadline));

        let queued_workers = workers.clone();
        let queued = tokio::spawn(async move {
            queued_workers
                .run(key("session-b"), async { 42 }, Duration::from_secs(1))
                .await
        });
        wait_until(|| workers.snapshot().queued == 1).await;
        assert!(
            !queued.is_finished(),
            "started timed-out work released capacity early"
        );
        let _ = release.send(());
        assert_eq!(queued.await.unwrap(), Ok(42));
    }

    #[tokio::test]
    async fn disconnected_client_cannot_release_a_running_workers_slot() {
        let workers = RequestWorkers::new(1, QueueBudget::new(32));
        let (release, wait) = std::sync::mpsc::channel();
        let (started, running) = tokio::sync::oneshot::channel();
        let running_workers = workers.clone();
        let caller = tokio::spawn(async move {
            running_workers
                .run(
                    key("session-a"),
                    async move {
                        started.send(()).unwrap();
                        let _ = wait.recv_timeout(Duration::from_secs(2));
                    },
                    Duration::from_secs(3),
                )
                .await
        });
        running.await.unwrap();
        caller.abort();
        let _ = caller.await;

        let queued_workers = workers.clone();
        let queued = tokio::spawn(async move {
            queued_workers
                .run(key("session-b"), async { 42 }, Duration::from_secs(1))
                .await
        });
        wait_until(|| workers.snapshot().queued == 1).await;
        let _ = release.send(());
        assert_eq!(queued.await.unwrap(), Ok(42));
    }

    #[tokio::test]
    async fn saturated_filesystem_work_waits_and_control_stays_responsive() {
        let scheduler = RequestScheduler::with_limits(RequestLimits {
            control: 1,
            filesystem: 1,
            process: 1,
            browser: 1,
            general: 1,
        });
        let (release, wait) = std::sync::mpsc::channel();
        let (started, running) = tokio::sync::oneshot::channel();
        let filesystem_scheduler = scheduler.clone();
        let filesystem = tokio::spawn(async move {
            filesystem_scheduler
                .run_keyed(
                    RequestClass::Filesystem,
                    key("session-a"),
                    async move {
                        started.send(()).unwrap();
                        let _ = wait.recv_timeout(Duration::from_secs(2));
                        7
                    },
                    Duration::from_secs(3),
                )
                .await
        });
        running.await.unwrap();

        let queued_scheduler = scheduler.clone();
        let second_filesystem = tokio::spawn(async move {
            queued_scheduler
                .run_keyed(
                    RequestClass::Filesystem,
                    key("session-b"),
                    async { 8 },
                    Duration::from_secs(2),
                )
                .await
        });
        wait_until(|| scheduler.snapshot().filesystem.queued == 1).await;
        assert!(!second_filesystem.is_finished());

        let control = scheduler
            .run_keyed(
                RequestClass::Control,
                key("session-c"),
                async { 42 },
                Duration::from_secs(1),
            )
            .await;
        assert_eq!(control, Ok(42));

        let _ = release.send(());
        assert_eq!(filesystem.await.unwrap(), Ok(7));
        assert_eq!(second_filesystem.await.unwrap(), Ok(8));
    }

    #[tokio::test]
    async fn queued_request_that_times_out_never_runs_later() {
        let scheduler = RequestScheduler::with_limits(RequestLimits {
            control: 1,
            filesystem: 1,
            process: 1,
            browser: 1,
            general: 1,
        });
        let (release, wait) = std::sync::mpsc::channel();
        let (started, running) = tokio::sync::oneshot::channel();
        let blocker_scheduler = scheduler.clone();
        let blocker = tokio::spawn(async move {
            blocker_scheduler
                .run_keyed(
                    RequestClass::Filesystem,
                    key("holder"),
                    async move {
                        started.send(()).unwrap();
                        let _ = wait.recv_timeout(Duration::from_secs(2));
                    },
                    Duration::from_secs(3),
                )
                .await
        });
        running.await.unwrap();

        let side_effect = Arc::new(AtomicBool::new(false));
        let side_effect_for_work = side_effect.clone();
        let result = scheduler
            .run_keyed(
                RequestClass::Filesystem,
                key("late"),
                async move {
                    side_effect_for_work.store(true, Ordering::SeqCst);
                },
                Duration::from_millis(20),
            )
            .await;
        assert_eq!(result, Err(RequestFailure::Deadline));
        assert_eq!(scheduler.snapshot().filesystem.queued, 0);

        let _ = release.send(());
        assert!(blocker.await.unwrap().is_ok());
        tokio::task::yield_now().await;
        assert!(!side_effect.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn aborted_caller_before_grant_never_runs_later() {
        let scheduler = RequestScheduler::with_limits(RequestLimits {
            control: 1,
            filesystem: 1,
            process: 1,
            browser: 1,
            general: 1,
        });
        let (release, wait) = std::sync::mpsc::channel();
        let watchdog = release.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(2));
            let _ = watchdog.send(());
        });
        let blocker_scheduler = scheduler.clone();
        let blocker = tokio::spawn(async move {
            blocker_scheduler
                .run_keyed(
                    RequestClass::Filesystem,
                    key("holder"),
                    async move {
                        let _ = wait.recv_timeout(Duration::from_secs(3));
                    },
                    Duration::from_secs(3),
                )
                .await
        });
        wait_until(|| scheduler.snapshot().filesystem.active == 1).await;

        let side_effect = Arc::new(AtomicBool::new(false));
        let queued_scheduler = scheduler.clone();
        let side_effect_for_work = side_effect.clone();
        let queued = tokio::spawn(async move {
            queued_scheduler
                .run_keyed(
                    RequestClass::Filesystem,
                    key("cancelled"),
                    async move {
                        side_effect_for_work.store(true, Ordering::SeqCst);
                    },
                    Duration::from_secs(2),
                )
                .await
        });
        wait_until(|| scheduler.snapshot().filesystem.queued == 1).await;
        queued.abort();
        let _ = queued.await;
        wait_until(|| scheduler.snapshot().filesystem.queued == 0).await;

        let _ = release.send(());
        assert!(blocker.await.unwrap().is_ok());
        tokio::task::yield_now().await;
        assert!(!side_effect.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn heavy_classes_share_queue_budget_but_keep_execution_capacity_isolated() {
        let scheduler = RequestScheduler::with_limits(RequestLimits {
            control: 1,
            filesystem: 1,
            process: 1,
            browser: 1,
            general: 1,
        });
        let release = Arc::new(tokio::sync::Notify::new());
        let mut blockers = Vec::new();
        for (class, session) in [
            (RequestClass::Filesystem, "fs"),
            (RequestClass::Process, "process"),
        ] {
            let scheduler = scheduler.clone();
            let release = release.clone();
            blockers.push(tokio::spawn(async move {
                scheduler
                    .run_keyed(
                        class,
                        key(session),
                        async move { release.notified().await },
                        Duration::from_secs(3),
                    )
                    .await
            }));
        }
        wait_until(|| {
            scheduler.snapshot().filesystem.active == 1 && scheduler.snapshot().process.active == 1
        })
        .await;

        let control = scheduler
            .run_keyed(
                RequestClass::Control,
                key("control"),
                async { 9u8 },
                Duration::from_secs(1),
            )
            .await;
        assert_eq!(control, Ok(9));
        release.notify_waiters();
        for blocker in blockers {
            assert!(blocker.await.unwrap().is_ok());
        }
    }
}
