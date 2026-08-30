![reposync logo](/images/logo.png)

# repo-sync

把一个 Git 仓库同步到一个或多个目标仓库，支持分支同步、完整镜像、定时任务和 Webhook 触发。

## 先选用法

| 场景 | 推荐方式 |
| --- | --- |
| 长期运行、管理多个任务 | `--serve` + Web 管理页面 |
| 手动执行任务 | Web 页面中的“立即同步” |
| 检查或维护 SQLite | `--check`、`--backup-*` 等维护命令 |

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

## 发布

推送版本标签会自动编译并创建 GitHub Release，当前发布 Linux x86_64 产物：

```sh
git tag v1.0.0
git push origin v1.0.0
```

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

## 在 Web 页面创建任务

点击“新建任务”，先填写这 4 项：

| 字段 | 填什么 |
| --- | --- |
| 源仓库 URL 或路径 | source Git 仓库 |
| 目标仓库 | 一个或多个 target，每行一个 |
| Workspace | 本地工作目录；不能是 `.` 或 `..` |
| 模式 | 普通分发选 `branch`；可丢弃的完整镜像选 `mirror` |

已有 workspace 必须是与模式匹配的 Git 仓库，且 `origin` 必须指向同一个 source。首次同步会创建 workspace，后续同步复用它。

保存任务后可以点击“立即同步”。保存、启停、取消和重试都会立即生效。

## 同步模式和安全策略

### `branch`：普通分支分发，推荐

在“分支”字段中填写分支名或 glob，每行一个，例如 `main`、`release/*`；留空表示所有分支。

目标分支比 source 多出提交时，“分支分叉策略”决定处理方式：

| 值 | 行为 |
| --- | --- |
| `fail` | 报错，不覆盖目标分支 |
| `keep` | 跳过这个目标分支，继续其他分支和目标 |
| `force` | 使用 `force-with-lease` 更新目标分支 |

标签冲突由“标签冲突策略”决定：

| 值 | 行为 |
| --- | --- |
| `preserve` | 保留目标标签，跳过冲突 |
| `fail` | 遇到冲突立即报错 |
| `force` | 使用 `force-with-lease` 更新目标标签 |

### `mirror`：完整镜像，只用于可丢弃目标

`mirror` 会使用 `git clone --mirror`，同步选中的所有 refs，并删除目标中 source 没有的对应 refs。它不能设置分支筛选或分支/标签清理，且标签冲突策略必须为 `force`。真正写入前先勾选“Dry run”验证计划，再取消 Dry run 并勾选“允许破坏性操作”。

`mirror`、分叉策略 `force`、标签策略 `force` 以及分支/标签清理都属于破坏性操作。普通分支分发通常不需要“允许破坏性操作”。

## refs 筛选

在“包含 refs”和“排除 refs”字段中填写完整 ref glob，每行一个，例如 `refs/heads/*`、`refs/tags/nightly-*`。

- `include_refs` 为空表示全部 refs。
- `exclude_refs` 优先级更高；被排除的 ref 不会同步。
- 模式必须以 `refs/` 开头。
- shell 中的 `*`、`?` 请加引号。
- `branch` 模式还会先经过“分支”字段的分支名筛选。

## 高级选项

| 字段 | 用途 |
| --- | --- |
| `dry_run` | 计算并显示 push 计划，不写入目标 refs |
| `sync_lfs` | 同步 Git LFS 对象；运行环境必须安装 Git LFS |
| `prune_branches` / `prune_tags` | 删除目标中 source 没有的、且在筛选范围内的分支/标签 |
| `atomic` | 要求单个目标上的 ref 更新使用 atomic push |
| `crontab` | 按项目使用的 schedule 语法定时执行；留空则不定时 |
| `timeout_secs` | Git 命令超时，默认 300 秒 |
| `max_retries` / `retry_backoff_secs` | 失败后的额外重试次数和初始间隔，默认 3 / 5 |
| `failure_cooldown_secs` | 所有目标持续失败时暂停定时任务，默认 60 秒 |
| `webhook_max_pending_events` / `webhook_event_lease_secs` | Webhook 队列上限和事件租约，默认 10000 / 900 秒 |

`atomic` 只覆盖“一个目标的一次 ref push”，多个目标、LFS 传输和 SQLite 状态更新不是一个事务。

## 任务数据库模式

CLI 不再接收 source、target 或任何同步配置参数，也不执行同步任务。所有任务只能在 Web 页面创建、修改和手动执行；CLI 只负责启动服务和维护数据库。

使用 `--serve` 时，任务来自 `--database`：

```sh
repo-sync --database /var/lib/repo-sync/repo-sync.sqlite3 --serve 127.0.0.1:8080
```

启用后的任务行为：

- 没有 `crontab`：等待 Webhook 或页面上的“立即同步”。
- 有 `crontab`：按计划自动入队。
- 页面上的“立即同步”、取消、重试和启停会立即生效。
- CLI 启动时不会自动执行任务；定时任务和 Webhook 由 `--serve` 后台处理。

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
| `POST /webhook` | GitHub/GitLab Webhook；POST 路径可使用反向代理统一规划 |

listener 自身不提供 TLS。对外提供 Webhook 或管理页面时，放在已有 TLS 反向代理后面。配置更新后发送 `SIGHUP` 可重新加载任务和轮换 secret，无需停止进程。

## 状态与维护

状态、Webhook 事件、队列和最近运行记录统一在 Web 页面查看；任务的创建、修改、立即执行、取消和重试也只在 Web 页面完成。

```sh
# 检查任务配置、workspace 和仓库访问
repo-sync --database ./repo-sync-tasks.sqlite3 --check
repo-sync --database ./repo-sync-tasks.sqlite3 --check --check-write

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
