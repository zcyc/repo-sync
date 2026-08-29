use crate::config;
use std::{
    cell::RefCell,
    collections::BTreeMap,
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Output, Stdio},
    sync::{Mutex, OnceLock},
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
    let mut child = command(dir, args).spawn()?;
    wait_for_exit(&mut child, timeout)
}

pub(crate) fn remote_exists(dir: &Path, name: &str) -> io::Result<bool> {
    let output = Command::new("git")
        .current_dir(dir)
        .args(["remote"])
        .output()?;
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
    command
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(args)
        .stdin(Stdio::null());
    command
}

fn wait_for_exit(child: &mut Child, timeout: Duration) -> io::Result<ExitStatus> {
    let started = Instant::now();
    loop {
        if cancellation_requested() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(io::ErrorKind::Interrupted, "sync cancelled"));
        }
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if started.elapsed() >= timeout {
            // ponytail: kill only covers git; use process groups if descendants leak.
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("git command timed out after {}s", timeout.as_secs()),
            ));
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn output_once(dir: &Path, args: &[&str], timeout: Duration) -> io::Result<Output> {
    let mut command = command(dir, args);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("git stdout was not piped"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("git stderr was not piped"))?;
    let stdout_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let stderr_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).map(|_| bytes)
    });
    let status = wait_for_exit(&mut child, timeout);
    let stdout = stdout_reader
        .join()
        .map_err(|_| io::Error::other("git stdout reader panicked"))??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| io::Error::other("git stderr reader panicked"))??;
    let status = status?;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
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
    use super::{cancel_workspace, cancellation_requested, CancellationScope};
    use std::path::Path;

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
}
