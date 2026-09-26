use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex as StdMutex, OnceLock};
use std::time::SystemTime;

use crate::perf_metrics::{self, CacheKind};
use crate::state::{
    AgentsPathMode, AppConfig, app_config_path, load_app_config, user_home_dir,
};

fn workspace_agents_path(workspace_root: &str) -> PathBuf {
    Path::new(workspace_root).join("AGENTS.md")
}

fn catdesk_agents_path() -> std::io::Result<PathBuf> {
    Ok(user_home_dir()?.join(".catdesk").join("AGENTS.md"))
}

fn codex_agents_path() -> PathBuf {
    user_home_dir()
        .unwrap_or_default()
        .join(".codex")
        .join("AGENTS.md")
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum FileStamp {
    Missing,
    Present { len: u64, modified: SystemTime },
}

fn file_stamp(path: &Path) -> std::io::Result<FileStamp> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(FileStamp::Present {
            len: metadata.len(),
            modified: metadata.modified()?,
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(FileStamp::Missing),
        Err(error) => Err(error),
    }
}

#[derive(Clone)]
pub(crate) struct CachedFileValue<T> {
    stamp: FileStamp,
    value: T,
}

const MAX_METADATA_CACHE_ENTRIES: usize = 128;

pub(crate) fn cached_file_value<T: Clone>(
    kind: CacheKind,
    cache: &StdMutex<HashMap<PathBuf, CachedFileValue<T>>>,
    path: &Path,
    load: impl FnOnce() -> std::io::Result<T>,
) -> std::io::Result<T> {
    let stamp = match file_stamp(path) {
        Ok(stamp) => stamp,
        Err(_) => {
            perf_metrics::record_cache_miss(kind);
            return load();
        }
    };
    {
        let guard = cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(entry) = guard.get(path).filter(|entry| entry.stamp == stamp) {
            perf_metrics::record_cache_hit(kind);
            return Ok(entry.value.clone());
        }
    }

    perf_metrics::record_cache_miss(kind);
    let value = load()?;
    let mut guard = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.len() >= MAX_METADATA_CACHE_ENTRIES && !guard.contains_key(path) {
        guard.clear();
    }
    guard.insert(
        path.to_path_buf(),
        CachedFileValue {
            stamp,
            value: value.clone(),
        },
    );
    Ok(value)
}

static APP_CONFIG_CACHE: OnceLock<StdMutex<HashMap<PathBuf, CachedFileValue<AppConfig>>>> =
    OnceLock::new();
static AGENTS_TEXT_CACHE: OnceLock<StdMutex<HashMap<PathBuf, CachedFileValue<Option<String>>>>> =
    OnceLock::new();

pub(crate) fn cached_app_config() -> std::io::Result<AppConfig> {
    let path = app_config_path()?;
    let cache = APP_CONFIG_CACHE.get_or_init(|| StdMutex::new(HashMap::new()));
    cached_file_value(CacheKind::AppConfig, cache, &path, load_app_config)
}

#[derive(Clone)]
pub(crate) struct AgentsOptionState {
    path: PathBuf,
    path_string: String,
    display_path: String,
    available: bool,
}

#[derive(Clone)]
pub(crate) struct AgentsWidgetState {
    pub(crate) mode: AgentsPathMode,
    pub(crate) current_path_string: String,
    pub(crate) current_display_path: String,
    pub(crate) resolved_path: Option<PathBuf>,
    pub(crate) workspace: AgentsOptionState,
    pub(crate) catdesk: AgentsOptionState,
    pub(crate) codex: AgentsOptionState,
}

fn agents_option_state(path: PathBuf) -> AgentsOptionState {
    let (path_string, display_path) = widget_path_strings(&path);
    AgentsOptionState {
        available: path.is_file(),
        path,
        path_string,
        display_path,
    }
}

pub(crate) fn agents_widget_state(workspace_root: &str) -> std::io::Result<AgentsWidgetState> {
    let mode = cached_app_config()?.agents_path_mode;
    let workspace = agents_option_state(workspace_agents_path(workspace_root));
    let catdesk = agents_option_state(catdesk_agents_path()?);
    let codex = agents_option_state(codex_agents_path());

    let (current_path_string, current_display_path, resolved_path) = match mode {
        AgentsPathMode::Default => {
            let resolved = if workspace.available {
                Some(workspace.path.clone())
            } else if catdesk.available {
                Some(catdesk.path.clone())
            } else if codex.available {
                Some(codex.path.clone())
            } else {
                None
            };
            if let Some(path) = resolved.as_ref() {
                let (path_string, display_path) = widget_path_strings(path);
                (path_string, display_path, resolved)
            } else {
                ("-".to_string(), "-".to_string(), None)
            }
        }
        AgentsPathMode::Workspace => (
            workspace.path_string.clone(),
            workspace.display_path.clone(),
            workspace.available.then_some(workspace.path.clone()),
        ),
        AgentsPathMode::Catdesk => (
            catdesk.path_string.clone(),
            catdesk.display_path.clone(),
            catdesk.available.then_some(catdesk.path.clone()),
        ),
        AgentsPathMode::Codex => (
            codex.path_string.clone(),
            codex.display_path.clone(),
            codex.available.then_some(codex.path.clone()),
        ),
        AgentsPathMode::Disabled => ("-".to_string(), "(disabled)".to_string(), None),
    };

    Ok(AgentsWidgetState {
        mode,
        current_path_string,
        current_display_path,
        resolved_path,
        workspace,
        catdesk,
        codex,
    })
}

pub(crate) fn agents_widget_state_payload(workspace_root: &str) -> std::io::Result<Value> {
    let state = agents_widget_state(workspace_root)?;
    Ok(json!({
        "agentsPathMode": state.mode,
        "agentsPath": state.current_path_string,
        "agentsPathDisplay": state.current_display_path,
        "agentsWorkspacePath": state.workspace.path_string,
        "agentsWorkspacePathDisplay": state.workspace.display_path,
        "agentsWorkspaceAvailable": state.workspace.available,
        "agentsCatdeskPath": state.catdesk.path_string,
        "agentsCatdeskPathDisplay": state.catdesk.display_path,
        "agentsCatdeskAvailable": state.catdesk.available,
        "agentsCodexPath": state.codex.path_string,
        "agentsCodexPathDisplay": state.codex.display_path,
        "agentsCodexAvailable": state.codex.available,
    }))
}

fn read_agents_text_result(path: &Path) -> std::io::Result<Option<String>> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let trimmed = content.trim();
    Ok((!trimmed.is_empty()).then(|| trimmed.to_string()))
}

pub(crate) fn cached_agents_text(path: &Path) -> Option<String> {
    let cache = AGENTS_TEXT_CACHE.get_or_init(|| StdMutex::new(HashMap::new()));
    cached_file_value(CacheKind::AgentsText, cache, path, || {
        read_agents_text_result(path)
    })
    .ok()
    .flatten()
}

fn display_path_with_tilde(path: &Path) -> String {
    let full_path = path.to_string_lossy().to_string();
    let Ok(home_dir) = user_home_dir() else {
        return full_path;
    };
    if path == home_dir {
        return "~".to_string();
    }
    let Ok(relative_path) = path.strip_prefix(&home_dir) else {
        return full_path;
    };
    if relative_path.as_os_str().is_empty() {
        return "~".to_string();
    }
    Path::new("~")
        .join(relative_path)
        .to_string_lossy()
        .to_string()
}

pub(crate) fn widget_path_strings(path: &Path) -> (String, String) {
    (
        path.to_string_lossy().to_string(),
        display_path_with_tilde(path),
    )
}

