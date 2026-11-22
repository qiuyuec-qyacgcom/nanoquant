# 执行内核 exec-core 设计（替代 exec-service sidecar）

> 日期：2026-11-23 · 状态：已与用户逐节确认 · 交付目标：任务 md（供下一个 AI 开发执行）

## 1. 背景与动机

现状调研结论（2026-11-23，基于代码逐文件核实）：

| 问题 | 证据 |
|---|---|
| 旧 sidecar（`rust/exec-service`）10 个 op 只被主服务消费 2 个（health/sign） | `docs/EXEC_PROTOCOL.md` L34-47 官方矩阵；主服务侧 grep 仅 `system.rs` 探活与 `ops verify-sidecar` 两处调用 |
| sidecar 无内置指标：无 metrics 模块、无 p50/p95/p99，probe 不留样本 | `exec-service/src/main.rs` L359-373；仅裸计数器 |
| 下单关键路径七段 p99 埋点随 Python 内核退役丢失，`/engine/execution` 延迟输出实际恒空 | `engine.rs` L715-743/L1543-1548 读 `execution.*` key，但 Rust 侧无写入方；历史能力见 `docs/archive/history/docs/TASKS.md` #12 |
| 主服务探活每次新建同步阻塞连接（800ms 超时，在 axum handler 里做网络 I/O） | `system.rs` `exec_probe_state` L1273-1370 |
| JSON Lines 客户端在主服务侧有三份独立实现（verify / 探活 / mock） | `ops_tools.rs` L676-710、`system.rs` L1330-1370、`ops_tools.rs` L637-674 |
| 签名 / urlencode / 保护单构造两边重复实现，靠单测固定向量手工对齐 | `exec-service/src/sign.rs` vs `nanoquant/src/sign.rs` 等 |

用户决策（已确认）：

1. **两期目标**：一期 = 指标内核 + 主服务下单路径分段埋点（p50/p95/p99 面板可见）；二期 = 下单直接经内核发出（协议号已预留，本设计不含二期实现）。
2. **新内核为项目全局默认内核**：不再是可选组件。
3. **一期就强依赖**：主服务启动时连不上内核直接拒绝启动交易。
4. **旧内核直接删除**（`rust/exec-service` 整目录），不归档。
5. **下线旧说明**：面板与文档中"可选组件 / 无需部署 / 自检照常通过 / Rust 内核直连交易所签名下单"等表述全部移除。
6. 进程形态：**独立进程 + 长连接**（方案 A：薄内核两期走）。

## 2. 目标与非目标

**一期目标**
- 新内核 `rust/exec-core`：签名校验 + metrics 聚合 + 长连接服务端，默认部署。
- 主服务：启动握手（强依赖）+ 下单路径七段计时 + 异步批量上报 + 面板读取实时分位数。
- 面板：探活往返 p50/p95/p99 保留；新增七段下单延迟表；未连接=红色异常。
- 统一主服务侧 JSON Lines 客户端为一份。

**非目标（一期不做）**
- 下单/保护单不经内核（二期 `order_open`/`order_close`/`protection_place` 仅预留协议号）。
- 不做跨进程持久化指标（内核重启指标清零，与 legacy Python `exec_metrics.py` 内存环形缓冲口径一致）。
- 不动 demo/live 交易逻辑本身（只加计时观测点，不改执行语义）。

## 3. 架构总览（一期）

```
主服务 nanoquant ──长连接(握手 hello/心跳/指数退避重连)──> exec-core（独立进程，默认部署）
  │ 下单路径七段计时(Instant 分段)                    │ sign.rs     签名校验(HMAC/Ed25519)
  │ 异步批量上报 mpsc 有界 8192(满则丢弃+计数,绝不阻塞)   │ metrics.rs  7 段环形缓冲(2000 条/段)
  │ 面板 <── GET /system/exec-core(经内核 metrics_query) │             p50/p95/p99/max/last
```

失败语义：内核握手失败 → 交易引擎拒绝启动（engine_events 记录 + 面板红色）；查询/前端功能照常；内核运行中掉线 → 主服务自动重连，期间埋点样本在上报队列排队（队列满丢弃计数），交易是否继续由现有 runtime 容错逻辑决定（一期不改）。

## 4. 新内核 `rust/exec-core`

### 4.1 模块

| 文件 | 职责 |
|---|---|
| `main.rs` | UDS+TCP 双监听（沿用 exec-service 模式：Unix 绑 `EXEC_SOCKET`，`EXEC_TCP` 回环，Windows 无 UDS 必须设 `EXEC_TCP`）；每连接一个 tokio task；`EXEC_SECRET` 缺失以退出码 78 退出 |
| `sign.rs` | HMAC-SHA256（小写 hex）/ Ed25519（base64）；`normalize_pem`；固定向量单测与主服务 `sign.rs` 逐字节一致（沿用现有约束） |
| `metrics.rs` | 七段样本存储：每段一个环形缓冲（容量 `EXEC_METRICS_CAPACITY`，默认 2000），`Mutex` 保护；ingest 批量写入（满容量覆盖最旧）；query 输出 `{count,p50,p95,p99,max,last}`（无样本=null）；reset 清零 |
| `model.rs` | 协议帧 `Request`/`Response`（serde，入参字段 `#[serde(default)]`） |
| `session.rs` | 连接会话：心跳（health 空转超时判定）、多连接允许（后连覆盖先连的会话标记）、帧 id 关联 |

### 4.2 协议 v1.0（JSON Lines，一行一请求/一响应，`\n` 结尾）

| op | 方向 | 入参 | 出参 | 说明 |
|---|---|---|---|---|
| `hello` | 主服务→内核 | `version` | `ok`/`kernel_version`/`capabilities[]`/`metrics_capacity` | 启动握手；失败=主服务拒绝启动交易（强依赖判定点） |
| `health` | 双向 | — | `algo`/`uptime_ms`/`sessions` | 探活；主服务侧采样往返耗时进 p50/p95/p99 |
| `sign` | 主服务→内核 | `query` | `signature`/`algo`/`query` | 签名校验（`ops verify-exec-core` 逐字节比对） |
| `metrics_ingest` | 主服务→内核 | `samples:[{segment,ms,ts}]` | `accepted`/`dropped` | 批量上报；segment ∈ 七段；未知 segment 拒绝该条并计数 |
| `metrics_query` | 主服务→内核 | — | `segments:{total:{count,p50,p95,p99,max,last},...}` | 无样本字段=null（面板显示"—"，不用 0 冒充） |
| `metrics_reset` | 主服务→内核 | — | `ok` | 清零全部段 |
| （预留） | — | — | — | `order_open`/`order_close`/`protection_place`：二期实现，一期收到这些 op 返回 `{"ok":false,"error":"reserved for phase 2"}` |

坏 JSON / 未知 op 沿用旧语义：`{"id":"...","ok":false,"error":"..."}`。

### 4.3 七段定义（对齐 legacy Python 口径）

| segment | 计时范围（主服务下单路径） |
|---|---|
| `total` | 整单：从意图准入（admit 后）到 settle 完成 |
| `plan` | 意图/参数构造（build_live + order_params） |
| `sign` | 签名（sign::signed_query） |
| `http` | 发送往返（live::submit 网络 RTT） |
| `wait` | Unknown 解析 + settle 等待（resolve_live/settle_live） |
| `protect` | 保护单两腿构造+发送（ensure_protection） |
| `persist` | 落库（reserve_order/insert_entry_position/orders 写入） |

### 4.4 环境变量

| 变量 | 默认 | 说明 |
|---|---|---|
| `EXEC_TCP` | — | Windows/容器联调：`127.0.0.1:9101` |
| `EXEC_SOCKET` | — | Linux 生产：`/run/nanoquant/exec.sock` |
| `EXEC_SECRET` | — | 交易所 Secret（缺失退出码 78） |
| `EXEC_METRICS_CAPACITY` | 2000 | 每段环形缓冲容量 |

### 4.5 Cargo 依赖

沿用 exec-service 清单：tokio 1（rt-multi-thread/macros/net/io-util/time/signal）、serde、serde_json、base64、hmac、sha2、ed25519-dalek 2（pkcs8）。**不再需要** reqwest、rusqlite（一期内核不发 HTTP、不落库）。release profile 沿用：opt-level 2、thin LTO、codegen-units 1、panic=abort。

## 5. 主服务侧改造（rust/nanoquant）

### 5.1 新模块 `exec_client.rs`（统一客户端）

- 长连接：启动连接 + `hello` 握手；心跳每 5s 发一次 `health`；断线指数退避重连（250ms 起，上限 8s）；连接状态（Connected/Reconnecting/Disconnected + 上次错误）进 `SystemState`。
- **替换三份客户端**：`system.rs` 探活（L1330-1370 同步阻塞实现删除）、`ops_tools.rs` verify 的 roundtrip、mock 联调路径，统一走 `exec_client`（CLI 一次性命令允许短连接模式）。
- 上报通道：`mpsc::sync_channel(8192)` + 独立上报线程攒批（64 条/批或 200ms 刷新）发 `metrics_ingest`；队列满丢弃 + `dropped` 计数，**绝不阻塞下单路径**（沿用 exec-service `store.rs` 模式）。

### 5.2 强依赖启动

- 主服务启动：连接内核 + `hello` 握手；失败 → 交易引擎（runtime）不启动，engine_events 记 `error` 事件"执行内核握手失败，交易引擎未启动"，面板红色；HTTP 查询/前端照常。
- `scripts/start.ps1` / `supervise.ps1`：先起内核（等端口/UDS 就绪）再起主服务。

### 5.3 七段埋点（runtime.rs 下单路径）

- 每单一个 `SegmentTimer`：七个 `Option<Instant>` 起止点，settle 完成后一次性产出七条样本投上报队列。
- 埋点位置（只加观测，不改语义）：`run_live_with`（total/plan/wait）、`build_live`（sign）、`live::submit`（http）、`ensure_protection`（protect）、`reserve_order`/`settle_live` 落库段（persist）。
- 失败单也计时（total 到失败点为止，已完成的段照常上报）。

### 5.4 端点改造

| 端点 | 变化 |
|---|---|
| `GET /system/exec-sidecar` → `GET /system/exec-core` | 返回 `connected`/`configured`/`algo`/`counts`/探活 `latency_ms{last,p50,p95,p99,samples}` + 新增 `segments`（七段分位数，经长连接 `metrics_query`）+ `verdict` |
| `POST /system/exec-core/self-test` | 语义反转：未连接=**失败**（下线"未部署照常通过"）；连上=探活+记数 |
| `POST /system/exec-core/reset` | 清零探活计数 + 转发内核 `metrics_reset` |
| `GET /system/exec-core/reconcile` | 沿用现有只读对账 |
| `/engine/execution`、`/engine/stats` 的延迟输出 | 从恒空接活：`latency_p50_ms/latency_p95_ms` 与七段 p99 读内核 `segments` |

`ExecStats`（64 样本探活环形）保留，职责收窄为通道健康度量。

### 5.5 CLI

- `ops verify-sidecar` → `ops verify-exec-core`（health + 2 条 sign 逐字节比对 + 空 query 边界，逻辑沿用）。
- `ops show-exec-secret` 沿用（部署内核取 Secret）。
- `ops mock-exec-sidecar` → `ops mock-exec-core`（mock 内核实现 v1.0 op 集，供联调与单测）。

## 6. 前端改造（frontend/src/views）

### 6.1 SystemView.vue 执行内核面板

- 标题："执行内核（exec-core）"；未连接=红色异常 tag（下线"无需部署（可选）"灰色态）。
- 通道行：`tcp:127.0.0.1:9101` / `unix:/run/nanoquant/exec.sock`（已连接/重连中）。
- 往返耗时行：保留 `最近 · p50 · p95 · p99`（探活一帧 health 的往返）。
- **新增"下单分段延迟"表**：七段行 × `count / p50 / p95 / p99 / max`（无样本显示"—"），数据源 `segments`。
- **下线旧说明**，hint 改为："默认执行内核，主服务启动需完成握手；端点通过环境变量配置：`EXEC_TCP=host:port` 或 `EXEC_SOCKET=/path/to.sock`（Windows 可执行 `scripts\start-core.ps1` 拉起内核）。"

### 6.2 DashboardView.vue 超短线执行卡片

- 下单 p99 / 写库 p99 / 保护单 p99 / 签名 p99 四个分段读数改接 `segments`（现状读恒空 engine_state key）。

## 7. 部署与脚本

| 项 | 动作 |
|---|---|
| `scripts/start-core.ps1` | 新增（替代 start-sidecar.ps1）：找 `rust\exec-core\target\release\exec-core.exe`，Secret 解析沿用（-Secret 或 ops show-exec-secret），`-Detach`/`-Stop` 沿用 |
| `scripts/setup_core.sh` | 新增（替代 setup_sidecar.sh）：systemd unit `nanoquant-exec-core.service`（UDS `/run/nanoquant/exec.sock`，`--from-settings` 自动读 Secret） |
| `scripts/start.ps1` / `supervise.ps1` | 先起内核再起主服务；主服务 env 带 `EXEC_TCP`（Windows）/`EXEC_SOCKET`（Linux） |
| `deploy/nanoquant-exec.service` | 删除，由 `deploy/nanoquant-exec-core.service` 替代 |

## 8. 删除清单

| 路径 | 动作 |
|---|---|
| `rust/exec-service/` | 整目录删除 |
| `scripts/start-sidecar.ps1`、`scripts/setup_sidecar.sh` | 删除 |
| `deploy/nanoquant-exec.service` | 删除 |
| `docs/EXEC_PROTOCOL.md` | 重写为 v1.0（exec-core 协议，含二期预留 op 说明） |
| SystemView.vue 旧 hint、"无需部署（可选）"文案 | 下线 |

## 9. 测试计划

**内核侧**
- `metrics.rs`：分位数正确性（已知样本集 p50/p95/p99/max）、环形覆盖（超容量丢最旧）、并发 ingest（多任务同时写）、空段 query=null、reset。
- `sign.rs`：沿用固定向量（与主服务逐字节一致约束）。
- `main.rs` dispatch：帧协议单测（坏 JSON、未知 op、预留 op 报错、hello 能力清单）——旧版缺失的补上。

**主服务侧**
- `exec_client`：握手成功/失败、断线重连状态机、上报队列满丢弃计数、攒批刷新。
- 强依赖：握手失败时 runtime 不启动 + engine_events 有记录。
- 埋点：mock 内核下跑一笔下单，七段样本齐全且 total ≥ 各分段之和的合理关系（total ≥ max(plan,sign,http,wait,persist)）。
- self-test 新语义：未连接=失败；`/engine/execution` 延迟输出非空。

**集成**
- `ops verify-exec-core` 对真实内核通过；`ops mock-exec-core` 联调面板全链路（握手→埋点→上报→面板显示分位数）。

## 10. 二期预留（不在本期实现）

- 协议号 `order_open`/`order_close`/`protection_place` 已预留；内核收到返回"reserved for phase 2"。
- 二期切换条件：一期面板分位数稳定运行后，将 `live::submit`/`ensure_protection` 改为经内核 op 发出，埋点随执行内聚进内核，主服务上报通道退役。

## 11. 验收标准（一期）

1. `scripts/start-core.ps1 -Detach` 拉起内核，主服务启动握手成功，面板"执行内核（exec-core）"显示已连接。
2. 主服务启动时内核未启动：交易引擎不启动，面板红色，engine_events 有"握手失败"记录；查询/前端可用。
3. 跑一笔 demo 下单后，面板"下单分段延迟"表七段均有非空 count 与 p50/p95/p99/max。
4. 内核重启后：主服务自动重连（面板恢复已连接），指标清零重新累计。
5. `cargo test`（exec-core + nanoquant）全绿；`ops verify-exec-core` 通过。
6. 全仓库无"可选组件/无需部署/自检照常通过（未部署）/Rust 内核直连交易所签名下单"残留文案。