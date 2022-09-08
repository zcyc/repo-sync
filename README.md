# repo-sync
This is a repositories synchronize tool alternative to gitlab mirroring repositories.

[NOTE] Before you begin the task, make sure that you can access and operate your source and target repositories.

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
