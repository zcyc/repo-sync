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
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
        Arc,
    },
    thread,
    time::Duration,
};

type HmacSha256 = Hmac<Sha256>;

const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;
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

#[derive(Debug)]
struct RequestError {
    status: &'static str,
    message: &'static str,
}

pub fn serve(addr: &str, secret: &str, config: Vec<Item>) -> Result<(), Box<dyn Error>> {
    for item in &config {
        config::validate_item(item)?;
        let db = StateDb::open(std::path::Path::new(&item.workspace), &item.source)?;
        let now = state::now_ms();
        db.recover_webhook_events(&item.source, now.saturating_sub(EVENT_RECOVERY_MS), now)?;
    }
    let listener = TcpListener::bind(addr)?;
    listener.set_nonblocking(true)?;
    let config = Arc::new(config);
    let secret = Arc::new(secret.to_owned());
    let (wake_sender, wake_receiver) = mpsc::channel();
    let shutdown = Arc::new(AtomicBool::new(false));
    let worker_shutdown = Arc::clone(&shutdown);
    let worker_config = Arc::clone(&config);
    let worker = thread::spawn(move || worker_loop(worker_config, wake_receiver, worker_shutdown));
    let shutdown_handler = Arc::clone(&shutdown);
    ctrlc::set_handler(move || shutdown_handler.store(true, Ordering::Relaxed))?;

    eprintln!("webhook listener started on {addr}");
    while !shutdown.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                let config = Arc::clone(&config);
                let secret = Arc::clone(&secret);
                let wake_sender = wake_sender.clone();
                thread::spawn(move || handle_connection(stream, &secret, &config, &wake_sender));
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

pub fn retry_event(config: &[Item], event_id: i64) -> Result<bool, Box<dyn Error>> {
    for item in config {
        let workspace = std::path::Path::new(&item.workspace);
        if state::retry_webhook_event(workspace, &item.source, event_id)? {
            process_item(item, Some(event_id))?;
            return Ok(true);
        }
    }
    Ok(false)
}

fn worker_loop(config: Arc<Vec<Item>>, wake_receiver: Receiver<()>, shutdown: Arc<AtomicBool>) {
    while !shutdown.load(Ordering::Relaxed) {
        for item in config.iter() {
            if let Err(error) = process_item(item, None) {
                eprintln!("webhook worker failed for {}: {error}", item.workspace);
            }
        }
        match wake_receiver.recv_timeout(Duration::from_millis(500)) {
            Ok(()) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn process_item(item: &Item, event_id: Option<i64>) -> Result<(), Box<dyn Error>> {
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
        let result = sync::sync(item);
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
            db.coalesce_webhook_events(&item.source, claimed_id, state::now_ms())?;
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
    secret: &str,
    config: &[Item],
    wake_sender: &Sender<()>,
) {
    let request = match read_request(&mut stream) {
        Ok(request) => request,
        Err(error) => {
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
    if request.method != "POST" {
        write_response(&mut stream, "405 Method Not Allowed", "POST required");
        return;
    }
    let event = match parse_event(&request.headers, &request.body, secret) {
        Ok(event) => event,
        Err(error) => {
            write_response(&mut stream, error.status, error.message);
            return;
        }
    };
    let Some(event) = event else {
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
    for item in config {
        if !item_matches(item, &event) {
            continue;
        }
        matched = true;
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
                let _ = wake_sender.send(());
            }
            Ok(false) => {}
            Err(error) => {
                eprintln!("webhook event enqueue failed: {error}");
                write_response(&mut stream, "500 Internal Server Error", "event failed");
                return;
            }
        }
    }
    if matched {
        write_response(&mut stream, "202 Accepted", "sync queued");
    } else {
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
    secret: &str,
) -> Result<Option<WebhookEvent>, RequestError> {
    let now_secs = state::now_ms() / 1000;
    if let Some(event_type) = headers.get("x-github-event") {
        if !verify_github_signature(headers, body, secret) {
            return Err(RequestError {
                status: "401 Unauthorized",
                message: "invalid GitHub signature",
            });
        }
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
        if !verify_gitlab_signature(headers, body, secret, now_secs) {
            return Err(RequestError {
                status: "401 Unauthorized",
                message: "invalid GitLab signature",
            });
        }
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
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::{parse_event, verify_github_signature};
    use base64::{engine::general_purpose::STANDARD, Engine};
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    use std::collections::BTreeMap;

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
        let event = parse_event(&headers, body, "secret").unwrap().unwrap();
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
        let event = parse_event(&headers, delete_body, "secret")
            .unwrap()
            .unwrap();
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
        let event = parse_event(&headers, body, "secret").unwrap().unwrap();
        assert_eq!(event.provider, "gitlab");
        assert_eq!(event.refs[0].reference, "refs/heads/main");

        let delete_body = br#"{"ref":"refs/tags/v1","before":"abc","after":"0000000000000000000000000000000000000000","project":{"path_with_namespace":"org/repo"}}"#;
        let mut headers = headers;
        headers.insert("x-gitlab-event".into(), "Tag Push Hook".into());
        let event = parse_event(&headers, delete_body, "secret")
            .unwrap()
            .unwrap();
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
        let event = parse_event(&headers, body, &secret).unwrap().unwrap();
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
