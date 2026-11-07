# NanoQuant 任务跟踪

> 更新日期：2026-11-06
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

### 47. 单行 PEM 导致的 -1022 签名失败（本次执行）
- **状态**：✅ 已完成，交易通道已连通（余额 5000 USDT 可读）
- **现象**：换用币安下发的 API Key 后，`-2014` 消失，但变成 `[-1022] Signature for this request is not valid.`
- **根因**：Secret 是**被压成一行的 PEM**——`-----BEGIN PRIVATE KEY----- MC4C... -----END PRIVATE KEY-----` 全在同一行、用空格分隔。旧的 `normalize_pem()` 按**行**过滤，整行以 `-----` 开头于是被整段丢弃，返回空串 → 私钥解析失败 → 静默回落到 HMAC 签名 → 签名对不上。
- **修复**：
  - `normalize_pem()` 改为按**标记片段**删除（正则去掉 `-----BEGIN ...-----` / `-----END ...-----`），兼容「标准多行 PEM / 单行 PEM / 纯 base64」三种粘贴形态。
  - `_private_key()` 统一支持 Ed25519 与 RSA 私钥（PEM 与 DER 均可），结果按 secret 缓存。
  - RSA 签名走 `PKCS1v15 + SHA256` 再 base64；HMAC 仍为 hex。
- **验证**：
  | 项 | 结果 |
  |---|---|
  | 私钥解析 | `Ed25519PrivateKey`，body 长度 64 |
  | `/fapi/v3/account` | **200**，`totalWalletBalance=5000` |
  | `/system/diagnostics` | **8/8 全绿**，交易通道「币安合约接口连通」 |
  | 引擎账户 | `wallet/equity=5000`，来源 `binance` |
  | 契约测试 | **22 passed**（含单行 PEM 回归用例） |
- **经验**：签名类问题要先确认「密钥是否被正确解析」，而不是先怀疑算法——本次两次报错（-2014 / -1022）都是**粘贴形态**造成的。

### 48. base64 签名未编码导致间歇性 -1022（本次执行）
- **状态**：✅ 已完成
- **现象**：换成正确的 Ed25519 凭证后，`/fapi/v3/account` 有时成功、仓位同步却间歇报 `-1022 Signature for this request is not valid`。
- **根因**：Ed25519/RSA 的签名是 **base64**，字符集含 `+` `/` `=`；而拼 URL 时直接 `f"...&signature={signature}"`，没有二次编码。服务端把 `+` 解析成空格 → 签名被改写 → 校验失败。HMAC 的 hex 全是字母数字，所以这个问题一直没暴露。
- **修复**：签名拼进 query 前先 `quote(signature, safe="")`。
- **验证**：
  | 项 | 结果 |
  |---|---|
  | 诊断 8 项 | 全部 True |
  | 连续 6 次轮询（30s） | 账户恒为 5000，无新增 `-1022` |
  | 引擎 | enabled/running，账户来源 binance |

### 49. 真实下单 p99 采样（本次执行）
- **状态**：✅ 已完成（测试网真实成交 15 组）
- **过程要点**：
  - 新增 `scripts/p99_real.py`：按固定频率「开仓 → 平仓」，用临时库 + 复制真实凭证，不污染运行数据；输出整单与分段 p50/p95/p99。
  - 首次真实下单连续暴露 4 个只在实盘链路才会出现的问题（见下），全部修复后才拿到干净数据。
- **实测（5 单/分钟 × 15 组，全部成功）**：
  | 分段 | p50 | p95 | p99 | max |
  |---|---|---|---|---|
  | open 整单 | 875ms | 1086ms | 1086ms | 1086ms |
  | total_ms | 505ms | 1014ms | 1082ms | 1085ms |
  | http_ms | 225ms | 474ms | 491ms | 494ms |
  | protect_ms | 425ms | 512ms | 593ms | 613ms |
  | persist_ms | 0.05ms | 0.20ms | 0.71ms | 0.92ms |
  | sign_ms | 0.65ms | 1.17ms | 1.37ms | 1.45ms |
- **对比 mock 基线**（0ms RTT，total p50 7.17ms）：应用侧开销只占端到端 **约 1.4%**，与「Rust 收益上限 ≤1.5%」的结论一致。
- **顺带修掉的 4 个真实链路问题**：
  1. `-4061` 双向持仓：开仓前自动检测并切换**单向持仓**。
  2. `-4120` 保护单：币安已要求 `closePosition` 条件单走 **`POST /fapi/v1/algoOrder`**（`algoType=CONDITIONAL` + `triggerPrice` + `clientAlgoId`），旧接口保留回落。
  3. `-1111` 精度：保护单触发价按 `price_tick` 对齐后再格式化。
  4. `division by zero`：回执无 `avgPrice` 且缓存价为空时会产生 `entry_price=0` 的脏仓位；现在会用盘口中间价兜底，取不到则放弃落库，并在平仓侧加除零保护。
- **验证**：15/15 成交；平仓后 `openOrders`、`openAlgoOrders`、`positionRisk` 均为空（无残留保护单）。

### 50. 部分止盈实盘验证（本次执行）
- **状态**：✅ 已完成（测试网真实部分平仓，12 项断言全部通过）
- **过程要点**：
  - 新增 `scripts/verify_partial_tp.py`：开仓 → 平一半 → 全平，逐项比对「交易所数量 / 本地数量 / 成交记录 / 保护单」。
  - 过程中修掉 3 个真实问题：
    1. **平仓数量未对齐步进**：`qty/2` 可能不是 `qty_step` 的整数倍，导致 `-1111 Precision`；现在 `close_position` 内统一 `floor_to_step` 并按最小量校验。
    2. **部分平仓误撤保护单**：原来无条件 `cancel_all`，部分止盈后剩余仓位会裸奔；改为**仅全平才撤**。
    3. **保护单迁移后的撤单缺口**：`cancel_all` 撤不到 Algo 单；新增 `open_algo_orders / cancel_all_algo`（逐个 `algoId` 撤），全平与 `amend_stop` 都会调用。
- **实测（BTCUSDT，多头，qty=0.003 → 部分平 0.0015）**：
  | 断言 | 结果 |
  |---|---|
  | 开仓后 本地=交易所 数量 | PASS（0.003） |
  | 保护单已挂上 | PASS（2 条 algo） |
  | 部分平仓调用 | PASS |
  | 仓位仍 open、剩余 = 原始一半 | PASS（0.0015，本地=交易所） |
  | 生成部分平仓成交 | PASS（trade.qty=0.0015） |
  | 部分平仓后保护单仍在 | PASS（2 条） |
  | 全平成功、状态 closed | PASS |
  | 成交记录 2 笔、保护单清空 | PASS |
- **回归**：契约测试 22 passed；服务重启后诊断 8/8。

### 51. 交易所保护单可视化与撤销（本次执行）
- **状态**：✅ 已完成
- **背景**：保护单挂在币安 Algo 通道，本地账本看不到；页面只能看到持仓，无法确认"到底有没有挂上止损"。
- **过程要点**：
  - 新增 `GET /engine/protection`：拉取 `openAlgoOrders`，补上现价与**触发距离**，按标的/类型排序。
  - 新增 `DELETE /engine/protection/{algo_id}`：单条撤销，并写一条 warn 事件留痕。
  - 总览页新增「交易所保护单」面板：标的 / 方向 / 类型（止损·止盈）/ 触发价 / 标记价 / 距离 / 状态 / 撤单，撤单前二次确认并提示"撤销后该仓位将失去自动止损"。
- **实测**：
  | 项 | 结果 |
  |---|---|
  | 开仓后接口 | 返回 2 条（STOP_MARKET −3.0%、TAKE_PROFIT_MARKET +3.001%） |
  | 撤销一条 | HTTP 200，剩余 1 条 |
  | 清理 | 全平后 algo 单归零 |
  | 页面 | 「交易所保护单」面板与空态正常渲染 |
  | 回归 | 22 passed、`vue-tsc` 0 error |
- **附带发现**：测试网最小**名义额是 50 USDT**（`-4164`），下单量过小时会被拒——引擎的仓位计算需保证名义额高于该阈值。

### 52. 最小名义额与所需保证金可视化（本次执行）
- **状态**：✅ 已完成
- **背景**：最小下单额是**每个标的自己的** `MIN_NOTIONAL`（BTC 50 / ETH 20 / 多数山寨 5 USDT），此前只在报错时才暴露。
- **过程要点**：
  - `services/admission.py` 为每个标的补充 `required_margin`（= 最小名义额 ÷ 当前杠杆）与 `required_margin_at_max`（÷ 标的允许的最高杠杆），并在「可交易标的」结论里直接点出**最低门槛标的**及其保证金。
  - 开仓检查页新增「所需保证金」列，主值按当前杠杆，括号内给出「最高 Nx 时」的下限。
- **实测（测试网，风控 default_leverage=1 / max_leverage=49）**：
  | 标的 | 最小名义额 | 1x 保证金 | 49x 保证金 |
  |---|---|---|---|
  | ETHUSDT | 20.23 U | 20.23 U | **0.41 U** |
  | BTCUSDT | 53.36 U | 53.36 U | **1.09 U** |
  → 印证「余额零点几 U 也能开，只要杠杆够」：真正的门槛是名义额，余额只需覆盖 名义额÷杠杆。
- **验证**：接口字段齐全、页面渲染「所需保证金 / 最高 49x」；`vue-tsc` 0 error、构建通过。

### 53. 运行时抖动探针（本次执行）
- **状态**：✅ 已完成
- **目的**：Rust 与否的判据不是"平均 RTT"，而是 **Python 自身的尾部抖动**。
- **过程要点**：新增 `services/runtime_probe.py`（事件循环延迟 + GC 停顿时长），随应用启动；`GET /system/runtime` 输出 p50/p95/p99/max、>20ms/>50ms 计数、GC 停顿分位。
- **实测（2 标的、强制 10 轮扫描、约 40s）**：
  | 指标 | 值 |
  |---|---|
  | 事件循环 lag | p50 10.4ms / p95 11.6 / p99 14.7 / max 37.8 |
  | >50ms 阻塞 | **0 次** |
  | GC 停顿 | 16 次，p99 **7.8ms**，累计 7.8ms |
- **说明**：Windows 默认定时器粒度约 15.6ms，lag 的 p50 偏高属测量基线，看 max 与 >50ms 计数更有意义。

### 54. Rust 执行内核 sidecar：协议 + 影子签名（本次执行）
- **状态**：✅ 骨架完成（当时未编译；已于 #56 在 Windows 上编译通过）
- **决策依据**：用户确认将迁移到交易所同区域 VPS → RTT 从 250ms 降到 ~5–40ms，应用侧 10–40ms 抖动将占主导，Rust 收益从 1–3% 提升到 12–40%，值得做。
- **交付物**：
  - `rust/exec-service/`：`main.rs`（UDS + 可选 TCP 回环、JSON Lines 分发）、`sign.rs`（HMAC hex / Ed25519 base64，含与 Python 对齐的 3 个向量测试）、`model.rs`。
  - `docs/EXEC_PROTOCOL.md`：传输（unix/tcp）、帧格式、op（health/plan/sign）、签名规则、影子比对流程、P2–P4 阶段计划与降级策略。
  - `backend/nanoquant/exchange/exec_client.py`：Python 侧 sidecar 客户端（支持 `unix:` / `tcp:`）。
  - `scripts/mock_exec_sidecar.py` + `scripts/verify_sidecar.py`：无 Rust 环境也能验证协议与签名一致性。
- **验证（本机，Python mock sidecar）**：
  | 项 | 结果 |
  |---|---|
  | health | PASS（algo=ed25519） |
  | 普通下单 query 签名 | PASS（88 字符 base64，与本地 `sign_query` 逐字节一致） |
  | 保护单 query 签名 | PASS |
  | 空 query | PASS（被拒绝） |
  | Rust 侧 | 3 个单测待在 VPS `cargo test` 验证 |
- **下一步**：VPS 上 `cargo test` → 接 `reqwest` 连接池 → 影子模式与 Python 并行比对 → 灰度切流。

### 55. 影子模式接入真实签名链路（本次执行）
- **状态**：✅ 已完成（Python 侧 P2 闭环；Rust 侧编译验证见 #56）
- **背景**：此前只有 `scripts/verify_sidecar.py` 的离线比对——真实下单链路上算出来的签名，从来没有和 sidecar 对过。
- **过程要点**：
  - `exchange/exec_client.py` 改为**长连接复用** + 串行锁；任何超时/断流都丢弃连接（避免读到上一条的迟到响应）；新增 `requests/errors/timeouts/connects` 计数与**锁内**往返计时。
  - 新增 `services/exec_shadow.py`：`off / shadow / enforce` 三档。`shadow` 走后台任务（并发上限 32），对下单零影响；`enforce` 同步比对，**只有不一致才 fail-closed**，sidecar 不可用时按协议降级回 Python 签名。
  - 掉线静默期 5s：掉线后不再为每个签名等一次超时；`skipped` 区分「抽样跳过」与「静默跳过」。
  - `client._request` 算出签名后立即调用 `exec_shadow.observe()`；`enforce` 不一致转 `-1022`，订单不会发出。
  - 新增配置：`system.exec_mode / exec_socket / exec_sample_pct / exec_timeout_ms`；参数页新增「执行内核（sidecar）」分区。
  - 新增 `GET /system/exec-sidecar`、`POST /system/exec-sidecar/self-test`、`POST /system/exec-sidecar/reset`；诊断新增第 9 项「执行内核」；系统诊断页新增面板（模式/通道/比对计数/耗时分位/自检/清零）。
- **实测**（真实测试网凭证 + mock sidecar 用同一把 Ed25519 私钥）：
  | 场景 | 结果 |
  |---|---|
  | shadow 稳态 | `matched` 10–28、`mismatched` 0；p50 **0.48ms** / p99 **1.79ms**（TCP 回环） |
  | 自检接口 | `ok=true`，两侧均 88 字符 base64，`compare_ms=0.23` |
  | sidecar 被杀 | `errors=4 / skipped=6`，verdict 变「不可用…已降级」，引擎 enabled/running 不受影响；max 814ms（单次超时，已排除排队） |
  | sidecar 恢复 | 自动重连，`matched` 继续增长（15→20） |
  | enforce | `matched=15 / mismatched=0`，下单链路正常，比对 p99 **0.92ms** |
  | 回归 | 契约测试 **30 passed**、`vue-tsc` 0 error、`npm run build` 通过 |
- **顺带修掉**：
  - 比对延迟口径：改用客户端锁内往返，避免把排队时间算成 sidecar 延迟（宕机时旧口径 p99 虚高到 3s）。
  - `rust/exec-service/src/sign.rs::normalize_pem` 旧状态机进入 `BEGIN` 后永远退不出（PEM 被清空 → 静默回落 HMAC → 签名必错），改为按 `-----...-----` 标记片段删除。**尚未在本机编译验证**（无 Rust 工具链）。
- **切流判据**：`counts.mismatched == 0` 且自检 88 字符 base64 一致，才允许把 `exec_mode` 调到 `enforce`。

### 56. Rust sidecar 实机编译与真实二进制验证（本次执行）
- **状态**：✅ 已完成（Windows 回环 TCP；Linux UDS 与 systemd 仍需 VPS）
- **背景**：#54/#55 只有源码与 Python mock，`cargo test` 从未跑过，等于"纸面 P2"。
- **工具链**（装在 F 盘，不动 C 盘）：`rustup` 1.99.1（`x86_64-pc-windows-gnu`）+ MinGW-w64 gcc 16.2.0（winlibs）。
- **首次编译暴露 3 个真实缺陷（全部修复）**：
  1. `use tokio::net::UnixListener` 无条件导入 → Windows 直接 `E0432`；改为 `#[cfg(unix)]`，并把 TCP 监听提到入口（Windows 只给 `EXEC_TCP` 时不再 panic，缺少时返回明确错误）。
  2. `stream.split()` 找不到方法（`E0599`，`AsyncReadExt` 在当前 tokio 下不提供）；改用 free function `tokio::io::split()`。
  3. `handle` 的 future 缺 `Send` 约束，`tokio::spawn` 会拒绝；补 `S: Send`。
- **验证结果**（release 构建，0.91MB，常驻 RSS 4MB）：
  | 场景 | 结果 |
  |---|---|
  | `cargo test` | **3 passed**（Ed25519 向量 / HMAC 向量 / 单行 PEM 归一化） |
  | 协议 + 签名比对（临时密钥） | Ed25519 88 字符、HMAC 64 字符均与 Python 逐字节一致；空 query 被拒 |
  | 真实凭证 shadow | `matched` 13–16 / `mismatched` 0，p50 **0.50ms** / p99 **1.18ms** |
  | 面板自检 | `ok=true`，两侧 88 字符，`compare_ms=0.12` |
  | **错误密钥**（shadow） | `mismatched=4`，verdict「签名不一致：禁止切流」，引擎不受影响 |
  | 杀掉 sidecar | `errors=2`、静默期剩余 2918ms、max 514ms（= 1 次 500ms 超时），引擎照常运行 |
  | 重启 sidecar | 自动重连，`matched` 10→14 |
  | enforce 稳态 | `matched=10 / mismatched=0`，p50 **0.176ms** / p99 **0.399ms**，下单链路正常 |
  | 回归 | 契约测试 **30 passed**、`compileall` 通过 |
- **顺带修掉**：
  - sidecar 请求成功后清空 `last_error`，否则面板会一直挂着早已恢复的「响应超时」。
  - `scripts/verify_sidecar.py` 自起 mock 模式会卡死：客户端 #55 起改为长连接，脚本却先 `server.wait_closed()`，
    处理协程还没退出 → 永远等下去。改为先 `client.close()` 再限时关闭 server，并支持 `--secret`/`EXEC_SECRET`
    对真实 sidecar 逐字节比对（之前外部 target 只校验协议）。
- **换密钥流程（实测有效）**：换 Secret 必须先 `POST /system/exec-sidecar/reset`——切换窗口内的在途请求会被判为不一致（本次故意用错密钥验证过），清零后立刻恢复 `mismatched=0`。
- **未覆盖**：UDS（Windows 无此能力）、systemd 守护、`enforce` 24h 灰度。

### 57. 回测保真度：资金费 / 部分止盈 / 点差成本 / 样本外验证（本次执行）
- **状态**：✅ 已完成（点差"闸门"仍无法回放，见边界）
- **背景**：回测不含资金费与部分止盈，点差只字未提，滑点 2bps 是拍出来的，参数网格只做样本内排序。
- **过程要点**：
  - `backtest/engine.py` 新增三个成本/行为参数：
    - `funding_bps_8h`（默认 1bp/8h）：按 UTC 00/08/16 结算点，用**当时剩余数量**扣/收（多付空收），
      记入权益与明细；
    - `spread_bps`（往返成本，开平各承担一半）；
    - **部分止盈与实盘同口径**：R 达标先平 `partial_tp_ratio`，剩余止损上移到保本；
      同一根 K 线内止损与部分止盈冲突时**按止损优先**（OHLC 无法判断先后，取保守）。
  - 明细/汇总补齐 `funding`、`partial_tp`、`funding_total`、`partial_tp_count`；入场手续费按腿分摊。
  - 新增 `backtest/walkforward.py` + `POST /backtest/walkforward`：滚动切分「前段选参 → 后段冻结检验」，
    输出每折参数、样本内/外收益、`overfit_gap_pct`、`consistency`；数据不足自动减折并报所需根数。
  - 回测页：表单加点差/资金费；明细加资金费列与「部分止盈」标签；新增「样本外验证」面板（KPI + 逐折表）。
  - `scripts/reconcile.py` 增加 `entry_diff_bps_median_abs` 与 `suggested_slippage_bps`（≥10 笔才给建议）。
- **验证**：新增 6 条用例（结算点计次、多付空收、半仓+保本、点差使收益下降、walk-forward 结构、短历史报错）；
  **45 passed**。真实接口当日被测试网 IP 限频（`-1003`，约 2h 封禁），改由桩 K 线覆盖路由与字段契约。
- **边界**：点差**闸门**（盘口过宽则不开仓）无法回放——历史 K 线没有买卖价，只能按成本近似；AI 复核仍不含。

### 58. 路由层测试 + 依赖锁定 + CI 补 Rust（本次执行）
- **状态**：✅ 已完成
- **过程要点**：
  - `tests/conftest.py` 在任何 nanoquant 导入前把 `NANOQUANT_HOME/DATA_DIR/DB/LOG_DIR/KEY_FILE` 指到临时目录，
    测试从此**不碰真实 data/ 与主密钥**（此前契约测试虽然没写库，但没有硬隔离）。
  - 新增 `tests/test_api.py`（9 条）：健康检查、未授权/伪造 token、登录失败、配置越界 400、
    `engine.mode` 不能绕过确认短语、执行内核/死信/写库健康字段、回测 404/422、
    桩 K 线跑通回测与 walk-forward（不依赖交易所）。
  - 依赖锁定：`requirements.in`（直接依赖）→ `requirements.txt`（uv pip compile，Python 3.12 + linux 平台；
    `uvloop` 手工补 `sys_platform != "win32"`，否则 Windows 装不上）。
  - CI：后端改跑 `pytest tests -q`；新增 `rust` job（`cargo test --release`）。
- **验证**：`pytest tests -q` → **45 passed**（约 2.1s）；运行后真实 `data/nanoquant.db` 无测试写入。

### 59. 主网演练手册 + 只读预检（本次执行）
- **状态**：✅ 文档与工具已完成；**演练本身未执行**（需主网凭证与资金）
- **交付物**：
  - `docs/MAINNET_DRILL.md`：前置条件、三段演练（最小名义额开平 / 部分止盈 / 资金费与对账）、
    判据、预算（≤2 USDT）、停止条件、回滚步骤、记录模板。
  - `scripts/mainnet_preflight.py --yes`：临时库 + 实盘凭证，**只读**检查连通性 / 账户 / 持仓模式 /
    遗留挂单 / 每个标的的最小名义额与所需保证金；致命项直接非零退出。
- **待办（用户侧）**：按手册执行 A/B/C 并回填记录表；把 `partial_tp_at_r` 等临时参数改回默认。

### 60. 组合风险控制 + 生产部署脚本（本次执行）
- **状态**：✅ 已完成
- **背景**：之前只有单笔/总量敞口上限，两个仓位可同时做多高相关币；仓位也不随波动率调整。
- **过程要点**：
  - `RiskSettings` 新增两个字段（默认都关闭，保持旧行为）：
    - `max_same_direction`（默认 0=不限制，建议小资金设 1）：同方向持仓上限，币种高度相关时同向多仓等于变相加杠杆；
    - `vol_target_atr_pct`（默认 0=关闭）：波动率目标，ATR% 高于目标自动缩仓、低于放大（限制 0.25–2x）。
  - `risk.py` 抽出纯函数 `side_cap_reason()` / `vol_risk_scale()`；`symbol_gates(..., side=)` 接入同向检查，
    `estimate_qty(..., risk_scale=)` / `plan_position` 透传缩放，worker 用 `features.atr_pct` 计算。
  - 参数页「仓位与杠杆」补两项并加提示。
  - 部署：新增 `deploy/nanoquant.service`、`deploy/nanoquant-exec.service`（sidecar UDS，`RuntimeDirectory` +
    `UMask=0077` 保证 socket 0600）、`deploy/README.md`；`scripts/start.ps1` 改为探测解释器
    （`-Python` / `NANOQUANT_VENV` / 仓库 `.venv` / 上级 `.venvtest`），新增 `scripts/stop.ps1`。
- **验证**：新增 3 条用例（同向计数、缩放钳位、仓位按 scale 减半）；**48 passed**；运行服务返回两个新字段默认值。

### 61. API 限频 + Prometheus 指标 + 启动脚本实测（本次执行）
- **状态**：✅ 已完成
- **过程要点**：
  - `SystemSettings` 新增 `api_rate_limit_per_min`（默认 0=关闭）与 `metrics_enabled`（默认开）。
  - 新增 `services/api_guard.py`：进程内滑动窗口按 IP 限流，`429` + `Retry-After`；
    `/health`、`/metrics`、OpenAPI/docs 不计数（探针不能被误伤）；配置读取失败时自动放行，避免把 API 全挡掉。
  - 新增 `GET /api/v1/metrics`（Prometheus 文本、只读、无需登录、不含账户/密钥/持仓）：
    `up` / `engine_running` / 写队列 pending·unfinished·failed·slow / 死信 pending·stuck /
    执行内核 matched·mismatched·errors·skipped·connected。
  - 参数页新增「安全与观测」分区。
  - **迁移场景实测** `scripts/start.ps1`：`NANOQUANT_VENV=F:\cb\.venvtest` + 临时 `data/` + 端口 8901
    → `/api/v1/health` 200，证明 F 盘共享 venv 下的一键启动已恢复。
- **验证**：新增 3 条用例（指标文本与脱敏、开关 404、限频 429 + 豁免）；**51 passed**。

### 62. 心法效果量化评估 + 前端 e2e 冒烟（本次执行）
- **状态**：✅ 已完成（CI 的 e2e job 待首次 runner 运行验证）
- **背景**：心法注入只进 prompt 文本（且不带 id），成交表里没有任何"决策时用了哪些心法"的快照，
  因此**无法分组比较**；`Mantra.hits/wins/losses` 有字段、`touch()` 有实现，但全仓无调用点。
- **过程要点**：
  - `worker._build_ai_context` 记录注入心法 id；`broker.open_position` 的 `features` 增加
    `ai_used` 与 `injected_mantra_ids`（平仓时随 `features` 自动复制到 `TradeRecord`）。
  - `review_trade()` 首次复盘时调 `mantra.touch(ids, won)` 回写命中/胜负，前端已有的「命中/胜负」列从此有数据。
  - 新增 `evolution/effectiveness.py` + `GET /evolution/effectiveness`：按「是否走 AI × 是否注入」分为
    injected / control / ai_off / legacy 四组，输出胜率、累计与单笔盈亏、盈亏因子、两组差值、
    样本门槛（各 ≥10 笔）与逐心法排名；明确标注**这是观察性对照，不是随机 A/B**。
  - `EvolutionView.vue` 新增「心法效果对照」面板（4 张 KPI + 逐心法表 + 口径说明）；
    顺带修正注入预览里「这里是真正进入模型上下文的文本」的错误文案（实际是 title/rule/health 的 JSON 快照）。
  - 前端 e2e：新增 `@playwright/test`、`playwright.config.ts`、`e2e/smoke.spec.ts`（3 条：
    公开首页 / 登录进总览 / 4 个后台关键面板），`npm run e2e`；CI 新增 `e2e` job（临时库起后端 + chromium）。
    路由是 **hash 模式**，首版 e2e 用 `/login` 全部找不到输入框，已按 `/#/login` 修正。
- **验证**：新增 3 条纯函数用例 + 1 条 API 用例 → **54 passed**；
  本机用系统 Edge 实测 `npm run e2e` → **3 passed (5.7s)**。

### 63. 轮询成本优化：资金费率缓存 / 信号合并 / 15s 下限（本次执行）
- **状态**：✅ 已完成
- **背景**：实测 2 标的 × 5s 轮询 ≈ 20 条信号/分钟（约 2.9 万条/天）；`premiumIndex`
  每轮拉全市场（权重 10）却无缓存，约 120 weight/min 属纯浪费。
- **过程要点**：
  - `exchange/client.py`：`funding_rates()` 缓存全市场资金费率 60s、`open_interest()` 逐标的缓存 60s，
    切换交易环境时随其它缓存一起清空；预计省下 100+ weight/min。
  - **信号合并**：`Signal` 新增 `repeats` 列（老库自动 ALTER）；同一标的/方向/拒绝原因的 flat 信号
    在 60s 窗口内合并成一行并累加 `repeats`，非 flat 信号仍每行独立（决策更新需要独立 id）。
    `/engine/signals` 用 `sum(repeats)` 还原真实扫描次数（`summary.scans`、按小时密度、Top 拒绝原因均按权重），
    信号审计页显示 `×N` 合并标记。实测 100s 内只写 **4 行、代表 14 次扫描**（旧逻辑约 33 行），写入量降约 88%。
  - **轮询下限 15s**：`EngineSettings.cycle_interval_sec` 由 `default=5, ge=1` 改为
    `default=15, ge=15`；`_migrate_legacy_group` 会把老库里的 1–14s 钳到 15 并**一次性写回**，
    避免整组配置校验失败后被回退成全默认值；参数页提示与输入框 `min=15`。
- **验证**：新增 5 条用例（下限校验与老库迁移、两类 REST 缓存、flat 合并与非 flat 不合并、
  API 400 与信号汇总字段）→ **59 passed**；本机 e2e 3 passed。

### 64. 修复「当前持仓不显示盈亏」（本次执行）
- **状态**：✅ 已完成
- **现象**：持仓行浮盈亏恒为 `0.00000 (0.0%)`，而交易所账户里浮亏已到 -88U。
- **根因**（实测对照）：交易所 `positionRisk` 的 `markPrice=0.00497112`、`unRealizedProfit=-88.80`，
  但本地 `mark_price` 被写成 24h ticker 的 `lastPrice=0.0049563`（测试网该值长时间不动）。
  合约盈亏按 **markPrice** 结算，用最新成交价估算就会得到 0；且 `_manage_positions` 每轮
  都会把 `_reconcile_live` 刚同步来的权威 markPrice 覆盖掉。
  另一个暴露的问题：账户接口偶发 `[-1] 服务异常` 时 `status()` 直接把整个账户面板刷成 0。
- **修复**：
  - `positions` 新增 `venue_unrealized / venue_ts`（老库自动 ALTER）；`_reconcile_live` 写入交易所
    权威盈亏与 markPrice；`_manage_positions` 不再用 lastPrice 覆盖，且出场判定也对齐 markPrice。
  - `position_view.derived()` 优先用 90s 内的交易所快照，超时才回退本地估算，并返回
    `unrealized_source`；面板显示「估算」标签、口径说明与「每轮同步」提示。
  - `status()` 账户接口失败时沿用上一轮成功读数并标记 `stale`，KPI 显示「缓存值」而不是 0。
  - 顺带修掉 `record_external_position()` 里模块级函数误用 `self.mode` 的 NameError（外部仓位根本写不进库），
    并补齐 `venue_*` 字段；修正空持仓 `colspan`。
- **验证**：新增 2 条用例（快照优先/超时回退、外部仓位写入）→ **61 passed**；
  真实服务重启后：账户浮盈亏 -90.36U、持仓行 -89.52U（同一轮同步的快照），浏览器实测
  持仓行显示 `-91.79427 (-3.7%)`、KPI 显示 `-85.3831`（两者为不同时刻读数，已在列头注明）。

### 65. 修复 -2019 Margin is insufficient（本次执行）
- **状态**：✅ 已完成
- **现象**：`13:33:32 ICNTUSDT 空 被拒 下单失败: 下单失败 -2019: Margin is insufficient.`
- **根因复盘**（用当时账户数据复算）：`equity≈5006`、`available≈2468.84`（已有一个 1000PEPEUSDT
  仓位占用约 2470 保证金），但 `estimate_qty()` **只按总权益**算：
  `margin_cap = equity × margin_pct(50%) × leverage(1) = 2503 USDT`；杠杆为 1 时所需初始保证金
  = 名义额 = 2503 > available → 交易所拒绝。已有仓位占掉的保证金完全没进入计算。
  「开仓检查」页也用同一个函数按 equity 估，因此会误报「可交易」。
- **修复**：
  - `estimate_qty(..., available=...)`：按 `available × 0.9 × leverage` 截断名义额（0.9 留手续费/
    资金费/保护单划转的安全垫）；连最小名义额都付不起时返回可读原因「可用保证金不足」并直接拒绝，
    不再把注定失败的订单丢给交易所。
  - `plan_position` 传入 `account.available` 并在 `plan` 里落库；`admission.evaluate` 的「可交易标的」
  同样改用可用余额，页面不再说谎。
  - `broker.open_position` 捕获 `-2019` 时给出 `margin_insufficient_hint()`：
    显示本单约需多少初始保证金、以及平仓/降风险/提杠杆三种处理方向；
    worker 不再拼出「下单失败: 下单失败 -2019」的重复前缀。
- **实测**（真实账户/ICNTUSDT 规格复算）：旧 qty 8507、需保证金 **2502.76 > 可用 2468.84**（必被拒）；
  新 qty 7552、需 **2221.80 < 可用 2468.84**，留 247 USDT 缓冲。
- **验证**：新增 3 条用例（可用余额截断、高杠杆仍受限、-2019 文案）→ **64 passed**。

---

## 二、未开始

### 1. VPS 上的 UDS 部署与 P3 接管
- **状态**：⬜ 未开始（二进制已在 Windows 编译验证，见 #56）
- **待办**：
  - VPS：`cargo test` + `cargo build --release`（Linux 产物）。
  - 起进程：`EXEC_SOCKET=/run/nanoquant/exec.sock EXEC_SECRET=... ./exec-service`，目录属主与 `0600` 权限。
  - `EXEC_SECRET=... python scripts/verify_sidecar.py --target unix:/run/nanoquant/exec.sock`（已支持 `--secret`/`EXEC_SECRET` 比对签名）。
  - 面板「自检」通过后切 `enforce` 灰度 24h（`mismatched` 必须恒为 0）。
  - P3：`reqwest` 连接池 + 订单状态机（Planned → Sent → Acked → Filled）+ 保护单并行 + 单写线程落库。

### 2. 回测 / 实盘同口径对账（需实盘样本）
- **状态**：⬜ 未开始（工具已就绪：`scripts/reconcile.py`，含 `suggested_slippage_bps`）
- **待办**：
  - 积累至少 30 笔实盘成交后运行对账，输出入场价偏差分布。
  - 用输出的 `suggested_slippage_bps`（中位绝对值，≥10 笔才给）替换回测默认的 2bps，
    并把实测资金费写进 `funding_bps_8h`。
