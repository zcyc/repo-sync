use crate::{
    config, dashboard, git,
    state::{self, StateDb, WebhookEnqueue, WebhookEventInput, WebhookRefChange},
    sync,
    tasks::{self, Task},
    Item,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use hmac::{Hmac, Mac};
use job_scheduler_ng::{Job, JobScheduler, Schedule};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;
use std::{
    collections::{BTreeMap, HashMap},
    error::Error,
    io::{self, Read, Write},
    net::{IpAddr, SocketAddr, TcpListener, TcpStream},
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
        Arc, Mutex, RwLock,
    },
    thread,
    time::{Duration, Instant},
};

type HmacSha256 = Hmac<Sha256>;

const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;
const MAX_ACTIVE_CONNECTIONS: u64 = 64;
const SIGNATURE_TOLERANCE_SECS: i64 = 300;
const LOGIN_FAILURE_LIMIT: u32 = 5;
const LOGIN_WINDOW: Duration = Duration::from_secs(60);
const LOGIN_BLOCK: Duration = Duration::from_secs(300);
const SECURITY_HEADERS: &str =
    "X-Content-Type-Options: nosniff\r\nX-Frame-Options: DENY\r\nReferrer-Policy: no-referrer\r\n";

#[derive(Clone, Debug)]
struct WebhookEvent {
    provider: &'static str,
    delivery_id: String,
    event_type: String,
    repository_keys: Vec<String>,
    refs: Vec<WebhookRefChange>,
}

struct HttpRequest {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

#[derive(Clone)]
struct WebhookItem {
    task_id: i64,
    enabled: bool,
    item: Item,
    secrets: Vec<String>,
}

enum WorkerCommand {
    ConfigChanged,
    Webhook(i64),
    Scheduled(i64),
}

#[derive(Default)]
struct LoginLimiter {
    attempts: Mutex<HashMap<IpAddr, LoginAttempt>>,
}

struct LoginAttempt {
    window_started: Instant,
    failures: u32,
    blocked_until: Option<Instant>,
}

impl LoginLimiter {
    fn allowed(&self, address: IpAddr) -> bool {
        let now = Instant::now();
        let mut attempts = self.attempts.lock().expect("login limiter lock poisoned");
        attempts.retain(|_, attempt| {
            attempt
                .blocked_until
                .is_some_and(|blocked_until| blocked_until > now)
                || now.duration_since(attempt.window_started) < LOGIN_WINDOW
        });
        let attempt = attempts.entry(address).or_insert(LoginAttempt {
            window_started: now,
            failures: 0,
            blocked_until: None,
        });
        attempt
            .blocked_until
            .is_none_or(|blocked_until| blocked_until <= now)
    }

    fn record_failure(&self, address: IpAddr) {
        let now = Instant::now();
        let mut attempts = self.attempts.lock().expect("login limiter lock poisoned");
        let attempt = attempts.entry(address).or_insert(LoginAttempt {
            window_started: now,
            failures: 0,
            blocked_until: None,
        });
        if now.duration_since(attempt.window_started) >= LOGIN_WINDOW {
            attempt.window_started = now;
            attempt.failures = 0;
        }
        attempt.failures = attempt.failures.saturating_add(1);
        if attempt.failures >= LOGIN_FAILURE_LIMIT {
            attempt.blocked_until = Some(now + LOGIN_BLOCK);
        }
    }

    fn record_success(&self, address: IpAddr) {
        self.attempts
            .lock()
            .expect("login limiter lock poisoned")
            .remove(&address);
    }
}

#[derive(Debug)]
struct RequestError {
    status: &'static str,
    message: &'static str,
}

#[derive(Default)]
struct Metrics {
    active_connections: AtomicU64,
    http_requests: AtomicU64,
    rejected_requests: AtomicU64,
    ignored_events: AtomicU64,
    enqueued_events: AtomicU64,
    deduplicated_events: AtomicU64,
    coalesced_events: AtomicU64,
    queue_full_events: AtomicU64,
    successful_syncs: AtomicU64,
    failed_syncs: AtomicU64,
    cancelled_syncs: AtomicU64,
    sync_duration_ms_total: AtomicU64,
    sync_duration_count: AtomicU64,
    collection_errors: AtomicU64,
    auth_failures: AtomicU64,
    auth_rate_limited: AtomicU64,
}

impl Metrics {
    fn render(&self, config: &[WebhookItem]) -> String {
        let mut output = String::new();
        output.push_str("# HELP repo_sync_webhook_active_connections Current webhook connections.\n# TYPE repo_sync_webhook_active_connections gauge\n");
        output.push_str(&format!(
            "repo_sync_webhook_active_connections {}\n",
            self.active_connections.load(Ordering::Relaxed)
        ));
        counter(
            &mut output,
            "repo_sync_http_requests_total",
            "HTTP requests received",
            self.http_requests.load(Ordering::Relaxed),
        );
        counter(
            &mut output,
            "repo_sync_webhook_rejected_total",
            "Webhook requests rejected",
            self.rejected_requests.load(Ordering::Relaxed),
        );
        counter(
            &mut output,
            "repo_sync_webhook_ignored_total",
            "Webhook events ignored",
            self.ignored_events.load(Ordering::Relaxed),
        );
        counter(
            &mut output,
            "repo_sync_webhook_events_enqueued_total",
            "Webhook events enqueued",
            self.enqueued_events.load(Ordering::Relaxed),
        );
        counter(
            &mut output,
            "repo_sync_webhook_events_deduplicated_total",
            "Webhook deliveries deduplicated",
            self.deduplicated_events.load(Ordering::Relaxed),
        );
        counter(
            &mut output,
            "repo_sync_webhook_events_coalesced_total",
            "Webhook events coalesced after a successful sync",
            self.coalesced_events.load(Ordering::Relaxed),
        );
        counter(
            &mut output,
            "repo_sync_webhook_queue_full_total",
            "Webhook requests rejected because the queue was full",
            self.queue_full_events.load(Ordering::Relaxed),
        );
        counter(
            &mut output,
            "repo_sync_sync_runs_succeeded_total",
            "Successful sync runs",
            self.successful_syncs.load(Ordering::Relaxed),
        );
        counter(
            &mut output,
            "repo_sync_sync_runs_failed_total",
            "Failed sync runs",
            self.failed_syncs.load(Ordering::Relaxed),
        );
        counter(
            &mut output,
            "repo_sync_sync_runs_cancelled_total",
            "Cancelled sync runs",
            self.cancelled_syncs.load(Ordering::Relaxed),
        );
        counter_float(
            &mut output,
            "repo_sync_sync_duration_seconds_sum",
            "Total sync duration in seconds",
            self.sync_duration_ms_total.load(Ordering::Relaxed) as f64 / 1000.0,
        );
        counter(
            &mut output,
            "repo_sync_sync_duration_seconds_count",
            "Number of completed sync durations",
            self.sync_duration_count.load(Ordering::Relaxed),
        );
        counter(
            &mut output,
            "repo_sync_metrics_collection_errors_total",
            "Metrics collection errors",
            self.collection_errors.load(Ordering::Relaxed),
        );
        counter(
            &mut output,
            "repo_sync_auth_failures_total",
            "Failed administrator logins",
            self.auth_failures.load(Ordering::Relaxed),
        );
        counter(
            &mut output,
            "repo_sync_auth_rate_limited_total",
            "Administrator logins rejected by rate limiting",
            self.auth_rate_limited.load(Ordering::Relaxed),
        );
        output.push_str("# HELP repo_sync_webhook_events Current webhook events by status.\n# TYPE repo_sync_webhook_events gauge\n");
        let mut queue_counts = BTreeMap::new();
        let mut oldest_pending_ms = None;
        for item in config {
            match state::webhook_queue_stats(
                std::path::Path::new(&item.item.workspace),
                &item.item.source,
            ) {
                Ok(stats) => {
                    for (status, count) in stats.counts {
                        *queue_counts.entry(status).or_insert(0) += count;
                    }
                    oldest_pending_ms = match (oldest_pending_ms, stats.oldest_pending_ms) {
                        (Some(left), Some(right)) => Some(left.min(right)),
                        (None, value) | (value, None) => value,
                    };
                }
                Err(error) => {
                    self.collection_errors.fetch_add(1, Ordering::Relaxed);
                    eprintln!(
                        "metrics collection failed for {}: {error}",
                        item.item.workspace
                    );
                }
            }
        }
        for (status, count) in queue_counts {
            output.push_str(&format!(
                "repo_sync_webhook_events{{status=\"{}\"}} {count}\n",
                escape_label(&status)
            ));
        }
        output.push_str(
            "# HELP repo_sync_webhook_oldest_event_age_seconds Age of the oldest pending webhook event.\n# TYPE repo_sync_webhook_oldest_event_age_seconds gauge\n",
        );
        let age = oldest_pending_ms
            .map(|received| state::now_ms().saturating_sub(received).max(0) as f64 / 1000.0)
            .unwrap_or(0.0);
        output.push_str(&format!(
            "repo_sync_webhook_oldest_event_age_seconds {age:.3}\n"
        ));
        output
    }
}

#[derive(Serialize)]
struct DashboardSnapshot {
    items: Vec<DashboardItem>,
}

#[derive(Serialize)]
struct DashboardItem {
    id: i64,
    enabled: bool,
    config: Item,
    status: state::StatusReport,
    queue: DashboardQueue,
    events: Vec<state::WebhookEventStatus>,
}

#[derive(Deserialize)]
struct TaskRequest {
    item: Item,
    enabled: bool,
}

#[derive(Deserialize)]
struct AuthRequest {
    username: String,
    password: String,
}

#[derive(Deserialize)]
struct PasswordChangeRequest {
    current_password: String,
    new_password: String,
}

#[derive(Serialize)]
struct DashboardQueue {
    counts: BTreeMap<String, i64>,
    oldest_pending_age_seconds: f64,
}

fn dashboard_snapshot(config: &[WebhookItem]) -> Result<String, Box<dyn Error>> {
    let mut items = Vec::with_capacity(config.len());
    for webhook_item in config {
        let item = &webhook_item.item;
        let queue_stats = state::webhook_queue_stats(Path::new(&item.workspace), &item.source)?;
        let oldest_pending_age_seconds = queue_stats
            .oldest_pending_ms
            .map(|received| state::now_ms().saturating_sub(received).max(0) as f64 / 1000.0)
            .unwrap_or(0.0);
        items.push(DashboardItem {
            id: webhook_item.task_id,
            enabled: webhook_item.enabled,
            config: item.clone(),
            status: state::status(Path::new(&item.workspace), &item.source)?,
            queue: DashboardQueue {
                counts: queue_stats.counts,
                oldest_pending_age_seconds,
            },
            events: state::webhook_events(Path::new(&item.workspace), &item.source, 10)?,
        });
    }
    Ok(serde_json::to_string(&DashboardSnapshot { items })?)
}

fn reload_task_config(
    database_path: &Path,
    config_store: &RwLock<Vec<WebhookItem>>,
) -> Result<(), Box<dyn Error>> {
    let task_records = tasks::list_tasks(database_path)?;
    let prepared = prepare_webhook_config(&task_records)?;
    *config_store.write().expect("webhook config lock poisoned") = prepared;
    Ok(())
}

fn task_id_path(path: &str) -> Option<i64> {
    path.strip_prefix("/api/tasks/")?.parse().ok()
}

fn run_task_id_path(path: &str) -> Option<i64> {
    path.strip_prefix("/api/tasks/")?
        .strip_suffix("/run")?
        .parse()
        .ok()
}

fn cancel_task_id_path(path: &str) -> Option<i64> {
    path.strip_prefix("/api/tasks/")?
        .strip_suffix("/cancel")?
        .parse()
        .ok()
}

fn retry_event_task_path(path: &str) -> Option<(i64, i64)> {
    let path = path.strip_prefix("/api/tasks/")?.strip_suffix("/retry")?;
    let (task_id, event_id) = path.split_once("/events/")?;
    Some((task_id.parse().ok()?, event_id.parse().ok()?))
}

fn counter(output: &mut String, name: &str, help: &str, value: u64) {
    output.push_str(&format!(
        "# HELP {name} {help}.\n# TYPE {name} counter\n{name} {value}\n"
    ));
}

fn counter_float(output: &mut String, name: &str, help: &str, value: f64) {
    output.push_str(&format!(
        "# HELP {name} {help}.\n# TYPE {name} counter\n{name} {value:.3}\n"
    ));
}

fn escape_label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

pub fn serve(addr: &str, database_path: &Path) -> Result<(), Box<dyn Error>> {
    let task_records = tasks::list_tasks(database_path)?;
    let config = Arc::new(RwLock::new(prepare_webhook_config(&task_records)?));
    let database_path = database_path.to_owned();
    let listener = TcpListener::bind(addr)?;
    listener.set_nonblocking(true)?;
    let metrics = Arc::new(Metrics::default());
    let (wake_sender, wake_receiver) = mpsc::channel();
    let login_limiter = Arc::new(LoginLimiter::default());
    let shutdown = Arc::new(AtomicBool::new(false));
    let reload = register_reload_flag()?;
    let worker_shutdown = Arc::clone(&shutdown);
    let worker_config = Arc::clone(&config);
    let worker_metrics = Arc::clone(&metrics);
    let worker_wake_sender = wake_sender.clone();
    let worker = thread::spawn(move || {
        worker_loop(
            worker_config,
            wake_receiver,
            worker_wake_sender,
            worker_shutdown,
            worker_metrics,
        )
    });
    let shutdown_handler = Arc::clone(&shutdown);
    ctrlc::set_handler(move || shutdown_handler.store(true, Ordering::Relaxed))?;

    eprintln!("webhook listener started on {addr}");
    while !shutdown.load(Ordering::Relaxed) {
        if reload.swap(false, Ordering::Relaxed) {
            match tasks::list_tasks(&database_path)
                .and_then(|new_tasks| prepare_webhook_config(&new_tasks))
            {
                Ok(new_config) => {
                    *config.write().expect("webhook config lock poisoned") = new_config;
                    let _ = wake_sender.send(WorkerCommand::ConfigChanged);
                    eprintln!("webhook tasks reloaded from {}", database_path.display());
                }
                Err(error) => eprintln!("webhook task reload failed: {error}"),
            }
        }
        match listener.accept() {
            Ok((stream, peer)) => {
                if metrics.active_connections.fetch_add(1, Ordering::Relaxed)
                    >= MAX_ACTIVE_CONNECTIONS
                {
                    metrics.active_connections.fetch_sub(1, Ordering::Relaxed);
                    metrics.rejected_requests.fetch_add(1, Ordering::Relaxed);
                    let mut stream = stream;
                    write_response(
                        &mut stream,
                        "503 Service Unavailable",
                        "too many connections",
                    );
                    continue;
                }
                let config = Arc::clone(&config);
                let database_path = database_path.clone();
                let metrics = Arc::clone(&metrics);
                let wake_sender = wake_sender.clone();
                let login_limiter = Arc::clone(&login_limiter);
                thread::spawn(move || {
                    handle_connection(
                        stream,
                        peer,
                        &config,
                        &database_path,
                        &wake_sender,
                        &metrics,
                        &login_limiter,
                    );
                    metrics.active_connections.fetch_sub(1, Ordering::Relaxed);
                });
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(100));
            }
            Err(error) => eprintln!("webhook connection failed: {error}"),
        }
    }
    drop(wake_sender);
    let _ = worker.join();
    eprintln!("webhook listener stopped");
    Ok(())
}

fn prepare_webhook_config(task_records: &[Task]) -> Result<Vec<WebhookItem>, Box<dyn Error>> {
    task_records
        .iter()
        .map(|task| {
            config::validate_item(&task.item)?;
            let mut secrets = Vec::new();
            for env_name in &task.item.webhook_secret_envs {
                if env_name.trim().is_empty() {
                    return Err("webhook_secret_envs cannot contain an empty name".into());
                }
                match std::env::var(env_name) {
                    Ok(secret) if !secret.is_empty() && !secrets.contains(&secret) => {
                        secrets.push(secret);
                    }
                    Ok(_) => eprintln!("webhook secret environment variable is empty: {env_name}"),
                    Err(_) => {
                        eprintln!("webhook secret environment variable is not set: {env_name}")
                    }
                }
            }
            Ok(WebhookItem {
                task_id: task.id,
                enabled: task.enabled,
                item: task.item.clone(),
                secrets,
            })
        })
        .collect()
}

fn register_reload_flag() -> Result<Arc<AtomicBool>, Box<dyn Error>> {
    let flag = Arc::new(AtomicBool::new(false));
    #[cfg(unix)]
    signal_hook::flag::register(signal_hook::consts::SIGHUP, Arc::clone(&flag))?;
    Ok(flag)
}

pub fn retry_event(
    config: &[Item],
    event_id: i64,
    selected_workspace: Option<&str>,
) -> Result<bool, Box<dyn Error>> {
    let mut candidate = None;
    for item in config {
        if selected_workspace.is_some_and(|workspace| workspace != item.workspace) {
            continue;
        }
        let workspace = std::path::Path::new(&item.workspace);
        if !state::has_retryable_webhook_event(workspace, &item.source, event_id)? {
            continue;
        }
        if candidate.is_some() {
            return Err("webhook event id is ambiguous; pass --workspace".into());
        }
        candidate = Some(item);
    }
    let Some(item) = candidate else {
        return Ok(false);
    };
    let workspace = std::path::Path::new(&item.workspace);
    if state::retry_webhook_event(workspace, &item.source, event_id)? {
        process_item(item, Some(event_id), None)?;
        return Ok(true);
    }
    Ok(false)
}

fn worker_loop(
    config: Arc<RwLock<Vec<WebhookItem>>>,
    wake_receiver: Receiver<WorkerCommand>,
    wake_sender: Sender<WorkerCommand>,
    shutdown: Arc<AtomicBool>,
    metrics: Arc<Metrics>,
) {
    let mut schedule = JobScheduler::new();
    let mut schedule_signature = Vec::new();
    let mut pending_tasks = BTreeMap::new();
    while !shutdown.load(Ordering::Relaxed) {
        let items = config.read().expect("webhook config lock poisoned").clone();
        let next_signature = items
            .iter()
            .map(|item| {
                (
                    item.task_id,
                    item.enabled,
                    serde_json::to_string(&item.item).unwrap_or_default(),
                )
            })
            .collect::<Vec<_>>();
        if next_signature != schedule_signature {
            schedule = JobScheduler::new();
            pending_tasks.clear();
            for item in items.iter().filter(|item| item.enabled) {
                match state::webhook_queue_stats(Path::new(&item.item.workspace), &item.item.source)
                {
                    Ok(stats) => {
                        if let Some(next_attempt_ms) = stats.next_attempt_ms {
                            pending_tasks.insert(item.task_id, next_attempt_ms);
                        }
                    }
                    Err(error) => eprintln!(
                        "webhook queue recovery failed for {}: {error}",
                        item.item.workspace
                    ),
                }
            }
            for item in &items {
                if !item.enabled {
                    continue;
                }
                let Some(crontab) = item.item.crontab.as_deref() else {
                    continue;
                };
                let Ok(crontab) = crontab.parse::<Schedule>() else {
                    eprintln!("invalid schedule for task {}", item.task_id);
                    continue;
                };
                let task_id = item.task_id;
                let wake_sender = wake_sender.clone();
                schedule.add(Job::new(crontab, move || {
                    let _ = wake_sender.send(WorkerCommand::Scheduled(task_id));
                }));
            }
            schedule_signature = next_signature;
        }
        schedule.tick();
        let now_ms = state::now_ms();
        let pending_ids = pending_tasks
            .iter()
            .filter(|(_, next_attempt_ms)| **next_attempt_ms <= now_ms)
            .map(|(task_id, _)| *task_id)
            .collect::<Vec<_>>();
        for task_id in pending_ids {
            let Some(item) = items
                .iter()
                .find(|item| item.task_id == task_id && item.enabled)
            else {
                pending_tasks.remove(&task_id);
                continue;
            };
            if let Err(error) = process_item(&item.item, None, Some(&metrics)) {
                eprintln!("webhook worker failed for {}: {error}", item.item.workspace);
            }
            pending_tasks.remove(&task_id);
            match state::webhook_queue_stats(Path::new(&item.item.workspace), &item.item.source) {
                Ok(stats) => {
                    if let Some(next_attempt_ms) = stats.next_attempt_ms {
                        pending_tasks.insert(task_id, next_attempt_ms);
                    }
                }
                Err(error) => eprintln!(
                    "webhook queue reschedule failed for {}: {error}",
                    item.item.workspace
                ),
            }
        }
        match wake_receiver.recv_timeout(Duration::from_millis(500)) {
            Ok(WorkerCommand::ConfigChanged) => {}
            Ok(WorkerCommand::Webhook(task_id)) => {
                pending_tasks.insert(task_id, 0);
            }
            Ok(WorkerCommand::Scheduled(task_id)) => {
                let Some(item) = items
                    .iter()
                    .find(|item| item.task_id == task_id && item.enabled)
                else {
                    continue;
                };
                match enqueue_scheduled_item(&item.item, &metrics) {
                    Ok(true) => {
                        pending_tasks.insert(task_id, 0);
                    }
                    Ok(false) => {}
                    Err(error) => eprintln!(
                        "scheduled event enqueue failed for {}: {error}",
                        item.item.workspace
                    ),
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn enqueue_scheduled_item(item: &Item, metrics: &Metrics) -> Result<bool, Box<dyn Error>> {
    match state::cooldown_active(
        Path::new(&item.workspace),
        &item.source,
        &item.target,
        item.failure_cooldown_secs,
    ) {
        Ok(true) => {
            eprintln!("sync {} paused by failure cooldown", item.workspace);
            return Ok(false);
        }
        Err(error) => eprintln!("sync {} cooldown check failed: {error}", item.workspace),
        Ok(false) => {}
    }
    let mut db = StateDb::open(Path::new(&item.workspace), &item.source)?;
    match db.enqueue_scheduled_event(
        &item.source,
        state::now_ms(),
        item.webhook_max_pending_events,
    )? {
        WebhookEnqueue::Enqueued => Ok(true),
        WebhookEnqueue::Full => {
            metrics.queue_full_events.fetch_add(1, Ordering::Relaxed);
            eprintln!(
                "scheduled sync {} skipped because the queue is full",
                item.workspace
            );
            Ok(false)
        }
        WebhookEnqueue::Duplicate => Ok(false),
    }
}

fn record_sync_metrics(metrics: &Metrics, started: Instant, result: &Result<(), Box<dyn Error>>) {
    metrics.sync_duration_ms_total.fetch_add(
        started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
        Ordering::Relaxed,
    );
    metrics.sync_duration_count.fetch_add(1, Ordering::Relaxed);
    match result {
        Ok(()) => metrics.successful_syncs.fetch_add(1, Ordering::Relaxed),
        Err(error) if error.to_string() == "sync cancelled" => {
            metrics.cancelled_syncs.fetch_add(1, Ordering::Relaxed)
        }
        Err(_) => metrics.failed_syncs.fetch_add(1, Ordering::Relaxed),
    };
}

fn should_coalesce_webhook_events(finished: bool, result_ok: bool, dry_run: bool) -> bool {
    finished && result_ok && !dry_run
}

fn process_item(
    item: &Item,
    event_id: Option<i64>,
    metrics: Option<&Metrics>,
) -> Result<(), Box<dyn Error>> {
    let workspace = std::path::Path::new(&item.workspace);
    let mut db = StateDb::open(workspace, &item.source)?;
    loop {
        let now = state::now_ms();
        let lease_ms =
            i64::try_from(item.webhook_event_lease_secs.saturating_mul(1000)).unwrap_or(i64::MAX);
        let claim = db.claim_webhook_event(&item.source, now, lease_ms, event_id)?;
        let Some(claimed) = claim else {
            return Ok(());
        };
        let claimed_id = claimed.event_id;
        let attempts = claimed.attempts;
        if !db.webhook_event_is_running(claimed_id)? {
            continue;
        }
        let heartbeat_interval_ms = (lease_ms / 3).clamp(100, 60_000);
        let (stop_heartbeat, heartbeat_stop) = mpsc::channel();
        let heartbeat_workspace = workspace.to_path_buf();
        let heartbeat_source = item.source.clone();
        let heartbeat_token = claimed.lease_token.clone();
        let heartbeat = thread::spawn(move || {
            let Ok(heartbeat_db) = StateDb::open(&heartbeat_workspace, &heartbeat_source) else {
                eprintln!("failed to open state database for webhook lease heartbeat");
                return;
            };
            let interval =
                Duration::from_millis(u64::try_from(heartbeat_interval_ms).unwrap_or(100));
            loop {
                match heartbeat_stop.recv_timeout(interval) {
                    Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        let lease_until = state::now_ms().saturating_add(lease_ms.max(1));
                        match heartbeat_db.renew_webhook_event(
                            claimed_id,
                            &heartbeat_token,
                            lease_until,
                        ) {
                            Ok(true) => {}
                            Ok(false) => break,
                            Err(error) => {
                                eprintln!(
                                    "failed to renew webhook event {claimed_id} lease: {error}"
                                );
                                break;
                            }
                        }
                    }
                }
            }
        });
        let started = Instant::now();
        let result = sync::sync(item);
        let _ = stop_heartbeat.send(());
        let _ = heartbeat.join();
        let error = result.as_ref().err().map(ToString::to_string);
        if let Some(metrics) = metrics {
            record_sync_metrics(metrics, started, &result);
        }
        let retry_after =
            state::now_ms().saturating_add(event_retry_delay(item.retry_backoff_secs, attempts));
        let finished = db.finish_webhook_event(
            &claimed,
            i64::from(item.max_retries) + 1,
            error.as_deref(),
            state::now_ms(),
            retry_after,
        )?;
        if should_coalesce_webhook_events(finished, result.is_ok(), item.dry_run) {
            // A successful non-dry-run full-state sync makes queued notifications redundant.
            let coalesced =
                db.coalesce_webhook_events(&item.source, claimed_id, state::now_ms())?;
            if let Some(metrics) = metrics {
                metrics
                    .coalesced_events
                    .fetch_add(coalesced as u64, Ordering::Relaxed);
            }
        }
        if event_id.is_some() {
            return result;
        }
    }
}

fn event_retry_delay(backoff_secs: u64, attempts: i64) -> i64 {
    // ponytail: reuse the existing retry knob and cap queue delays at five minutes.
    let multiplier = 1_u64
        .checked_shl(attempts.saturating_sub(1).min(31) as u32)
        .unwrap_or(u64::MAX);
    backoff_secs
        .saturating_mul(multiplier)
        .min(300)
        .saturating_mul(1000) as i64
}

fn handle_connection(
    mut stream: TcpStream,
    peer: SocketAddr,
    config_store: &RwLock<Vec<WebhookItem>>,
    database_path: &Path,
    wake_sender: &Sender<WorkerCommand>,
    metrics: &Metrics,
    login_limiter: &LoginLimiter,
) {
    metrics.http_requests.fetch_add(1, Ordering::Relaxed);
    let request = match read_request(&mut stream) {
        Ok(request) => request,
        Err(error) => {
            metrics.rejected_requests.fetch_add(1, Ordering::Relaxed);
            write_response(&mut stream, "400 Bad Request", error);
            return;
        }
    };
    let path = request.path.split('?').next().unwrap_or_default();
    if request.method == "GET" && path == "/healthz" {
        write_response(&mut stream, "200 OK", "ok");
        return;
    }
    if request.method == "GET" && path == "/readyz" {
        write_response(&mut stream, "200 OK", "ready");
        return;
    }
    if request.method == "GET" && path == "/metrics" {
        let config = config_store
            .read()
            .expect("webhook config lock poisoned")
            .clone();
        write_response_with_type(
            &mut stream,
            "200 OK",
            "text/plain; version=0.0.4; charset=utf-8",
            &metrics.render(&config),
        );
        return;
    }
    if request.method == "GET" && path == "/" {
        write_response_with_type(
            &mut stream,
            "200 OK",
            "text/html; charset=utf-8",
            dashboard::HTML,
        );
        return;
    }
    if path.starts_with("/api/auth/") {
        handle_auth_request(
            &mut stream,
            peer,
            database_path,
            &request,
            path,
            metrics,
            login_limiter,
        );
        return;
    }
    if path.starts_with("/api/") {
        let authenticated = session_token(&request.headers).is_some_and(|token| {
            tasks::authenticate_session(database_path, token)
                .ok()
                .flatten()
                .is_some()
        });
        if !authenticated {
            metrics.rejected_requests.fetch_add(1, Ordering::Relaxed);
            write_response(&mut stream, "401 Unauthorized", "login required");
            return;
        }
        match (request.method.as_str(), path) {
            ("GET", "/api/status") => {
                let config = config_store
                    .read()
                    .expect("webhook config lock poisoned")
                    .clone();
                match dashboard_snapshot(&config) {
                    Ok(body) => write_response_with_type(
                        &mut stream,
                        "200 OK",
                        "application/json; charset=utf-8",
                        &body,
                    ),
                    Err(error) => {
                        eprintln!("dashboard status failed: {error}");
                        write_response(
                            &mut stream,
                            "500 Internal Server Error",
                            "status unavailable",
                        );
                    }
                }
            }
            ("GET", "/api/tasks") => match tasks::list_tasks(database_path) {
                Ok(task_records) => match serde_json::to_string(&task_records) {
                    Ok(body) => write_response_with_type(
                        &mut stream,
                        "200 OK",
                        "application/json; charset=utf-8",
                        &body,
                    ),
                    Err(error) => {
                        eprintln!("dashboard task serialization failed: {error}");
                        write_response(
                            &mut stream,
                            "500 Internal Server Error",
                            "tasks unavailable",
                        );
                    }
                },
                Err(error) => {
                    eprintln!("dashboard task read failed: {error}");
                    write_response(
                        &mut stream,
                        "500 Internal Server Error",
                        "tasks unavailable",
                    );
                }
            },
            ("POST", "/api/tasks") => match serde_json::from_slice::<TaskRequest>(&request.body) {
                Ok(task_request) => match tasks::create_task(
                    database_path,
                    &task_request.item,
                    task_request.enabled,
                ) {
                    Ok(task) => match reload_task_config(database_path, config_store) {
                        Ok(()) => {
                            let _ = wake_sender.send(WorkerCommand::ConfigChanged);
                            match serde_json::to_string(&task) {
                                Ok(body) => write_response_with_type(
                                    &mut stream,
                                    "201 Created",
                                    "application/json; charset=utf-8",
                                    &body,
                                ),
                                Err(error) => {
                                    eprintln!("dashboard task serialization failed: {error}");
                                    write_response(
                                        &mut stream,
                                        "500 Internal Server Error",
                                        "task unavailable",
                                    );
                                }
                            }
                        }
                        Err(error) => {
                            eprintln!("dashboard task reload failed: {error}");
                            write_response(
                                &mut stream,
                                "500 Internal Server Error",
                                "task reload failed",
                            );
                        }
                    },
                    Err(error) => {
                        eprintln!("dashboard task create failed: {error}");
                        write_response(&mut stream, "400 Bad Request", "invalid task");
                    }
                },
                Err(error) => {
                    eprintln!("dashboard task request failed: {error}");
                    write_response(&mut stream, "400 Bad Request", "invalid task request");
                }
            },
            ("POST", task_path) if run_task_id_path(task_path).is_some() => {
                let task_id = run_task_id_path(task_path).expect("checked task id path");
                let task = config_store
                    .read()
                    .expect("webhook config lock poisoned")
                    .iter()
                    .find(|item| item.task_id == task_id)
                    .cloned();
                let Some(task) = task else {
                    write_response(&mut stream, "404 Not Found", "unknown task");
                    return;
                };
                if !task.enabled {
                    write_response(&mut stream, "409 Conflict", "task is disabled");
                    return;
                }
                let item = task.item;
                let mut db = match StateDb::open(Path::new(&item.workspace), &item.source) {
                    Ok(db) => db,
                    Err(error) => {
                        eprintln!("manual run state open failed: {error}");
                        write_response(&mut stream, "500 Internal Server Error", "run unavailable");
                        return;
                    }
                };
                match db.enqueue_manual_event(
                    &item.source,
                    state::now_ms(),
                    item.webhook_max_pending_events,
                ) {
                    Ok(WebhookEnqueue::Enqueued) => {
                        let _ = wake_sender.send(WorkerCommand::Webhook(task_id));
                        let body = format!(r#"{{"task_id":{task_id},"status":"queued"}}"#);
                        write_response_with_type(
                            &mut stream,
                            "202 Accepted",
                            "application/json; charset=utf-8",
                            &body,
                        );
                    }
                    Ok(WebhookEnqueue::Full) => {
                        write_response(&mut stream, "503 Service Unavailable", "task queue is full")
                    }
                    Ok(WebhookEnqueue::Duplicate) => write_response(
                        &mut stream,
                        "500 Internal Server Error",
                        "run enqueue failed",
                    ),
                    Err(error) => {
                        eprintln!("manual run enqueue failed: {error}");
                        write_response(
                            &mut stream,
                            "500 Internal Server Error",
                            "run enqueue failed",
                        );
                    }
                }
            }
            ("POST", task_path) if cancel_task_id_path(task_path).is_some() => {
                let task_id = cancel_task_id_path(task_path).expect("checked task id path");
                let item = config_store
                    .read()
                    .expect("webhook config lock poisoned")
                    .iter()
                    .find(|item| item.task_id == task_id)
                    .map(|item| item.item.clone());
                let Some(item) = item else {
                    write_response(&mut stream, "404 Not Found", "unknown task");
                    return;
                };
                let cancelled =
                    match state::cancel_webhook_events(Path::new(&item.workspace), &item.source) {
                        Ok(cancelled) => cancelled,
                        Err(error) => {
                            eprintln!("task cancellation failed: {error}");
                            write_response(
                                &mut stream,
                                "500 Internal Server Error",
                                "cancellation failed",
                            );
                            return;
                        }
                    };
                git::cancel_workspace(Path::new(&item.workspace));
                let body = format!(
                    r#"{{"task_id":{task_id},"status":"cancellation_requested","events":{cancelled}}}"#
                );
                write_response_with_type(
                    &mut stream,
                    "202 Accepted",
                    "application/json; charset=utf-8",
                    &body,
                );
            }
            ("POST", task_path) if retry_event_task_path(task_path).is_some() => {
                let (task_id, event_id) =
                    retry_event_task_path(task_path).expect("checked event retry path");
                let task = config_store
                    .read()
                    .expect("webhook config lock poisoned")
                    .iter()
                    .find(|item| item.task_id == task_id)
                    .cloned();
                let Some(task) = task else {
                    write_response(&mut stream, "404 Not Found", "unknown task");
                    return;
                };
                if !task.enabled {
                    write_response(&mut stream, "409 Conflict", "task is disabled");
                    return;
                }
                let item = task.item;
                match state::retry_webhook_event(Path::new(&item.workspace), &item.source, event_id)
                {
                    Ok(true) => {
                        let _ = wake_sender.send(WorkerCommand::Webhook(task_id));
                        let body = format!(
                            r#"{{"task_id":{task_id},"event_id":{event_id},"status":"queued"}}"#
                        );
                        write_response_with_type(
                            &mut stream,
                            "202 Accepted",
                            "application/json; charset=utf-8",
                            &body,
                        );
                    }
                    Ok(false) => {
                        write_response(&mut stream, "404 Not Found", "event not retryable")
                    }
                    Err(error) => {
                        eprintln!("event retry failed: {error}");
                        write_response(
                            &mut stream,
                            "500 Internal Server Error",
                            "event retry failed",
                        );
                    }
                }
            }
            ("PUT", task_path) => {
                let Some(task_id) = task_id_path(task_path) else {
                    metrics.rejected_requests.fetch_add(1, Ordering::Relaxed);
                    write_response(&mut stream, "404 Not Found", "unknown task");
                    return;
                };
                match serde_json::from_slice::<TaskRequest>(&request.body) {
                    Ok(task_request) => match tasks::update_task(
                        database_path,
                        task_id,
                        &task_request.item,
                        task_request.enabled,
                    ) {
                        Ok(Some(task)) => match reload_task_config(database_path, config_store) {
                            Ok(()) => {
                                let _ = wake_sender.send(WorkerCommand::ConfigChanged);
                                match serde_json::to_string(&task) {
                                    Ok(body) => write_response_with_type(
                                        &mut stream,
                                        "200 OK",
                                        "application/json; charset=utf-8",
                                        &body,
                                    ),
                                    Err(error) => {
                                        eprintln!("dashboard task serialization failed: {error}");
                                        write_response(
                                            &mut stream,
                                            "500 Internal Server Error",
                                            "task unavailable",
                                        );
                                    }
                                }
                            }
                            Err(error) => {
                                eprintln!("dashboard task reload failed: {error}");
                                write_response(
                                    &mut stream,
                                    "500 Internal Server Error",
                                    "task reload failed",
                                );
                            }
                        },
                        Ok(None) => write_response(&mut stream, "404 Not Found", "unknown task"),
                        Err(error) => {
                            eprintln!("dashboard task update failed: {error}");
                            write_response(&mut stream, "400 Bad Request", "invalid task");
                        }
                    },
                    Err(error) => {
                        eprintln!("dashboard task request failed: {error}");
                        write_response(&mut stream, "400 Bad Request", "invalid task request");
                    }
                }
            }
            ("DELETE", task_path) => {
                let Some(task_id) = task_id_path(task_path) else {
                    metrics.rejected_requests.fetch_add(1, Ordering::Relaxed);
                    write_response(&mut stream, "404 Not Found", "unknown task");
                    return;
                };
                match tasks::delete_task(database_path, task_id) {
                    Ok(true) => match reload_task_config(database_path, config_store) {
                        Ok(()) => {
                            let _ = wake_sender.send(WorkerCommand::ConfigChanged);
                            write_response(&mut stream, "204 No Content", "")
                        }
                        Err(error) => {
                            eprintln!("dashboard task reload failed: {error}");
                            write_response(
                                &mut stream,
                                "500 Internal Server Error",
                                "task reload failed",
                            );
                        }
                    },
                    Ok(false) => write_response(&mut stream, "404 Not Found", "unknown task"),
                    Err(error) => {
                        eprintln!("dashboard task delete failed: {error}");
                        write_response(
                            &mut stream,
                            "500 Internal Server Error",
                            "task delete failed",
                        );
                    }
                }
            }
            _ => {
                metrics.rejected_requests.fetch_add(1, Ordering::Relaxed);
                write_response(&mut stream, "404 Not Found", "unknown dashboard endpoint");
            }
        }
        return;
    }
    if request.method != "POST" {
        metrics.rejected_requests.fetch_add(1, Ordering::Relaxed);
        write_response(&mut stream, "405 Method Not Allowed", "POST required");
        return;
    }
    let config = config_store
        .read()
        .expect("webhook config lock poisoned")
        .clone();
    let event = match parse_event(&request.headers, &request.body) {
        Ok(event) => event,
        Err(error) => {
            metrics.rejected_requests.fetch_add(1, Ordering::Relaxed);
            write_response(&mut stream, error.status, error.message);
            return;
        }
    };
    let Some(event) = event else {
        metrics.ignored_events.fetch_add(1, Ordering::Relaxed);
        write_response(&mut stream, "202 Accepted", "event ignored");
        return;
    };
    let refs_json = match serde_json::to_string(&event.refs) {
        Ok(refs) => refs,
        Err(error) => {
            eprintln!("webhook ref serialization failed: {error}");
            write_response(&mut stream, "500 Internal Server Error", "event failed");
            return;
        }
    };
    let mut matched = false;
    let mut authenticated = false;
    let mut queue_full = false;
    for webhook_item in &config {
        if !webhook_item.enabled || webhook_item.secrets.is_empty() {
            continue;
        }
        if !item_matches(&webhook_item.item, &event) {
            continue;
        }
        matched = true;
        if !verify_event(
            &request.headers,
            &request.body,
            &event,
            &webhook_item.secrets,
        ) {
            continue;
        }
        authenticated = true;
        let item = &webhook_item.item;
        let mut db = match StateDb::open(std::path::Path::new(&item.workspace), &item.source) {
            Ok(db) => db,
            Err(error) => {
                eprintln!("webhook state open failed: {error}");
                write_response(&mut stream, "500 Internal Server Error", "event failed");
                return;
            }
        };
        match db.enqueue_webhook_event(
            WebhookEventInput {
                source: &item.source,
                provider: event.provider,
                delivery_id: &event.delivery_id,
                event_type: &event.event_type,
                refs_json: &refs_json,
                received_ms: state::now_ms(),
            },
            item.webhook_max_pending_events,
        ) {
            Ok(WebhookEnqueue::Enqueued) => {
                metrics.enqueued_events.fetch_add(1, Ordering::Relaxed);
                let _ = wake_sender.send(WorkerCommand::Webhook(webhook_item.task_id));
            }
            Ok(WebhookEnqueue::Duplicate) => {
                metrics.deduplicated_events.fetch_add(1, Ordering::Relaxed);
            }
            Ok(WebhookEnqueue::Full) => {
                metrics.queue_full_events.fetch_add(1, Ordering::Relaxed);
                metrics.rejected_requests.fetch_add(1, Ordering::Relaxed);
                eprintln!("webhook queue is full for {}", item.workspace);
                queue_full = true;
            }
            Err(error) => {
                eprintln!("webhook event enqueue failed: {error}");
                write_response(&mut stream, "500 Internal Server Error", "event failed");
                return;
            }
        }
    }
    if matched && !authenticated {
        metrics.rejected_requests.fetch_add(1, Ordering::Relaxed);
        let message = match event.provider {
            "github" => "invalid GitHub signature",
            "gitlab" => "invalid GitLab signature",
            _ => "invalid webhook signature",
        };
        write_response(&mut stream, "401 Unauthorized", message);
        return;
    }
    if queue_full {
        write_response(
            &mut stream,
            "503 Service Unavailable",
            "webhook queue is full",
        );
        return;
    }
    if matched {
        write_response(&mut stream, "202 Accepted", "sync queued");
    } else {
        metrics.ignored_events.fetch_add(1, Ordering::Relaxed);
        write_response(&mut stream, "202 Accepted", "event ignored");
    }
}

fn handle_auth_request(
    stream: &mut TcpStream,
    peer: SocketAddr,
    database_path: &Path,
    request: &HttpRequest,
    path: &str,
    metrics: &Metrics,
    login_limiter: &LoginLimiter,
) {
    match (request.method.as_str(), path) {
        ("GET", "/api/auth/status") => match tasks::auth_initialized(database_path) {
            Ok(initialized) => write_response_with_type(
                stream,
                "200 OK",
                "application/json; charset=utf-8",
                &format!(r#"{{"initialized":{initialized}}}"#),
            ),
            Err(error) => {
                eprintln!("auth status failed: {error}");
                write_response(stream, "500 Internal Server Error", "auth unavailable");
            }
        },
        ("POST", "/api/auth/setup") => {
            if matches!(tasks::auth_initialized(database_path), Ok(true)) {
                write_response(
                    stream,
                    "409 Conflict",
                    "admin account is already initialized",
                );
                return;
            }
            let credentials = match serde_json::from_slice::<AuthRequest>(&request.body) {
                Ok(credentials) => credentials,
                Err(error) => {
                    eprintln!("auth setup request failed: {error}");
                    write_response(stream, "400 Bad Request", "invalid credentials");
                    return;
                }
            };
            match tasks::setup_admin(database_path, &credentials.username, &credentials.password) {
                Ok(session) => write_session_response(stream, "201 Created", &session),
                Err(error) => {
                    eprintln!("auth setup failed: {error}");
                    write_response(stream, "400 Bad Request", "invalid credentials");
                }
            }
        }
        ("POST", "/api/auth/login") => {
            if !login_limiter.allowed(peer.ip()) {
                metrics.auth_rate_limited.fetch_add(1, Ordering::Relaxed);
                write_response_with_headers(
                    stream,
                    "429 Too Many Requests",
                    "text/plain; charset=utf-8",
                    "too many login attempts",
                    "Retry-After: 300\r\n",
                );
                return;
            }
            let credentials = match serde_json::from_slice::<AuthRequest>(&request.body) {
                Ok(credentials) => credentials,
                Err(error) => {
                    eprintln!("auth login request failed: {error}");
                    login_limiter.record_failure(peer.ip());
                    metrics.auth_failures.fetch_add(1, Ordering::Relaxed);
                    write_response(stream, "400 Bad Request", "invalid credentials");
                    return;
                }
            };
            match tasks::login_admin(database_path, &credentials.username, &credentials.password) {
                Ok(session) => {
                    login_limiter.record_success(peer.ip());
                    write_session_response(stream, "200 OK", &session);
                }
                Err(_) => {
                    login_limiter.record_failure(peer.ip());
                    metrics.auth_failures.fetch_add(1, Ordering::Relaxed);
                    write_response(stream, "401 Unauthorized", "invalid credentials");
                }
            }
        }
        ("POST", "/api/auth/password") => {
            let Some(token) = session_token(&request.headers) else {
                write_response(stream, "401 Unauthorized", "login required");
                return;
            };
            let authenticated = tasks::authenticate_session(database_path, token)
                .ok()
                .flatten()
                .is_some();
            if !authenticated {
                write_response(stream, "401 Unauthorized", "login required");
                return;
            }
            let credentials = match serde_json::from_slice::<PasswordChangeRequest>(&request.body) {
                Ok(credentials) => credentials,
                Err(error) => {
                    eprintln!("auth password request failed: {error}");
                    write_response(stream, "400 Bad Request", "invalid password request");
                    return;
                }
            };
            match tasks::change_password(
                database_path,
                token,
                &credentials.current_password,
                &credentials.new_password,
            ) {
                Ok(session) => write_session_response(stream, "200 OK", &session),
                Err(_) => write_response(stream, "400 Bad Request", "invalid password request"),
            }
        }
        ("POST", "/api/auth/logout") => {
            if let Some(token) = session_token(&request.headers) {
                if let Err(error) = tasks::logout_session(database_path, token) {
                    eprintln!("auth logout failed: {error}");
                }
            }
            write_response_with_cookie(
                stream,
                "204 No Content",
                "text/plain; charset=utf-8",
                "",
                "repo_sync_session=; Max-Age=0; HttpOnly; SameSite=Strict; Path=/",
            );
        }
        _ => {
            metrics.rejected_requests.fetch_add(1, Ordering::Relaxed);
            write_response(stream, "404 Not Found", "unknown auth endpoint");
        }
    }
}

fn session_token(headers: &BTreeMap<String, String>) -> Option<&str> {
    headers
        .get("cookie")?
        .split(';')
        .map(str::trim)
        .find_map(|cookie| cookie.strip_prefix("repo_sync_session="))
        .filter(|token| !token.is_empty())
}

fn write_session_response(stream: &mut TcpStream, status: &str, session: &tasks::LoginSession) {
    let body = format!(
        r#"{{"username":{}}}"#,
        serde_json::to_string(&session.username).unwrap_or_else(|_| "\"admin\"".into())
    );
    let cookie = format!(
        "repo_sync_session={}; Max-Age={}; HttpOnly; SameSite=Strict; Path=/",
        session.token,
        12 * 60 * 60
    );
    write_response_with_cookie(
        stream,
        status,
        "application/json; charset=utf-8",
        &body,
        &cookie,
    );
}

fn read_request(stream: &mut TcpStream) -> Result<HttpRequest, &'static str> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|_| "request setup failed")?;
    let mut request = Vec::new();
    let mut buffer = [0; 8192];
    let header_end = loop {
        if request.len() > MAX_HEADER_BYTES {
            return Err("request headers too large");
        }
        let Some(offset) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
            let size = stream
                .read(&mut buffer)
                .map_err(|_| "request read failed")?;
            if size == 0 {
                return Err("incomplete request");
            }
            request.extend_from_slice(&buffer[..size]);
            continue;
        };
        break offset + 4;
    };
    let header_text =
        std::str::from_utf8(&request[..header_end - 4]).map_err(|_| "invalid headers")?;
    let mut lines = header_text.split("\r\n");
    let request_line = lines.next().ok_or("missing request line")?;
    let mut request_fields = request_line.split_whitespace();
    let method = request_fields.next().ok_or("missing method")?.to_owned();
    let path = request_fields.next().ok_or("missing path")?.to_owned();
    let mut headers = BTreeMap::new();
    for line in lines {
        let (name, value) = line.split_once(':').ok_or("invalid header")?;
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
    }
    let body_length = headers
        .get("content-length")
        .map(|value| value.parse::<usize>().map_err(|_| "invalid content length"))
        .transpose()?
        .unwrap_or(0);
    if body_length > MAX_BODY_BYTES {
        return Err("request body too large");
    }
    let required = header_end
        .checked_add(body_length)
        .ok_or("request too large")?;
    while request.len() < required {
        let size = stream
            .read(&mut buffer)
            .map_err(|_| "request read failed")?;
        if size == 0 {
            return Err("incomplete request body");
        }
        request.extend_from_slice(&buffer[..size]);
        if request.len() > required {
            break;
        }
    }
    Ok(HttpRequest {
        method,
        path,
        headers,
        body: request[header_end..required].to_vec(),
    })
}

fn parse_event(
    headers: &BTreeMap<String, String>,
    body: &[u8],
) -> Result<Option<WebhookEvent>, RequestError> {
    if let Some(event_type) = headers.get("x-github-event") {
        let delivery_id = headers
            .get("x-github-delivery")
            .filter(|value| !value.is_empty())
            .ok_or(RequestError {
                status: "400 Bad Request",
                message: "missing GitHub delivery id",
            })?
            .clone();
        return parse_github_event(event_type, delivery_id, body);
    }
    if let Some(event_type) = headers.get("x-gitlab-event") {
        let delivery_id = headers
            .get("webhook-id")
            .or_else(|| headers.get("idempotency-key"))
            .or_else(|| headers.get("x-gitlab-event-uuid"))
            .filter(|value| !value.is_empty())
            .ok_or(RequestError {
                status: "400 Bad Request",
                message: "missing GitLab delivery id",
            })?
            .clone();
        return parse_gitlab_event(event_type, delivery_id, body);
    }
    Err(RequestError {
        status: "400 Bad Request",
        message: "unsupported webhook provider",
    })
}

fn verify_event(
    headers: &BTreeMap<String, String>,
    body: &[u8],
    event: &WebhookEvent,
    secrets: &[String],
) -> bool {
    match event.provider {
        "github" => secrets
            .iter()
            .any(|secret| verify_github_signature(headers, body, secret)),
        "gitlab" => secrets
            .iter()
            .any(|secret| verify_gitlab_signature(headers, body, secret, state::now_ms() / 1000)),
        _ => false,
    }
}

fn parse_github_event(
    event_type: &str,
    delivery_id: String,
    body: &[u8],
) -> Result<Option<WebhookEvent>, RequestError> {
    if !matches!(event_type, "push" | "delete") {
        return Ok(None);
    }
    let payload: Value = serde_json::from_slice(body).map_err(|_| RequestError {
        status: "400 Bad Request",
        message: "invalid GitHub JSON payload",
    })?;
    let repository_keys = github_repository_keys(&payload);
    if repository_keys.is_empty() {
        return Err(RequestError {
            status: "400 Bad Request",
            message: "GitHub payload has no repository",
        });
    }
    let reference = if event_type == "delete" {
        let ref_name = string_at(&payload, &["ref"]).ok_or(RequestError {
            status: "400 Bad Request",
            message: "GitHub delete payload has no ref",
        })?;
        let ref_type = string_at(&payload, &["ref_type"]).ok_or(RequestError {
            status: "400 Bad Request",
            message: "GitHub delete payload has no ref_type",
        })?;
        github_ref(ref_type, ref_name)
    } else {
        string_at(&payload, &["ref"])
            .ok_or(RequestError {
                status: "400 Bad Request",
                message: "GitHub push payload has no ref",
            })?
            .to_owned()
    };
    let reference = supported_ref(&reference).ok_or(RequestError {
        status: "202 Accepted",
        message: "event ignored",
    })?;
    let deleted = event_type == "delete"
        || bool_at(&payload, &["deleted"])
        || string_at(&payload, &["after"]).is_some_and(is_zero_sha);
    let new_sha = (!deleted)
        .then(|| string_at(&payload, &["after"]).map(str::to_owned))
        .flatten();
    Ok(Some(WebhookEvent {
        provider: "github",
        delivery_id,
        event_type: event_type.to_owned(),
        repository_keys,
        refs: vec![WebhookRefChange {
            reference,
            deleted,
            new_sha,
        }],
    }))
}

fn parse_gitlab_event(
    event_type: &str,
    delivery_id: String,
    body: &[u8],
) -> Result<Option<WebhookEvent>, RequestError> {
    if !matches!(event_type, "Push Hook" | "Tag Push Hook") {
        return Ok(None);
    }
    let payload: Value = serde_json::from_slice(body).map_err(|_| RequestError {
        status: "400 Bad Request",
        message: "invalid GitLab JSON payload",
    })?;
    let repository_keys = gitlab_repository_keys(&payload);
    if repository_keys.is_empty() {
        return Err(RequestError {
            status: "400 Bad Request",
            message: "GitLab payload has no project",
        });
    }
    let reference = string_at(&payload, &["ref"]).ok_or(RequestError {
        status: "400 Bad Request",
        message: "GitLab payload has no ref",
    })?;
    let reference = supported_ref(reference).ok_or(RequestError {
        status: "202 Accepted",
        message: "event ignored",
    })?;
    let after = string_at(&payload, &["after"]);
    let deleted = after.is_some_and(is_zero_sha);
    Ok(Some(WebhookEvent {
        provider: "gitlab",
        delivery_id,
        event_type: event_type.to_owned(),
        repository_keys,
        refs: vec![WebhookRefChange {
            reference,
            deleted,
            new_sha: (!deleted).then(|| after.map(str::to_owned)).flatten(),
        }],
    }))
}

fn verify_github_signature(headers: &BTreeMap<String, String>, body: &[u8], secret: &str) -> bool {
    let Some(signature) = headers.get("x-hub-signature-256") else {
        return false;
    };
    let Some(signature) = signature.strip_prefix("sha256=") else {
        return false;
    };
    let Some(received) = decode_hex(signature) else {
        return false;
    };
    let Ok(mut mac) = HmacSha256::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&received).is_ok()
}

fn verify_gitlab_signature(
    headers: &BTreeMap<String, String>,
    body: &[u8],
    secret: &str,
    now_secs: i64,
) -> bool {
    if let Some(signature) = headers.get("webhook-signature") {
        let Some(webhook_id) = headers.get("webhook-id") else {
            return false;
        };
        let Some(timestamp) = headers
            .get("webhook-timestamp")
            .and_then(|value| value.parse::<i64>().ok())
        else {
            return false;
        };
        let lower_bound = now_secs.saturating_sub(SIGNATURE_TOLERANCE_SECS);
        let upper_bound = now_secs.saturating_add(SIGNATURE_TOLERANCE_SECS);
        if !(lower_bound..=upper_bound).contains(&timestamp) {
            return false;
        }
        let Some(key) = secret
            .strip_prefix("whsec_")
            .and_then(|value| STANDARD.decode(value).ok())
        else {
            return false;
        };
        let Ok(mut mac) = HmacSha256::new_from_slice(&key) else {
            return false;
        };
        mac.update(webhook_id.as_bytes());
        mac.update(b".");
        mac.update(timestamp.to_string().as_bytes());
        mac.update(b".");
        mac.update(body);
        let expected = format!("v1,{}", STANDARD.encode(mac.finalize().into_bytes()));
        return signature
            .split_whitespace()
            .any(|value| constant_time_equal(value.as_bytes(), expected.as_bytes()));
    }
    headers
        .get("x-gitlab-token")
        .is_some_and(|token| constant_time_equal(token.as_bytes(), secret.as_bytes()))
}

fn item_matches(item: &Item, event: &WebhookEvent) -> bool {
    let source_keys = repository_keys(&item.source);
    let repository_matches = event
        .repository_keys
        .iter()
        .any(|event_key| source_keys.iter().any(|source_key| source_key == event_key));
    repository_matches
        && event.refs.iter().any(|change| {
            if let Some(branch) = change.reference.strip_prefix("refs/heads/") {
                config::branch_selected(&item.branches, branch)
                    && config::ref_selected(
                        &item.include_refs,
                        &item.exclude_refs,
                        &change.reference,
                    )
            } else {
                config::ref_selected(&item.include_refs, &item.exclude_refs, &change.reference)
            }
        })
}

fn github_repository_keys(payload: &Value) -> Vec<String> {
    ["clone_url", "ssh_url", "git_url", "html_url", "full_name"]
        .iter()
        .filter_map(|field| string_at(payload, &["repository", field]))
        .flat_map(repository_keys)
        .collect()
}

fn gitlab_repository_keys(payload: &Value) -> Vec<String> {
    [
        "git_http_url",
        "git_ssh_url",
        "web_url",
        "path_with_namespace",
    ]
    .iter()
    .filter_map(|field| string_at(payload, &["project", field]))
    .flat_map(repository_keys)
    .collect()
}

fn repository_keys(value: &str) -> Vec<String> {
    let value = value.trim().trim_end_matches('/').to_ascii_lowercase();
    let value = value.strip_suffix(".git").unwrap_or(&value);
    let mut keys = Vec::new();
    if let Some((_, rest)) = value.split_once("://") {
        let mut parts = rest.splitn(2, '/');
        let host = parts
            .next()
            .unwrap_or_default()
            .rsplit('@')
            .next()
            .unwrap_or_default();
        let path = parts.next().unwrap_or_default();
        if !host.is_empty() && !path.is_empty() {
            keys.push(format!("{host}/{path}"));
        }
    } else if let Some((user_host, path)) = value.split_once(':') {
        let host = user_host.rsplit('@').next().unwrap_or(user_host);
        if !host.is_empty() && !path.is_empty() {
            keys.push(format!("{host}/{path}"));
        }
    } else {
        keys.push(value.trim_start_matches('/').to_owned());
    }
    keys.sort();
    keys.dedup();
    keys
}

fn github_ref(ref_type: &str, name: &str) -> String {
    match ref_type {
        "branch" => format!("refs/heads/{name}"),
        "tag" => format!("refs/tags/{name}"),
        _ => name.to_owned(),
    }
}

fn supported_ref(reference: &str) -> Option<String> {
    (reference.starts_with("refs/heads/") || reference.starts_with("refs/tags/"))
        .then(|| reference.to_owned())
}

fn string_at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut current = value;
    for field in path {
        current = current.get(*field)?;
    }
    current.as_str()
}

fn bool_at(value: &Value, path: &[&str]) -> bool {
    let mut current = value;
    for field in path {
        let Some(next) = current.get(*field) else {
            return false;
        };
        current = next;
    }
    current.as_bool().unwrap_or(false)
}

fn is_zero_sha(value: &str) -> bool {
    !value.is_empty() && value.chars().all(|character| character == '0')
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return None;
    }
    (0..value.len())
        .step_by(2)
        .map(|index| {
            let high = value.as_bytes()[index].to_ascii_lowercase();
            let low = value.as_bytes()[index + 1].to_ascii_lowercase();
            Some((hex_digit(high)? << 4) | hex_digit(low)?)
        })
        .collect()
}

fn hex_digit(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        _ => None,
    }
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

fn write_response(stream: &mut TcpStream, status: &str, body: &str) {
    write_response_with_type(stream, status, "text/plain; charset=utf-8", body);
}

fn write_response_with_type(stream: &mut TcpStream, status: &str, content_type: &str, body: &str) {
    write_response_with_cookie(stream, status, content_type, body, "");
}

fn write_response_with_headers(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
    headers: &str,
) {
    write_response_with_cookie_and_headers(stream, status, content_type, body, "", headers);
}

fn write_response_with_cookie(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
    cookie: &str,
) {
    write_response_with_cookie_and_headers(stream, status, content_type, body, cookie, "");
}

fn write_response_with_cookie_and_headers(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
    cookie: &str,
    headers: &str,
) {
    let cookie_header = if cookie.is_empty() {
        String::new()
    } else {
        format!("Set-Cookie: {cookie}\r\n")
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\n{SECURITY_HEADERS}{headers}Cache-Control: no-store\r\n{cookie_header}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len(),
    );
    let _ = stream.write_all(response.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::{
        escape_label, parse_event, should_coalesce_webhook_events, verify_event,
        verify_github_signature, verify_gitlab_signature, LoginLimiter, Metrics,
    };
    use crate::{DivergencePolicy, Item, SyncMode, TagPolicy};
    use base64::{engine::general_purpose::STANDARD, Engine};
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    use std::{
        collections::BTreeMap,
        fs,
        net::{IpAddr, Ipv4Addr},
        path::Path,
        sync::{
            atomic::{AtomicBool, Ordering},
            mpsc, Arc, RwLock,
        },
        thread,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    type HmacSha256 = Hmac<Sha256>;

    #[test]
    fn dry_run_does_not_coalesce_webhook_events() {
        assert!(!should_coalesce_webhook_events(true, true, true));
        assert!(should_coalesce_webhook_events(true, true, false));
        assert!(!should_coalesce_webhook_events(false, true, false));
    }

    #[test]
    fn limits_repeated_login_failures_per_address() {
        let limiter = LoginLimiter::default();
        let address = IpAddr::V4(Ipv4Addr::LOCALHOST);
        for _ in 0..5 {
            assert!(limiter.allowed(address));
            limiter.record_failure(address);
        }
        assert!(!limiter.allowed(address));
        limiter.record_success(address);
        assert!(limiter.allowed(address));
    }

    #[test]
    fn parses_github_push_and_delete() {
        let body = br#"{"ref":"refs/heads/main","after":"abc","deleted":false,"repository":{"full_name":"org/repo"}}"#;
        let mut headers = BTreeMap::from([
            ("x-github-event".into(), "push".into()),
            ("x-github-delivery".into(), "delivery-1".into()),
        ]);
        let mut mac = HmacSha256::new_from_slice(b"secret").unwrap();
        mac.update(body);
        let signature = mac.finalize().into_bytes();
        headers.insert(
            "x-hub-signature-256".into(),
            format!(
                "sha256={}",
                signature
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            ),
        );
        let event = parse_event(&headers, body).unwrap().unwrap();
        assert!(verify_event(&headers, body, &event, &["secret".into()]));
        assert_eq!(event.provider, "github");
        assert_eq!(event.refs[0].reference, "refs/heads/main");
        assert!(!event.refs[0].deleted);

        let delete_body =
            br#"{"ref":"main","ref_type":"branch","repository":{"full_name":"org/repo"}}"#;
        headers.insert("x-github-event".into(), "delete".into());
        let mut mac = HmacSha256::new_from_slice(b"secret").unwrap();
        mac.update(delete_body);
        let signature = mac.finalize().into_bytes();
        headers.insert(
            "x-hub-signature-256".into(),
            format!(
                "sha256={}",
                signature
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            ),
        );
        let event = parse_event(&headers, delete_body).unwrap().unwrap();
        assert!(verify_event(
            &headers,
            delete_body,
            &event,
            &["secret".into()]
        ));
        assert_eq!(event.refs[0].reference, "refs/heads/main");
        assert!(event.refs[0].deleted);
    }

    #[test]
    fn parses_gitlab_push_and_tag_delete_with_token() {
        let body = br#"{"ref":"refs/heads/main","before":"0","after":"abc","project":{"path_with_namespace":"org/repo"}}"#;
        let headers = BTreeMap::from([
            ("x-gitlab-event".into(), "Push Hook".into()),
            ("x-gitlab-token".into(), "secret".into()),
            ("webhook-id".into(), "delivery-2".into()),
        ]);
        let event = parse_event(&headers, body).unwrap().unwrap();
        assert!(verify_event(&headers, body, &event, &["secret".into()]));
        assert_eq!(event.provider, "gitlab");
        assert_eq!(event.refs[0].reference, "refs/heads/main");

        let delete_body = br#"{"ref":"refs/tags/v1","before":"abc","after":"0000000000000000000000000000000000000000","project":{"path_with_namespace":"org/repo"}}"#;
        let mut headers = headers;
        headers.insert("x-gitlab-event".into(), "Tag Push Hook".into());
        let event = parse_event(&headers, delete_body).unwrap().unwrap();
        assert!(verify_event(
            &headers,
            delete_body,
            &event,
            &["secret".into()]
        ));
        assert_eq!(event.refs[0].reference, "refs/tags/v1");
        assert!(event.refs[0].deleted);
    }

    #[test]
    fn parses_gitlab_signed_delivery() {
        let body = br#"{"ref":"refs/heads/main","after":"abc","project":{"path_with_namespace":"org/repo"}}"#;
        let key = [7_u8; 32];
        let secret = format!("whsec_{}", STANDARD.encode(key));
        let webhook_id = "delivery-signed";
        let timestamp = super::state::now_ms() / 1000;
        let mut mac = HmacSha256::new_from_slice(&key).unwrap();
        mac.update(webhook_id.as_bytes());
        mac.update(b".");
        mac.update(timestamp.to_string().as_bytes());
        mac.update(b".");
        mac.update(body);
        let headers = BTreeMap::from([
            ("x-gitlab-event".into(), "Push Hook".into()),
            ("webhook-id".into(), webhook_id.into()),
            ("webhook-timestamp".into(), timestamp.to_string()),
            (
                "webhook-signature".into(),
                format!("v1,{}", STANDARD.encode(mac.finalize().into_bytes())),
            ),
        ]);
        let event = parse_event(&headers, body).unwrap().unwrap();
        assert!(verify_event(&headers, body, &event, &[secret]));
        assert_eq!(event.provider, "gitlab");
        assert_eq!(event.delivery_id, webhook_id);
    }

    #[test]
    fn rejects_bad_github_signature() {
        let body = br#"{}"#;
        let headers = BTreeMap::from([
            ("x-github-event".into(), "push".into()),
            ("x-github-delivery".into(), "delivery-3".into()),
            ("x-hub-signature-256".into(), "sha256=00".into()),
        ]);
        assert!(!verify_github_signature(&headers, body, "secret"));
    }

    #[test]
    fn rejects_extreme_gitlab_timestamp_without_panicking() {
        let headers = BTreeMap::from([
            ("webhook-signature".into(), "v1,invalid".into()),
            ("webhook-id".into(), "delivery-extreme".into()),
            ("webhook-timestamp".into(), i64::MIN.to_string()),
        ]);
        assert!(!verify_gitlab_signature(
            &headers,
            b"{}",
            "whsec_invalid",
            1_700_000_000
        ));
    }

    #[test]
    fn worker_recovers_pending_events_and_retries_failures() {
        let root = std::env::temp_dir().join(format!(
            "repo-sync-worker-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let existing_workspace = root.join("existing-workspace");
        let missing_workspace = root.join("missing-workspace");
        fs::create_dir_all(&existing_workspace).unwrap();

        let item = |workspace: &Path| Item {
            source: "source".into(),
            target: vec!["target".into()],
            workspace: workspace.to_string_lossy().into_owned(),
            mode: SyncMode::Branch,
            crontab: None,
            branches: Vec::new(),
            include_refs: Vec::new(),
            exclude_refs: Vec::new(),
            timeout_secs: 1,
            dry_run: true,
            allow_destructive: false,
            sync_lfs: false,
            divergence: DivergencePolicy::Fail,
            tag_policy: TagPolicy::Preserve,
            prune_branches: false,
            prune_tags: false,
            atomic: true,
            max_retries: 1,
            retry_backoff_secs: 1,
            failure_cooldown_secs: 0,
            webhook_secret_envs: Vec::new(),
            webhook_max_pending_events: 10,
            webhook_event_lease_secs: 1,
        };
        let existing_item = item(&existing_workspace);
        let missing_item = item(&missing_workspace);
        for value in [&existing_item, &missing_item] {
            let mut db =
                super::state::StateDb::open(Path::new(&value.workspace), &value.source).unwrap();
            assert!(matches!(
                db.enqueue_manual_event(&value.source, super::state::now_ms(), 10)
                    .unwrap(),
                super::state::WebhookEnqueue::Enqueued
            ));
        }

        let config = Arc::new(RwLock::new(vec![
            super::WebhookItem {
                task_id: 1,
                enabled: true,
                item: existing_item.clone(),
                secrets: Vec::new(),
            },
            super::WebhookItem {
                task_id: 2,
                enabled: true,
                item: missing_item.clone(),
                secrets: Vec::new(),
            },
        ]));
        let (wake_sender, wake_receiver) = mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = Arc::clone(&shutdown);
        let metrics = Arc::new(Metrics::default());
        let worker = thread::spawn(move || {
            super::worker_loop(config, wake_receiver, wake_sender, worker_shutdown, metrics)
        });

        thread::sleep(Duration::from_millis(2_500));
        shutdown.store(true, Ordering::Relaxed);
        let _ = worker.join();

        let existing_events = super::state::webhook_events(
            Path::new(&existing_item.workspace),
            &existing_item.source,
            10,
        )
        .unwrap();
        assert!(existing_events[0].attempts >= 2);
        let missing_events = super::state::webhook_events(
            Path::new(&missing_item.workspace),
            &missing_item.source,
            10,
        )
        .unwrap();
        assert!(missing_events[0].attempts >= 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn renders_prometheus_metrics_and_escapes_labels() {
        let metrics = Metrics::default();
        metrics.http_requests.fetch_add(3, Ordering::Relaxed);
        let output = metrics.render(&[]);
        assert!(output.contains("# TYPE repo_sync_http_requests_total counter"));
        assert!(output.contains("repo_sync_http_requests_total 3"));
        assert_eq!(escape_label("a\\b\"c\nd"), "a\\\\b\\\"c\\nd");
    }

    #[test]
    fn repository_matching_keeps_hosts_distinct() {
        assert_eq!(
            super::repository_keys("https://github.com/org/repo.git"),
            vec!["github.com/org/repo"]
        );
        assert_eq!(
            super::repository_keys("git@gitlab.com:org/repo.git"),
            vec!["gitlab.com/org/repo"]
        );
        assert_eq!(
            super::repository_keys("ssh://git@github.com/org/repo.git"),
            vec!["github.com/org/repo"]
        );
        assert_ne!(
            super::repository_keys("https://github.com/org/repo.git"),
            super::repository_keys("https://gitlab.com/org/repo.git")
        );
    }
}
