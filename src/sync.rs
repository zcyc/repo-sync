use crate::{
    config::{self, DivergencePolicy, Item, SyncMode, TagPolicy},
    git::{self, RetryPolicy},
    state::{self, StateFile},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process,
    time::{Duration, Instant},
};

pub fn sync(item: &Item) -> Result<(), Box<dyn Error>> {
    config::validate_item(item)?;
    let repo_dir = Path::new(&item.workspace);
    let timeout = Duration::from_secs(item.timeout_secs);
    let retry = RetryPolicy {
        max_retries: item.max_retries,
        backoff_secs: item.retry_backoff_secs,
    };
    let run_id = format!("{}-{}", state::now_ms(), process::id());
    let started = Instant::now();
    eprintln!("[{run_id}] sync started");
    let _lock = WorkspaceLock::acquire(repo_dir)?;
    let (state_path, mut sync_state) = state::load(repo_dir)?;
    if !sync_state.source.is_empty() && sync_state.source != item.source {
        return Err("state source does not match configuration source".into());
    }
    sync_state.source = item.source.clone();

    if let Err(error) = sync_source(item, repo_dir, timeout, retry) {
        mark_source_failure(&mut sync_state, &item.target, &error.to_string());
        let _ = state::save(&state_path, &sync_state);
        return Err(error.into());
    }
    if let Err(error) = sync_lfs_source(item, repo_dir, timeout, retry) {
        mark_source_failure(&mut sync_state, &item.target, &error.to_string());
        let _ = state::save(&state_path, &sync_state);
        return Err(error.into());
    }

    let source_refs = match item.mode {
        SyncMode::Branch => match source_refs(repo_dir, item, timeout, retry) {
            Ok(source_refs) => Some(source_refs),
            Err(error) => {
                mark_source_failure(&mut sync_state, &item.target, &error.to_string());
                let _ = state::save(&state_path, &sync_state);
                return Err(error.into());
            }
        },
        SyncMode::Mirror => None,
    };
    let mut pushed_targets = 0;
    let mut skipped_branches = 0;
    let mut skipped_tags = 0;
    let mut errors = Vec::new();

    for (index, target) in item.target.iter().enumerate() {
        let remote = format!("target{index}");
        let target_started = Instant::now();
        let target_state = sync_state.targets.entry(target.clone()).or_default();
        target_state.last_attempt_ms = state::now_ms();
        target_state.status = "running".into();
        target_state.last_error = None;
        state::save(&state_path, &sync_state)?;

        match sync_target(
            repo_dir,
            &remote,
            target,
            item,
            source_refs.as_ref(),
            timeout,
            retry,
        ) {
            Ok(outcome) => {
                pushed_targets += usize::from(outcome.pushed);
                skipped_branches += outcome.skipped_branches;
                skipped_tags += outcome.skipped_tags;
                let target_state = sync_state.targets.get_mut(target).unwrap();
                target_state.status = if outcome.pushed {
                    "synced".into()
                } else {
                    "skipped".into()
                };
                target_state.consecutive_failures = 0;
                target_state.last_success_ms = Some(state::now_ms());
                target_state.synced_refs = outcome.synced_refs;
                eprintln!(
                    "[{run_id}] {remote} complete: pushed={}, skipped_branches={}, skipped_tags={}, elapsed_ms={}",
                    outcome.pushed,
                    outcome.skipped_branches,
                    outcome.skipped_tags,
                    target_started.elapsed().as_millis()
                );
            }
            Err(error) => {
                let target_state = sync_state.targets.get_mut(target).unwrap();
                target_state.status = "failed".into();
                target_state.consecutive_failures =
                    target_state.consecutive_failures.saturating_add(1);
                target_state.last_error = Some(error.to_string());
                eprintln!(
                    "[{run_id}] {remote} failed after {}ms: {error}",
                    target_started.elapsed().as_millis()
                );
                errors.push(format!("{remote}: {error}"));
            }
        }
        state::save(&state_path, &sync_state)?;
    }

    eprintln!(
        "[{run_id}] sync summary: pushed_targets={pushed_targets}, skipped_branches={skipped_branches}, skipped_tags={skipped_tags}, failed_targets={}, elapsed_ms={}",
        errors.len(),
        started.elapsed().as_millis()
    );
    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!("{} target(s) failed: {}", errors.len(), errors.join("; ")).into())
    }
}

pub fn check(item: &Item) -> Result<(), Box<dyn Error>> {
    config::validate_item(item)?;
    let timeout = Duration::from_secs(item.timeout_secs);
    let retry = RetryPolicy {
        max_retries: item.max_retries,
        backoff_secs: item.retry_backoff_secs,
    };
    let current_dir = Path::new(".");
    let source_heads = remote_refs(
        current_dir,
        &["ls-remote", "--heads", item.source.as_str()],
        timeout,
        retry,
    )?;
    if matches!(item.mode, SyncMode::Branch)
        && !source_heads
            .keys()
            .any(|branch| config::branch_selected(&item.branches, branch))
    {
        return Err("no source branches match the configured branches".into());
    }
    if matches!(item.mode, SyncMode::Mirror) {
        git::run(
            current_dir,
            &["ls-remote", item.source.as_str()],
            timeout,
            retry,
        )?;
    }
    git::run(
        current_dir,
        &["ls-remote", "--tags", item.source.as_str()],
        timeout,
        retry,
    )?;
    if item.sync_lfs {
        git::run(current_dir, &["lfs", "env"], timeout, retry)?;
    }
    if Path::new(&item.workspace).exists() {
        validate_workspace(Path::new(&item.workspace), item, timeout)?;
    }
    for target in &item.target {
        git::run(current_dir, &["ls-remote", target], timeout, retry)?;
    }
    eprintln!("configuration and repository access checks passed");
    Ok(())
}

struct SourceRefs {
    branches: Vec<SourceBranch>,
    tags: Vec<SourceTag>,
}

struct SourceBranch {
    branch: String,
    source_ref: String,
    sha: String,
}

struct SourceTag {
    tag: String,
    source_ref: String,
    sha: String,
}

fn source_refs(
    repo_dir: &Path,
    item: &Item,
    timeout: Duration,
    retry: RetryPolicy,
) -> io::Result<SourceRefs> {
    let branch_output = git::output(
        repo_dir,
        &[
            "for-each-ref",
            "--format=%(refname:strip=3) %(objectname)",
            "refs/remotes/origin",
        ],
        timeout,
        retry,
    )?;
    if !branch_output.status.success() {
        return Err(io::Error::other(format!(
            "git for-each-ref failed with {}",
            branch_output.status
        )));
    }
    let mut branches = String::from_utf8_lossy(&branch_output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let branch = fields.next()?;
            let sha = fields.next()?;
            (branch != "HEAD" && config::branch_selected(&item.branches, branch)).then(|| {
                SourceBranch {
                    branch: branch.into(),
                    source_ref: format!("refs/remotes/origin/{branch}"),
                    sha: sha.into(),
                }
            })
        })
        .collect::<Vec<_>>();
    branches.sort_by(|left, right| left.branch.cmp(&right.branch));
    if branches.is_empty() {
        return Err(io::Error::other(
            "no source branches match the configured branches",
        ));
    }

    let tag_output = git::output(
        repo_dir,
        &[
            "for-each-ref",
            "--format=%(refname:strip=2) %(objectname)",
            "refs/tags",
        ],
        timeout,
        retry,
    )?;
    if !tag_output.status.success() {
        return Err(io::Error::other(format!(
            "git for-each-ref tags failed with {}",
            tag_output.status
        )));
    }
    let mut tags = String::from_utf8_lossy(&tag_output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let tag = fields.next()?;
            let sha = fields.next()?;
            Some(SourceTag {
                tag: tag.into(),
                source_ref: format!("refs/tags/{tag}"),
                sha: sha.into(),
            })
        })
        .collect::<Vec<_>>();
    tags.sort_by(|left, right| left.tag.cmp(&right.tag));
    Ok(SourceRefs { branches, tags })
}

fn sync_source(
    item: &Item,
    repo_dir: &Path,
    timeout: Duration,
    retry: RetryPolicy,
) -> io::Result<()> {
    let exists = match item.mode {
        SyncMode::Branch => repo_dir.join(".git").exists(),
        SyncMode::Mirror => repo_dir.join("HEAD").is_file() && repo_dir.join("objects").is_dir(),
    };
    if repo_dir.exists() && !exists {
        return Err(io::Error::other(
            "workspace exists but is not a repository for the configured mode",
        ));
    }
    if !exists {
        let args = match item.mode {
            SyncMode::Branch => vec![
                "clone",
                "--no-checkout",
                item.source.as_str(),
                item.workspace.as_str(),
            ],
            SyncMode::Mirror => vec![
                "clone",
                "--mirror",
                item.source.as_str(),
                item.workspace.as_str(),
            ],
        };
        return git::run(Path::new("."), &args, timeout, retry);
    }
    validate_workspace(repo_dir, item, timeout)?;
    git::run(
        repo_dir,
        &["fetch", "--prune", "--tags", "origin"],
        timeout,
        retry,
    )
}

fn sync_lfs_source(
    item: &Item,
    repo_dir: &Path,
    timeout: Duration,
    retry: RetryPolicy,
) -> io::Result<()> {
    if item.sync_lfs {
        git::run(
            repo_dir,
            &["lfs", "fetch", "--all", "origin"],
            timeout,
            retry,
        )?;
    }
    Ok(())
}

fn validate_workspace(repo_dir: &Path, item: &Item, timeout: Duration) -> io::Result<()> {
    let local_only = RetryPolicy {
        max_retries: 0,
        backoff_secs: 0,
    };
    let bare = git::output(
        repo_dir,
        &["rev-parse", "--is-bare-repository"],
        timeout,
        local_only,
    )?;
    if !bare.status.success() {
        return Err(io::Error::other("workspace is not a valid Git repository"));
    }
    let actual_bare = String::from_utf8_lossy(&bare.stdout).trim() == "true";
    if actual_bare != matches!(item.mode, SyncMode::Mirror) {
        return Err(io::Error::other(
            "workspace repository type does not match the configured mode",
        ));
    }
    let origin = git::output(
        repo_dir,
        &["remote", "get-url", "origin"],
        timeout,
        local_only,
    )?;
    if !origin.status.success() {
        return Err(io::Error::other("workspace must have an origin remote"));
    }
    if !git::same_repository(
        String::from_utf8_lossy(&origin.stdout).trim(),
        item.source.trim(),
    ) {
        return Err(io::Error::other(
            "workspace origin does not match source; use a new workspace for a new source",
        ));
    }
    Ok(())
}

struct TargetOutcome {
    pushed: bool,
    skipped_branches: usize,
    skipped_tags: usize,
    synced_refs: BTreeMap<String, String>,
}

struct PushPlan {
    refspecs: Vec<String>,
    leases: Vec<String>,
    lfs_refs: Vec<String>,
    synced_refs: BTreeMap<String, String>,
    skipped_branches: usize,
    skipped_tags: usize,
}

fn sync_target(
    repo_dir: &Path,
    remote: &str,
    target: &str,
    item: &Item,
    source: Option<&SourceRefs>,
    timeout: Duration,
    retry: RetryPolicy,
) -> io::Result<TargetOutcome> {
    if git::remote_exists(repo_dir, remote)? {
        git::run(
            repo_dir,
            &["remote", "set-url", remote, target],
            timeout,
            retry,
        )?;
    } else {
        git::run(repo_dir, &["remote", "add", remote, target], timeout, retry)?;
    }

    match item.mode {
        SyncMode::Branch => {
            let source = source.ok_or_else(|| io::Error::other("source refs are missing"))?;
            let mut plan = branch_plan(repo_dir, remote, item, source, timeout, retry)?;
            let target_tags = tag_refs(repo_dir, remote, timeout, retry)?;
            append_tag_plan(item, source, &target_tags, &mut plan)?;
            if plan.refspecs.is_empty() {
                return Ok(TargetOutcome {
                    pushed: false,
                    skipped_branches: plan.skipped_branches,
                    skipped_tags: plan.skipped_tags,
                    synced_refs: plan.synced_refs,
                });
            }
            if item.sync_lfs {
                push_lfs(repo_dir, remote, &plan.lfs_refs, item, timeout, retry)?;
            }
            push_plan(repo_dir, remote, item, &plan, timeout, retry)?;
            Ok(TargetOutcome {
                pushed: true,
                skipped_branches: plan.skipped_branches,
                skipped_tags: plan.skipped_tags,
                synced_refs: plan.synced_refs,
            })
        }
        SyncMode::Mirror => {
            git::run(repo_dir, &["ls-remote", remote], timeout, retry)?;
            let synced_refs = local_refs(repo_dir, timeout, retry)?;
            if item.sync_lfs {
                push_lfs(repo_dir, remote, &[], item, timeout, retry)?;
            }
            let mut args = vec!["push".to_owned()];
            if item.dry_run {
                args.push("--dry-run".into());
            }
            if item.atomic {
                args.push("--atomic".into());
            }
            args.push("--mirror".into());
            args.push(remote.into());
            let args = args.iter().map(String::as_str).collect::<Vec<_>>();
            git::run(repo_dir, &args, timeout, retry)?;
            Ok(TargetOutcome {
                pushed: true,
                skipped_branches: 0,
                skipped_tags: 0,
                synced_refs,
            })
        }
    }
}

fn branch_plan(
    repo_dir: &Path,
    remote: &str,
    item: &Item,
    source: &SourceRefs,
    timeout: Duration,
    retry: RetryPolicy,
) -> io::Result<PushPlan> {
    let target_branches = branch_refs(repo_dir, remote, timeout, retry)?;
    let source_names = source
        .branches
        .iter()
        .map(|branch| branch.branch.as_str())
        .collect::<BTreeSet<_>>();
    let mut plan = PushPlan {
        refspecs: Vec::new(),
        leases: Vec::new(),
        lfs_refs: Vec::new(),
        synced_refs: BTreeMap::new(),
        skipped_branches: 0,
        skipped_tags: 0,
    };
    for source_branch in &source.branches {
        let target_sha = target_branches.get(&source_branch.branch).cloned();
        let divergent = match target_sha.as_deref() {
            Some(target_sha) => branch_is_divergent(
                repo_dir,
                remote,
                &source_branch.branch,
                &source_branch.source_ref,
                target_sha,
                timeout,
                retry,
            )?,
            None => false,
        };
        if divergent && item.divergence == DivergencePolicy::Fail {
            return Err(io::Error::other(format!(
                "target branch is divergent: {}",
                source_branch.branch
            )));
        }
        if divergent && item.divergence == DivergencePolicy::Keep {
            plan.skipped_branches += 1;
            continue;
        }
        plan.refspecs.push(format!(
            "{}:refs/heads/{}",
            source_branch.source_ref, source_branch.branch
        ));
        plan.lfs_refs.push(source_branch.source_ref.clone());
        plan.synced_refs.insert(
            format!("refs/heads/{}", source_branch.branch),
            source_branch.sha.clone(),
        );
        if item.divergence == DivergencePolicy::Force {
            plan.leases.push(format!(
                "--force-with-lease=refs/heads/{}:{}",
                source_branch.branch,
                target_sha.as_deref().unwrap_or("")
            ));
        }
    }
    if item.prune_branches {
        for (branch, target_sha) in &target_branches {
            if source_names.contains(branch.as_str())
                || !config::branch_selected(&item.branches, branch)
            {
                continue;
            }
            plan.refspecs.push(format!(":refs/heads/{branch}"));
            plan.leases.push(format!(
                "--force-with-lease=refs/heads/{branch}:{target_sha}"
            ));
        }
    }
    Ok(plan)
}

fn append_tag_plan(
    item: &Item,
    source: &SourceRefs,
    target_tags: &BTreeMap<String, String>,
    plan: &mut PushPlan,
) -> io::Result<()> {
    let source_names = source
        .tags
        .iter()
        .map(|tag| tag.tag.as_str())
        .collect::<BTreeSet<_>>();
    for source_tag in &source.tags {
        match target_tags.get(&source_tag.tag) {
            None => {
                plan.refspecs.push(format!(
                    "{}:refs/tags/{}",
                    source_tag.source_ref, source_tag.tag
                ));
                plan.lfs_refs.push(source_tag.source_ref.clone());
                plan.synced_refs.insert(
                    format!("refs/tags/{}", source_tag.tag),
                    source_tag.sha.clone(),
                );
            }
            Some(target_sha) if target_sha == &source_tag.sha => {}
            Some(target_sha) => match item.tag_policy {
                TagPolicy::Preserve => plan.skipped_tags += 1,
                TagPolicy::Fail => {
                    return Err(io::Error::other(format!(
                        "target tag is divergent: {}",
                        source_tag.tag
                    )))
                }
                TagPolicy::Force => {
                    plan.refspecs.push(format!(
                        "{}:refs/tags/{}",
                        source_tag.source_ref, source_tag.tag
                    ));
                    plan.leases.push(format!(
                        "--force-with-lease=refs/tags/{}:{}",
                        source_tag.tag, target_sha
                    ));
                    plan.lfs_refs.push(source_tag.source_ref.clone());
                    plan.synced_refs.insert(
                        format!("refs/tags/{}", source_tag.tag),
                        source_tag.sha.clone(),
                    );
                }
            },
        }
    }
    if item.prune_tags {
        for (tag, target_sha) in target_tags {
            if source_names.contains(tag.as_str()) {
                continue;
            }
            plan.refspecs.push(format!(":refs/tags/{tag}"));
            plan.leases
                .push(format!("--force-with-lease=refs/tags/{tag}:{target_sha}"));
        }
    }
    Ok(())
}

fn push_plan(
    repo_dir: &Path,
    remote: &str,
    item: &Item,
    plan: &PushPlan,
    timeout: Duration,
    retry: RetryPolicy,
) -> io::Result<()> {
    let mut args = vec!["push".to_owned()];
    if item.dry_run {
        args.push("--dry-run".into());
    }
    if item.atomic {
        args.push("--atomic".into());
    }
    args.extend(plan.leases.iter().cloned());
    args.push(remote.into());
    args.extend(plan.refspecs.iter().cloned());
    let args = args.iter().map(String::as_str).collect::<Vec<_>>();
    git::run(repo_dir, &args, timeout, retry)
}

fn push_lfs(
    repo_dir: &Path,
    remote: &str,
    source_refs: &[String],
    item: &Item,
    timeout: Duration,
    retry: RetryPolicy,
) -> io::Result<()> {
    let mut args = vec!["lfs".to_owned(), "push".to_owned()];
    if item.dry_run {
        args.push("--dry-run".into());
    }
    args.push("--all".into());
    args.push(remote.into());
    args.extend(source_refs.iter().cloned());
    let args = args.iter().map(String::as_str).collect::<Vec<_>>();
    git::run(repo_dir, &args, timeout, retry)
}

fn branch_is_divergent(
    repo_dir: &Path,
    remote: &str,
    branch: &str,
    source_ref: &str,
    target_sha: &str,
    timeout: Duration,
    retry: RetryPolicy,
) -> io::Result<bool> {
    let object = format!("{target_sha}^{{commit}}");
    let target_ref = if git::status(
        repo_dir,
        &["rev-parse", "--verify", "--quiet", &object],
        timeout,
    )?
    .success()
    {
        target_sha.to_owned()
    } else {
        git::run(
            repo_dir,
            &["fetch", "--no-tags", remote, branch],
            timeout,
            retry,
        )?;
        format!("refs/remotes/{remote}/{branch}")
    };
    let ancestry = git::status(
        repo_dir,
        &["merge-base", "--is-ancestor", &target_ref, source_ref],
        timeout,
    )?;
    match ancestry.code() {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        _ => Err(io::Error::other(format!(
            "git merge-base failed with {ancestry}"
        ))),
    }
}

fn branch_refs(
    repo_dir: &Path,
    remote: &str,
    timeout: Duration,
    retry: RetryPolicy,
) -> io::Result<BTreeMap<String, String>> {
    remote_refs(repo_dir, &["ls-remote", "--heads", remote], timeout, retry)
}

fn tag_refs(
    repo_dir: &Path,
    remote: &str,
    timeout: Duration,
    retry: RetryPolicy,
) -> io::Result<BTreeMap<String, String>> {
    remote_refs(repo_dir, &["ls-remote", "--tags", remote], timeout, retry)
}

fn remote_refs(
    repo_dir: &Path,
    args: &[&str],
    timeout: Duration,
    retry: RetryPolicy,
) -> io::Result<BTreeMap<String, String>> {
    let output = git::output(repo_dir, args, timeout, retry)?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "git ls-remote failed with {}",
            output.status
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let sha = fields.next()?;
            let name = fields.next()?;
            if name.ends_with("^{}") {
                return None;
            }
            let name = name
                .strip_prefix("refs/heads/")
                .or_else(|| name.strip_prefix("refs/tags/"))?
                .to_owned();
            Some((name, sha.to_owned()))
        })
        .collect())
}

fn local_refs(
    repo_dir: &Path,
    timeout: Duration,
    retry: RetryPolicy,
) -> io::Result<BTreeMap<String, String>> {
    let output = git::output(
        repo_dir,
        &["for-each-ref", "--format=%(refname) %(objectname)", "refs"],
        timeout,
        retry,
    )?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "git for-each-ref failed with {}",
            output.status
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some((fields.next()?.to_owned(), fields.next()?.to_owned()))
        })
        .collect())
}

fn mark_source_failure(state: &mut StateFile, targets: &[String], error: &str) {
    for target in targets {
        let target_state = state.targets.entry(target.clone()).or_default();
        target_state.last_attempt_ms = state::now_ms();
        target_state.status = "failed".into();
        target_state.consecutive_failures = target_state.consecutive_failures.saturating_add(1);
        target_state.last_error = Some(error.into());
    }
}

struct WorkspaceLock {
    path: PathBuf,
}

impl WorkspaceLock {
    fn acquire(workspace: &Path) -> io::Result<Self> {
        let name = workspace.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "workspace has no file name")
        })?;
        let mut path = workspace.to_path_buf();
        path.set_file_name(format!("{}.lock", name.to_string_lossy()));
        let mut file = match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("workspace is locked: {}", path.display()),
                ));
            }
            Err(error) => return Err(error),
        };
        if let Err(error) = writeln!(file, "{}", process::id()) {
            let _ = fs::remove_file(&path);
            return Err(error);
        }
        Ok(Self { path })
    }
}

impl Drop for WorkspaceLock {
    fn drop(&mut self) {
        // ponytail: a crash leaves a stale lock; add PID liveness checks only if this needs recovery.
        let _ = fs::remove_file(&self.path);
    }
}
