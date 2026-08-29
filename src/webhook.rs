use crate::{
    config,
    state::{self, QueuedEvent, StateDb, WebhookRefChange},
    sync, Item,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;
use std::{
    collections::BTreeMap,
    error::Error,
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
        Arc, RwLock,
    },
    thread,
    time::{Duration, Instant},
};

type HmacSha256 = Hmac<Sha256>;

const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;
const MAX_ACTIVE_CONNECTIONS: u64 = 64;
const SIGNATURE_TOLERANCE_SECS: i64 = 300;
const EVENT_RECOVERY_MS: i64 = 10 * 60 * 1000;

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
    item: Item,
    secrets: Vec<String>,
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
    successful_syncs: AtomicU64,
    failed_syncs: AtomicU64,
    sync_duration_ms_total: AtomicU64,
    sync_duration_count: AtomicU64,
    collection_errors: AtomicU64,
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

pub fn serve(
    addr: &str,
    config: Vec<Item>,
    config_path: Option<&str>,
    direct_secret: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let config = Arc::new(RwLock::new(prepare_webhook_config(&config, direct_secret)?));
    let listener = TcpListener::bind(addr)?;
    listener.set_nonblocking(true)?;
    let metrics = Arc::new(Metrics::default());
    let (wake_sender, wake_receiver) = mpsc::channel();
    let shutdown = Arc::new(AtomicBool::new(false));
    let reload = register_reload_flag()?;
    let worker_shutdown = Arc::clone(&shutdown);
    let worker_config = Arc::clone(&config);
    let worker_metrics = Arc::clone(&metrics);
    let worker = thread::spawn(move || {
        worker_loop(
            worker_config,
            wake_receiver,
            worker_shutdown,
            worker_metrics,
        )
    });
    let shutdown_handler = Arc::clone(&shutdown);
    ctrlc::set_handler(move || shutdown_handler.store(true, Ordering::Relaxed))?;

    eprintln!("webhook listener started on {addr}");
    while !shutdown.load(Ordering::Relaxed) {
        if reload.swap(false, Ordering::Relaxed) {
            if let Some(path) = config_path {
                match config::load(path)
                    .and_then(|new_config| prepare_webhook_config(&new_config, None))
                {
                    Ok(new_config) => {
                        *config.write().expect("webhook config lock poisoned") = new_config;
                        eprintln!("webhook configuration reloaded from {path}");
                    }
                    Err(error) => eprintln!("webhook configuration reload failed: {error}"),
                }
            } else {
                eprintln!("SIGHUP ignored: direct webhook mode has no config file");
            }
        }
        match listener.accept() {
            Ok((stream, _)) => {
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
                let metrics = Arc::clone(&metrics);
                let wake_sender = wake_sender.clone();
                thread::spawn(move || {
                    handle_connection(stream, &config, &wake_sender, &metrics);
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

fn prepare_webhook_config(
    config_items: &[Item],
    direct_secret: Option<&str>,
) -> Result<Vec<WebhookItem>, Box<dyn Error>> {
    config::validate(config_items)?;
    config_items
        .iter()
        .map(|item| {
            let secrets = if let Some(secret) = direct_secret {
                if secret.is_empty() {
                    return Err("webhook secret cannot be empty".into());
                }
                vec![secret.to_owned()]
            } else {
                let mut secrets = Vec::new();
                for env_name in &item.webhook_secret_envs {
                    if env_name.trim().is_empty() {
                        return Err("webhook_secret_envs cannot contain an empty name".into());
                    }
                    let secret = std::env::var(env_name).map_err(|_| {
                        format!("webhook secret environment variable is not set: {env_name}")
                    })?;
                    if secret.is_empty() {
                        return Err(format!(
                            "webhook secret environment variable is empty: {env_name}"
                        )
                        .into());
                    }
                    if !secrets.contains(&secret) {
                        secrets.push(secret);
                    }
                }
                if secrets.is_empty() {
                    return Err(format!(
                        "webhook_secret_envs is required for workspace {}",
                        item.workspace
                    )
                    .into());
                }
                secrets
            };
            let db = StateDb::open(std::path::Path::new(&item.workspace), &item.source)?;
            let now = state::now_ms();
            db.recover_webhook_events(&item.source, now.saturating_sub(EVENT_RECOVERY_MS), now)?;
            Ok(WebhookItem {
                item: item.clone(),
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
    wake_receiver: Receiver<()>,
    shutdown: Arc<AtomicBool>,
    metrics: Arc<Metrics>,
) {
    while !shutdown.load(Ordering::Relaxed) {
        let items = config.read().expect("webhook config lock poisoned").clone();
        for item in &items {
            if let Err(error) = process_item(&item.item, None, Some(&metrics)) {
                eprintln!("webhook worker failed for {}: {error}", item.item.workspace);
            }
        }
        match wake_receiver.recv_timeout(Duration::from_millis(500)) {
            Ok(()) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn process_item(
    item: &Item,
    event_id: Option<i64>,
    metrics: Option<&Metrics>,
) -> Result<(), Box<dyn Error>> {
    let workspace = std::path::Path::new(&item.workspace);
    let mut db = StateDb::open(workspace, &item.source)?;
    loop {
        let claim = db.claim_webhook_event(&item.source, state::now_ms(), event_id)?;
        let Some(QueuedEvent {
            event_id: claimed_id,
            attempts,
        }) = claim
        else {
            return Ok(());
        };
        let started = Instant::now();
        let result = sync::sync(item);
        if let Some(metrics) = metrics {
            metrics.sync_duration_ms_total.fetch_add(
                started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
                Ordering::Relaxed,
            );
            metrics.sync_duration_count.fetch_add(1, Ordering::Relaxed);
        }
        let error = result.as_ref().err().map(ToString::to_string);
        let retry_after =
            state::now_ms().saturating_add(event_retry_delay(item.retry_backoff_secs, attempts));
        db.finish_webhook_event(
            claimed_id,
            attempts,
            i64::from(item.max_retries) + 1,
            error.as_deref(),
            state::now_ms(),
            retry_after,
        )?;
        if result.is_ok() {
            // ponytail: a successful full-state sync makes queued notifications redundant.
            let coalesced =
                db.coalesce_webhook_events(&item.source, claimed_id, state::now_ms())?;
            if let Some(metrics) = metrics {
                metrics
                    .coalesced_events
                    .fetch_add(coalesced as u64, Ordering::Relaxed);
            }
        }
        if let Some(metrics) = metrics {
            if result.is_ok() {
                metrics.successful_syncs.fetch_add(1, Ordering::Relaxed);
            } else {
                metrics.failed_syncs.fetch_add(1, Ordering::Relaxed);
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
    config: &RwLock<Vec<WebhookItem>>,
    wake_sender: &Sender<()>,
    metrics: &Metrics,
) {
    let config = config.read().expect("webhook config lock poisoned").clone();
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
        write_response_with_type(
            &mut stream,
            "200 OK",
            "text/plain; version=0.0.4; charset=utf-8",
            &metrics.render(&config),
        );
        return;
    }
    if request.method != "POST" {
        metrics.rejected_requests.fetch_add(1, Ordering::Relaxed);
        write_response(&mut stream, "405 Method Not Allowed", "POST required");
        return;
    }
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
    for webhook_item in &config {
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
        let db = match StateDb::open(std::path::Path::new(&item.workspace), &item.source) {
            Ok(db) => db,
            Err(error) => {
                eprintln!("webhook state open failed: {error}");
                write_response(&mut stream, "500 Internal Server Error", "event failed");
                return;
            }
        };
        match db.enqueue_webhook_event(
            &item.source,
            event.provider,
            &event.delivery_id,
            &event.event_type,
            &refs_json,
            state::now_ms(),
        ) {
            Ok(true) => {
                metrics.enqueued_events.fetch_add(1, Ordering::Relaxed);
                let _ = wake_sender.send(());
            }
            Ok(false) => {
                metrics.deduplicated_events.fetch_add(1, Ordering::Relaxed);
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
    if matched {
        write_response(&mut stream, "202 Accepted", "sync queued");
    } else {
        metrics.ignored_events.fetch_add(1, Ordering::Relaxed);
        write_response(&mut stream, "202 Accepted", "event ignored");
    }
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
        if (now_secs - timestamp).abs() > SIGNATURE_TOLERANCE_SECS {
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
        let host = parts.next().unwrap_or_default();
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
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::{escape_label, parse_event, verify_event, verify_github_signature, Metrics};
    use base64::{engine::general_purpose::STANDARD, Engine};
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    use std::{collections::BTreeMap, sync::atomic::Ordering};

    type HmacSha256 = Hmac<Sha256>;

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
        assert_ne!(
            super::repository_keys("https://github.com/org/repo.git"),
            super::repository_keys("https://gitlab.com/org/repo.git")
        );
    }
}
