# 生产部署（systemd）

> **就地部署，不依赖 /opt**：主服务与 sidecar 都直接放在 `git clone` 出来的仓库根目录下，
> 用仓库自带的脚本自动生成 systemd unit。本目录里的两个 `.service` 仅是**模板参考**，
> 正常部署请勿手抄。

## 主服务

```bash
git clone https://github.com/qiuyuec-qyacgcom/nanoquant.git
cd nanoquant
sudo bash scripts/install.sh --systemd --password '你的密码'
```

`install.sh --systemd` 会在 clone 目录里建 venv、编译前端，并动态生成
`/etc/systemd/system/nanoquant.service`（`WorkingDirectory`/`ExecStart` 都指向 clone 目录）。

## 执行内核 sidecar（可选）

```bash
# Secret 用单引号包住，避免 shell 解释；与 Python 侧交易所 API secret 是同一把密钥
sudo bash scripts/setup_sidecar.sh --secret '<base64 PKCS#8 或 HMAC secret>'
```

脚本会检测/安装 Rust、`cargo build --release`，把二进制装到 clone 目录 `exec-service`，
写 `/etc/nanoquant/exec.env`（0600），并动态生成 `/etc/systemd/system/nanoquant-exec.service`
（`ExecStart` 指向 clone 目录的 `exec-service`）。

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
