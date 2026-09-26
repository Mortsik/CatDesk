use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::state::{DailyUsageByModel, UsageTotals, persist_usage_by_model_at_path_if_current};

type UsageSnapshot = BTreeMap<String, UsageTotals>;

#[derive(Clone)]
struct PendingUsage {
    generation: u64,
    snapshot: UsageSnapshot,
    daily_snapshot: DailyUsageByModel,
    total_request_count: u64,
}

#[cfg(not(test))]
const USAGE_PERSIST_DEBOUNCE: Duration = Duration::from_secs(2);
#[cfg(test)]
const USAGE_PERSIST_DEBOUNCE: Duration = Duration::from_millis(25);

pub(crate) struct UsagePersistence {
    pending: Arc<Mutex<Option<PendingUsage>>>,
    generation: Arc<AtomicU64>,
    stopping: Arc<AtomicBool>,
    wake_tx: Option<SyncSender<()>>,
    worker: Option<JoinHandle<()>>,
}

impl UsagePersistence {
    pub(crate) fn new(config_path: PathBuf) -> Self {
        let pending = Arc::new(Mutex::new(None));
        let generation = Arc::new(AtomicU64::new(0));
        let stopping = Arc::new(AtomicBool::new(false));
        let (wake_tx, wake_rx) = sync_channel(1);
        let worker_pending = pending.clone();
        let worker_generation = generation.clone();
        let worker_stopping = stopping.clone();
        let worker = thread::Builder::new()
            .name("catdesk-usage-persist".to_string())
            .spawn(move || {
                run_worker(
                    config_path,
                    worker_pending,
                    worker_generation,
                    worker_stopping,
                    wake_rx,
                    USAGE_PERSIST_DEBOUNCE,
                );
            })
            .expect("failed to start CatDesk usage persistence worker");

        Self {
            pending,
            generation,
            stopping,
            wake_tx: Some(wake_tx),
            worker: Some(worker),
        }
    }

    pub(crate) fn schedule(
        &self,
        snapshot: UsageSnapshot,
        daily_snapshot: DailyUsageByModel,
        total_request_count: u64,
    ) {
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        *self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(PendingUsage {
            generation,
            snapshot,
            daily_snapshot,
            total_request_count,
        });

        let Some(wake_tx) = self.wake_tx.as_ref() else {
            return;
        };
        match wake_tx.try_send(()) {
            Ok(()) | Err(TrySendError::Full(())) => {}
            Err(TrySendError::Disconnected(())) => {
                crate::diagnostics::event("usage_persist_worker_disconnected");
            }
        }
    }

    pub(crate) fn supersede_pending(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
    }
}

impl Drop for UsagePersistence {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        if let Some(wake_tx) = self.wake_tx.take() {
            let _ = wake_tx.try_send(());
            drop(wake_tx);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn flush_pending(
    config_path: &PathBuf,
    pending: &Mutex<Option<PendingUsage>>,
    current_generation: &AtomicU64,
) {
    let pending_usage = pending
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    let Some(pending_usage) = pending_usage else {
        return;
    };
    match persist_usage_by_model_at_path_if_current(
        config_path,
        pending_usage.snapshot.clone(),
        pending_usage.daily_snapshot.clone(),
        pending_usage.total_request_count,
        pending_usage.generation,
        current_generation,
    ) {
        Ok(true) => {}
        Ok(false) => crate::diagnostics::event("usage_persist_stale_skipped"),
        Err(_) => {
            crate::diagnostics::event("usage_persist_failed");
            if current_generation.load(Ordering::Acquire) == pending_usage.generation {
                let mut pending = pending
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if pending.is_none()
                    && current_generation.load(Ordering::Acquire) == pending_usage.generation
                {
                    *pending = Some(pending_usage);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;
    use crate::usage_pricing::FALLBACK_USAGE_BUCKET;
    use std::fs;
    use uuid::Uuid;

    #[test]
    fn failed_write_keeps_snapshot_for_shutdown_retry() {
        let root = std::env::temp_dir().join(format!("catdesk-usage-retry-{}", Uuid::new_v4()));
        let config_dir = root.join("config-dir");
        let config_path = config_dir.join("config.toml");
        fs::create_dir_all(&config_dir).expect("create config dir");
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).expect("create workspace");
        let seed = AppState::new_for_test(
            0,
            workspace.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("create seed app state");
        seed.persist_state().expect("seed config");
        drop(seed);

        let mut usage = UsageSnapshot::new();
        let mut totals = UsageTotals::default();
        totals.accumulate(55, 7, 1);
        usage.insert(FALLBACK_USAGE_BUCKET.to_string(), totals);
        let pending = Mutex::new(Some(PendingUsage {
            generation: 1,
            snapshot: usage,
            daily_snapshot: DailyUsageByModel::new(),
            total_request_count: 0,
        }));
        let generation = AtomicU64::new(1);

        fs::remove_file(&config_path).expect("remove config file");
        fs::remove_dir(&config_dir).expect("remove config dir");
        fs::write(&config_dir, "block directory recreation").expect("create blocking file");
        flush_pending(&config_path, &pending, &generation);
        assert!(
            pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_some(),
            "failed persistence must keep the snapshot pending",
        );

        fs::remove_file(&config_dir).expect("remove blocking file");
        fs::create_dir_all(&config_dir).expect("restore config dir");
        let repaired = AppState::new_for_test(
            0,
            workspace.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("repair config app state");
        repaired.persist_state().expect("repair config");
        drop(repaired);

        flush_pending(&config_path, &pending, &generation);
        assert!(
            pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_none(),
            "successful retry must clear the pending snapshot",
        );

        let saved = AppState::new_for_test(
            0,
            workspace.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("load retried usage");
        let all_usage = saved.all_time_usage_totals();
        let usage = saved
            .usage_by_model
            .get(FALLBACK_USAGE_BUCKET)
            .expect("missing usage after retry");
        assert_eq!(usage.tool_input_tokens, 55);
        assert_eq!(usage.tool_output_tokens, 7);
        assert_eq!(usage.tool_call_count, 1);
        assert_eq!(all_usage.tool_call_count, 1);
        drop(saved);

        let _ = fs::remove_dir_all(root);
    }
}

fn run_worker(
    config_path: PathBuf,
    pending: Arc<Mutex<Option<PendingUsage>>>,
    current_generation: Arc<AtomicU64>,
    stopping: Arc<AtomicBool>,
    wake_rx: Receiver<()>,
    debounce: Duration,
) {
    loop {
        if wake_rx.recv().is_err() {
            flush_pending(&config_path, &pending, &current_generation);
            return;
        }
        if stopping.load(Ordering::Acquire) {
            flush_pending(&config_path, &pending, &current_generation);
            return;
        }

        let deadline = Instant::now() + debounce;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match wake_rx.recv_timeout(remaining) {
                Ok(()) => {
                    if stopping.load(Ordering::Acquire) {
                        flush_pending(&config_path, &pending, &current_generation);
                        return;
                    }
                }
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => {
                    flush_pending(&config_path, &pending, &current_generation);
                    return;
                }
            }
        }

        flush_pending(&config_path, &pending, &current_generation);
    }
}
