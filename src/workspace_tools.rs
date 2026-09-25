use crate::command;
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use image::imageops::FilterType;
use image::{ImageFormat, ImageReader};
use regex::{Regex, RegexBuilder};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::fs;
use std::io::{BufRead, BufReader, Cursor, Read};
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// Per-file cap; MAX_READ_BATCH_BYTES caps the whole batch.
const MAX_READ_BYTES: usize = 512 * 1024;
pub const MAX_READ_BATCH_FILES: usize = 32;
pub const MAX_READ_BATCH_BYTES: usize = 512 * 1024;
pub const DEFAULT_IMAGE_MAX_WIDTH: u32 = 1600;
pub const DEFAULT_IMAGE_MAX_HEIGHT: u32 = 1600;
pub const MAX_IMAGE_OUTPUT_DIMENSION: u32 = 4096;
pub const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;
pub const MAX_IMAGE_PIXELS: u64 = 40_000_000;
const MAX_WRITE_BYTES: usize = 512 * 1024;
const DEFAULT_LIST_LIMIT: usize = 200;
const HARD_LIST_LIMIT: usize = 1000;
const DEFAULT_SEARCH_LIMIT: usize = 100;
const HARD_SEARCH_LIMIT: usize = 500;
const HARD_SEARCH_CONTEXT_LINES: usize = 20;
static RG_AVAILABLE: OnceLock<bool> = OnceLock::new();
static GREP_AVAILABLE: OnceLock<bool> = OnceLock::new();
/// Hard wall-clock bound for one text search. A rare pattern never reaches
/// the `max_matches` cap, and without a deadline rg walks the whole tree
/// (measured: 17+ min per search while six duplicates pinned every CPU and
/// the GLM semaphore starved — 2026-09-20).
const SEARCH_DEADLINE: Duration = Duration::from_secs(60);
/// Excluded from every search unless the caller explicitly passes
/// `no_ignore`. Appended AFTER the caller's glob because rg lets the LAST
/// matching glob decide, so a caller whitelist like `**/*` must not
/// resurrect these.
const DEFAULT_SEARCH_EXCLUDES: &[&str] = &[
    "!**/.git/**",
    "!**/target/**",
    "!**/node_modules/**",
    "!**/.venv/**",
    "!**/__pycache__/**",
];
const MAX_FALLBACK_SEARCH_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_FALLBACK_SEARCH_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
const MAX_FALLBACK_SEARCH_FILES: usize = 2_000;

#[cfg(unix)]
fn filesystem_device(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(path).ok().map(|metadata| metadata.dev())
}

#[cfg(not(unix))]
fn filesystem_device(_path: &Path) -> Option<u64> {
    None
}

fn may_recurse_on_same_filesystem(root_device: Option<u64>, path: &Path) -> bool {
    root_device.is_none_or(|device| filesystem_device(path) == Some(device))
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListFilesEntry {
    pub path: String,
    pub name: String,
    pub kind: String,
    pub depth: usize,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListFilesOutput {
    pub path: String,
    pub item_count: usize,
    pub directory_count: usize,
    pub file_count: usize,
    pub other_count: usize,
    pub truncated: bool,
    pub limit: usize,
    pub entries: Vec<ListFilesEntry>,
}

impl ListFilesOutput {
    pub fn render_text(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("path: {}\n", self.path));
        out.push_str(&format!("items: {}\n\n", self.item_count));
        out.push_str(
            &self
                .entries
                .iter()
                .map(|entry| match entry.kind.as_str() {
                    "dir" => format!("dir  {}/", entry.path),
                    "file" => format!("file {}", entry.path),
                    _ => format!("other {}", entry.path),
                })
                .collect::<Vec<_>>()
                .join("\n"),
        );
        if self.truncated {
            out.push_str(&format!("\n\n[truncated at {} items]", self.limit));
        }
        out
    }
}

struct ReadFileOutput {
    path: String,
    size_bytes: u64,
    line_count: usize,
    text: String,
    truncated: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchTextEntry {
    pub path: String,
    pub line: usize,
    pub text: String,
    pub is_context: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchTextOutput {
    pub pattern: String,
    pub path: String,
    pub backend: String,
    pub backend_note: String,
    pub match_count: usize,
    pub truncated: bool,
    pub limit: usize,
    pub results: Vec<SearchTextEntry>,
}

impl SearchTextOutput {
    pub fn render_text(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("pattern: {}\n", self.pattern));
        out.push_str(&format!("path: {}\n", self.path));
        out.push_str(&format!("backend: {}\n", self.backend));
        if !self.backend_note.is_empty() {
            out.push_str(&format!("backend_note: {}\n", self.backend_note));
        }
        out.push_str(&format!("matches: {}\n\n", self.match_count));
        out.push_str(
            &self
                .results
                .iter()
                .map(|entry| {
                    let separator = if entry.is_context { "-" } else { ":" };
                    format!(
                        "{}{}{}{} {}",
                        entry.path, separator, entry.line, separator, entry.text
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
        );
        if self.truncated {
            out.push_str(&format!("\n\n[truncated at {} matches]", self.limit));
        }
        out
    }
}

pub struct SearchTextOptions<'a> {
    pub pattern: &'a str,
    pub path: Option<&'a str>,
    pub glob: Option<&'a str>,
    pub fixed_strings: bool,
    pub case_insensitive: bool,
    pub context: Option<usize>,
    pub before: Option<usize>,
    pub after: Option<usize>,
    pub max_matches: Option<usize>,
    pub max_matches_per_file: Option<usize>,
    pub include_hidden: bool,
    pub no_ignore: bool,
}

#[derive(Clone, Copy)]
struct ResolvedSearchTextOptions<'a> {
    pattern: &'a str,
    glob: Option<&'a str>,
    fixed_strings: bool,
    case_insensitive: bool,
    before: usize,
    after: usize,
    max_matches: usize,
    max_matches_per_file: Option<usize>,
    include_hidden: bool,
    no_ignore: bool,
}

enum SearchBackendError {
    Unavailable,
    Failed(String),
}

// `Debug` so tests can `.expect()`/`.unwrap_err()` on backend results.
impl std::fmt::Debug for SearchBackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SearchBackendError::Unavailable => write!(f, "SearchBackendError::Unavailable"),
            SearchBackendError::Failed(message) => {
                write!(f, "SearchBackendError::Failed({message:?})")
            }
        }
    }
}

enum SearchMatcher {
    Fixed {
        pattern: String,
        case_insensitive: bool,
    },
    Regex(Regex),
}

impl SearchMatcher {
    fn new(options: ResolvedSearchTextOptions<'_>) -> Result<Self, String> {
        if options.fixed_strings {
            let pattern = if options.case_insensitive {
                options.pattern.to_lowercase()
            } else {
                options.pattern.to_string()
            };
            return Ok(Self::Fixed {
                pattern,
                case_insensitive: options.case_insensitive,
            });
        }

        let regex = RegexBuilder::new(options.pattern)
            .case_insensitive(options.case_insensitive)
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self::Regex(regex))
    }

    fn is_match(&self, line: &str) -> bool {
        match self {
            Self::Fixed {
                pattern,
                case_insensitive,
            } => {
                if *case_insensitive {
                    line.to_lowercase().contains(pattern)
                } else {
                    line.contains(pattern)
                }
            }
            Self::Regex(regex) => regex.is_match(line),
        }
    }
}

fn workspace_root_path(workspace_root: &str) -> Result<PathBuf, String> {
    Path::new(workspace_root)
        .canonicalize()
        .map(command::normalize_windows_verbatim_path)
        .map_err(|e| e.to_string())
}

fn to_workspace_relative(root: &Path, path: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(rel) if rel.as_os_str().is_empty() => ".".into(),
        Ok(rel) => tool_path_string(rel),
        Err(_) => tool_path_string(path),
    }
}

fn tool_path_string(path: &Path) -> String {
    let path = path.display().to_string();
    #[cfg(windows)]
    {
        path.replace('\\', "/")
    }
    #[cfg(not(windows))]
    {
        path
    }
}

fn safe_limit(value: Option<usize>, default_value: usize, hard_max: usize) -> usize {
    value.unwrap_or(default_value).clamp(1, hard_max)
}

fn resolve_target_path(workspace_root: &str, path: &str) -> Result<PathBuf, String> {
    command::resolve_workspace_path(workspace_root, Some(path))
}

/// Planning and the read share this check so an unreadable path fails the same
/// way from either one, rather than with whatever the open happened to return.
fn readable_size(target: &Path) -> Result<u64, String> {
    let metadata = fs::metadata(target).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => format!("File not found: {}", target.display()),
        _ => format!("{e}: {}", target.display()),
    })?;
    if !metadata.is_file() {
        return Err(format!("Not a file: {}", target.display()));
    }
    Ok(metadata.len())
}

fn image_mime_type(format: ImageFormat) -> Result<&'static str, String> {
    match format {
        ImageFormat::Png => Ok("image/png"),
        ImageFormat::Jpeg => Ok("image/jpeg"),
        ImageFormat::WebP => Ok("image/webp"),
        _ => Err(format!(
            "Unsupported image format: {format:?}. Supported formats are PNG, JPEG, and WebP."
        )),
    }
}

fn resolve_image_limit(name: &str, value: Option<u32>, default_value: u32) -> Result<u32, String> {
    let value = value.unwrap_or(default_value);
    if value == 0 {
        return Err(format!("{name} must be at least 1"));
    }
    if value > MAX_IMAGE_OUTPUT_DIMENSION {
        return Err(format!(
            "{name} must not exceed {MAX_IMAGE_OUTPUT_DIMENSION}"
        ));
    }
    Ok(value)
}

#[derive(Clone, Debug)]
pub struct ReadImageOutput {
    pub path: String,
    pub mime_type: String,
    pub size_bytes: u64,
    pub width: u32,
    pub height: u32,
    pub original_width: u32,
    pub original_height: u32,
    pub resized: bool,
    pub data: Vec<u8>,
}

pub fn read_image(
    workspace_root: &str,
    path: &str,
    max_width: Option<u32>,
    max_height: Option<u32>,
) -> Result<ReadImageOutput, String> {
    let root = workspace_root_path(workspace_root)?;
    let target = resolve_target_path(workspace_root, path)?;
    let size_bytes = readable_size(&target)?;
    if size_bytes > MAX_IMAGE_BYTES {
        return Err(format!(
            "Image is too large: {size_bytes} bytes exceeds the {MAX_IMAGE_BYTES}-byte limit"
        ));
    }

    let mut file =
        fs::File::open(&target).map_err(|error| format!("{error}: {}", target.display()))?;
    let mut bytes = Vec::with_capacity(size_bytes.min(MAX_IMAGE_BYTES) as usize);
    file.by_ref()
        .take(MAX_IMAGE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("{error}: {}", target.display()))?;
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        return Err(format!(
            "Image is too large: more than {MAX_IMAGE_BYTES} bytes"
        ));
    }

    let format = image::guess_format(&bytes)
        .map_err(|error| format!("Unsupported or invalid image: {error}"))?;
    let mime_type = image_mime_type(format)?;

    let (original_width, original_height) =
        ImageReader::with_format(Cursor::new(bytes.as_slice()), format)
            .into_dimensions()
            .map_err(|error| format!("Failed to read image dimensions: {error}"))?;
    let pixels = u64::from(original_width)
        .checked_mul(u64::from(original_height))
        .ok_or_else(|| "Image dimensions overflow the pixel limit calculation".to_string())?;
    if pixels > MAX_IMAGE_PIXELS {
        return Err(format!(
            "Image dimensions are too large: {original_width}x{original_height} ({pixels} pixels) exceeds the {MAX_IMAGE_PIXELS}-pixel limit"
        ));
    }

    let max_width = resolve_image_limit("max_width", max_width, DEFAULT_IMAGE_MAX_WIDTH)?;
    let max_height = resolve_image_limit("max_height", max_height, DEFAULT_IMAGE_MAX_HEIGHT)?;
    let resized = original_width > max_width || original_height > max_height;

    let (data, width, height) = if resized {
        let decoded = image::load_from_memory_with_format(&bytes, format)
            .map_err(|error| format!("Failed to decode image: {error}"))?;
        let resized_image = decoded.resize(max_width, max_height, FilterType::Lanczos3);
        let width = resized_image.width();
        let height = resized_image.height();
        let mut encoded = Cursor::new(Vec::new());
        resized_image
            .write_to(&mut encoded, format)
            .map_err(|error| format!("Failed to encode resized image: {error}"))?;
        let data = encoded.into_inner();
        if data.len() as u64 > MAX_IMAGE_BYTES {
            return Err(format!(
                "Resized image is too large to return: {} bytes exceeds the {MAX_IMAGE_BYTES}-byte limit",
                data.len()
            ));
        }
        (data, width, height)
    } else {
        (bytes, original_width, original_height)
    };

    Ok(ReadImageOutput {
        path: to_workspace_relative(&root, &target),
        mime_type: mime_type.to_string(),
        size_bytes,
        width,
        height,
        original_width,
        original_height,
        resized,
        data,
    })
}

fn read_file(workspace_root: &str, path: &str, budget: usize) -> Result<ReadFileOutput, String> {
    let root = workspace_root_path(workspace_root)?;
    let target = resolve_target_path(workspace_root, path)?;
    let size_bytes = readable_size(&target)?;
    let mut file =
        fs::File::open(&target).map_err(|error| format!("{error}: {}", target.display()))?;
    // A single read() may come back short on a network filesystem, and
    // line_count is derived from what was read.
    // Whatever the batch cannot keep would be read and then thrown away, so
    // the read stops at the budget. The open still happens, which is what lets
    // a file that cannot be read report its own error instead of a budget cut.
    let cap = budget.min(MAX_READ_BYTES);
    let mut buf = Vec::with_capacity(cap + 1);
    let read_n = file
        .by_ref()
        .take((cap + 1) as u64)
        .read_to_end(&mut buf)
        .map_err(|e| e.to_string())?;
    let mut truncated = read_n > cap;
    let data = &buf[..read_n.min(cap)];
    let mut text = String::from_utf8_lossy(data).into_owned();
    // from_utf8_lossy expands one invalid byte into three, so the text can
    // exceed the cap even when the file did not. Cutting it here keeps that a
    // per-file limit rather than something the batch blames on its budget.
    if text.len() > MAX_READ_BYTES {
        let keep = floor_char_boundary(&text, MAX_READ_BYTES);
        text.truncate(keep);
        truncated = true;
    }
    // Over what was read, not the whole file: a full scan costs the entire
    // file in disk reads to return at most MAX_READ_BYTES.
    let line_count = count_lines(&text);

    Ok(ReadFileOutput {
        path: to_workspace_relative(&root, &target),
        size_bytes,
        line_count,
        text,
        truncated,
    })
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadBatchEntry {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub bytes: usize,
    pub size_bytes: u64,
    pub line_count: usize,
    pub text: String,
    pub truncated: bool,
    /// This entry was cut by the shared budget, so asking for fewer files
    /// returns more of it. A file over the per-file cap sets `truncated`
    /// without this: no retry returns the rest.
    pub budget_truncated: bool,
}

pub struct ReadBatchOutput {
    pub files: Vec<ReadBatchEntry>,
    pub total_bytes: usize,
    pub total_line_count: usize,
    /// The shared budget cut something short, so a smaller retry returns more.
    /// A file over the per-file cap does not set this: no retry helps.
    pub batch_truncated: bool,
}

fn floor_char_boundary(text: &str, max: usize) -> usize {
    if max >= text.len() {
        return text.len();
    }
    let mut index = max;
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// One path form for every entry, readable or not.
fn entry_path(root: &Path, planned: &PlannedRead, requested: &str) -> String {
    planned
        .target
        .as_ref()
        .map(|target| to_workspace_relative(root, target))
        .unwrap_or_else(|| requested.to_string())
}

fn failed_entry(path: &str, error: String) -> ReadBatchEntry {
    ReadBatchEntry {
        path: path.to_string(),
        error: Some(error),
        bytes: 0,
        size_bytes: 0,
        line_count: 0,
        text: String::new(),
        truncated: false,
        budget_truncated: false,
    }
}

struct PlannedRead {
    size_bytes: u64,
    error: Option<String>,
    target: Option<PathBuf>,
}

fn plan_read(workspace_root: &str, path: &str) -> PlannedRead {
    let resolved = resolve_target_path(workspace_root, path);
    let target = resolved.as_ref().ok().cloned();
    let size_bytes = resolved.and_then(|target| readable_size(&target));
    match size_bytes {
        Ok(size_bytes) => PlannedRead {
            size_bytes,
            error: None,
            target,
        },
        Err(error) => PlannedRead {
            size_bytes: 0,
            error: Some(error),
            target,
        },
    }
}

/// Smallest on disk first. Lossy expansion can still make a small file
/// spend more than its size, so this is a heuristic, not a guarantee.
fn read_order(planned: &[PlannedRead]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..planned.len())
        .filter(|index| planned[*index].error.is_none())
        .collect();
    order.sort_by_key(|index| planned[*index].size_bytes);
    order
}

/// Entries come back in the order requested, whatever order they were read in.
pub fn read_files(workspace_root: &str, paths: &[String]) -> Result<ReadBatchOutput, String> {
    if paths.is_empty() {
        return Err("paths must contain at least one path".into());
    }
    if paths.len() > MAX_READ_BATCH_FILES {
        return Err(format!(
            "paths must contain at most {MAX_READ_BATCH_FILES} entries (got {})",
            paths.len()
        ));
    }
    let root = workspace_root_path(workspace_root)?;

    let planned: Vec<PlannedRead> = paths
        .iter()
        .map(|path| plan_read(workspace_root, path))
        .collect();

    let mut entries: Vec<Option<ReadBatchEntry>> = (0..paths.len()).map(|_| None).collect();
    let mut remaining = MAX_READ_BATCH_BYTES;
    let mut batch_truncated = false;
    let mut total_bytes = 0_usize;
    let mut total_line_count = 0_usize;

    // A symlink and its target canonicalize to the same place, as does the
    // same string twice. Reading it twice would charge the budget twice.
    let mut already_read: HashSet<PathBuf> = HashSet::new();

    for index in read_order(&planned) {
        let path = &paths[index];
        if let Some(target) = &planned[index].target
            && already_read.contains(target)
        {
            continue;
        }
        match read_file(workspace_root, path, remaining) {
            Ok(output) => {
                let keep = floor_char_boundary(&output.text, remaining);
                let text_cut = keep < output.text.len();
                // Now that the read stops at the budget, how much came back no
                // longer says which limit stopped it. The budget is the binding
                // one whenever it is tighter than the per-file cap.
                let budget_cut = text_cut || (output.truncated && remaining < MAX_READ_BYTES);
                let truncated = output.truncated || text_cut;
                batch_truncated |= budget_cut;
                let mut text = output.text;
                text.truncate(keep);
                let line_count = if text_cut {
                    count_lines(&text)
                } else {
                    output.line_count
                };
                remaining -= keep;
                total_bytes += keep;
                total_line_count += line_count;
                if let Some(target) = &planned[index].target {
                    already_read.insert(target.clone());
                }
                entries[index] = Some(ReadBatchEntry {
                    path: output.path,
                    error: None,
                    bytes: keep,
                    size_bytes: output.size_bytes,
                    line_count,
                    text,
                    truncated,
                    budget_truncated: budget_cut,
                });
            }
            Err(error) => {
                entries[index] = Some(failed_entry(
                    &entry_path(&root, &planned[index], path),
                    error,
                ))
            }
        }
    }

    let files: Vec<ReadBatchEntry> = entries
        .into_iter()
        .enumerate()
        .filter_map(|(index, entry)| match (entry, &planned[index].error) {
            (Some(entry), _) => Some(entry),
            (None, Some(error)) => Some(failed_entry(
                &entry_path(&root, &planned[index], &paths[index]),
                error.clone(),
            )),
            // Deduplicated: an earlier path named the same file.
            (None, None) => None,
        })
        .collect();

    Ok(ReadBatchOutput {
        files,
        total_bytes,
        total_line_count,
        batch_truncated,
    })
}

fn count_lines(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let newlines = text.bytes().filter(|byte| *byte == b'\n').count();
    if text.ends_with('\n') {
        newlines
    } else {
        newlines + 1
    }
}

pub fn write_file(
    workspace_root: &str,
    path: &str,
    content: &str,
    create_dirs: bool,
) -> Result<String, String> {
    if content.len() > MAX_WRITE_BYTES {
        return Err(format!(
            "Content too large: {} bytes (max {})",
            content.len(),
            MAX_WRITE_BYTES
        ));
    }

    let root = workspace_root_path(workspace_root)?;
    let target = resolve_target_path(workspace_root, path)?;
    if let Some(parent) = target.parent() {
        if !parent.exists() {
            if create_dirs {
                fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            } else {
                return Err(format!(
                    "Parent directory does not exist: {} (set create_dirs=true)",
                    parent.display()
                ));
            }
        }
    }

    fs::write(&target, content).map_err(|e| e.to_string())?;
    Ok(format!(
        "wrote {} bytes to {}",
        content.len(),
        to_workspace_relative(&root, &target)
    ))
}

pub fn list_files_filtered(
    workspace_root: &str,
    path: Option<&str>,
    include_hidden: bool,
    limit: Option<usize>,
    filter: command::FileListingFilter,
) -> Result<ListFilesOutput, String> {
    let root = workspace_root_path(workspace_root)?;
    let start = command::resolve_workspace_path(workspace_root, path)?;
    if !start.exists() {
        return Err(format!("Path not found: {}", start.display()));
    }
    if !start.is_dir() {
        return Err(format!("Not a directory: {}", start.display()));
    }

    let max_items = safe_limit(limit, DEFAULT_LIST_LIMIT, HARD_LIST_LIMIT);
    let root_device = filesystem_device(&start);
    let mut queue = VecDeque::new();
    queue.push_back(start.clone());

    let mut entries: Vec<ListFilesEntry> = Vec::new();
    let mut directory_count: usize = 0;
    let mut file_count: usize = 0;
    let mut other_count: usize = 0;
    let mut truncated = false;

    while let Some(dir) = queue.pop_front() {
        let mut dir_entries = fs::read_dir(&dir)
            .map_err(|e| e.to_string())?
            .map(|entry_res| {
                let entry = entry_res.map_err(|e| e.to_string())?;
                let name = entry.file_name().to_string_lossy().to_string();
                Ok((name, entry))
            })
            .collect::<Result<Vec<_>, String>>()?;
        dir_entries.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, entry) in dir_entries {
            if !include_hidden && name.starts_with('.') {
                continue;
            }

            let path = entry.path();
            let ft = entry.file_type().map_err(|e| e.to_string())?;
            let rel = to_workspace_relative(&root, &path);
            let rel_from_start = path.strip_prefix(&start).unwrap_or(path.as_path());
            let depth = rel_from_start.components().count().saturating_sub(1);

            if ft.is_dir() {
                if matches!(
                    filter,
                    command::FileListingFilter::All | command::FileListingFilter::DirectoriesOnly
                ) {
                    directory_count += 1;
                    entries.push(ListFilesEntry {
                        path: rel,
                        name,
                        kind: "dir".into(),
                        depth,
                    });
                }
                if may_recurse_on_same_filesystem(root_device, &path) {
                    queue.push_back(path);
                }
            } else if ft.is_file() {
                if matches!(
                    filter,
                    command::FileListingFilter::All | command::FileListingFilter::FilesOnly
                ) {
                    file_count += 1;
                    entries.push(ListFilesEntry {
                        path: rel,
                        name,
                        kind: "file".into(),
                        depth,
                    });
                }
            } else {
                if matches!(filter, command::FileListingFilter::All) {
                    other_count += 1;
                    entries.push(ListFilesEntry {
                        path: rel,
                        name,
                        kind: "other".into(),
                        depth,
                    });
                }
            }

            if entries.len() >= max_items {
                truncated = true;
                break;
            }
        }

        if truncated {
            break;
        }
    }

    Ok(ListFilesOutput {
        path: to_workspace_relative(&root, &start),
        item_count: entries.len(),
        directory_count,
        file_count,
        other_count,
        truncated,
        limit: max_items,
        entries,
    })
}

pub fn search_text(
    workspace_root: &str,
    options: SearchTextOptions<'_>,
) -> Result<SearchTextOutput, String> {
    if options.pattern.trim().is_empty() {
        return Err("Pattern must not be empty".into());
    }

    let root = workspace_root_path(workspace_root)?;
    let start = command::resolve_workspace_path(workspace_root, options.path)?;
    if !start.exists() {
        return Err(format!("Path not found: {}", start.display()));
    }
    if !start.is_dir() && !start.is_file() {
        return Err(format!("Not a file or directory: {}", start.display()));
    }

    validate_search_context(options.context, "context")?;
    validate_search_context(options.before, "before")?;
    validate_search_context(options.after, "after")?;
    let max_matches =
        validate_search_limit(options.max_matches, "max_matches", DEFAULT_SEARCH_LIMIT)?;
    let max_matches_per_file =
        validate_optional_search_limit(options.max_matches_per_file, "max_matches_per_file")?;
    let (before, after) = match options.context {
        Some(value) => (value, value),
        None => (options.before.unwrap_or(0), options.after.unwrap_or(0)),
    };
    let resolved = ResolvedSearchTextOptions {
        pattern: options.pattern,
        glob: options.glob.filter(|value| !value.trim().is_empty()),
        fixed_strings: options.fixed_strings,
        case_insensitive: options.case_insensitive,
        before,
        after,
        max_matches,
        max_matches_per_file,
        include_hidden: options.include_hidden,
        no_ignore: options.no_ignore,
    };

    // No admission gate here by operator decision (2026-09-25, catdesk-cft):
    // overload on 2026-09-20 (retry storms stacking whole-workspace walks) is
    // answered by the per-request deadline in server.rs (search is a
    // Filesystem-class call, bounded by MCP_HTTP_REQUEST_MAX_DURATION and
    // enforced around tokio::spawn_blocking), not by a search count cap.

    if command_available("rg") {
        return search_text_rg(&root, &start, resolved).map_err(|e| match e {
            SearchBackendError::Unavailable => "ripgrep disappeared while running search".into(),
            SearchBackendError::Failed(message) => message,
        });
    }
    if command_available("grep") {
        match search_text_grep(&root, &start, resolved) {
            Ok(output) => return Ok(output),
            Err(SearchBackendError::Unavailable) => {}
            Err(SearchBackendError::Failed(message)) => {
                return search_text_rust(
                    &root,
                    &start,
                    resolved,
                    format!("rg not found; grep failed ({message}); used built-in search"),
                );
            }
        }
    }

    search_text_rust(
        &root,
        &start,
        resolved,
        "rg and grep not found; used built-in search".into(),
    )
}

fn cached_command_available(
    cache: &OnceLock<bool>,
    program: &str,
    probe: impl FnOnce(&str) -> bool,
) -> bool {
    *cache.get_or_init(|| probe(program))
}

fn command_available(program: &str) -> bool {
    match program {
        "rg" => cached_command_available(&RG_AVAILABLE, program, probe_command_available),
        "grep" => cached_command_available(&GREP_AVAILABLE, program, probe_command_available),
        _ => probe_command_available(program),
    }
}

fn probe_command_available(program: &str) -> bool {
    match ProcessCommand::new(program)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => false,
    }
}

/// Single source of the rg argument vector, split out so the always-on
/// excludes are unit-testable without spawning a process.
fn rg_argument_list(options: ResolvedSearchTextOptions<'_>, start: &Path) -> Vec<String> {
    let mut args: Vec<String> = [
        "--json",
        "--one-file-system",
        "--line-number",
        "--with-filename",
        "--no-heading",
        "--no-messages",
        "--color",
        "never",
    ]
    .iter()
    .map(|argument| (*argument).to_string())
    .collect();
    if options.fixed_strings {
        args.push("--fixed-strings".into());
    }
    if options.case_insensitive {
        args.push("--ignore-case".into());
    }
    if options.include_hidden {
        args.push("--hidden".into());
    }
    if options.no_ignore {
        args.push("--no-ignore".into());
    }
    if let Some(glob) = options.glob {
        args.push("--glob".into());
        args.push(glob.to_string());
    }
    // After the caller's glob on purpose: rg lets the LAST matching glob
    // decide, so a caller whitelist like `**/*` must not resurrect these.
    if !options.no_ignore {
        for exclude in DEFAULT_SEARCH_EXCLUDES {
            args.push("--glob".into());
            args.push((*exclude).to_string());
        }
    }
    if let Some(value) = options.max_matches_per_file {
        args.push("--max-count".into());
        args.push(value.to_string());
    }
    if options.before == options.after && options.before > 0 {
        args.push("--context".into());
        args.push(options.before.to_string());
    } else {
        if options.before > 0 {
            args.push("--before-context".into());
            args.push(options.before.to_string());
        }
        if options.after > 0 {
            args.push("--after-context".into());
            args.push(options.after.to_string());
        }
    }
    args.push("--regexp".into());
    args.push(options.pattern.to_string());
    args.push(start.to_string_lossy().into_owned());
    args
}

/// Kills the rg child when dropped unless the reader loop finished first —
/// covers early returns, panics and caller-cancellation paths.
struct RgChildGuard {
    child: Option<std::process::Child>,
    finished: Arc<AtomicBool>,
}

impl RgChildGuard {
    fn kill(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
        }
    }
}

impl Drop for RgChildGuard {
    fn drop(&mut self) {
        if !self.finished.load(Ordering::SeqCst) {
            self.kill();
            if let Some(mut child) = self.child.take() {
                let _ = child.wait();
            }
        }
    }
}

/// Returns the "deadline fired" flag. Sleeps until `deadline`, then SIGKILLs
/// `pid` unless `finished` was set first. Keeps working even when the reader
/// thread is blocked or its caller walked away — that is the orphaned-search
/// failure mode from 2026-09-20.
#[cfg(unix)]
fn spawn_deadline_watchdog(
    pid: u32,
    deadline: Instant,
    finished: Arc<AtomicBool>,
) -> Arc<AtomicBool> {
    let killed = Arc::new(AtomicBool::new(false));
    let killed_flag = Arc::clone(&killed);
    std::thread::spawn(move || {
        let now = Instant::now();
        if deadline > now {
            std::thread::sleep(deadline - now);
        }
        if !finished.load(Ordering::SeqCst) {
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
            killed_flag.store(true, Ordering::SeqCst);
        }
    });
    killed
}

#[cfg(not(unix))]
fn spawn_deadline_watchdog(
    _pid: u32,
    _deadline: Instant,
    _finished: Arc<AtomicBool>,
) -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

fn search_text_rg(
    root: &Path,
    start: &Path,
    options: ResolvedSearchTextOptions<'_>,
) -> Result<SearchTextOutput, SearchBackendError> {
    search_text_rg_with_deadline(root, start, options, Instant::now() + SEARCH_DEADLINE)
}

fn search_text_rg_with_deadline(
    root: &Path,
    start: &Path,
    options: ResolvedSearchTextOptions<'_>,
    deadline: Instant,
) -> Result<SearchTextOutput, SearchBackendError> {
    let mut command = ProcessCommand::new("rg");
    command.current_dir(&root);
    for argument in rg_argument_list(options, start) {
        command.arg(argument);
    }
    // A search is background noise: it must never out-prioritize interactive
    // traffic (the GLM semaphore stalls when searches pin every CPU).
    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            libc::nice(10);
            Ok(())
        });
    }
    command.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = command.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            SearchBackendError::Unavailable
        } else {
            SearchBackendError::Failed(e.to_string())
        }
    })?;
    let pid = child.id();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| SearchBackendError::Failed("Failed to capture ripgrep stdout".into()))?;
    let finished = Arc::new(AtomicBool::new(false));
    let killed_by_deadline = spawn_deadline_watchdog(pid, deadline, Arc::clone(&finished));
    let mut child_guard = RgChildGuard {
        child: Some(child),
        finished: Arc::clone(&finished),
    };
    let mut reader = BufReader::new(stdout);
    let include_context = options.before > 0 || options.after > 0;
    let mut results: Vec<SearchTextEntry> = Vec::new();
    let mut returned_matches = 0_usize;
    let mut truncated = false;
    let mut line = String::new();

    loop {
        line.clear();
        let bytes_read = reader
            .read_line(&mut line)
            .map_err(|e| SearchBackendError::Failed(e.to_string()))?;
        if bytes_read == 0 {
            break;
        }
        let event: Value = serde_json::from_str(line.trim_end()).map_err(|e| {
            SearchBackendError::Failed(format!("Failed to parse ripgrep JSON output: {e}"))
        })?;
        let event_type = event.get("type").and_then(Value::as_str);
        match event_type {
            Some("match") => {
                if returned_matches >= options.max_matches {
                    truncated = true;
                    child_guard.kill();
                    break;
                }
                results.push(
                    parse_rg_search_entry(root, &event, false)
                        .map_err(SearchBackendError::Failed)?,
                );
                returned_matches += 1;
            }
            Some("context") if include_context => {
                results.push(
                    parse_rg_search_entry(root, &event, true)
                        .map_err(SearchBackendError::Failed)?,
                );
            }
            _ => {}
        }
    }

    finished.store(true, Ordering::SeqCst);
    let killed_by_deadline = killed_by_deadline.load(Ordering::SeqCst);
    let output = child_guard
        .child
        .take()
        .expect("rg child consumed exactly once")
        .wait_with_output()
        .map_err(|e| SearchBackendError::Failed(e.to_string()))?;
    let status_code = output.status.code().unwrap_or(2);
    // A deadline kill surfaces as a signal exit (code None -> 2); that is a
    // first-class truncated outcome, not a backend failure.
    if killed_by_deadline {
        truncated = true;
    }
    if !truncated && status_code != 0 && status_code != 1 {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = stderr.trim();
        return Err(SearchBackendError::Failed(if message.is_empty() {
            format!("ripgrep exited with status {status_code}")
        } else {
            message.to_string()
        }));
    }

    Ok(SearchTextOutput {
        pattern: options.pattern.to_string(),
        path: to_workspace_relative(root, start),
        backend: "rg".into(),
        backend_note: if killed_by_deadline {
            "search exceeded its time budget and was truncated; narrow the path or refine the pattern".into()
        } else {
            String::new()
        },
        match_count: returned_matches,
        truncated,
        limit: options.max_matches,
        results,
    })
}

fn search_text_grep(
    root: &Path,
    start: &Path,
    options: ResolvedSearchTextOptions<'_>,
) -> Result<SearchTextOutput, SearchBackendError> {
    let collected = collect_search_files(root, start, options)
        .map_err(|e| SearchBackendError::Failed(e.to_string()))?;
    let files = collected.files;
    let mut results = Vec::new();
    let mut returned_matches = 0_usize;
    let mut truncated = false;

    for file in files.iter() {
        let remaining_matches = options.max_matches.saturating_sub(returned_matches);
        let probe_limit = remaining_matches.saturating_add(1);
        let file_match_limit = options
            .max_matches_per_file
            .map(|value| value.min(probe_limit))
            .unwrap_or(probe_limit);
        let mut command = ProcessCommand::new("grep");
        command
            .current_dir(root)
            .arg("-n")
            .arg("-I")
            .arg("-m")
            .arg(file_match_limit.to_string());
        if options.fixed_strings {
            command.arg("-F");
        } else {
            command.arg("-E");
        }
        if options.case_insensitive {
            command.arg("-i");
        }
        if options.before == options.after && options.before > 0 {
            command.arg("-C").arg(options.before.to_string());
        } else {
            if options.before > 0 {
                command.arg("-B").arg(options.before.to_string());
            }
            if options.after > 0 {
                command.arg("-A").arg(options.after.to_string());
            }
        }
        command.arg("--").arg(options.pattern).arg(file);
        command.stdout(Stdio::piped()).stderr(Stdio::piped());

        let output = command.output().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                SearchBackendError::Unavailable
            } else {
                SearchBackendError::Failed(e.to_string())
            }
        })?;
        let status_code = output.status.code().unwrap_or(2);
        if status_code != 0 && status_code != 1 {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let message = stderr.trim();
            return Err(SearchBackendError::Failed(if message.is_empty() {
                format!("grep exited with status {status_code}")
            } else {
                message.to_string()
            }));
        }

        let mut file_matches = 0_usize;
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            if line == "--" {
                continue;
            }
            let Some(entry) = parse_grep_search_entry(root, file, line) else {
                continue;
            };
            if !entry.is_context {
                if returned_matches >= options.max_matches {
                    truncated = true;
                    break;
                }
                if let Some(max_per_file) = options.max_matches_per_file {
                    if file_matches >= max_per_file {
                        continue;
                    }
                }
                file_matches += 1;
                returned_matches += 1;
            }
            results.push(entry);
        }
        if truncated {
            break;
        }
    }

    Ok(SearchTextOutput {
        pattern: options.pattern.to_string(),
        path: to_workspace_relative(root, start),
        backend: "grep".into(),
        backend_note: fallback_search_note("rg not found; used grep", collected.limited),
        match_count: returned_matches,
        truncated,
        limit: options.max_matches,
        results,
    })
}

fn search_text_rust(
    root: &Path,
    start: &Path,
    options: ResolvedSearchTextOptions<'_>,
    backend_note: String,
) -> Result<SearchTextOutput, String> {
    let matcher = SearchMatcher::new(options)?;
    let collected = collect_search_files(root, start, options)?;
    let files = collected.files;
    let backend_note = fallback_search_note(&backend_note, collected.limited);
    let mut results = Vec::new();
    let mut returned_matches = 0_usize;
    let mut truncated = false;

    for file in files.iter() {
        if returned_matches >= options.max_matches {
            truncated = true;
            break;
        }
        let mut file_handle = fs::File::open(file).map_err(|e| e.to_string())?;
        let mut data = Vec::with_capacity(MAX_FALLBACK_SEARCH_FILE_BYTES as usize + 1);
        file_handle
            .by_ref()
            .take(MAX_FALLBACK_SEARCH_FILE_BYTES + 1)
            .read_to_end(&mut data)
            .map_err(|e| e.to_string())?;
        if data.len() as u64 > MAX_FALLBACK_SEARCH_FILE_BYTES {
            data.truncate(MAX_FALLBACK_SEARCH_FILE_BYTES as usize);
        }
        if data.iter().any(|b| *b == 0) {
            continue;
        }
        let text = String::from_utf8_lossy(&data);
        let lines: Vec<&str> = text.lines().collect();
        let mut match_indexes = Vec::new();
        for (idx, line) in lines.iter().enumerate() {
            if !matcher.is_match(line) {
                continue;
            }
            if let Some(max_per_file) = options.max_matches_per_file {
                if match_indexes.len() >= max_per_file {
                    break;
                }
            }
            if returned_matches + match_indexes.len() >= options.max_matches {
                truncated = true;
                break;
            }
            match_indexes.push(idx);
        }
        append_rust_search_entries(
            root,
            file,
            &lines,
            &match_indexes,
            options.before,
            options.after,
            &mut results,
        );
        returned_matches += match_indexes.len();
    }

    Ok(SearchTextOutput {
        pattern: options.pattern.to_string(),
        path: to_workspace_relative(root, start),
        backend: "rust".into(),
        backend_note,
        match_count: returned_matches,
        truncated,
        limit: options.max_matches,
        results,
    })
}

fn validate_search_limit(
    value: Option<usize>,
    name: &str,
    default_value: usize,
) -> Result<usize, String> {
    match value {
        Some(value) if value == 0 || value > HARD_SEARCH_LIMIT => {
            Err(format!("{name} must be between 1 and {HARD_SEARCH_LIMIT}"))
        }
        Some(value) => Ok(value),
        None => Ok(default_value),
    }
}

fn validate_optional_search_limit(
    value: Option<usize>,
    name: &str,
) -> Result<Option<usize>, String> {
    match value {
        Some(value) if value == 0 || value > HARD_SEARCH_LIMIT => {
            Err(format!("{name} must be between 1 and {HARD_SEARCH_LIMIT}"))
        }
        Some(value) => Ok(Some(value)),
        None => Ok(None),
    }
}

fn validate_search_context(value: Option<usize>, name: &str) -> Result<(), String> {
    if let Some(value) = value {
        if value > HARD_SEARCH_CONTEXT_LINES {
            return Err(format!(
                "{name} must be between 0 and {HARD_SEARCH_CONTEXT_LINES}"
            ));
        }
    }
    Ok(())
}

fn parse_rg_search_entry(
    root: &Path,
    event: &Value,
    is_context: bool,
) -> Result<SearchTextEntry, String> {
    let data = event
        .get("data")
        .and_then(Value::as_object)
        .ok_or_else(|| "ripgrep JSON event is missing data".to_string())?;
    let raw_path = data
        .get("path")
        .and_then(|path| path.get("text"))
        .and_then(Value::as_str)
        .ok_or_else(|| "ripgrep JSON event is missing path text".to_string())?;
    let line = data
        .get("line_number")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| "ripgrep JSON event is missing line number".to_string())?;
    let text = data
        .get("lines")
        .and_then(|lines| lines.get("text"))
        .and_then(Value::as_str)
        .ok_or_else(|| "ripgrep JSON event is missing line text".to_string())?
        .trim_end_matches(['\r', '\n'])
        .to_string();
    let path = PathBuf::from(raw_path);
    let absolute_path = if path.is_absolute() {
        path
    } else {
        root.join(path)
    };

    Ok(SearchTextEntry {
        path: to_workspace_relative(root, &absolute_path),
        line,
        text,
        is_context,
    })
}

fn parse_grep_search_entry(root: &Path, file: &Path, line: &str) -> Option<SearchTextEntry> {
    let (line_number, separator, text) = parse_grep_line_prefix(line)?;
    Some(SearchTextEntry {
        path: to_workspace_relative(root, file),
        line: line_number,
        text: text.to_string(),
        is_context: separator == '-',
    })
}

fn parse_grep_line_prefix(line: &str) -> Option<(usize, char, &str)> {
    let mut digits_end = 0_usize;
    for (idx, ch) in line.char_indices() {
        if ch.is_ascii_digit() {
            digits_end = idx + ch.len_utf8();
            continue;
        }
        if digits_end == 0 || (ch != ':' && ch != '-') {
            return None;
        }
        let line_number = line[..digits_end].parse::<usize>().ok()?;
        let text_start = idx + ch.len_utf8();
        return Some((line_number, ch, &line[text_start..]));
    }
    None
}

struct CollectedSearchFiles {
    files: Vec<PathBuf>,
    limited: bool,
}

fn fallback_search_note(base: &str, limited: bool) -> String {
    if !limited {
        return base.to_string();
    }
    format!(
        "{base}; fallback scan bounded to 2 MiB per file, 64 MiB total, and 2000 files"
    )
}

fn collect_search_files(
    root: &Path,
    start: &Path,
    options: ResolvedSearchTextOptions<'_>,
) -> Result<CollectedSearchFiles, String> {
    let glob = compile_search_glob(options.glob)?;
    let mut builder = WalkBuilder::new(start);
    builder
        .hidden(!options.include_hidden)
        .same_file_system(true)
        .parents(!options.no_ignore)
        .ignore(!options.no_ignore)
        .git_global(!options.no_ignore)
        .git_ignore(!options.no_ignore)
        .git_exclude(!options.no_ignore);

    let mut files = Vec::new();
    let mut total_bytes = 0_u64;
    let mut limited = false;
    for entry_result in builder.build() {
        let entry = entry_result.map_err(|e| e.to_string())?;
        let path = entry.path();
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() {
            continue;
        }
        if let Some(glob) = glob.as_ref() {
            let rel = path.strip_prefix(root).unwrap_or(path);
            if !glob.is_match(rel) {
                continue;
            }
        }
        let file_bytes = match entry.metadata() {
            Ok(metadata) => metadata.len(),
            Err(_) => {
                limited = true;
                continue;
            }
        };
        if file_bytes > MAX_FALLBACK_SEARCH_FILE_BYTES {
            limited = true;
            continue;
        }
        if files.len() >= MAX_FALLBACK_SEARCH_FILES
            || total_bytes.saturating_add(file_bytes) > MAX_FALLBACK_SEARCH_TOTAL_BYTES
        {
            limited = true;
            break;
        }
        total_bytes = total_bytes.saturating_add(file_bytes);
        files.push(path.to_path_buf());
    }
    files.sort();
    Ok(CollectedSearchFiles { files, limited })
}

fn compile_search_glob(glob: Option<&str>) -> Result<Option<GlobSet>, String> {
    let Some(glob) = glob else {
        return Ok(None);
    };
    let mut builder = GlobSetBuilder::new();
    builder.add(Glob::new(glob).map_err(|e| e.to_string())?);
    if !glob.contains('/') && !glob.contains('\\') {
        builder.add(Glob::new(&format!("**/{glob}")).map_err(|e| e.to_string())?);
    }
    builder.build().map(Some).map_err(|e| e.to_string())
}

fn append_rust_search_entries(
    root: &Path,
    file: &Path,
    lines: &[&str],
    match_indexes: &[usize],
    before: usize,
    after: usize,
    results: &mut Vec<SearchTextEntry>,
) {
    let mut selected: BTreeMap<usize, bool> = BTreeMap::new();
    for match_idx in match_indexes {
        let start_idx = match_idx.saturating_sub(before);
        let end_idx = (*match_idx + after).min(lines.len().saturating_sub(1));
        for idx in start_idx..=end_idx {
            selected.entry(idx).or_insert(true);
        }
        selected.insert(*match_idx, false);
    }

    let rel = to_workspace_relative(root, file);
    for (idx, is_context) in selected {
        results.push(SearchTextEntry {
            path: rel.clone(),
            line: idx + 1,
            text: lines[idx].to_string(),
            is_context,
        });
    }
}

pub fn delete_path(workspace_root: &str, path: &str, recursive: bool) -> Result<String, String> {
    let root = workspace_root_path(workspace_root)?;
    let target = resolve_target_path(workspace_root, path)?;

    if !target.exists() {
        return Err(format!("Path not found: {}", target.display()));
    }

    let meta = fs::symlink_metadata(&target).map_err(|e| e.to_string())?;
    let ft = meta.file_type();
    if ft.is_dir() {
        if recursive {
            fs::remove_dir_all(&target).map_err(|e| e.to_string())?;
            Ok(format!(
                "deleted directory recursively: {}",
                to_workspace_relative(&root, &target)
            ))
        } else {
            fs::remove_dir(&target).map_err(|e| e.to_string())?;
            Ok(format!(
                "deleted empty directory: {}",
                to_workspace_relative(&root, &target)
            ))
        }
    } else {
        fs::remove_file(&target).map_err(|e| e.to_string())?;
        Ok(format!(
            "deleted file: {}",
            to_workspace_relative(&root, &target)
        ))
    }
}

fn copy_file(src: &Path, dst: &Path) -> Result<(), String> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::copy(src, dst).map_err(|e| e.to_string())?;
    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), String> {
    fs::create_dir_all(dst).map_err(|e| e.to_string())?;
    for entry_res in fs::read_dir(src).map_err(|e| e.to_string())? {
        let entry = entry_res.map_err(|e| e.to_string())?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        let ft = entry.file_type().map_err(|e| e.to_string())?;
        if ft.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            copy_file(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

pub fn move_path(
    workspace_root: &str,
    from: &str,
    to: &str,
    overwrite: bool,
    create_dirs: bool,
) -> Result<String, String> {
    let root = workspace_root_path(workspace_root)?;
    let src = resolve_target_path(workspace_root, from)?;
    let dst = resolve_target_path(workspace_root, to)?;

    if !src.exists() {
        return Err(format!("Source path not found: {}", src.display()));
    }

    if src == dst {
        return Ok("source and destination are the same path".into());
    }

    if let Some(parent) = dst.parent() {
        if !parent.exists() {
            if create_dirs {
                fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            } else {
                return Err(format!(
                    "Destination parent does not exist: {} (set create_dirs=true)",
                    parent.display()
                ));
            }
        }
    }

    if dst.exists() {
        if !overwrite {
            return Err(format!(
                "Destination already exists: {} (set overwrite=true)",
                dst.display()
            ));
        }
        let meta = fs::symlink_metadata(&dst).map_err(|e| e.to_string())?;
        if meta.file_type().is_dir() {
            fs::remove_dir_all(&dst).map_err(|e| e.to_string())?;
        } else {
            fs::remove_file(&dst).map_err(|e| e.to_string())?;
        }
    }

    match fs::rename(&src, &dst) {
        Ok(_) => {}
        Err(e) => {
            // Cross-device rename fallback.
            if e.raw_os_error() == Some(18) {
                let meta = fs::symlink_metadata(&src).map_err(|err| err.to_string())?;
                if meta.file_type().is_dir() {
                    copy_dir_recursive(&src, &dst)?;
                    fs::remove_dir_all(&src).map_err(|err| err.to_string())?;
                } else {
                    copy_file(&src, &dst)?;
                    fs::remove_file(&src).map_err(|err| err.to_string())?;
                }
            } else {
                return Err(e.to_string());
            }
        }
    }

    Ok(format!(
        "moved {} -> {}",
        to_workspace_relative(&root, &src),
        to_workspace_relative(&root, &dst)
    ))
}

#[derive(Clone, Debug)]
pub enum EditOperation {
    Replace {
        old_string: String,
        new_string: String,
        replace_all: bool,
    },
    Range {
        start_line: usize,
        end_line: usize,
        old_text: String,
        new_text: String,
    },
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EditFileOutput {
    pub path: String,
    pub operation_count: usize,
    pub applied_operations: usize,
    pub replaced_occurrences: usize,
    pub bytes_written: usize,
}

impl EditFileOutput {
    pub fn render_text(&self) -> String {
        format!(
            "applied {} edit operation(s) to {} ({} replacement occurrence(s), {} bytes)",
            self.applied_operations, self.path, self.replaced_occurrences, self.bytes_written
        )
    }
}

fn line_range_byte_bounds(
    content: &str,
    start_line: usize,
    end_line: usize,
) -> Result<(usize, usize), String> {
    if start_line == 0 || end_line == 0 {
        return Err("range line numbers are 1-based and must be at least 1".into());
    }
    if start_line > end_line {
        return Err(format!(
            "range start_line ({start_line}) must not exceed end_line ({end_line})"
        ));
    }

    let mut starts = if content.is_empty() {
        Vec::new()
    } else {
        vec![0_usize]
    };
    for (index, byte) in content.bytes().enumerate() {
        if byte == b'\n' {
            starts.push(index + 1);
        }
    }
    if content.ends_with('\n') {
        starts.pop();
    }

    let line_count = starts.len();
    if start_line > line_count || end_line > line_count {
        return Err(format!(
            "range {start_line}-{end_line} exceeds file line count {line_count}"
        ));
    }

    let start = starts[start_line - 1];
    let end = if end_line < line_count {
        starts[end_line]
    } else {
        content.len()
    };
    Ok((start, end))
}

pub fn edit_file(
    workspace_root: &str,
    path: &str,
    operations: &[EditOperation],
) -> Result<EditFileOutput, String> {
    if operations.is_empty() {
        return Err("edits must contain at least one operation".into());
    }

    let root = workspace_root_path(workspace_root)?;
    let target = resolve_target_path(workspace_root, path)?;
    if !target.exists() {
        return Err(format!("File not found: {}", target.display()));
    }
    if !target.is_file() {
        return Err(format!("Not a file: {}", target.display()));
    }

    let original_content = fs::read_to_string(&target).map_err(|e| e.to_string())?;
    let mut content = original_content.clone();
    let mut replaced_occurrences = 0_usize;

    for (index, operation) in operations.iter().enumerate() {
        let operation_number = index + 1;
        match operation {
            EditOperation::Replace {
                old_string,
                new_string,
                replace_all,
            } => {
                if old_string.is_empty() {
                    return Err(format!(
                        "edit operation {operation_number}: old_string must not be empty"
                    ));
                }
                if old_string == new_string {
                    return Err(format!(
                        "edit operation {operation_number}: old_string and new_string must be different"
                    ));
                }

                let matched = content.matches(old_string).count();
                if matched == 0 {
                    return Err(format!(
                        "edit operation {operation_number}: old_string not found in {}",
                        to_workspace_relative(&root, &target)
                    ));
                }
                if matched > 1 && !replace_all {
                    return Err(format!(
                        "edit operation {operation_number}: old_string matched {} occurrences in {}. Set replace_all=true to replace every occurrence, or provide more context to make old_string unique.",
                        matched,
                        to_workspace_relative(&root, &target)
                    ));
                }

                content = if *replace_all {
                    content.replace(old_string, new_string)
                } else {
                    content.replacen(old_string, new_string, 1)
                };
                replaced_occurrences = replaced_occurrences.saturating_add(matched);
            }
            EditOperation::Range {
                start_line,
                end_line,
                old_text,
                new_text,
            } => {
                if old_text == new_text {
                    return Err(format!(
                        "edit operation {operation_number}: old_text and new_text must be different"
                    ));
                }
                let (start, end) = line_range_byte_bounds(&content, *start_line, *end_line)
                    .map_err(|error| format!("edit operation {operation_number}: {error}"))?;
                let selected = &content[start..end];
                if selected != old_text {
                    return Err(format!(
                        "edit operation {operation_number}: old_text does not match lines {}-{} in {}",
                        start_line,
                        end_line,
                        to_workspace_relative(&root, &target)
                    ));
                }

                content.replace_range(start..end, new_text);
                replaced_occurrences = replaced_occurrences.saturating_add(1);
            }
        }
    }

    let current_content = fs::read_to_string(&target).map_err(|e| e.to_string())?;
    if current_content != original_content {
        return Err(format!(
            "file changed during edit; refusing to overwrite {}",
            to_workspace_relative(&root, &target)
        ));
    }

    fs::write(&target, &content).map_err(|e| e.to_string())?;
    Ok(EditFileOutput {
        path: to_workspace_relative(&root, &target),
        operation_count: operations.len(),
        applied_operations: operations.len(),
        replaced_occurrences,
        bytes_written: content.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn named_backend_probe_is_cached_once() {
        let cache = std::sync::OnceLock::new();
        let calls = std::cell::Cell::new(0usize);

        assert!(cached_command_available(&cache, "rg", |_| {
            calls.set(calls.get() + 1);
            true
        }));
        assert!(cached_command_available(&cache, "rg", |_| {
            calls.set(calls.get() + 1);
            false
        }));
        assert_eq!(calls.get(), 1, "backend availability should be probed once");
    }

    fn test_workspace(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("catdesk-workspace-tools-{name}-{}", Uuid::new_v4()))
    }

    #[test]
    fn rust_search_backend_supports_regex_glob_and_context() {
        let workspace_root = test_workspace("search-rust");
        fs::create_dir_all(workspace_root.join("src")).expect("create workspace");
        fs::write(workspace_root.join("notes.txt"), "alpha1\n").expect("write notes");
        fs::write(
            workspace_root.join("src").join("main.rs"),
            "before\nalpha1\nafter\nalpha2\n",
        )
        .expect("write source");

        let output = search_text_rust(
            &workspace_root,
            &workspace_root,
            ResolvedSearchTextOptions {
                pattern: "alpha[0-9]",
                glob: Some("*.rs"),
                fixed_strings: false,
                case_insensitive: false,
                before: 1,
                after: 1,
                max_matches: 1,
                max_matches_per_file: None,
                include_hidden: false,
                no_ignore: false,
            },
            "test rust backend".into(),
        )
        .expect("search");

        assert_eq!(output.backend, "rust");
        assert_eq!(output.match_count, 1);
        assert!(output.truncated);
        assert_eq!(
            output
                .results
                .iter()
                .filter(|entry| !entry.is_context)
                .map(|entry| (entry.path.as_str(), entry.line, entry.text.as_str()))
                .collect::<Vec<_>>(),
            vec![("src/main.rs", 2, "alpha1")]
        );
        assert!(output.results.iter().any(|entry| entry.is_context));

        let _ = fs::remove_dir_all(workspace_root);
    }

    // The 2026-09-20 incident (an agent retry storm stacking whole-workspace
    // searches) is no longer answered by a count cap: search has no admission
    // gate, and overload is bounded by the per-request deadline in server.rs
    // (Filesystem class, enforced around tokio::spawn_blocking). So parallel
    // — including duplicate — searches must be admitted, never rejected.
    #[test]
    fn concurrent_and_duplicate_searches_are_admitted_without_a_cap() {
        use std::sync::{Arc, Barrier};

        let workspace_root = test_workspace("search-concurrent-admission");
        fs::create_dir_all(&workspace_root).expect("create workspace");
        let needles = ["needle-0", "needle-1", "needle-2", "needle-3"];
        for (index, needle) in needles.iter().enumerate() {
            fs::write(
                workspace_root.join(format!("file-{index}.txt")),
                format!("before {needle}\n{needle}\nafter\n"),
            )
            .expect("write search file");
        }
        fs::write(
            workspace_root.join("shared.txt"),
            "needle-shared\nneedle-shared\n",
        )
        .expect("write shared file");
        let workspace_root_str = workspace_root.to_string_lossy().into_owned();

        let options = |pattern: &'static str| SearchTextOptions {
            pattern,
            path: None,
            glob: None,
            fixed_strings: true,
            case_insensitive: false,
            context: None,
            before: None,
            after: None,
            max_matches: Some(100),
            max_matches_per_file: None,
            include_hidden: false,
            no_ignore: false,
        };

        // Distinct queries, more than the retired cap of 2, released through a
        // barrier so every call is in flight at the same time.
        let barrier = Arc::new(Barrier::new(needles.len()));
        let mut handles = Vec::new();
        for needle in needles {
            let barrier = Arc::clone(&barrier);
            let workspace = workspace_root_str.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                search_text(&workspace, options(needle)).unwrap_or_else(|error| {
                    panic!("concurrent search '{needle}' was not admitted: {error}")
                })
            }));
        }
        for (index, handle) in handles.into_iter().enumerate() {
            let output = handle.join().expect("search thread did not panic");
            assert!(
                output
                    .results
                    .iter()
                    .any(|entry| entry.text.contains(needles[index])),
                "search for {} lost its match (backend {})",
                needles[index],
                output.backend
            );
        }

        // Duplicate queries in flight at once (the retry-storm shape): every
        // copy must run to completion instead of being refused.
        let barrier = Arc::new(Barrier::new(4));
        let mut handles = Vec::new();
        for _ in 0..4 {
            let barrier = Arc::clone(&barrier);
            let workspace = workspace_root_str.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                search_text(&workspace, options("needle-shared"))
                    .unwrap_or_else(|error| panic!("duplicate search was not admitted: {error}"))
            }));
        }
        let match_counts = handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .expect("duplicate search thread did not panic")
            })
            .map(|output| output.match_count)
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(
            match_counts.into_iter().collect::<Vec<_>>(),
            vec![2],
            "duplicate searches must all complete with identical results"
        );

        let _ = fs::remove_dir_all(workspace_root);
    }

    #[test]
    fn builtin_search_caps_bytes_read_per_file() {
        use std::io::Write;

        let workspace_root = test_workspace("search-rust-file-cap");
        fs::create_dir_all(&workspace_root).expect("create workspace");
        let mut file = fs::File::create(workspace_root.join("large.txt")).expect("create large file");
        file.write_all(&vec![b'a'; MAX_FALLBACK_SEARCH_FILE_BYTES as usize])
            .expect("write prefix");
        file.write_all(b"\nneedle-after-cap\n")
            .expect("write suffix");
        drop(file);

        let output = search_text_rust(
            &workspace_root,
            &workspace_root,
            ResolvedSearchTextOptions {
                pattern: "needle-after-cap",
                glob: None,
                fixed_strings: true,
                case_insensitive: false,
                before: 0,
                after: 0,
                max_matches: 100,
                max_matches_per_file: None,
                include_hidden: false,
                no_ignore: false,
            },
            "test rust backend".into(),
        )
        .expect("search");

        assert_eq!(output.match_count, 0, "fallback read past its per-file byte cap");
        assert!(
            output.backend_note.contains("2 MiB per file"),
            "missing bounded-scan note: {}",
            output.backend_note
        );
        let _ = fs::remove_dir_all(workspace_root);
    }

    #[test]
    fn grep_search_backend_marks_truncated_when_an_extra_match_exists() {
        if !command_available("grep") {
            return;
        }

        let workspace_root = test_workspace("search-grep-truncated");
        fs::create_dir_all(workspace_root.join("src")).expect("create workspace");
        fs::write(
            workspace_root.join("src").join("main.rs"),
            "alpha1\nbeta\nalpha2\n",
        )
        .expect("write source");

        let output = search_text_grep(
            &workspace_root,
            &workspace_root,
            ResolvedSearchTextOptions {
                pattern: "alpha[0-9]",
                glob: Some("*.rs"),
                fixed_strings: false,
                case_insensitive: false,
                before: 0,
                after: 0,
                max_matches: 1,
                max_matches_per_file: None,
                include_hidden: false,
                no_ignore: false,
            },
        )
        .unwrap_or_else(|_| panic!("search"));

        assert_eq!(output.backend, "grep");
        assert_eq!(output.match_count, 1);
        assert!(output.truncated);
        assert_eq!(
            output
                .results
                .iter()
                .filter(|entry| !entry.is_context)
                .map(|entry| (entry.path.as_str(), entry.line, entry.text.as_str()))
                .collect::<Vec<_>>(),
            vec![("src/main.rs", 1, "alpha1")]
        );

        let _ = fs::remove_dir_all(workspace_root);
    }

    #[test]
    fn grep_search_backend_does_not_mark_exact_limit_as_truncated() {
        if !command_available("grep") {
            return;
        }

        let workspace_root = test_workspace("search-grep-exact-limit");
        fs::create_dir_all(workspace_root.join("src")).expect("create workspace");
        fs::write(workspace_root.join("src").join("a.rs"), "alpha1\n").expect("write match");
        fs::write(workspace_root.join("src").join("b.rs"), "beta\n").expect("write non-match");

        let output = search_text_grep(
            &workspace_root,
            &workspace_root,
            ResolvedSearchTextOptions {
                pattern: "alpha[0-9]",
                glob: Some("*.rs"),
                fixed_strings: false,
                case_insensitive: false,
                before: 0,
                after: 0,
                max_matches: 1,
                max_matches_per_file: None,
                include_hidden: false,
                no_ignore: false,
            },
        )
        .unwrap_or_else(|_| panic!("search"));

        assert_eq!(output.backend, "grep");
        assert_eq!(output.match_count, 1);
        assert!(!output.truncated);
        assert_eq!(
            output
                .results
                .iter()
                .filter(|entry| !entry.is_context)
                .map(|entry| (entry.path.as_str(), entry.line, entry.text.as_str()))
                .collect::<Vec<_>>(),
            vec![("src/a.rs", 1, "alpha1")]
        );

        let _ = fs::remove_dir_all(workspace_root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn recursive_listing_and_builtin_search_do_not_cross_mount_boundary() {
        use std::process::Command;

        if std::env::var_os("CATDESK_WORKSPACE_MOUNT_TEST_CHILD").is_none() {
            let available = Command::new("unshare")
                .args(["--user", "--map-root-user", "--mount", "true"])
                .output()
                .is_ok_and(|result| result.status.success());
            if !available {
                eprintln!("mount-boundary fixture unavailable: user/mount namespaces are disabled");
                return;
            }
            let result = Command::new("unshare")
                .args(["--user", "--map-root-user", "--mount", "--fork"])
                .arg(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "workspace_tools::tests::recursive_listing_and_builtin_search_do_not_cross_mount_boundary",
                    "--nocapture",
                ])
                .env("CATDESK_WORKSPACE_MOUNT_TEST_CHILD", "1")
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{} {}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }

        let root = test_workspace("mount-boundary");
        let archive = root.join("archive");
        fs::create_dir_all(&archive).unwrap();
        assert!(
            Command::new("mount")
                .args(["-t", "tmpfs", "tmpfs"])
                .arg(&archive)
                .status()
                .unwrap()
                .success()
        );
        fs::write(root.join("local.txt"), "needle-local\n").unwrap();
        fs::write(archive.join("sentinel.txt"), "needle-mounted\n").unwrap();
        let root_string = root.to_string_lossy().into_owned();

        let listing = list_files_filtered(
            &root_string,
            None,
            false,
            Some(100),
            command::FileListingFilter::All,
        )
        .expect("list workspace");
        assert!(listing.entries.iter().any(|entry| entry.path == "local.txt"));
        assert!(
            listing
                .entries
                .iter()
                .all(|entry| entry.path != "archive/sentinel.txt"),
            "recursive listing crossed into mounted filesystem"
        );

        let search = search_text_rust(
            &root,
            &root,
            ResolvedSearchTextOptions {
                pattern: "needle",
                glob: None,
                fixed_strings: true,
                case_insensitive: false,
                before: 0,
                after: 0,
                max_matches: 100,
                max_matches_per_file: None,
                include_hidden: false,
                no_ignore: false,
            },
            "test rust backend".into(),
        )
        .expect("search workspace");
        assert!(search.results.iter().any(|entry| entry.path == "local.txt"));
        assert!(
            search
                .results
                .iter()
                .all(|entry| entry.path != "archive/sentinel.txt"),
            "built-in search crossed into mounted filesystem"
        );

        let public_search = search_text(
            &root_string,
            SearchTextOptions {
                pattern: "needle",
                path: None,
                glob: None,
                fixed_strings: true,
                case_insensitive: false,
                context: None,
                before: None,
                after: None,
                max_matches: Some(100),
                max_matches_per_file: None,
                include_hidden: false,
                no_ignore: false,
            },
        )
        .expect("public search workspace");
        assert!(public_search.results.iter().any(|entry| entry.path == "local.txt"));
        assert!(
            public_search
                .results
                .iter()
                .all(|entry| entry.path != "archive/sentinel.txt"),
            "public search crossed into mounted filesystem via {}",
            public_search.backend
        );

        assert!(Command::new("umount").arg(&archive).status().unwrap().success());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn edit_file_applies_replace_and_range_operations_in_one_atomic_batch() {
        let workspace_root = test_workspace("edit-batch");
        fs::create_dir_all(&workspace_root).expect("create workspace");
        fs::write(workspace_root.join("notes.txt"), "alpha\nbeta\ngamma\n").expect("write file");
        let workspace_root_str = workspace_root.to_string_lossy().into_owned();

        let output = edit_file(
            &workspace_root_str,
            "notes.txt",
            &[
                EditOperation::Replace {
                    old_string: "alpha".into(),
                    new_string: "ALPHA".into(),
                    replace_all: false,
                },
                EditOperation::Range {
                    start_line: 2,
                    end_line: 3,
                    old_text: "beta\ngamma\n".into(),
                    new_text: "BETA\nGAMMA\n".into(),
                },
            ],
        )
        .expect("edit batch");

        assert_eq!(
            fs::read_to_string(workspace_root.join("notes.txt")).expect("read file"),
            "ALPHA\nBETA\nGAMMA\n"
        );
        assert_eq!(output.path, "notes.txt");
        assert_eq!(output.operation_count, 2);
        assert_eq!(output.applied_operations, 2);
        assert_eq!(output.replaced_occurrences, 2);
        assert_eq!(output.bytes_written, "ALPHA\nBETA\nGAMMA\n".len());

        let _ = fs::remove_dir_all(workspace_root);
    }

    #[test]
    fn edit_file_does_not_write_partial_results_when_later_operation_fails() {
        let workspace_root = test_workspace("edit-atomic-failure");
        fs::create_dir_all(&workspace_root).expect("create workspace");
        fs::write(workspace_root.join("notes.txt"), "alpha\nbeta\n").expect("write file");
        let workspace_root_str = workspace_root.to_string_lossy().into_owned();

        let error = edit_file(
            &workspace_root_str,
            "notes.txt",
            &[
                EditOperation::Replace {
                    old_string: "alpha".into(),
                    new_string: "ALPHA".into(),
                    replace_all: false,
                },
                EditOperation::Replace {
                    old_string: "missing".into(),
                    new_string: "present".into(),
                    replace_all: false,
                },
            ],
        )
        .expect_err("second operation should fail");

        assert!(error.contains("edit operation 2"));
        assert_eq!(
            fs::read_to_string(workspace_root.join("notes.txt")).expect("read file"),
            "alpha\nbeta\n"
        );

        let _ = fs::remove_dir_all(workspace_root);
    }

    #[test]
    fn edit_file_range_requires_exact_guard_text() {
        let workspace_root = test_workspace("edit-range-guard");
        fs::create_dir_all(&workspace_root).expect("create workspace");
        fs::write(workspace_root.join("notes.txt"), "alpha\nbeta\ngamma\n").expect("write file");
        let workspace_root_str = workspace_root.to_string_lossy().into_owned();

        let error = edit_file(
            &workspace_root_str,
            "notes.txt",
            &[EditOperation::Range {
                start_line: 2,
                end_line: 2,
                old_text: "wrong\n".into(),
                new_text: "BETA\n".into(),
            }],
        )
        .expect_err("guard should reject stale range");

        assert!(error.contains("old_text does not match lines 2-2"));
        assert_eq!(
            fs::read_to_string(workspace_root.join("notes.txt")).expect("read file"),
            "alpha\nbeta\ngamma\n"
        );

        let _ = fs::remove_dir_all(workspace_root);
    }

    fn rg_args_for(pattern: &str, glob: Option<&str>, no_ignore: bool) -> Vec<String> {
        rg_argument_list(
            ResolvedSearchTextOptions {
                pattern,
                glob,
                fixed_strings: false,
                case_insensitive: false,
                before: 0,
                after: 0,
                max_matches: 10,
                max_matches_per_file: None,
                include_hidden: true,
                no_ignore,
            },
            Path::new("/tmp"),
        )
    }

    #[test]
    fn rg_arguments_exclude_heavy_trees_after_caller_glob() {
        let args = rg_args_for("needle", Some("**/*"), false);
        let caller_glob = args
            .iter()
            .position(|argument| argument == "**/*")
            .expect("caller glob passed through");
        let git_exclude = args
            .iter()
            .position(|argument| argument == "!**/.git/**")
            .expect("git exclude always present");
        assert!(
            git_exclude > caller_glob,
            "excludes must come after the caller glob: rg lets the last matching glob decide"
        );
        assert!(args.contains(&"!**/target/**".to_string()));
        assert!(args.contains(&"!**/node_modules/**".to_string()));
    }

    #[test]
    fn rg_arguments_respect_no_ignore_escape_hatch() {
        let args = rg_args_for("needle", Some("**/*"), true);
        assert!(!args.contains(&"!**/.git/**".to_string()));
        assert!(args.contains(&"--no-ignore".to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn deadline_watchdog_kills_a_stuck_child() {
        use std::process::{Command, Stdio};
        let mut child = Command::new("sleep")
            .arg("30")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let finished = Arc::new(AtomicBool::new(false));
        let killed = spawn_deadline_watchdog(
            child.id(),
            Instant::now() + Duration::from_millis(100),
            Arc::clone(&finished),
        );
        let bail_out = Instant::now() + Duration::from_secs(5);
        while child.try_wait().expect("poll child").is_none() {
            assert!(
                Instant::now() < bail_out,
                "watchdog failed to kill the stuck child"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(killed.load(Ordering::SeqCst));
        let _ = child.wait();
    }

    #[test]
    fn rg_search_with_tiny_deadline_returns_instead_of_walking() {
        if !command_available("rg") {
            return; // backend guard, same skip style as the fallback chain
        }
        let workspace_root = test_workspace("search-deadline");
        fs::create_dir_all(&workspace_root).expect("create workspace");
        for index in 0..50 {
            fs::write(
                workspace_root.join(format!("file-{index}.txt")),
                "filler\n",
            )
            .expect("write file");
        }
        let started = Instant::now();
        let output = search_text_rg_with_deadline(
            &workspace_root,
            &workspace_root,
            ResolvedSearchTextOptions {
                pattern: "never-present-needle",
                glob: None,
                fixed_strings: false,
                case_insensitive: false,
                before: 0,
                after: 0,
                max_matches: 100,
                max_matches_per_file: None,
                include_hidden: true,
                no_ignore: false,
            },
            Instant::now() + Duration::from_millis(50),
        )
        .expect("deadline search completes");
        assert_eq!(output.backend, "rg");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "deadline must bound the search, took {:?}",
            started.elapsed()
        );
        let _ = fs::remove_dir_all(workspace_root);
    }
}
