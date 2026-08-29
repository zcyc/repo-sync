use clap::Parser;
use job_scheduler_ng::{Job, JobScheduler, Schedule};
use repo_sync::{
    check, cooldown_active, load, retry_event as retry_webhook, serve_webhook, status_report, sync,
    validate, webhook_events, DivergencePolicy, Item, SyncMode, TagPolicy,
};
use std::{
    error::Error,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Args {
    #[clap(short, long, help = "source repository URL or path")]
    source: Option<String>,

    #[clap(short, long, num_args = 1.., help = "target repository URLs or paths")]
    target: Option<Vec<String>>,

    #[clap(short, long, help = "TOML config file path")]
    file: Option<String>,

    #[clap(
        long,
        help = "local repository workspace; selects --retry-event when used with --file"
    )]
    workspace: Option<String>,

    #[clap(long, value_enum, help = "sync mode: branch or mirror")]
    mode: Option<SyncMode>,

    #[clap(short, long, help = "cron schedule")]
    crontab: Option<String>,

    #[clap(long, num_args = 1.., help = "branch names or glob patterns")]
    branches: Option<Vec<String>>,

    #[clap(long, num_args = 1.., help = "full ref glob patterns to include")]
    include_refs: Option<Vec<String>>,

    #[clap(long, num_args = 1.., help = "full ref glob patterns to exclude")]
    exclude_refs: Option<Vec<String>>,

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

    #[clap(long, help = "pause scheduled runs while every target is failing")]
    failure_cooldown_secs: Option<u64>,

    #[clap(long, help = "validate config and repository access only")]
    check: bool,

    #[clap(long, help = "also test target write access with a dry-run push")]
    check_write: bool,

    #[clap(long, help = "show persisted synchronization status")]
    status: bool,

    #[clap(long, help = "format --status as JSON")]
    json: bool,

    #[clap(long, help = "listen for GitHub/GitLab webhook POST triggers")]
    serve: Option<String>,

    #[clap(long, help = "GitHub webhook secret or GitLab signing/secret token")]
    webhook_secret: Option<String>,

    #[clap(long, help = "show recent webhook event history")]
    events: bool,

    #[clap(long, help = "retry a dead or failed webhook event by event id")]
    retry_event: Option<i64>,

    #[clap(
        long,
        value_name = "DAYS",
        help = "delete finished SQLite history older than DAYS"
    )]
    prune_history_days: Option<u64>,

    #[clap(
        long,
        value_name = "PATH",
        help = "backup one workspace SQLite database"
    )]
    backup_state: Option<String>,

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
        include_refs,
        exclude_refs,
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
        failure_cooldown_secs,
        check: check_only,
        check_write,
        status: status_only,
        json,
        serve: serve_addr,
        webhook_secret,
        events,
        retry_event,
        prune_history_days,
        backup_state,
        once,
    } = Args::parse();
    let retry_workspace = workspace.clone();
    let config_file = file.clone();

    if check_only && once {
        return Err("--check and --once cannot be used together".into());
    }
    if check_write && !check_only {
        return Err("--check-write requires --check".into());
    }
    if status_only && (check_only || once) {
        return Err("--status cannot be combined with --check or --once".into());
    }
    if json && !status_only && !events {
        return Err("--json requires --status or --events".into());
    }
    if events
        && (check_only || status_only || once || serve_addr.is_some() || retry_event.is_some())
    {
        return Err("--events cannot be combined with another control command".into());
    }
    if retry_event.is_some() && (check_only || status_only || once || serve_addr.is_some()) {
        return Err("--retry-event cannot be combined with another control command".into());
    }
    if prune_history_days == Some(0) {
        return Err("--prune-history-days must be greater than zero".into());
    }
    if prune_history_days.is_some()
        && (check_only
            || status_only
            || once
            || serve_addr.is_some()
            || retry_event.is_some()
            || events)
    {
        return Err("--prune-history-days cannot be combined with another control command".into());
    }
    if backup_state.is_some()
        && (check_only
            || status_only
            || once
            || serve_addr.is_some()
            || retry_event.is_some()
            || events
            || prune_history_days.is_some())
    {
        return Err("--backup-state cannot be combined with another control command".into());
    }
    if serve_addr.is_some() && config_file.is_none() && webhook_secret.is_none() {
        return Err("direct --serve requires --webhook-secret".into());
    }
    if serve_addr.is_some() && config_file.is_some() && webhook_secret.is_some() {
        return Err(
            "--webhook-secret cannot be used with --file; configure webhook_secret_envs".into(),
        );
    }
    if serve_addr.is_none() && webhook_secret.is_some() {
        return Err("--webhook-secret requires --serve".into());
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
                include_refs: include_refs.unwrap_or_default(),
                exclude_refs: exclude_refs.unwrap_or_default(),
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
                failure_cooldown_secs: failure_cooldown_secs
                    .ok_or("--failure-cooldown-secs is required")?,
                webhook_secret_envs: Vec::new(),
            }]
        }
        (None, None, Some(file))
            if (workspace.is_none() || retry_event.is_some())
                && mode.is_none()
                && crontab.is_none()
                && branches.is_none()
                && include_refs.is_none()
                && exclude_refs.is_none()
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
                && retry_backoff_secs.is_none()
                && failure_cooldown_secs.is_none() =>
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
            check(item, check_write)?;
        }
        return Ok(());
    }
    if let Some(days) = prune_history_days {
        if config.len() != 1 {
            return Err("--prune-history-days requires exactly one sync item".into());
        }
        let item = &config[0];
        let deleted = repo_sync::prune_history(Path::new(&item.workspace), &item.source, days)?;
        println!("pruned {deleted} SQLite history rows");
        return Ok(());
    }
    if let Some(destination) = backup_state {
        if config.len() != 1 {
            return Err("--backup-state requires exactly one sync item".into());
        }
        let item = &config[0];
        repo_sync::backup_state(
            Path::new(&item.workspace),
            &item.source,
            Path::new(&destination),
        )?;
        println!("SQLite state backed up to {destination}");
        return Ok(());
    }
    if status_only {
        let reports = config
            .iter()
            .map(|item| status_report(Path::new(&item.workspace), &item.source))
            .collect::<Result<Vec<_>, _>>()?;
        if json {
            println!("{}", serde_json::to_string_pretty(&reports)?);
        } else {
            for report in reports {
                println!(
                    "workspace={} initialized={} latest_run={}",
                    report.workspace,
                    report.initialized,
                    report
                        .latest_run
                        .as_ref()
                        .map(|run| run.status.as_str())
                        .unwrap_or("none")
                );
                for target in report.targets {
                    println!(
                        "  target={} status={} failures={} refs={}",
                        target.target,
                        target.status,
                        target.consecutive_failures,
                        target.synced_refs.len()
                    );
                    if let Some(error) = target.last_error {
                        println!("    error={error}");
                    }
                }
            }
        }
        return Ok(());
    }
    if events {
        let history = config
            .iter()
            .map(|item| webhook_events(Path::new(&item.workspace), &item.source, 50))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        if json {
            println!("{}", serde_json::to_string_pretty(&history)?);
        } else {
            for event in history {
                println!(
                    "event_id={} workspace={} provider={} type={} status={} attempts={} refs={}",
                    event.event_id,
                    event.workspace,
                    event.provider,
                    event.event_type,
                    event.status,
                    event.attempts,
                    event
                        .refs
                        .iter()
                        .map(|change| change.reference.as_str())
                        .collect::<Vec<_>>()
                        .join(",")
                );
                if let Some(error) = event.last_error {
                    println!("  error={error}");
                }
            }
        }
        return Ok(());
    }
    if let Some(event_id) = retry_event {
        if retry_webhook(&config, event_id, retry_workspace.as_deref())? {
            println!("webhook event {event_id} retried");
            return Ok(());
        }
        return Err(format!("webhook event not retryable or not found: {event_id}").into());
    }
    if once {
        return run_once(config);
    }
    if let Some(addr) = serve_addr {
        return serve_webhook(
            &addr,
            config,
            config_file.as_deref(),
            webhook_secret.as_deref(),
        );
    }

    let mut schedule = JobScheduler::new();
    let mut scheduled = false;
    let mut one_time_failures = Vec::new();
    let shutdown = Arc::new(AtomicBool::new(false));
    for item in config {
        if let Some(crontab) = item.crontab.clone() {
            scheduled = true;
            let shutdown = Arc::clone(&shutdown);
            schedule.add(Job::new(crontab.parse()?, move || {
                if shutdown.load(Ordering::Relaxed) {
                    return;
                }
                match cooldown_active(
                    Path::new(&item.workspace),
                    &item.source,
                    &item.target,
                    item.failure_cooldown_secs,
                ) {
                    Ok(true) => {
                        eprintln!("sync {} paused by failure cooldown", item.workspace);
                        return;
                    }
                    Err(error) => {
                        eprintln!("sync {} cooldown check failed: {error}", item.workspace)
                    }
                    Ok(false) => {}
                }
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
        let shutdown_handler = Arc::clone(&shutdown);
        ctrlc::set_handler(move || shutdown_handler.store(true, Ordering::Relaxed))?;
        while !shutdown.load(Ordering::Relaxed) {
            schedule.tick();
            std::thread::sleep(Duration::from_millis(500));
        }
        eprintln!("shutdown requested");
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
