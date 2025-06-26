![reposync logo](/images/logo.png)

# repo-sync
A lightweight synchronization tool for git repositories.

## Manual
```
USAGE:
    repo-sync [OPTIONS]

OPTIONS:
    -c, --crontab <CRONTAB>     crontab string, eg: '0 * * * * ? *'
    -f, --file <FILE>           config file path, eg: ./config.json
    -h, --help                  Print help information
    -s, --source <SOURCE>       source repo, eg: https://github.com/zcyc/repo-sync.git
    -t, --target <TARGET>...    target repo, eg: https://github.com/zcyc/repo-sync.git
    -b, --branch <BRANCH>       specific branch to sync, eg: 'main'
    -V, --version               Print version information
```

## Notice
Before you begin the task, make sure that you can access and operate your source and target repositories.

## Configuration
You can configure repo-sync using a JSON file. Here's an example:

```json
[
    {
        "source": "https://github.com/zcyc/repo-sync.git",
        "target": [
            "https://github.com/zcyc/repo-sync-1.git",
            "https://github.com/zcyc/repo-sync-2.git"
        ],
        "crontab": "0/10 * * * * ? *",
        "branch": "main"
    },
    {
        "source": "https://github.com/zcyc/repo-sync.git",
        "target": [
            "https://github.com/zcyc/repo-sync-3.git"
        ],
        "crontab": "0/10 * * * * ? *"
    }
]
```

- `source`: The source repository URL
- `target`: An array of target repository URLs
- `crontab`: The schedule for synchronization
- `branch`: (Optional) Specific branch to sync. If provided, the tool will:
  - Clone the repository with the specified branch (`git clone -b <branch>`)
  - Pull from the specified branch (`git pull origin <branch>`)
  - Push only the specified branch to target repositories (`git push target <branch>`)
  - If not provided, all branches will be synced (`git push --all`)

## Why Not
- [git-sync](https://github.com/kubernetes/git-sync) of `kubernetes` only synchronizes the repository into the folder.
- [Repository mirroring](https://docs.gitlab.com/ee/user/project/repository/mirror/) of `GitLab` requires a paid version.
