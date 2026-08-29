use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    error::Error,
    fs, io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

pub fn get_config_vec(path: &str) -> Result<Vec<Item>, Box<dyn Error>> {
    parse_config(&fs::read_to_string(path)?)
}

#[derive(Debug, Deserialize)]
struct ConfigFile {
    sync: Vec<Item>,
}

fn parse_config(content: &str) -> Result<Vec<Item>, Box<dyn Error>> {
    let config: ConfigFile = toml::from_str(content)?;
    Ok(config.sync)
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Item {
    pub source: String,
    pub target: Vec<String>,
    pub workspace: String,
    pub mode: SyncMode,
    pub crontab: Option<String>,
    pub branch: Option<String>,
    pub timeout_secs: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum SyncMode {
    Branch,
    Mirror,
}

pub fn validate_config(config: &[Item]) -> Result<(), Box<dyn Error>> {
    if config.is_empty() {
        return Err("configuration must contain at least one item".into());
    }

    let mut workspaces = HashSet::new();
    for item in config {
        validate_item(item)?;
        if !workspaces.insert(PathBuf::from(&item.workspace)) {
            return Err(format!("workspace is duplicated: {}", item.workspace).into());
        }
    }
    Ok(())
}

pub fn sync(config: &Item) -> Result<(), Box<dyn Error>> {
    validate_item(config)?;
    let repo_dir = Path::new(&config.workspace);
    let timeout = Duration::from_secs(config.timeout_secs);

    sync_source(config, repo_dir, timeout)?;

    let mut errors = Vec::new();
    for (index, target) in config.target.iter().enumerate() {
        let remote = format!("target{index}");
        if let Err(error) = sync_target(repo_dir, &remote, target, config, timeout) {
            eprintln!("sync {remote} failed: {error}");
            errors.push(format!("{remote}: {error}"));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!("{} target(s) failed: {}", errors.len(), errors.join("; ")).into())
    }
}

fn validate_item(config: &Item) -> Result<(), Box<dyn Error>> {
    if config.source.trim().is_empty() {
        return Err("source cannot be empty".into());
    }
    if config.target.is_empty() {
        return Err("at least one target is required".into());
    }

    let mut targets = HashSet::new();
    for (index, target) in config.target.iter().enumerate() {
        if target.trim().is_empty() {
            return Err(format!("target {index} cannot be empty").into());
        }
        if !targets.insert(target) {
            return Err(format!("target {index} is duplicated").into());
        }
    }

    let workspace = Path::new(&config.workspace);
    if config.workspace.trim().is_empty()
        || workspace == Path::new(".")
        || workspace == Path::new("..")
    {
        return Err("workspace must be a non-current directory".into());
    }
    if config.timeout_secs == 0 {
        return Err("timeout_secs must be greater than zero".into());
    }

    match config.mode {
        SyncMode::Branch if config.branch.is_none() => Err("branch mode requires branch".into()),
        SyncMode::Mirror if config.branch.is_some() => Err("mirror mode cannot set branch".into()),
        _ if config
            .branch
            .as_deref()
            .is_some_and(|branch| branch.trim().is_empty()) =>
        {
            Err("branch cannot be empty".into())
        }
        _ => Ok(()),
    }
}

fn sync_source(config: &Item, repo_dir: &Path, timeout: Duration) -> io::Result<()> {
    let exists = match config.mode {
        SyncMode::Branch => repo_dir.join(".git").exists(),
        SyncMode::Mirror => repo_dir.join("HEAD").is_file() && repo_dir.join("objects").is_dir(),
    };

    if !exists {
        let clone_args = match config.mode {
            SyncMode::Branch => vec![
                "clone",
                "--branch",
                config.branch.as_deref().unwrap(),
                config.source.as_str(),
                config.workspace.as_str(),
            ],
            SyncMode::Mirror => vec![
                "clone",
                "--mirror",
                config.source.as_str(),
                config.workspace.as_str(),
            ],
        };
        return run_git(Path::new("."), &clone_args, timeout);
    }

    run_git(
        repo_dir,
        &["remote", "set-url", "origin", &config.source],
        timeout,
    )?;
    match config.mode {
        SyncMode::Branch => run_git(
            repo_dir,
            &[
                "pull",
                "--ff-only",
                "origin",
                config.branch.as_deref().unwrap(),
            ],
            timeout,
        ),
        SyncMode::Mirror => run_git(repo_dir, &["fetch", "--prune", "origin"], timeout),
    }
}

fn sync_target(
    repo_dir: &Path,
    remote: &str,
    target: &str,
    config: &Item,
    timeout: Duration,
) -> io::Result<()> {
    if remote_exists(repo_dir, remote)? {
        run_git(repo_dir, &["remote", "set-url", remote, target], timeout)?;
    } else {
        run_git(repo_dir, &["remote", "add", remote, target], timeout)?;
    }

    match config.mode {
        SyncMode::Branch => run_git(
            repo_dir,
            &["push", remote, config.branch.as_deref().unwrap()],
            timeout,
        ),
        SyncMode::Mirror => run_git(repo_dir, &["push", "--mirror", remote], timeout),
    }
}

fn run_git(dir: &Path, args: &[&str], timeout: Duration) -> io::Result<()> {
    let mut child = Command::new("git")
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(args)
        .stdin(Stdio::null())
        .spawn()?;
    let started = Instant::now();

    loop {
        if let Some(status) = child.try_wait()? {
            if status.success() {
                return Ok(());
            }
            let operation = args.first().copied().unwrap_or("command");
            return Err(io::Error::other(format!(
                "git {operation} failed with {status}"
            )));
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

fn remote_exists(dir: &Path, name: &str) -> io::Result<bool> {
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

#[cfg(test)]
mod tests {
    use super::{parse_config, validate_config};

    #[test]
    fn parses_commented_toml_and_validates_it() {
        let mut config = parse_config(
            r#"
            # A human-readable sync item.
            [[sync]]
            source = "source"
            target = ["target"]
            workspace = "./work"
            mode = "branch"
            branch = "main"
            timeout_secs = 300
            "#,
        )
        .unwrap();
        assert!(validate_config(&config).is_ok());

        let mut invalid = config.remove(0);
        invalid.branch = None;
        assert!(validate_config(&[invalid]).is_err());
    }
}
