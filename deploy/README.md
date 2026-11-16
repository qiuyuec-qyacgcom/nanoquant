# 生产部署（systemd）

> **就地部署，不依赖 /opt**：主服务与 sidecar 都直接放在 `git clone` 出来的仓库根目录下，
> 用仓库自带的脚本自动生成 systemd unit。本目录里的两个 `.service` 仅是**模板参考**，
> 正常部署请勿手抄。

## 已部署机器如何更新

**不要重跑 `install.sh`**：它发现 `frontend/dist` 已存在会**跳过前端构建**，更新后会继续跑旧界面。
用专用更新脚本（在仓库根目录）：

```bash
cd ~/nanoquant
bash scripts/update.sh --check            # 先看会更新什么（只读，不改任何文件）
sudo bash scripts/update.sh               # 实际更新：拉代码 + 备份 + 依赖 + 强制重建前端 + 重启 + 健康检查
```

常用参数：

| 参数 | 用途 |
|---|---|
| `--check` | 只预览版本差异、变更文件，不动任何东西 |
| `--no-backup` | 跳过更新前备份（默认会在 `data/backups/` 建 `pre-update` 备份） |
| `--no-frontend` | 只更新后端（已有可用 dist 时） |
| `--with-sidecar` | 顺带重编 Rust sidecar（之前装过才需要） |
| `--force` | 已是最新也强制重装依赖 + 重建前端 |

安全边界：有未提交改动会**中止**（先 `git stash`）；`data/` 与 `.master_key` 从不覆盖；
健康检查失败会打印回滚命令（输出末尾的 `git reset --hard <旧版本>`）。

非 systemd（nohup 部署）同样适用，脚本会调用 `scripts/stop.sh` / `start.sh --daemon`。

## 主服务

一条命令在线安装（`curl` 缺失自动改用 `wget`），无需先 clone：

```bash
if [ -f /usr/bin/curl ];then curl -sSO https://raw.githubusercontent.com/qiuyuec-qyacgcom/nanoquant/main/install.sh;else wget -O install.sh https://raw.githubusercontent.com/qiuyuec-qyacgcom/nanoquant/main/install.sh;fi;bash install.sh --password '你的密码' --systemd
```

引导脚本 `install.sh` 下载代码后转交 `scripts/install.sh --systemd`：在 clone 目录里建 venv、
编译前端，并动态生成 `/etc/systemd/system/nanoquant.service`（`WorkingDirectory`/`ExecStart` 都指向 clone 目录）。

### 安装时会选「执行内核」

安装脚本会问一句用哪套执行内核：

```
请选择执行内核：
  1) Python（默认；无需额外配置，装完即用）
  2) Rust（更低的签名延迟；需要填写币安 API Key 与 Secret）
```

- 选 **1 / 直接回车**：走 Python 内核，跳过所有 Rust 步骤，装完即用（推荐首次部署）。
- 选 **2**：要求填写 **API Key + API Secret**（Secret 输入不回显），随后自动完成
  「装 Rust → 编译 → 装二进制 → 写 `/etc/nanoquant/exec.env` → 注册并启动 `nanoquant-exec`」，
  **不需要再手工执行任何 cargo / systemctl 命令**。填写的凭证会在首次启动时加密入库并删除明文。

非交互场景用参数：

```bash
bash install.sh --kernel python          # 显式选 Python
bash install.sh --kernel rust            # 选 Rust（凭证走环境变量或交互）
NANOQUANT_KERNEL=rust NANOQUANT_API_KEY=xxx NANOQUANT_API_SECRET=yyy bash install.sh
```

> 若 Rust 编译失败，脚本会**自动回退 Python 内核**并继续装完主服务，
> 事后可补：`sudo bash scripts/setup_sidecar.sh --from-settings`。

## 执行内核 sidecar（补装 / 重建）

面板填好 API 凭证后，一条命令即可（自动读取**当前交易环境**的 Secret，不必手工复制）：

```bash
sudo bash scripts/setup_sidecar.sh --from-settings
```

也可以显式给密钥（与 Python 侧交易所 API secret 是同一把）：

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
EXEC_SECRET='<密钥>' .venv/bin/python scripts/verify_sidecar.py --target unix:/run/nanoquant/exec.sock
```

## 启用 sidecar 参与签名

1. 面板「系统诊断 → 执行内核」确认 `target=/run/nanoquant/exec.sock`、`connected=true`；
2. 点「自检」，两侧 88 字符 base64 一致后先把 `exec_mode` 设为 `shadow` 观察；
3. `mismatched` 恒为 0 再切 `enforce`（灰度 24h）；
4. 换 Secret 后必须点「清零」，否则切换窗口内的在途比对会残留不一致计数。

> 保护单代发、台账权威源等 P3 能力的**启用顺序、判据与回滚**见
> [`deploy/P3_ROLLOUT.md`](./P3_ROLLOUT.md)。

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

> 升级代码后：`git pull` → `pip install -r backend/requirements.txt` → `cd frontend && npm ci && npm run build`
> → `systemctl restart nanoquant`。数据与主密钥在 `data/`，不要覆盖。
