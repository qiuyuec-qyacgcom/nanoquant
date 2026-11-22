# 执行内核 exec-core 协议（v1.0）

> exec-core 是 NanoQuant 的**默认执行内核**：独立进程，负责签名校验与七段延迟指标聚合。
> 主服务启动时必须完成 `hello` 握手，否则交易引擎不启动；HTTP 查询与前端照常可用。
> 二期将把下单/保护单切换到内核 op（协议号已预留）。

## 1. 传输

| 环境 | 端点 | 说明 |
|---|---|---|
| Linux / 生产 | `unix:/run/nanoquant/exec.sock` | AF_UNIX，权限 0600，由 systemd `RuntimeDirectory` 创建 |
| Windows 开发 | `tcp:127.0.0.1:9101` | Windows 无 UDS，使用回环 TCP 联调 |

主服务从 `EXEC_SOCKET`（unix）或 `EXEC_TCP`（tcp）读取端点；两者都未配置时交易引擎不启动。

## 2. 帧格式

**JSON Lines**：一行一个请求/响应，UTF-8，以 `\n` 结尾。`id` 由调用方生成，响应原样回传。

```json
{"id":"nq-1","op":"hello","version":"0.1.0"}
{"id":"nq-1","ok":true,"kernel_version":"1.0.0","capabilities":["health","sign","metrics_ingest","metrics_query","metrics_reset"],"metrics_capacity":2000}
```

错误响应：`{"id":"...","ok":false,"error":"..."}`。坏 JSON 返回 `id="unknown"`。

## 3. 操作

| op | 方向 | 入参 | 出参 | 说明 |
|---|---|---|---|---|
| `hello` | 主服务→内核 | `version` | `ok`/`kernel_version`/`capabilities[]`/`metrics_capacity` | 启动握手；失败 = 交易引擎不启动 |
| `health` | 双向 | — | `algo`/`uptime_ms`/`sessions` | 探活；主服务每 5s 一次 |
| `sign` | 主服务→内核 | `query` | `signature`/`algo`/`query` | 签名；`verify-exec-core` 逐字节比对 |
| `metrics_ingest` | 主服务→内核 | `samples:[{segment,ms,ts}]` | `accepted`/`dropped` | 批量上报；未知 segment 拒绝该条 |
| `metrics_query` | 主服务→内核 | — | `segments` | 七段分位数；无样本字段为 null |
| `metrics_reset` | 主服务→内核 | — | `ok` | 清零全部段 |
| `order_open` | 预留 | — | `ok:false`/`error` | 二期实现；一期返回 `reserved for phase 2` |
| `order_close` | 预留 | — | 同上 | 同上 |
| `protection_place` | 预留 | — | 同上 | 同上 |

七段：`total` / `plan` / `sign` / `http` / `wait` / `protect` / `persist`，每段返回
`{count,p50,p95,p99,max,last}`。分位数采用排序后索引法，与主服务探活 `latency_view` 同口径。

### 3.1 七段口径（主服务下单路径）

| segment | 计时范围 |
|---|---|
| `total` | 单循环起点 → settle 返回（失败单到失败点） |
| `plan` | admit → build_live 返回（含 BanGate、参数构造；扣除 reserve 与签名净耗时） |
| `sign` | `sign::signed_query` 净耗时 |
| `http` | `live::submit` 往返 |
| `wait` | `resolve_live` 往返（Unknown 时才有） |
| `protect` | `ensure_protection` 总耗时（tick 级，每次一条样本） |
| `persist` | `reserve_order` + `settle_live` 落库耗时 |

## 4. 签名规则

| 密钥类型 | 判定 | 签名 | 编码 |
|---|---|---|---|
| HMAC | 默认 | HMAC-SHA256(secret, query) | 小写 hex |
| Ed25519 | base64(PKCS#8 DER) 可解析 | Ed25519 sign(query) | 标准 base64（含 `=`） |

- `query` 是**未编码**的参数串（不含 `signature`）。
- 拼 URL 时 `signature` 必须再做一次 URL 编码（`+` 会被服务端当空格）。
- PEM 粘贴形态（多行 / 单行 / 纯 base64）一律先归一化。
- 主服务 `rust/nanoquant/src/sign.rs` 与内核 `rust/exec-core/src/sign.rs` 必须逐字节一致，
  各自的固定向量单测守着这条约束（`ops verify-exec-core` 再做线上比对）。

## 5. 职责边界

| | 主服务（nanoquant） | exec-core |
|---|---|---|
| 签名 | 实际下单用它，并上报 `sign` 净耗时 | 提供独立实现供逐字节校验 |
| 下单 / 保护单 / 持仓 | ✅ | ❌ 一期不接触账户与订单 |
| 指标 | 下单路径七段计时 + 异步批量上报 | 七段环形缓冲与分位数聚合 |
| 故障影响 | 握手失败 → 交易引擎不启动 | 内核掉线 → 主服务自动重连，样本排队/丢弃 |

密钥只在 exec-core 进程内存与主服务各自的解密路径中，不写日志、不进事件流。

## 6. 主服务接口

| 端点 | 用途 |
|---|---|
| `GET /system/exec-core` | `connected`/`configured`/`state`/`last_error`/`target`/`algo`/`counts`/`latency_ms{last,p50,p95,p99,samples}`/`segments`/`dropped`/`verdict` |
| `POST /system/exec-core/self-test` | 未连接 = `error`；已连接且配 Secret = `ok`；已连接无 Secret = `skipped` |
| `POST /system/exec-core/reset` | 清零探活计数，并转发内核 `metrics_reset` |
| `GET /system/exec-core/reconcile` | 只读对账：本地 `orders` 与执行死信 |

`/engine/execution` 与 `/engine/stats` 的延迟读数来自 `segments`（未连接时字段为 null，面板显示「—」）。
面板「执行内核（exec-core）」显示通道状态、探活往返 p50/p95/p99、七段下单延迟与上报丢弃计数。

## 7. 部署与验证

Linux（systemd）：

```bash
sudo bash scripts/setup_core.sh --from-settings
systemctl status nanoquant-exec-core
```

Windows：

```powershell
powershell -ExecutionPolicy Bypass -File scripts\start-core.ps1 -Detach
```

逐字节校验：

```bash
nanoquant ops verify-exec-core --target unix:/run/nanoquant/exec.sock --secret '<密钥>'
# Windows 联调
nanoquant ops verify-exec-core --target tcp:127.0.0.1:9101 --secret '<密钥>'
```

该命令依次检查 `hello`（kernel_version）、`health`（algo）、两笔真实下单 query 的 `sign`
逐字节比对、空 query 必须被拒绝。联调可用 `ops mock-exec-core --port 9101 --secret <密钥>`。

## 8. 风险控制

- **内核掉线**：主服务指数退避重连（250ms 起，上限 8s），期间七段样本在有界队列（8192）排队；
  队列满丢弃并计数，绝不阻塞下单路径。
- **内核重启**：内存指标清零重新累计（一期不做跨进程持久化）。
- **握手失败**：交易引擎不启动，`engine_events` 记录 error，面板红色「未连接」；HTTP/前端照常。
- **签名比对不一致**：必须视为失败，不能猜测。