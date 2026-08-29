use clap::Parser;
use job_scheduler_ng::{Job, JobScheduler, Schedule};
use repo_sync::{check, load, sync, validate, DivergencePolicy, Item, SyncMode, TagPolicy};
use std::{error::Error, time::Duration};

#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Args {
    #[clap(short, long, help = "source repository URL or path")]
    source: Option<String>,

    #[clap(short, long, num_args = 1.., help = "target repository URLs or paths")]
    target: Option<Vec<String>>,

    #[clap(short, long, help = "TOML config file path")]
    file: Option<String>,

    #[clap(long, help = "local repository workspace")]
    workspace: Option<String>,

    #[clap(long, value_enum, help = "sync mode: branch or mirror")]
    mode: Option<SyncMode>,

    #[clap(short, long, help = "cron schedule")]
    crontab: Option<String>,

    #[clap(long, num_args = 1.., help = "branch names or glob patterns")]
    branches: Option<Vec<String>>,

    #[clap(long, help = "sync all source branches")]
    all_branches: bool,

    #[clap(long, help = "Git command timeout in seconds")]
    timeout_secs: Option<u64>,

    #[clap(long, help = "show planned changes without writing targets")]
    dry_run: bool,

    #[clap(long, help = "allow ref deletion or forced tag/mirror updates")]
    allow_destructive: bool,

    #[clap(long, help = "sync Git LFS objects")]
    sync_lfs: bool,

    #[clap(long, value_enum, help = "branch divergence policy")]
    divergence: Option<DivergencePolicy>,

    #[clap(long, value_enum, help = "tag conflict policy")]
    tag_policy: Option<TagPolicy>,

    #[clap(long, help = "delete target branches absent from source")]
    prune_branches: bool,

    #[clap(long, help = "delete target tags absent from source")]
    prune_tags: bool,

    #[clap(long, help = "require atomic target ref updates")]
    atomic: bool,

    #[clap(long, help = "additional retries per Git command")]
    max_retries: Option<u32>,

    #[clap(long, help = "initial retry backoff in seconds")]
    retry_backoff_secs: Option<u64>,

    #[clap(long, help = "validate config and repository access only")]
    check: bool,

    #[clap(long, help = "run scheduled items once and exit")]
    once: bool,
}

fn main() -> Result<(), Box<dyn Error>> {
    let Args {
        source,
        target,
        file,
        workspace,
        mode,
        crontab,
        branches,
        all_branches,
        timeout_secs,
        dry_run,
        allow_destructive,
        sync_lfs,
        divergence,
        tag_policy,
        prune_branches,
        prune_tags,
        atomic,
        max_retries,
        retry_backoff_secs,
        check: check_only,
        once,
    } = Args::parse();

    if check_only && once {
        return Err("--check and --once cannot be used together".into());
    }

    let config = match (source, target, file) {
        (Some(source), Some(target), None) => {
            if branches.is_some() && all_branches {
                return Err("use either --branches or --all-branches".into());
            }
            let mode = mode.ok_or("--mode is required")?;
            let branches = if all_branches {
                Vec::new()
            } else if let Some(branches) = branches {
                branches
            } else if matches!(mode, SyncMode::Mirror) {
                Vec::new()
            } else {
                return Err("--branches or --all-branches is required in branch mode".into());
            };
            vec![Item {
                source,
                target,
                workspace: workspace.ok_or("--workspace is required")?,
                mode,
                crontab,
                branches,
                timeout_secs: timeout_secs.ok_or("--timeout-secs is required")?,
                dry_run,
                allow_destructive,
                sync_lfs,
                divergence: divergence.ok_or("--divergence is required")?,
                tag_policy: tag_policy.ok_or("--tag-policy is required")?,
                prune_branches,
                prune_tags,
                atomic,
                max_retries: max_retries.ok_or("--max-retries is required")?,
                retry_backoff_secs: retry_backoff_secs.ok_or("--retry-backoff-secs is required")?,
            }]
        }
        (None, None, Some(file))
            if workspace.is_none()
                && mode.is_none()
                && crontab.is_none()
                && branches.is_none()
                && !all_branches
                && timeout_secs.is_none()
                && !dry_run
                && !allow_destructive
                && !sync_lfs
                && divergence.is_none()
                && tag_policy.is_none()
                && !prune_branches
                && !prune_tags
                && !atomic
                && max_retries.is_none()
                && retry_backoff_secs.is_none() =>
        {
            load(&file)?
        }
        _ => return Err("use either --file or all direct sync options".into()),
    };

    validate(&config)?;
    for item in &config {
        if let Some(crontab) = item.crontab.as_deref() {
            crontab.parse::<Schedule>()?;
        }
    }

    if check_only {
        for item in &config {
            check(item)?;
        }
        return Ok(());
    }
    if once {
        return run_once(config);
    }

    let mut schedule = JobScheduler::new();
    let mut scheduled = false;
    let mut one_time_failures = Vec::new();
    for item in config {
        if let Some(crontab) = item.crontab.clone() {
            scheduled = true;
            schedule.add(Job::new(crontab.parse()?, move || {
                if let Err(error) = sync(&item) {
                    eprintln!("sync {} failed: {error}", item.workspace);
                }
            }));
        } else if let Err(error) = sync(&item) {
            eprintln!("sync {} failed: {error}", item.workspace);
            one_time_failures.push(item.workspace);
        }
    }

    if scheduled {
        loop {
            schedule.tick();
            std::thread::sleep(Duration::from_millis(500));
        }
    }
    if one_time_failures.is_empty() {
        Ok(())
    } else {
        Err(format!("sync failed: {}", one_time_failures.join(", ")).into())
    }
}

fn run_once(config: Vec<Item>) -> Result<(), Box<dyn Error>> {
    let mut failures = Vec::new();
    for item in config {
        if let Err(error) = sync(&item) {
            eprintln!("sync {} failed: {error}", item.workspace);
            failures.push(item.workspace);
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!("sync failed: {}", failures.join(", ")).into())
    }
}
