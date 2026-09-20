//! Durable records for command jobs (docs/command-job-durability.md).
//!
//! One JSON file per job, written atomically at job start and at the terminal
//! transition. Persistence is best-effort: store failures are diagnostic-only
//! and must never prevent command execution.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::command_jobs::CommandJobState;

const JOB_RECORD_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
pub struct JobRecord {
    pub schema_version: u32,
    pub job_id: String,
    pub command: String,
    pub cwd: String,
    pub workspace_root: String,
    #[serde(default)]
    pub owner_session: Option<String>,
    pub timeout_ms: u64,
    pub started_at_ms: u64,
    pub state: CommandJobState,
    pub exit_code: Option<i32>,
    pub finished_at_ms: Option<u64>,
    pub elapsed_ms: Option<u64>,
}

#[derive(Clone, Debug, Default)]
pub struct JobStore {
    dir: Option<Arc<PathBuf>>,
}

impl JobStore {
    pub fn open(dir: PathBuf) -> Self {
        Self {
            dir: Some(Arc::new(dir)),
        }
    }

    pub fn disabled() -> Self {
        Self { dir: None }
    }

    pub fn default_dir() -> Option<PathBuf> {
        crate::state::user_home_dir()
            .ok()
            .map(|home| home.join(".catdesk").join("jobs"))
    }

    pub fn write(&self, record: &JobRecord) {
        let Some(dir) = self.dir.as_deref() else {
            return;
        };
        let result = std::fs::create_dir_all(dir).and_then(|()| {
            let final_path = dir.join(format!("{}.json", record.job_id));
            let tmp_path = dir.join(format!("{}.json.tmp", record.job_id));
            let payload = serde_json::to_vec(record).map_err(std::io::Error::other)?;
            std::fs::write(&tmp_path, payload)?;
            std::fs::rename(&tmp_path, &final_path)
        });
        if result.is_err() {
            crate::diagnostics::event("job_store_write_failed");
        }
    }

    pub fn read_all(&self) -> Vec<JobRecord> {
        let Some(dir) = self.dir.as_deref() else {
            return Vec::new();
        };
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(_) => return Vec::new(),
        };
        let mut records = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            match Self::parse(&path) {
                Some(record) => records.push(record),
                None => {
                    let _ = std::fs::rename(&path, path.with_extension("json.corrupt"));
                    crate::diagnostics::event("job_store_record_corrupt");
                }
            }
        }
        records
    }

    pub fn remove(&self, job_id: &str) {
        let Some(dir) = self.dir.as_deref() else {
            return;
        };
        let _ = std::fs::remove_file(dir.join(format!("{job_id}.json")));
    }

    fn parse(path: &Path) -> Option<JobRecord> {
        let bytes = std::fs::read(path).ok()?;
        let record: JobRecord = serde_json::from_slice(&bytes).ok()?;
        (record.schema_version == JOB_RECORD_SCHEMA_VERSION).then_some(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (JobStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!("catdesk-jobstore-{}", uuid::Uuid::new_v4()));
        (JobStore::open(dir.clone()), dir)
    }

    fn sample_record(state: CommandJobState) -> JobRecord {
        JobRecord {
            schema_version: 1,
            job_id: "job-1".into(),
            command: "sleep 1".into(),
            cwd: "/w".into(),
            workspace_root: "/w".into(),
            owner_session: None,
            timeout_ms: 60_000,
            started_at_ms: 1_000,
            state,
            exit_code: Some(0),
            finished_at_ms: Some(2_000),
            elapsed_ms: Some(1_000),
        }
    }

    #[test]
    fn record_roundtrip_through_disk() {
        let (store, dir) = temp_store();
        store.write(&sample_record(CommandJobState::Succeeded));
        let records = store.read_all();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].job_id, "job-1");
        assert_eq!(records[0].state, CommandJobState::Succeeded);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn write_leaves_no_temp_file() {
        let (store, dir) = temp_store();
        store.write(&sample_record(CommandJobState::Running));
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .expect("read dir")
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left behind");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn corrupt_record_is_renamed_aside_and_skipped() {
        let (store, dir) = temp_store();
        std::fs::create_dir_all(&dir).expect("create dir");
        std::fs::write(dir.join("bad.json"), b"{ not json").expect("write bad record");
        store.write(&sample_record(CommandJobState::Running));
        let records = store.read_all();
        assert_eq!(records.len(), 1);
        assert!(dir.join("bad.json.corrupt").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn disabled_store_is_noop() {
        let store = JobStore::disabled();
        store.write(&sample_record(CommandJobState::Running));
        assert!(store.read_all().is_empty());
        store.remove("job-1");
    }

    #[test]
    fn missing_directory_reads_empty() {
        let dir = std::env::temp_dir().join(format!("catdesk-jobstore-{}", uuid::Uuid::new_v4()));
        let store = JobStore::open(dir.clone());
        assert!(store.read_all().is_empty());
    }
}
