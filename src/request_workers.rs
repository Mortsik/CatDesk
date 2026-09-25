//! Keep synchronous filesystem/tool work off the async reactor while enforcing
//! per-class response deadlines.
//!
//! CatDesk owns no admission control: request concurrency is unbounded and the
//! host OS plus the Tokio runtime are the only physical concurrency limiters.
//! This module guarantees only that synchronous tool work never stalls the
//! async reactor and that every request honors its response deadline.
use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RequestFailure {
    Deadline,
    Failed,
}

impl std::fmt::Display for RequestFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Deadline => "CatDesk response deadline exceeded. If execution had already started, the operation may still be running; do not repeat a write or command blindly. Check its result or poll the command job.",
            Self::Failed => "CatDesk request worker failed; inspect diagnostics before retrying.",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RequestDeadlineStage {
    Queue,
    Execution,
}

impl RequestDeadlineStage {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Queue => "queue",
            Self::Execution => "execution",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RequestTiming {
    pub(crate) queue_wait_ms: u64,
    pub(crate) execution_ms: u64,
    pub(crate) deadline_stage: Option<RequestDeadlineStage>,
}

#[derive(Debug)]
pub(crate) struct TimedRequestResult<T> {
    pub(crate) value: T,
    pub(crate) timing: RequestTiming,
}

#[derive(Debug)]
pub(crate) struct TimedRequestError {
    pub(crate) failure: RequestFailure,
    pub(crate) timing: RequestTiming,
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Sentinel stored in the shared dispatch timestamp until the blocking task
/// has actually started running.
const NOT_DISPATCHED: u64 = u64::MAX;

pub(crate) async fn run_timed<F>(
    work: F,
    deadline: Duration,
) -> Result<TimedRequestResult<F::Output>, TimedRequestError>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let request_started = Instant::now();
    let runtime = tokio::runtime::Handle::current();
    // Synchronous tool work must never stall the async reactor, so it runs on
    // the blocking pool. A disconnected client or an expired response deadline
    // cannot safely cancel work that already started; the task always runs to
    // completion. The dispatch timestamp is shared with the caller so a timed
    // out join handle can still split blocking-pool dispatch from execution.
    let dispatch_wait_ms = Arc::new(AtomicU64::new(NOT_DISPATCHED));
    let observed_dispatch_ms = dispatch_wait_ms.clone();
    let task = tokio::task::spawn_blocking(move || {
        let queue_wait_ms = elapsed_ms(request_started);
        observed_dispatch_ms.store(queue_wait_ms, Ordering::Release);
        let execution_started = Instant::now();
        let value = runtime.block_on(work);
        (value, queue_wait_ms, elapsed_ms(execution_started))
    });
    match tokio::time::timeout(deadline, task).await {
        Ok(Ok((value, queue_wait_ms, execution_ms))) => Ok(TimedRequestResult {
            value,
            timing: RequestTiming {
                queue_wait_ms,
                execution_ms,
                deadline_stage: None,
            },
        }),
        Ok(Err(_)) => {
            let timing = observed_timing(&request_started, &dispatch_wait_ms, None);
            Err(TimedRequestError {
                failure: RequestFailure::Failed,
                timing,
            })
        }
        Err(_) => {
            // The join handle timed out, but the blocking task published its
            // dispatch timestamp when it started, so the timing still splits
            // blocking-pool dispatch from real execution.
            let dispatch_ms = dispatch_wait_ms.load(Ordering::Acquire);
            let deadline_stage = if dispatch_ms == NOT_DISPATCHED {
                RequestDeadlineStage::Queue
            } else {
                RequestDeadlineStage::Execution
            };
            let timing = observed_timing(&request_started, &dispatch_wait_ms, Some(deadline_stage));
            Err(TimedRequestError {
                failure: RequestFailure::Deadline,
                timing,
            })
        }
    }
}

/// Reconstruct timing from the outside when the join result is unavailable:
/// if the blocking task started, its published dispatch timestamp splits the
/// elapsed time; otherwise the whole wait was blocking-pool dispatch.
fn observed_timing(
    request_started: &Instant,
    dispatch_wait_ms: &AtomicU64,
    deadline_stage: Option<RequestDeadlineStage>,
) -> RequestTiming {
    let total_ms = elapsed_ms(*request_started);
    match dispatch_wait_ms.load(Ordering::Acquire) {
        NOT_DISPATCHED => RequestTiming {
            queue_wait_ms: total_ms,
            execution_ms: 0,
            deadline_stage,
        },
        dispatch_ms => RequestTiming {
            queue_wait_ms: dispatch_ms,
            execution_ms: total_ms.saturating_sub(dispatch_ms),
            deadline_stage,
        },
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[tokio::test]
    async fn synchronous_tool_does_not_starve_runtime_timer() {
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

        let released_by_runtime = run_timed(
            async move {
                started_tx.send(()).unwrap();
                release_rx
                    .recv_timeout(Duration::from_secs(4))
                    .expect("worker fixture was never released")
            },
            Duration::from_secs(5),
        )
        .await
        .expect("worker request failed")
        .value;
        async_release.await.expect("runtime release task panicked");

        assert!(
            released_by_runtime,
            "synchronous work blocked the runtime timer until the watchdog intervened"
        );
    }

    #[tokio::test]
    async fn response_deadline_expires_while_started_work_runs_to_completion() {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let watchdog = release.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(2));
            let _ = watchdog.send(());
        });
        let error = run_timed(
            async move {
                started_tx
                    .send(())
                    .expect("announce that execution started");
                let _ = wait.recv_timeout(Duration::from_secs(4));
            },
            Duration::from_millis(100),
        )
        .await
        .expect_err("work outliving its deadline must hit the response deadline");
        started_rx
            .await
            .expect("execution must start before the response deadline expires");
        assert_eq!(error.failure, RequestFailure::Deadline);
        assert_eq!(
            error.timing.deadline_stage,
            Some(RequestDeadlineStage::Execution)
        );
        assert!(
            error.timing.execution_ms > 0,
            "a timeout after execution started must be attributed to execution time, not dispatch: {:?}",
            error.timing
        );
        assert!(
            error.timing.queue_wait_ms <= 100,
            "dispatch wait exceeded the whole deadline: {:?}",
            error.timing
        );

        // The already-started work cannot be cancelled; release it so the
        // detached blocking task does not outlive the test.
        let _ = release.send(());
    }

    #[tokio::test]
    async fn requests_beyond_every_former_worker_limit_run_concurrently() {
        // The old per-class worker caps totaled 48 slots (largest single class:
        // 16). All of these requests block on one barrier, so they must not
        // merely be accepted but actually run at the same time.
        const CONCURRENCY: usize = 64;
        let barrier = Arc::new(tokio::sync::Barrier::new(CONCURRENCY));
        let completed = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..CONCURRENCY {
            let barrier = barrier.clone();
            let completed = completed.clone();
            tasks.push(tokio::spawn(async move {
                let result = run_timed(
                    async move {
                        barrier.wait().await;
                    },
                    Duration::from_secs(10),
                )
                .await
                .expect("no request may be rejected");
                assert_eq!(result.timing.deadline_stage, None);
                completed.fetch_add(1, Ordering::SeqCst);
            }));
        }
        for task in tasks {
            task.await.expect("concurrent request task panicked");
        }
        assert_eq!(completed.load(Ordering::SeqCst), CONCURRENCY);
    }

    #[tokio::test]
    async fn timed_requests_report_successful_dispatch_and_execution() {
        let result = run_timed(async { 42u8 }, Duration::from_secs(1))
            .await
            .expect("timed request should succeed");
        assert_eq!(result.value, 42);
        assert_eq!(result.timing.deadline_stage, None);
    }
}
