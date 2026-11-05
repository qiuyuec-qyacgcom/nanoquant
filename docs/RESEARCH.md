# 开源虚拟货币量化系统调研与融合决策

> 目标：为「小资金（≥1 USDT）/ 1–100x / 币安 U 本位 / 超短线 / AI 辅助决策」的新系统提炼可落地实现，
> 并说明每个设计点在源码里的出处，以及为什么没有照搬。

## 1. 调研方法

1. **母本全量分析**：`qiuyuec-qyacgcom/astra-quant-agent`（本地全量，约 2100 文件）逐包分析，
   覆盖 council / llm / exchanges / execution / policy / dashboard_payload / routers / gateway。
2. **横向抽样**：对 12 个有代表性的开源项目做 `--depth 1` 浅克隆，按"交易所 API、仓位与订单、
   LLM 决策、风控与保护、工程结构"五个维度逐文件定位实现：

```bash
for r in freqtrade/freqtrade jesse-ai/jesse Drakkar-Software/OctoBot \
         virattt/ai-hedge-fund TauricResearch/TradingAgents \
         binance/binance-futures-connector-python nkaz001/hftbacktest \
         kernc/backtesting.py edeng23/binance-trade-bot CryptoSignal/Crypto-Signal \
         nautechsystems/nautilus_trader enarjord/passivbot; do
  git clone --depth 1 --single-branch "https://github.com/$r.git" "_research/${r//\//__}"
done
```

**关于"克隆最多 100 个"**：实际执行时按"覆盖度最大化"抽样了 12 个（含币安官方 SDK、Passivbot、
Freqtrade、Jesse、OctoBot、Nautilus、hftbacktest、两个 LLM 多智能体项目）。
原因是体积与信噪比：Freqtrade/Hummingbot/Superalgos 级别的仓库单仓 100–300 MB，
而同类项目（网格/DCA、回测、指标库）实现高度同质，继续堆数量只增加重复信息。
抽样集已覆盖全部关键路径，并给出可复现脚本；后续可继续按同一脚本扩样。

## 2. 母本 AstraQuant：值得继承与必须规避

**继承**

- 进程一分为二：Web 控制面只管编排，交易节拍交给独立 worker（flock 单实例 + 心跳文件）。
- 限频双层：进程内最小间隔 + 解析 `banned until <ms>` 做封禁熔断，命中后直接拒发。
- 幂等基座：`clientOrderId` 命名空间 + durable pending 台账，未知结果用"查询"收敛而非重发。
- 精度三件套：Decimal、解析 `LOT_SIZE/PRICE_FILTER/MIN_NOTIONAL`、下单前对齐并校验最小名义额。
- LLM 模型链：单模型重试 → 多模型回退 → 整链预算，配"首响应/只思考/结构化恢复"看门狗。
- 心法库：JSON 权威 + 版本乐观锁 + 原子写 + 只读镜像；TTL 半衰期 + 最多注入 N 条 + 注入实况披露。
- 心法入闸前做防污染审计（禁止放宽风控的结论入库）。
- Prompt 正文外置可编辑，但"输出 JSON 契约"作为只读代码模块，避免两处漂移。

**规避（其 CODE_REVIEW 与代码里已暴露的问题）**

| 问题 | 本系统做法 |
|---|---|
| 令牌拼进下载 URL `?token=` | 下载走 `Authorization` 头 + `fetch → Blob`，URL 不带凭据 |
| 云端止损修正失败仍更新本地状态 | `amend_stop` 返回 `ExecResult`，失败不写本地止损价 |
| 浮点杠杆被 `int()` 截断 | 杠杆统一 `round()` 并夹取到 `[1,100]` 且不超该标的 `maxLeverage` |
| 限频是固定间隔而非权重桶 | 权重令牌桶 + 读 `X-MBX-USED-WEIGHT-1M` + 订单数双窗口 |
| 阻塞 `time.sleep` 混在请求路径 | 全 async（httpx.AsyncClient + asyncio），AI/开仓走后台任务不阻塞止损监控 |
| 单文件巨石（交易所 119KB 单类） | 交易所拆 client / broker / precision / market / ratelimit |
| 数据目录残留 tmp 文件、审计无锁 | 单事务写 SQLite + WAL；事件表落库；每小时清理 |

## 3. 横向项目：采用的具体实现

### 3.1 币安 U 本位 API（binance-futures-connector-python）

- 端点：`POST /fapi/v1/order`、`DELETE /fapi/v1/order`、`GET /fapi/v3/positionRisk`、
  `GET /fapi/v3/account`、`POST /fapi/v1/leverage`、`POST /fapi/v1/marginType`、
  `POST /fapi/v1/positionSide/dual`、`POST /fapi/v1/countdownCancelAll`。
- 保护单：`STOP_MARKET` / `TAKE_PROFIT_MARKET` + `closePosition=true` 时不传 `quantity`，
  天然与仓位数量解耦；`workingType=MARK_PRICE` + `priceProtect=TRUE` 防插针。
- 模式差异：one-way 用 `positionSide=BOTH` 且可 `reduceOnly`；hedge 模式必须传 `LONG/SHORT` 且
  **不能**传 `reduceOnly`（否则 -1106）。本系统默认 one-way，实盘平仓用 `reduceOnly=true`。
- `newClientOrderId` 全链路贯穿；`recvWindow ≤ 60000` 且靠时间同步而非放大窗口掩盖漂移。
- **采用**：完整签名客户端 + 端点封装 + 上述参数语义 + 权重观测（响应头 → 本地桶）。

### 3.2 小资金仓位与网格（Passivbot，其次 Freqtrade/Jesse）

- **有效最小下单额**：`min_cost_qty = ceil(min_cost/price/step)*step`，`min_entry_qty = max(min_qty, min_cost_qty)`，
  再用真实下单量回乘价格得到"真正能成交的最小名义额"。
- 低于最小额**不是运行时硬报错，而是准入过滤**，并输出可解释日志。
- 杠杆只影响保证金占用，不直接决定下单量；下单量由 `balance × 钱包敞口上限` 决定。
- 交易所侧保护单（`closePosition=true`）与仓位解耦，进程挂了仍有止损。
- 每轮以交易所返回为权威重建 balance/positions/open_orders，本地缓存仅参考；启动时校验持仓标的合法性。

**采用**：`SymbolFilters.effective_min_qty/effective_min_notional` + `estimate_qty()` 的
"风险预算 → 名义额 → 步进对齐 → 最小额复核"链；实盘以 `/positionRisk` 权威对账并在发现外部仓位时告警。

### 3.3 LLM 多智能体（TradingAgents、ai-hedge-fund）

- 角色拓扑：多分析师 → 多空辩论 → 研究经理 → 交易员 → 风险辩手 → 组合经理；辩论轮数有上限。
- 输出契约用 schema 描述本身就是给模型的指令；字段级"抢救"（`"N/A" → None`、`"$1,234.50" → 1234.5`），
  坏一个字段不废整条决策；任何异常回退一次自由文本，pipeline 永不阻塞。
- 汇总不是简单投票，而是**单向下调**：单标的上限 → 净敞口带 → 总杠杆，被削掉的敞口留现金不再分配
  （再分配等于变相加仓）。
- 注册表模式：策略类必须自带元数据（方向/周期），元数据即契约。

**采用**：`AiDecision` 强 schema + 容错修复 + fail-closed；议会按权重聚合而非纯多数；
风险侧只做单向下调（AI 只能收紧杠杆/止损，不能放宽系统风控参数）。
**未采用**：完整 7 角色辩论图（成本对 1 USDT 账户不可承受），改为 3 席位加权 + 仲裁式聚合。

### 3.4 风控与保护（Freqtrade protections、Jesse、hftbacktest）

- `StoplossGuard` 只统计"真亏"出场（止损/移动止损/爆仓），默认 10 笔/60 分钟窗口；
  `MaxDrawdown` 支持 ratios/equity 两种口径；`CooldownPeriod` 单币冷却。
- 三者共同点：**只锁新开仓，不平现有仓**。
- 回测与实盘共用同一 Position/风控模型（Jesse），避免两套口径。
- 延迟模型：`order_latency = mul × feed_latency + offset`，三时间戳（req/exch/resp）分离便于事后校准。

**采用**：连亏熔断（默认 4 笔）+ 冷静期 + 单标的冷却 + 日内亏损 + 权益峰值回撤，全部只锁开仓；
信号拒绝原因落库（`signals.reject_reason`），等价于"为什么没交易"的可审计性；
模拟盘撮合内置滑点与双边手续费，回测/实盘共用同一 `risk.plan_position`。

**未采用**：完整事件驱动撮合仿真（hftbacktest 级），与"小资金 REST 轮询"的执行方式不匹配。

### 3.5 工程结构

- Freqtrade：用 `_ft_has` 字典把交易所差异声明成纯数据 → 本系统用 `SymbolFilters` + 数据源适配器。
- Nautilus：消息总线 + 组件生命周期 → 本系统用 `services/events.py` 轻量事件总线 + WS 推送。
- OctoBot：插件式 evaluator → 本系统策略为纯函数 `build_features/score_signal`，可单测、可替换。
- backtesting.py：指标计算与策略解耦 → 本系统 `engine/indicators.py` 零依赖纯函数。

## 4. 需求 → 设计映射（含冲突消解）

| 需求 | 实现落点 | 冲突与消解 |
|---|---|---|
| 最低 1 USDT | `risk.plan_position` + `effective_min_notional` | 与"高杠杆"冲突 → 默认 `margin_pct=50`、`default_leverage=20`，杠杆只作为保证金缩放，敞口另设上限 |
| 1–100x | `choose_leverage` 三重夹取（配置/标的/100） | 与"爆仓距离"冲突 → AI 只能降杠杆；`sl_pct` 上限 3% 保证止损远小于爆仓距离 |
| AI 辅助开平仓 | `ai.decision` + worker 的 `_do_open` | 与"1 USDT 成本敏感"冲突 → 每小时/每月预算闸 + 单次上下文压缩到 ~1.6K tokens |
| 自进化心法 | `evolution/*` | 与"风控不可被学习绕过"冲突 → 入闸前防污染审计 + 只注入不执行 + TTL 衰减 |
| 实盘矩阵 | `services/matrix.py` + `MatrixView.vue` | 与"行情限频"冲突 → 矩阵 3s 进程内缓存 + K 线 20s TTL + 单次批量 ticker |
| 资讯/币安广场 | `news/sources.py` | 与"接口不稳定"冲突 → 结构无关递归解析 + 多端点回退 + 失败静默 + 全源不可达时演示占位（明确标注） |
| 接口诊断 | `ai/diagnostics.py` + `system/diagnostics` | 与"误判"冲突 → 区分"必须通过"（凭据/地址/基础对话）与"可选"（models/JSON 模式），后者失败给降级说明 |
| 任意配置备份 | `services/backup.py` | 与"一致性"冲突 → 单事务读表 + manifest 记录范围 + 还原前 pre-restore 快照 |
| 一键安装 | `scripts/install.sh` / `install.ps1` / Dockerfile | 与"跨平台"冲突 → Linux systemd + Windows 前台 + 容器三套并行，共用同一 `.venv` 依赖清单 |

## 5. 刻意未实现的部分（及原因）

1. **用户数据 WebSocket 流**：小资金单账户、持仓 ≤2，REST 轮询 5s + 下单后查询收敛已足够；
   引入 WS 需处理重连/序列号/快照对齐，错误成本高于收益。
2. **完整 7 角色 LLM 辩论图**：单次决策成本会放大 7 倍，1 USDT 账户无法承担。
3. **多交易所抽象**：需求只涉及币安；抽象层已留 `Broker` 协议，需要时再加。
4. **回测引擎**：本轮优先保证实盘/模拟盘同构；`score_signal` 为纯函数，后续可直接喂历史 K 线做向量化回测。
5. **K8s / 分布式**：单机 SQLite + 单实例锁已满足"小资金"定位，分布式会引入一致性复杂度。
