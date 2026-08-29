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
        --once                  run scheduled items once and exit
    -s, --source <SOURCE>       source repo, eg: https://github.com/zcyc/repo-sync.git
    -t, --target <TARGET>...    target repo, eg: https://github.com/zcyc/repo-sync.git
        --branches <BRANCHES>... branch names or glob patterns; empty means all branches
        --all-branches          sync all source branches
        --timeout-secs <N>       Git command timeout in seconds
        --atomic                require atomic target ref updates
        --max-retries <N>       additional retries per Git command
        --retry-backoff-secs <N> initial retry backoff in seconds
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

# 镜像模式会同步全部 refs，可能强制更新或删除目标仓库中的 refs。
[[sync]]
source = "https://github.com/zcyc/repo-sync.git"
target = ["https://github.com/zcyc/repo-sync-3.git"]
workspace = "./repo-sync-mirror"
mode = "mirror"
crontab = "0/10 * * * * ? *"
branches = []
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
```

- `source`: The source repository URL
- `target`: An array of target repository URLs
- `workspace`: A unique local path used by this sync item
- `mode`: `branch` or `mirror`
- `crontab`: The schedule for synchronization
- `branches`: Branch names or glob patterns in `branch` mode. An empty array syncs all source branches; `*` matches any sequence and `?` matches one character.
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
- A `<workspace-name>.state.toml` file is written next to the workspace with the last attempt, result, failure count, error, and synced ref SHAs. Invalid state is reported instead of being silently replaced.

`mirror` mode uses `git clone --mirror`, fetches all refs, and runs
`git push --mirror`. This can force-update or delete refs on targets; use
`allow_destructive = true` only when targets are disposable mirrors. The sample
keeps this mode in dry-run until explicitly enabled. `branch` mode is the safe
choice for ordinary branch fan-out. Before pushing branches, repo-sync checks
the target with `git ls-remote` and compares commit ancestry. Existing
workspaces must point to the exact configured source and use the configured
repository type. Credentials in HTTP(S) URLs are rejected; configure an SSH
agent or Git credential helper instead. This configuration format is
intentionally breaking: existing items must add `workspace`, `mode`,
`timeout_secs`, `dry_run`, `allow_destructive`, `sync_lfs`, `divergence`,
`tag_policy`, `prune_branches`, `prune_tags`, `atomic`, `max_retries`,
`retry_backoff_secs`, and replace `branch` with `branches`.

For one-time runs, omit `crontab`. When using a configuration file, every item
without a schedule runs once; scheduled items continue running in the scheduler.
Configuration errors are rejected before any sync starts. A failed target does
not prevent other targets from being attempted, and Git never waits for an
interactive terminal credential prompt. Use `--check` for a read-only access
check and `--once` to execute scheduled items once without entering the loop.

## Why Not
- [git-sync](https://github.com/kubernetes/git-sync) of `kubernetes` only synchronizes the repository into the folder.
- [Repository mirroring](https://docs.gitlab.com/ee/user/project/repository/mirror/) of `GitLab` requires a paid version.
