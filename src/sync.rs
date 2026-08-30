use crate::{
    config::{self, DivergencePolicy, Item, SyncMode, TagPolicy},
    git::{self, RetryPolicy},
    state::{self, RunSummary, StateDb},
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
    let _cancellation_scope = git::CancellationScope::enter(repo_dir);
    let timeout = Duration::from_secs(item.timeout_secs);
    let retry = RetryPolicy {
        max_retries: item.max_retries,
        backoff_secs: item.retry_backoff_secs,
    };
    let run_id = format!("{}-{}", state::now_ms(), process::id());
    let started = Instant::now();
    let started_ms = state::now_ms();
    eprintln!("[{run_id}] sync started");
    let _lock = WorkspaceLock::acquire(repo_dir)?;
    let mut state_db = StateDb::open(repo_dir, &item.source)?;
    state_db.begin_run(&run_id, &item.source, started_ms)?;

    if let Err(error) = sync_source(item, repo_dir, timeout, retry) {
        let message = error.to_string();
        let _ = state_db.mark_source_failure(&item.source, &item.target, &message);
        let _ = state_db.finish_run(
            &run_id,
            state::now_ms(),
            &RunSummary {
                status: if git::cancellation_requested() {
                    "cancelled"
                } else {
                    "failed"
                }
                .into(),
                pushed_targets: 0,
                skipped_branches: 0,
                skipped_tags: 0,
                failed_targets: item.target.len(),
                error: Some(message.clone()),
            },
        );
        return Err(error.into());
    }
    if let Err(error) = sync_lfs_source(item, repo_dir, timeout, retry) {
        let message = error.to_string();
        let _ = state_db.mark_source_failure(&item.source, &item.target, &message);
        let _ = state_db.finish_run(
            &run_id,
            state::now_ms(),
            &RunSummary {
                status: if git::cancellation_requested() {
                    "cancelled"
                } else {
                    "failed"
                }
                .into(),
                pushed_targets: 0,
                skipped_branches: 0,
                skipped_tags: 0,
                failed_targets: item.target.len(),
                error: Some(message.clone()),
            },
        );
        return Err(error.into());
    }

    let source_refs = match item.mode {
        SyncMode::Branch => match source_refs(repo_dir, item, timeout, retry) {
            Ok(source_refs) => Some(source_refs),
            Err(error) => {
                let message = error.to_string();
                let _ = state_db.mark_source_failure(&item.source, &item.target, &message);
                let _ = state_db.finish_run(
                    &run_id,
                    state::now_ms(),
                    &RunSummary {
                        status: if git::cancellation_requested() {
                            "cancelled"
                        } else {
                            "failed"
                        }
                        .into(),
                        pushed_targets: 0,
                        skipped_branches: 0,
                        skipped_tags: 0,
                        failed_targets: item.target.len(),
                        error: Some(message.clone()),
                    },
                );
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
        state_db.mark_running(&item.source, target, state::now_ms())?;

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
                if item.dry_run {
                    state_db.mark_dry_run(
                        &item.source,
                        target,
                        target_started.elapsed().as_millis() as i64,
                    )?;
                } else {
                    state_db.mark_success(
                        &item.source,
                        target,
                        if outcome.pushed { "synced" } else { "skipped" },
                        target_started.elapsed().as_millis() as i64,
                        &outcome.synced_refs,
                        outcome.pushed,
                    )?;
                }
                eprintln!(
                    "[{run_id}] {remote} complete: pushed={}, skipped_branches={}, skipped_tags={}, elapsed_ms={}",
                    outcome.pushed,
                    outcome.skipped_branches,
                    outcome.skipped_tags,
                    target_started.elapsed().as_millis()
                );
            }
            Err(error) => {
                let message = error.to_string();
                state_db.mark_failure(
                    &item.source,
                    target,
                    target_started.elapsed().as_millis() as i64,
                    &message,
                )?;
                eprintln!(
                    "[{run_id}] {remote} failed after {}ms: {error}",
                    target_started.elapsed().as_millis()
                );
                errors.push(format!("{remote}: {error}"));
                if git::cancellation_requested() {
                    break;
                }
            }
        }
    }

    eprintln!(
        "[{run_id}] sync summary: pushed_targets={pushed_targets}, skipped_branches={skipped_branches}, skipped_tags={skipped_tags}, failed_targets={}, elapsed_ms={}",
        errors.len(),
        started.elapsed().as_millis()
    );
    let cancelled = git::cancellation_requested();
    let run_error = if cancelled {
        Some("sync cancelled".to_owned())
    } else {
        (!errors.is_empty()).then(|| errors.join("; "))
    };
    state_db.finish_run(
        &run_id,
        state::now_ms(),
        &RunSummary {
            status: if cancelled {
                "cancelled".into()
            } else if run_error.is_some() {
                "failed".into()
            } else {
                "succeeded".into()
            },
            pushed_targets,
            skipped_branches,
            skipped_tags,
            failed_targets: errors.len(),
            error: run_error.clone(),
        },
    )?;
    if cancelled {
        Err("sync cancelled".into())
    } else if let Some(error) = run_error {
        Err(format!("{} target(s) failed: {error}", errors.len()).into())
    } else {
        Ok(())
    }
}

pub fn check(item: &Item, check_write: bool) -> Result<(), Box<dyn Error>> {
    config::validate_item(item)?;
    let timeout = Duration::from_secs(item.timeout_secs);
    let retry = RetryPolicy {
        max_retries: item.max_retries,
        backoff_secs: item.retry_backoff_secs,
    };
    let current_dir = Path::new(".");
    let source_heads = remote_refs(
        current_dir,
        &["ls-remote", "--heads", "--", item.source.as_str()],
        timeout,
        retry,
    )?;
    if matches!(item.mode, SyncMode::Branch)
        && !source_heads.keys().any(|branch| {
            config::branch_selected(&item.branches, branch)
                && config::ref_selected(
                    &item.include_refs,
                    &item.exclude_refs,
                    &format!("refs/heads/{branch}"),
                )
        })
    {
        return Err("no source branches match the configured branches".into());
    }
    if matches!(item.mode, SyncMode::Mirror) {
        git::run(
            current_dir,
            &["ls-remote", "--", item.source.as_str()],
            timeout,
            retry,
        )?;
    }
    git::run(
        current_dir,
        &["ls-remote", "--tags", "--", item.source.as_str()],
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
        git::run(current_dir, &["ls-remote", "--", target], timeout, retry)?;
    }
    if check_write {
        let repo_dir = Path::new(&item.workspace);
        if !repo_dir.exists() {
            return Err("--check-write requires an existing workspace".into());
        }
        let source = match item.mode {
            SyncMode::Branch => Some(source_refs(repo_dir, item, timeout, retry)?),
            SyncMode::Mirror => None,
        };
        let (source_ref, destination_ref) = match (&source, item.mode) {
            (Some(source), SyncMode::Branch) => source
                .branches
                .first()
                .map(|branch| {
                    (
                        branch.source_ref.clone(),
                        format!("refs/heads/{}", branch.branch),
                    )
                })
                .ok_or("no source branch is available for write preflight")?,
            (None, SyncMode::Mirror) => local_refs(repo_dir, timeout, retry)?
                .keys()
                .find(|reference| {
                    reference.starts_with("refs/heads/")
                        && config::ref_selected(&item.include_refs, &item.exclude_refs, reference)
                })
                .map(|reference| (reference.clone(), reference.clone()))
                .ok_or("no source branch is available for write preflight")?,
            _ => unreachable!(),
        };
        for (index, target) in item.target.iter().enumerate() {
            let remote = format!("target{index}");
            configure_target_remote(repo_dir, &remote, target, timeout, retry)?;
            let destination = format!("{source_ref}:{destination_ref}");
            let mut args = vec!["push".to_owned(), "--dry-run".to_owned()];
            if item.atomic {
                args.push("--atomic".into());
            }
            args.push(remote);
            args.push(destination);
            let args = args.iter().map(String::as_str).collect::<Vec<_>>();
            git::run(repo_dir, &args, timeout, retry)
                .map_err(|error| io::Error::other(format!("write preflight failed: {error}")))?;
        }
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
            "--format=%(refname:strip=3)%09%(objectname)",
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
            let (branch, sha) = ref_fields(line)?;
            let reference = format!("refs/heads/{branch}");
            (branch != "HEAD"
                && config::branch_selected(&item.branches, branch)
                && config::ref_selected(&item.include_refs, &item.exclude_refs, &reference))
            .then(|| SourceBranch {
                branch: branch.into(),
                source_ref: format!("refs/remotes/origin/{branch}"),
                sha: sha.into(),
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
            "--format=%(refname:strip=2)%09%(objectname)",
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
            let (tag, sha) = ref_fields(line)?;
            let reference = format!("refs/tags/{tag}");
            config::ref_selected(&item.include_refs, &item.exclude_refs, &reference).then(|| {
                SourceTag {
                    tag: tag.into(),
                    source_ref: reference,
                    sha: sha.into(),
                }
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
                "--",
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
        &["fetch", "--prune", "--prune-tags", "--tags", "origin"],
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
    deleted_refs: Vec<String>,
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
    configure_target_remote(repo_dir, remote, target, timeout, retry)?;

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
            if !item.dry_run {
                verify_target(repo_dir, remote, &plan, timeout, retry)?;
            }
            Ok(TargetOutcome {
                pushed: !item.dry_run,
                skipped_branches: plan.skipped_branches,
                skipped_tags: plan.skipped_tags,
                synced_refs: plan.synced_refs,
            })
        }
        SyncMode::Mirror => {
            git::run(repo_dir, &["ls-remote", remote], timeout, retry)?;
            let plan = mirror_plan(repo_dir, remote, item, timeout, retry)?;
            if plan.refspecs.is_empty() {
                return Ok(TargetOutcome {
                    pushed: false,
                    skipped_branches: 0,
                    skipped_tags: 0,
                    synced_refs: plan.synced_refs,
                });
            }
            if item.sync_lfs {
                push_lfs(repo_dir, remote, &plan.lfs_refs, item, timeout, retry)?;
            }
            push_plan(repo_dir, remote, item, &plan, timeout, retry)?;
            if !item.dry_run {
                verify_target(repo_dir, remote, &plan, timeout, retry)?;
            }
            Ok(TargetOutcome {
                pushed: !item.dry_run,
                skipped_branches: 0,
                skipped_tags: 0,
                synced_refs: plan.synced_refs,
            })
        }
    }
}

fn configure_target_remote(
    repo_dir: &Path,
    remote: &str,
    target: &str,
    timeout: Duration,
    retry: RetryPolicy,
) -> io::Result<()> {
    if git::remote_exists(repo_dir, remote, timeout, retry)? {
        git::run(
            repo_dir,
            &["remote", "set-url", remote, target],
            timeout,
            retry,
        )?;
    } else {
        git::run(repo_dir, &["remote", "add", remote, target], timeout, retry)?;
    }
    Ok(())
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
        deleted_refs: Vec::new(),
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
            if let Some(target_sha) = target_sha {
                plan.synced_refs
                    .insert(format!("refs/heads/{}", source_branch.branch), target_sha);
            }
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
            let reference = format!("refs/heads/{branch}");
            if source_names.contains(branch.as_str())
                || !config::branch_selected(&item.branches, branch)
                || !config::ref_selected(&item.include_refs, &item.exclude_refs, &reference)
            {
                continue;
            }
            plan.refspecs.push(format!(":refs/heads/{branch}"));
            plan.leases.push(format!(
                "--force-with-lease=refs/heads/{branch}:{target_sha}"
            ));
            plan.deleted_refs.push(format!("refs/heads/{branch}"));
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
            Some(target_sha) if target_sha == &source_tag.sha => {
                plan.synced_refs.insert(
                    format!("refs/tags/{}", source_tag.tag),
                    source_tag.sha.clone(),
                );
            }
            Some(target_sha) => match item.tag_policy {
                TagPolicy::Preserve => {
                    plan.skipped_tags += 1;
                    plan.synced_refs
                        .insert(format!("refs/tags/{}", source_tag.tag), target_sha.clone());
                }
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
            let reference = format!("refs/tags/{tag}");
            if source_names.contains(tag.as_str())
                || !config::ref_selected(&item.include_refs, &item.exclude_refs, &reference)
            {
                continue;
            }
            plan.refspecs.push(format!(":refs/tags/{tag}"));
            plan.leases
                .push(format!("--force-with-lease=refs/tags/{tag}:{target_sha}"));
            plan.deleted_refs.push(format!("refs/tags/{tag}"));
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
            let (sha, name) = ref_fields(line)?;
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

fn all_remote_refs(
    repo_dir: &Path,
    remote: &str,
    timeout: Duration,
    retry: RetryPolicy,
) -> io::Result<BTreeMap<String, String>> {
    let output = git::output(repo_dir, &["ls-remote", remote], timeout, retry)?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "git ls-remote failed with {}",
            output.status
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let (sha, reference) = ref_fields(line)?;
            if reference.ends_with("^{}") || !reference.starts_with("refs/") {
                return None;
            }
            Some((reference.to_owned(), sha.to_owned()))
        })
        .collect())
}

fn mirror_plan(
    repo_dir: &Path,
    remote: &str,
    item: &Item,
    timeout: Duration,
    retry: RetryPolicy,
) -> io::Result<PushPlan> {
    let source_refs = local_refs(repo_dir, timeout, retry)?;
    let target_refs = all_remote_refs(repo_dir, remote, timeout, retry)?;
    let mut plan = PushPlan {
        refspecs: Vec::new(),
        leases: Vec::new(),
        deleted_refs: Vec::new(),
        lfs_refs: Vec::new(),
        synced_refs: BTreeMap::new(),
        skipped_branches: 0,
        skipped_tags: 0,
    };
    for (reference, sha) in &source_refs {
        if !config::ref_selected(&item.include_refs, &item.exclude_refs, reference) {
            continue;
        }
        plan.synced_refs.insert(reference.clone(), sha.clone());
        if target_refs.get(reference) == Some(sha) {
            continue;
        }
        plan.refspecs.push(format!("{reference}:{reference}"));
        plan.leases.push(format!(
            "--force-with-lease={reference}:{}",
            target_refs.get(reference).map(String::as_str).unwrap_or("")
        ));
        plan.lfs_refs.push(reference.clone());
    }
    for (reference, sha) in &target_refs {
        if source_refs.contains_key(reference)
            || !config::ref_selected(&item.include_refs, &item.exclude_refs, reference)
        {
            continue;
        }
        plan.refspecs.push(format!(":{reference}"));
        plan.leases
            .push(format!("--force-with-lease={reference}:{sha}"));
        plan.deleted_refs.push(reference.clone());
    }
    Ok(plan)
}

fn verify_target(
    repo_dir: &Path,
    remote: &str,
    plan: &PushPlan,
    timeout: Duration,
    retry: RetryPolicy,
) -> io::Result<()> {
    let actual = all_remote_refs(repo_dir, remote, timeout, retry)?;
    for (reference, expected_sha) in &plan.synced_refs {
        if actual.get(reference) != Some(expected_sha) {
            return Err(io::Error::other(format!(
                "target verification failed for {reference}"
            )));
        }
    }
    for reference in &plan.deleted_refs {
        if actual.contains_key(reference) {
            return Err(io::Error::other(format!(
                "target verification found undeleted ref {reference}"
            )));
        }
    }
    Ok(())
}

fn local_refs(
    repo_dir: &Path,
    timeout: Duration,
    retry: RetryPolicy,
) -> io::Result<BTreeMap<String, String>> {
    let output = git::output(
        repo_dir,
        &[
            "for-each-ref",
            "--format=%(refname)%09%(objectname)",
            "refs",
        ],
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
            let (reference, sha) = ref_fields(line)?;
            Some((reference.to_owned(), sha.to_owned()))
        })
        .collect())
}

fn ref_fields(line: &str) -> Option<(&str, &str)> {
    line.split_once('\t')
}

struct WorkspaceLock {
    path: PathBuf,
}

impl WorkspaceLock {
    fn acquire(workspace: &Path) -> io::Result<Self> {
        let workspace = config::workspace_identity(workspace)?;
        let name = workspace
            .file_name()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "workspace has no file name")
            })?
            .to_string_lossy()
            .into_owned();
        let mut path = workspace;
        path.set_file_name(format!("{name}.lock"));
        for attempt in 0..=1 {
            let mut file = match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => file,
                Err(error)
                    if error.kind() == io::ErrorKind::AlreadyExists
                        && attempt == 0
                        && stale_lock(&path) =>
                {
                    fs::remove_file(&path)?;
                    continue;
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("workspace is locked: {}", path.display()),
                    ));
                }
                Err(error) => return Err(error),
            };
            if let Err(error) =
                writeln!(file, "pid={} created_ms={}", process::id(), state::now_ms())
            {
                let _ = fs::remove_file(&path);
                return Err(error);
            }
            return Ok(Self { path });
        }
        unreachable!()
    }
}

impl Drop for WorkspaceLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn stale_lock(path: &Path) -> bool {
    let Ok(content) = fs::read_to_string(path) else {
        return false;
    };
    let mut pid = None;
    let mut created_ms = None;
    for field in content.split_whitespace() {
        if let Some(value) = field.strip_prefix("pid=") {
            pid = value.parse::<u32>().ok();
        } else if let Some(value) = field.strip_prefix("created_ms=") {
            created_ms = value.parse::<i64>().ok();
        }
    }
    #[cfg(unix)]
    if let Some(pid) = pid {
        if std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .output()
            .map(|output| output.status.success())
            .unwrap_or(true)
        {
            return false;
        }
        return true;
    }
    #[cfg(not(unix))]
    let _ = pid;
    created_ms.is_some_and(|created| state::now_ms().saturating_sub(created) > 86_400_000)
}

#[cfg(test)]
mod tests {
    use super::ref_fields;

    #[test]
    fn parses_ref_names_containing_spaces() {
        assert_eq!(
            ref_fields("refs/heads/feature with spaces\t0123456789abcdef"),
            Some(("refs/heads/feature with spaces", "0123456789abcdef"))
        );
    }
}
