# 生产部署（systemd）

单位文件是**模板**：默认按 `/opt/nanoquant` + `nanoquant` 用户写死，路径不同请先改。

## 一次性准备

```bash
sudo useradd --system --home /opt/nanoquant --shell /usr/sbin/nologin nanoquant
sudo mkdir -p /opt/nanoquant /etc/nanoquant
# 代码 + venv + 前端 dist（任选 git clone 或上传）
sudo rsync -a --exclude data --exclude logs ./ /opt/nanoquant/
sudo -u nanoquant python3 -m venv /opt/nanoquant/.venv
sudo -u nanoquant /opt/nanoquant/.venv/bin/pip install -r /opt/nanoquant/backend/requirements.txt
sudo chown -R nanoquant:nanoquant /opt/nanoquant
```

## 执行内核 sidecar（可选）

```bash
sudo install -m 0755 rust/exec-service/target/release/exec-service /opt/nanoquant/exec-service
# Secret 只写进环境文件，权限 0600；值用单引号包住，避免 shell 解释
sudo sh -c 'umask 077; printf "EXEC_SECRET=%s\n" "<base64 PKCS#8 或 HMAC secret>" > /etc/nanoquant/exec.env'
sudo chown root:root /etc/nanoquant/exec.env
```

## 安装 unit

```bash
sudo cp deploy/nanoquant-exec.service deploy/nanoquant.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now nanoquant-exec nanoquant
systemctl status nanoquant --no-pager
curl -s localhost:8899/api/v1/health
```

## 启用 sidecar 参与签名

1. 面板「系统诊断 → 执行内核」确认 `target=/run/nanoquant/exec.sock`、`connected=true`；
2. 点「自检」，两侧 88 字符 base64 一致后先把 `exec_mode` 设为 `shadow` 观察；
3. `mismatched` 恒为 0 再切 `enforce`（灰度 24h）；
4. 换 Secret 后必须点「清零」，否则切换窗口内的在途比对会残留不一致计数。

## 常用

```bash
systemctl restart nanoquant            # 只重启主服务
journalctl -u nanoquant -f             # 应用日志（另有 logs/nanoquant.log 按 8MB×5 轮转）
journalctl -u nanoquant-exec -n 50     # sidecar 输出
```

> 升级代码后：`git pull` → `pip install -r backend/requirements.txt` → `cd frontend && npm ci && npm run build`
> → `systemctl restart nanoquant`。数据与主密钥在 `data/`，不要覆盖。
