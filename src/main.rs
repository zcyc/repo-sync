use clap::Parser;
use repo_sync::{backup_task_database, check, check_task_database, list_tasks, serve_webhook};
use std::{error::Error, path::Path};

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

    #[clap(
        long,
        help = "listen for Webhook POST triggers and serve the embedded page"
    )]
    serve: Option<String>,

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
}

fn main() -> Result<(), Box<dyn Error>> {
    let Args {
        database,
        workspace,
        check: check_only,
        check_write,
        serve: serve_addr,
        prune_history_days,
        backup_state,
        backup_tasks,
        reset_admin,
    } = Args::parse();
    let database_path = Path::new(&database);
    let control_command = check_only
        || prune_history_days.is_some()
        || backup_state.is_some()
        || backup_tasks.is_some()
        || reset_admin;
    if check_write && !check_only {
        return Err("--check-write requires --check".into());
    }
    if let Some(days) = prune_history_days {
        if days < 7 {
            return Err(
                "--prune-history-days must be at least 7 to preserve webhook deduplication".into(),
            );
        }
    }
    if prune_history_days.is_some() && (check_only || serve_addr.is_some()) {
        return Err("--prune-history-days cannot be combined with another control command".into());
    }
    if backup_state.is_some()
        && (check_only || serve_addr.is_some() || prune_history_days.is_some())
    {
        return Err("--backup-state cannot be combined with another control command".into());
    }
    if backup_tasks.is_some()
        && (check_only
            || serve_addr.is_some()
            || prune_history_days.is_some()
            || backup_state.is_some())
    {
        return Err("--backup-tasks cannot be combined with another control command".into());
    }
    if reset_admin
        && (check_only
            || serve_addr.is_some()
            || prune_history_days.is_some()
            || backup_state.is_some()
            || backup_tasks.is_some())
    {
        return Err("--reset-admin cannot be combined with another control command".into());
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
            (control_command || task.enabled)
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
    let Some(addr) = serve_addr else {
        return Err("--serve is required; run tasks from the Web page".into());
    };
    serve_webhook(&addr, database_path)
}
