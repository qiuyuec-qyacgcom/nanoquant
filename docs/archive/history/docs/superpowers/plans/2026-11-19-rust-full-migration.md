> 历史记录：已退役，不可执行
> 源路径：docs/superpowers/plans/2026-11-19-rust-full-migration.md
> 归档时间：2026-11-20T09:17:25Z

# Rust 全量迁移实现计划

> **面向 AI 代理的工作者：** 使用 `executing-plans` 逐任务实现本计划，并使用 `test-driven-development` 在每个任务中先写失败测试。

**目标：** 将 Rust 内核从可选 API 实现推进为能够覆盖 NanoQuant 核心交易、实时数据、回测、备份、剩余接口和生产启动流程的实现。

**架构：** 保留现有 Axum/Tokio/Rusqlite 模块边界，先建立可注入的行情、账户、交易所和时钟接口，再接入 runtime 与 cycle；所有真实外部依赖均通过 provider trait 注入，测试使用确定性的本地实现。逐阶段保留明确的 `501`，只有完成真实行为和测试后才移除。

**技术栈：** Rust 2021、Axum 0.8、Tokio、Reqwest、Rusqlite、Serde、现有签名/风控/提交模块。

**规格：** 当前 Rust 迁移账本 `docs/superpowers/sdd/2026-11-18-rust-rewrite/progress.md` 与 Python 路由/数据库契约。

## 全局约束

- 不使用假行情、假余额或假成交掩盖未接入的生产能力。
- 先写失败测试，再写实现；每个阶段完成后运行 Rust 全量测试。
- 保持 Python 数据库字段、token、签名和 API 响应兼容。
- 外部行情和交易所访问必须可超时、可取消、可记录明确错误。
- 下单结果未知时不得自动重复下单，必须进入对账流程。
- 生产切换前 Rust 仍可作为回滚内核，默认切换需有跨平台验证证据。

## 审查重点（Review Focus）

- 外部请求超时或断线：周期必须退出并保留可诊断错误，不得卡死后台任务。
- 下单响应丢失：不得重复提交，必须标记未知并等待对账。
- 数据库写入中断：不得产生半更新的订单、备份或认证状态。
- 多实例并发：同一交易周期、备份恢复和订单状态更新必须有互斥或幂等约束。
- 恶意或过大的请求数据：API、WebSocket、回测和恢复流程必须限制大小并拒绝非法输入。

## 文件边界

- 修改：`rust/nanoquant/src/runtime.rs`、`cycle.rs`、`engine.rs`、`main.rs`、`db.rs`、`ws.rs`、`backtest.rs`、`backup.rs`、`system.rs`、`journal.rs`、`settings_api.rs`、`evolution_api.rs`、`Cargo.toml`。
- 创建：`rust/nanoquant/src/providers.rs`、必要的 provider、备份和回测辅助模块；阶段测试优先放在对应模块的 `#[cfg(test)]` 中。
- 修改：`scripts/start.sh`、`scripts/start.ps1`、`.github/workflows/ci.yml`，仅在阶段 7 切换默认内核和验证矩阵。
- 文档：更新 `.superpowers/sdd/2026-11-18-rust-rewrite/progress.md`，记录每阶段真实完成范围和验证结果。

## 阶段 1：核心交易闭环

### 任务 1.1：定义可注入数据与交易接口

- [ ] 为行情、账户、持仓、订单、交易所和时钟定义 trait 与错误类型。
- [ ] 测试 provider 超时、空数据、非法数据和取消行为。
- [ ] 运行 `cargo test --locked --bin nanoquant providers`，预期新增测试先失败后通过。

### 任务 1.2：实现真实行情和账户同步

- [ ] 使用现有 `reqwest`、签名和配置读取实现 provider。
- [ ] 测试请求参数、超时、错误分类、重试边界和响应解析。
- [ ] 将快照写入现有 SQLite 状态表，事务失败不更新部分状态。

### 任务 1.3：接通 demo 执行器和交易周期

- [ ] 先测试 `cycle::run_cycle` 从行情和账户快照生成信号、订单和周期结果。
- [ ] 接入 `runtime.rs`，停用、超时、重复周期和失败状态均可观察。
- [ ] demo 模式使用确定性撮合，不调用真实交易所。

### 任务 1.4：接通实盘提交、回报和对账

- [x] 复用 `submit.rs`、`reconcile.rs`、`risk.rs`，接通订单提交和未知结果对账（管仓单；开仓待信号契约）。
- [x] 测试成功、拒绝、超时、未知、重复周期和保护单失败。
- [x] 只有通过风控和 admission 的订单才可发送。

## 阶段 2：引擎控制面

### 任务 2.1：实现 `/engine/admission`、`trade-mode`、`cycle-now`。
### 任务 2.2：实现 `/engine/flatten`、`protection`、`stats`。
### 任务 2.3：实现 `/system/runtime`、exec-sidecar、告警测试和凭证管理。
### 任务 2.4：逐项对照 Python 路由，补充鉴权、错误码和审计测试。

## 阶段 3：实时数据和 WebSocket

### 任务 3.1：实现行情缓存、订阅、断线重连和取消。

- [x] `rust/nanoquant/src/kline_stream.rs`：上游连接用 `KlineSource` trait 抽象，
      真实实现 `BinanceWsSource`（tokio-tungstenite + rustls），测试用假上游。
- [x] 一条连接复用多路 `(symbol, interval)` 订阅，按需 `SUBSCRIBE`/`UNSUBSCRIBE`，
      增量订阅不重连（`subscribers_receive_bars_pushed_by_the_upstream`、
      `a_new_subscription_reaches_the_upstream_without_reconnecting`、
      `dropping_the_last_subscriber_unsubscribes_upstream`）。
- [x] 断线指数退避重连（1s → 15s 封顶），交易环境变化自动换域名重连，
      进程退出时 `KlineHub::stop()` 取消上游任务。

### 任务 3.2：让 `/ws/klines` 推送真实 K 线并限制订阅规模。

- [x] `src/ws.rs` 的 `/ws/klines` 订阅中枢并推送 `{"type":"kline","data":{...}}`，
      字段与 Python `kline_stream.py` 逐字对齐，前端零改动。
- [x] 订阅规模限制：流数上限 `MAX_STREAMS=32`、单流客户端上限
      `MAX_CLIENTS_PER_STREAM=128`，超限回 `4429` 关闭码
      （`subscription_count_is_capped`、`clients_on_one_stream_are_capped`）。

### 任务 3.3：推送账户、持仓、订单事件并完成连接清理。

- [x] `src/events.rs`：落 `engine_events` 表 + 400 条环形缓冲 + `broadcast` 扇出，
      与 Python `services/events.py` 同口径。
- [x] 运行时在事务提交后推送成交/平仓事件（category=trade）与订单对账不一致告警，
      并扇出 `account` / `positions` / `orders` 三类实时事件；未落库的账不会外泄。
- [x] 连接清理：K 线连接结束必退订（否则上游一直推）；事件流退出即退订广播；
      上游任务随进程退出取消。

### 任务 3.4：补充 WebSocket 鉴权、心跳、背压和异常断开测试。

- [x] 鉴权沿用首帧 `{"type":"auth","token":...}`：事件流 4401、K 线流 600ms 宽限。
- [x] 心跳：事件流 25s、K 线流 20s 空闲发 ping（与 Python 一致）。
- [x] 背压：`broadcast` 容量 500/256，慢订阅者收到 `Lagged` 后丢帧而不阻塞生产方；
      单帧发送 5s 超时即断开卡住的客户端。
- [x] 异常断开：上游错误/关闭触发重连，坏消息与超长消息被丢弃不 panic。

## 阶段 4：回测

### 任务 4.1：实现历史 K 线 provider 和缓存。
### 任务 4.2：实现撮合、手续费、滑点、步进和最小数量规则。
### 任务 4.3：实现 `/backtest/run`、`grid`、`walkforward` 的实际执行和结果持久化。
### 任务 4.4：加入结果一致性、取消、资源限制和 Python 对照测试。

## 阶段 5：备份与恢复

### 任务 5.1：实现数据库打包、主密钥加密、校验和本地创建。
### 任务 5.2：实现上传、下载、删除和保留策略。
### 任务 5.3：实现恢复前校验、事务恢复和失败回滚。
### 任务 5.4：实现备份调度、状态 API 和权限/审计测试。

## 阶段 6：剩余业务接口

### 任务 6.1：实现 `/journal/label` 的价格注入、窗口和隔离。
### 任务 6.2：实现 `/system/write-health`、dead-letter 和 evolution effectiveness。
### 任务 6.3：迁移 credentials、exec-sidecar 及剩余 `501` 路由。
### 任务 6.4：对照 Python 路由清单，确保响应、鉴权和失败行为一致。

## 阶段 7：生产切换

### 任务 7.1：完成 Rust/Python 数据和行为对照测试。
### 任务 7.2：完成 Linux、Windows、容器和 CI 验证。
### 任务 7.3：增加 Rust/Python 灰度和回滚开关。
### 任务 7.4：切换默认 Rust 内核，保留 Python 回滚路径，更新启动文档和迁移账本。

## 验收

- 每个阶段的定向测试和 Rust 全量测试通过。
- 所有仍未实现的接口有明确记录，不以成功响应伪装。
- 阶段 7 前不宣称完全替代 Python；阶段 7 只有在跨平台验证和回滚验证完成后才可切换默认值。
