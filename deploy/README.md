# 生产部署（Linux）

> **就地部署，不依赖 /opt**：主服务与 sidecar 都直接放在 `git clone` 出来的仓库根目录下，
> 用仓库自带的脚本自动生成 systemd unit。本目录里的两个 `.service` 仅是**模板参考**，
> 正常部署请勿手抄。

## 系统要求

- 仅支持 Linux：Debian/Ubuntu、RHEL/Fedora、Alpine、Arch、openSUSE。
- systemd 或 nohup 二选一；systemd 模式需要 root。
- Rust 1.88+、Node.js 20+、gcc/make/perl/curl/git/pkg-config/acl；安装脚本会默认自动补齐缺失工具。
- 仓库可直接放在 `/root` 下；安装脚本会自动为 `nanoquant` 授予路径穿越权限（ACL 优先，失败回退最小 chmod）。
- systemd 模式默认创建 `nanoquant` 系统用户；sidecar 必须使用同一用户。

## 已部署机器如何更新

优先用专用更新脚本；`scripts/install.sh` 现在也会检测前端源码比 dist 新并自动重建，不再固定跳过前端。

```bash
cd ~/nanoquant
bash scripts/update.sh --check            # 先看版本与工作树状态（只读）
sudo bash scripts/update.sh               # 拉代码 + 可选备份 + 构建 + 重启 + 健康检查
```

常用参数：

| 参数 | 用途 |
|---|---|
| `--check` | 只预览远端版本与工作树状态，不动文件 |
| `--no-backup` | 跳过更新前备份（默认在 `data/backups/` 建 `pre-update` 备份） |
| `--no-frontend` | 只更新后端（已有可用 dist 时） |
| `--with-sidecar` | 顺带重编并重装 Rust sidecar（之前装过才需要） |
| `--force` | 已是最新也强制重建前端并重启 |

安全边界：有未提交改动会**中止**；`data/`、`.master_key` 与数据库从不覆盖；
健康检查失败时 `install.sh` 会尝试恢复旧二进制/旧 unit。

非 systemd（nohup 部署）同样适用，脚本会调用 `scripts/stop.sh` / `start.sh --daemon`。

## 主服务

一条命令在线安装（`curl` 缺失自动改用 `wget`），无需先 clone：

```bash
if [ -f /usr/bin/curl ];then curl -fsSL --proto '=https' --tlsv1.2 https://raw.githubusercontent.com/qiuyuec-qyacgcom/nanoquant/main/install.sh -o install.sh;else wget --https-only -O install.sh https://raw.githubusercontent.com/qiuyuec-qyacgcom/nanoquant/main/install.sh;fi;bash install.sh --password '你的密码' --systemd
```

引导脚本 `install.sh` 下载代码后转交 `scripts/install.sh --systemd`：在 clone 目录里构建 Rust
二进制 `nanoquant`、编译前端，并动态生成 `/etc/systemd/system/nanoquant.service`
（`WorkingDirectory`/`ExecStart` 都指向 clone 目录）。默认创建并切换到 `nanoquant` 系统用户。

### 安装目标

安装只部署 Rust 二进制 `nanoquant`（主服务），运行阶段不需要任何解释器。`deploy/nanoquant.service`
的 `ExecStart` 直接指向该二进制。

非交互场景用参数：

```bash
bash install.sh --systemd
NANOQUANT_ADMIN_PASSWORD='***' bash install.sh --systemd
bash install.sh --password-stdin --systemd < /root/nq-password
```

安装参数：

| 参数 | 用途 |
|---|---|
| `--systemd` | 强制 systemd；创建 `nanoquant` 用户并启动服务（需要 root） |
| `--no-systemd` | 强制 nohup；使用 `scripts/start.sh --daemon` |
| `--install-deps` | 兼容旧参数；默认已自动安装系统依赖 |
| `--install-rust` | 兼容旧参数；默认已自动安装 Rust |
| `--install-node` | 兼容旧参数；默认已自动安装 Node.js 20+ |
| `--no-auto-deps` | 关闭自动安装；缺失依赖时直接报错 |
| `--password <p>` | 首次启动管理员密码；建议改用 `--password-stdin` 或环境变量 |
| `--password-stdin` | 从 stdin 读取密码，避免 shell 历史 |
| `--password-file <f>` | 从文件首行读取密码，并收紧为 0600 |
| 未提供密码 | 自动生成 20 位初始密码，安装成功后仅终端打印一次 |
| 已有管理员 | 已有密码时不会覆盖；重置使用 `nanoquant ops reset-password` |
| `--port <p>` | 覆盖端口；未指定时保留已有 `data/.port`，否则 8899 |
| `--no-frontend` | 只处理后端；要求已有 `frontend/dist/index.html` |
| `--force-frontend` | 即使 dist 已存在也重新构建前端 |
| `--skip-build` | 复用已编译的 Rust 二进制 |
| `--skip-deps` | 跳过工具链检查（已知环境） |
| `--offline` | cargo/npm 离线模式（依赖需已缓存） |
| `--user <name>` | systemd 运行用户；需已存在，默认自动创建 `nanoquant` |
| `--no-start` | 只安装，不启动服务 |
| `--no-restart` | 不重启已运行服务（新二进制不会生效） |
| `--check` | 只检查环境与产物，不修改文件 |

> sidecar 与主服务共用 0600 的 UDS；systemd 模式下两者必须使用同一用户。`scripts/setup_sidecar.sh`
> 默认复用已存在的 `nanoquant` 用户；自定义 `--user` 时也要给 sidecar 传同一个 `--user`。

## 执行内核 sidecar（补装 / 重建）

面板填好 API 凭证后，一条命令即可（自动读取**当前交易环境**的 Secret，不必手工复制）：

```bash
sudo bash scripts/setup_sidecar.sh --from-settings
```

> 默认复用 systemd 主服务的 `nanoquant` 用户；自定义用户时传 `--user <name>`，且必须与主服务一致。
> UDS 权限为 0600，两个服务不同用户时主服务无法连接。

也可以显式给密钥（与面板「参数配置 → 数据源」保存的交易所 API secret 是同一把）：

```bash
sudo bash scripts/setup_sidecar.sh --secret '<base64 PKCS#8 或 HMAC secret>'
```

脚本会检测/安装 Rust、`cargo build --release`（已编译可加 `--skip-build` 复用），
把二进制装到 clone 目录 `exec-service`，写 `/etc/nanoquant/exec.env`（0600），
并生成 `/etc/systemd/system/nanoquant-exec.service`。

> unit 带 `ConditionPathExists=/etc/nanoquant/exec.env`：**没写密钥就不会启动**，
> 而不是启动后 panic 被 `Restart=always` 无限拉起。

## 验证

```bash
systemctl status nanoquant --no-pager
curl -s localhost:8899/api/v1/health
# sidecar：
ls -l /run/nanoquant/exec.sock
EXEC_SECRET='<密钥>' nanoquant ops verify-sidecar --target unix:/run/nanoquant/exec.sock
```

## 启用 sidecar 参与签名

1. 面板「系统诊断 → 执行内核」确认 `target=/run/nanoquant/exec.sock`、`connected=true`；
2. 点「自检」，两侧 88 字符 base64 一致后先把 `exec_mode` 设为 `shadow` 观察；
3. `mismatched` 恒为 0 再切 `enforce`（灰度 24h）；
4. 换 Secret 后必须点「清零」，否则切换窗口内的在途比对会残留不一致计数。

> 保护单代发、台账权威源等 P3 能力的**启用顺序、判据与回滚**见
> [`docs/archive/history/deploy/P3_ROLLOUT.md`](../docs/archive/history/deploy/P3_ROLLOUT.md)。

## 监控（可选）

`GET /api/v1/metrics` 是免鉴权的 Prometheus 文本（可在参数页「系统 → 安全与观测」关闭）：

```yaml
- job_name: nanoquant
  scrape_interval: 15s
  metrics_path: /api/v1/metrics
  static_configs: [{ targets: ['127.0.0.1:8899'] }]
```

建议告警规则：`nanoquant_up == 0`、`nanoquant_dead_letters_stuck > 0`、
`nanoquant_exec_shadow_mismatched > 0`（切了 enforce 时尤其重要）。

## 常用

```bash
systemctl restart nanoquant            # 只重启主服务
journalctl -u nanoquant -f             # 应用日志（另有 logs/nanoquant.log 按 8MB×5 轮转）
journalctl -u nanoquant-exec -n 50     # sidecar 输出
```

> 升级代码后：`git pull` → `cargo build --release --locked --manifest-path rust/nanoquant/Cargo.toml`
> → `cd frontend && npm ci && npm run build` → `systemctl restart nanoquant`。数据与主密钥在 `data/`，不要覆盖。
