//! Keep synchronous filesystem/tool work off the async reactor.
use std::{future::Future, sync::Arc, time::Duration};
use tokio::sync::Semaphore;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RequestFailure {
    Busy,
    Deadline,
    Failed,
}

impl std::fmt::Display for RequestFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Busy => "CatDesk is busy: all request workers are occupied. No new work was started.",
            Self::Deadline => "CatDesk response deadline exceeded. The operation may still be running; do not repeat a write or command blindly. Check its result or poll the command job.",
            Self::Failed => "CatDesk request worker failed; inspect diagnostics before retrying.",
        })
    }
}

#[derive(Clone)]
pub(crate) struct RequestWorkers(Arc<Semaphore>);

impl RequestWorkers {
    pub(crate) fn new(limit: usize) -> Self {
        Self(Arc::new(Semaphore::new(limit)))
    }

    pub(crate) async fn run<F>(
        &self,
        work: F,
        deadline: Duration,
    ) -> Result<F::Output, RequestFailure>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let permit = self
            .0
            .clone()
            .try_acquire_owned()
            .map_err(|_| RequestFailure::Busy)?;
        let runtime = tokio::runtime::Handle::current();
        let task = tokio::task::spawn_blocking(move || {
            // A disconnected client or a response timeout cannot cancel a synchronous
            // filesystem call. Keep its slot until the actual work has ended.
            let _permit = permit;
            runtime.block_on(work)
        });
        match tokio::time::timeout(deadline, task).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(_)) => Err(RequestFailure::Failed),
            Err(_) => Err(RequestFailure::Deadline),
        }
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
        Self {
            control: RequestWorkers::new(limits.control),
            filesystem: RequestWorkers::new(limits.filesystem),
            process: RequestWorkers::new(limits.process),
            browser: RequestWorkers::new(limits.browser),
            general: RequestWorkers::new(limits.general),
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

    pub(crate) async fn run<F>(
        &self,
        class: RequestClass,
        work: F,
        deadline: Duration,
    ) -> Result<F::Output, RequestFailure>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.workers(class).run(work, deadline).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn synchronous_tool_does_not_starve_runtime_timer() {
        let workers = RequestWorkers::new(1);
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
    async fn timeout_keeps_capacity_reserved_until_work_really_finishes() {
        let workers = RequestWorkers::new(1);
        let (release, wait) = std::sync::mpsc::channel();
        let watchdog = release.clone();
        // Safety only: never leave a blocking fixture behind if an assertion fails.
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(2));
            let _ = watchdog.send(());
        });
        let result = workers
            .run(
                async move {
                    wait.recv().unwrap();
                },
                Duration::from_millis(20),
            )
            .await;
        assert!(
            result.is_err(),
            "blocked work must have a response deadline"
        );
        let rejected = workers.run(async { 42 }, Duration::from_secs(1)).await;
        let _ = release.send(());
        assert_eq!(
            rejected,
            Err(RequestFailure::Busy),
            "timed out work still owns its slot"
        );

        let value = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match workers.run(async { 42 }, Duration::from_secs(1)).await {
                    Ok(value) => break value,
                    Err(RequestFailure::Busy) => tokio::task::yield_now().await,
                    Err(error) => panic!("unexpected worker failure after release: {error}"),
                }
            }
        })
        .await
        .expect("worker capacity did not recover after underlying work finished");
        assert_eq!(value, 42);
    }

    #[tokio::test]
    async fn disconnected_client_cannot_release_a_running_workers_slot() {
        let workers = RequestWorkers::new(1);
        let (release, wait) = std::sync::mpsc::channel();
        let (started, running) = tokio::sync::oneshot::channel();
        let running_workers = workers.clone();
        let caller = tokio::spawn(async move {
            running_workers
                .run(
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
        let rejected = workers.run(async { 42 }, Duration::from_secs(1)).await;
        let _ = release.send(());
        assert_eq!(rejected, Err(RequestFailure::Busy));
        let value = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(value) = workers.run(async { 42 }, Duration::from_secs(1)).await {
                    break value;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(value, 42);
    }

    #[tokio::test]
    async fn mixed_saturation_fails_fast_without_starving_control_and_recovers_capacity() {
        let scheduler = RequestScheduler::with_limits(RequestLimits {
            control: 2,
            filesystem: 2,
            process: 2,
            browser: 1,
            general: 1,
        });
        let release = Arc::new(tokio::sync::Notify::new());
        let (started_tx, mut started_rx) = tokio::sync::mpsc::channel(4);
        let mut blockers = Vec::new();

        for class in [
            RequestClass::Filesystem,
            RequestClass::Filesystem,
            RequestClass::Process,
            RequestClass::Process,
        ] {
            let scheduler = scheduler.clone();
            let release = release.clone();
            let started_tx = started_tx.clone();
            blockers.push(tokio::spawn(async move {
                scheduler
                    .run(
                        class,
                        async move {
                            started_tx.send(()).await.unwrap();
                            release.notified().await;
                            class
                        },
                        Duration::from_secs(5),
                    )
                    .await
            }));
        }
        drop(started_tx);
        for _ in 0..4 {
            tokio::time::timeout(Duration::from_secs(2), started_rx.recv())
                .await
                .expect("saturating worker did not start")
                .expect("started channel closed early");
        }

        let mut rejected = Vec::new();
        for index in 0..20 {
            let scheduler = scheduler.clone();
            let class = if index % 2 == 0 {
                RequestClass::Filesystem
            } else {
                RequestClass::Process
            };
            rejected.push(tokio::spawn(async move {
                scheduler
                    .run(class, async { 1u8 }, Duration::from_secs(1))
                    .await
            }));
        }
        for task in rejected {
            assert_eq!(task.await.unwrap(), Err(RequestFailure::Busy));
        }

        for expected in 0..8u8 {
            let value = scheduler
                .run(
                    RequestClass::Control,
                    async move { expected },
                    Duration::from_secs(1),
                )
                .await;
            assert_eq!(value, Ok(expected));
        }

        release.notify_waiters();
        for blocker in blockers {
            assert!(blocker.await.unwrap().is_ok());
        }

        assert_eq!(
            scheduler
                .run(
                    RequestClass::Filesystem,
                    async { 41u8 },
                    Duration::from_secs(1),
                )
                .await,
            Ok(41)
        );
        assert_eq!(
            scheduler
                .run(
                    RequestClass::Process,
                    async { 42u8 },
                    Duration::from_secs(1),
                )
                .await,
            Ok(42)
        );
    }

    #[tokio::test]
    async fn saturated_filesystem_pool_does_not_starve_control_pool() {
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
                .run(
                    RequestClass::Filesystem,
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

        let second_filesystem = scheduler
            .run(
                RequestClass::Filesystem,
                async { 8 },
                Duration::from_secs(1),
            )
            .await;
        assert_eq!(second_filesystem, Err(RequestFailure::Busy));

        let control = scheduler
            .run(
                RequestClass::Control,
                async { 42 },
                Duration::from_secs(1),
            )
            .await;
        assert_eq!(control, Ok(42));

        let _ = release.send(());
        assert_eq!(filesystem.await.unwrap(), Ok(7));
    }
}
