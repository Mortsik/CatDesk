use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MAX_TRACKED_SESSIONS: usize = 1_024;
const SESSION_TTL: Duration = Duration::from_secs(60 * 60);
const CHECKPOINT_AFTER: Duration = Duration::from_secs(10 * 60);
const CHECKPOINT_IDLE_RESET: Duration = Duration::from_secs(5 * 60);
const CHECKPOINT_TOOL_CALLS: u64 = 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProjectStateChange {
    Ignored,
    Selected,
    Changed,
    Unchanged,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CheckpointRecommendation {
    pub age_ms: u64,
    pub tool_calls: u64,
}

#[derive(Clone, Debug)]
struct CheckpointWindow {
    started_at: Instant,
    last_tool_call_at: Instant,
    tool_calls: u64,
    delivered: bool,
}

#[derive(Clone, Debug)]
struct NamedSessionState {
    instruction_called: bool,
    active_project: Option<PathBuf>,
    checkpoint: Option<CheckpointWindow>,
    last_seen: Instant,
}

#[derive(Default)]
struct NamedSessionMap {
    sessions: HashMap<String, NamedSessionState>,
}

#[derive(Clone)]
pub(crate) struct SessionContextStore {
    anonymous_instruction_called: Arc<AtomicBool>,
    sessions: Arc<Mutex<NamedSessionMap>>,
}

impl SessionContextStore {
    pub(crate) fn with_anonymous(called: bool) -> Self {
        Self {
            anonymous_instruction_called: Arc::new(AtomicBool::new(called)),
            sessions: Arc::new(Mutex::new(NamedSessionMap::default())),
        }
    }

    fn prune_sessions(state: &mut NamedSessionMap, now: Instant) {
        state
            .sessions
            .retain(|_, session| now.duration_since(session.last_seen) <= SESSION_TTL);
    }

    fn ensure_capacity(state: &mut NamedSessionMap, session_id: &str) {
        if state.sessions.contains_key(session_id) || state.sessions.len() < MAX_TRACKED_SESSIONS {
            return;
        }
        if let Some(oldest) = state
            .sessions
            .iter()
            .min_by_key(|(_, session)| session.last_seen)
            .map(|(id, _)| id.clone())
        {
            state.sessions.remove(&oldest);
        }
    }

    fn session_mut<'a>(
        state: &'a mut NamedSessionMap,
        session_id: &str,
        now: Instant,
    ) -> &'a mut NamedSessionState {
        Self::ensure_capacity(state, session_id);
        let session = state
            .sessions
            .entry(session_id.to_string())
            .or_insert_with(|| NamedSessionState {
                instruction_called: false,
                active_project: None,
                checkpoint: None,
                last_seen: now,
            });
        session.last_seen = now;
        session
    }

    pub(crate) fn instruction_called(&self, session_id: Option<&str>) -> bool {
        let Some(session_id) = session_id else {
            return self.anonymous_instruction_called.load(Ordering::Acquire);
        };
        let now = Instant::now();
        let mut state = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Self::prune_sessions(&mut state, now);
        Self::session_mut(&mut state, session_id, now).instruction_called
    }

    pub(crate) fn mark_instruction_called(&self, session_id: Option<&str>) {
        let Some(session_id) = session_id else {
            self.anonymous_instruction_called
                .store(true, Ordering::Release);
            return;
        };
        let now = Instant::now();
        let mut state = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Self::prune_sessions(&mut state, now);
        Self::session_mut(&mut state, session_id, now).instruction_called = true;
    }

    pub(crate) fn active_project(&self, session_id: Option<&str>) -> Option<PathBuf> {
        let session_id = session_id?;
        let now = Instant::now();
        let mut state = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Self::prune_sessions(&mut state, now);
        Self::session_mut(&mut state, session_id, now)
            .active_project
            .clone()
    }

    pub(crate) fn set_active_project(
        &self,
        session_id: Option<&str>,
        project: PathBuf,
    ) -> ProjectStateChange {
        let Some(session_id) = session_id else {
            return ProjectStateChange::Ignored;
        };
        let now = Instant::now();
        let mut state = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Self::prune_sessions(&mut state, now);
        let session = Self::session_mut(&mut state, session_id, now);
        match session.active_project.as_ref() {
            None => {
                session.active_project = Some(project);
                ProjectStateChange::Selected
            }
            Some(current) if current == &project => ProjectStateChange::Unchanged,
            Some(_) => {
                session.active_project = Some(project);
                ProjectStateChange::Changed
            }
        }
    }

    pub(crate) fn clear_active_project(&self, session_id: Option<&str>) -> bool {
        let Some(session_id) = session_id else {
            return false;
        };
        let now = Instant::now();
        let mut state = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Self::prune_sessions(&mut state, now);
        Self::session_mut(&mut state, session_id, now)
            .active_project
            .take()
            .is_some()
    }

    pub(crate) fn record_tool_call(&self, session_id: Option<&str>) {
        self.record_tool_call_at(session_id, Instant::now());
    }

    fn record_tool_call_at(&self, session_id: Option<&str>, now: Instant) {
        let Some(session_id) = session_id else {
            return;
        };
        let mut state = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Self::prune_sessions(&mut state, now);
        let session = Self::session_mut(&mut state, session_id, now);
        let reset = session.checkpoint.as_ref().is_none_or(|window| {
            now.duration_since(window.last_tool_call_at) >= CHECKPOINT_IDLE_RESET
        });
        if reset {
            session.checkpoint = Some(CheckpointWindow {
                started_at: now,
                last_tool_call_at: now,
                tool_calls: 1,
                delivered: false,
            });
            return;
        }
        if let Some(window) = session.checkpoint.as_mut() {
            window.last_tool_call_at = now;
            window.tool_calls = window.tool_calls.saturating_add(1);
        }
    }

    pub(crate) fn claim_checkpoint(
        &self,
        session_id: Option<&str>,
    ) -> Option<CheckpointRecommendation> {
        self.claim_checkpoint_at(session_id, Instant::now())
    }

    fn claim_checkpoint_at(
        &self,
        session_id: Option<&str>,
        now: Instant,
    ) -> Option<CheckpointRecommendation> {
        let session_id = session_id?;
        let mut state = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Self::prune_sessions(&mut state, now);
        let session = Self::session_mut(&mut state, session_id, now);
        let window = session.checkpoint.as_mut()?;
        let age = now.duration_since(window.started_at);
        if window.delivered || (age < CHECKPOINT_AFTER && window.tool_calls < CHECKPOINT_TOOL_CALLS)
        {
            return None;
        }
        window.delivered = true;
        Some(CheckpointRecommendation {
            age_ms: age.as_millis().try_into().unwrap_or(u64::MAX),
            tool_calls: window.tool_calls,
        })
    }

    pub(crate) fn forget(&self, session_id: Option<&str>) {
        let Some(session_id) = session_id else {
            self.anonymous_instruction_called
                .store(false, Ordering::Release);
            return;
        };
        let mut state = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.sessions.remove(session_id);
    }

    pub(crate) fn has_named_sessions(&self) -> bool {
        let now = Instant::now();
        let mut state = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Self::prune_sessions(&mut state, now);
        !state.sessions.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn contains_named_session(&self, session_id: &str) -> bool {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .sessions
            .contains_key(session_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn named_sessions_keep_independent_active_projects() {
        let store = SessionContextStore::with_anonymous(false);
        let repo_a = PathBuf::from("/workspace/repo-a");
        let repo_b = PathBuf::from("/workspace/repo-b");

        assert_eq!(
            store.set_active_project(Some("session-a"), repo_a.clone()),
            ProjectStateChange::Selected
        );
        assert_eq!(
            store.set_active_project(Some("session-b"), repo_b.clone()),
            ProjectStateChange::Selected
        );

        assert_eq!(store.active_project(Some("session-a")), Some(repo_a));
        assert_eq!(store.active_project(Some("session-b")), Some(repo_b));
    }

    #[test]
    fn anonymous_session_never_keeps_active_project() {
        let store = SessionContextStore::with_anonymous(false);

        assert_eq!(
            store.set_active_project(None, PathBuf::from("/workspace/repo")),
            ProjectStateChange::Ignored
        );
        assert_eq!(store.active_project(None), None);
    }

    #[test]
    fn instruction_state_survives_project_clear_and_forget_removes_both() {
        let store = SessionContextStore::with_anonymous(false);
        store.mark_instruction_called(Some("session-a"));
        store.set_active_project(Some("session-a"), PathBuf::from("/workspace/repo"));

        assert!(store.instruction_called(Some("session-a")));
        assert!(store.clear_active_project(Some("session-a")));
        assert!(store.instruction_called(Some("session-a")));
        assert_eq!(store.active_project(Some("session-a")), None);

        store.forget(Some("session-a"));
        assert!(!store.instruction_called(Some("session-a")));
        assert_eq!(store.active_project(Some("session-a")), None);
    }

    #[test]
    fn changing_one_session_does_not_affect_another() {
        let store = SessionContextStore::with_anonymous(false);
        let repo_a = PathBuf::from("/workspace/repo-a");
        let repo_b = PathBuf::from("/workspace/repo-b");
        let repo_c = PathBuf::from("/workspace/repo-c");
        store.set_active_project(Some("session-a"), repo_a);
        store.set_active_project(Some("session-b"), repo_b.clone());

        assert_eq!(
            store.set_active_project(Some("session-a"), repo_c.clone()),
            ProjectStateChange::Changed
        );
        assert_eq!(store.active_project(Some("session-a")), Some(repo_c));
        assert_eq!(store.active_project(Some("session-b")), Some(repo_b));
    }

    #[test]
    fn checkpoint_is_recommended_on_sixtieth_tool_call_and_claimed_once() {
        let store = SessionContextStore::with_anonymous(false);
        let started = Instant::now();

        for offset in 0..59 {
            store.record_tool_call_at(Some("session-a"), started + Duration::from_secs(offset));
            assert_eq!(
                store.claim_checkpoint_at(Some("session-a"), started + Duration::from_secs(offset)),
                None
            );
        }

        let now = started + Duration::from_secs(59);
        store.record_tool_call_at(Some("session-a"), now);
        let recommendation = store
            .claim_checkpoint_at(Some("session-a"), now)
            .expect("sixtieth call should recommend a checkpoint");
        assert_eq!(recommendation.tool_calls, 60);
        assert_eq!(recommendation.age_ms, 59_000);
        assert_eq!(store.claim_checkpoint_at(Some("session-a"), now), None);
    }

    #[test]
    fn checkpoint_is_recommended_after_ten_active_minutes() {
        let store = SessionContextStore::with_anonymous(false);
        let started = Instant::now();
        store.record_tool_call_at(Some("session-a"), started);
        store.record_tool_call_at(
            Some("session-a"),
            started + Duration::from_secs(4 * 60 + 59),
        );
        store.record_tool_call_at(
            Some("session-a"),
            started + Duration::from_secs(9 * 60 + 58),
        );

        let now = started + CHECKPOINT_AFTER;
        store.record_tool_call_at(Some("session-a"), now);
        assert_eq!(
            store.claim_checkpoint_at(Some("session-a"), now),
            Some(CheckpointRecommendation {
                age_ms: CHECKPOINT_AFTER.as_millis() as u64,
                tool_calls: 4,
            })
        );
    }

    #[test]
    fn five_minutes_idle_starts_a_fresh_checkpoint_window() {
        let store = SessionContextStore::with_anonymous(false);
        let started = Instant::now();
        for offset in 0..59 {
            store.record_tool_call_at(Some("session-a"), started + Duration::from_secs(offset));
        }
        let after_idle = started + Duration::from_secs(58) + CHECKPOINT_IDLE_RESET;
        store.record_tool_call_at(Some("session-a"), after_idle);

        assert_eq!(
            store.claim_checkpoint_at(Some("session-a"), after_idle),
            None
        );
        for offset in 1..60 {
            let now = after_idle + Duration::from_secs(offset);
            store.record_tool_call_at(Some("session-a"), now);
            if offset < 59 {
                assert_eq!(store.claim_checkpoint_at(Some("session-a"), now), None);
            }
        }
        let recommendation = store
            .claim_checkpoint_at(Some("session-a"), after_idle + Duration::from_secs(59))
            .expect("fresh window should recommend on its own sixtieth call");
        assert_eq!(recommendation.tool_calls, 60);
    }

    #[test]
    fn out_of_order_concurrent_timestamps_do_not_panic_or_reset_the_window() {
        let store = SessionContextStore::with_anonymous(false);
        let started = Instant::now();
        store.record_tool_call_at(Some("session-a"), started + Duration::from_secs(1));
        store.record_tool_call_at(Some("session-a"), started);
        for offset in 2..60 {
            store.record_tool_call_at(Some("session-a"), started + Duration::from_secs(offset));
        }

        let recommendation = store
            .claim_checkpoint_at(Some("session-a"), started + Duration::from_secs(59))
            .expect("out-of-order arrival must still count toward the same window");
        assert_eq!(recommendation.tool_calls, 60);
    }

    #[test]
    fn checkpoint_windows_are_isolated_and_anonymous_calls_are_ignored() {
        let store = SessionContextStore::with_anonymous(false);
        let started = Instant::now();
        for offset in 0..60 {
            let now = started + Duration::from_secs(offset);
            store.record_tool_call_at(Some("session-a"), now);
            if offset < 10 {
                store.record_tool_call_at(Some("session-b"), now);
            }
            store.record_tool_call_at(None, now);
        }

        assert!(
            store
                .claim_checkpoint_at(Some("session-a"), started + Duration::from_secs(59))
                .is_some()
        );
        assert_eq!(
            store.claim_checkpoint_at(Some("session-b"), started + Duration::from_secs(59)),
            None
        );
        assert_eq!(
            store.claim_checkpoint_at(None, started + CHECKPOINT_AFTER),
            None
        );
    }
}
