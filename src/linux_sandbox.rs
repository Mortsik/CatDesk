use std::collections::BTreeSet;
use std::io;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt};
use std::path::{Path, PathBuf};
use std::process::Command;

fn canonical_existing(path: &Path) -> io::Result<PathBuf> {
    path.canonicalize().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("failed to canonicalize {}: {error}", path.display()),
        )
    })
}

fn insert_existing(paths: &mut BTreeSet<PathBuf>, path: impl AsRef<Path>) {
    let path = path.as_ref();
    if let Ok(canonical) = path.canonicalize() {
        paths.insert(canonical);
    }
}

fn insert_env_path_list(paths: &mut BTreeSet<PathBuf>, variable: &str) {
    let Some(value) = std::env::var_os(variable) else {
        return;
    };
    for path in std::env::split_paths(&value) {
        insert_existing(paths, path);
    }
}

fn insert_env_path(paths: &mut BTreeSet<PathBuf>, variable: &str) {
    if let Some(path) = std::env::var_os(variable) {
        insert_existing(paths, PathBuf::from(path));
    }
}

fn insert_ssh_read_paths(paths: &mut BTreeSet<PathBuf>, home: &Path) {
    let ssh_dir = home.join(".ssh");
    for name in ["config", "known_hosts", "known_hosts2"] {
        insert_existing(paths, ssh_dir.join(name));
    }
    if let Ok(entries) = std::fs::read_dir(&ssh_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|extension| extension == "pub") {
                insert_existing(paths, path);
            }
        }
    }
}

fn existing_unix_socket(path: &Path) -> Option<PathBuf> {
    let canonical = path.canonicalize().ok()?;
    std::fs::metadata(&canonical)
        .ok()?
        .file_type()
        .is_socket()
        .then_some(canonical)
}

fn ssh_agent_socket() -> Option<PathBuf> {
    let path = PathBuf::from(std::env::var_os("SSH_AUTH_SOCK")?);
    existing_unix_socket(&path)
}

fn runtime_read_paths() -> BTreeSet<PathBuf> {
    let mut paths = BTreeSet::new();

    for path in ["/bin", "/sbin", "/usr", "/lib", "/lib64", "/etc", "/sys"] {
        insert_existing(&mut paths, path);
    }

    insert_existing(&mut paths, "/etc/resolv.conf");

    // Executables installed outside the standard system prefixes must remain
    // executable when their directory is explicitly present in PATH.
    insert_env_path_list(&mut paths, "PATH");

    // Rust toolchains are commonly installed under the user's home directory.
    // Expose only executable/cache trees from Cargo so registry credentials
    // remain outside the sandbox. Rustup does not store registry credentials.
    if let Some(cargo_home) = std::env::var_os("CARGO_HOME") {
        let cargo_home = PathBuf::from(cargo_home);
        insert_existing(&mut paths, cargo_home.join("bin"));
        insert_existing(&mut paths, cargo_home.join("registry"));
        insert_existing(&mut paths, cargo_home.join("git"));
    }
    insert_env_path(&mut paths, "RUSTUP_HOME");
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        let cargo_home = home.join(".cargo");
        insert_existing(&mut paths, cargo_home.join("bin"));
        insert_existing(&mut paths, cargo_home.join("registry"));
        insert_existing(&mut paths, cargo_home.join("git"));
        insert_existing(&mut paths, home.join(".rustup"));

        // Git treats an unreadable global config as fatal. Grant only the
        // configuration files, keeping credential stores and the rest of HOME
        // inaccessible.
        insert_existing(&mut paths, home.join(".gitconfig"));
        insert_existing(&mut paths, home.join(".config/git/config"));
        insert_ssh_read_paths(&mut paths, &home);
    }

    // Playwright browsers (A2): default cache lives under ~/.cache and is
    // otherwise absent from the namespace, so sandboxed run_command cannot
    // see browsers installed on the host. Bind explicitly, mirroring
    // Playwright resolution order: $PLAYWRIGHT_BROWSERS_PATH >
    // $XDG_CACHE_HOME/ms-playwright > ~/.cache/ms-playwright.
    // insert_* only keeps canonicalizable (existing) paths; --ro-bind-try
    // skips the rest, so a missing cache is safe. Empty values are skipped:
    // an empty XDG_CACHE_HOME would otherwise resolve to the cwd-relative
    // "ms-playwright". HOME itself stays unbound
    // (see runtime_read_paths_do_not_grant_the_home_directory_itself).
    if let Some(browsers) = std::env::var_os("PLAYWRIGHT_BROWSERS_PATH")
        && !browsers.is_empty()
    {
        insert_existing(&mut paths, PathBuf::from(browsers));
    }
    if let Some(xdg_cache) = std::env::var_os("XDG_CACHE_HOME")
        && !xdg_cache.is_empty()
    {
        insert_existing(
            &mut paths,
            PathBuf::from(xdg_cache).join("ms-playwright"),
        );
    }
    if let Some(home) = std::env::var_os("HOME")
        && !home.is_empty()
    {
        insert_existing(
            &mut paths,
            PathBuf::from(home).join(".cache/ms-playwright"),
        );
    }

    paths
}

/// Locate an executable `bwrap` on PATH. Bubblewrap confines through mount
/// namespaces rather than an LSM, so it works on kernels far older than
/// Landlock's 5.13 baseline -- RHEL 8 / Rocky 8 (4.18), Ubuntu 20.04 (5.4) and
/// Debian 11 (5.10) included.
///
/// A non-executable file named `bwrap` earlier in PATH must not shadow a real
/// one later, so the execute bit is checked rather than just the file type.
fn bubblewrap_executable_in_paths(
    paths: impl IntoIterator<Item = PathBuf>,
    workspace: &Path,
) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let workspace = workspace.canonicalize().ok();
    paths
        .into_iter()
        .map(|dir| dir.join("bwrap"))
        .find_map(|candidate| {
            let metadata = std::fs::metadata(&candidate).ok()?;
            if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
                return None;
            }
            let canonical = candidate.canonicalize().ok()?;
            if workspace
                .as_ref()
                .is_some_and(|root| canonical.starts_with(root))
            {
                None
            } else {
                Some(canonical)
            }
        })
}

fn bubblewrap_executable(workspace: &Path) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    bubblewrap_executable_in_paths(std::env::split_paths(&path), workspace)
}

/// Build a bubblewrap invocation that confines `command` to `workspace` plus its
/// private `scratch` directory.
///
/// The namespace contains only what [`runtime_read_paths`] returns plus the
/// workspace and scratch directories, so an unbound path is simply absent
/// rather than merely denied. `--dev /dev` supplies a minimal set of device
/// nodes (`/dev/null`, `/dev/zero`, `/dev/random`, `/dev/tty` and the like),
/// `--tmpfs /tmp` keeps the host's `/tmp` out of reach, and `--unshare-pid`
/// hides host processes. `--new-session` gives the sandboxed command its
/// own session.
fn bubblewrap_command(
    bwrap: &Path,
    command: &str,
    workspace: &Path,
    cwd: &Path,
    scratch: &Path,
) -> io::Result<Command> {
    let workspace = canonical_existing(workspace)?;
    let cwd = canonical_existing(cwd)?;
    let scratch = canonical_existing(scratch)?;

    let mut bwrap_command = Command::new(bwrap);
    bwrap_command
        .arg("--unshare-user")
        .arg("--unshare-pid")
        .arg("--unshare-ipc")
        .arg("--unshare-uts")
        .arg("--new-session")
        .arg("--die-with-parent")
        .arg("--proc")
        .arg("/proc")
        .arg("--dev")
        .arg("/dev")
        .arg("--tmpfs")
        .arg("/tmp");

    for path in runtime_read_paths() {
        bwrap_command.arg("--ro-bind-try").arg(&path).arg(&path);
    }

    // Root-owned SSH client config appears as uid 65534 inside the unprivileged
    // user namespace, which OpenSSH rejects before authentication. Hide the
    // system config and let OpenSSH use the read-only user config/defaults.
    if Path::new("/etc/ssh").is_dir() {
        bwrap_command.arg("--tmpfs").arg("/etc/ssh");
    }

    let ssh_agent_socket = ssh_agent_socket();
    if let Some(socket) = &ssh_agent_socket {
        // Forward only the agent socket. Private key files remain outside the
        // sandbox while Git/SSH can authenticate and perform SSH signing.
        bwrap_command.arg("--bind").arg(socket).arg(socket);
    }

    // Replicate merged-/usr symlinks. runtime_read_paths canonicalises, so on
    // distributions where /bin, /sbin, /lib and /lib64 are symlinks into /usr
    // it yields only the /usr targets. Bubblewrap builds a fresh namespace:
    // without these links /bin/bash does not exist and every sandboxed command
    // fails with "execvp /bin/bash: No such file or directory".
    for link in ["/bin", "/sbin", "/lib", "/lib64"] {
        let link = Path::new(link);
        if let Ok(target) = std::fs::read_link(link) {
            bwrap_command.arg("--symlink").arg(target).arg(link);
        }
    }

    for path in [&workspace, &scratch] {
        bwrap_command.arg("--bind").arg(path).arg(path);
    }

    bwrap_command.arg("--chdir").arg(&cwd);
    if let Some(socket) = &ssh_agent_socket {
        bwrap_command
            .arg("--setenv")
            .arg("SSH_AUTH_SOCK")
            .arg(socket);
    }
    bwrap_command
        .arg("--setenv")
        .arg("TMPDIR")
        .arg(&scratch)
        .arg("--setenv")
        .arg("TMP")
        .arg(&scratch)
        .arg("--setenv")
        .arg("TEMP")
        .arg(&scratch)
        .arg("/bin/bash")
        .arg("-c")
        .arg(command);

    Ok(bwrap_command)
}

/// Build the command that runs `command` confined to `workspace`, together with
/// the private scratch directory created for it.
///
/// Confinement is through bubblewrap, which builds a fresh mount namespace
/// containing only the allowlisted paths. When `bwrap` is not on `PATH` the
/// error says so, since the caller cannot run anything unconfined.
///
/// The scratch directory is removed again if the command could not be prepared.
pub fn helper_command(
    command: &str,
    workspace: &Path,
    cwd: &Path,
) -> io::Result<(Command, PathBuf)> {
    let scratch_dir =
        std::env::temp_dir().join(format!("catdesk-sandbox-{}", uuid::Uuid::new_v4()));
    let mut dir_builder = std::fs::DirBuilder::new();
    dir_builder
        .mode(0o700)
        .create(&scratch_dir)
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "failed to create sandbox scratch directory {}: {error}",
                    scratch_dir.display()
                ),
            )
        })?;

    let prepared = match bubblewrap_executable(workspace) {
        Some(bwrap) => bubblewrap_command(&bwrap, command, workspace, cwd, &scratch_dir),
        None => Err(io::Error::other(
            "no usable sandbox: bwrap was not found on PATH outside the workspace. Install \
             bubblewrap to run commands confined.",
        )),
    };

    match prepared {
        Ok(prepared) => Ok((prepared, scratch_dir)),
        Err(error) => {
            let _ = std::fs::remove_dir_all(&scratch_dir);
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    /// Serializes tests that read or mutate the process-global env consumed
    /// by `runtime_read_paths` (cargo runs tests in threads, so an env
    /// mutation in one test would otherwise leak into a concurrent reader).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Locks `ENV_LOCK`, tolerating poisoning: a test that fails while
    /// holding the lock must not cascade into every other env test.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    /// Scoped env overrides: sets the given vars for the test body, holds
    /// `ENV_LOCK` for the whole scope, and restores previous values on drop
    /// (including on panic). One lock acquisition per scope, so multi-var
    /// tests must go through a single `set_many` call (std Mutex is not
    /// reentrant).
    struct EnvGuards {
        prev: Vec<(&'static str, Option<std::ffi::OsString>)>,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl EnvGuards {
        fn set_many(pairs: &[(&'static str, &Path)]) -> Self {
            let guard = env_lock();
            // Snapshot every previous value before mutating anything, so a
            // mid-loop panic cannot leave an earlier var unrestored.
            let prev: Vec<(&'static str, Option<std::ffi::OsString>)> = pairs
                .iter()
                .map(|(name, _)| (*name, std::env::var_os(*name)))
                .collect();
            for (name, value) in pairs {
                // SAFETY: ENV_LOCK serializes all in-process env readers and
                // writers in this test module; no other thread observes the
                // swap. Restore happens in Drop while still holding the lock.
                unsafe { std::env::set_var(*name, *value) };
            }
            Self {
                prev,
                _guard: guard,
            }
        }

        fn set(name: &'static str, value: &Path) -> Self {
            Self::set_many(&[(name, value)])
        }

        /// Read-only env tests go through here so every env access in this
        /// module holds the lock via a single entry point (std Mutex is not
        /// reentrant — never nest `read`/`set`/`set_many`).
        fn read() -> Self {
            Self::set_many(&[])
        }
    }

    impl Drop for EnvGuards {
        fn drop(&mut self) {
            // Reverse order so duplicate keys restore to the original value.
            for (name, value) in self.prev.iter().rev() {
                // SAFETY: same serialization as in `set_many`; Drop runs
                // before `_guard` is released.
                match value {
                    Some(value) => unsafe { std::env::set_var(*name, value) },
                    None => unsafe { std::env::remove_var(*name) },
                }
            }
        }
    }

    #[test]
    fn runtime_read_paths_include_resolv_conf_target() {
        let _env = EnvGuards::read();
        let resolv_conf = Path::new("/etc/resolv.conf")
            .canonicalize()
            .expect("canonical /etc/resolv.conf");
        assert!(runtime_read_paths().contains(&resolv_conf));
    }

    #[test]
    fn runtime_read_paths_include_ssh_known_hosts_target() {
        let _env = EnvGuards::read();
        let Some(home) = std::env::var_os("HOME") else {
            return;
        };
        let known_hosts = PathBuf::from(home).join(".ssh/known_hosts");
        let Ok(known_hosts) = known_hosts.canonicalize() else {
            return;
        };
        assert!(runtime_read_paths().contains(&known_hosts));
    }

    #[test]
    fn ssh_read_paths_include_config_known_hosts_and_public_keys_only() {
        let tree = TempTree::new();
        let ssh_dir = tree.path().join(".ssh");
        std::fs::create_dir_all(&ssh_dir).expect("create .ssh");
        let config = ssh_dir.join("config");
        let known_hosts = ssh_dir.join("known_hosts");
        let public_key = ssh_dir.join("id_ed25519.pub");
        let private_key = ssh_dir.join("id_ed25519");
        for path in [&config, &known_hosts, &public_key, &private_key] {
            std::fs::write(path, b"test\n").expect("write ssh fixture");
        }

        let mut paths = BTreeSet::new();
        insert_ssh_read_paths(&mut paths, tree.path());

        assert!(paths.contains(&config.canonicalize().expect("canonical config")));
        assert!(paths.contains(&known_hosts.canonicalize().expect("canonical known_hosts")));
        assert!(paths.contains(&public_key.canonicalize().expect("canonical public key")));
        assert!(!paths.contains(&private_key.canonicalize().expect("canonical private key")));
    }

    #[test]
    fn existing_unix_socket_accepts_socket_and_rejects_regular_file() {
        use std::os::unix::net::UnixListener;

        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let socket = PathBuf::from(format!("/tmp/cd-{}.sock", &suffix[..8]));
        let regular = PathBuf::from(format!("/tmp/cd-{}.file", &suffix[..8]));
        let _listener = UnixListener::bind(&socket).expect("bind unix socket");
        std::fs::write(&regular, b"not a socket").expect("write regular file");

        assert_eq!(
            existing_unix_socket(&socket),
            Some(socket.canonicalize().expect("canonical socket"))
        );
        assert_eq!(existing_unix_socket(&regular), None);

        let _ = std::fs::remove_file(&socket);
        let _ = std::fs::remove_file(&regular);
    }

    #[test]
    fn runtime_read_paths_do_not_grant_the_home_directory_itself() {
        let _env = EnvGuards::read();
        let Some(home) = std::env::var_os("HOME") else {
            return;
        };
        let home = PathBuf::from(home).canonicalize().expect("canonical HOME");
        assert!(!runtime_read_paths().contains(&home));
    }

    #[test]
    fn runtime_read_paths_include_playwright_browsers_path() {
        let tree = TempTree::new();
        let browsers = tree.path().join("browsers");
        std::fs::create_dir_all(&browsers).expect("create browsers dir");
        let canonical = browsers.canonicalize().expect("canonical browsers");
        let _env = EnvGuards::set("PLAYWRIGHT_BROWSERS_PATH", &canonical);
        assert!(runtime_read_paths().contains(&canonical));
    }

    #[test]
    fn runtime_read_paths_include_xdg_playwright_cache() {
        let tree = TempTree::new();
        let cache = tree.path().join("xdg-cache").join("ms-playwright");
        std::fs::create_dir_all(&cache).expect("create xdg cache dir");
        let canonical = cache.canonicalize().expect("canonical xdg cache");
        let _env = EnvGuards::set("XDG_CACHE_HOME", &tree.path().join("xdg-cache"));
        assert!(runtime_read_paths().contains(&canonical));
    }

    #[test]
    fn runtime_read_paths_include_default_playwright_cache() {
        let tree = TempTree::new();
        let cache = tree.path().join("home").join(".cache/ms-playwright");
        std::fs::create_dir_all(&cache).expect("create default cache dir");
        let canonical = cache.canonicalize().expect("canonical default cache");
        let _env = EnvGuards::set("HOME", &tree.path().join("home"));
        assert!(runtime_read_paths().contains(&canonical));
    }

    #[test]
    fn runtime_read_paths_skip_missing_playwright_dirs() {
        let tree = TempTree::new();
        let missing = tree.path().join("does-not-exist");
        let empty_home = tree.path().join("empty-home");
        std::fs::create_dir_all(&empty_home).expect("create empty home");
        let _env = EnvGuards::set_many(&[
            ("PLAYWRIGHT_BROWSERS_PATH", &missing),
            ("XDG_CACHE_HOME", &missing),
            ("HOME", &empty_home),
        ]);
        let paths = runtime_read_paths();
        assert!(!paths.contains(&missing));
        assert!(!paths.contains(&missing.join("ms-playwright")));
        // Prefix-absence: no bound path may live under the temp home at all,
        // so a bug inserting the wrong child there still goes red. Compare
        // canonical against canonical — TempTree may sit under a symlinked
        // TMPDIR, where a literal prefix would miss real escapes.
        let empty_home = empty_home.canonicalize().expect("canonical home");
        assert!(!paths.contains(&empty_home));
        assert!(paths.iter().all(|path| !path.starts_with(&empty_home)));
    }

    #[test]
    fn helper_command_creates_private_scratch_directory() {
        use std::os::unix::fs::PermissionsExt;
        let _env = EnvGuards::read();

        // bwrap may not be installed in every environment. helper_command
        // reports that rather than returning a command, so there is nothing to
        // assert about the scratch directory here.
        if bubblewrap_executable(Path::new(".")).is_none() {
            return;
        }

        let (_command, scratch) = helper_command("true", Path::new("."), Path::new("."))
            .expect("prepare sandbox helper command");
        let mode = std::fs::metadata(&scratch)
            .expect("scratch metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);
        std::fs::remove_dir_all(scratch).expect("remove scratch directory");
    }

    #[test]
    fn bubblewrap_command_chdirs_to_cwd_and_uses_new_session() {
        let _env = EnvGuards::read();
        let tree = TempTree::new();
        let workspace = tree.path().join("workspace");
        let cwd = workspace.join("src");
        let scratch = tree.path().join("scratch");
        std::fs::create_dir_all(&cwd).expect("create cwd");
        std::fs::create_dir_all(&scratch).expect("create scratch");

        let command = bubblewrap_command(
            Path::new("/usr/bin/bwrap"),
            "pwd",
            &workspace,
            &cwd,
            &scratch,
        )
        .expect("build bubblewrap command");
        let args: Vec<_> = command.get_args().map(|arg| arg.to_os_string()).collect();

        assert!(args.iter().any(|arg| arg.as_os_str() == "--new-session"));
        if Path::new("/etc/ssh").is_dir() {
            assert!(args.windows(2).any(|pair| {
                pair[0].as_os_str() == OsStr::new("--tmpfs")
                    && pair[1].as_os_str() == OsStr::new("/etc/ssh")
            }));
        }
        let chdir = args
            .windows(2)
            .find(|pair| pair[0].as_os_str() == OsStr::new("--chdir"))
            .map(|pair| PathBuf::from(pair[1].clone()))
            .expect("--chdir argument");
        assert_eq!(chdir, cwd.canonicalize().expect("canonical cwd"));
    }

    #[test]
    fn bubblewrap_executable_skips_workspace_symlink_and_uses_later_candidate() {
        use std::os::unix::fs::PermissionsExt;

        let tree = TempTree::new();
        let workspace = tree.path().join("workspace");
        let workspace_bin = workspace.join("bin");
        std::fs::create_dir_all(&workspace_bin).expect("create workspace bin");
        let hijacked = workspace_bin.join("bwrap");
        std::fs::write(&hijacked, b"#!/bin/sh\n").expect("write workspace bwrap");
        std::fs::set_permissions(&hijacked, std::fs::Permissions::from_mode(0o755))
            .expect("chmod workspace bwrap");

        let symlink_bin = tree.path().join("symlink-bin");
        std::fs::create_dir_all(&symlink_bin).expect("create symlink bin");
        std::os::unix::fs::symlink(&hijacked, symlink_bin.join("bwrap")).expect("symlink bwrap");

        let real_bin = tree.path().join("real-bin");
        std::fs::create_dir_all(&real_bin).expect("create real bin");
        let real = real_bin.join("bwrap");
        std::fs::write(&real, b"#!/bin/sh\n").expect("write real bwrap");
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755))
            .expect("chmod real bwrap");

        assert_eq!(
            bubblewrap_executable_in_paths(vec![symlink_bin, real_bin], &workspace),
            Some(real.canonicalize().expect("canonical real bwrap"))
        );
    }

    struct TempTree(PathBuf);

    impl TempTree {
        fn new() -> Self {
            let dir =
                std::env::temp_dir().join(format!("catdesk-sandbox-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).expect("create temp tree");
            Self(dir.canonicalize().expect("canonical temp tree"))
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
