use crate::config;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(windows)]
use std::os::windows::{io::AsRawHandle, process::CommandExt};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Output, Stdio},
    sync::{mpsc, Mutex, OnceLock},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

thread_local! {
    static CANCELLATION_SCOPE: RefCell<Option<ActiveCancellation>> = const { RefCell::new(None) };
}

static CANCELLATION_GENERATIONS: OnceLock<Mutex<BTreeMap<PathBuf, u64>>> = OnceLock::new();

struct ActiveCancellation {
    workspace: PathBuf,
    generation: u64,
}

pub(crate) struct CancellationScope;

impl CancellationScope {
    pub(crate) fn enter(workspace: &Path) -> Self {
        let workspace =
            config::workspace_identity(workspace).unwrap_or_else(|_| workspace.to_owned());
        let generation = CANCELLATION_GENERATIONS
            .get_or_init(|| Mutex::new(BTreeMap::new()))
            .lock()
            .expect("git cancellation lock poisoned")
            .get(&workspace)
            .copied()
            .unwrap_or_default();
        CANCELLATION_SCOPE.with(|scope| {
            *scope.borrow_mut() = Some(ActiveCancellation {
                workspace: workspace.to_owned(),
                generation,
            })
        });
        Self
    }
}

impl Drop for CancellationScope {
    fn drop(&mut self) {
        CANCELLATION_SCOPE.with(|scope| *scope.borrow_mut() = None);
    }
}

pub(crate) fn cancel_workspace(workspace: &Path) {
    let workspace = config::workspace_identity(workspace).unwrap_or_else(|_| workspace.to_owned());
    let mut generations = CANCELLATION_GENERATIONS
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .expect("git cancellation lock poisoned");
    let generation = generations.entry(workspace.to_owned()).or_default();
    *generation = generation.saturating_add(1);
}

pub(crate) fn cancellation_requested() -> bool {
    CANCELLATION_SCOPE.with(|scope| {
        scope.borrow().as_ref().is_some_and(|active| {
            CANCELLATION_GENERATIONS
                .get_or_init(|| Mutex::new(BTreeMap::new()))
                .lock()
                .expect("git cancellation lock poisoned")
                .get(&active.workspace)
                .copied()
                .unwrap_or_default()
                != active.generation
        })
    })
}

#[derive(Clone, Copy)]
pub(crate) struct RetryPolicy {
    pub(crate) max_retries: u32,
    pub(crate) backoff_secs: u64,
}

pub(crate) fn run(
    dir: &Path,
    args: &[&str],
    timeout: Duration,
    retry: RetryPolicy,
) -> io::Result<()> {
    for attempt in 0..=retry.max_retries {
        match output_once(dir, args, timeout) {
            Ok(output) if output.status.success() => return Ok(()),
            Ok(output) => {
                let error = command_error(args, &output);
                if attempt == retry.max_retries || !retryable_output(&output) {
                    return Err(io::Error::other(error));
                }
                retry_later(args, attempt, retry, &error)?;
            }
            Err(error) => {
                if attempt == retry.max_retries || !retryable_error(&error) {
                    return Err(error);
                }
                retry_later(args, attempt, retry, &error.to_string())?;
            }
        }
    }
    unreachable!()
}

pub(crate) fn output(
    dir: &Path,
    args: &[&str],
    timeout: Duration,
    retry: RetryPolicy,
) -> io::Result<Output> {
    for attempt in 0..=retry.max_retries {
        match output_once(dir, args, timeout) {
            Ok(output) if output.status.success() => return Ok(output),
            Ok(output) if attempt == retry.max_retries || !retryable_output(&output) => {
                return Ok(output)
            }
            Ok(output) => {
                let error = command_error(args, &output);
                retry_later(args, attempt, retry, &error)?;
            }
            Err(error) if attempt == retry.max_retries || !retryable_error(&error) => {
                return Err(error)
            }
            Err(error) => retry_later(args, attempt, retry, &error.to_string())?,
        }
    }
    unreachable!()
}

pub(crate) fn status(dir: &Path, args: &[&str], timeout: Duration) -> io::Result<ExitStatus> {
    let mut command = command(dir, args);
    let mut child = ManagedChild::spawn(&mut command)?;
    wait_for_exit(&mut child, timeout)
}

pub(crate) fn remote_exists(
    dir: &Path,
    name: &str,
    timeout: Duration,
    retry: RetryPolicy,
) -> io::Result<bool> {
    let output = output(dir, &["remote"], timeout, retry)?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "git remote failed with {}",
            output.status
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|remote| remote == name))
}

pub(crate) fn same_repository(actual: &str, expected: &str) -> bool {
    if actual == expected {
        return true;
    }
    match (
        Path::new(actual).canonicalize(),
        Path::new(expected).canonicalize(),
    ) {
        (Ok(actual), Ok(expected)) => actual == expected,
        _ => false,
    }
}

fn command(dir: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("git");
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    command.creation_flags(0x0000_0004);
    command
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(args)
        .stdin(Stdio::null());
    command
}

struct ManagedChild {
    child: Child,
    #[cfg(windows)]
    job: Option<WindowsJob>,
}

impl ManagedChild {
    fn spawn(command: &mut Command) -> io::Result<Self> {
        #[cfg(windows)]
        let mut child = command.spawn()?;
        #[cfg(not(windows))]
        let child = command.spawn()?;
        #[cfg(windows)]
        {
            let inherited_job = match is_process_in_job(&child) {
                Ok(inherited_job) => inherited_job,
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error);
                }
            };
            let job = match WindowsJob::attach(&child) {
                Ok(job) => Some(job),
                Err(_error) if inherited_job => None,
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error);
                }
            };
            if let Err(error) = resume_process(&child) {
                if let Some(job) = &job {
                    job.terminate();
                }
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
            return Ok(Self { child, job });
        }
        #[cfg(not(windows))]
        Ok(Self { child })
    }
}

#[cfg(windows)]
struct WindowsJob {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl WindowsJob {
    fn attach(child: &Child) -> io::Result<Self> {
        use std::{mem::size_of, ptr::null};
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };

        unsafe {
            let handle = CreateJobObjectW(null(), null());
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            let job = Self { handle };
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                job.handle,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            if AssignProcessToJobObject(job.handle, child.as_raw_handle()) == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(job)
        }
    }
}

#[cfg(windows)]
fn is_process_in_job(child: &Child) -> io::Result<bool> {
    use std::ptr::null_mut;
    use windows_sys::Win32::System::JobObjects::IsProcessInJob;

    unsafe {
        let mut in_job = 0;
        if IsProcessInJob(child.as_raw_handle(), null_mut(), &mut in_job) == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(in_job != 0)
    }
}

#[cfg(windows)]
fn resume_process(child: &Child) -> io::Result<()> {
    use std::mem::size_of;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};

    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let result = (|| {
            let mut entry = THREADENTRY32 {
                dwSize: size_of::<THREADENTRY32>() as u32,
                ..Default::default()
            };
            if Thread32First(snapshot, &mut entry) == 0 {
                return Err(io::Error::last_os_error());
            }
            loop {
                if entry.th32OwnerProcessID == child.id() {
                    let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                    if thread.is_null() {
                        return Err(io::Error::last_os_error());
                    }
                    let result = if ResumeThread(thread) == u32::MAX {
                        Err(io::Error::last_os_error())
                    } else {
                        Ok(())
                    };
                    let _ = CloseHandle(thread);
                    return result;
                }
                if Thread32Next(snapshot, &mut entry) == 0 {
                    break;
                }
            }
            Err(io::Error::other("git process main thread not found"))
        })();
        let _ = CloseHandle(snapshot);
        result
    }
}

#[cfg(windows)]
impl WindowsJob {
    fn terminate(&self) {
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;

        unsafe {
            let _ = TerminateJobObject(self.handle, 1);
        }
    }
}

#[cfg(windows)]
impl Drop for WindowsJob {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;

        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

fn wait_for_exit(child: &mut ManagedChild, timeout: Duration) -> io::Result<ExitStatus> {
    let started = Instant::now();
    loop {
        if cancellation_requested() {
            kill_process_group(child);
            return Err(io::Error::new(io::ErrorKind::Interrupted, "sync cancelled"));
        }
        if started.elapsed() >= timeout {
            kill_process_group(child);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("git command timed out after {}s", timeout.as_secs()),
            ));
        }
        if let Some(status) = child.child.try_wait()? {
            return Ok(status);
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn kill_process_group(child: &mut ManagedChild) {
    #[cfg(unix)]
    {
        let process_group = format!("-{}", child.child.id());
        let _ = Command::new("kill")
            .args(["-KILL", process_group.as_str()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(windows)]
    if let Some(job) = &child.job {
        job.terminate();
    } else {
        terminate_process_tree(child.child.id());
    }
    let _ = child.child.kill();
    let _ = child.child.wait();
}

#[cfg(windows)]
fn terminate_process_tree(pid: u32) {
    let pid = pid.to_string();
    let _ = Command::new("taskkill")
        .args(["/PID", pid.as_str(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn output_once(dir: &Path, args: &[&str], timeout: Duration) -> io::Result<Output> {
    let started = Instant::now();
    let mut command = command(dir, args);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = ManagedChild::spawn(&mut command)?;
    let mut stdout = child
        .child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("git stdout was not piped"))?;
    let mut stderr = child
        .child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("git stderr was not piped"))?;
    let (stdout_sender, stdout_receiver) = mpsc::channel();
    let stdout_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout_sender.send(stdout.read_to_end(&mut bytes).map(|_| bytes));
    });
    let (stderr_sender, stderr_receiver) = mpsc::channel();
    let stderr_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr_sender.send(stderr.read_to_end(&mut bytes).map(|_| bytes));
    });
    let status = wait_for_exit(&mut child, timeout);
    let status = status?;
    let stdout = receive_output(stdout_receiver, &mut child, started, timeout)?;
    let stderr = receive_output(stderr_receiver, &mut child, started, timeout)?;
    stdout_reader
        .join()
        .map_err(|_| io::Error::other("git stdout reader panicked"))?;
    stderr_reader
        .join()
        .map_err(|_| io::Error::other("git stderr reader panicked"))?;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn receive_output(
    receiver: mpsc::Receiver<io::Result<Vec<u8>>>,
    child: &mut ManagedChild,
    started: Instant,
    timeout: Duration,
) -> io::Result<Vec<u8>> {
    loop {
        if cancellation_requested() {
            kill_process_group(child);
            return Err(io::Error::new(io::ErrorKind::Interrupted, "sync cancelled"));
        }
        let remaining = timeout.checked_sub(started.elapsed()).unwrap_or_default();
        let wait = remaining.min(Duration::from_millis(100));
        match receiver.recv_timeout(wait) {
            Ok(output) => return output,
            Err(mpsc::RecvTimeoutError::Timeout) if started.elapsed() >= timeout => {
                kill_process_group(child);
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("git command timed out after {}s", timeout.as_secs()),
                ));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(io::Error::other("git output reader stopped unexpectedly"));
            }
        }
    }
}

fn command_error(args: &[&str], output: &Output) -> String {
    let operation = args.first().copied().unwrap_or("command");
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if detail.is_empty() {
        format!("git {operation} failed with {}", output.status)
    } else {
        format!("git {operation} failed with {}: {detail}", redact(&detail))
    }
}

fn retryable_output(output: &Output) -> bool {
    let detail = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    [
        "could not resolve host",
        "connection timed out",
        "connection reset",
        "connection refused",
        "network is unreachable",
        "temporary failure",
        "remote end hung up",
        "early eof",
        "unable to access",
        "502",
        "503",
        "504",
        "429",
    ]
    .iter()
    .any(|message| detail.contains(message))
}

fn retryable_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    )
}

fn retry_later(args: &[&str], attempt: u32, retry: RetryPolicy, error: &str) -> io::Result<()> {
    let delay = retry_delay(retry, attempt);
    eprintln!(
        "git {} failed (attempt {}/{}): {}; retrying in {}s",
        args.first().copied().unwrap_or("command"),
        attempt + 1,
        retry.max_retries + 1,
        error,
        delay.as_secs()
    );
    let started = Instant::now();
    while started.elapsed() < delay {
        if cancellation_requested() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "sync cancelled"));
        }
        thread::sleep((delay - started.elapsed()).min(Duration::from_millis(100)));
    }
    Ok(())
}

fn retry_delay(retry: RetryPolicy, attempt: u32) -> Duration {
    // ponytail: cap exponential retry delay at five minutes; make it configurable only if needed.
    let multiplier = 1u64.checked_shl(attempt.min(31)).unwrap_or(u64::MAX);
    let exponential = retry.backoff_secs.saturating_mul(multiplier).min(300);
    let jitter = if exponential == 0 {
        0
    } else {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos() as u64;
        nanos % (exponential / 4 + 1)
    };
    Duration::from_secs(exponential.saturating_add(jitter))
}

fn redact(value: &str) -> String {
    let mut output = value.to_owned();
    for scheme in ["http://", "https://"] {
        let mut search_from = 0;
        while let Some(relative) = output[search_from..].find(scheme) {
            let start = search_from + relative + scheme.len();
            let end = output[start..]
                .find(|character: char| {
                    character.is_whitespace() || character == '\'' || character == '"'
                })
                .map(|offset| start + offset)
                .unwrap_or(output.len());
            let Some(authority_end) = output[start..end].find('/') else {
                search_from = end;
                continue;
            };
            let authority_end = start + authority_end;
            let Some(at) = output[start..authority_end].rfind('@') else {
                search_from = end;
                continue;
            };
            let at = start + at;
            output.replace_range(start..at, "***");
            search_from = at + 3;
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::{
        cancel_workspace, cancellation_requested, receive_output, CancellationScope, ManagedChild,
    };
    #[cfg(unix)]
    use super::{output, RetryPolicy};
    use std::path::Path;
    #[cfg(unix)]
    use std::{
        fs,
        io::Read,
        os::unix::fs::PermissionsExt,
        os::unix::process::CommandExt,
        process::{Command, Stdio},
        sync::mpsc,
        thread,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn cancellation_is_scoped_to_the_active_workspace() {
        let workspace = Path::new("./git-cancel-test-workspace");
        {
            let _scope = CancellationScope::enter(workspace);
            assert!(!cancellation_requested());
            cancel_workspace(workspace);
            assert!(cancellation_requested());
        }
        let _scope = CancellationScope::enter(workspace);
        assert!(!cancellation_requested());
    }

    #[cfg(unix)]
    fn repository_with_background_hook() -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "repo-sync-git-timeout-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        assert!(Command::new("git")
            .current_dir(&root)
            .args(["init", "--quiet"])
            .status()
            .unwrap()
            .success());
        let hook = root.join(".git/hooks/pre-commit");
        fs::write(&hook, "#!/bin/sh\n(sleep 2) &\n").unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        root
    }

    #[cfg(unix)]
    #[test]
    fn output_timeout_does_not_wait_for_descendant_pipes() {
        let root = repository_with_background_hook();

        let started = Instant::now();
        let result = output(
            &root,
            &[
                "-c",
                "user.name=repo-sync-test",
                "-c",
                "user.email=repo-sync-test@example.test",
                "commit",
                "--allow-empty",
                "-m",
                "test",
            ],
            Duration::from_secs(1),
            RetryPolicy {
                max_retries: 0,
                backoff_secs: 0,
            },
        );
        let elapsed = started.elapsed();
        assert!(matches!(result, Err(error) if error.kind() == std::io::ErrorKind::TimedOut));
        assert!(
            elapsed < Duration::from_millis(1_500),
            "git output waited {elapsed:?} past its timeout"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_interrupts_descendant_pipe_wait() {
        let root = repository_with_background_hook();
        let mut command = Command::new("sh");
        command
            .process_group(0)
            .args(["-c", "(sleep 2) &"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = ManagedChild::spawn(&mut command).unwrap();
        let mut stdout = child.child.stdout.take().unwrap();
        let (sender, receiver) = mpsc::channel();
        let reader = thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = sender.send(stdout.read_to_end(&mut bytes).map(|_| bytes));
        });
        let _scope = CancellationScope::enter(&root);
        let cancel_root = root.clone();
        let canceller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            cancel_workspace(&cancel_root);
        });
        let result = receive_output(receiver, &mut child, Instant::now(), Duration::from_secs(5));
        canceller.join().unwrap();
        assert!(matches!(result, Err(error) if error.kind() == std::io::ErrorKind::Interrupted));
        let _ = reader.join();
        let _ = fs::remove_dir_all(root);
    }
}
