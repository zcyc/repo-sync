![reposync logo](/images/logo.png)

# repo-sync
A lightweight synchronization tool for git repositories.

## Manual
```
USAGE:
    repo-sync [OPTIONS]

OPTIONS:
    -c, --crontab <CRONTAB>     crontab string, eg: '0 * * * * ? *'
    -d, --database <PATH>       SQLite task database path [default: repo-sync-tasks.sqlite3]
    -h, --help                  Print help information
        --allow-destructive    allow mirror mode to force-update and delete target refs
        --divergence <POLICY>  divergence policy: fail, keep, or force [possible values: fail, keep, force]
        --dry-run               show planned pushes without changing targets
        --mode <MODE>           sync mode: branch or mirror [possible values: branch, mirror]
        --sync-lfs              sync Git LFS objects
        --tag-policy <POLICY>   tag conflict policy: preserve, fail, or force
        --prune-branches        delete target branches absent from source
        --prune-tags            delete target tags absent from source
        --check                 validate task database and repository access only
        --check-write           also test target write access with a dry-run push
        --status                show persisted synchronization status
        --json                  format --status as JSON
        --serve <ADDR>          listen for Webhook POST triggers and serve the embedded page
        --webhook-max-pending-events <N>
                                maximum pending webhook events per sync item
        --webhook-event-lease-secs <N>
                                lease for a running webhook event in seconds
        --events                show recent webhook event history
        --retry-event <ID>      retry a failed or dead webhook event
        --prune-history-days <DAYS>
                                delete finished SQLite history older than DAYS
        --backup-state <PATH>   backup one workspace SQLite database
        --backup-tasks <PATH>   backup the SQLite task database
        --reset-admin           clear the administrator account for offline recovery
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

## Task database

Tasks are stored directly in the SQLite database passed with `--database`; no
TOML/JSON configuration file is read. The default is `repo-sync-tasks.sqlite3` in the
current directory. Start the listener with:

```sh
repo-sync --database /var/lib/repo-sync/repo-sync.sqlite3 --serve 127.0.0.1:8080
```

Open `/`, set the administrator account on first use, then log in to create,
edit, enable, disable, or delete tasks. The page validates each task before
saving it. A task contains the source
and targets, workspace, branch/ref filters, safety policies, retry settings,
schedule, and webhook secret environment variable names. Secret values are
never stored in SQLite or returned by the API. The page also supports an
immediate manual sync and password changes; changing the password invalidates
all existing sessions and signs in the current browser again.

The task registry database and each workspace's `<workspace-name>.sqlite3`
runtime state database are separate. The latter stores target state, run
history, errors, durations, synced refs, webhook delivery ids, queue state, and
dead-letter events. Both use SQLite WAL mode and a busy timeout for local
readers.

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

`REPO_SYNC_WEBHOOK_SECRET_MAIN=... REPO_SYNC_WEBHOOK_SECRET_MIRROR=... repo-sync --database repo-sync.sqlite3 --serve 127.0.0.1:8080` starts a webhook listener. It authenticates and parses GitHub
`push`/`delete` events and GitLab `Push Hook`/`Tag Push Hook` events, matches the
payload repository and ref against the loaded items, then queues only matching
syncs. GitHub uses `X-Hub-Signature-256`; GitLab signing tokens and legacy secret
tokens are accepted. `/healthz` and `/readyz` are available without provider
headers. Each item reads the environment variables named by
`webhook_secret_envs`; send `SIGHUP` to reload tasks and rotate secrets
without stopping the listener. The listener returns `202` after SQLite enqueue and a background worker
performs the sync, so provider retries do not block on Git. Put it behind an
existing TLS reverse proxy and stop it with Ctrl-C. `/metrics` exposes Prometheus
text metrics for request outcomes, queue status, deduplication, coalescing, and
sync results; protect it at the reverse proxy if it is not on a private network.
The listener caps active connections at 64 and returns `503` when saturated.
Login failures are limited per client address: five failures in one minute
temporarily block further login attempts for five minutes. The worker reacts
to matching Webhook deliveries, manual runs, and scheduled runs immediately
instead of scanning every task on a fixed polling interval. Webhook, manual,
and cron triggers are persisted in the same SQLite event queue, so a process
restart does not silently drop a requested run. The page can cancel
queued or running work; a running Git command is stopped cooperatively and is
not retried as a normal failure.
For systemd, start from `repo-sync.service.example` and
`webhook.env.example`; the example keeps the task database in
`/var/lib/repo-sync` and the secrets in `/etc/repo-sync`. `systemctl reload
repo-sync` reloads the task database.

The embedded administration page is available at `/`. On first use, set the
administrator username and a 12-256 character password; later visits use the
account/password login and an HttpOnly session cookie. The page can view each
task's latest and recent runs, target status, Webhook queue, and recent events;
it can filter tasks, run or cancel work, retry failed events, and edit tasks
directly in SQLite. Passwords are stored as Argon2id hashes and session tokens
are stored only as hashes. Keep the listener on a private address or put it
behind a TLS reverse proxy.

## systemd（Linux 可选）

systemd 是 Linux 的系统和服务管理器。它可以在开机时启动 repo-sync，进程
异常退出时自动重启，并把 stdout/stderr 纳入 journald，适合长期运行 Webhook
监听器；macOS 和 Windows 不需要它。复制 `repo-sync.service.example` 后执行：

```sh
sudo install -d -o repo-sync -g repo-sync /var/lib/repo-sync /etc/repo-sync
sudo install -m 600 webhook.env.example /etc/repo-sync/webhook.env
sudo install -m 644 repo-sync.service.example /etc/systemd/system/repo-sync.service
sudo systemctl daemon-reload
sudo systemctl enable --now repo-sync
sudo journalctl -u repo-sync -f
```

服务示例将任务数据库和 workspace 限制在 `/var/lib/repo-sync`，Webhook
secret 放在 `/etc/repo-sync/webhook.env`；首次账号设置仍在 Web 页面完成。

`--events` shows the latest 50 webhook events per configured workspace;
`--events --json` is suitable for monitoring. `--retry-event <ID>` resets a
failed/dead event and executes it immediately. Event retries use the existing
`max_retries` and `retry_backoff_secs` settings; exhausted events remain in the
dead-letter state until manually retried. When several deliveries are waiting,
one successful full-state sync coalesces the redundant queued deliveries. Event
IDs are local to each workspace; pass `--workspace <PATH>` with
`--retry-event <ID>` when the ID exists in more than one workspace.
`--prune-history-days N` explicitly removes finished run and webhook history
older than `N` days; `N` must be at least 7 so provider delivery IDs remain
deduplicated across normal redeliveries. `--backup-state PATH` creates a
non-overwriting workspace SQLite backup. `--backup-tasks PATH` creates a
non-overwriting task-registry SQLite backup. The destination must not already
exist, so use a timestamped filename. These maintenance commands never run
automatically. If the administrator password is lost, stop the listener and
run `repo-sync --database /var/lib/repo-sync/repo-sync.sqlite3 --reset-admin`;
the next visit to `/` will require setting a new account and password. This
clears only the administrator account and sessions, not tasks or workspaces.

`prometheus-alerts.example.yml` contains starter rules for queue stalls, sync
failures, dead-letter events, and login abuse. Load it into Prometheus and use
Alertmanager for notification routing and deduplication.

For Linux, the optional `repo-sync-backup.sh.example`,
`repo-sync-backup.service.example`, and `repo-sync-backup.timer.example` run a
daily task-database snapshot with a unique UTC filename. Install the script as
`/usr/local/libexec/repo-sync-backup`, install both unit files under
`/etc/systemd/system`, then run:

```sh
sudo install -d -m 755 /usr/local/libexec
sudo install -d -o repo-sync -g repo-sync -m 700 /var/lib/repo-sync/backups
sudo install -m 700 repo-sync-backup.sh.example /usr/local/libexec/repo-sync-backup
sudo install -m 644 repo-sync-backup.service.example repo-sync-backup.timer.example /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now repo-sync-backup.timer
```

The template does not delete old backups; apply the retention policy used by
your host separately.

`atomic` applies to one Git ref push for one target. Multiple targets, LFS
transfers, and the SQLite status update are separate operations and are not one
transaction.

Existing workspaces must point to the exact configured source and use the
configured repository type. Credentials in HTTP(S) URLs are rejected;
configure an SSH agent or Git credential helper instead. This task storage
change is intentionally breaking: `--file`, `config.toml`, the old JSON/TOML
configuration loader, `REPO_SYNC_ADMIN_TOKEN`, and the old global
`REPO_SYNC_WEBHOOK_SECRET` are no longer supported. SQLite databases without the current schema version are
rejected and must be removed and rebuilt; no task or runtime-state migration
is provided. Deleting a task does not delete its workspace or runtime-state
database. The old generic `X-Repo-Sync-Secret` webhook is not accepted.

For one-time runs, omit `crontab`. In database mode, disabled tasks are not
scheduled or processed; enabled tasks without a schedule run once, while
scheduled tasks continue running in the scheduler.
Configuration errors are rejected before any sync starts. A failed target does
not prevent other targets from being attempted, and Git never waits for an
interactive terminal credential prompt. Use `--check` for a read-only access
check, `--check-write` to test target write access, `--status` to inspect state,
and `--once` to execute scheduled items once without entering the loop.

## Why Not
- [git-sync](https://github.com/kubernetes/git-sync) of `kubernetes` only synchronizes the repository into the folder.
- [Repository mirroring](https://docs.gitlab.com/ee/user/project/repository/mirror/) of `GitLab` requires a paid version.
