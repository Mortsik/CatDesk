//! Bounds concurrent text searches.
//!
//! 2026-09-20: a retrying agent session stacked six identical whole-workspace
//! `rg --hidden --glob **/*` walks (17+ minutes each, a CPU core apiece) and
//! starved the host — the GLM semaphore event loop stalled with them. The
//! gate rejects duplicate queries instead of queueing them and caps total
//! search parallelism.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

/// How many searches may run at the same time.
pub const MAX_CONCURRENT_SEARCHES: usize = 2;

/// Identity of one search query — identical queries must not stack.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SearchKey {
    pub pattern: String,
    pub path: String,
    pub glob: Option<String>,
    pub fixed_strings: bool,
    pub case_insensitive: bool,
    pub include_hidden: bool,
    pub no_ignore: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchGateError {
    /// The exact same query is already running.
    DuplicateInFlight,
    /// The concurrency cap is reached by other queries.
    Busy,
}

/// Releases the slot (and the query identity) when dropped.
pub struct SearchPermit {
    key: SearchKey,
}

impl Drop for SearchPermit {
    fn drop(&mut self) {
        gate_state().lock().unwrap().active.remove(&self.key);
    }
}

struct GateState {
    active: HashSet<SearchKey>,
}

fn gate_state() -> &'static Mutex<GateState> {
    static STATE: OnceLock<Mutex<GateState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(GateState { active: HashSet::new() }))
}

/// Takes a search slot or explains why the query is rejected.
pub fn acquire(key: SearchKey) -> Result<SearchPermit, SearchGateError> {
    let mut state = gate_state().lock().unwrap();
    if state.active.contains(&key) {
        return Err(SearchGateError::DuplicateInFlight);
    }
    if state.active.len() >= MAX_CONCURRENT_SEARCHES {
        return Err(SearchGateError::Busy);
    }
    state.active.insert(key.clone());
    Ok(SearchPermit { key })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(pattern: &str) -> SearchKey {
        SearchKey {
            pattern: pattern.into(),
            path: "/home/morts/dev".into(),
            glob: None,
            fixed_strings: false,
            case_insensitive: false,
            include_hidden: false,
            no_ignore: false,
        }
    }

    // One test fn on purpose: the gate is a process-global singleton, so the
    // phases must run sequentially instead of as parallel #[test] fns.
    #[test]
    fn gate_rejects_duplicates_caps_parallelism_and_frees_on_drop() {
        let first = acquire(key("focus")).expect("first acquire");

        assert!(
            matches!(
                acquire(key("focus")),
                Err(SearchGateError::DuplicateInFlight)
            ),
            "identical query while in flight must be rejected, not stacked"
        );

        let second = acquire(key("other")).expect("different query takes second slot");
        assert!(
            matches!(acquire(key("third")), Err(SearchGateError::Busy)),
            "third concurrent search must be rejected at the cap"
        );

        drop(first);
        drop(second);

        let _again = acquire(key("focus")).expect("identity freed after drop");
        let _third = acquire(key("third")).expect("slot freed after drop");
    }
}
