use repo_sync::{
    check, cooldown_active, status_report, sync, DivergencePolicy, Item, SyncMode, TagPolicy,
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "repo-sync-test-{}-{suffix}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = git_output(dir, args);
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn git_output(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(args)
        .output()
        .unwrap()
}

fn item(source: &Path, target: &Path, workspace: &Path) -> Item {
    Item {
        source: source.to_string_lossy().into_owned(),
        target: vec![target.to_string_lossy().into_owned()],
        workspace: workspace.to_string_lossy().into_owned(),
        mode: SyncMode::Branch,
        crontab: None,
        branches: Vec::new(),
        include_refs: Vec::new(),
        exclude_refs: Vec::new(),
        timeout_secs: 30,
        dry_run: false,
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
    }
}

fn mirror_item(source: &Path, target: &Path, workspace: &Path) -> Item {
    let mut item = item(source, target, workspace);
    item.mode = SyncMode::Mirror;
    item.include_refs = vec!["refs/heads/*".into(), "refs/tags/*".into()];
    item.exclude_refs = vec!["refs/heads/feature".into()];
    item.divergence = DivergencePolicy::Force;
    item.tag_policy = TagPolicy::Force;
    item.allow_destructive = true;
    item
}

fn setup_source(root: &Path) -> (PathBuf, PathBuf) {
    let source = root.join("source.git");
    let work = root.join("source-work");
    let source_text = source.to_string_lossy().into_owned();
    git(root, &["init", "--bare", source.to_str().unwrap()]);
    git(root, &["init", "-b", "main", work.to_str().unwrap()]);
    git(&work, &["config", "user.name", "repo-sync test"]);
    git(&work, &["config", "user.email", "repo-sync@example.test"]);
    fs::write(work.join("README.md"), "main\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-m", "main"]);
    git(&work, &["remote", "add", "origin", &source_text]);
    git(&work, &["push", "-u", "origin", "main"]);
    git(&work, &["switch", "-c", "feature"]);
    fs::write(work.join("feature.txt"), "feature\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-m", "feature"]);
    git(&work, &["push", "-u", "origin", "feature"]);
    git(&work, &["switch", "main"]);
    git(&work, &["tag", "v1"]);
    git(&work, &["push", "origin", "v1"]);
    (source, work)
}

#[test]
fn syncs_all_branches_and_tags_then_prunes_target_refs() {
    let temp = TempDir::new();
    let (source, _) = setup_source(&temp.0);
    let target = temp.0.join("target.git");
    let workspace = temp.0.join("workspace");
    git(&temp.0, &["init", "--bare", target.to_str().unwrap()]);
    fs::write(
        temp.0.join("workspace.lock"),
        "pid=999999999 created_ms=0\n",
    )
    .unwrap();

    sync(&item(&source, &target, &workspace)).unwrap();
    assert_eq!(
        git(&target, &["rev-parse", "refs/heads/main"]).trim().len(),
        40
    );
    assert_eq!(
        git(&target, &["rev-parse", "refs/heads/feature"])
            .trim()
            .len(),
        40
    );
    assert_eq!(
        git(&target, &["rev-parse", "refs/tags/v1"]).trim().len(),
        40
    );
    let state = status_report(&workspace, &source.to_string_lossy()).unwrap();
    assert!(state.initialized);
    assert_eq!(state.targets[0].status, "synced");
    assert!(state.targets[0]
        .synced_refs
        .contains_key("refs/heads/feature"));

    let target_work = temp.0.join("target-work");
    git(
        &temp.0,
        &[
            "clone",
            target.to_str().unwrap(),
            target_work.to_str().unwrap(),
        ],
    );
    git(&target_work, &["config", "user.name", "repo-sync test"]);
    git(
        &target_work,
        &["config", "user.email", "repo-sync@example.test"],
    );
    git(&target_work, &["switch", "-c", "stale"]);
    fs::write(target_work.join("stale.txt"), "stale\n").unwrap();
    git(&target_work, &["add", "."]);
    git(&target_work, &["commit", "-m", "stale"]);
    git(&target_work, &["tag", "stale-tag"]);
    git(&target_work, &["push", "origin", "stale", "stale-tag"]);

    let mut prune = item(&source, &target, &workspace);
    prune.allow_destructive = true;
    prune.divergence = DivergencePolicy::Force;
    prune.prune_branches = true;
    prune.prune_tags = true;
    sync(&prune).unwrap();
    assert!(!git_output(
        &target,
        &["show-ref", "--verify", "--quiet", "refs/heads/stale"]
    )
    .status
    .success());
    assert!(!git_output(
        &target,
        &["show-ref", "--verify", "--quiet", "refs/tags/stale-tag"]
    )
    .status
    .success());
}

#[test]
fn tag_policy_preserves_fails_or_forces_conflicts() {
    let temp = TempDir::new();
    let (source, _) = setup_source(&temp.0);
    let target = temp.0.join("target.git");
    let workspace = temp.0.join("workspace");
    git(&temp.0, &["init", "--bare", target.to_str().unwrap()]);
    sync(&item(&source, &target, &workspace)).unwrap();

    let target_work = temp.0.join("target-work");
    git(
        &temp.0,
        &[
            "clone",
            target.to_str().unwrap(),
            target_work.to_str().unwrap(),
        ],
    );
    git(&target_work, &["config", "user.name", "repo-sync test"]);
    git(
        &target_work,
        &["config", "user.email", "repo-sync@example.test"],
    );
    git(&target_work, &["switch", "-c", "target-only"]);
    fs::write(target_work.join("target.txt"), "target\n").unwrap();
    git(&target_work, &["add", "."]);
    git(&target_work, &["commit", "-m", "target"]);
    git(&target_work, &["tag", "-f", "v1", "target-only"]);
    git(&target_work, &["push", "origin", "target-only"]);
    git(&target_work, &["push", "--force", "origin", "refs/tags/v1"]);
    let conflicting_sha = git(&target, &["rev-parse", "refs/tags/v1"])
        .trim()
        .to_owned();

    let mut preserve = item(&source, &target, &workspace);
    preserve.tag_policy = TagPolicy::Preserve;
    sync(&preserve).unwrap();
    assert_eq!(
        git(&target, &["rev-parse", "refs/tags/v1"]).trim(),
        conflicting_sha
    );

    let mut fail = item(&source, &target, &workspace);
    fail.tag_policy = TagPolicy::Fail;
    fail.failure_cooldown_secs = 60;
    assert!(sync(&fail).is_err());
    let state = status_report(&workspace, &source.to_string_lossy()).unwrap();
    assert_eq!(state.targets[0].status, "failed");
    assert!(state.targets[0]
        .last_error
        .as_deref()
        .is_some_and(|error| error.contains("target tag is divergent")));
    assert!(cooldown_active(
        &workspace,
        &source.to_string_lossy(),
        &[target.to_string_lossy().into_owned()],
        60
    )
    .unwrap());

    let mut force = item(&source, &target, &workspace);
    force.tag_policy = TagPolicy::Force;
    force.allow_destructive = true;
    sync(&force).unwrap();
    assert!(!cooldown_active(
        &workspace,
        &source.to_string_lossy(),
        &[target.to_string_lossy().into_owned()],
        60
    )
    .unwrap());
    assert_ne!(
        git(&target, &["rev-parse", "refs/tags/v1"]).trim(),
        conflicting_sha
    );
}

#[test]
fn mirror_respects_ref_filters_and_write_preflight() {
    let temp = TempDir::new();
    let (source, _) = setup_source(&temp.0);
    let target = temp.0.join("target.git");
    let workspace = temp.0.join("mirror-workspace");
    git(&temp.0, &["init", "--bare", target.to_str().unwrap()]);
    let mirror = mirror_item(&source, &target, &workspace);

    sync(&mirror).unwrap();
    assert!(git_output(
        &target,
        &["show-ref", "--verify", "--quiet", "refs/heads/main"]
    )
    .status
    .success());
    assert!(!git_output(
        &target,
        &["show-ref", "--verify", "--quiet", "refs/heads/feature"]
    )
    .status
    .success());
    check(&mirror, true).unwrap();

    let target_work = temp.0.join("mirror-target-work");
    git(
        &temp.0,
        &[
            "clone",
            target.to_str().unwrap(),
            target_work.to_str().unwrap(),
        ],
    );
    git(&target_work, &["config", "user.name", "repo-sync test"]);
    git(
        &target_work,
        &["config", "user.email", "repo-sync@example.test"],
    );
    git(&target_work, &["switch", "-c", "stale"]);
    fs::write(target_work.join("stale.txt"), "stale\n").unwrap();
    git(&target_work, &["add", "."]);
    git(&target_work, &["commit", "-m", "stale"]);
    git(&target_work, &["push", "origin", "stale"]);

    sync(&mirror).unwrap();
    assert!(!git_output(
        &target,
        &["show-ref", "--verify", "--quiet", "refs/heads/stale"]
    )
    .status
    .success());
}
