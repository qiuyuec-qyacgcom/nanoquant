# 执行内核 sidecar 协议（v0.1 · 影子模式）

> 目标：把「签名 + 下单 + 保护单 + 落库」从 Python 主循环搬到 Rust，
> 先只做**签名与预检**（影子模式），逐字节比对通过后再接管真实下单。

## 1. 传输

| 环境 | 目标 | 说明 |
|---|---|---|
| Linux / 生产 | `unix:/run/nanoquant/exec.sock` | AF_UNIX，低延迟、无端口暴露 |
| Windows 开发 | `tcp:127.0.0.1:9101` | Windows 的 asyncio 不支持 unix socket，用回环 TCP 验证协议 |

Python 侧通过 `EXEC_SOCKET` 指定；Rust 侧通过 `EXEC_SOCKET`（unix）或 `EXEC_TCP`（tcp）监听。

## 2. 帧格式

**JSON Lines**：一行一个请求，一行一个响应，UTF-8，以 `\n` 结束。

```json
{"id":"a1b2c3d4","op":"plan","query":"symbol=BTCUSDT&side=BUY&type=MARKET&timestamp=1700000000000"}
{"id":"a1b2c3d4","ok":true,"signature":"9Z7/7rbp...","algo":"ed25519","query":"symbol=BTCUSDT&..."}
```

## 3. 操作

| op | 入参 | 出参 | 说明 |
|---|---|---|---|
| `health` | — | `algo` | 探活，返回当前密钥类型 |
| `plan` / `sign` | `query` | `signature` / `algo` / `query` | **只签名，不发单**（影子模式） |

错误响应：`{"id":"...","ok":false,"error":"..."}`，调用方应视为失败而非重试。

## 4. 签名规则（必须与 Python 逐字节一致）

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
| `off`（默认） | 无 | 只用 Python 本地签名，不连 sidecar |
| `shadow` | 后台任务，**零阻塞** | 每次签名另送一份给 sidecar 比对，只统计 |
| `enforce` | 同步，通常 +0.5–1ms | 不一致**立刻拒绝下单**（转 `-1022`） |

语义约定：

1. `shadow` 下比对投递到后台（并发上限 32），抽样比例由 `exec_sample_pct` 控制；
2. 比对内容 = `signature` 逐字节 + `query` 回显，任一不一致即计入 `mismatched`；
3. **只有不一致 fail-closed**；sidecar 不可用（连接失败/超时）一律降级回 Python 签名，
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

> 侧车只持有 Secret 并回传签名，不接触账户、持仓与订单。

## 6. 阶段计划

| 阶段 | 内容 | 退出标准 |
|---|---|---|
| P2 | 协议 + 签名（影子） | ✅ 已完成：Rust 二进制已编译（`cargo test` 3 passed），真实 Ed25519 签名逐字节一致（88 字符 base64），`shadow` p99 1.18ms / `enforce` p99 0.40ms，sidecar 崩溃 3s 内降级且不影响主流程 |
| P3（下一步） | 接管 `place_order` / 保护单，单写线程落库 | demo 全量跑 24h 无差异 |
| P4 | 多标的并发 + 连接预热 | p99 达标、无重复单 |

## 7. 风险控制

- sidecar 不可用时 **降级回 Python 签名**（`exec_mode=off` 或自动降级 + 5s 静默），不阻塞交易；
- 幂等键仍用 `clientOrderId`，sidecar 崩溃重启后按 cid 向交易所查询收敛；
- 任何签名不一致都必须**拒绝下单**，不能"猜"。
