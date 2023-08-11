# repo-sync
repositories synchronization tool

# Notice
Before you begin the task, make sure that you can access and operate your source and target repositories.

![reposync logo](/images/logo.png)

```
USAGE:
    repo-sync [OPTIONS]

OPTIONS:
    -c, --crontab <CRONTAB>     crontab string, eg: '0 * * * * ? *'
    -f, --file <FILE>           config file path, eg: ./config.json
    -h, --help                  Print help information
    -s, --source <SOURCE>       source repo, eg: https://github.com/zcyc/repo-sync.git
    -t, --target <TARGET>...    target repo, eg: https://github.com/zcyc/repo-sync.git
    -V, --version               Print version information
```

# Why Not
- [git-sync](https://github.com/kubernetes/git-sync) of `kubernetes` only synchronizes the repository into the folder.
- [Repository mirroring](https://docs.gitlab.com/ee/user/project/repository/mirror/) of `GitLab` requires a paid version.
