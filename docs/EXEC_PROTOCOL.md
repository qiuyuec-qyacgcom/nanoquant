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

## 5. 影子模式比对流程

1. Python 在真实下单前，把同一个 `query` 发给 sidecar 的 `plan`；
2. 比对 `signature` 与本地 `sign_query()` 是否**完全一致**，`query` 回显是否一致；
3. 不一致 → 记录差异并**阻断真实下单**（fail closed）；
4. 连续 N 笔一致后，才允许把 `EXEC_BACKEND` 切到 `rust`。

## 6. 阶段计划

| 阶段 | 内容 | 退出标准 |
|---|---|---|
| P2（当前） | 协议 + 签名（影子） | 签名逐字节一致；sidecar 崩溃不影响主流程 |
| P3 | 接管 `place_order` / 保护单，单写线程落库 | demo 全量跑 24h 无差异 |
| P4 | 多标的并发 + 连接预热 | p99 达标、无重复单 |

## 7. 风险控制

- sidecar 不可用时 **降级回 Python 签名**（`EXEC_BACKEND=python`），不阻塞交易；
- 幂等键仍用 `clientOrderId`，sidecar 崩溃重启后按 cid 向交易所查询收敛；
- 任何签名不一致都必须**拒绝下单**，不能"猜"。
