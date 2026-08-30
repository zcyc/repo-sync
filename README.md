![reposync logo](/images/logo.png)

# repo-sync

把一个 Git 仓库同步到一个或多个目标仓库，支持分支同步、完整镜像、定时任务和 Webhook 触发。

## 先选用法

| 场景 | 推荐方式 |
| --- | --- |
| 长期运行、管理多个任务 | `--serve` + Web 管理页面 |
| 手动执行一次同步 | 直接传 `--source`、`--target` 等参数 |
| 一次执行任务数据库里的所有任务 | `--once` |
| 查看状态或维护 SQLite | `--status`、`--events`、`--backup-*` 等维护命令 |

任务配置只存 SQLite，不读取 `config.toml`、JSON 或 TOML 配置文件。

完整参数列表和当前版本号：

```sh
repo-sync --help
repo-sync --version
```

## 最快开始：Web 管理页面

### 1. 构建

```sh
cargo build --release
```

### 2. 启动

```sh
./target/release/repo-sync \
  --database ./repo-sync-tasks.sqlite3 \
  --serve 127.0.0.1:8080
```

打开 <http://127.0.0.1:8080/>，首次访问时设置管理员账号和密码，然后点击“新建任务”。任务保存后立即生效。

Web 表单已经提供安全的常用默认值：

| 配置 | 默认值 |
| --- | --- |
| `mode` | `branch` |
| `branches` | 空，即所有分支 |
| `timeout_secs` | `300` |
| `divergence` | `fail` |
| `tag_policy` | `preserve` |
| `atomic` | `true` |
| `max_retries` / `retry_backoff_secs` | `3` / `5` |
| `failure_cooldown_secs` | `60` |
| `webhook_max_pending_events` / `webhook_event_lease_secs` | `10000` / `900` |
| `dry_run`、`sync_lfs`、清理和破坏性操作 | 默认关闭 |

管理员密码长度必须为 12–256 个字符。密码和 Webhook secret 不会写入任务 SQLite；任务中只保存 secret 的环境变量名。

## 直接执行一次同步

CLI 直连模式没有同步参数默认值，下面是一条可直接改写的安全起步命令。省略 `--crontab` 时执行一次后退出：

```sh
repo-sync \
  --source 'https://github.com/example/source.git' \
  --target 'https://github.com/example/target.git' \
  --workspace './source-workspace' \
  --mode branch \
  --all-branches \
  --timeout-secs 300 \
  --divergence fail \
  --tag-policy preserve \
  --max-retries 3 \
  --retry-backoff-secs 5 \
  --failure-cooldown-secs 60 \
  --webhook-max-pending-events 10000 \
  --webhook-event-lease-secs 900
```

多个目标直接放在同一个 `--target` 后面：

```sh
--target 'https://example.com/team/a.git' 'https://example.com/team/b.git'
```

已有 workspace 必须是与 `--mode` 匹配的 Git 仓库，且 `origin` 必须指向同一个 source。不要把 workspace 设为当前目录 `.` 或父目录 `..`。首次同步会创建 workspace，后续同步复用它。

先检查访问权限，不执行同步：

```sh
repo-sync <上面的参数> --check
```

连目标写权限也检查：

```sh
repo-sync <上面的参数> --check --check-write
```

## 同步模式和安全策略

### `branch`：普通分支分发，推荐

必须选择分支：

```sh
--branches main 'release/*'
```

或者同步所有分支：

```sh
--all-branches
```

目标分支比 source 多出提交时，`--divergence` 决定处理方式：

| 值 | 行为 |
| --- | --- |
| `fail` | 报错，不覆盖目标分支 |
| `keep` | 跳过这个目标分支，继续其他分支和目标 |
| `force` | 使用 `force-with-lease` 更新目标分支 |

标签冲突由 `--tag-policy` 决定：

| 值 | 行为 |
| --- | --- |
| `preserve` | 保留目标标签，跳过冲突 |
| `fail` | 遇到冲突立即报错 |
| `force` | 使用 `force-with-lease` 更新目标标签 |

### `mirror`：完整镜像，只用于可丢弃目标

```sh
repo-sync \
  --source 'https://github.com/example/source.git' \
  --target 'https://example.com/mirror.git' \
  --workspace './mirror-workspace' \
  --mode mirror \
  --timeout-secs 300 \
  --divergence fail \
  --tag-policy force \
  --max-retries 3 \
  --retry-backoff-secs 5 \
  --failure-cooldown-secs 60 \
  --webhook-max-pending-events 10000 \
  --webhook-event-lease-secs 900 \
  --dry-run
```

`mirror` 会使用 `git clone --mirror`，同步选中的所有 refs，并删除目标中 source 没有的对应 refs。它不能同时设置 `--branches`、`--prune-branches` 或 `--prune-tags`，且 `tag_policy` 必须为 `force`。`divergence` 在 mirror 模式中不参与 ref 计划；CLI 直连模式仍需提供它。

真正写入 mirror 目标前，去掉 `--dry-run`，并明确允许破坏性操作：

```sh
--allow-destructive
```

`mirror`、`divergence=force`、`tag_policy=force`、`--prune-branches` 和 `--prune-tags` 都属于破坏性操作。非 dry-run 时必须同时设置 `--allow-destructive`。普通分支分发通常不需要它。

## refs 筛选

这两个选项接收完整 ref glob，每个模式一个参数：

```sh
--include-refs 'refs/heads/*' \
--exclude-refs 'refs/heads/tmp/*' 'refs/tags/nightly-*'
```

- `include_refs` 为空表示全部 refs。
- `exclude_refs` 优先级更高；被排除的 ref 不会同步。
- 模式必须以 `refs/` 开头。
- shell 中的 `*`、`?` 请加引号。
- `branch` 模式还会先经过 `--branches` 的分支名筛选。

## 其他同步选项

| 参数 | 用途 |
| --- | --- |
| `--dry-run` | 计算并显示 push 计划，不写入目标 refs |
| `--sync-lfs` | 同步 Git LFS 对象；运行环境必须安装 Git LFS |
| `--prune-branches` | 删除目标中 source 没有的、且在筛选范围内的分支 |
| `--prune-tags` | 删除目标中 source 没有的、且在筛选范围内的标签 |
| `--atomic` | 要求单个目标上的 ref 更新使用 atomic push |
| `--crontab EXPR` | 按项目使用的 schedule 语法定时执行；不传则不定时 |

`--atomic` 只覆盖“一个目标的一次 ref push”，多个目标、LFS 传输和 SQLite 状态更新不是一个事务。

## 任务数据库模式

使用 `--serve` 时，任务来自 `--database`：

```sh
repo-sync --database /var/lib/repo-sync/repo-sync.sqlite3 --serve 127.0.0.1:8080
```

启用后的任务行为：

- 没有 `crontab`：等待 Webhook 或页面上的“立即同步”。
- 有 `crontab`：按计划自动入队。
- 页面上的“立即同步”、取消、重试和启停会立即生效。
- `--once`：把当前加载的任务各执行一次，然后退出。

任务数据库和 workspace 状态库是分开的：

| 文件 | 内容 |
| --- | --- |
| `--database` 指定的 SQLite | 任务、管理员账号、会话 |
| `<workspace 名>.sqlite3` | 目标状态、运行历史、Webhook 队列和事件 |

删除任务不会删除 workspace，也不会删除 workspace 的状态库。一个 workspace 只能属于一个任务。

## Webhook

### 配置

1. 在任务的“高级选项”中，把 `webhook_secret_envs` 设置为环境变量名，例如 `REPO_SYNC_WEBHOOK_SECRET_MAIN`。
2. 启动 listener 前设置对应的 secret：

   ```sh
   export REPO_SYNC_WEBHOOK_SECRET_MAIN='replace-with-a-long-random-secret'
   repo-sync --database ./repo-sync-tasks.sqlite3 --serve 127.0.0.1:8080
   ```

3. 在 GitHub 或 GitLab 中把 Webhook URL 指向 listener，例如 `https://sync.example.com/webhook`。

支持的事件和签名：

| 提供方 | 事件 | 签名 |
| --- | --- | --- |
| GitHub | `push`、`delete` | `X-Hub-Signature-256` |
| GitLab | `Push Hook`、`Tag Push Hook` | `Webhook-Signature` 或 `X-Gitlab-Token` |

事件必须同时匹配任务的 source 和 refs 筛选，才会入队。listener 收到并写入 SQLite 队列后返回 `202`，Git 同步在后台执行；重复 delivery 会去重。

### Listener 地址

| 地址 | 用途 |
| --- | --- |
| `GET /` | 管理页面 |
| `GET /healthz` | 存活检查 |
| `GET /readyz` | 就绪检查 |
| `GET /metrics` | Prometheus 文本指标 |
| `POST /webhook` | GitHub/GitLab Webhook；POST 路径可使用反向代理统一规划 |

listener 自身不提供 TLS。对外提供 Webhook 或管理页面时，放在已有 TLS 反向代理后面，并限制 `/metrics` 的访问范围。配置更新后发送 `SIGHUP` 可重新加载任务和轮换 secret，无需停止进程。

## 状态与维护

```sh
# 文本状态
repo-sync --database ./repo-sync-tasks.sqlite3 --status

# JSON 状态，适合脚本和监控
repo-sync --database ./repo-sync-tasks.sqlite3 --status --json

# 每个 workspace 最近 50 条 Webhook 事件
repo-sync --database ./repo-sync-tasks.sqlite3 --events
repo-sync --database ./repo-sync-tasks.sqlite3 --events --json

# 重试失败或 dead 事件；ID 跨多个 workspace 重复时加 --workspace
repo-sync --database ./repo-sync-tasks.sqlite3 --retry-event 42
repo-sync --database ./repo-sync-tasks.sqlite3 --workspace ./source-workspace --retry-event 42

# 只删除已结束且超过 30 天的历史；最小值为 7
repo-sync --database ./repo-sync-tasks.sqlite3 --prune-history-days 30

# 备份；目标文件必须不存在，不会覆盖已有文件
repo-sync --database ./repo-sync-tasks.sqlite3 --backup-tasks ./tasks-backup.sqlite3
repo-sync --database ./repo-sync-tasks.sqlite3 --backup-state ./workspace-state-backup.sqlite3
```

`--prune-history-days` 和 `--backup-state` 需要恰好加载一个同步任务；备份前确保目标路径不存在。丢失管理员密码时，先停止 listener，再执行：

```sh
repo-sync --database /var/lib/repo-sync/repo-sync.sqlite3 --reset-admin
```

这只清除管理员账号和会话，不删除任务、workspace 或运行历史；下次打开页面时重新设置账号。

## Linux systemd（可选）

项目提供了 [repo-sync.service.example](repo-sync.service.example) 和 [webhook.env.example](webhook.env.example)。最小安装步骤：

```sh
sudo install -d -o repo-sync -g repo-sync /var/lib/repo-sync /etc/repo-sync
sudo install -m 600 webhook.env.example /etc/repo-sync/webhook.env
sudo install -m 644 repo-sync.service.example /etc/systemd/system/repo-sync.service
sudo systemctl daemon-reload
sudo systemctl enable --now repo-sync
```

查看日志：

```sh
sudo journalctl -u repo-sync -f
```

服务示例使用 `/var/lib/repo-sync` 保存任务数据库和 workspace，使用 `/etc/repo-sync/webhook.env` 保存 secret。修改任务数据库后执行 `sudo systemctl reload repo-sync`。

每日任务数据库备份还可以使用 [repo-sync-backup.sh.example](repo-sync-backup.sh.example)、[repo-sync-backup.service.example](repo-sync-backup.service.example) 和 [repo-sync-backup.timer.example](repo-sync-backup.timer.example)。备份模板不会自动删除旧备份。

## 认证、凭据和升级注意事项

- HTTP(S) 仓库 URL 不能包含用户名或密码；使用 SSH agent 或 Git credential helper。
- 配置错误会在同步开始前拒绝；单个目标失败不会阻止其他目标继续尝试。
- Git 不会等待交互式凭据输入。
- 当前版本的 SQLite schema 不兼容旧数据库；旧的 `--file`、`config.toml`、JSON/TOML 配置加载器、`REPO_SYNC_ADMIN_TOKEN`、旧的全局 `REPO_SYNC_WEBHOOK_SECRET` 和通用 `X-Repo-Sync-Secret` 均不再支持。旧数据库不会迁移，需要移除后重新创建任务库。

## License

见 [LICENSE](LICENSE)。
