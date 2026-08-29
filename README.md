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
        --mode <MODE>           sync mode: branch or mirror [possible values: branch, mirror]
    -s, --source <SOURCE>       source repo, eg: https://github.com/zcyc/repo-sync.git
    -t, --target <TARGET>...    target repo, eg: https://github.com/zcyc/repo-sync.git
    -b, --branch <BRANCH>       branch to sync in branch mode, eg: 'main'
        --timeout-secs <N>     Git command timeout in seconds
        --workspace <PATH>     local checkout path
    -V, --version               Print version information
```

## Notice
Before you begin the task, make sure that you can access and operate your source and target repositories.

The source is cloned into a folder named after the repository. Later runs reuse
that checkout, update its `origin` URL, and update target remotes instead of
adding duplicates. Git arguments are passed directly to Git, so URLs and branch
names are not interpreted by a shell.

## Configuration
You can configure repo-sync using a TOML file. Here's an example:

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
branch = "main"
timeout_secs = 300

# 镜像模式会同步全部 refs，可能强制更新或删除目标仓库中的 refs。
[[sync]]
source = "https://github.com/zcyc/repo-sync.git"
target = ["https://github.com/zcyc/repo-sync-3.git"]
workspace = "./repo-sync-mirror"
mode = "mirror"
crontab = "0/10 * * * * ? *"
timeout_secs = 300
```

- `source`: The source repository URL
- `target`: An array of target repository URLs
- `workspace`: A unique local path used by this sync item
- `mode`: `branch` or `mirror`
- `crontab`: The schedule for synchronization
- `branch`: Required in `branch` mode. The tool clones, fast-forward pulls, and pushes only this branch.
- `timeout_secs`: Maximum time allowed for each Git command.

`mirror` mode uses `git clone --mirror`, fetches all refs, and runs
`git push --mirror`. This can force-update or delete refs on targets; use it
only when targets are disposable mirrors. `branch` mode is the safe default
for ordinary branch fan-out. This configuration format is intentionally
breaking: existing items must add `workspace`, `mode`, and `timeout_secs`.

For one-time runs, omit `crontab`. When using a configuration file, every item
without a schedule runs once; scheduled items continue running in the scheduler.
Configuration errors are rejected before any sync starts. A failed target does
not prevent other targets from being attempted, and Git never waits for an
interactive terminal credential prompt.

## Why Not
- [git-sync](https://github.com/kubernetes/git-sync) of `kubernetes` only synchronizes the repository into the folder.
- [Repository mirroring](https://docs.gitlab.com/ee/user/project/repository/mirror/) of `GitLab` requires a paid version.
