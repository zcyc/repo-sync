![reposync logo](/images/logo.png)

# repo-sync

Sync a Git repository to one or more target repositories with branch synchronization, full mirroring, scheduled tasks, and webhook triggers.

[简体中文](README.zh-CN.md)

## Choose a usage mode

| Scenario | Recommended approach |
| --- | --- |
| Long-running service managing multiple tasks | `--serve` + Web management page |
| Run a task manually | “Sync now” on the Web page |
| Check or maintain SQLite | `--check`, `--backup-*`, and other maintenance commands |

Task configuration is stored only in SQLite. `config.toml`, JSON, and TOML configuration files are not read.

For the complete option list and current version:

```sh
repo-sync --help
repo-sync --version
```

## Quick start: Web management page

### 1. Build

```sh
cargo build --release
```

### 2. Start

```sh
./target/release/repo-sync \
  --database ./repo-sync-tasks.sqlite3 \
  --serve 127.0.0.1:8080
```

Open <http://127.0.0.1:8080/>, set the administrator account and password on first access, then click “Create task”. The task takes effect immediately after it is saved.

The Web form provides safe common defaults:

| Setting | Default |
| --- | --- |
| `mode` | `branch` |
| `branches` | Empty, meaning all branches |
| `timeout_secs` | `300` |
| `divergence` | `fail` |
| `tag_policy` | `preserve` |
| `atomic` | `true` |
| `max_retries` / `retry_backoff_secs` | `3` / `5` |
| `failure_cooldown_secs` | `60` |
| `webhook_max_pending_events` / `webhook_event_lease_secs` | `10000` / `900` |
| `dry_run`, LFS synchronization, cleanup, and destructive operations | Disabled |

Administrator passwords must be 12–256 characters long. Passwords and webhook secrets are not written to the task SQLite database; tasks store only the names of the environment variables containing those secrets.

## Create a task in the Web page

Click “Create task” and fill in these four fields first:

| Field | What to enter |
| --- | --- |
| Source repository URL or path | The source Git repository |
| Target repositories | One or more targets, one per line |
| Workspace | Local working directory; cannot be `.` or `..` |
| Mode | Choose `branch` for normal distribution; choose `mirror` for a disposable full mirror |

An existing workspace must be a Git repository matching the selected mode, and its `origin` must point to the same source. The first synchronization creates the workspace; later synchronizations reuse it.

After saving a task, click “Sync now” to run it. Saving, enabling, disabling, canceling, and retrying take effect immediately.

## Synchronization modes and safety policies

### `branch`: normal branch distribution, recommended

Enter a branch name or glob in the “Branches” field, one per line, such as `main` or `release/*`. Leave it empty to include all branches.

When a target branch contains commits not present in the source, the “Branch divergence policy” determines what happens:

| Value | Behavior |
| --- | --- |
| `fail` | Report an error without overwriting the target branch |
| `keep` | Skip this target branch and continue with other branches and targets |
| `force` | Update the target branch with `force-with-lease` |

The “Tag conflict policy” determines how tag conflicts are handled:

| Value | Behavior |
| --- | --- |
| `preserve` | Keep the target tag and skip the conflict |
| `fail` | Report an error immediately when a conflict is found |
| `force` | Update the target tag with `force-with-lease` |

### `mirror`: full mirror, for disposable targets only

`mirror` uses `git clone --mirror`, synchronizes all selected refs, and deletes corresponding refs that do not exist in the source. It cannot use branch filters or branch/tag pruning, and its tag conflict policy must be `force`. Before writing anything, select “Dry run” to verify the plan, then clear “Dry run” and select “Allow destructive operations”.

`mirror`, `force` branch divergence, `force` tag conflict, and branch/tag pruning are destructive operations. Normal branch distribution generally does not require “Allow destructive operations”.

## Ref filtering

Enter complete ref globs in the “Include refs” and “Exclude refs” fields, one per line, such as `refs/heads/*` or `refs/tags/nightly-*`.

- An empty `include_refs` means all refs.
- `exclude_refs` takes precedence; excluded refs are not synchronized.
- Patterns must start with `refs/`.
- Quote `*` and `?` in the shell.
- `branch` mode also applies the branch-name filter from the “Branches” field first.

## Advanced options

| Field | Purpose |
| --- | --- |
| `dry_run` | Calculate and display the push plan without writing target refs |
| `sync_lfs` | Synchronize Git LFS objects; Git LFS must be installed in the runtime environment |
| `prune_branches` / `prune_tags` | Delete branches/tags within the filter scope that exist in the target but not in the source |
| `atomic` | Require ref updates for a single target to use an atomic push |
| `crontab` | Run tasks on a schedule using the project's schedule syntax; leave empty for no schedule |
| `timeout_secs` | Git command timeout, 300 seconds by default |
| `max_retries` / `retry_backoff_secs` | Additional retries after failures and the initial delay, 3 / 5 by default |
| `failure_cooldown_secs` | Pause the scheduled task when all targets continue to fail, 60 seconds by default |
| `webhook_max_pending_events` / `webhook_event_lease_secs` | Webhook queue limit and event lease, 10000 / 900 seconds by default |

`atomic` covers only the ref updates in one ref push to one target. Multiple targets, LFS transfers, and SQLite state updates are not part of one transaction.

## Task database schema

The CLI no longer accepts source, target, or any synchronization configuration parameters, and it does not execute synchronization tasks. Tasks can only be created, modified, and run manually from the Web page; the CLI only starts the service and maintains the database.

When using `--serve`, tasks come from `--database`:

```sh
repo-sync --database /var/lib/repo-sync/repo-sync.sqlite3 --serve 127.0.0.1:8080
```

Behavior of enabled tasks:

- Without `crontab`: wait for a webhook or “Sync now” on the Web page.
- With `crontab`: enqueue automatically according to the schedule.
- “Sync now”, cancel, retry, enable, and disable on the Web page take effect immediately.
- The CLI does not run tasks automatically at startup; scheduled tasks and webhooks are handled in the background by `--serve`.

The task database and workspace state database are separate:

| File | Contents |
| --- | --- |
| SQLite specified by `--database` | Tasks, administrator account, and sessions |
| `<workspace name>.sqlite3` | Target state, run history, webhook queue, and events |

Deleting a task does not delete its workspace or workspace state database. A workspace can belong to only one task.

## Webhooks

### Configuration

1. In the task’s “Advanced options”, set `webhook_secret_envs` to an environment variable name, such as `REPO_SYNC_WEBHOOK_SECRET_MAIN`.
2. Set the corresponding secret before starting the listener:

   ```sh
   export REPO_SYNC_WEBHOOK_SECRET_MAIN='replace-with-a-long-random-secret'
   repo-sync --database ./repo-sync-tasks.sqlite3 --serve 127.0.0.1:8080
   ```

3. In GitHub or GitLab, set the webhook URL to the listener, such as `https://sync.example.com/webhook`.

Supported events and signatures:

| Provider | Events | Signature |
| --- | --- | --- |
| GitHub | `push`, `delete` | `X-Hub-Signature-256` |
| GitLab | `Push Hook`, `Tag Push Hook` | `Webhook-Signature` or `X-Gitlab-Token` |

An event is enqueued only when it matches both the task’s source and ref filters. After receiving and writing the event to the SQLite queue, the listener returns `202`; Git synchronization runs in the background. Duplicate deliveries are deduplicated.

### Listener addresses

| Address | Purpose |
| --- | --- |
| `GET /` | Management page |
| `GET /healthz` | Liveness check |
| `GET /readyz` | Readiness check |
| `POST /webhook` | GitHub/GitLab webhooks; a reverse proxy can use a unified POST path |

The listener does not provide TLS. Put it behind an existing TLS reverse proxy when exposing webhooks or the management page externally. SQLite is the single source of truth for task configuration; the management page and background workers read the latest configuration directly, so the process does not need to be restarted.

## Status and maintenance

View status, webhook events, queues, and recent run history on the Web page. Task creation, editing, manual execution, cancellation, and retries are also performed only on the Web page.

```sh
# Check task configuration, workspace, and repository access
repo-sync --database ./repo-sync-tasks.sqlite3 --check
repo-sync --database ./repo-sync-tasks.sqlite3 --check --check-write

# Delete only completed history older than 30 days; the minimum is 7
repo-sync --database ./repo-sync-tasks.sqlite3 --prune-history-days 30

# Back up; the destination file must not exist and will not be overwritten
repo-sync --database ./repo-sync-tasks.sqlite3 --backup-tasks ./tasks-backup.sqlite3
repo-sync --database ./repo-sync-tasks.sqlite3 --backup-state ./workspace-state-backup.sqlite3
```

`--prune-history-days` and `--backup-state` require exactly one synchronization task to be loaded. Ensure the destination path does not exist before backing up. If the administrator password is lost, stop the listener first, then run:

```sh
repo-sync --database /var/lib/repo-sync/repo-sync.sqlite3 --reset-admin
```

This clears only the administrator account and sessions; it does not delete tasks, workspaces, or run history. Set up the account again the next time you open the page.

## Linux systemd (optional)

The project provides [repo-sync.service.example](repo-sync.service.example) and [webhook.env.example](webhook.env.example). Minimal installation steps:

```sh
sudo install -d -o repo-sync -g repo-sync /var/lib/repo-sync /etc/repo-sync
sudo install -m 600 webhook.env.example /etc/repo-sync/webhook.env
sudo install -m 644 repo-sync.service.example /etc/systemd/system/repo-sync.service
sudo systemctl daemon-reload
sudo systemctl enable --now repo-sync
```

View logs:

```sh
sudo journalctl -u repo-sync -f
```

The service example uses `/var/lib/repo-sync` for the task database and workspaces, and `/etc/repo-sync/webhook.env` for secrets. Whether or not the management page is open, background workers and webhooks read the latest task configuration directly from the task database; external SQL inserts or changes take effect without restarting the process.

Daily task database backups can also use [repo-sync-backup.sh.example](repo-sync-backup.sh.example), [repo-sync-backup.service.example](repo-sync-backup.service.example), and [repo-sync-backup.timer.example](repo-sync-backup.timer.example). The backup templates do not automatically delete old backups.

## Authentication, credentials, and upgrade notes

- HTTP(S) repository URLs cannot contain usernames or passwords; use an SSH agent or Git credential helper.
- Configuration errors are rejected before synchronization starts; a failure for one target does not prevent attempts on other targets.
- Git does not wait for interactive credential input.

## License

See [LICENSE](LICENSE).
