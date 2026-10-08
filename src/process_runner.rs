use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Instant;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, ChildStderr, ChildStdout, Command};
use tokio::time::{Duration, timeout};

use crate::process_exit::{EXIT_CODE_INTERNAL_ERROR, EXIT_CODE_TIMEOUT, exit_code_for_status};

const READ_CHUNK_BYTES: usize = 8 * 1024;

#[cfg(unix)]
pub(crate) fn exit_status_signal_diagnostic(status: &std::process::ExitStatus) -> Option<String> {
    use std::os::unix::process::ExitStatusExt;

    let signal = status.signal()?;
    let name = match signal {
        libc::SIGHUP => "SIGHUP",
        libc::SIGINT => "SIGINT",
        libc::SIGQUIT => "SIGQUIT",
        libc::SIGILL => "SIGILL",
        libc::SIGABRT => "SIGABRT",
        libc::SIGFPE => "SIGFPE",
        libc::SIGKILL => "SIGKILL",
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGPIPE => "SIGPIPE",
        libc::SIGALRM => "SIGALRM",
        libc::SIGTERM => "SIGTERM",
        libc::SIGBUS => "SIGBUS",
        libc::SIGXCPU => "SIGXCPU",
        libc::SIGXFSZ => "SIGXFSZ",
        _ => "UNKNOWN",
    };
    let core = if status.core_dumped() {
        " (core dumped)"
    } else {
        ""
    };
    Some(format!(
        "Command terminated by signal {signal} ({name}){core}"
    ))
}

#[cfg(not(unix))]
pub(crate) fn exit_status_signal_diagnostic(_status: &std::process::ExitStatus) -> Option<String> {
    None
}

#[derive(Debug)]
pub struct ProcessRunResult {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
    pub exit_code: Option<i32>,
    pub elapsed_ms: u64,
    pub timed_out: bool,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

/// A spawned shell process owned by CatDesk.
///
/// Dropping this value is intentionally destructive: if the command is still
/// alive, CatDesk terminates the process tree. This is what keeps a cancelled
/// MCP request from leaving a compiler or build process behind.
pub struct SpawnedProcess {
    child: Child,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
    tree: ProcessTreeGuard,
    cleanup_dir: Option<PathBuf>,
    /// The resolved `systemd-run` that wrapped this launch, when a transient
    /// scope was created. Read by the scoped-launch retry in
    /// `run_shell_command` once the command's outcome is known.
    scope: Option<PathBuf>,
}

impl SpawnedProcess {
    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.stdout.take()
    }

    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.stderr.take()
    }

    pub async fn wait(&mut self) -> io::Result<std::process::ExitStatus> {
        self.child.wait().await
    }

    /// Terminate the root process and all descendants owned by this command.
    pub async fn terminate_tree(&mut self) {
        self.tree.terminate().await;
        // Job-object / process-group termination should already include the
        // root, but keep Tokio's direct kill as a best-effort fallback.
        let _ = self.child.start_kill();
    }

    /// Finalize ownership after the root process exits. Any descendants still
    /// alive at that point are terminated so a command cannot silently detach
    /// work that outlives its CatDesk job.
    pub async fn disarm(&mut self) {
        self.tree.disarm().await;
        self.cleanup_command_dir();
    }

    fn cleanup_command_dir(&mut self) {
        if let Some(path) = self.cleanup_dir.take() {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

impl Drop for SpawnedProcess {
    fn drop(&mut self) {
        if self.tree.is_armed() {
            self.tree.terminate_blocking();
            let _ = self.child.start_kill();
        }
        self.cleanup_command_dir();
    }
}

#[derive(Debug)]
struct ProcessTreeGuard {
    pid: u32,
    armed: bool,
    #[cfg(windows)]
    job_handle: Option<usize>,
}

impl ProcessTreeGuard {
    #[cfg(not(windows))]
    fn new(pid: u32) -> Self {
        Self { pid, armed: true }
    }

    #[cfg(windows)]
    fn with_windows_job(pid: u32, job_handle: usize) -> Self {
        Self {
            pid,
            armed: true,
            job_handle: Some(job_handle),
        }
    }

    fn is_armed(&self) -> bool {
        self.armed
    }

    async fn disarm(&mut self) {
        if !self.armed {
            return;
        }
        #[cfg(windows)]
        {
            if self.job_handle.is_some() {
                close_windows_job(&mut self.job_handle);
            } else {
                terminate_process_tree_async(self.pid).await;
            }
        }
        #[cfg(not(windows))]
        terminate_process_tree(self.pid);
        self.armed = false;
    }

    async fn terminate(&mut self) {
        if !self.armed {
            return;
        }
        #[cfg(windows)]
        {
            if !terminate_windows_job(&mut self.job_handle) {
                terminate_process_tree_async(self.pid).await;
            }
        }
        #[cfg(not(windows))]
        terminate_process_tree(self.pid);
        self.armed = false;
    }

    fn terminate_blocking(&mut self) {
        if !self.armed {
            return;
        }
        #[cfg(windows)]
        {
            if !terminate_windows_job(&mut self.job_handle) {
                terminate_process_tree_blocking(self.pid);
            }
        }
        #[cfg(not(windows))]
        terminate_process_tree(self.pid);
        self.armed = false;
    }
}

impl Drop for ProcessTreeGuard {
    fn drop(&mut self) {
        self.terminate_blocking();
    }
}

#[cfg(windows)]
fn create_windows_job_for_process(pid: u32) -> io::Result<usize> {
    use std::ffi::c_void;
    use std::mem::{size_of, zeroed};
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };

    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return Err(io::Error::last_os_error());
        }

        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const c_void,
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) == 0
        {
            let error = io::Error::last_os_error();
            CloseHandle(job);
            return Err(error);
        }

        let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
        if process.is_null() {
            let error = io::Error::last_os_error();
            CloseHandle(job);
            return Err(error);
        }
        let assigned = AssignProcessToJobObject(job, process) != 0;
        let assign_error = if assigned {
            None
        } else {
            Some(io::Error::last_os_error())
        };
        CloseHandle(process);
        if let Some(error) = assign_error {
            CloseHandle(job);
            return Err(error);
        }

        Ok(job as usize)
    }
}

#[cfg(windows)]
fn resume_windows_process(pid: u32) -> io::Result<()> {
    use std::mem::{size_of, zeroed};
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};

    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }

        let mut entry: THREADENTRY32 = zeroed();
        entry.dwSize = size_of::<THREADENTRY32>() as u32;
        let mut found = Thread32First(snapshot, &mut entry) != 0;
        while found {
            if entry.th32OwnerProcessID == pid {
                let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                if thread.is_null() {
                    let error = io::Error::last_os_error();
                    CloseHandle(snapshot);
                    return Err(error);
                }
                let previous_suspend_count = ResumeThread(thread);
                let resume_error = if previous_suspend_count == u32::MAX {
                    Some(io::Error::last_os_error())
                } else {
                    None
                };
                CloseHandle(thread);
                CloseHandle(snapshot);
                return match resume_error {
                    Some(error) => Err(error),
                    None => Ok(()),
                };
            }
            found = Thread32Next(snapshot, &mut entry) != 0;
        }

        CloseHandle(snapshot);
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "suspended process did not expose a resumable thread",
        ))
    }
}

#[cfg(windows)]
fn close_windows_job(job_handle: &mut Option<usize>) {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    if let Some(raw) = job_handle.take() {
        unsafe {
            CloseHandle(raw as HANDLE);
        }
    }
}

#[cfg(windows)]
fn terminate_windows_job(job_handle: &mut Option<usize>) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::TerminateJobObject;
    let Some(raw) = job_handle.take() else {
        return false;
    };
    let handle = raw as HANDLE;
    unsafe {
        let terminated = TerminateJobObject(handle, 1) != 0;
        CloseHandle(handle);
        terminated
    }
}

#[cfg(windows)]
fn terminate_process_tree_blocking(pid: u32) {
    // `/T` includes descendants and `/F` makes cancellation deterministic.
    // Use the executable directly rather than a shell command so the PID never
    // passes through shell parsing. This synchronous path is reserved for Drop,
    // where Rust cannot await cleanup.
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(windows)]
async fn terminate_process_tree_async(pid: u32) {
    let _ = tokio::task::spawn_blocking(move || terminate_process_tree_blocking(pid)).await;
}

#[cfg(unix)]
fn terminate_process_tree(pid: u32) {
    let pgid = match i32::try_from(pid) {
        Ok(value) => value,
        Err(_) => return,
    };
    // The shell is placed in its own process group at spawn time. A negative
    // PID targets the complete process group, including compiler descendants.
    unsafe {
        let _ = libc::kill(-pgid, libc::SIGKILL);
    }
}

#[cfg(not(any(windows, unix)))]
fn terminate_process_tree(_pid: u32) {}

struct PreparedShellCommand {
    command: Command,
    cwd: PathBuf,
    cleanup_dir: Option<PathBuf>,
    /// The resolved `systemd-run` wrapping the launch when a transient scope
    /// was actually decided; `None` on every non-scoped path (other platforms,
    /// plain bubblewrap). Only a scoped launch can fail on a dead bus, so the
    /// retry in `run_shell_command` keys off this.
    scope: Option<PathBuf>,
}

#[cfg(target_os = "linux")]
fn prepare_linux_sandbox_command(
    command: &str,
    workspace_root: &Path,
    cwd: &Path,
) -> io::Result<PreparedShellCommand> {
    let helper = crate::linux_sandbox::helper_command(command, workspace_root, cwd)?;
    Ok(PreparedShellCommand {
        command: Command::from(helper.command),
        cwd: cwd.to_path_buf(),
        cleanup_dir: Some(helper.scratch_dir),
        scope: helper.scope,
    })
}

fn shell_command(
    command: &str,
    workspace_root: &Path,
    cwd: &Path,
) -> io::Result<PreparedShellCommand> {
    #[cfg(windows)]
    {
        let _ = workspace_root;
        let _ = cwd;
        let mut shell = Command::new("powershell.exe");
        shell
            .arg("-NoLogo")
            .arg("-NoProfile")
            .arg("-NonInteractive")
            .arg("-ExecutionPolicy")
            .arg("Bypass")
            .arg("-Command")
            .arg(command);
        Ok(PreparedShellCommand {
            command: shell,
            cwd: cwd.to_path_buf(),
            cleanup_dir: None,
            scope: None,
        })
    }

    #[cfg(all(not(windows), not(target_os = "linux")))]
    {
        let _ = workspace_root;
        let _ = cwd;
        let mut shell = Command::new("/bin/bash");
        shell.arg("-c").arg(command);
        Ok(PreparedShellCommand {
            command: shell,
            cwd: cwd.to_path_buf(),
            cleanup_dir: None,
            scope: None,
        })
    }

    #[cfg(all(target_os = "linux", not(test)))]
    {
        prepare_linux_sandbox_command(command, workspace_root, cwd)
    }

    #[cfg(all(target_os = "linux", test))]
    {
        let _ = workspace_root;
        let _ = cwd;
        let mut shell = Command::new("/bin/bash");
        shell.arg("-c").arg(command);
        Ok(PreparedShellCommand {
            command: shell,
            cwd: cwd.to_path_buf(),
            cleanup_dir: None,
            scope: None,
        })
    }
}

fn spawn_prepared_shell_command(prepared: PreparedShellCommand) -> io::Result<SpawnedProcess> {
    let mut shell = prepared.command;
    let cwd = prepared.cwd;
    let scope = prepared.scope;
    let mut cleanup_dir = prepared.cleanup_dir;
    shell
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        shell.as_std_mut().process_group(0);
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::CREATE_SUSPENDED;
        shell.as_std_mut().creation_flags(CREATE_SUSPENDED);
    }

    let mut child = match shell.spawn() {
        Ok(child) => child,
        Err(error) => {
            if let Some(path) = cleanup_dir.take() {
                let _ = std::fs::remove_dir_all(path);
            }
            return Err(error);
        }
    };
    let Some(pid) = child.id() else {
        let _ = child.start_kill();
        if let Some(path) = cleanup_dir.take() {
            let _ = std::fs::remove_dir_all(path);
        }
        return Err(io::Error::other(
            "spawned command did not expose a process id",
        ));
    };

    #[cfg(windows)]
    let tree = {
        let job_handle = match create_windows_job_for_process(pid) {
            Ok(handle) => handle,
            Err(error) => {
                let _ = child.start_kill();
                return Err(io::Error::new(
                    error.kind(),
                    format!("failed to assign suspended command to Windows Job Object: {error}"),
                ));
            }
        };
        if let Err(error) = resume_windows_process(pid) {
            let mut job_handle = Some(job_handle);
            close_windows_job(&mut job_handle);
            let _ = child.start_kill();
            return Err(io::Error::new(
                error.kind(),
                format!("failed to resume suspended command process: {error}"),
            ));
        }
        ProcessTreeGuard::with_windows_job(pid, job_handle)
    };

    #[cfg(not(windows))]
    let tree = ProcessTreeGuard::new(pid);

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    Ok(SpawnedProcess {
        child,
        stdout,
        stderr,
        tree,
        cleanup_dir,
        scope,
    })
}

#[cfg(not(target_os = "linux"))]
fn spawn_shell_command_blocking(
    command: &str,
    workspace_root: &Path,
    cwd: &Path,
) -> io::Result<SpawnedProcess> {
    spawn_prepared_shell_command(shell_command(command, workspace_root, cwd)?)
}

#[cfg(target_os = "linux")]
async fn prepare_linux_command_async(
    command: String,
    workspace_root: PathBuf,
    cwd: PathBuf,
    force_sandbox: bool,
) -> io::Result<PreparedShellCommand> {
    tokio::task::spawn_blocking(move || {
        if force_sandbox {
            prepare_linux_sandbox_command(&command, &workspace_root, &cwd)
        } else {
            shell_command(&command, &workspace_root, &cwd)
        }
    })
    .await
    .map_err(|error| io::Error::other(format!("command preparation task failed: {error}")))?
}

#[cfg(all(target_os = "linux", test))]
async fn spawn_linux_sandboxed_for_test(
    command: &str,
    workspace_root: &Path,
    cwd: &Path,
) -> io::Result<SpawnedProcess> {
    let prepared = prepare_linux_command_async(
        command.to_owned(),
        workspace_root.to_path_buf(),
        cwd.to_path_buf(),
        true,
    )
    .await?;
    // bwrap --die-with-parent must be spawned by a runtime worker that lives
    // for the runtime lifetime, never by a transient spawn_blocking thread.
    spawn_prepared_shell_command(prepared)
}

pub async fn spawn_shell_command(
    command: &str,
    workspace_root: &Path,
    cwd: &Path,
) -> io::Result<SpawnedProcess> {
    spawn_shell_command_with_mode(command, workspace_root, cwd, false).await
}

/// [`spawn_shell_command`] with `force_sandbox`, mirroring
/// [`prepare_linux_command_async`]: production Linux is always sandboxed,
/// while the test build normally prepares a plain shell and needs the flag to
/// exercise the real sandbox — including its scoped-launch retry — hermetically.
async fn spawn_shell_command_with_mode(
    command: &str,
    workspace_root: &Path,
    cwd: &Path,
    force_sandbox: bool,
) -> io::Result<SpawnedProcess> {
    // Operator-only actions are denied before spawn so the explanation lands
    // in the command's own error channel (see command_policy for the why).
    if let Err(denied) = crate::command_policy::check_vm_bounce(command) {
        return Err(io::Error::other(denied));
    }
    #[cfg(target_os = "linux")]
    {
        let prepared = prepare_linux_command_async(
            command.to_owned(),
            workspace_root.to_path_buf(),
            cwd.to_path_buf(),
            force_sandbox,
        )
        .await?;
        // On Linux the prepared command is bwrap --die-with-parent in
        // production. Spawn it on a long-lived runtime worker: spawning it in
        // spawn_blocking makes bwrap inherit an ephemeral parent thread and it
        // receives SIGKILL when Tokio retires that thread.
        return spawn_prepared_shell_command(prepared);
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = force_sandbox;
        let command = command.to_owned();
        let workspace_root = workspace_root.to_path_buf();
        let cwd = cwd.to_path_buf();
        tokio::task::spawn_blocking(move || {
            spawn_shell_command_blocking(&command, &workspace_root, &cwd)
        })
        .await
        .map_err(|error| io::Error::other(format!("command spawn task failed: {error}")))?
    }
}

#[derive(Debug)]
struct BoundedBytes {
    bytes: Vec<u8>,
    max_bytes: usize,
    truncated: bool,
}

impl BoundedBytes {
    fn new(max_bytes: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(max_bytes.min(64 * 1024)),
            max_bytes,
            truncated: false,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        if self.max_bytes == 0 {
            self.truncated |= !chunk.is_empty();
            return;
        }
        let remaining = self.max_bytes.saturating_sub(self.bytes.len());
        if chunk.len() <= remaining {
            self.bytes.extend_from_slice(chunk);
            return;
        }
        self.bytes.extend_from_slice(&chunk[..remaining]);
        self.truncated = true;
    }

    fn into_text(self) -> (String, bool) {
        (
            String::from_utf8_lossy(&self.bytes).into_owned(),
            self.truncated,
        )
    }
}

#[derive(Debug, Default)]
struct CapturedOutput {
    text: String,
    truncated: bool,
    read_error: Option<String>,
}

async fn capture_reader<R>(mut reader: R, max_bytes: usize) -> CapturedOutput
where
    R: AsyncRead + Unpin,
{
    let mut output = BoundedBytes::new(max_bytes);
    let mut buffer = vec![0_u8; READ_CHUNK_BYTES];
    let mut read_error = None;
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => break,
            Ok(read) => output.push(&buffer[..read]),
            Err(error) => {
                read_error = Some(error.to_string());
                break;
            }
        }
    }
    let (text, truncated) = output.into_text();
    CapturedOutput {
        text,
        truncated,
        read_error,
    }
}

async fn finish_capture(
    task: Option<tokio::task::JoinHandle<CapturedOutput>>,
    stream: &str,
) -> CapturedOutput {
    let Some(task) = task else {
        return CapturedOutput::default();
    };
    match task.await {
        Ok(captured) => captured,
        Err(error) => CapturedOutput {
            read_error: Some(format!("{stream} capture task failed: {error}")),
            ..CapturedOutput::default()
        },
    }
}

fn append_stderr_diagnostic(stderr: &mut String, message: &str) {
    if !stderr.is_empty() && !stderr.ends_with('\n') {
        stderr.push('\n');
    }
    stderr.push_str(message);
}

pub async fn run_shell_command(
    command: &str,
    workspace_root: &Path,
    cwd: &Path,
    timeout_ms: u64,
    max_capture_bytes: usize,
) -> ProcessRunResult {
    run_shell_command_with_mode(
        command,
        workspace_root,
        cwd,
        timeout_ms,
        max_capture_bytes,
        false,
    )
    .await
}

/// [`run_shell_command`] with `force_sandbox`, so tests can drive the real
/// Linux sandbox (and its scoped-launch retry) through the same pipeline the
/// production `run_shell_command` uses.
#[cfg(all(target_os = "linux", test))]
async fn run_sandboxed_shell_command_for_test(
    command: &str,
    workspace_root: &Path,
    cwd: &Path,
    timeout_ms: u64,
    max_capture_bytes: usize,
) -> ProcessRunResult {
    run_shell_command_with_mode(
        command,
        workspace_root,
        cwd,
        timeout_ms,
        max_capture_bytes,
        true,
    )
    .await
}

async fn run_shell_command_with_mode(
    command: &str,
    workspace_root: &Path,
    cwd: &Path,
    timeout_ms: u64,
    max_capture_bytes: usize,
    force_sandbox: bool,
) -> ProcessRunResult {
    let started = Instant::now();
    let process =
        match spawn_shell_command_with_mode(command, workspace_root, cwd, force_sandbox).await {
            Ok(process) => process,
            Err(error) => {
                return ProcessRunResult {
                    stdout: String::new(),
                    stderr: format!("Failed to execute: {error}"),
                    success: false,
                    exit_code: Some(EXIT_CODE_INTERNAL_ERROR),
                    elapsed_ms: started.elapsed().as_millis() as u64,
                    timed_out: false,
                    stdout_truncated: false,
                    stderr_truncated: false,
                };
            }
        };

    #[cfg(target_os = "linux")]
    let scope = process.scope.clone();
    let result = run_spawned_process(process, started, timeout_ms, max_capture_bytes).await;

    #[cfg(target_os = "linux")]
    if let Some(retried) = retry_scoped_launch_without_scope(
        command,
        workspace_root,
        cwd,
        timeout_ms,
        max_capture_bytes,
        force_sandbox,
        scope,
        &result,
    )
    .await
    {
        return retried;
    }

    result
}

/// One retry without the scope after a launch-time bus failure.
///
/// The preflight's positive verdict (45 s TTL) can go stale: the bus dies
/// between the probe and the real `systemd-run --scope` launch, the client
/// never starts the command, and the whole request would fail with "Failed to
/// connect to bus" even though plain bubblewrap confinement is fully
/// available. Recording the failure first (permanent, like a negative
/// preflight) steers this retry spawn — and every later command — away from
/// the doomed scope, which also keeps the `CATDESK_SANDBOX_UNIT` marker
/// consistent: it comes from the same decision as the scope itself.
///
/// Only a leading bus-connect failure retries. A success has nothing to
/// recover from, a timeout ran the real command (a second execution could
/// repeat its side effects), and any other launch error is out of this
/// fallback's contract.
#[cfg(target_os = "linux")]
async fn retry_scoped_launch_without_scope(
    command: &str,
    workspace_root: &Path,
    cwd: &Path,
    timeout_ms: u64,
    max_capture_bytes: usize,
    force_sandbox: bool,
    scope: Option<PathBuf>,
    failed: &ProcessRunResult,
) -> Option<ProcessRunResult> {
    let scope = scope.filter(|_| {
        !failed.success
            && !failed.timed_out
            && crate::linux_sandbox::scope_launch_failed_with_bus_error(&failed.stderr)
    })?;
    crate::linux_sandbox::mark_scope_unusable(&scope);

    let process = spawn_shell_command_with_mode(command, workspace_root, cwd, force_sandbox)
        .await
        .ok()?;
    Some(run_spawned_process(process, Instant::now(), timeout_ms, max_capture_bytes).await)
}

/// Wait for an already-spawned process, capture its output and fold in the
/// timeout/signal diagnostics.
async fn run_spawned_process(
    mut process: SpawnedProcess,
    started: Instant,
    timeout_ms: u64,
    max_capture_bytes: usize,
) -> ProcessRunResult {
    let stdout_task = process
        .take_stdout()
        .map(|stdout| tokio::spawn(capture_reader(stdout, max_capture_bytes)));
    let stderr_task = process
        .take_stderr()
        .map(|stderr| tokio::spawn(capture_reader(stderr, max_capture_bytes)));

    let mut timed_out = false;
    let mut wait_error = None;
    let status = match timeout(Duration::from_millis(timeout_ms), process.wait()).await {
        Ok(Ok(status)) => Some(status),
        Ok(Err(error)) => {
            wait_error = Some(error.to_string());
            process.terminate_tree().await;
            process.wait().await.ok()
        }
        Err(_) => {
            timed_out = true;
            process.terminate_tree().await;
            process.wait().await.ok()
        }
    };
    process.disarm().await;

    let stdout_capture = finish_capture(stdout_task, "stdout").await;
    let stderr_capture = finish_capture(stderr_task, "stderr").await;
    let stdout = stdout_capture.text;
    let mut stderr = stderr_capture.text;

    if let Some(error) = wait_error.as_deref() {
        append_stderr_diagnostic(
            &mut stderr,
            &format!("Failed while waiting for command: {error}"),
        );
    }
    if let Some(error) = stdout_capture.read_error.as_deref() {
        append_stderr_diagnostic(
            &mut stderr,
            &format!("CatDesk failed to read stdout: {error}"),
        );
    }
    if let Some(error) = stderr_capture.read_error.as_deref() {
        append_stderr_diagnostic(
            &mut stderr,
            &format!("CatDesk failed to read stderr: {error}"),
        );
    }
    if timed_out {
        append_stderr_diagnostic(
            &mut stderr,
            &format!("Command timed out after {timeout_ms} ms"),
        );
    }
    if !timed_out
        && wait_error.is_none()
        && let Some(status) = status.as_ref()
        && let Some(signal) = exit_status_signal_diagnostic(status)
    {
        append_stderr_diagnostic(&mut stderr, &signal);
    }

    let exit_code = if wait_error.is_some() {
        EXIT_CODE_INTERNAL_ERROR
    } else if timed_out {
        EXIT_CODE_TIMEOUT
    } else {
        status
            .as_ref()
            .map(exit_code_for_status)
            .unwrap_or(EXIT_CODE_INTERNAL_ERROR)
    };
    let success = wait_error.is_none()
        && !timed_out
        && status
            .as_ref()
            .is_some_and(std::process::ExitStatus::success);

    ProcessRunResult {
        stdout,
        stderr,
        success,
        exit_code: Some(exit_code),
        elapsed_ms: started.elapsed().as_millis() as u64,
        timed_out,
        stdout_truncated: stdout_capture.truncated,
        stderr_truncated: stderr_capture.truncated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::ReadBuf;
    use uuid::Uuid;

    struct PartialThenError {
        emitted: bool,
    }

    impl AsyncRead for PartialThenError {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            if !self.emitted {
                self.emitted = true;
                buf.put_slice(b"partial-output");
                Poll::Ready(Ok(()))
            } else {
                Poll::Ready(Err(io::Error::other("synthetic read failure")))
            }
        }
    }

    fn workspace(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("catdesk-process-{name}-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create test workspace");
        path
    }

    #[tokio::test]
    async fn capture_reader_preserves_partial_output_on_read_error() {
        let captured = capture_reader(PartialThenError { emitted: false }, 1024).await;
        assert_eq!(captured.text, "partial-output");
        assert!(!captured.truncated);
        assert_eq!(
            captured.read_error.as_deref(),
            Some("synthetic read failure")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_sandboxed_process_survives_blocking_pool_thread_retirement() {
        let _env = crate::test_serialization::lock_env();
        let root = workspace("sandbox-parent-lifetime");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(1)
            .thread_keep_alive(Duration::from_millis(100))
            .enable_all()
            .build()
            .expect("build isolated runtime");

        runtime.block_on(async {
            let mut process = spawn_linux_sandboxed_for_test("sleep 2", &root, &root)
                .await
                .expect("spawn sandboxed command");
            let mut stderr = process.take_stderr().expect("sandbox stderr");
            let premature = timeout(Duration::from_millis(350), process.wait()).await;
            if premature.is_ok() {
                let mut diagnostic = String::new();
                stderr
                    .read_to_string(&mut diagnostic)
                    .await
                    .expect("read sandbox stderr");
                panic!(
                    "bwrap died when the transient blocking-pool thread retired: {premature:?}; stderr={diagnostic:?}"
                );
            }
            process.terminate_tree().await;
            let _ = process.wait().await;
        });

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn spawn_shell_command_denies_wsl_shutdown_before_spawn() {
        let root = workspace("deny-vm-bounce");
        let error =
            match spawn_shell_command("/mnt/c/Windows/System32/wsl.exe --shutdown", &root, &root)
                .await
            {
                Ok(_) => panic!("wsl --shutdown must be denied before spawn"),
                Err(error) => error,
            };
        assert!(
            error.to_string().contains("DENIED (operator-only)"),
            "unexpected error: {error}"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn run_shell_command_captures_output_and_exit_status() {
        let _env = crate::test_serialization::lock_env();
        let root = workspace("success");
        let command = if cfg!(windows) {
            "Write-Output 'hello'"
        } else {
            "printf 'hello\\n'"
        };
        let result = run_shell_command(command, &root, &root, 5_000, 1024).await;
        assert!(result.success, "stderr: {}", result.stderr);
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.stdout.trim(), "hello");
        assert!(!result.timed_out);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn large_stdout_and_stderr_are_drained_without_deadlock_and_bounded() {
        let _env = crate::test_serialization::lock_env();
        let root = workspace("bounded-output");
        let command = if cfg!(windows) {
            "[Console]::Out.Write(('x' * 200000)); [Console]::Error.Write(('y' * 200000))"
        } else {
            "printf '%*s' 200000 ''; printf '%*s' 200000 '' >&2"
        };
        let result = run_shell_command(command, &root, &root, 5_000, 4_096).await;
        assert!(
            result.success,
            "large-output command failed: {}",
            result.stderr
        );
        assert!(result.stdout.len() <= 4_096);
        assert!(result.stderr.len() <= 4_096);
        assert!(result.stdout_truncated);
        assert!(result.stderr_truncated);
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn signalled_command_reports_signal_in_stderr() {
        let _env = crate::test_serialization::lock_env();
        let root = workspace("signal-diagnostic");
        let result = run_shell_command("kill -KILL $$", &root, &root, 5_000, 1024).await;
        assert!(!result.success);
        assert_eq!(result.exit_code, Some(137));
        assert!(
            result.stderr.contains("SIGKILL") || result.stderr.contains("signal 9"),
            "missing signal diagnostic: {:?}",
            result.stderr
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn timed_out_command_cannot_continue_after_return() {
        let _env = crate::test_serialization::lock_env();
        let root = workspace("timeout");
        let sentinel = root.join("sentinel.txt");
        let command = if cfg!(windows) {
            "Start-Sleep -Milliseconds 700; Set-Content -Path sentinel.txt -Value survived"
        } else {
            "sleep 0.7; printf survived > sentinel.txt"
        };
        let result = run_shell_command(command, &root, &root, 100, 1024).await;
        assert!(result.timed_out);
        assert_eq!(result.exit_code, Some(EXIT_CODE_TIMEOUT));
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert!(
            !sentinel.exists(),
            "timed-out process survived and wrote sentinel"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn timeout_terminates_descendant_process_tree() {
        let _env = crate::test_serialization::lock_env();
        let root = workspace("descendant-timeout");
        let sentinel = root.join("descendant.txt");
        let command = if cfg!(windows) {
            "Start-Process powershell.exe -ArgumentList '-NoProfile','-Command','Start-Sleep -Milliseconds 800; Set-Content -Path descendant.txt -Value survived' -WorkingDirectory .; Start-Sleep -Seconds 5"
        } else {
            "(sleep 0.8; printf survived > descendant.txt) & sleep 5"
        };
        let result = run_shell_command(command, &root, &root, 150, 1024).await;
        assert!(result.timed_out);
        tokio::time::sleep(Duration::from_millis(1_000)).await;
        assert!(
            !sentinel.exists(),
            "timed-out root shell left a descendant process alive"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn successful_root_exit_cannot_leave_detached_descendant_alive() {
        let _env = crate::test_serialization::lock_env();
        let root = workspace("detached-success");
        let sentinel = root.join("detached.txt");
        let command = if cfg!(windows) {
            "Start-Process powershell.exe -ArgumentList '-NoProfile','-Command','Start-Sleep -Milliseconds 800; Set-Content -Path detached.txt -Value survived' -WorkingDirectory .; Write-Output root-done"
        } else {
            "(sleep 0.8; printf survived > detached.txt) & printf 'root-done\\n'"
        };
        let result = run_shell_command(command, &root, &root, 5_000, 1024).await;
        assert!(result.success, "root command failed: {}", result.stderr);
        assert!(result.stdout.contains("root-done"));
        tokio::time::sleep(Duration::from_millis(1_000)).await;
        assert!(
            !sentinel.exists(),
            "successful root shell detached a descendant outside CatDesk ownership"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn dropping_run_future_terminates_the_process() {
        let _env = crate::test_serialization::lock_env();
        let root = workspace("drop");
        let sentinel = root.join("sentinel.txt");
        let command = if cfg!(windows) {
            "Start-Sleep -Milliseconds 700; Set-Content -Path sentinel.txt -Value survived"
        } else {
            "sleep 0.7; printf survived > sentinel.txt"
        };
        let root_for_task = root.clone();
        let task = tokio::spawn(async move {
            run_shell_command(command, &root_for_task, &root_for_task, 5_000, 1024).await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        task.abort();
        let _ = task.await;
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert!(
            !sentinel.exists(),
            "dropped command future left the process alive"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// Env overrides serialized by the crate-wide env lock and restored on
    /// drop (including on panic), mirroring `EnvGuards` in the linux_sandbox
    /// tests. Additional `set_str` calls inside one lock scope push their own
    /// restore entries; drop replays everything in reverse.
    #[cfg(target_os = "linux")]
    struct TestEnv {
        prev: Vec<(&'static str, Option<std::ffi::OsString>)>,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    #[cfg(target_os = "linux")]
    impl TestEnv {
        fn set(pairs: &[(&'static str, &std::path::Path)]) -> Self {
            let guard = crate::test_serialization::lock_env();
            let prev = pairs
                .iter()
                .map(|(name, _)| (*name, std::env::var_os(name)))
                .collect();
            for (name, value) in pairs {
                // SAFETY: the crate env lock is held for the struct's whole
                // lifetime, so no other test thread reads or writes the env
                // concurrently; drop restores while still holding the lock.
                unsafe { std::env::set_var(name, value) };
            }
            Self {
                prev,
                _guard: guard,
            }
        }

        fn set_str(&mut self, name: &'static str, value: &str) {
            self.prev.push((name, std::env::var_os(name)));
            // SAFETY: same lock discipline as `set`.
            unsafe { std::env::set_var(name, value) };
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for TestEnv {
        fn drop(&mut self) {
            for (name, value) in self.prev.iter().rev() {
                // SAFETY: same lock discipline as `set`.
                match value {
                    Some(value) => unsafe { std::env::set_var(name, value) },
                    None => unsafe { std::env::remove_var(name) },
                }
            }
        }
    }

    /// A `systemd-run` stub whose preflight probe (`--user --scope --quiet
    /// true`, exactly four arguments ending in "true") succeeds while any real
    /// launch writes `launch_stderr` to stderr, appends a line to `marker` and
    /// exits nonzero — the dead-bus-between-probe-and-launch shape, without a
    /// wall clock in sight.
    #[cfg(target_os = "linux")]
    fn write_systemd_run_stub(bin: &Path, launch_stderr: &str, marker: &Path) {
        use std::os::unix::fs::PermissionsExt;

        let stub = bin.join("systemd-run");
        let script = format!(
            "#!/bin/sh\n\
             if [ \"$#\" -eq 4 ] && [ \"$4\" = \"true\" ]; then\n\
             \x20   exit 0\n\
             fi\n\
             echo '{launch_stderr}' >&2\n\
             echo launch >> '{}'\n\
             exit 1\n",
            marker.display()
        );
        std::fs::write(&stub, script).expect("write systemd-run stub");
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
            .expect("chmod systemd-run stub");
    }

    /// Whether this host can run the sandbox at all: bwrap plus unprivileged
    /// user namespaces can be absent or blocked (container seccomp), and the
    /// scoped-launch tests must skip instead of failing there.
    #[cfg(target_os = "linux")]
    fn host_runs_plain_sandbox() -> bool {
        std::process::Command::new("bwrap")
            .arg("--ro-bind")
            .arg("/")
            .arg("/")
            .arg("true")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    /// PATH with the stub bin PREPENDED to the real one. Tests running in
    /// parallel spawn `python3`/`sleep` through PATH without taking the env
    /// lock, so the override must stay fully functional — the stub only needs
    /// to win the `systemd-run` resolution, not own the whole PATH.
    #[cfg(target_os = "linux")]
    fn path_with_stub_first(bin: &Path) -> PathBuf {
        std::env::join_paths(
            std::iter::once(bin.to_path_buf()).chain(std::env::split_paths(
                &std::env::var_os("PATH").unwrap_or_default(),
            )),
        )
        .expect("join stub PATH")
        .into()
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn scoped_launch_bus_failure_retries_once_without_scope() {
        if !host_runs_plain_sandbox() {
            return;
        }

        // The stub lives OUTSIDE the command's workspace: workspace-local
        // `systemd-run` candidates are rejected by design (hijack guard), so a
        // stub under the workspace would silently yield no scope at all.
        let stubs = workspace("scope-stubs");
        let bin = stubs.join("bin");
        std::fs::create_dir_all(&bin).expect("create stub bin");
        let marker = stubs.join("scope-launches.log");
        write_systemd_run_stub(&bin, "Failed to connect to bus: No medium found", &marker);

        let root = workspace("scope-retry");

        let stub_path = path_with_stub_first(&bin);
        let mut env = TestEnv::set(&[("PATH", &stub_path)]);
        env.set_str("CATDESK_SANDBOX_MEMORY", "off");
        // Probe with the scope disabled: hosts that cannot sandbox at all skip
        // rather than fail, and the probe must not touch the stub (which would
        // already trip the retry under test).
        let probe = run_sandboxed_shell_command_for_test("true", &root, &root, 10_000, 1024).await;
        if !probe.success {
            let _ = std::fs::remove_dir_all(&root);
            let _ = std::fs::remove_dir_all(&stubs);
            return;
        }
        env.set_str("CATDESK_SANDBOX_MEMORY", "on");

        let result =
            run_sandboxed_shell_command_for_test("printf ok", &root, &root, 15_000, 1024).await;
        drop(env);

        assert!(
            result.success,
            "the bus failure must degrade to a successful plain-bwrap run; stderr: {}",
            result.stderr
        );
        assert_eq!(result.stdout.trim(), "ok");
        assert_eq!(
            result.exit_code,
            Some(0),
            "the retried run must report its own exit status, not the failed launch's"
        );
        let launches = std::fs::read_to_string(&marker).unwrap_or_default();
        assert_eq!(
            launches.lines().count(),
            1,
            "exactly one scoped launch, then the scope-less retry"
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&stubs);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn scoped_launch_non_bus_failure_does_not_retry() {
        if !host_runs_plain_sandbox() {
            return;
        }

        let stubs = workspace("scope-stubs");
        let bin = stubs.join("bin");
        std::fs::create_dir_all(&bin).expect("create stub bin");
        let marker = stubs.join("scope-launches.log");
        write_systemd_run_stub(
            &bin,
            "Failed to start transient service unit: Unit catdesk-sb-x.service is masked.",
            &marker,
        );

        let root = workspace("scope-no-retry");

        let stub_path = path_with_stub_first(&bin);
        let mut env = TestEnv::set(&[("PATH", &stub_path)]);
        env.set_str("CATDESK_SANDBOX_MEMORY", "on");

        let result =
            run_sandboxed_shell_command_for_test("printf ok", &root, &root, 15_000, 1024).await;
        drop(env);

        assert!(
            !result.success,
            "non-bus launch errors stay failed: the fallback is scoped to the bus race"
        );
        assert!(
            result
                .stderr
                .contains("Failed to start transient service unit"),
            "the original failure must surface unchanged; stderr: {}",
            result.stderr
        );
        let launches = std::fs::read_to_string(&marker).unwrap_or_default();
        assert_eq!(
            launches.lines().count(),
            1,
            "a non-bus error must not trigger the scope-less retry"
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&stubs);
    }
}
