use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

pub(crate) const DEFAULT_RESULT_TTL: Duration = Duration::from_secs(60 * 60);
pub(crate) const DEFAULT_MAX_ENTRY_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const DEFAULT_MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
pub(crate) const DEFAULT_MAX_RANGE_BYTES: usize = 128 * 1024;
pub(crate) const DEFAULT_MAX_SEARCH_MATCHES: usize = 100;
const DEFAULT_SEARCH_CHUNK_BYTES: usize = 64 * 1024;
const DEFAULT_TOMBSTONE_LIMIT: usize = 1024;
const SEARCH_SNIPPET_BYTES: usize = 256;

#[derive(Clone, Copy)]
pub(crate) struct LargeResultStoreConfig {
    pub(crate) ttl: Duration,
    pub(crate) max_entry_bytes: u64,
    pub(crate) max_total_bytes: u64,
    pub(crate) max_range_bytes: usize,
    pub(crate) max_search_matches: usize,
    pub(crate) search_chunk_bytes: usize,
    pub(crate) tombstone_limit: usize,
}

impl Default for LargeResultStoreConfig {
    fn default() -> Self {
        Self {
            ttl: DEFAULT_RESULT_TTL,
            max_entry_bytes: DEFAULT_MAX_ENTRY_BYTES,
            max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
            max_range_bytes: DEFAULT_MAX_RANGE_BYTES,
            max_search_matches: DEFAULT_MAX_SEARCH_MATCHES,
            search_chunk_bytes: DEFAULT_SEARCH_CHUNK_BYTES,
            tombstone_limit: DEFAULT_TOMBSTONE_LIMIT,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ResultKind {
    Text,
    Binary,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ResultMetadata {
    pub(crate) result_id: String,
    pub(crate) size_bytes: u64,
    pub(crate) kind: ResultKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) content_type: Option<String>,
    pub(crate) created_at_ms: u64,
    pub(crate) expires_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StoredResult {
    pub(crate) metadata: ResultMetadata,
    pub(crate) evicted_result_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RangeResult {
    pub(crate) metadata: ResultMetadata,
    pub(crate) offset: u64,
    pub(crate) bytes: Vec<u8>,
    pub(crate) text: Option<String>,
    pub(crate) next_offset: u64,
    pub(crate) eof: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SearchMatch {
    pub(crate) offset: u64,
    pub(crate) end_offset: u64,
    pub(crate) snippet_offset: u64,
    pub(crate) snippet: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SearchResult {
    pub(crate) metadata: ResultMetadata,
    pub(crate) matches: Vec<SearchMatch>,
    pub(crate) next_offset: u64,
    pub(crate) eof: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StoreError {
    EntryTooLarge {
        size_bytes: u64,
        max_bytes: u64,
    },
    RangeTooLarge {
        requested: usize,
        max_bytes: usize,
    },
    SearchLimitTooLarge {
        requested: usize,
        max_matches: usize,
    },
    InvalidRangeSize,
    EmptyQuery,
    OffsetPastEnd {
        offset: u64,
        size_bytes: u64,
    },
    SearchRequiresText,
    Unavailable,
    Expired,
    Evicted,
    Io(String),
}

impl StoreError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::EntryTooLarge { .. } => "entry_too_large",
            Self::RangeTooLarge { .. } => "range_too_large",
            Self::SearchLimitTooLarge { .. } => "search_limit_too_large",
            Self::InvalidRangeSize => "invalid_range_size",
            Self::EmptyQuery => "empty_query",
            Self::OffsetPastEnd { .. } => "offset_past_end",
            Self::SearchRequiresText => "search_requires_text",
            Self::Unavailable => "unavailable",
            Self::Expired => "expired",
            Self::Evicted => "evicted",
            Self::Io(_) => "storage_io",
        }
    }
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EntryTooLarge {
                size_bytes,
                max_bytes,
            } => write!(
                f,
                "result payload is {size_bytes} bytes; maximum accepted size is {max_bytes} bytes"
            ),
            Self::RangeTooLarge {
                requested,
                max_bytes,
            } => write!(
                f,
                "requested range is {requested} bytes; maximum is {max_bytes} bytes"
            ),
            Self::SearchLimitTooLarge {
                requested,
                max_matches,
            } => write!(
                f,
                "requested {requested} search matches; maximum is {max_matches}"
            ),
            Self::InvalidRangeSize => write!(f, "range size must be at least 1 byte"),
            Self::EmptyQuery => write!(f, "search query must not be empty"),
            Self::OffsetPastEnd { offset, size_bytes } => {
                write!(f, "offset {offset} is past result size {size_bytes}")
            }
            Self::SearchRequiresText => {
                write!(f, "search is available only for UTF-8 text results")
            }
            Self::Unavailable => write!(f, "result is unknown or unavailable in this session"),
            Self::Expired => write!(f, "result expired"),
            Self::Evicted => write!(f, "result was evicted"),
            Self::Io(message) => write!(f, "large-result storage I/O failed: {message}"),
        }
    }
}

impl std::error::Error for StoreError {}

#[derive(Clone)]
pub(crate) struct LargeResultStore {
    inner: Arc<StoreInner>,
}

struct StoreInner {
    root: PathBuf,
    config: LargeResultStoreConfig,
    state: Mutex<StoreState>,
}

impl Drop for StoreInner {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

struct StoreState {
    entries: HashMap<String, Entry>,
    insertion_order: VecDeque<String>,
    tombstones: HashMap<String, Tombstone>,
    tombstone_order: VecDeque<String>,
    total_live_bytes: u64,
}

struct Entry {
    metadata: ResultMetadata,
    owner_session: Option<String>,
    workspace: PathBuf,
    path: PathBuf,
    expires_at: Instant,
}

#[derive(Clone, Copy)]
enum TombstoneState {
    Expired,
    Evicted,
}

struct Tombstone {
    owner_session: Option<String>,
    workspace: PathBuf,
    state: TombstoneState,
}

impl LargeResultStore {
    pub(crate) fn new(config: LargeResultStoreConfig) -> Result<Self, StoreError> {
        if config.max_entry_bytes == 0
            || config.max_total_bytes == 0
            || config.max_range_bytes == 0
            || config.max_search_matches == 0
            || config.search_chunk_bytes == 0
            || config.tombstone_limit == 0
        {
            return Err(StoreError::Io("invalid zero-sized store limit".into()));
        }

        let base = std::env::temp_dir().join("catdesk-large-results");
        std::fs::create_dir_all(&base).map_err(io_error)?;
        let root = base.join(Uuid::new_v4().to_string());
        create_private_dir(&root)?;

        Ok(Self {
            inner: Arc::new(StoreInner {
                root,
                config,
                state: Mutex::new(StoreState {
                    entries: HashMap::new(),
                    insertion_order: VecDeque::new(),
                    tombstones: HashMap::new(),
                    tombstone_order: VecDeque::new(),
                    total_live_bytes: 0,
                }),
            }),
        })
    }

    pub(crate) fn new_default() -> Result<Self, StoreError> {
        Self::new(LargeResultStoreConfig::default())
    }

    pub(crate) fn max_range_bytes(&self) -> usize {
        self.inner.config.max_range_bytes
    }

    pub(crate) fn max_search_matches(&self) -> usize {
        self.inner.config.max_search_matches
    }

    pub(crate) fn put(
        &self,
        owner_session: Option<&str>,
        workspace_root: &Path,
        bytes: &[u8],
        content_type: Option<&str>,
    ) -> Result<StoredResult, StoreError> {
        let size_bytes = bytes.len() as u64;
        let max_accepted = self
            .inner
            .config
            .max_entry_bytes
            .min(self.inner.config.max_total_bytes);
        if size_bytes > max_accepted {
            return Err(StoreError::EntryTooLarge {
                size_bytes,
                max_bytes: max_accepted,
            });
        }

        let workspace = workspace_identity(workspace_root)?;
        let result_id = format!("result_{}", Uuid::new_v4().simple());
        let path = self.inner.root.join(&result_id);
        write_private_file(&path, bytes)?;

        let now_instant = Instant::now();
        let created_at_ms = unix_now_ms();
        let ttl_ms = self.inner.config.ttl.as_millis().min(u64::MAX as u128) as u64;
        let expires_at_ms = created_at_ms.saturating_add(ttl_ms);
        let kind = if std::str::from_utf8(bytes).is_ok() {
            ResultKind::Text
        } else {
            ResultKind::Binary
        };
        let metadata = ResultMetadata {
            result_id: result_id.clone(),
            size_bytes,
            kind,
            content_type: content_type.map(str::to_owned),
            created_at_ms,
            expires_at_ms,
        };

        let mut state = self.lock_state();
        self.cleanup_expired_locked(&mut state, now_instant);
        let mut evicted_result_ids = Vec::new();
        while state.total_live_bytes.saturating_add(size_bytes) > self.inner.config.max_total_bytes
        {
            let Some(oldest_id) = next_live_id(&mut state) else {
                let _ = std::fs::remove_file(&path);
                return Err(StoreError::Io(
                    "global result-store capacity is inconsistent".into(),
                ));
            };
            if self.remove_live_locked(&mut state, &oldest_id, TombstoneState::Evicted) {
                evicted_result_ids.push(oldest_id);
            }
        }

        state.total_live_bytes = state.total_live_bytes.saturating_add(size_bytes);
        state.insertion_order.push_back(result_id.clone());
        state.entries.insert(
            result_id,
            Entry {
                metadata: metadata.clone(),
                owner_session: owner_session.map(str::to_owned),
                workspace,
                path,
                expires_at: now_instant
                    .checked_add(self.inner.config.ttl)
                    .unwrap_or(now_instant),
            },
        );

        Ok(StoredResult {
            metadata,
            evicted_result_ids,
        })
    }

    pub(crate) fn read_range(
        &self,
        owner_session: Option<&str>,
        workspace_root: &Path,
        result_id: &str,
        offset: u64,
        max_bytes: usize,
    ) -> Result<RangeResult, StoreError> {
        if max_bytes == 0 {
            return Err(StoreError::InvalidRangeSize);
        }
        if max_bytes > self.inner.config.max_range_bytes {
            return Err(StoreError::RangeTooLarge {
                requested: max_bytes,
                max_bytes: self.inner.config.max_range_bytes,
            });
        }

        let workspace = workspace_identity(workspace_root)?;
        let mut state = self.lock_state();
        self.cleanup_expired_locked(&mut state, Instant::now());
        let entry = self.entry_for_scope(&state, owner_session, &workspace, result_id)?;

        if offset > entry.metadata.size_bytes {
            return Err(StoreError::OffsetPastEnd {
                offset,
                size_bytes: entry.metadata.size_bytes,
            });
        }

        let remaining = entry.metadata.size_bytes.saturating_sub(offset);
        let to_read = remaining.min(max_bytes as u64) as usize;
        let mut file = File::open(&entry.path).map_err(io_error)?;
        file.seek(SeekFrom::Start(offset)).map_err(io_error)?;
        let mut bytes = vec![0_u8; to_read];
        file.read_exact(&mut bytes).map_err(io_error)?;
        let next_offset = offset.saturating_add(bytes.len() as u64);
        let text = std::str::from_utf8(&bytes).ok().map(str::to_owned);

        Ok(RangeResult {
            metadata: entry.metadata.clone(),
            offset,
            bytes,
            text,
            next_offset,
            eof: next_offset == entry.metadata.size_bytes,
        })
    }

    pub(crate) fn search(
        &self,
        owner_session: Option<&str>,
        workspace_root: &Path,
        result_id: &str,
        query: &str,
        start_offset: u64,
        max_matches: usize,
    ) -> Result<SearchResult, StoreError> {
        if query.is_empty() {
            return Err(StoreError::EmptyQuery);
        }
        if max_matches == 0 || max_matches > self.inner.config.max_search_matches {
            return Err(StoreError::SearchLimitTooLarge {
                requested: max_matches,
                max_matches: self.inner.config.max_search_matches,
            });
        }

        let workspace = workspace_identity(workspace_root)?;
        let mut state = self.lock_state();
        self.cleanup_expired_locked(&mut state, Instant::now());
        let entry = self.entry_for_scope(&state, owner_session, &workspace, result_id)?;

        if entry.metadata.kind != ResultKind::Text {
            return Err(StoreError::SearchRequiresText);
        }
        if start_offset > entry.metadata.size_bytes {
            return Err(StoreError::OffsetPastEnd {
                offset: start_offset,
                size_bytes: entry.metadata.size_bytes,
            });
        }
        if start_offset == entry.metadata.size_bytes {
            return Ok(SearchResult {
                metadata: entry.metadata.clone(),
                matches: Vec::new(),
                next_offset: start_offset,
                eof: true,
            });
        }

        let pattern = query.as_bytes();
        let prefix = kmp_prefix(pattern);
        let mut file = File::open(&entry.path).map_err(io_error)?;
        file.seek(SeekFrom::Start(start_offset)).map_err(io_error)?;

        let mut buffer = vec![0_u8; self.inner.config.search_chunk_bytes];
        let mut absolute = start_offset;
        let mut matched = 0_usize;
        let mut matches = Vec::new();

        loop {
            let read = file.read(&mut buffer).map_err(io_error)?;
            if read == 0 {
                return Ok(SearchResult {
                    metadata: entry.metadata.clone(),
                    matches,
                    next_offset: entry.metadata.size_bytes,
                    eof: true,
                });
            }

            for (index, byte) in buffer[..read].iter().copied().enumerate() {
                while matched > 0 && pattern[matched] != byte {
                    matched = prefix[matched - 1];
                }
                if pattern[matched] == byte {
                    matched += 1;
                }
                if matched == pattern.len() {
                    let end_offset = absolute + index as u64 + 1;
                    let match_offset = end_offset - pattern.len() as u64;
                    let (snippet_offset, snippet) = read_snippet(
                        &entry.path,
                        entry.metadata.size_bytes,
                        match_offset,
                        end_offset,
                    )?;
                    matches.push(SearchMatch {
                        offset: match_offset,
                        end_offset,
                        snippet_offset,
                        snippet,
                    });
                    matched = prefix[matched - 1];

                    if matches.len() == max_matches {
                        return Ok(SearchResult {
                            metadata: entry.metadata.clone(),
                            next_offset: match_offset.saturating_add(1),
                            matches,
                            eof: false,
                        });
                    }
                }
            }
            absolute = absolute.saturating_add(read as u64);
        }
    }

    pub(crate) fn remove_session(&self, owner_session: Option<&str>) -> usize {
        let mut state = self.lock_state();
        self.cleanup_expired_locked(&mut state, Instant::now());
        let ids = state
            .entries
            .iter()
            .filter(|(_, entry)| entry.owner_session.as_deref() == owner_session)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        let mut removed = 0;
        for id in ids {
            if self.remove_live_locked(&mut state, &id, TombstoneState::Evicted) {
                removed += 1;
            }
        }
        removed
    }

    fn lock_state(&self) -> MutexGuard<'_, StoreState> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn cleanup_expired_locked(&self, state: &mut StoreState, now: Instant) {
        let expired = state
            .entries
            .iter()
            .filter(|(_, entry)| entry.expires_at <= now)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in expired {
            self.remove_live_locked(state, &id, TombstoneState::Expired);
        }
    }

    fn remove_live_locked(
        &self,
        state: &mut StoreState,
        result_id: &str,
        tombstone_state: TombstoneState,
    ) -> bool {
        let Some(entry) = state.entries.remove(result_id) else {
            return false;
        };
        state.total_live_bytes = state
            .total_live_bytes
            .saturating_sub(entry.metadata.size_bytes);
        let _ = std::fs::remove_file(&entry.path);
        self.record_tombstone_locked(
            state,
            result_id.to_string(),
            Tombstone {
                owner_session: entry.owner_session,
                workspace: entry.workspace,
                state: tombstone_state,
            },
        );
        true
    }

    fn record_tombstone_locked(
        &self,
        state: &mut StoreState,
        result_id: String,
        tombstone: Tombstone,
    ) {
        state.tombstones.insert(result_id.clone(), tombstone);
        state.tombstone_order.push_back(result_id);
        while state.tombstone_order.len() > self.inner.config.tombstone_limit {
            if let Some(oldest) = state.tombstone_order.pop_front() {
                state.tombstones.remove(&oldest);
            }
        }
    }

    fn entry_for_scope<'a>(
        &self,
        state: &'a StoreState,
        owner_session: Option<&str>,
        workspace: &Path,
        result_id: &str,
    ) -> Result<&'a Entry, StoreError> {
        if let Some(entry) = state.entries.get(result_id) {
            if entry.owner_session.as_deref() == owner_session && entry.workspace == workspace {
                return Ok(entry);
            }
            return Err(StoreError::Unavailable);
        }

        if let Some(tombstone) = state.tombstones.get(result_id)
            && tombstone.owner_session.as_deref() == owner_session
            && tombstone.workspace == workspace
        {
            return Err(match tombstone.state {
                TombstoneState::Expired => StoreError::Expired,
                TombstoneState::Evicted => StoreError::Evicted,
            });
        }
        Err(StoreError::Unavailable)
    }

    #[cfg(test)]
    fn force_expire_for_test(&self, result_id: &str) {
        let mut state = self.lock_state();
        if let Some(entry) = state.entries.get_mut(result_id) {
            entry.expires_at = Instant::now()
                .checked_sub(Duration::from_secs(1))
                .unwrap_or_else(Instant::now);
        }
    }

    #[cfg(test)]
    fn storage_root_for_test(&self) -> &Path {
        &self.inner.root
    }

    #[cfg(test)]
    fn payload_path_for_test(&self, result_id: &str) -> PathBuf {
        self.inner.root.join(result_id)
    }
}

fn next_live_id(state: &mut StoreState) -> Option<String> {
    while let Some(id) = state.insertion_order.pop_front() {
        if state.entries.contains_key(&id) {
            return Some(id);
        }
    }
    None
}

fn workspace_identity(path: &Path) -> Result<PathBuf, StoreError> {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return Ok(canonical);
    }
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .map_err(io_error)
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

fn io_error(error: std::io::Error) -> StoreError {
    StoreError::Io(error.kind().to_string())
}

fn create_private_dir(path: &Path) -> Result<(), StoreError> {
    std::fs::create_dir(path).map_err(io_error)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(io_error)?;
    }
    Ok(())
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(io_error)?;
    file.write_all(bytes).map_err(io_error)?;
    file.flush().map_err(io_error)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(io_error)?;
    }
    Ok(())
}

fn kmp_prefix(pattern: &[u8]) -> Vec<usize> {
    let mut prefix = vec![0; pattern.len()];
    let mut matched = 0;
    for index in 1..pattern.len() {
        while matched > 0 && pattern[index] != pattern[matched] {
            matched = prefix[matched - 1];
        }
        if pattern[index] == pattern[matched] {
            matched += 1;
            prefix[index] = matched;
        }
    }
    prefix
}

fn read_snippet(
    path: &Path,
    size_bytes: u64,
    match_offset: u64,
    end_offset: u64,
) -> Result<(u64, String), StoreError> {
    let context_before = (SEARCH_SNIPPET_BYTES / 3) as u64;
    let mut start = match_offset.saturating_sub(context_before);
    let mut end = (start + SEARCH_SNIPPET_BYTES as u64).min(size_bytes);
    if end < end_offset {
        end = end_offset.min(size_bytes);
        start = end.saturating_sub(SEARCH_SNIPPET_BYTES as u64);
    }

    let mut file = File::open(path).map_err(io_error)?;
    file.seek(SeekFrom::Start(start)).map_err(io_error)?;
    let mut bytes = vec![0_u8; (end - start) as usize];
    file.read_exact(&mut bytes).map_err(io_error)?;

    for leading_trim in 0..=bytes.len().min(3) {
        let candidate = &bytes[leading_trim..];
        match std::str::from_utf8(candidate) {
            Ok(text) => return Ok((start + leading_trim as u64, text.to_string())),
            Err(error) if error.valid_up_to() > 0 => {
                let valid = &candidate[..error.valid_up_to()];
                let text = std::str::from_utf8(valid).map_err(|_| {
                    StoreError::Io("stored UTF-8 result became undecodable".to_string())
                })?;
                return Ok((start + leading_trim as u64, text.to_string()));
            }
            Err(_) => {}
        }
    }

    Err(StoreError::Io(
        "stored UTF-8 result became undecodable".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn config(max_entry_bytes: u64, max_total_bytes: u64) -> LargeResultStoreConfig {
        LargeResultStoreConfig {
            ttl: Duration::from_secs(3600),
            max_entry_bytes,
            max_total_bytes,
            max_range_bytes: 128 * 1024,
            max_search_matches: 100,
            search_chunk_bytes: 32,
            tombstone_limit: 16,
        }
    }

    fn workspace(label: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "catdesk-result-store-test-{label}-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).expect("create workspace");
        root
    }

    #[test]
    fn stores_text_and_reads_deterministic_ranges_through_eof() {
        let ws = workspace("ranges");
        let store = LargeResultStore::new(config(1024, 4096)).expect("create store");
        let stored = store
            .put(
                Some("session-a"),
                &ws,
                "zażółć gęślą jaźń".as_bytes(),
                Some("text/plain"),
            )
            .expect("store text");

        assert_eq!(stored.metadata.size_bytes, "zażółć gęślą jaźń".len() as u64);
        assert_eq!(stored.metadata.kind, ResultKind::Text);
        assert_eq!(stored.metadata.content_type.as_deref(), Some("text/plain"));
        assert!(stored.evicted_result_ids.is_empty());

        let first = store
            .read_range(Some("session-a"), &ws, &stored.metadata.result_id, 0, 5)
            .expect("first range");
        assert_eq!(first.offset, 0);
        assert_eq!(first.bytes.len(), 5);
        assert_eq!(first.next_offset, 5);
        assert!(!first.eof);

        let tail = store
            .read_range(
                Some("session-a"),
                &ws,
                &stored.metadata.result_id,
                first.next_offset,
                128,
            )
            .expect("tail range");
        assert_eq!(tail.next_offset, stored.metadata.size_bytes);
        assert!(tail.eof);

        let eof = store
            .read_range(
                Some("session-a"),
                &ws,
                &stored.metadata.result_id,
                stored.metadata.size_bytes,
                128,
            )
            .expect("eof range");
        assert!(eof.bytes.is_empty());
        assert_eq!(eof.next_offset, stored.metadata.size_bytes);
        assert!(eof.eof);

        let err = store
            .read_range(
                Some("session-a"),
                &ws,
                &stored.metadata.result_id,
                stored.metadata.size_bytes + 1,
                1,
            )
            .expect_err("offset past eof must fail");
        assert!(matches!(err, StoreError::OffsetPastEnd { .. }));

        std::fs::remove_dir_all(ws).ok();
    }

    #[test]
    fn binary_metadata_and_ranges_preserve_exact_bytes() {
        let ws = workspace("binary");
        let store = LargeResultStore::new(config(1024, 4096)).expect("create store");
        let payload = [0_u8, 159, 146, 150, 255, 1, 2, 3];
        let stored = store
            .put(
                Some("session-a"),
                &ws,
                &payload,
                Some("application/octet-stream"),
            )
            .expect("store binary");

        assert_eq!(stored.metadata.kind, ResultKind::Binary);
        let range = store
            .read_range(Some("session-a"), &ws, &stored.metadata.result_id, 1, 5)
            .expect("read binary range");
        assert_eq!(range.bytes, payload[1..6]);

        std::fs::remove_dir_all(ws).ok();
    }

    #[test]
    fn session_and_workspace_boundaries_hide_foreign_results() {
        let ws_a = workspace("scope-a");
        let ws_b = workspace("scope-b");
        let store = LargeResultStore::new(config(1024, 4096)).expect("create store");
        let stored = store
            .put(Some("session-a"), &ws_a, b"secret payload", None)
            .expect("store payload");

        for (session, workspace) in [
            (Some("session-b"), ws_a.as_path()),
            (Some("session-a"), ws_b.as_path()),
            (None, ws_a.as_path()),
        ] {
            let err = store
                .read_range(session, workspace, &stored.metadata.result_id, 0, 16)
                .expect_err("foreign lookup must fail");
            assert_eq!(err, StoreError::Unavailable);
        }

        std::fs::remove_dir_all(ws_a).ok();
        std::fs::remove_dir_all(ws_b).ok();
    }

    #[test]
    fn caps_reject_oversize_and_evict_oldest_with_explicit_tombstone() {
        let ws = workspace("caps");
        let store = LargeResultStore::new(config(8, 10)).expect("create store");

        let too_large = store
            .put(Some("session-a"), &ws, b"123456789", None)
            .expect_err("per-entry cap must reject");
        assert!(matches!(
            too_large,
            StoreError::EntryTooLarge {
                size_bytes: 9,
                max_bytes: 8
            }
        ));

        let first = store
            .put(Some("session-a"), &ws, b"123456", None)
            .expect("first");
        let second = store
            .put(Some("session-a"), &ws, b"abcdef", None)
            .expect("second");
        assert_eq!(
            second.evicted_result_ids,
            vec![first.metadata.result_id.clone()]
        );

        let own = store
            .read_range(Some("session-a"), &ws, &first.metadata.result_id, 0, 1)
            .expect_err("evicted ref must be explicit");
        assert_eq!(own, StoreError::Evicted);

        let foreign = store
            .read_range(Some("session-b"), &ws, &first.metadata.result_id, 0, 1)
            .expect_err("foreign caller must not learn tombstone state");
        assert_eq!(foreign, StoreError::Unavailable);

        let still_live = store
            .read_range(Some("session-a"), &ws, &second.metadata.result_id, 0, 6)
            .expect("newest remains live");
        assert_eq!(still_live.bytes, b"abcdef");

        std::fs::remove_dir_all(ws).ok();
    }

    #[test]
    fn ttl_cleanup_reports_expired_and_session_removal_reports_evicted() {
        let ws = workspace("ttl");
        let store = LargeResultStore::new(config(1024, 4096)).expect("create store");
        let expired = store
            .put(Some("session-a"), &ws, b"old", None)
            .expect("store expired candidate");
        store.force_expire_for_test(&expired.metadata.result_id);

        assert_eq!(
            store
                .read_range(Some("session-a"), &ws, &expired.metadata.result_id, 0, 1)
                .expect_err("expired"),
            StoreError::Expired
        );

        let removed = store
            .put(Some("session-a"), &ws, b"remove me", None)
            .expect("store removal candidate");
        let other = store
            .put(Some("session-b"), &ws, b"keep me", None)
            .expect("store other session");
        assert_eq!(store.remove_session(Some("session-a")), 1);
        assert_eq!(
            store
                .read_range(Some("session-a"), &ws, &removed.metadata.result_id, 0, 1)
                .expect_err("removed"),
            StoreError::Evicted
        );
        assert_eq!(
            store
                .read_range(Some("session-b"), &ws, &other.metadata.result_id, 0, 7)
                .expect("other session stays")
                .bytes,
            b"keep me"
        );

        std::fs::remove_dir_all(ws).ok();
    }

    #[test]
    fn literal_search_streams_across_internal_chunk_boundaries() {
        let ws = workspace("search");
        let store = LargeResultStore::new(config(4096, 8192)).expect("create store");
        let mut payload = "x".repeat(31);
        payload.push_str("needle");
        payload.push_str(&"y".repeat(40));
        payload.push_str("needle");
        let stored = store
            .put(
                Some("session-a"),
                &ws,
                payload.as_bytes(),
                Some("text/plain"),
            )
            .expect("store searchable text");

        let first = store
            .search(
                Some("session-a"),
                &ws,
                &stored.metadata.result_id,
                "needle",
                0,
                1,
            )
            .expect("search first");
        assert_eq!(first.matches.len(), 1);
        assert_eq!(first.matches[0].offset, 31);
        assert!(first.matches[0].snippet.contains("needle"));
        assert!(!first.eof);

        let second = store
            .search(
                Some("session-a"),
                &ws,
                &stored.metadata.result_id,
                "needle",
                first.next_offset,
                10,
            )
            .expect("search second");
        assert_eq!(
            second.matches.iter().map(|m| m.offset).collect::<Vec<_>>(),
            vec![77]
        );
        assert!(second.eof);

        let binary = store
            .put(Some("session-a"), &ws, &[0xff, 0x00, 0x01], None)
            .expect("store binary");
        assert_eq!(
            store
                .search(
                    Some("session-a"),
                    &ws,
                    &binary.metadata.result_id,
                    "x",
                    0,
                    1,
                )
                .expect_err("binary search"),
            StoreError::SearchRequiresText
        );

        std::fs::remove_dir_all(ws).ok();
    }

    #[test]
    fn multi_megabyte_payload_reconstructs_without_exceeding_range_cap() {
        let ws = workspace("reconstruct");
        let store =
            LargeResultStore::new(config(4 * 1024 * 1024, 8 * 1024 * 1024)).expect("create store");
        let payload = (0..(3 * 1024 * 1024 + 17))
            .map(|i| ((i * 31) % 251) as u8)
            .collect::<Vec<_>>();
        let stored = store
            .put(Some("session-a"), &ws, &payload, None)
            .expect("store multi-mb");

        let mut rebuilt = Vec::with_capacity(payload.len());
        let mut offset = 0_u64;
        loop {
            let range = store
                .read_range(
                    Some("session-a"),
                    &ws,
                    &stored.metadata.result_id,
                    offset,
                    128 * 1024,
                )
                .expect("paged range");
            assert!(range.bytes.len() <= 128 * 1024);
            rebuilt.extend_from_slice(&range.bytes);
            offset = range.next_offset;
            if range.eof {
                break;
            }
        }

        assert_eq!(rebuilt, payload);
        std::fs::remove_dir_all(ws).ok();
    }

    #[cfg(unix)]
    #[test]
    fn storage_directory_and_payload_files_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let ws = workspace("permissions");
        let store = LargeResultStore::new(config(1024, 4096)).expect("create store");
        let stored = store
            .put(Some("session-a"), &ws, b"private", None)
            .expect("store payload");

        let root_mode = std::fs::metadata(store.storage_root_for_test())
            .expect("root metadata")
            .permissions()
            .mode()
            & 0o777;
        let file_mode = std::fs::metadata(store.payload_path_for_test(&stored.metadata.result_id))
            .expect("payload metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(root_mode, 0o700);
        assert_eq!(file_mode, 0o600);

        std::fs::remove_dir_all(ws).ok();
    }
}
