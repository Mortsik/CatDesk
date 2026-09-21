use std::{
    collections::{HashMap, VecDeque},
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};
use tokio::sync::oneshot;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct SchedulingKey {
    pub(crate) session: String,
    pub(crate) project: Option<String>,
}

impl SchedulingKey {
    pub(crate) fn new(session: impl Into<String>, project: Option<&str>) -> Self {
        Self {
            session: session.into(),
            project: project.map(str::to_owned),
        }
    }

    pub(crate) fn anonymous() -> Self {
        Self::new("anonymous", None)
    }
}

pub(crate) struct QueueBudget {
    max_queued: usize,
    queued: AtomicUsize,
}

impl QueueBudget {
    pub(crate) fn new(max_queued: usize) -> Arc<Self> {
        Arc::new(Self {
            max_queued,
            queued: AtomicUsize::new(0),
        })
    }

    fn reserve(self: &Arc<Self>) -> Option<QueueReservation> {
        let mut queued = self.queued.load(Ordering::Acquire);
        loop {
            if queued >= self.max_queued {
                return None;
            }
            match self.queued.compare_exchange_weak(
                queued,
                queued + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(QueueReservation {
                        budget: self.clone(),
                    });
                }
                Err(observed) => queued = observed,
            }
        }
    }

    pub(crate) fn queued(&self) -> usize {
        self.queued.load(Ordering::Acquire)
    }
}

struct QueueReservation {
    budget: Arc<QueueBudget>,
}

impl Drop for QueueReservation {
    fn drop(&mut self) {
        self.budget.queued.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FairAcquireError {
    Deadline,
    Overloaded,
    Closed,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct FairGateSnapshot {
    pub(crate) queued: usize,
    pub(crate) active: usize,
    pub(crate) limit: usize,
    pub(crate) rejected_overload: u64,
    pub(crate) cancelled_before_start: u64,
    pub(crate) completed_waits: u64,
    pub(crate) total_wait_ms: u64,
}

#[derive(Default)]
struct GateStats {
    rejected_overload: u64,
    cancelled_before_start: u64,
    completed_waits: u64,
    total_wait_ms: u64,
}

struct Waiter {
    id: u64,
    key: SchedulingKey,
    enqueued_at: Instant,
    grant: oneshot::Sender<FairPermit>,
    _budget: QueueReservation,
}

#[derive(Default)]
struct SessionQueue {
    project_order: VecDeque<Option<String>>,
    projects: HashMap<Option<String>, VecDeque<u64>>,
}

struct GateState {
    limit: usize,
    active: usize,
    waiters: HashMap<u64, Waiter>,
    session_order: VecDeque<String>,
    sessions: HashMap<String, SessionQueue>,
    closed: bool,
    next_waiter_id: u64,
    stats: GateStats,
}

struct GateInner {
    state: Mutex<GateState>,
    budget: Arc<QueueBudget>,
}

#[derive(Clone)]
pub(crate) struct FairGate {
    inner: Arc<GateInner>,
}

pub(crate) struct FairPermit {
    inner: Option<Arc<GateInner>>,
}

impl fmt::Debug for FairPermit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FairPermit").finish_non_exhaustive()
    }
}

impl PartialEq for FairPermit {
    fn eq(&self, other: &Self) -> bool {
        match (&self.inner, &other.inner) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        }
    }
}

impl Eq for FairPermit {}

impl FairPermit {
    fn new(inner: Arc<GateInner>) -> Self {
        Self { inner: Some(inner) }
    }

    fn disarm(&mut self) {
        self.inner = None;
    }
}

impl Drop for FairPermit {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            inner.release();
        }
    }
}

struct WaitRegistration {
    inner: Arc<GateInner>,
    id: Option<u64>,
}

impl WaitRegistration {
    fn disarm(&mut self) {
        self.id = None;
    }
}

impl Drop for WaitRegistration {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            self.inner.cancel_waiter(id);
        }
    }
}

impl FairGate {
    pub(crate) fn new(limit: usize, budget: Arc<QueueBudget>) -> Self {
        Self {
            inner: Arc::new(GateInner {
                state: Mutex::new(GateState {
                    limit,
                    active: 0,
                    waiters: HashMap::new(),
                    session_order: VecDeque::new(),
                    sessions: HashMap::new(),
                    closed: false,
                    next_waiter_id: 1,
                    stats: GateStats::default(),
                }),
                budget,
            }),
        }
    }

    pub(crate) async fn acquire(
        &self,
        key: SchedulingKey,
        deadline: Instant,
    ) -> Result<FairPermit, FairAcquireError> {
        if deadline <= Instant::now() {
            return Err(FairAcquireError::Deadline);
        }
        let Some((id, rx)) = self.inner.enqueue(key)? else {
            return Ok(FairPermit::new(self.inner.clone()));
        };
        let mut registration = WaitRegistration {
            inner: self.inner.clone(),
            id: Some(id),
        };
        let deadline = tokio::time::Instant::from_std(deadline);
        match tokio::time::timeout_at(deadline, rx).await {
            Ok(Ok(permit)) => {
                registration.disarm();
                Ok(permit)
            }
            Ok(Err(_)) => {
                registration.disarm();
                Err(FairAcquireError::Closed)
            }
            Err(_) => Err(FairAcquireError::Deadline),
        }
    }

    pub(crate) async fn acquire_unbounded(
        &self,
        key: SchedulingKey,
    ) -> Result<FairPermit, FairAcquireError> {
        let Some((id, rx)) = self.inner.enqueue(key)? else {
            return Ok(FairPermit::new(self.inner.clone()));
        };
        let mut registration = WaitRegistration {
            inner: self.inner.clone(),
            id: Some(id),
        };
        match rx.await {
            Ok(permit) => {
                registration.disarm();
                Ok(permit)
            }
            Err(_) => {
                registration.disarm();
                Err(FairAcquireError::Closed)
            }
        }
    }

    pub(crate) fn set_limit(&self, limit: usize) {
        let mut state = self.inner.state.lock().unwrap();
        state.limit = limit;
        self.inner.dispatch_locked(&mut state);
    }

    pub(crate) fn snapshot(&self) -> FairGateSnapshot {
        let state = self.inner.state.lock().unwrap();
        FairGateSnapshot {
            queued: state.waiters.len(),
            active: state.active,
            limit: state.limit,
            rejected_overload: state.stats.rejected_overload,
            cancelled_before_start: state.stats.cancelled_before_start,
            completed_waits: state.stats.completed_waits,
            total_wait_ms: state.stats.total_wait_ms,
        }
    }

    pub(crate) fn close(&self) {
        let mut state = self.inner.state.lock().unwrap();
        if state.closed {
            return;
        }
        state.closed = true;
        state.waiters.clear();
        state.session_order.clear();
        state.sessions.clear();
    }
}

impl GateInner {
    fn enqueue(
        self: &Arc<Self>,
        key: SchedulingKey,
    ) -> Result<Option<(u64, oneshot::Receiver<FairPermit>)>, FairAcquireError> {
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return Err(FairAcquireError::Closed);
        }
        if state.active < state.limit && state.waiters.is_empty() {
            state.active += 1;
            return Ok(None);
        }

        let Some(reservation) = self.budget.reserve() else {
            state.stats.rejected_overload = state.stats.rejected_overload.saturating_add(1);
            return Err(FairAcquireError::Overloaded);
        };
        let id = state.next_waiter_id;
        state.next_waiter_id = state.next_waiter_id.wrapping_add(1).max(1);
        let (grant, rx) = oneshot::channel();
        let session = key.session.clone();
        let project = key.project.clone();

        if !state.sessions.contains_key(&session) {
            state
                .sessions
                .insert(session.clone(), SessionQueue::default());
            state.session_order.push_back(session.clone());
        }
        let session_queue = state.sessions.get_mut(&session).unwrap();
        if !session_queue.projects.contains_key(&project) {
            session_queue
                .projects
                .insert(project.clone(), VecDeque::new());
            session_queue.project_order.push_back(project.clone());
        }
        session_queue
            .projects
            .get_mut(&project)
            .unwrap()
            .push_back(id);
        state.waiters.insert(
            id,
            Waiter {
                id,
                key,
                enqueued_at: Instant::now(),
                grant,
                _budget: reservation,
            },
        );
        self.dispatch_locked(&mut state);
        Ok(Some((id, rx)))
    }

    fn release(self: &Arc<Self>) {
        let mut state = self.state.lock().unwrap();
        state.active = state.active.saturating_sub(1);
        self.dispatch_locked(&mut state);
    }

    fn cancel_waiter(self: &Arc<Self>, id: u64) {
        let mut state = self.state.lock().unwrap();
        let Some(waiter) = state.waiters.remove(&id) else {
            return;
        };
        Self::remove_from_rotation(&mut state, &waiter.key, waiter.id);
        state.stats.cancelled_before_start = state.stats.cancelled_before_start.saturating_add(1);
        drop(waiter);
        self.dispatch_locked(&mut state);
    }

    fn remove_from_rotation(state: &mut GateState, key: &SchedulingKey, id: u64) {
        let mut remove_session = false;
        if let Some(session_queue) = state.sessions.get_mut(&key.session) {
            let mut remove_project = false;
            if let Some(ids) = session_queue.projects.get_mut(&key.project) {
                ids.retain(|candidate| *candidate != id);
                remove_project = ids.is_empty();
            }
            if remove_project {
                session_queue.projects.remove(&key.project);
                session_queue
                    .project_order
                    .retain(|project| project != &key.project);
            }
            remove_session = session_queue.projects.is_empty();
        }
        if remove_session {
            state.sessions.remove(&key.session);
            state
                .session_order
                .retain(|session| session != &key.session);
        }
    }

    fn pop_next_waiter(state: &mut GateState) -> Option<Waiter> {
        let session_attempts = state.session_order.len();
        for _ in 0..session_attempts {
            let session = state.session_order.pop_front()?;
            let mut selected_id = None;
            let mut keep_session = false;

            if let Some(session_queue) = state.sessions.get_mut(&session) {
                let project_attempts = session_queue.project_order.len();
                for _ in 0..project_attempts {
                    let Some(project) = session_queue.project_order.pop_front() else {
                        break;
                    };
                    let mut keep_project = false;
                    let mut remove_project = false;
                    if let Some(ids) = session_queue.projects.get_mut(&project) {
                        while let Some(id) = ids.pop_front() {
                            if state.waiters.contains_key(&id) {
                                selected_id = Some(id);
                                break;
                            }
                        }
                        keep_project = !ids.is_empty();
                        remove_project = ids.is_empty();
                    }
                    if keep_project {
                        session_queue.project_order.push_back(project.clone());
                    }
                    if remove_project {
                        session_queue.projects.remove(&project);
                    }
                    if selected_id.is_some() {
                        break;
                    }
                }
                keep_session = !session_queue.projects.is_empty();
            }

            if keep_session {
                state.session_order.push_back(session.clone());
            } else {
                state.sessions.remove(&session);
            }
            if let Some(id) = selected_id {
                return state.waiters.remove(&id);
            }
        }
        None
    }

    fn dispatch_locked(self: &Arc<Self>, state: &mut GateState) {
        if state.closed {
            return;
        }
        while state.active < state.limit {
            let Some(waiter) = Self::pop_next_waiter(state) else {
                break;
            };
            let waited_ms = waiter
                .enqueued_at
                .elapsed()
                .as_millis()
                .min(u64::MAX as u128) as u64;
            state.stats.completed_waits = state.stats.completed_waits.saturating_add(1);
            state.stats.total_wait_ms = state.stats.total_wait_ms.saturating_add(waited_ms);
            state.active += 1;
            let grant = waiter.grant;
            drop(waiter._budget);
            let permit = FairPermit::new(self.clone());
            if let Err(mut returned) = grant.send(permit) {
                returned.disarm();
                state.active = state.active.saturating_sub(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };

    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(2)
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
    async fn saturated_gate_queues_instead_of_failing_busy() {
        let budget = QueueBudget::new(32);
        let gate = FairGate::new(1, budget);
        let held = gate
            .acquire(SchedulingKey::new("a", Some("p")), deadline())
            .await
            .unwrap();

        let queued_gate = gate.clone();
        let queued = tokio::spawn(async move {
            queued_gate
                .acquire(SchedulingKey::new("b", Some("q")), deadline())
                .await
        });

        wait_until(|| gate.snapshot().queued == 1).await;
        drop(held);
        assert!(queued.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn noisy_session_cannot_starve_another_session() {
        let gate = FairGate::new(1, QueueBudget::new(32));
        let held = gate
            .acquire(SchedulingKey::new("holder", None), deadline())
            .await
            .unwrap();
        let order = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let mut tasks = Vec::new();

        for label in ["a1", "a2", "a3", "b1"] {
            let task_gate = gate.clone();
            let order = order.clone();
            let session = if label.starts_with('a') { "a" } else { "b" };
            tasks.push(tokio::spawn(async move {
                let _permit = task_gate
                    .acquire(SchedulingKey::new(session, Some("p")), deadline())
                    .await
                    .unwrap();
                order.lock().await.push(label);
            }));
            wait_until(|| gate.snapshot().queued == tasks.len()).await;
        }

        drop(held);
        for task in tasks {
            task.await.unwrap();
        }
        let order = order.lock().await.clone();
        let b = order.iter().position(|label| *label == "b1").unwrap();
        let a3 = order.iter().position(|label| *label == "a3").unwrap();
        assert!(b < a3, "session b was starved: {order:?}");
    }

    #[tokio::test]
    async fn projects_rotate_within_one_session() {
        let gate = FairGate::new(1, QueueBudget::new(32));
        let held = gate
            .acquire(SchedulingKey::new("holder", None), deadline())
            .await
            .unwrap();
        let order = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let mut tasks = Vec::new();

        for (label, project) in [("p1a", "p1"), ("p1b", "p1"), ("p2", "p2")] {
            let task_gate = gate.clone();
            let order = order.clone();
            tasks.push(tokio::spawn(async move {
                let _permit = task_gate
                    .acquire(SchedulingKey::new("a", Some(project)), deadline())
                    .await
                    .unwrap();
                order.lock().await.push(label);
            }));
            wait_until(|| gate.snapshot().queued == tasks.len()).await;
        }

        drop(held);
        for task in tasks {
            task.await.unwrap();
        }
        let order = order.lock().await.clone();
        let p2 = order.iter().position(|label| *label == "p2").unwrap();
        let p1b = order.iter().position(|label| *label == "p1b").unwrap();
        assert!(p2 < p1b, "project p2 was starved: {order:?}");
    }

    #[tokio::test]
    async fn deadline_removes_waiter_and_returns_budget_immediately() {
        let budget = QueueBudget::new(1);
        let gate = FairGate::new(1, budget.clone());
        let held = gate
            .acquire(SchedulingKey::anonymous(), deadline())
            .await
            .unwrap();
        let result = gate
            .acquire(
                SchedulingKey::new("late", None),
                Instant::now() + Duration::from_millis(20),
            )
            .await;
        assert_eq!(result, Err(FairAcquireError::Deadline));
        assert_eq!(gate.snapshot().queued, 0);
        assert_eq!(budget.queued(), 0);
        drop(held);
    }

    #[tokio::test]
    async fn shared_budget_is_global_across_gates() {
        let budget = QueueBudget::new(1);
        let a = FairGate::new(0, budget.clone());
        let b = FairGate::new(0, budget.clone());
        let first = tokio::spawn({
            let a = a.clone();
            async move { a.acquire(SchedulingKey::new("a", None), deadline()).await }
        });
        wait_until(|| a.snapshot().queued == 1).await;
        assert_eq!(
            b.acquire(SchedulingKey::new("b", None), deadline()).await,
            Err(FairAcquireError::Overloaded)
        );
        first.abort();
        let _ = first.await;
        wait_until(|| budget.queued() == 0).await;
    }

    #[tokio::test]
    async fn close_wakes_all_waiters_without_granting_work() {
        let gate = FairGate::new(1, QueueBudget::new(8));
        let held = gate
            .acquire(SchedulingKey::anonymous(), deadline())
            .await
            .unwrap();
        let first = tokio::spawn({
            let gate = gate.clone();
            async move {
                gate.acquire(SchedulingKey::new("a", None), deadline())
                    .await
            }
        });
        let second = tokio::spawn({
            let gate = gate.clone();
            async move {
                gate.acquire(SchedulingKey::new("b", None), deadline())
                    .await
            }
        });
        wait_until(|| gate.snapshot().queued == 2).await;
        gate.close();
        assert_eq!(first.await.unwrap(), Err(FairAcquireError::Closed));
        assert_eq!(second.await.unwrap(), Err(FairAcquireError::Closed));
        drop(held);
        assert_eq!(gate.snapshot().active, 0);
        assert_eq!(gate.snapshot().queued, 0);
    }

    #[tokio::test]
    async fn lowering_limit_does_not_revoke_running_permits() {
        let gate = FairGate::new(2, QueueBudget::new(32));
        let a = gate
            .acquire(SchedulingKey::new("a", None), deadline())
            .await
            .unwrap();
        let b = gate
            .acquire(SchedulingKey::new("b", None), deadline())
            .await
            .unwrap();
        gate.set_limit(1);
        assert_eq!(gate.snapshot().active, 2);
        drop(a);
        assert_eq!(gate.snapshot().active, 1);
        drop(b);
        assert_eq!(gate.snapshot().active, 0);
    }
}
