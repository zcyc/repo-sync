use std::{
    io::{self, Read},
    path::Path,
    process::{Child, Command, ExitStatus, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

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
    // ponytail: retries all Git failures; classify transient transport errors only if needed.
    for attempt in 0..=retry.max_retries {
        let result = status(dir, args, timeout);
        let error = match result {
            Ok(status) if status.success() => return Ok(()),
            Ok(status) => {
                let operation = args.first().copied().unwrap_or("command");
                io::Error::other(format!("git {operation} failed with {status}"))
            }
            Err(error) => error,
        };
        if attempt == retry.max_retries {
            return Err(error);
        }
        eprintln!(
            "git {} failed (attempt {}/{}): {error}; retrying in {}s",
            args.first().copied().unwrap_or("command"),
            attempt + 1,
            retry.max_retries + 1,
            retry_delay(retry, attempt).as_secs()
        );
        thread::sleep(retry_delay(retry, attempt));
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
            Ok(output) if attempt == retry.max_retries => return Ok(output),
            Ok(output) => eprintln!(
                "git {} failed (attempt {}/{}): {}; retrying in {}s",
                args.first().copied().unwrap_or("command"),
                attempt + 1,
                retry.max_retries + 1,
                output.status,
                retry_delay(retry, attempt).as_secs()
            ),
            Err(error) if attempt == retry.max_retries => return Err(error),
            Err(error) => eprintln!(
                "git {} failed (attempt {}/{}): {error}; retrying in {}s",
                args.first().copied().unwrap_or("command"),
                attempt + 1,
                retry.max_retries + 1,
                retry_delay(retry, attempt).as_secs()
            ),
        }
        thread::sleep(retry_delay(retry, attempt));
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
    command.stdout(Stdio::piped());
    let mut child = command.spawn()?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("git stdout was not piped"))?;
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let status = wait_for_exit(&mut child, timeout);
    let stdout = reader
        .join()
        .map_err(|_| io::Error::other("git output reader panicked"))??;
    let status = status?;
    Ok(Output {
        status,
        stdout,
        stderr: Vec::new(),
    })
}

fn retry_delay(retry: RetryPolicy, attempt: u32) -> Duration {
    let multiplier = 1u64.checked_shl(attempt.min(31)).unwrap_or(u64::MAX);
    Duration::from_secs(retry.backoff_secs.saturating_mul(multiplier))
}
