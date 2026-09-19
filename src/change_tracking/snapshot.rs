use std::collections::{HashMap, hash_map::DefaultHasher};
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{self, Read};
use std::path::Path;

use ignore::WalkBuilder;

use super::ignore::{has_project_ignore_file, is_default_ignored_path, is_vcs_admin_path};
use super::{ChangeTarget, MAX_FILE_CAPTURE_BYTES, MAX_WATCHED_ENTRIES};

#[derive(Clone, Debug, Default)]
pub(super) struct WorkspaceSnapshot {
    pub(super) files: HashMap<String, FileSnapshot>,
}

#[derive(Clone, Debug)]
pub(super) struct FileSnapshot {
    pub(super) digest: u64,
    pub(super) size_bytes: usize,
    pub(super) is_binary: bool,
    pub(super) is_directory: bool,
    pub(super) is_symlink: bool,
    pub(super) text: String,
    pub(super) text_truncated: bool,
}

pub(super) fn collect_snapshot(
    workspace_root: &Path,
    targets: &[ChangeTarget],
) -> WorkspaceSnapshot {
    let mut files = HashMap::new();
    let mut remaining = MAX_WATCHED_ENTRIES;

    for target in targets {
        if remaining == 0 {
            break;
        }
        collect_target(workspace_root, target, &mut files, &mut remaining);
    }

    WorkspaceSnapshot { files }
}

fn collect_target(
    workspace_root: &Path,
    target: &ChangeTarget,
    files: &mut HashMap<String, FileSnapshot>,
    remaining: &mut usize,
) {
    if *remaining == 0 || is_vcs_admin_path(workspace_root, &target.path) {
        return;
    }

    let use_default_ignores =
        target.respect_project_ignores && !has_project_ignore_file(workspace_root);
    let Ok(metadata) = fs::symlink_metadata(&target.path) else {
        return;
    };
    let file_type = metadata.file_type();
    if file_type.is_file() || file_type.is_symlink() {
        if use_default_ignores && is_default_ignored_path(&target.path, false) {
            return;
        }
        capture_path(workspace_root, &target.path, files, remaining);
        return;
    }
    if !file_type.is_dir() {
        return;
    }

    if target.respect_project_ignores {
        collect_directory_with_project_ignores(
            workspace_root,
            &target.path,
            target.recursive,
            use_default_ignores,
            files,
            remaining,
        );
    } else {
        collect_directory_explicit(
            workspace_root,
            &target.path,
            target.recursive,
            files,
            remaining,
        );
    }
}

fn collect_directory_with_project_ignores(
    workspace_root: &Path,
    start: &Path,
    recursive: bool,
    use_default_ignores: bool,
    files: &mut HashMap<String, FileSnapshot>,
    remaining: &mut usize,
) {
    let mut builder = WalkBuilder::new(start);
    builder
        .hidden(false)
        .parents(true)
        .ignore(true)
        .git_global(true)
        .git_ignore(true)
        .git_exclude(true)
        .require_git(false)
        .follow_links(false)
        // Automatic change previews must not scan mounted archive/network disks.
        .same_file_system(true)
        .max_depth((!recursive).then_some(1));
    let root = workspace_root.to_path_buf();
    builder.filter_entry(move |entry| {
        if is_vcs_admin_path(&root, entry.path()) {
            return false;
        }
        if !use_default_ignores {
            return true;
        }
        let is_directory = entry
            .file_type()
            .is_some_and(|file_type| file_type.is_dir());
        !is_default_ignored_path(entry.path(), is_directory)
    });

    for entry in builder.build().flatten() {
        if *remaining == 0 {
            return;
        }
        let path = entry.path();
        if is_vcs_admin_path(workspace_root, path) {
            continue;
        }
        let is_directory = entry
            .file_type()
            .is_some_and(|file_type| file_type.is_dir());
        if use_default_ignores && is_default_ignored_path(path, is_directory) {
            continue;
        }
        capture_path(workspace_root, path, files, remaining);
    }
}

fn collect_directory_explicit(
    workspace_root: &Path,
    start: &Path,
    recursive: bool,
    files: &mut HashMap<String, FileSnapshot>,
    remaining: &mut usize,
) {
    capture_path(workspace_root, start, files, remaining);
    if *remaining == 0 {
        return;
    }

    let mut stack = vec![start.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        let mut entries = entries.flatten().collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());

        for entry in entries {
            if *remaining == 0 {
                return;
            }
            let path = entry.path();
            if is_vcs_admin_path(workspace_root, &path) {
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            capture_path(workspace_root, &path, files, remaining);
            if file_type.is_dir() && recursive {
                stack.push(path);
            }
        }
    }
}

fn capture_path(
    workspace_root: &Path,
    path: &Path,
    files: &mut HashMap<String, FileSnapshot>,
    remaining: &mut usize,
) {
    if *remaining == 0 || is_vcs_admin_path(workspace_root, path) {
        return;
    }
    let key = relative_path(workspace_root, path);
    if files.contains_key(&key) {
        return;
    }
    let Some(snapshot) = capture_entry(path) else {
        return;
    };
    files.insert(key, snapshot);
    *remaining = remaining.saturating_sub(1);
}

fn capture_entry(path: &Path) -> Option<FileSnapshot> {
    let metadata = fs::symlink_metadata(path).ok()?;
    let file_type = metadata.file_type();
    if file_type.is_dir() {
        return Some(FileSnapshot {
            digest: 0,
            size_bytes: 0,
            is_binary: false,
            is_directory: true,
            is_symlink: false,
            text: String::new(),
            text_truncated: false,
        });
    }
    if file_type.is_symlink() {
        let target = fs::read_link(path).ok()?.to_string_lossy().into_owned();
        let mut hasher = DefaultHasher::new();
        target.hash(&mut hasher);
        return Some(FileSnapshot {
            digest: hasher.finish(),
            size_bytes: target.len(),
            is_binary: false,
            is_directory: false,
            is_symlink: true,
            text: target,
            text_truncated: false,
        });
    }
    if !file_type.is_file() {
        return None;
    }

    capture_file(fs::File::open(path).ok()?).ok()
}

fn capture_file(mut reader: impl Read) -> io::Result<FileSnapshot> {
    let mut preview = Vec::with_capacity(MAX_FILE_CAPTURE_BYTES);
    let mut buffer = [0u8; 64 * 1024];
    let mut hasher = DefaultHasher::new();
    let mut size_bytes = 0usize;
    loop {
        let count = match reader.read(&mut buffer) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if count == 0 {
            break;
        }
        hasher.write(&buffer[..count]);
        size_bytes = size_bytes.saturating_add(count);
        let keep = count.min(MAX_FILE_CAPTURE_BYTES - preview.len());
        preview.extend_from_slice(&buffer[..keep]);
    }
    let digest = hasher.finish();
    let is_binary = preview.iter().any(|byte| *byte == 0);
    let mut text = String::new();
    let text_truncated = size_bytes > MAX_FILE_CAPTURE_BYTES;

    if !is_binary {
        text = String::from_utf8_lossy(&preview).into_owned();
    }

    Ok(FileSnapshot {
        digest,
        size_bytes,
        is_binary,
        is_directory: false,
        is_symlink: false,
        text,
        text_truncated,
    })
}

#[cfg(test)]
mod bounded_tests {
    use super::*;

    struct ChunkChecked {
        remaining: usize,
        tail: u8,
    }
    impl Read for ChunkChecked {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            assert!(
                buf.len() <= 64 * 1024,
                "snapshot requested an unbounded read buffer"
            );
            let n = self.remaining.min(buf.len());
            buf[..n].fill(b'a');
            self.remaining -= n;
            if self.remaining == 0 && n > 0 {
                buf[n - 1] = self.tail;
            }
            Ok(n)
        }
    }

    #[test]
    fn large_file_is_streamed_and_changes_beyond_preview_are_detected() {
        let first = capture_file(ChunkChecked {
            remaining: 1024 * 1024,
            tail: b'b',
        })
        .unwrap();
        let second = capture_file(ChunkChecked {
            remaining: 1024 * 1024,
            tail: b'c',
        })
        .unwrap();
        assert_eq!(first.size_bytes, 1024 * 1024);
        assert_eq!(first.text.len(), MAX_FILE_CAPTURE_BYTES);
        assert!(first.text_truncated);
        assert_eq!(first.text, second.text);
        assert!(!snapshots_equal(&first, &second));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn automatic_snapshot_does_not_cross_mount_boundary() {
        use std::process::Command;
        if std::env::var_os("CATDESK_MOUNT_TEST_CHILD").is_none() {
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
                .args(["--exact", "change_tracking::snapshot::bounded_tests::automatic_snapshot_does_not_cross_mount_boundary", "--nocapture"])
                .env("CATDESK_MOUNT_TEST_CHILD", "1").output().unwrap();
            assert!(
                result.status.success(),
                "{} {}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }
        // Only this child owns the mount namespace; never mount in the host.
        let root = std::env::temp_dir().join(format!("catdesk-mount-{}", uuid::Uuid::new_v4()));
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
        fs::write(root.join("local.txt"), "local").unwrap();
        fs::write(archive.join("sentinel.txt"), "mounted").unwrap();
        let automatic = collect_snapshot(&root, &[ChangeTarget::discovered(root.clone(), true)]);
        let explicit_root =
            collect_snapshot(&archive, &[ChangeTarget::discovered(archive.clone(), true)]);
        let unmounted = Command::new("umount")
            .arg(&archive)
            .status()
            .unwrap()
            .success();
        fs::remove_dir_all(&root).unwrap();
        assert!(unmounted);
        assert!(automatic.files.contains_key("local.txt"));
        assert!(!automatic.files.contains_key("archive/sentinel.txt"));
        assert!(explicit_root.files.contains_key("sentinel.txt"));
    }
}

pub(super) fn snapshots_equal(left: &FileSnapshot, right: &FileSnapshot) -> bool {
    left.digest == right.digest
        && left.size_bytes == right.size_bytes
        && left.is_binary == right.is_binary
        && left.is_directory == right.is_directory
        && left.is_symlink == right.is_symlink
}

fn relative_path(workspace_root: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(workspace_root).unwrap_or(path);
    if relative.as_os_str().is_empty() {
        return "./".to_string();
    }
    let value = relative.display().to_string();
    #[cfg(windows)]
    {
        value.replace('\\', "/")
    }
    #[cfg(not(windows))]
    {
        value
    }
}
