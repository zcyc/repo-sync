use clap::Parser;
use job_scheduler_ng::{Job, JobScheduler, Schedule};
use repo_sync::{get_config_vec, sync, validate_config, Item, SyncMode};
use std::{error::Error, time::Duration};

#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Args {
    #[clap(
        short,
        long,
        value_parser,
        help = "source repo, eg: https://github.com/zcyc/repo-sync.git"
    )]
    source: Option<String>,

    #[clap(
        short,
        long,
        num_args = 1..,
        value_parser,
        help = "target repo, eg: https://github.com/zcyc/repo-sync.git"
    )]
    target: Option<Vec<String>>,

    #[clap(
        short,
        long,
        value_parser,
        help = "config file path, eg: ./config.toml"
    )]
    file: Option<String>,

    #[clap(long, value_parser, help = "local checkout path")]
    workspace: Option<String>,

    #[clap(long, value_enum, help = "sync mode: branch or mirror")]
    mode: Option<SyncMode>,

    #[clap(
        short,
        long,
        value_parser,
        help = "crontab string, eg: '0 * * * * ? *'"
    )]
    crontab: Option<String>,

    #[clap(
        short = 'b',
        long,
        value_parser,
        help = "branch to sync in branch mode"
    )]
    branch: Option<String>,

    #[clap(long, value_parser, help = "Git command timeout in seconds")]
    timeout_secs: Option<u64>,
}

fn main() -> Result<(), Box<dyn Error>> {
    let Args {
        source,
        target,
        file,
        workspace,
        mode,
        crontab,
        branch,
        timeout_secs,
    } = Args::parse();

    let config_vec = match (source, target, file) {
        (Some(source), Some(target), None) => vec![Item {
            source,
            target,
            workspace: workspace.ok_or("--workspace is required")?,
            mode: mode.ok_or("--mode is required")?,
            crontab,
            branch,
            timeout_secs: timeout_secs.ok_or("--timeout-secs is required")?,
        }],
        (None, None, Some(file))
            if workspace.is_none()
                && mode.is_none()
                && crontab.is_none()
                && branch.is_none()
                && timeout_secs.is_none() =>
        {
            get_config_vec(&file)?
        }
        _ => return Err("use either --file or all direct sync options".into()),
    };

    validate_config(&config_vec)?;
    for item in &config_vec {
        if let Some(crontab) = item.crontab.as_deref() {
            crontab.parse::<Schedule>()?;
        }
    }

    let mut schedule = JobScheduler::new();
    let mut scheduled = false;
    let mut one_time_failures = Vec::new();

    for config_item in config_vec {
        if let Some(crontab) = config_item.crontab.clone() {
            scheduled = true;
            schedule.add(Job::new(crontab.parse()?, move || {
                if let Err(error) = sync(&config_item) {
                    eprintln!("sync {} failed: {error}", config_item.workspace);
                }
            }));
        } else if let Err(error) = sync(&config_item) {
            eprintln!("sync {} failed: {error}", config_item.workspace);
            one_time_failures.push(config_item.workspace);
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
