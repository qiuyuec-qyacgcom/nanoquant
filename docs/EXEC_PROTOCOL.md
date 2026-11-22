# 执行内核 sidecar 协议（v0.2 · 校验用 sidecar）

> 执行内核（sidecar）是**可选**的签名校验组件。主服务用 Rust 内核自己签名下单，
> 不部署 sidecar 也能正常交易；sidecar 提供的是「另一份独立实现的签名能否逐字节对齐」的旁证，
> 外加诊断面板上的探活状态与实测往返耗时。
>
> 历史上这里描述过 `shadow` / `enforce` 两种影子比对与强制路由模式。那是 legacy Python 内核
> 退役、迁移到 Rust 内核期间的过渡脚手架（见 `docs/archive/history/deploy/P3_ROLLOUT.md`），
> 随 Python 内核一并移除，**代码里已不存在**。

## 1. 传输

| 环境 | 目标 | 说明 |
|---|---|---|
| Linux / 生产 | `unix:/run/nanoquant/exec.sock` | AF_UNIX，低延迟、无端口暴露，权限 0600 |
| Windows 开发 | `tcp:127.0.0.1:9101` | Windows 不支持 unix socket，用回环 TCP 验证协议 |

主服务通过 `EXEC_SOCKET`（unix）或 `EXEC_TCP`（tcp）指定端点，只认 TCP 会让 UDS 部署永远显示「未配置」。
未配置时诊断面板显示「未部署」，属于正常状态，**不影响交易**。

## 2. 帧格式

**JSON Lines**：一行一个请求，一行一个响应，UTF-8，以 `\n` 结束。

```json
{"id":"a1b2c3d4","op":"sign","query":"symbol=BTCUSDT&side=BUY&type=MARKET&timestamp=1700000000000"}
{"id":"a1b2c3d4","ok":true,"signature":"9Z7/7rbp...","algo":"ed25519","query":"symbol=BTCUSDT&..."}
```

错误响应：`{"id":"...","ok":false,"error":"..."}`，调用方应视为失败而非重试。

## 3. 操作

| op | 入参 | 出参 | 主服务是否调用 |
|---|---|---|---|
| `health` | — | `algo` | **是**：诊断探活 |
| `sign` / `plan` | `query` | `signature` / `algo` / `query` | **是**：`ops verify-sidecar` 做逐字节比对 |
| `probe` | — | `latency_ms` | 否 |
| `order_register` | `client_order_id` / `symbol` / `state?` | `order` / `duplicate` | 否：sidecar 侧保留的台账能力 |
| `order_transition` | `client_order_id` / `state` / `filled_qty?` / `avg_price?` | `order` | 否 |
| `order_get` / `order_snapshot` | `client_order_id` / — | `order` | 否 |
| `protection_plan` | `symbol` / `close_side` / `stop_price` / `take_price` / ... | `protections` | 否 |
| `protection_place` | 同上 + `api_key` | `protections`[] | 否：需 `EXEC_PLACE_MODE=enabled` |
| `store_status` | — | `store` | 否 |

> 台账与保护单相关 op 在 sidecar 二进制里仍然可用，但 Rust 主服务目前**没有接线**：
> 下单链路走 `crate::sign::order_params` 自己签名、自己发单。想用这些能力需要先补主服务侧的调用。

## 4. 签名规则

| 密钥类型 | 判定 | 签名 | 编码 |
|---|---|---|---|
| HMAC | 默认 | HMAC-SHA256(secret, query) | 小写 hex |
| Ed25519 | base64(PKCS#8 DER) 可解析 | Ed25519 sign(query) | 标准 base64（含 `=`） |

- `query` 是**未编码**的参数串（不含 `signature`）。
- 拼 URL 时 `signature` 必须再做一次 URL 编码（`+` 会被服务端当空格）。
- PEM 粘贴形态（多行 / 单行 / 纯 base64）一律先归一化。
- 主服务的 `rust/nanoquant/src/sign.rs` 与 sidecar 的 `rust/exec-service/src/sign.rs` 必须逐字节一致，
  各自的单测分别守着这条约束。

## 5. 职责边界

| | 主服务（Rust 内核） | sidecar（exec-service） |
|---|---|---|
| 签名 | ✅ 实际下单用它 | ✅ 只回传签名 |
| 下单 / 保护单 / 持仓 | ✅ | ❌ 不接触账户、持仓与订单 |
| 交易所 Secret | 加密存库，按需解密 | 只在启动时从环境变量读取 |
| 故障影响 | sidecar 挂掉**不影响**交易 | — |

> sidecar 目前与主服务通常共用同一个系统用户、同一权限边界，
> 所以「密钥隔离」在默认部署下并没有额外收益，它的主要价值是**独立实现的交叉校验**。

## 6. 主服务接口

| 端点 | 用途 |
|---|---|
| `GET /system/exec-sidecar` | 连接状态、algo、自检计数（成功/失败/跳过）、往返耗时 p50/p95/p99、verdict |
| `POST /system/exec-sidecar/self-test` | 探活一次并记入计数（不消耗权重、不下单）；未配置端点时记为「通过」——可选组件没部署不是故障 |
| `POST /system/exec-sidecar/reset` | 清零观测计数（换密钥/换环境后重新统计） |
| `GET /system/exec-sidecar/reconcile` | 只读对账：本地 `orders` 与执行死信 |

> 没有样本的字段返回 `null`，面板显示「—」，不会用 0 冒充实测值。

## 7. 部署与验证

Linux（systemd）：

```bash
sudo bash scripts/setup_sidecar.sh --from-settings   # 自动读取当前交易环境的 Secret
```

Windows：

```powershell
powershell -ExecutionPolicy Bypass -File scripts\start-sidecar.ps1 -Detach
```

逐字节校验（这才是 sidecar 的主要用途）：

```bash
EXEC_SECRET='<密钥>' nanoquant ops verify-sidecar --target unix:/run/nanoquant/exec.sock
```

它会发一条 `health`、两条真实下单 query 的 `sign`、一条空 query 边界，
用主服务自己的 `sign::sign_query` 对比 sidecar 的签名，任一不一致即失败。

## 8. 风险控制

- sidecar 不可用时主服务照常用自己的签名下单，不阻塞交易；
- 密钥只在 sidecar 进程与主服务各自的内存里，不写日志、不进事件流（`ops show-exec-secret` 只打标准输出）；
- 签名比对不一致必须视为失败，不能「猜」；
- 主服务不下单时 sidecar 不做任何事，两者没有隐式耦合。
