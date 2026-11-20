# 执行内核 sidecar 协议（v0.1 · 影子模式）

> 目标：把「签名 + 下单 + 保护单 + 落库」从 legacy 内核（已退役）搬到 Rust，
> 先只做**签名与预检**（影子模式），逐字节比对通过后再接管真实下单。

## 1. 传输

| 环境 | 目标 | 说明 |
|---|---|---|
| Linux / 生产 | `unix:/run/nanoquant/exec.sock` | AF_UNIX，低延迟、无端口暴露 |
| Windows 开发 | `tcp:127.0.0.1:9101` | Windows 的 asyncio 不支持 unix socket，用回环 TCP 验证协议 |

legacy 内核通过 `EXEC_SOCKET` 指定；Rust 侧通过 `EXEC_SOCKET`（unix）或 `EXEC_TCP`（tcp）监听。

## 2. 帧格式

**JSON Lines**：一行一个请求，一行一个响应，UTF-8，以 `\n` 结束。

```json
{"id":"a1b2c3d4","op":"plan","query":"symbol=BTCUSDT&side=BUY&type=MARKET&timestamp=1700000000000"}
{"id":"a1b2c3d4","ok":true,"signature":"9Z7/7rbp...","algo":"ed25519","query":"symbol=BTCUSDT&..."}
```

## 3. 操作

| op | 入参 | 出参 | 说明 |
|---|---|---|---|
| `health` | — | `algo` / `http` | 探活，返回当前密钥类型与连接池状态（`http` 可忽略） |
| `plan` / `sign` | `query` | `signature` / `algo` / `query` | **只签名，不发单**（影子模式） |
| `probe` | — | `latency_ms` / `http` | P3-1：只读探活连接池（`GET {base}/fapi/v1/time`），不签名、不下单 |
| `order_register` | `client_order_id` / `symbol` / `state?` | `order` / `duplicate` | P3-2：幂等登记订单（重复登记返回原记录、`duplicate=true`） |
| `order_transition` | `client_order_id` / `state` / `filled_qty?` / `avg_price?` | `order` | P3-2：状态迁移，非法迁移 / 终态回退一律 `ok:false` |
| `order_get` | `client_order_id` | `order` | P3-2：查询单笔台账 |
| `order_snapshot` | — | `order` | P3-2：全量台账 + 按状态统计 + `duplicate_hits` |
| `protection_plan` | `symbol` / `close_side` / `stop_price` / `take_price` / `position_side?` / `stop_client_id?` / `take_client_id?` | `protections` | P3-3：并行构造 STOP+TP 两笔保护单的 query+签名，**只构造不发送** |
| `store_status` | — | `store` | P3-4：台账落库统计（pending/written/batches/dropped/errors） |
| `protection_place` | 同 `protection_plan` + `api_key` | `protections`[] | P3-3 收尾：**真正代发**保护单（需 `EXEC_PLACE_MODE=enabled`）|

### 3.4 保护单代发闸门（P3-3 收尾）

**双重闸门**，缺一不发：

| 闸门 | 位置 | 默认 |
|---|---|---|
| `system.exec_place_protection` | 参数配置 → 系统 | `false` |
| `EXEC_PLACE_MODE=enabled` | sidecar 环境变量 | 未设置（拒绝） |

失败语义（决定能否回退 legacy 内核重发）：

| 结果 | 含义 | 能否回退重发 |
|---|---|---|
| `ok=true` | 交易所已受理 | 已发出，**不得再发** |
| `ok=false, unknown=false` | 明确拒绝（4xx / 缺 Key / 未启用） | ✅ 可安全回退 |
| `ok=false, unknown=true` | 超时 / 5xx / 429 / 连接中断 | ❌ **禁止重发**（可能已挂上），记 error 事件 |

> `unknown=true` 时会写一条 error 事件，提示用交易所持仓/挂单人工核对。

### 3.5 台账权威源（P3-5 收尾）

`system.exec_ledger_authoritative`（默认 `false`）：

| 状态 | `GET /engine/orders` 行为 | `ledger_source` |
|---|---|---|
| 关闭 | 完全用本地 `orders` 表（原行为） | `local` |
| 开启 + sidecar 可用 | `status/qty/price` 以 sidecar 台账覆盖 | `sidecar` |
| 开启 + sidecar 不可用 | **自动回退本地表**，绝不 5xx | `local` |

- 只影响**读取视图**，不写任何数据；关闭开关即刻恢复。
- 本地有、sidecar 没有的单仍然返回（不会因权威源而消失）。
- 推荐顺序：`reconcile.diff_count` 连续多日为 0 后再开启。

### 3.3 台账落库（P3-4）

| 变量 | 默认 | 说明 |
|---|---|---|
| `EXEC_LEDGER_DB` | 空（纯内存） | sidecar 自己的台账库路径，如 `/var/lib/nanoquant/exec-ledger.db` |

- **只写 sidecar 自己的两张表**（`exec_orders` / `exec_order_history`），**不动 legacy 内核的 21 张主表**。
- 单写线程 + 批量事务（每批 ≤64），与 legacy 内核 `db_writer` 同取舍：关键路径只付一次入队。
- 队列满（8192）**丢弃并计数**，绝不阻塞下单；`shutdown` 时 drain。
- 打不开库就降级为纯内存，行为与 P3-2 完全一致。

错误响应：`{"id":"...","ok":false,"error":"..."}`，调用方应视为失败而非重试。

### 3.2 订单状态机（P3-2）

状态：`PLANNED → SENT → ACKED → PARTIALLY_FILLED → FILLED`。

| 约束 | 说明 |
|---|---|
| 幂等 | 同一 `client_order_id` 重复 `order_register` 不新增记录，返回 `duplicate=true` |
| 终态 | `FILLED / CANCELED / EXPIRED / REJECTED` 不可再迁移 |
| 禁跨级 | `PLANNED → FILLED` 之类无成交过程的跳转被拒（异常数据） |
| 累计量 | `filled_qty` 只增不减；`avg_price` 仅接受 >0 |
| 别名 | `NEW→ACKED`、`CANCELLED→CANCELED`、`TRADE→FILLED` 等交易所口径可解析 |

> 本阶段**只记账不发单**；终态集合与 `services/order_sync.py::TERMINAL` 保持一致。

### 3.1 连接池环境变量（P3-1，均可选）

| 变量 | 默认 | 说明 |
|---|---|---|
| `EXEC_BASE_URL` | 空（关闭） | 交易所 REST 根地址，如 `https://fapi.binance.com`；为空则只做签名，不建连接池 |
| `EXEC_HTTP_TIMEOUT_MS` | `3000` | 单请求超时（200–30000） |
| `EXEC_HTTP_WARMUP` | `1` | 启动时后台预热（DNS+TLS+HTTP2 握手），失败只告警、不阻断签名 |

连接池失败或预热超时**不会影响** `sign`/`health`：P2 影子比对照常可用。

## 4. 签名规则（必须与 legacy 内核逐字节一致）

| 密钥类型 | 判定 | 签名 | 编码 |
|---|---|---|---|
| HMAC | 默认 | HMAC-SHA256(secret, query) | 小写 hex |
| Ed25519 | base64(PKCS#8 DER) 可解析 | Ed25519 sign(query) | 标准 base64（含 `=`） |

- `query` 是**未编码**的参数串（`urlencode` 之后的形态，不含 `signature`）。
- 拼 URL 时 `signature` 必须再做一次 URL 编码（`+` 会被服务端当空格）。
- PEM 粘贴形态（多行 / 单行 / 纯 base64）一律先归一化。

## 5. 影子模式比对流程（已接入，见 TASKS #55）

配置入口：参数页「系统 → 执行内核（sidecar）」，或 `PUT /settings/system` 的
`exec_mode / exec_socket / exec_sample_pct / exec_timeout_ms`。

| 模式 | 关键路径影响 | 行为 |
|---|---|---|
| `off`（默认） | 无 | 只用内建签名，不连 sidecar |
| `shadow` | 后台任务，**零阻塞** | 每次签名另送一份给 sidecar 比对，只统计 |
| `enforce` | 同步，通常 +0.5–1ms | 不一致**立刻拒绝下单**（转 `-1022`） |

语义约定：

1. `shadow` 下比对投递到后台（并发上限 32），抽样比例由 `exec_sample_pct` 控制；
2. 比对内容 = `signature` 逐字节 + `query` 回显，任一不一致即计入 `mismatched`；
3. **只有不一致 fail-closed**；sidecar 不可用（连接失败/超时）一律降级为内建签名，
   并在 5s 静默期内不再投递，避免每个签名都付一次超时；
4. `enforce` 的切流判据是 `mismatched == 0`，而不是"连上了"；
5. 观测口径：`latency_ms` 是**锁内测得的 sidecar 往返**（不含排队），
   因此 sidecar 掉线时最大延迟 ≈ 超时值，不会出现"排队 N 秒"造成的假长尾。

接口：

| 端点 | 用途 |
|---|---|
| `GET /system/exec-sidecar` | 模式、连接、algo、一致/不一致/失败/跳过、p50/p95/p99、verdict |
| `POST /system/exec-sidecar/self-test` | 用固定 query 与当前环境 Secret 做一次比对（不消耗权重、不下单） |
| `POST /system/exec-sidecar/reset` | 清零计数（换密钥/换环境后重新统计） |
| `GET /system/exec-sidecar/reconcile` | P3-5 前置：sidecar 台账 ↔ `orders` 表**双向对账**（只读） |

> 侧车只持有 Secret 并回传签名，不接触账户、持仓与订单。

## 6. 阶段计划

| 阶段 | 内容 | 退出标准 |
|---|---|---|
| P2 | 协议 + 签名（影子） | ✅ 已完成：Rust 二进制已编译（`cargo test` 3 passed），真实 Ed25519 签名逐字节一致（88 字符 base64），`shadow` p99 1.18ms / `enforce` p99 0.40ms，sidecar 崩溃 3s 内降级且不影响主流程 |
| P3（下一步） | 接管 `place_order` / 保护单，单写线程落库 | demo 全量跑 24h 无差异 |
| P4 | 多标的并发 + 连接预热 | p99 达标、无重复单 |

## 7. 风险控制

- sidecar 不可用时 **降级为内建签名**（`exec_mode=off` 或自动降级 + 5s 静默），不阻塞交易；
- 幂等键仍用 `clientOrderId`，sidecar 崩溃重启后按 cid 向交易所查询收敛；
- 任何签名不一致都必须**拒绝下单**，不能"猜"。
