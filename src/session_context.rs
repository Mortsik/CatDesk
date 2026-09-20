use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MAX_TRACKED_SESSIONS: usize = 1_024;
const SESSION_TTL: Duration = Duration::from_secs(60 * 60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProjectStateChange {
    Ignored,
    Selected,
    Changed,
    Unchanged,
}

#[derive(Clone, Debug)]
struct NamedSessionState {
    instruction_called: bool,
    active_project: Option<PathBuf>,
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
}
