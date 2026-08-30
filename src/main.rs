use clap::Parser;
use job_scheduler_ng::{Job, JobScheduler};
use repo_sync::{
    backup_task_database, check, check_task_database, cooldown_active, list_tasks,
    retry_event as retry_webhook, serve_webhook, status_report, sync, webhook_events, Item,
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
    #[clap(
        short = 'd',
        long,
        default_value = "repo-sync-tasks.sqlite3",
        help = "SQLite task database path"
    )]
    database: String,

    #[clap(long, help = "select a task by workspace for task-scoped commands")]
    workspace: Option<String>,

    #[clap(long, help = "validate config and repository access only")]
    check: bool,

    #[clap(long, help = "also test target write access with a dry-run push")]
    check_write: bool,

    #[clap(long, help = "show persisted synchronization status")]
    status: bool,

    #[clap(long, help = "format --status as JSON")]
    json: bool,

    #[clap(
        long,
        help = "listen for Webhook POST triggers and serve the embedded page"
    )]
    serve: Option<String>,

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

    #[clap(long, value_name = "PATH", help = "backup the SQLite task database")]
    backup_tasks: Option<String>,

    #[clap(
        long,
        help = "remove the administrator account and require first-use setup"
    )]
    reset_admin: bool,

    #[clap(long, help = "run scheduled items once and exit")]
    once: bool,
}

fn main() -> Result<(), Box<dyn Error>> {
    let Args {
        database,
        workspace,
        check: check_only,
        check_write,
        status: status_only,
        json,
        serve: serve_addr,
        events,
        retry_event,
        prune_history_days,
        backup_state,
        backup_tasks,
        reset_admin,
        once,
    } = Args::parse();
    let retry_workspace = workspace.clone();
    let database_path = Path::new(&database);
    let control_command = check_only
        || status_only
        || events
        || retry_event.is_some()
        || prune_history_days.is_some()
        || backup_state.is_some()
        || backup_tasks.is_some()
        || reset_admin;
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
    if let Some(days) = prune_history_days {
        if days < 7 {
            return Err(
                "--prune-history-days must be at least 7 to preserve webhook deduplication".into(),
            );
        }
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
    if backup_tasks.is_some()
        && (check_only
            || status_only
            || once
            || serve_addr.is_some()
            || retry_event.is_some()
            || events
            || prune_history_days.is_some()
            || backup_state.is_some())
    {
        return Err("--backup-tasks cannot be combined with another control command".into());
    }
    if reset_admin
        && (check_only
            || status_only
            || once
            || serve_addr.is_some()
            || retry_event.is_some()
            || events
            || prune_history_days.is_some()
            || backup_state.is_some()
            || backup_tasks.is_some())
    {
        return Err("--reset-admin cannot be combined with another control command".into());
    }
    if serve_addr.is_some() && once {
        return Err("--serve cannot be combined with --once".into());
    }

    if let Some(destination) = backup_tasks {
        backup_task_database(database_path, Path::new(&destination))?;
        println!("SQLite task database backed up to {destination}");
        return Ok(());
    }
    if reset_admin {
        if repo_sync::reset_admin(database_path)? {
            println!("administrator account reset; set a new account and password in the Web page");
        } else {
            println!("no administrator account was initialized");
        }
        return Ok(());
    }

    let config = list_tasks(database_path)?
        .into_iter()
        .filter(|task| {
            ((control_command && retry_event.is_none()) || task.enabled)
                && match workspace.as_deref() {
                    Some(selected) => selected == task.item.workspace,
                    None => true,
                }
        })
        .map(|task| task.item)
        .collect::<Vec<_>>();
    if check_only {
        check_task_database(database_path)?;
        for item in &config {
            repo_sync::check_state(Path::new(&item.workspace), &item.source)?;
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
        return serve_webhook(&addr, database_path);
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
