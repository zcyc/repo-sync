use hmac::{Hmac, Mac};
use repo_sync::{create_task, DivergencePolicy, Item, SyncMode, TagPolicy};
use sha2::Sha256;
use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    process::{Command, Stdio},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

type HmacSha256 = Hmac<Sha256>;

#[test]
fn accepts_signed_github_webhook_over_http() {
    let root = std::env::temp_dir().join(format!(
        "repo-sync-webhook-test-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&root).unwrap();
    let workspace = root.join("workspace");
    let database_path = root.join("tasks.sqlite3");
    let item = Item {
        source: "example/repo".into(),
        target: vec!["target".into()],
        workspace: workspace.to_string_lossy().into_owned(),
        mode: SyncMode::Branch,
        crontab: None,
        branches: vec!["main".into()],
        include_refs: Vec::new(),
        exclude_refs: Vec::new(),
        timeout_secs: 5,
        dry_run: true,
        allow_destructive: false,
        sync_lfs: false,
        divergence: DivergencePolicy::Fail,
        tag_policy: TagPolicy::Preserve,
        prune_branches: false,
        prune_tags: false,
        atomic: true,
        max_retries: 0,
        retry_backoff_secs: 0,
        failure_cooldown_secs: 0,
        webhook_secret_envs: vec!["REPO_SYNC_TEST_WEBHOOK_SECRET".into()],
        webhook_max_pending_events: 100,
        webhook_event_lease_secs: 60,
    };
    let task = create_task(&database_path, &item, true).unwrap();
    let Some(address) = free_address() else {
        let _ = fs::remove_dir_all(root);
        eprintln!("skipping HTTP integration test: TCP bind is unavailable");
        return;
    };
    let mut child = Command::new(env!("CARGO_BIN_EXE_repo-sync"))
        .args([
            "--serve",
            &address,
            "--database",
            database_path.to_str().unwrap(),
        ])
        .env("REPO_SYNC_TEST_WEBHOOK_SECRET", "secret")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let ready = (0..40).any(|_| {
        let Ok(mut stream) = TcpStream::connect(&address) else {
            thread::sleep(Duration::from_millis(50));
            return false;
        };
        stream
            .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .is_ok()
    });
    if !ready {
        let _ = child.kill();
        let _ = child.wait();
        let _ = fs::remove_dir_all(root);
        panic!("webhook listener did not start");
    }

    let unauthorized_response = http_request(
        &address,
        "GET /api/status HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        &[],
    );
    assert!(unauthorized_response.starts_with("HTTP/1.1 401 Unauthorized"));

    let auth_status = http_request(
        &address,
        "GET /api/auth/status HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        &[],
    );
    assert!(auth_status.starts_with("HTTP/1.1 200 OK"));
    assert!(auth_status.contains("\"initialized\":false"));

    let setup_body = br#"{"username":"admin","password":"correct horse battery staple"}"#;
    let setup_request = format!(
        "POST /api/auth/setup HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        setup_body.len()
    );
    let setup_response = http_request(&address, &setup_request, setup_body);
    assert!(setup_response.starts_with("HTTP/1.1 201 Created"));
    let session = setup_response
        .lines()
        .find_map(|line| line.strip_prefix("Set-Cookie: "))
        .and_then(|cookie| cookie.split(';').next())
        .unwrap()
        .to_owned();

    let dashboard_response = http_request(
        &address,
        &format!(
            "GET /api/status HTTP/1.1\r\nHost: localhost\r\nCookie: {session}\r\nConnection: close\r\n\r\n"
        ),
        &[],
    );
    assert!(dashboard_response.starts_with("HTTP/1.1 200 OK"));
    assert!(dashboard_response.contains("example/repo"));

    let config_body = serde_json::to_vec(&serde_json::json!({
        "item": item,
        "enabled": true
    }))
    .unwrap();
    let put_request = format!(
        "PUT /api/tasks/{} HTTP/1.1\r\nHost: localhost\r\nCookie: {session}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        task.id,
        config_body.len()
    );
    let put_response = http_request(&address, &put_request, &config_body);
    assert!(put_response.starts_with("HTTP/1.1 200 OK"));

    let logout_response = http_request(
        &address,
        &format!(
            "POST /api/auth/logout HTTP/1.1\r\nHost: localhost\r\nCookie: {session}\r\nConnection: close\r\n\r\n"
        ),
        &[],
    );
    assert!(logout_response.starts_with("HTTP/1.1 204 No Content"));

    let login_body = br#"{"username":"admin","password":"correct horse battery staple"}"#;
    let login_request = format!(
        "POST /api/auth/login HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        login_body.len()
    );
    let login_response = http_request(&address, &login_request, login_body);
    assert!(login_response.starts_with("HTTP/1.1 200 OK"));
    let login_session = login_response
        .lines()
        .find_map(|line| line.strip_prefix("Set-Cookie: "))
        .and_then(|cookie| cookie.split(';').next())
        .unwrap()
        .to_owned();

    let password_body =
        br#"{"current_password":"correct horse battery staple","new_password":"another correct battery phrase"}"#;
    let password_request = format!(
        "POST /api/auth/password HTTP/1.1\r\nHost: localhost\r\nCookie: {login_session}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        password_body.len()
    );
    let password_response = http_request(&address, &password_request, password_body);
    assert!(password_response.starts_with("HTTP/1.1 200 OK"));
    let changed_session = password_response
        .lines()
        .find_map(|line| line.strip_prefix("Set-Cookie: "))
        .and_then(|cookie| cookie.split(';').next())
        .unwrap()
        .to_owned();

    let run_request = format!(
        "POST /api/tasks/{}/run HTTP/1.1\r\nHost: localhost\r\nCookie: {changed_session}\r\nConnection: close\r\n\r\n",
        task.id
    );
    let run_response = http_request(&address, &run_request, &[]);
    assert!(run_response.starts_with("HTTP/1.1 202 Accepted"));

    let cancel_request = format!(
        "POST /api/tasks/{}/cancel HTTP/1.1\r\nHost: localhost\r\nCookie: {changed_session}\r\nConnection: close\r\n\r\n",
        task.id
    );
    let cancel_response = http_request(&address, &cancel_request, &[]);
    assert!(cancel_response.starts_with("HTTP/1.1 202 Accepted"));

    let body =
        br#"{"ref":"refs/heads/main","after":"abc","repository":{"full_name":"example/repo"}}"#;
    let mut mac = HmacSha256::new_from_slice(b"secret").unwrap();
    mac.update(body);
    let signature = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let request = format!(
        "POST /hook HTTP/1.1\r\nHost: localhost\r\nX-GitHub-Event: push\r\nX-GitHub-Delivery: e2e-1\r\nX-Hub-Signature-256: sha256={signature}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let mut stream = TcpStream::connect(&address).unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    stream.write_all(body).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();

    let accepted = response.starts_with("HTTP/1.1 202 Accepted");
    let _ = child.kill();
    let _ = child.wait();
    let _ = fs::remove_dir_all(root);
    assert!(accepted, "{response}");
}

fn free_address() -> Option<String> {
    let listener = TcpListener::bind("127.0.0.1:0").ok()?;
    Some(listener.local_addr().ok()?.to_string())
}

fn http_request(address: &str, headers: &str, body: &[u8]) -> String {
    let mut stream = TcpStream::connect(address).unwrap();
    stream.write_all(headers.as_bytes()).unwrap();
    stream.write_all(body).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}
