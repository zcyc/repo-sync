use clap::Parser;
use job_scheduler_ng::{Job, JobScheduler, Schedule};
use repo_sync::{
    check, cooldown_active, load, status_report, sync, validate, DivergencePolicy, Item, SyncMode,
    TagPolicy,
};
use std::{
    error::Error,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
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

    #[clap(long, help = "local repository workspace")]
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

    #[clap(long, help = "listen for generic authenticated HTTP POST triggers")]
    serve: Option<String>,

    #[clap(long, help = "shared secret for generic webhook requests")]
    webhook_secret: Option<String>,

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
        once,
    } = Args::parse();
    let webhook_secret = webhook_secret.or_else(|| std::env::var("REPO_SYNC_WEBHOOK_SECRET").ok());

    if check_only && once {
        return Err("--check and --once cannot be used together".into());
    }
    if check_write && !check_only {
        return Err("--check-write requires --check".into());
    }
    if status_only && (check_only || once) {
        return Err("--status cannot be combined with --check or --once".into());
    }
    if json && !status_only {
        return Err("--json requires --status".into());
    }
    if serve_addr.is_some() && (check_only || status_only || once) {
        return Err("--serve cannot be combined with --check, --status, or --once".into());
    }
    if serve_addr.is_some() && webhook_secret.is_none() {
        return Err("--serve requires --webhook-secret or REPO_SYNC_WEBHOOK_SECRET".into());
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
            }]
        }
        (None, None, Some(file))
            if workspace.is_none()
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
    if once {
        return run_once(config);
    }
    if let Some(addr) = serve_addr {
        return serve(&addr, webhook_secret.as_deref().unwrap(), config);
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

fn serve(addr: &str, secret: &str, config: Vec<Item>) -> Result<(), Box<dyn Error>> {
    let listener = TcpListener::bind(addr)?;
    listener.set_nonblocking(true)?;
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_handler = Arc::clone(&shutdown);
    ctrlc::set_handler(move || shutdown_handler.store(true, Ordering::Relaxed))?;
    eprintln!("webhook listener started on {addr}");
    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        match listener.accept() {
            Ok((stream, _)) => handle_webhook(stream, secret, &config),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(error) => eprintln!("webhook connection failed: {error}"),
        }
    }
    eprintln!("webhook listener stopped");
    Ok(())
}

fn handle_webhook(mut stream: TcpStream, secret: &str, config: &[Item]) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut request = Vec::new();
    let mut buffer = [0; 4096];
    while request.len() < 16 * 1024 {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(size) => {
                request.extend_from_slice(&buffer[..size]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            Err(error) => {
                write_response(&mut stream, "400 Bad Request", "invalid request");
                eprintln!("webhook request read failed: {error}");
                return;
            }
        }
    }
    let request = String::from_utf8_lossy(&request);
    let mut lines = request.split("\r\n");
    let method = lines.next().unwrap_or_default();
    if !method.starts_with("POST ") {
        write_response(&mut stream, "405 Method Not Allowed", "POST required");
        return;
    }
    let provided_secret = lines
        .by_ref()
        .take_while(|line| !line.is_empty())
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("x-repo-sync-secret")
                .then(|| value.trim())
        })
        .unwrap_or_default();
    if !constant_time_equal(provided_secret.as_bytes(), secret.as_bytes()) {
        write_response(&mut stream, "401 Unauthorized", "invalid webhook secret");
        return;
    }
    write_response(&mut stream, "202 Accepted", "sync accepted");
    if let Err(error) = run_once(config.to_vec()) {
        eprintln!("webhook sync failed: {error}");
    }
}

fn write_response(stream: &mut TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    for index in 0..left.len().max(right.len()) {
        difference |= usize::from(
            left.get(index).copied().unwrap_or(0) ^ right.get(index).copied().unwrap_or(0),
        );
    }
    difference == 0
}
