# P3 启用与回滚手册（Rust 执行内核 sidecar）

> 面向运维。**每一步都可单独回退**，任何一步数据不达标就停在那一步，不要往下走。
> 协议细节见 `docs/EXEC_PROTOCOL.md`；任务记录见 `docs/TASKS.md` #93–#100。

## 0. 前置：P2 已完成

确认 `exec_mode` 已是 `enforce` 且稳定（`mismatched == 0`）。若尚未完成，先按
`deploy/README.md` 走完 P2（部署 → 验证 → shadow → enforce）。

```bash
curl -s -H "Authorization: Bearer <token>" \
  http://127.0.0.1:8899/api/v1/system/exec-sidecar | python3 -m json.tool
```

必须看到：`mode="enforce"`、`connected=true`、`counts.mismatched=0`。

---

## 1. 部署 P3（一次性）

```bash
cd ~/nanoquant && git pull

# sidecar：重编（首次含 SQLite bundled，约 +1.5 分钟）
cd rust/exec-service && cargo test --release && cargo build --release
sudo install -m 0755 target/release/exec-service ~/nanoquant/exec-service

# 主服务：重启（Python 侧有改动）
cd ~/nanoquant && sudo systemctl restart nanoquant-exec && sudo systemctl restart nanoquant
```

启动后应看到：

```bash
journalctl -u nanoquant-exec -n 20 --no-pager | grep -E 'ledger store|http pool'
```

- `exec-service http pool enabled: base=...`（若配置了 `EXEC_BASE_URL`）
- `exec-service ledger store: ...`（若配置了 `EXEC_LEDGER_DB`）

> 重启 sidecar 会有约 3s 连接空窗，`enforce` 下自动降级 Python 签名，`失败` 计数 +1~2 属正常。

### 可选环境变量（`/etc/nanoquant/exec.env`，0600）

| 变量 | 默认 | 作用 | 建议 |
|---|---|---|---|
| `EXEC_BASE_URL` | 空（关闭） | 启用 HTTP/2 连接池 + 预热 | 建议 `https://fapi.binance.com`（模拟盘用 `https://testnet.binancefuture.com`） |
| `EXEC_HTTP_TIMEOUT_MS` | `3000` | 单请求超时 | 保持默认 |
| `EXEC_HTTP_WARMUP` | `1` | 启动预热 | 保持默认 |
| `EXEC_LEDGER_DB` | 空（纯内存） | 台账落库路径 | 建议 `/var/lib/nanoquant/exec-ledger.db` |
| `EXEC_PLACE_MODE` | 未设置 | 允许 sidecar 代发保护单 | **最后一步再开** |

写完后 `sudo systemctl restart nanoquant-exec`。

---

## 2. 阶段 A：观察（**不改任何开关**，建议 ≥ 24h）

启用 `EXEC_BASE_URL` 与 `EXEC_LEDGER_DB` 后，纯观察，把下面三个数当基线。

### A1. 签名比对

```bash
curl -s -H "Authorization: Bearer <token>" \
  http://127.0.0.1:8899/api/v1/system/exec-sidecar \
  | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["counts"], d["protect_counts"], d["order_ledger"]["counts"])'
```

| 指标 | 目标 | 含义 |
|---|---|---|
| `counts.mismatched` | **0** | 签名不一致（P2 既有判据） |
| `protect_counts.mismatched` | **0** | 保护单构造不一致（P3-3 影子对账） |
| `protect_counts.errors` | 0 | 对账投递失败 |
| `order_ledger.counts.errors` | 0 | 台账投递失败 |

### A2. 落库健康

```bash
curl -s -H "Authorization: Bearer <token>" http://127.0.0.1:8899/api/v1/health >/dev/null
# sidecar 侧（通过面板「系统诊断 → 执行内核」看，或直接读 health 响应里的 store 段）
```

| 指标 | 目标 | 含义 |
|---|---|---|
| `store.dropped` | **0** | 队列满丢弃（>0 说明写线程跟不上或磁盘慢） |
| `store.errors` | 0 | 批量提交失败 |
| `store.pending` | 长期接近 0 | 积压 |

### A3. 台账双向对账

```bash
curl -s -H "Authorization: Bearer <token>" \
  'http://127.0.0.1:8899/api/v1/system/exec-sidecar/reconcile?lookback_hours=24' \
  | python3 -m json.tool
```

| 字段 | 目标 | 处理 |
|---|---|---|
| `aligned` | `true` | — |
| `diff_count` | **0** | 见下表 |
| `missing_in_sidecar` | 空 | 多为开关前的历史单；若持续增长 → 投递链断 |
| `missing_in_local` | 空 | 本地写库丢数据 → 查 `write_failures` 死信 |
| `state_mismatch` | 空 | 迁移丢失 → 查 `order_ledger.counts.errors` |
| `qty_mismatch` | 空 | 成交量口径不一致 → 需排查 |

> **`diff_count` 连续多日为 0 是开启权威源（阶段 C）的唯一依据。**

---

## 3. 阶段 B：启用保护单代发

### 前置

`protect_counts.mismatched == 0` 且 `matched` 已积累足够样本（建议 ≥ 50 次开仓）。

### 步骤（两处都要开，缺一不发）

1. sidecar 侧：`exec.env` 增加 `EXEC_PLACE_MODE=enabled` → `sudo systemctl restart nanoquant-exec`
2. Python 侧：参数配置 → 系统 → 执行内核 → 打开「由 sidecar 代发保护单」→ 保存

### 验证

```bash
curl -s -H "Authorization: Bearer <token>" \
  http://127.0.0.1:8899/api/v1/system/exec-sidecar \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["order_ledger"]["protect_counts"])'
```

| 指标 | 目标 |
|---|---|
| `placed` | 随开仓增长 |
| `place_unknown` | **0** |
| `place_failed` | 0 或极少 |

### 失败语义（重要）

| 结果 | 系统行为 | 运维动作 |
|---|---|---|
| 成功 | 不再由 Python 发保护单 | — |
| 明确拒绝（4xx/缺 Key/未启用） | 自动回退 Python 发单 | 看 `place_failed` 原因 |
| **`place_unknown`（超时/5xx/429/断连）** | **阻止重发** + 写 error 事件 | **立即人工核对该标的交易所挂单**，必要时手工补挂 |

### 回滚

任一项不达标：把「由 sidecar 代发保护单」关掉即可（Python 侧立即恢复自建），
必要时再移除 `EXEC_PLACE_MODE`。

---

## 4. 阶段 C：启用台账读取权威源

### 前置

`reconcile.diff_count` **连续多日为 0**。

### 步骤

参数配置 → 系统 → 执行内核 → 打开「台账以 sidecar 为权威」→ 保存。

### 验证

```bash
curl -s -H "Authorization: Bearer <token>" 'http://127.0.0.1:8899/api/v1/engine/orders?limit=5'
```

响应应带 `"ledger_source": "sidecar"`；每笔被覆盖的单带 `ledger_state` 字段。

故意停掉 sidecar 再查一次，应自动变成 `"ledger_source": "local"`（**不能 5xx**）。

### 回滚

关掉开关即可，**无数据影响**（该项只改读取视图，从不写库）。

---

## 5. 全局回滚速查

| 想撤销 | 操作 | 影响 |
|---|---|---|
| 保护单代发 | 关闭「由 sidecar 代发保护单」+ 移除 `EXEC_PLACE_MODE` | 立即恢复 Python 自建保护单 |
| 台账权威源 | 关闭「台账以 sidecar 为权威」 | 立即恢复读本地表 |
| 台账落库 | 移除 `EXEC_LEDGER_DB` | sidecar 回到纯内存，历史库文件可保留或删除 |
| 连接池 | 移除 `EXEC_BASE_URL` | 回到纯签名模式 |
| 整个 sidecar | `exec_mode` 设为 `off` | 只用 Python 签名，sidecar 可停 |

> 回滚顺序建议：**倒着来**（C → B → A），除非是签名不一致（此时直接降 `exec_mode=shadow`）。

---

## 6. 常见故障

| 现象 | 排查 |
|---|---|
| `reconcile` 持续 `missing_in_local` | 查 `write_failures` 死信表；本地写库在丢数据 |
| `store.dropped > 0` | 磁盘慢或写入量过大；查 `journalctl -u nanoquant-exec` 有无事务失败 |
| `place_unknown > 0` | **立即人工核对**该标的交易所挂单，确认是否已挂上；不要点重试 |
| `protect_counts.mismatched > 0` | 两侧构造不一致，先关代发，比对 `last_protect_mismatch` 里的 query |
| sidecar 反复重启 | `journalctl -u nanoquant-exec -n 50`；`EXEC_LEDGER_DB` 路径不可写会降级为纯内存（不致命） |
| 面板 `ledger_source` 一直是 `local` | 开关未保存、或 `exec_mode=off`、或 sidecar 不可用 |

---

## 7. 每日巡检（一条命令）

```bash
TOKEN='<token>'
curl -s -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8899/api/v1/system/exec-sidecar \
  | python3 -c 'import json,sys; d=json.load(sys.stdin); l=d["order_ledger"]; print("mismatch",d["counts"]["mismatched"],"| protect",l["protect_counts"],"| ledger",l["counts"],"| auth",l["authoritative"],"| place",l["place_enabled"])'
curl -s -H "Authorization: Bearer $TOKEN" \
  'http://127.0.0.1:8899/api/v1/system/exec-sidecar/reconcile?lookback_hours=24' \
  | python3 -c 'import json,sys; d=json.load(sys.stdin); print("aligned",d["aligned"],"diff",d["diff_count"],"checked",d["checked"])'
```

目标：`mismatched=0`、`protect.mismatched=0`、`ledger.errors=0`、`aligned=True`、`diff=0`。
