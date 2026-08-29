![reposync logo](/images/logo.png)

# repo-sync
A lightweight synchronization tool for git repositories.

## Manual
```
USAGE:
    repo-sync [OPTIONS]

OPTIONS:
    -c, --crontab <CRONTAB>     crontab string, eg: '0 * * * * ? *'
    -f, --file <FILE>           config file path, eg: ./config.toml
    -h, --help                  Print help information
        --allow-destructive    allow mirror mode to force-update and delete target refs
        --divergence <POLICY>  divergence policy: fail, keep, or force [possible values: fail, keep, force]
        --dry-run               show planned pushes without changing targets
        --mode <MODE>           sync mode: branch or mirror [possible values: branch, mirror]
        --sync-lfs              sync Git LFS objects
        --tag-policy <POLICY>   tag conflict policy: preserve, fail, or force
        --prune-branches        delete target branches absent from source
        --prune-tags            delete target tags absent from source
        --check                 validate config and repository access only
        --check-write           also test target write access with a dry-run push
        --status                show persisted synchronization status
        --json                  format --status as JSON
        --serve <ADDR>          listen for GitHub/GitLab webhook POST triggers
        --webhook-secret <SECRET>
                                GitHub secret or GitLab signing/secret token
        --events                show recent webhook event history
        --retry-event <ID>      retry a failed or dead webhook event
        --once                  run scheduled items once and exit
    -s, --source <SOURCE>       source repo, eg: https://github.com/zcyc/repo-sync.git
    -t, --target <TARGET>...    target repo, eg: https://github.com/zcyc/repo-sync.git
        --branches <BRANCHES>... branch names or glob patterns; empty means all branches
        --all-branches          sync all source branches
        --timeout-secs <N>       Git command timeout in seconds
        --atomic                require atomic target ref updates
        --max-retries <N>       additional retries per Git command
        --retry-backoff-secs <N> initial retry backoff in seconds
        --failure-cooldown-secs <N>
                                pause scheduled runs while every target is failing
        --include-refs <REFS>... full ref glob patterns to include
        --exclude-refs <REFS>... full ref glob patterns to exclude
        --workspace <PATH>      local checkout path
    -V, --version               Print version information
```

## Notice
Before you begin the task, make sure that you can access and operate your source and target repositories.

The source is cloned into the configured `workspace`. Later runs reuse that
checkout only when its source URL and repository type match the configuration.
Git arguments are passed directly to Git, so URLs and branch names are not
interpreted by a shell.

## Configuration
You can configure repo-sync using a TOML file. Here's an example:

`config.json` is no longer supported; use `config.toml`.

```toml
# 同步主仓库的 main 分支到多个目标仓库。
[[sync]]
source = "https://github.com/zcyc/repo-sync.git"
target = [
  "https://github.com/zcyc/repo-sync-1.git",
  "https://github.com/zcyc/repo-sync-2.git",
]
workspace = "./repo-sync-main"
mode = "branch"
crontab = "0/10 * * * * ? *"
# 空数组同步全部分支；也可以写 ["main", "release/*"]。
branches = []
# 空数组包含全部 refs；exclude_refs 优先级更高。匹配完整 refs 名称。
include_refs = []
exclude_refs = []
timeout_secs = 300
dry_run = false
allow_destructive = false
sync_lfs = false
divergence = "fail"
tag_policy = "preserve"
prune_branches = false
prune_tags = false
atomic = true
max_retries = 3
retry_backoff_secs = 5
failure_cooldown_secs = 60

# 镜像模式会同步全部 refs，可能强制更新或删除目标仓库中的 refs。
[[sync]]
source = "https://github.com/zcyc/repo-sync.git"
target = ["https://github.com/zcyc/repo-sync-3.git"]
workspace = "./repo-sync-mirror"
mode = "mirror"
crontab = "0/10 * * * * ? *"
branches = []
include_refs = []
exclude_refs = []
timeout_secs = 300
dry_run = true
allow_destructive = false
sync_lfs = false
divergence = "force"
tag_policy = "force"
prune_branches = false
prune_tags = false
atomic = true
max_retries = 3
retry_backoff_secs = 5
failure_cooldown_secs = 60
```

- `source`: The source repository URL
- `target`: An array of target repository URLs
- `workspace`: A unique local path used by this sync item
- `mode`: `branch` or `mirror`
- `crontab`: The schedule for synchronization
- `branches`: Branch names or glob patterns in `branch` mode. An empty array syncs all source branches; `*` matches any sequence and `?` matches one character.
- `include_refs`: Full ref glob patterns such as `refs/heads/*` or `refs/tags/v*`. Empty includes all refs handled by the selected mode.
- `exclude_refs`: Full ref glob patterns to exclude. Exclusions take precedence over inclusions and are never deleted by pruning.
- `timeout_secs`: Maximum time allowed for each Git command.
- `dry_run`: Runs Git push with `--dry-run`; source checkout updates can still occur.
- `allow_destructive`: Must be `true` for a real `mirror` run, tag forcing, or ref pruning.
- `sync_lfs`: Fetches all source LFS objects and pushes the objects reachable from the synced refs. It requires `git-lfs` to be installed.
- `divergence`: Behavior when the target branch is not an ancestor of the source branch: `fail` stops that target, `keep` skips it, and `force` uses `--force-with-lease`.
- `tag_policy`: `preserve` skips conflicting target tags, `fail` aborts the target, and `force` replaces them.
- `prune_branches`: Deletes target branches matching `branches` that no longer exist in source. `branches = []` includes all target branches.
- `prune_tags`: Deletes target tags absent from source.
- `atomic`: Uses `git push --atomic`; the target must support atomic pushes.
- `max_retries`: Number of additional attempts for failed Git commands, up to 10.
- `retry_backoff_secs`: Initial exponential backoff between attempts.
- `failure_cooldown_secs`: When greater than zero, scheduled runs pause while every target is repeatedly failing; manual `--once` runs are not suppressed.
- A `<workspace-name>.sqlite3` database is written next to the workspace with target state, run history, errors, durations, synced ref SHAs, webhook delivery ids, queue state, and dead-letter events. It uses SQLite WAL mode and a busy timeout for safe local readers.

`mirror` mode uses `git clone --mirror`, fetches all refs, and builds an explicit
ref plan so `include_refs` and `exclude_refs` remain effective. It can
force-update or delete selected refs on targets; use `allow_destructive = true`
only when targets are disposable mirrors. The sample keeps this mode in dry-run
until explicitly enabled. `branch` mode is the safe choice for ordinary branch
fan-out. Before pushing branches, repo-sync checks the target with `git
ls-remote` and compares commit ancestry. After a real push it verifies the
selected target refs again.

`--check` performs read-only source/target access checks. `--check-write`
requires an existing workspace and performs a target `git push --dry-run`; it
can change only local remote configuration. `--status` reads the SQLite
database, and `--status --json` is intended for scripts and monitoring.

`REPO_SYNC_WEBHOOK_SECRET=... repo-sync --serve 127.0.0.1:8080 --file
config.toml` starts a webhook listener. It authenticates and parses GitHub
`push`/`delete` events and GitLab `Push Hook`/`Tag Push Hook` events, matches the
payload repository and ref against the loaded items, then queues only matching
syncs. GitHub uses `X-Hub-Signature-256`; GitLab signing tokens and legacy secret
tokens are accepted. `/healthz` and `/readyz` are available without provider
headers. The listener returns `202` after SQLite enqueue and a background worker
performs the sync, so provider retries do not block on Git. Put it behind an
existing TLS reverse proxy and stop it with Ctrl-C.

`--events` shows the latest 50 webhook events per configured workspace;
`--events --json` is suitable for monitoring. `--retry-event <ID>` resets a
failed/dead event and executes it immediately. Event retries use the existing
`max_retries` and `retry_backoff_secs` settings; exhausted events remain in the
dead-letter state until manually retried. When several deliveries are waiting,
one successful full-state sync coalesces the redundant queued deliveries.

`atomic` applies to one Git ref push for one target. Multiple targets, LFS
transfers, and the SQLite status update are separate operations and are not one
transaction.

Existing
workspaces must point to the exact configured source and use the configured
repository type. Credentials in HTTP(S) URLs are rejected; configure an SSH
agent or Git credential helper instead. This configuration format is
intentionally breaking: existing items must add `workspace`, `mode`,
`timeout_secs`, `dry_run`, `allow_destructive`, `sync_lfs`, `divergence`,
`tag_policy`, `prune_branches`, `prune_tags`, `atomic`, `max_retries`,
`retry_backoff_secs`, `failure_cooldown_secs`, `include_refs`,
`exclude_refs`, and replace `branch` with `branches`. Previous TOML state files
and the old generic `X-Repo-Sync-Secret` webhook are not read/accepted; the new
SQLite database starts a fresh state record.

For one-time runs, omit `crontab`. When using a configuration file, every item
without a schedule runs once; scheduled items continue running in the scheduler.
Configuration errors are rejected before any sync starts. A failed target does
not prevent other targets from being attempted, and Git never waits for an
interactive terminal credential prompt. Use `--check` for a read-only access
check, `--check-write` to test target write access, `--status` to inspect state,
and `--once` to execute scheduled items once without entering the loop.

## Why Not
- [git-sync](https://github.com/kubernetes/git-sync) of `kubernetes` only synchronizes the repository into the folder.
- [Repository mirroring](https://docs.gitlab.com/ee/user/project/repository/mirror/) of `GitLab` requires a paid version.
