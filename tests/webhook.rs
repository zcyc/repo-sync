use hmac::{Hmac, Mac};
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
    let config_path = root.join("config.toml");
    fs::write(
        &config_path,
        format!(
            "[[sync]]\nsource = \"example/repo\"\ntarget = [\"target\"]\nworkspace = \"{}\"\nmode = \"branch\"\nbranches = [\"main\"]\ninclude_refs = []\nexclude_refs = []\ntimeout_secs = 5\ndry_run = true\nallow_destructive = false\nsync_lfs = false\ndivergence = \"fail\"\ntag_policy = \"preserve\"\nprune_branches = false\nprune_tags = false\natomic = true\nmax_retries = 0\nretry_backoff_secs = 0\nfailure_cooldown_secs = 0\nwebhook_secret_envs = [\"REPO_SYNC_TEST_WEBHOOK_SECRET\"]\n",
            workspace.display()
        ),
    )
    .unwrap();
    let Some(address) = free_address() else {
        let _ = fs::remove_dir_all(root);
        eprintln!("skipping HTTP integration test: TCP bind is unavailable");
        return;
    };
    let mut child = Command::new(env!("CARGO_BIN_EXE_repo-sync"))
        .args(["--serve", &address, "--file", config_path.to_str().unwrap()])
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
