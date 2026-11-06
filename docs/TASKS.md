# NanoQuant 任务跟踪

> 更新日期：2026-11-04
> 说明：本文件区分「已完成 / 进行中」与「未开始」两部分，记录任务名称、状态、执行过程要点、结果或待办，便于后续跟踪。
> 状态标记：`✅ 已完成` / `🟡 进行中` / `⬜ 未开始`

---

## 一、已完成 / 进行中

### 1. K 线全面走币安 WebSocket，消除 REST 轮询与 IP 封禁
- **状态**：✅ 已完成
- **过程要点**：
  - 新增 `exchange/kline_stream.py`：单条上游 WS 连接 + 动态 `SUBSCRIBE/UNSUBSCRIBE` + 多订阅者扇出 + 断线指数退避重连 + 交易环境切换自动重连。
  - 新增 `exchange/ticker_stream.py`：订阅 `symbol@ticker`，把权重 40 的 `/fapi/v1/ticker/24hr` 从引擎扫描与前后台报价中移出；REST 仅做首帧/断流兜底。
  - 新增 `WS /api/v1/ws/klines?symbol=&interval=`：浏览器只连本服务，标的必须在池内。
  - 前端 `KlineBoard.vue` 由 3 秒轮询改为 WS 推送；历史 600 根仅在切标的/切周期/首屏拉一次。
  - 修复事件解析：原始 `/ws` 订阅的 kline 事件在顶层而非 `data` 字段（首版会全部丢弃）。
- **结果**：连续 20 秒观测，REST `klines` 请求数 = 0；`/ticker/24hr` 在 WS 预热后不再新增调用；切换标的 2 秒内图表即为新标的数据。

### 2. 前后台 K 线工作站对齐 astra-quant-agent
- **状态**：✅ 已完成
- **过程要点**：
  - 引入 `klinecharts@9.8.11`（astra 同款），新增 `KlineChartPanel.vue`：主图 MA/EMA/BOLL、副图 VOL/MACD/RSI、十字光标、缩放拖动、最新价标签。
  - 历史 600 根（原 160 根）；`klines(..., fresh=True)` 绕过 20s 缓存，保证末根蜡烛实时。
  - 前端周期按钮：5m / 15m / 1h / 1d（对齐 5–30m 短线定位）。
  - 标的标签按**添加顺序**编号（①②③），右侧顺序列表 + 独立图表区；空池显示「未添加标的」占位。
  - 后端补充：`/public/klines` 返回 `count / next_close_at / server_time`；新增 `/chart/klines`（后台登录态、同样 600 根）；`/public/quotes` 供列表报价。
  - 修复历史遗留：公共 GET 丢弃 query 参数（`-1102`）；24h ticker 强制读 `bidPrice/askPrice` 导致整表为空；K 线时间戳被重复乘 1000。
- **结果**：前后台各 600 根，倒计时正常（实测 240s→递减）；指标切换改为父级 `:key` 重挂载，规避 `dispose+init` 导致图表实例失效、切标的不刷新的问题。

### 3. 系统由 1m 超短线改为 5–30m 短线
- **状态**：✅ 已完成
- **过程要点**：
  - `StrategySettings.timeframe` 限定 `5m/15m/30m/1h`（默认 5m），旧库 `1m/3m` 读入自动迁移。
  - 最长持仓 30→180 分钟；止损 ATR 1.1→1.2、止盈 1.8→2.0、移动止损 0.5%→0.8%。
  - 时间阶梯止盈改为 0min 0.8% / 15min 0.6% / 30min 0.45% / 60min 0.3% / 120min 0.2%。
  - 首页、参数页、README 文案统一为「5–30m 短线」。
- **结果**：运行库已重置为该组默认值，`/settings/strategy` 校验通过。

### 4. 后台「实盘矩阵」替换为「信号审计」
- **状态**：✅ 已完成
- **过程要点**：
  - 删除 `MatrixView.vue`、`/admin/matrix` 路由与导航、`services/matrix.py` 聚合服务、`/engine/matrix` 接口（实测 404）。
  - 新增 `/admin/signals` 信号审计页：四类裁决 KPI、信号流水表（评分/特征读数/AI/裁决/拒绝原因）、拒绝原因 Top 分布、标的/方向/裁决筛选。
  - `/engine/signals` 增强：`symbol / decision / direction` 过滤 + `summary` 聚合。
  - 新增 `/engine/quotes` 供后台 K 线列表报价（原依赖矩阵接口）。
- **结果**：全仓 `matrix` 引用清零；页面渲染与筛选验证通过。

### 5. 标的池按 astra 规则重写 + 添加顺序落库
- **状态**：✅ 已完成
- **过程要点**：
  - 添加前用币安在线 `exchangeInfo` 校验（`PERPETUAL` / `TRADING` / `USDT`）；重复 409、容量上限 409、非法合约 400。
  - 规格随合约落库：`precision / tick_sz / min_sz / min_notional / ct_val`；分层派生 BTC/ETH → tier1。
  - 删除保护：必须输入 `REMOVE <SYMBOL>`；本地或交易所有持仓、无法确认持仓时拒绝。
  - 新增 `trading_symbols.sort_order`（启动自动 `ALTER` + 按 `created_at` 回填），新增取 `max+1`，所有查询统一 `ORDER BY sort_order, created_at, symbol`。
  - 清空出厂 49 个标的，允许空池。
- **结果**：按序添加 DOGE→ETH→BTC 得到 1/2/3；删除中间项后收敛为 1:ETH 2:BTC。

### 6. 模拟盘 / 实盘双环境 + 双凭证
- **状态**：✅ 已完成
- **过程要点**：
  - 删除本地撮合 `DemoBroker`，统一 `BinanceBroker`：**模拟盘 = 币安测试网真实下单，实盘 = 主网真实下单**。
  - `DataSettings` 拆为 `demo_api_key_enc / live_api_key_enc / demo_base_url / live_base_url`；移除 `mode / testnet / synthetic` 等演示开关。
  - `/engine/control`：切实盘需 `LIVE` 短语；两个环境都必须有各自凭证；无凭证禁止启动引擎（实测 400）。
  - 仓位、订单、成交、权益、统计全部按 `mode` 隔离。
- **结果**：双环境凭证状态、切换校验、环境隔离均验证通过。

### 7. 执行质量观测（滑点 / 下单延迟）
- **状态**：✅ 已完成
- **过程要点**：
  - 新增列：`orders.latency_ms`、`positions.entry_slippage_bps`、`trades.slippage_bps`（老库自动 `ALTER`）。
  - 下单/平仓处埋点测 `place_order` 耗时；入场滑点 =（成交均价 − 决策价）/ 决策价 × 10000，方向校正。
  - `/engine/status` 增加 `stats.execution`（订单数、P50、P95、平均滑点）与 `stoploss_guard_remaining`。
  - 总览「超短线执行」面板增加第二行：下单延迟 P50/P95、平均入场滑点、止损熔断状态。
- **结果**：接口与页面均验证通过；演示环境暂无成交，指标显示 `--`，有成交后自动填充。

### 8. 止损熔断（StoplossGuard）
- **状态**：✅ 已完成
- **过程要点**：
  - `RiskSettings` 新增 `stoploss_guard_count / window_min / pause_min`（默认 3 次 / 60 分钟 / 暂停 30 分钟，0=关闭）。
  - 平仓后按当前交易环境统计窗口内 `stop_loss / trailing_stop` 次数，达阈值写入 `sl_guard_until`。
  - `global_gates` 拦截新开仓并写入拒绝原因；准入体检同步提示。
  - 参数页新增「止损熔断」分区；总览显示状态。
- **结果**：接口返回新字段与默认值，页面渲染「正常」；开仓链路编译与导入通过。

### 9. 执行层 p99 快速优化（P1，不换语言）
- **状态**：✅ 已完成（本次执行）
- **过程要点**：
  1. **保护单并发**：`_place_protection` 由「STOP → TP」串行改为 `asyncio.gather` 并行，关键路径少一个 RTT。
  2. **启动连接预热**：`_probe` 增加一次 `book_tickers(["BTCUSDT"])`，提前建立 TCP/TLS/HTTP2 连接，首单不再付握手长尾。
  3. **精简写库负载**：`orders.raw` 由整份交易所回执改为 `_compact_raw()` 只留 7 个关键字段，降低下单路径上的 JSON 序列化开销。
- **结果**：`compileall` + `import` 通过，服务重启正常（health=ok），前后台 K 线 600 根、报价与倒计时均正常。
- **待办（未闭环）**：尚无 p99 基线数据，收益需按「未开始 #1」采集后确认。

### 10. 前台首页改版（去重，落地页化）
- **状态**：✅ 已完成
- **过程要点**：首页改为 Hero + 实时状态卡 + 能力网格 + 脱敏绩效 + 三入口分流 + 风险提示；移除与「行情」「观点」重复的 K 线与资讯列表。
- **结果**：Hero 栅格与全局 `.hero` 类名冲突已通过重命名（`landing-*`）解决，页面渲染正常。

### 11. 移除行情模块（页面 + 接口 + 数据层）
- **状态**：✅ 已完成
- **过程要点**：删除 `exchange/market.py`、`api/routes/market.py`、后台「行情与资讯」页、前台 `/markets` 页与 `klinecharts` 旧组件；价格改由 `exchange/client.py` 内部提供；资讯模块按要求保留。
- **结果**：`/market/*` 全部 404；引擎、矩阵、标的池校验改用内部客户端。

### 12. p99 分段埋点与本地基线（本次执行）
- **状态**：✅ 已完成（真实下单采样仍待凭证，见「未开始 #1」）
- **过程要点**：
  - 新增 `services/exec_metrics.py`：**内存环形缓冲**（2000 条）记录七段耗时并计算 p50/p95/p99/max；**不落库**——把写库塞进下单路径本身就是 p99 来源之一。
  - `client._request` 通过 `ContextVar`（`STAGE_SINK`）回填 `wait_ms`（权重/订单桶排队）、`sign_ms`（时间戳+urlencode+HMAC）、`http_ms`（HTTP 往返）；首次校时归入连接准备，不计入签名。
  - `broker.open_position / close_position` 记录 `plan_ms / persist_ms / protect_ms / total_ms`，并在开仓成功后 `exec_metrics.record()`。
  - 新增 `GET /engine/execution` 返回七段 p50/p95/p99/max；总览「超短线执行」新增第三行：下单 p99 / 写库 p99 / 保护单 p99 / 签名 p99。
  - 新增 `scripts/p99_bench.py`：无需凭证即可压测本地可控段（签名、回执序列化、SQLite 落库）。
- **实测结果**（N=1500，本机）：
  | 段 | p50 | p95 | p99 | max |
  |---|---|---|---|---|
  | sign | 0.017ms | 0.019ms | 0.022ms | 0.094ms |
  | serialize | 0.008ms | 0.009ms | 0.011ms | 0.047ms |
  | persist（SQLite 提交） | 0.021ms | 0.046ms | 0.085ms | **74.05ms** |
  | http（测试网实网，3 次采样） | 199ms | 345ms | 358ms | 361ms |
- **结论**：签名与序列化可忽略（p99 < 0.03ms）；**长尾全部来自网络 RTT**；落库 p99 很低但 **max 出现 74ms 尖刺**（checkpoint/fsync），印证了「DB 移出关键路径」是下一步的必要性。

---

### 13. 开仓落库移出下单关键路径（本次执行）
- **状态**：✅ 已完成
- **过程要点**：
  - 新增 `services/db_writer.py`：有界队列（20000）+ 独立写线程；队列满时退化为同步写（宁可慢，不丢单）；写失败打印堆栈并写事件日志。
  - `broker.open_position` 不再在协程里 `session_scope()` 提交，改为构造订单/持仓字段后 `db_writer.submit(_persist_entry, ...)`；订单与持仓仍在同一事务内，避免「有订单无持仓」。
  - `persist_ms` 口径改为「入队耗时」；停机时 `db_writer.stop()` 先排空队列再退出。
  - 崩溃窗口由交易所仓位对账兜底（`worker._reconcile_live`）。
- **实测结果**：
  | 指标 | 数值 |
  |---|---|
  | 入队耗时（关键路径） | **0.0016ms/单** |
  | 同步写对照（无 PRAGMA 优化） | 5.15ms/单 |
  | 后台写吞吐（生产同款 PRAGMA） | **3597 条/秒** |
  | drain 2000 条 | 556ms，落库 2000/2000 |
- **结论**：下单路径上的 SQLite 提交成本从「毫秒级且带 74ms 尖刺」降到「1.6 微秒入队」；真实订单速率（≤60 单/分钟）下队列不会积压。
- **范围说明**：本次只改**开仓**；平仓仍需同步读仓位做校验与分笔记账，异步化列入「未开始 #2」。

### 14. 部分止盈（1R 平一半 + 剩余移动止损，本次执行）
- **状态**：✅ 已完成（真实链路的数量一致性见「未开始 #4」）
- **过程要点**：
  - `StrategySettings` 新增 `partial_tp_at_r`（默认 1.0，0=关闭）与 `partial_tp_ratio`（默认 0.5，校验 0<ratio≤1）。
  - `strategy.partial_tp_r()` 纯函数计算 R 倍数，**用开仓时的初始止损**而非当前 `sl_price`（移动止损上移后 R 会失真）；`initial_sl <= 0` 直接返回 0，避免把缺失止损当成 0 价算出假 R。
  - `positions` 新增 `initial_sl_price`、`partial_taken_at`（老库自动 ALTER）；开仓时写入初始止损。
  - `worker._manage_positions` 在硬止损/止盈判断**之前**检查部分止盈，触发后按 `qty_step` 取整下单并 `continue`，同轮不会重复平仓；`partial_taken_at` 保证只触发一次。
  - `broker.close_position` 的部分平仓分支补记 `TradeRecord`（数量=本腿、盈亏=本腿），否则这一腿的盈亏只落在 `positions.realized_pnl` 里，复盘统计会漏。
  - 参数页新增「部分止盈」分区；`EXIT_LABELS` 增加「部分止盈」。
- **验证**：
  - 纯函数：`long 1R=1.0`、`short 1R=1.0`、`short -0.5R` 正确；`initial_sl=0` / `entry=0` 返回 0；`ratio>1` 与 `at_r<0` 被 Pydantic 拒绝。
  - 打桩平仓（`place_order` 返回 0.5 成交）：仓位 `qty 1.0→0.5` 且 `status=open`；新增 1 条 `TradeRecord(qty=0.5, pnl=0.9495, exit_reason=partial_tp)`，手续费计算正确；测试数据已清理。
  - 服务重启 health ok，`/settings/strategy` 返回 `at_r=1.0 / ratio=0.5`，DB 两列已补齐，日志无异常。

### 15. 轻量向量化回测引擎（本次执行）
- **状态**：✅ 已完成
- **过程要点**：
  - 新增 `backtest/engine.py`：**复用实盘的 `build_features` + `score_signal`**，按逐根 K 线推进，出场覆盖止损 / 止盈 / 阶梯止盈 / 移动止损 / 超时。
  - 手续费与滑点显式入参（默认 taker 5bps + 滑点 2bps），滑点按不利方向打进成交价。
  - 仓位按「单笔风险 %」反推，并被 `保证金% × 杠杆` 截断；未平仓按最后一根收盘强平，避免收益虚高。
  - 历史 K 线没有 24h 成交额快照，用**滚动窗口 quote volume 之和**近似，否则 `score_signal` 的「24h 成交额偏低」闸门会拦掉所有信号（首版就是这个原因导致 0 交易）。
  - 新增 `POST /backtest/run`（登录态、只接受池内标的）+ `BacktestIn` 入参校验。
  - 新增 `/admin/backtest` 页面：标的/周期/根数/费率/滑点/初始权益 → KPI（交易数/胜率/期望/总收益/回撤/手续费）+ 成交明细。
- **实测**（ETHUSDT 5m × 1000 根，费率 5bps、滑点 2bps）：
  | 指标 | 值 |
  |---|---|
  | 交易数 | 53 |
  | 胜率 | 41.51% |
  | 总收益 | -2.83% |
  | 最大回撤 | 2.83% |
  | 盈亏比 | 0.43 |
  | 平均持仓 | ~6 根 |
- **结论**：引擎能跑通全链路且给出可解释的负收益（当前参数在该区间确实不赚钱），符合「粗筛工具」定位；池外标的返回 404。
- **边界**：不含 AI 复核与盘口点差闸门。

### 16. 语义契约测试 + CI（本次执行）
- **状态**：✅ 已完成
- **过程要点**：
  - 新增 `backend/tests/conftest.py`（把 `backend/` 加入 `sys.path`，免安装即可 import）与 `backend/tests/test_contracts.py`。
  - 覆盖 14 个用例：部分止盈 R 计算（含缺失止损防御）、阶梯止盈档位、出场优先级（止损/止盈/ROI/移动止损/超时）、周期白名单、阶梯排序与越界、部分止盈比例边界、分段耗时百分位、回测跑通与最少 K 线数。
  - 全部**不联网、不碰数据库**，可在 CI 里裸跑。
  - 新增 `.github/workflows/ci.yml`：push/PR 触发；后端 `compileall + pytest`，前端 `npm ci && npm run build`；带 pip/npm 缓存与并发取消。
  - README 补充测试命令与 CI 说明；顺带把「持仓管理」链路描述更新为阶梯止盈 + 部分止盈。
- **结果**：`python -m pytest tests/test_contracts.py -q` → **14 passed in 0.64s**。

### 17. 信号表补 `mode` 维度（本次执行）
- **状态**：✅ 已完成
- **过程要点**：
  - `Signal` 新增 `mode` 列（默认 `demo`，带索引）；老库由 `migrate_exec_columns` 自动 `ALTER` 并回填。
  - 引擎写信号时记录当前交易环境（先取 `mode` 再开事务，避免写事务里嵌套读配置）。
  - `/engine/signals` 默认只返回当前环境的信号，`summary` 与「拒绝原因 Top」同样受环境约束。
  - 公开绩效的 `signals` 计数也按环境过滤，避免模拟盘数据撑大实盘统计。
- **验证**：迁移后 `signals.mode` 存在，历史 64 条全部回填为 `demo`；`/engine/signals` 返回 `total=64 / items=3`；`compileall` 与 14 条契约测试全部通过；服务重启 health ok（PID 9424）。

### 18. 告警通道（Webhook / Telegram，本次执行）
- **状态**：✅ 已完成
- **过程要点**：
  - 新增 `services/alerts.py`：通用 Webhook（钉钉/飞书/企微文本机器人同构）与 Telegram 双通道；发送为 fire-and-forget，失败只记日志。
  - 钩在 `events.record` 末尾：按 `alert_min_level` 过滤级别，同一 `category` 在冷却窗口内只发一次；无事件循环的上下文（写库线程）直接跳过。
  - `SystemSettings` 新增 6 个字段：开关、最低级别、Webhook、Telegram token/chat、同类冷却。
  - 新增 `POST /system/alerts/test`，返回每个通道的 HTTP 结果。
  - 参数页「系统」新增「告警推送」分区；停机时关闭共享 httpx 客户端。
- **验证**：本地起 HTTP 接收端 → 配置 Webhook → 调测试接口：
  `{"ok":true,"results":[{"channel":"webhook","ok":true,"status":200}]}`，接收端收到 1 条 POST；验证后已把配置恢复为关闭并清空 URL。
- **范围**：只覆盖事件总线里的 warn/error；分级路由与告警历史见「未开始 #6」。

### 19. 平仓落库异步化 + 互斥闸门（本次执行）
- **状态**：✅ 已完成
- **过程要点**：
  - `broker.close_position` 不再在协程里提交：先做一次**只读**校验（WAL 读不阻塞写，约 0.05ms）拿权威 `qty/status`，下单后把「更新持仓 + 记成交 + 记订单」整体交给 `db_writer`。
  - 新增模块级 `_CLOSING: set[int]` 互斥闸门：写库是异步的，行状态在几百微秒内仍是 `open`，没有闸门时引擎轮询与手动平仓可能重复下单；写线程在 `finally` 里释放。
  - 新增 `_persist_close(payload, position_id)`：数量、手续费、盈亏都按**写库当时的行**重算，避免用下单前快照覆盖并发修改；全平与部分平仓分别记账。
  - `persist_ms` 口径改为入队耗时。
- **顺带修掉的真 Bug**：`DbWriter.drain()` 原本用 `queue.empty()` 判断，队列项一被 `get()` 取走就显示为空，但写线程可能还在提交事务 → 停机时可能漏写最后一批。改为等 `unfinished_tasks`（`task_done()` 递减）归零。
- **验证**（打桩 `place_order`）：
  | 场景 | 结果 |
  |---|---|
  | 全平 1.0 | `qty=1.0 / status=closed`，`TradeRecord(1.0, pnl=1.899, take_profit)`，1 条 exit 订单 |
  | 部分平 0.4 | `qty=0.6 / status=open`，`TradeRecord(0.4, pnl=0.7596, partial_tp)` |
  | 重复平仓 | 返回「该仓位正在平仓中, 请稍候」 |
  | 队列 | drain 后 `pending=0`，测试数据已清理 |
- **崩溃窗口**：已 ack 但未落库的平仓，由 `_reconcile_live` 的「本地有仓 / 交易所已无」分支收敛为 `venue_sync` 关闭。

### 20. 写队列可观测性与积压告警（本次执行）
- **状态**：✅ 已完成
- **过程要点**：
  - `DbWriter` 新增计数：`total / failed / last_ms / last_error / pending / unfinished`，`stats()` 线程安全读取。
  - 每次写入计时并记入统计；`_safe_call` 改为返回 `(ok, error)`，失败原因进 `last_error`。
  - 积压 > 200 条或单次写入 > 800ms 时，写一条 `warn` 事件（30 秒节流），复用告警通道；检查在**写线程**里做，不进下单路径。
  - `/system/diagnostics` 新增「写库队列」检查项，诊断页直接可见；同时修正了诊断项注释编号。
- **验证**：
  | 项 | 结果 |
  |---|---|
  | 纯计数 | 4 次提交（含 1 次人为抛错）→ `total=4, failed=1, last_error='boom'` |
  | 诊断接口 | 新增「写库队列」项，`ok=true`，detail 显示积压/未完成/累计/失败/最近耗时 |
  | 回归 | 14 条契约测试通过，服务重启 health ok |
- **范围**：只做观测与告警；失败重试/死信见「未开始 #2」。

### 21. 部分止盈的前端标记（本次执行）
- **状态**：✅ 已完成
- **过程要点**：
  - `position_view.derived()` 统一输出 `partial_taken_at` 与 `partial_tp`，因此 `/engine/status` 与 `/engine/positions` 两个接口口径一致（该方法本来就是两接口的单一实现）。
  - 总览「当前持仓」表在标的后显示「已部分止盈」标签，悬浮提示「剩余仓位继续持有并受移动止损保护」。
- **验证**：
  - 纯函数：`partial_taken_at=0 → partial_tp=False`；置为时间戳后 `True`，`roi_pct` 等派生字段不受影响。
  - 接口：临时插入一条带 `partial_taken_at` 的持仓，`/engine/positions` 与 `/engine/status` 均返回 `partial_tp=true`。
  - 浏览器：总览持仓行 DOM 实测包含「已部分止盈」，截图确认渲染（首次未刷新是浏览器缓存，reload 后正常）。
  - 临时数据已清理（`positions=0`），14 条契约测试通过。

### 22. 历史数据归档与保留策略（本次执行）
- **状态**：✅ 已完成
- **背景（改造前）**：信号固定 3 天后**直接删除**（不可配置、无备份）；`engine_events` 有 `event_retention_days` 配置但**从未被使用**，表只增不减。
- **过程要点**：
  - 新增 `services/retention.py`：`purge_expired()` 统一处理 signals / events / equity / cooldowns。
  - **先归档再删除**：每批 400 行先写成 `data/archive/{signals|events}-YYYYMMDD.jsonl.gz`，再按 id 删除；批次间 `sleep(0.05)` 让出写锁，避免把读请求拖到 `busy_timeout`。
  - 新增配置 `signal_retention_days`（默认 14）、`equity_retention_days`（默认 30），`event_retention_days` 终于生效。
  - `worker._cleanup_async` 改为 `asyncio.to_thread(purge_expired)`，清理不再阻塞事件循环。
  - 诊断页「数据库」项显示「事件 X 条（保留 14 天）、信号 Y 条（保留 14 天）」，一眼看出保留策略是否按预期生效。
  - 参数页「数据保留」分区补齐三个保留天数。
- **验证**：
  | 项 | 结果 |
  |---|---|
  | 造旧数据（40 天前） | 写入 1 条旧信号 + 1 条旧事件 |
  | 执行清理 | `{'signals': 1, 'events': 1, 'equity': 0, 'cooldowns': 0}` |
  | 归档文件 | `signals-20261104.jsonl.gz` / `events-20261104.jsonl.gz`，内容为对应行 JSON |
  | 表内残留 | 旧信号 0 条、旧事件 0 条 |
  | 配置 | `/settings/system` 返回 `signal_days=14 / event_days=14 / equity_days=30` |
  | 回归 | 14 条契约测试通过，服务重启 health ok |

### 23. 回测参数网格（本次执行）
- **状态**：✅ 已完成
- **过程要点**：
  - 新增 `backtest/grid.py`：对 `entry_threshold / sl_atr_mult / tp_atr_mult / trailing_pct` 做笛卡尔积扫描，**组合上限 48 组**（超出截断并标记）。
  - 每组用 `StrategySettings.model_copy(update=...)` 生成变体后复用同一个 `run_backtest`，保证与单次回测口径完全一致。
  - 排序规则：总收益 → 盈亏比降序；返回 `best` 与完整 `items`，并标注「样本内排序，过拟合自负」。
  - 新增 `POST /backtest/grid`（池内标的校验 + 参数区间校验 400 + 用 `asyncio.to_thread` 跑，不阻塞事件循环）。
  - 回测页新增「参数网格」面板：四个逗号分隔输入 + 排名表（参数 / 交易数 / 胜率 / 盈亏比 / 期望 / 总收益 / 回撤 / 平均持仓）。
  - 契约测试新增 1 条（`runs=4`、排序单调、参数字段准确）。
- **实测**（ETHUSDT 5m × 1000 根，`entry_threshold×sl_atr_mult` = 3×3）：
  | 项 | 值 |
  |---|---|
  | 组合数 | 9 |
  | 耗时 | 293ms（单次约 32ms） |
  | 最优（样本内） | `entry_threshold=0.4, sl_atr_mult=1.5`，总收益 -0.98% |
  | 越界参数 | `sl_atr_mult=99` → 400 |
  | 回归 | 15 条契约测试通过 |
- **边界**：仍不含 AI 复核与盘口点差闸门；排序只代表样本内表现。

### 24. 写失败重试与死信队列（本次执行）
- **状态**：✅ 已完成
- **过程要点**：
  - 新增 `write_failures` 表（`kind / payload / error / attempts / resolved`），`create_all` 自动建表，无需手工迁移。
  - `DbWriter._run` 对**可重试错误**（`database is locked` / `table is locked` / `database is busy` / `disk i/o error`）做 50/100/200ms 退避重试（最多 3 次）；其它错误立即进死信，不浪费重试。
  - `_safe_call` 不再每次都写 `error` 事件（否则 3 次重试会刷 3 条），改为仅在最终失败时由 `_dead_letter()` 记一条 `error` 事件——自动走告警通道。
  - 死信保留完整调用参数（`{"args": [...], "kwargs": {...}}`），可通过 `POST /system/dead-letters/{id}/replay` 用原参数重放。
  - 新增 `GET /system/dead-letters`（未处理列表 + 总数）；诊断页新增「死信队列」检查项（有未处理即 `ok=false`）。
- **验证**：
  | 场景 | 结果 |
  |---|---|
  | 前两次 `database is locked`、第三次成功 | 执行 3 次后成功，**不产生死信** |
  | 不可重试错误 `boom` | 立即进死信，`attempts=1`，事件日志出现「已进死信队列」 |
  | `GET /system/dead-letters` | `total=0`（测试数据已清理） |
  | 诊断页 | 检查项 8 个，新增「死信队列」为 `ok=true` |
  | 回归 | 16 条契约测试通过（新增 `_is_retryable` 分类用例） |

### 25. 死信面板与批量重放（本次执行）
- **状态**：✅ 已完成
- **过程要点**：
  - 新增 `services/dead_letter.py`：`replay_one / replay_all / ignore_one`，路由层只做参数校验与错误码映射。
  - **幂等重放**：解析 payload 里的 `client_order_id`，若 `orders` 表已有同号记录 → 直接标记 resolved 并返回 `skipped`，不重复插入（覆盖「重试成功但进程崩溃、重启后重放」的场景）。
  - 新增 `POST /system/dead-letters/{id}/ignore`（仅标记，不写库）与 `POST /system/dead-letters/replay-all?limit=`（按时间正序批量处理，返回 `replayed/skipped/failed/unsupported`）。
  - 新增后台「写失败」页（`/admin/dead-letters`，运维分组）：待处理数标签、列表（时间/类型/`client_order_id`/错误/尝试次数）、单条重放/忽略、全部重放、20 秒自动刷新。
- **验证**：
  | 场景 | 结果 |
  |---|---|
  | 同 `client_order_id` 已存在 | 重放返回 `skipped`，`orders` 中该 cid 仍只有 **1 条**（未重复记账） |
  | 不支持的类型 `unknown_kind` | 单条重放 → **400**；批量重放计入 `unsupported=1`，记录保持未处理 |
  | `replay-all` | `{total:1, unsupported:1}`，行为符合预期 |
  | 页面 | `/admin/dead-letters` 渲染正常，空态显示「没有待处理的写失败记录」 |
  | 清理 | 测试死信与订单已删除，`dead_total=0` |
  | 回归 | 16 条契约测试通过 |

### 26. 告警分级路由与发送历史（本次执行）
- **状态**：✅ 已完成
- **过程要点**：
  - 新增配置 `alert_error_webhook_url`：**error 级优先走值班群**，未配置时回落默认 Webhook；warn/info 仍走默认通道。
  - 新增 `alert_logs` 表（`level / category / message / channels / ok / error`），每次发送后落库；随 `event_retention_days` 归档清理。
  - `POST /system/alerts/test?level=info|warn|error` 支持指定级别，用来验证路由是否按预期分流。
  - `GET /system/alerts/logs` 返回最近发送记录；系统诊断页新增「最近告警」面板（级别、类别、内容、通道、已送达/失败）。
  - 参数页「告警推送」分区补充 Error 专用 Webhook。
- **验证**：
  | 场景 | 结果 |
  |---|---|
  | `level=warn` | 默认 Webhook 命中 1 次，值班群 0 次 |
  | `level=error` | **值班群命中 1 次**，默认 Webhook 0 次 |
  | 发送记录 | `[('error','webhook',True), ('warn','webhook',True)]` |
  | 页面 | 系统诊断页「最近告警」表格渲染正常，结果列显示「已送达」 |
  | 清理 | 测试记录已删除，`alert_logs=0`；配置已恢复关闭 |
  | 回归 | 16 条契约测试通过 |

### 27. 回测净值曲线与 CSV 导出（本次执行）
- **状态**：✅ 已完成
- **过程要点**：
  - 回测页新增「净值曲线（按已平仓交易结算）」面板，复用 `EChart` 渲染 `equity_curve`，带面积渐变与缩放。
  - 成交明细与参数网格各加「导出 CSV」按钮（前端生成，带 BOM 以兼容 Excel 中文）。
  - **修掉一个数据契约冲突**：接口原来是 `**result.summary`（`trades` 是笔数）后面又用 `"trades": [...]`（明细数组）覆盖，导致前端把数组当笔数渲染，KPI 卡片直接显示一大段 JSON。现在 summary 的笔数改名为 `trade_count`，明细仍叫 `trades`。
- **验证**：
  | 项 | 结果 |
  |---|---|
  | 接口 | `trade_count=56 / trades_len=56 / equity_len=56`，两者不再互相覆盖 |
  | 页面 KPI | 「交易数 56 / 胜率 39.29%」正常，`document.body.innerText` 中不再出现 `entry_ts` 等 JSON 片段 |
  | 净值曲线 | ECharts canvas 渲染 1 个，曲线从 100 降到约 96.3，与总收益 -3.285% 一致 |
  | 导出 | 两个「导出 CSV」按钮均在位，无数据时禁用 |
  | 回归 | 16 条契约测试通过，服务 health ok |

### 28. 死信趋势与自动补偿（本次执行）
- **状态**：✅ 已完成
- **过程要点**：
  - `WriteFailure` 新增 `auto_attempts` 列（老库自动 `ALTER`），用于限制自动重放次数，避免无限刷。
  - `dead_letter.auto_replay()`：只挑 `resolved=false 且 auto_attempts < 3` 的记录，按时间正序处理；失败时累加 `auto_attempts`，达到上限后不再自动尝试（保留人工处理）。
  - `dead_letter.summary()` 返回 `{pending, stuck, last_ts}`；`/engine/status` 的 `stats.dead_letters` 直接暴露。
  - Worker 每 10 分钟触发一次自动补偿（`asyncio.to_thread`，不阻塞主循环），启动后 2 分钟内不触发。
  - 总览「超短线执行」面板标题旁新增 **「死信 N」** 标签（含卡住数），点击直达 `/admin/dead-letters`。
- **验证**：
  | 场景 | 结果 |
  |---|---|
  | 迁移 | `write_failures.auto_attempts` 存在 |
  | 可重放死信（平仓指向不存在仓位 → 写入函数空操作） | `replayed=1`，标记 resolved |
  | 不可重放死信（参数缺字段） | `failed=1`，`auto_attempts=1`，保持未处理 |
  | 汇总 | `before pending=2` → `after pending=1`，`stuck=0` |
  | 状态接口 | `stats.dead_letters={"pending":1,...}` |
  | 总览 | DOM 中检测到「死信 1」标签且可点击跳转 |
  | 回归 | 16 条契约测试通过，服务 health ok |
- **说明**：自动补偿只处理 `_persist_entry / _persist_close`，未知类型保持未处理，等人工判断。

### 29. 死信按类型分组统计（本次执行）
- **状态**：✅ 已完成
- **过程要点**：`dead_letter.summary()` 增加 `by_kind`（SQL `GROUP BY kind` 计数）；`/engine/status` 的 `stats.dead_letters` 直接带出；死信面板标题旁按类型显示数量徽章。
- **验证**：接口返回 `pending/stuck/last_ts/by_kind`；面板按 `_persist_entry / _persist_close` 分组渲染。

### 30. 回测分段统计（本次执行）
- **状态**：✅ 已完成
- **过程要点**：`_summarize` 新增 `by_hour`（UTC 小时）/ `by_weekday` / `by_hold`（<15m、15–60m、60–180m、≥180m）/ `by_exit`，每组含笔数、胜率、盈亏；回测页新增「分段统计」三表 + 出场原因徽章。
- **验证**：ETHUSDT 5m × 1000 根 → `by_hour=24 / by_weekday=4 / by_hold=3 / by_exit=stop_loss:35,take_profit:20,roi:2`；页面 DOM 四个区块均渲染。

### 31. 告警模板与静默时段（本次执行）
- **状态**：✅ 已完成
- **过程要点**：新增 `alert_template`（支持 `{level}/{category}/{message}/{time}`，写错自动回落默认格式）与 `alert_quiet_hours`（如 `23:00-07:00`，支持跨天，期间只放行 error）；参数页补齐两项。
- **验证**：模板渲染 `[ERROR][engine] test-msg @ 2026-11-04 22:51:42`；错误模板触发回落并告警日志；静默窗口覆盖当前时间返回 `True`、无关窗口返回 `False`。

### 32. 前端类型检查接入 CI（本次执行）
- **状态**：✅ 已完成
- **过程要点**：
  - 前端原本**没有 `tsconfig.json`**（`vue-tsc` 只会打印 tsc 帮助）。
  - 新增 `tsconfig.json`（`strict: false`，先保证可运行），安装 `vue-tsc@2.1.10`，新增 `npm run typecheck`。
  - 摸底出 **7 个真实类型错误**并全部修掉：路由数组缺 `RouteRecordRaw[]`、回测导出 `key` 为 `unknown`、持仓缺 `partial_tp` 字段、`notify` 用了不存在的 `'success'`、心法分类索引 `k as string`。
  - CI 前端 job 增加 `npm run typecheck` 步骤。
- **验证**：`vue-tsc --noEmit` **0 error**；`npm run build` 通过；17 条后端契约测试通过。

---


### 33. 写库健康与死信归因（本次执行）
- **状态**：✅ 已完成
- **过程要点**：
  - `DbWriter.stats()` 增加 `slow`（超过 800ms 的慢写次数）与 `max_ms`（历史最大写耗时），与既有的 `pending/failed/last_ms` 一起构成写队列画像。
  - 新增 `GET /system/write-health`：把队列统计、死信概览（`pending/stuck/by_kind`）与一句 **结论**（verdict）放在同一响应里——死信多但慢写少 = 并发冲突，死信多且慢写多 = 磁盘/锁竞争。
  - 诊断页「写库队列」项继续展示基础计数，`/system/write-health` 供排障时进一步下钻。
- **验证**：接口返回 `queue{pending,slow,max_ms,failed} + dead_letters{pending,stuck,by_kind} + slow_ratio_pct + verdict`；实测 `pending=0 slow=0 max_ms=0.0 dlq=0 → verdict=正常`。

### 34. 告警摘要与多值班人（本次执行）
- **状态**：✅ 已完成
- **过程要点**：
  - `alert_webhook_url` 支持**逗号分隔多个地址**，逐个发送并汇总结果，实现 error 多人同时值班。
  - 新增 `alert_digest_enabled` / `alert_digest_minutes`：开启后 **warn 先入内存缓冲**，由引擎每分钟检查是否到点，合并成一条「过去 N 分钟共 X 条告警：类别×次数」再发送；error 仍即时。
  - `flush_digest(force=True)` 供手动/停机触发；参数页补齐两个新字段。
- **验证**：缓冲 3 条 warn → `flush` 发送 1 条摘要，本地接收端收到 **2 次**（两个 webhook 地址各一次），内容为 `过去 60 分钟共 3 条告警：engine×2、db×1`。

### 35. 共享出口 IP 容灾（本次执行）
- **状态**：✅ 已完成
- **过程要点**：
  - 新增 `demo_backup_urls`（模拟盘备用线路，可填反向代理/多出口网关）；实盘沿用已有 `backup_base_urls`。
  - `client._rotate_base()`：按 `_bases()` 顺序轮转线路；**只对 GET 自动重试一次**，写请求保持「结果未知」语义不重发（避免重复下单）。
  - 命中 `-1003` 时**先尝试换线路**，换不了才本地熔断——不同出口 IP 不会被同一条封禁连累。
  - 环境切换时重置线路指针，避免测试网/主网串线。
- **验证**：把 `demo_base_url` 指到死端口 `127.0.0.1:9`、备用指向测试网，调用 `book_tickers` → 自动切换并成功返回 1 个 ticker，`client._base` 变为备用地址；随后已还原配置。

### 36. 归档回放（本次执行）
- **状态**：✅ 已完成（归档文件的"信号密度视图"见「未开始 #9」）
- **过程要点**：
  - 新增 `GET /system/archives`（文件名/类型/日期/大小）与 `GET /system/archives/{name}?limit=`（读取 gzip JSONL 尾部 N 行，带文件名白名单与路径穿越校验）。
  - 新增 `scripts/archive_replay.py`：把归档导入临时 SQLite（`rows` 表含 ts/level/category/symbol 等常用列 + 原始 payload），并打印时间范围与示例 SQL。
  - 系统诊断页新增「历史归档」面板：文件列表 + 一键预览最近 20 条。
- **验证**：归档列表 2 个文件；预览返回 1 行；CLI 导入现有 `signals-20261104.jsonl.gz` 成功并输出时间范围；面板在浏览器中渲染出 2 个文件行。

### 37. 前端开启全量 strict（本次执行）
- **状态**：✅ 已完成
- **过程要点**：
  - 先用 CLI 覆盖参数摸底：`--strictNullChecks` 仅 1 个错误，`--noImplicitAny` 与 `--strict` 竟然都是 **0 错误**——说明存量代码比预期干净。
  - 唯一的错误是 `DashboardView` 里 `status.heartbeat_age` 可能为 undefined，改为 `(status?.heartbeat_age ?? -1) >= 0`。
  - `tsconfig.json` 从 `strict: false` 直接切到 **`strict: true`**（同时移除临时的 `strictNullChecks` / `noImplicitAny` 覆盖）。
- **验证**：`npm run typecheck` 0 error；`npm run build` 通过；CI 的前端 job 已包含该步骤。

### 38. 信号密度视图（本次执行）
- **状态**：✅ 已完成
- **过程要点**：
  - `/engine/signals` 的 `summary` 新增 `by_hour`：用 SQLite `strftime('%H', datetime(ts,'unixepoch'))` 按小时聚合**近 7 天**的信号数，输出 24 个桶（无数据补 0）。
  - 信号审计页右侧新增 24 列迷你柱状图，悬浮显示「小时 · 条数」；纵轴按窗口内最大值归一。
- **验证**：接口返回 `by_hour_len=24`、非零桶 1 个（历史 64 条信号集中在同一小时）；页面渲染出 24 根柱子，`拒绝原因 Top` 与密度图并存。

---

### 39. 本地 Mock 交易所与应用侧 p99 压测（本次执行）
- **状态**：✅ 已完成
- **过程要点**：
  - 新增 `scripts/mock_exchange.py`：最小 `/fapi/*`（time / marginType / leverage / order / allOpenOrders / account），支持 `--delay-ms` 模拟 RTT；用临时库、假凭证，不碰运行数据。
  - 新增 `scripts/bench_exec.py`：走真实 `broker.open_position / close_position` 代码路径，输出整单与各分段 p50/p95/p99，并回读 `exec_metrics`。
  - 修正压测口径：首次调用含冷启动（建连 + 校时 + marginType/leverage），单独标注；高并发会触发本地订单限频（250/10s），需控制单量或看 `wait_ms`。
- **应用侧实测（mock，0ms RTT，60 单）**：
  | 分段 | p50 | p95 | p99 |
  |---|---|---|---|
  | open 整单 | 10.3ms | 11.1ms | 87.0ms（含首单冷启动；稳态 p99≈11ms） |
  | total_ms | 7.17ms | 10.85ms | 11.1ms |
  | protect_ms（2 个保护单） | 7.32ms | 7.94ms | 9.69ms |
  | http_ms（单次往返） | 2.27ms | 2.75ms | 2.90ms |
  | persist_ms（入队） | **0.04ms** | 0.07ms | 0.10ms |
  | sign_ms | 0.48ms | 0.63ms | 0.76ms |
- **模拟 150ms RTT（40 单）**：open p50 **311ms**；protect p50 **157ms**（= 1 个 RTT，证明两个保护单确实并发；串行会是 ~310ms）。

### 40. 公网 RTT 长尾采样（本次执行）
- **状态**：✅ 已完成
- **过程要点**：新增 `scripts/rtt_probe.py`，用权重 1 的 `/fapi/v1/time` 对测试网连续采样（先预热排除握手）。
- **实测（testnet，45s，79 样本）**：mean **309ms** / p50 **285ms** / p95 **468ms** / p99 **672ms**，0 错误。
- **意义**：这是端到端延迟的**地板**——应用侧再怎么优化也绕不过它。

### 41. 部分止盈的数量边界（本次执行）
- **状态**：✅ 已完成（交易所侧语义仍需凭证验证，见「未开始 #4」）
- **过程要点**：把 `qty × ratio → 按 step 取整 → 低于 min_qty 放弃` 抽成纯函数 `strategy.partial_close_qty()`，worker 改为调用它；新增 6 条边界用例（取整后为 0、低于最小量、无步进、非法输入）。
- **验证**：契约测试 **18 passed**。

### 42. 回测 / 实盘对账工具（本次执行）
- **状态**：✅ 已完成（等待真实样本，见「未开始 #5」）
- **过程要点**：新增 `scripts/reconcile.py`：回测 CSV × 实盘 CSV/数据库，按「方向 + 开仓时间容差」配对，输出匹配率、入场价偏差（bps）、出场原因不一致数、平均时间差；支持 `--self-test` 用合成数据自检。
- **验证**：self-test 5 笔全匹配、1 笔未匹配实盘、偏差 1.96bps、1 笔出场原因不一致，结果符合预期。

### 43. Rust sidecar 决策：暂不实施（本次评估）
- **状态**：✅ 已决策（不做，附数据与重启条件）
- **依据**：
  | 项 | 数值 | 说明 |
  |---|---|---|
  | 真实公网 RTT p99 | **672ms** | 采样测试网 79 次 |
  | 应用侧总开销 p99 | **11.1ms** | 本地 mock，0ms RTT |
  | 应用侧占端到端比例 | **≈1.6%** | 11.1 / 672 |
  | Rust 理论上限收益 | **≤1.5%** | 即使应用侧归零 |
  | 已落地的"保护单并发"单项收益 | **150–300ms（30–45%）** | 3 个 RTT → 2 个 RTT |
- **结论**：当前 p99 由网络 RTT 主导，换语言的天花板不到 2%；同样的工程量投在"减少往返次数/连接复用"上收益高一个数量级。**Rust sidecar 暂不实施**。
- **重启条件**（任一满足再评估）：① 应用侧 p99 超过 50ms；② 端到端 p99 目标进入 100ms 以内（需要 colocation/VIP 通道）；③ 出现 GC/HOL 阻塞导致的 >50ms 尖刺（当前 p99 未出现）。

### 44. AI 模型接入：端点探测与一键切换（本次执行）
- **状态**：✅ 已完成
- **背景**：原来只能手填模型名；不同供应商的版本段不同（`/v1`、`/v4`、`/api/paas/v4`），填错就 404，且必须先保存供应商才能试。
- **过程要点**：
  - `ai/client.fetch_remote_models()`：按「原样拼接 → 去版本段 → 补 `/v1`」生成候选地址；Anthropic 用 `x-api-key` + `anthropic-version`；兼容 `data` / `models` / 顶层数组三种返回；12s 超时；返回实际命中的 `endpoint` 与 `tried` 列表。
  - 新增 `POST /ai/providers/fetch-models`：可只带 `base_url + api_key` 先探测（不必先保存）；带 `provider_id` 时自动复用已存密钥。
  - `GET /ai/providers/{id}/models` 升级为富结构（模型带 `name/context`、`endpoint`、`tried`），旧调用方仍可用 `models[].id`。
  - AI 页面：表单内「探测端点」按钮 → 结果以模型 chips 呈现，点一下填入模型名；列表每行新增「探测」→ 弹窗选择器，点击即 PATCH 保存并刷新。
  - 连接失败时错误信息补齐底层原因（`ConnectError` 不再只有类型名）。
- **验证**：
  | 场景 | 结果 |
  |---|---|
  | 本地 mock `/v1/models`（3 个模型） | 表单与弹窗均列出 3 个，`endpoint` 正确 |
  | 点击 `qwen-plus` | `PATCH` 生效，provider.model 变为 `qwen-plus` |
  | 非法地址 `ftp://bad` | 返回「Base URL 必须以 http:// 或 https:// 开头」 |
  | 真实供应商不可达 | 错误显示为 `ConnectError: ...`，并附 `tried` 数量 |
  | 回归 | 18 passed、`vue-tsc` 0 error |
- **清理**：测试供应商与本地 mock 已删除，`/ai/providers` 恢复为空。

### 45. API Key 类型识别：支持 Ed25519 签名（本次执行）
- **状态**：✅ 已完成
- **问题**：开仓检查报 `BinanceError: [-2014] API-key format invalid.`。检查配置后发现「Key」以 `MCow...` 开头、长度 60，「Secret」以 `MC4C...` 开头、长度 64——这是**币安 Ed25519 密钥对**（SPKI / PKCS#8 的 base64），而客户端一直只用 HMAC-SHA256 签名，测试网按"格式非法"拒绝。
- **过程要点**：
  - `exchange/client.py` 新增 `sign_query()`：用「能否按 base64 DER 解析出 Ed25519 私钥」判定类型——是则 `base64(ed25519_sign(query))`，否则保持 `hex(hmac_sha256)`；私钥解析结果按 secret 缓存，热路径无额外开销。
  - `-2014` 且识别为 Ed25519 时，错误信息追加「当前 Key 为 Ed25519 类型，币安测试网不支持，请改用 HMAC 密钥」。
  - 修复诊断项「交易所接入」的 `'BinanceClient' object has no attribute 'client'`（`binance_client.client.ping()` → `binance_client.ping()`），该项从长期误报恢复为真实结果。
  - 参数页密钥区补充提示：两种密钥类型都支持，并说明 Ed25519 的 Key/Secret 分别该填什么。
- **实测判据**（同一台机器、同一时间窗）：
  | 密钥类型 | 是否注册 | 测试网返回 |
  |---|---|---|
  | Ed25519（格式合法） | 未注册 | `-2014 API-key format invalid` |
  | HMAC 64 位（格式合法） | 未注册 | `-2015 Invalid API-key, IP, or permissions` |
  → 说明**测试网在格式层就拒绝 Ed25519**，与密钥是否正确无关。
- **验证**：自签 Ed25519 能用公钥验签通过；HMAC 结果与原生 `hmac.new` 逐字节一致；契约测试 **19 passed**；诊断「交易所接入」转 True。
- **待办（用户侧）**：在币安测试网重新生成 **HMAC（System generated）** 密钥后填入「数据源」，交易通道即可连通。

### 46. Ed25519 凭证支持完善（本次执行）
- **状态**：✅ 已完成（客户端侧；密钥需在币安侧正确创建）
- **依据**：币安官方《API Key Types》——Ed25519 由用户本地生成密钥对，**签名用 Base64**（HMAC 用 hex），私钥不出本地。
- **过程要点**：
  - `sign_query()` 已按官方规范：Ed25519 → `base64(sign(query))`，HMAC → `hex(hmac_sha256)`。
  - 新增 `normalize_pem()`：自动去掉 `-----BEGIN/END-----` 头尾与换行——用户从 openssl / 币安页面复制 PEM 直接粘贴即可，不必手工压成一行。
  - 新增 `looks_like_public_key()` 与更精确的 `-2014` 提示：识别出「填的是公钥」时直接说明「API Key 应填币安创建密钥后返回的 Key，公钥只用于网页注册」。
  - 私钥读取同时兼容 **PEM** 与 **base64 DER** 两种粘贴形式。
- **实测**：
  | 头部取值（同一把 Ed25519 公钥） | 测试网返回 |
  |---|---|
  | 60 位 base64（公钥） | `-2014 API-key format invalid` |
  | 去 padding / hex 公钥 / URL 编码 PEM | 均为 `-2014` |
  | 64 位普通字符串（未注册 HMAC） | `-2015 Invalid API-key` |
  → 说明**公钥不能直接当 API Key 用**，头部必须是币安下发的 Key（格式为普通串）。
- **验证**：PEM 私钥签名可用公钥验签；PEM 公钥归一化后以 `MCow` 开头且能被识别；契约测试 **21 passed**。
- **待办（用户侧）**：在币安测试网创建 Ed25519 密钥时，把**公钥 PEM** 粘到币安页面，然后把币安返回的 **API Key** 与本地私钥分别填入本系统。

---

## 二、未开始

### 1. 真实下单 p99 采样（需测试网凭证）
- **状态**：⬜ 未开始（埋点、mock 压测、RTT 采样均已就绪，只差凭证）
- **目标**：把"应用侧 11ms + RTT"拼成完整端到端基线，并验证并发保护单在真实网络下的收益。
- **待办**：
  - 配置模拟盘 API 凭证，启动引擎。
  - 5 单/分钟与 60 单/分钟两档各采 2 万单，记录 `total/http/persist/protect` 的 p50/p95/p99/p99.9/max。
  - 与 `scripts/bench_exec.py --delay-ms` 的预测值对比，确认无隐藏长尾。

### 4. 部分止盈的实盘验证（需测试网凭证）
- **状态**：⬜ 未开始（应用侧边界已用纯函数+测试覆盖）
- **待办**：
  - 真实部分平仓一轮，确认 reduce-only 数量与本地 `qty` 一致。
  - 确认部分止盈后保护单仍按剩余数量生效（`closePosition` 与数量解耦，需实测）。
  - 部分平仓 `TradeRecord` 与交易所账单逐笔对账。

### 5. 回测 / 实盘同口径对账（需实盘样本）
- **状态**：⬜ 未开始（工具已就绪：`scripts/reconcile.py`）
- **待办**：
  - 积累至少 30 笔实盘成交后运行对账，输出入场价偏差分布。
  - 用实测滑点校准回测默认的 2bps 假设。
