# 链路完整度对标：NanoQuant vs 同业量化系统

> 问题：对比网上其他量化交易系统，本项目的链路是否完整？是否能读取所有数据和参数进行交易与分析？
> 结论先行：**链路在「结构」上完整，在「数据留存」和「参数生效」上不完整。**
> **阅读说明：** 本文描述的是 2026-11-27 改造前的基线；文中 P0/P1/P2 缺口已于同日按五份任务文档关闭，完成标记与提交见第四节。
> 口径：本地代码全部以 ripgrep 复核 HEAD=d1952d0 实际内容；同业结论来自 2026-11-27 实际抓取的官方文档/官方仓库，均附 URL。
> 更新：2026-11-27 晚间，行号已同步到「止损口径重构」工作区状态；其中 **ATR 周期与 EMA 周期的分叉均已收口**。
> **2026-11-27 收官：** 对标缺口已按五份任务文档全部落地，完成标记见第四节「已落成并完成的任务文档」；里程碑 tag `p0-p2-parity-2026-11-27`。

---

## 〇、结论摘要

### 一句话结论

**本项目是一个「链路骨架完整、生产级运维面齐全、但数据不留存、且存在装饰参数」的实盘系统。**
它的决策→风控→执行→落库→复盘→写回参数这条主链是通的，运维与审计面甚至比多数同类开源项目更厚；
但它**不保存任何行情数据**，且**仍有个别已注册参数在计算侧不产生任何作用**（装饰参数，目前只剩 `atr_stop_mult`），
因此「读取所有数据和参数进行交易与分析」这个问题的答案是**否**。

### 核心对比表

| 环节 | 本项目 | Freqtrade | NautilusTrader | LEAN | Hummingbot | vn.py |
|---|---|---|---|---|---|---|
| 数据源覆盖 | 部分 | 部分 | 完整 | 完整 | 部分 | 完整 |
| **数据持久化** | **缺失** | 部分 | 完整 | 完整 | 部分 | 完整 |
| 特征指标 | 部分 | 部分 | 部分 | 完整 | 部分 | 部分 |
| 参数体系 | 完整 | 完整 | 部分 | 部分 | 部分 | 完整 |
| 策略与回测 | 完整 | 部分 | 完整 | 完整 | 部分 | 完整 |
| 风控 | 完整 | 部分 | 完整 | 完整 | 部分 | 完整 |
| 执行 | 完整 | 部分 | 完整 | 完整 | 完整 | 部分 |
| 分析复盘 | 部分 | 完整 | 完整 | 完整 | 部分 | 部分 |

> 本项目在「参数体系 / 策略与回测 / 风控 / 执行」四项上已达到或超过多数同业；
> 短板集中在**一个数据问题**和**一个参数问题**上，详见第三节。

---

## 一、本项目链路测绘（逐环节带代码证据）

### 1.1 数据层 —— 覆盖广，但**一个字节都不留**

**接入的数据源**：

| 数据 | 来源 | 位置 |
|---|---|---|
| K 线 | Binance `/fapi/v1/klines`，支持 1m/3m/5m/15m/30m/1h/2h/4h/6h/8h/12h/1d 共 12 周期 | `history.rs:68-83`、`history.rs:150` |
| 资金费率 | Binance `/fapi/v1/fundingRate` | `history.rs:153` |
| 24h 行情 | `/fapi/v1/ticker/24hr` | `providers.rs:512` |
| 盘口 | **仅 bookTicker（L1 最优买卖一档）** + REST 回落 | `book_stream.rs:164`、`providers.rs:518` |
| 持仓量 | `/futures/data/openInterestHist` | `providers.rs:744` |
| 新闻 | RSS/RSSHub（币安公告、CoinDesk 等） | `news.rs:22` |

**关键缺口 —— 28 张表里没有一张行情表**：

数据库共 28 张表（`app_settings`/`positions`/`trades`/`signals`/`orders`/`equity_points`/`decision_journal`/`mantras`/`experiences`/`news_items`/`cooldowns`/`ai_call_logs`/`evolution_runs`/`parameter_changes`/`strategy_profiles`/`alert_logs`/`backup_jobs`/`write_failures` 等），
**没有任何 candles / klines / orderbook / funding 的落库表**，仓库内也没有任何 `.parquet` / `.csv` 数据文件。

K 线只在内存里有 TTL 缓存（`public.rs:334` 的 `kline_ttl_sec`，默认 20 秒）；
行情真正被保留下来的，只有三个「快照 JSON」字段：

- `signals.features` —— 信号产生瞬间的特征快照
- `positions.features` —— 持仓期间的特征快照
- `decision_journal.snapshot` —— 决策时刻的上下文快照（`journal.rs:256`）

**后果**：

1. 无法回放「当时的市场环境」，复盘只能看特征快照，看不到原始 K 线；
2. 无法做事件驱动回测与仿真（NautilusTrader 的 Parquet 目录 / vn.py 的 DataRecorder 都能做到）；
3. 无法训练自己的 ML 模型（Jesse 的 ML 流水线需要历史数据回灌）；
4. 交易所断线或限流期间没有本地行情可回补。

> 对照：Freqtrade 的 `download-data` 支持 `funding_rate`/`open_interest`/`mark`/`index`/`premiumIndex` 并落盘 feather/parquet；
> vn.py 的 DataRecorder 实时录制 Tick/K 线入库，产出的数据**直接供 CtaBacktester 回测与策略初始化**。

### 1.2 特征层 —— 只有 7 个指标，且参数写了死

`ai_analysis.rs:149-200` 的 `feature_snapshot` 产出的全部特征：

```
bars, close, ema_fast, ema_slow, rsi, atr, atr_pct, vol_ratio,
ret_1, ret_5, ret_15, funding, oi_change_pct, oi_regime,
spread_bps, quote_volume, change_24h, ema_gap_pct
```

**没有 MACD、没有布林带、没有 ADX、没有 VWAP、没有 OBV、没有随机指标** ——
对照 LEAN 内置 200+ 指标（ADX/布林/Ichimoku/ATR/Stoch 等）、
Hummingbot 通过 pandas_ta 接入 RSI/BBANDS/EMA，属于「部分」覆盖。

**更关键的问题 —— 装饰参数（详见第三节 P0-2）**：

| 参数 | 注册 | AI 可调 | 引擎计算侧 |
|---|---|---|---|
| `rsi_period` | `param_registry.rs:71` | 否 | **正确接入**（`ai_analysis.rs:450` 传入 `feature_snapshot`） |
| `atr_period` | `param_registry.rs:72` | 否 | **已全部接入**（本轮收口）：实盘 `runtime.rs:1638`、特征快照 `ai_analysis.rs:158`、回测 `backtest_engine.rs:427`、扫描 `symbol_scan.rs:751` 读的是同一份配置 |
| `ema_fast` | `param_registry.rs:69` | 否 | **已全部接入**（本轮收口）：信号内核 `trade_policy.rs:242` 与 AI 特征快照 `ai_analysis.rs:176` 共用同一份配置 |
| `ema_slow` | `param_registry.rs:70` | 否 | **已全部接入**（同上） |
| `atr_stop_mult` | `param_registry.rs:61` | 否 | **装饰**：值塞进 AI 上下文（`ai_analysis.rs:601`），引擎自己不算 ATR 止损 |

> **「AI 可调」一列全为「否」**：`tuning.rs:366-366` 的 `INTEGER_STRATEGY_KEYS`（含 `ema_fast`/`ema_slow`/`rsi_period`/`atr_period`）
> 是**排除**清单 —— 这些整数语义键不进 AI 调参目录；`risk` 组整组在 `param_registry.rs:21` 的 `AI_BLOCKED_GROUPS` 里，
> `atr_stop_mult` 自然也轮不到 AI。
> 换句话说：**你在参数页把 `ema_fast` 从 9 改成 5，UI 显示保存成功、`parameter_changes` 表留下审计记录，
> 但引擎算出来的 EMA 依然是 9/21。**
> 这与此前 `atr_stop_mult` 的病因完全同源。

**多周期也缺失**：K 线周期强制跟随策略档案的 `timeframe`（`symbol_scan.rs:327-329`），
而 `strategy_profiles` 表根本没有 `timeframe` 字段（`strategies.rs:87`），周期是全局的 —— 
**没有高周期趋势过滤、没有多周期共振**，vn.py / Freqtrade 都有 informative timeframe 机制。

### 1.3 策略层 —— 单一量化内核 + 自然语言规则注入 AI

量化信号只有一个内核：`trade_policy.rs:242` 的 `evaluate`，内部是多因子融合（`trade_policy.rs:269`）：

```
fusion: score + vol + regime + rs + break + carry + flow + conf + coverage
```

`strategy_profiles` 的实际内容是「**过滤条件 + 自然语言规则**」（`strategies.rs:87`：`name/description/rule/enabled/priority/filters`），
其中 `rule` 是给 AI 看的文字，不是可执行代码。所以**系统实际只有一套量化策略**，用参数而非多策略区分。

**AI 的权限边界是干净的**（这点做得好，值得肯定）：

```rust
// review.rs:299
/// AI 只能同意/否决/降仓：方向、杠杆、保证金、止损、止盈始终取自量化提案。
// review.rs:316-321  Agree 分支
side: proposal.side.clone(),
leverage: proposal.leverage,
stop_price: proposal.stop_price,
take_profit: proposal.take_profit,
```

**对照 LEAN**：若把 AI 放在 Alpha 段就有越权风险，LEAN 用「Risk Management 段在 Execution 前改写目标仓位」来定位；
本项目把 AI 钉在 Alpha 之后的「否决/降仓」位子上，**架构位置与 LEAN 一致，且实现更严格**。

### 1.4 参数层 —— 这是本项目最强的一环

**127 个注册参数**，分 7 组（`engine` / `strategy` / `risk` / `ai` / `ai_tuning` / `data` / `news`）。

我逐个参数做了全仓库消费扫描（排除 `param_registry.rs` 与 `default_settings.json`），
**127 个全部被业务代码真实读取，0 个完全无人认领** —— 参数中心不是摆设，这是实打实的优点。

配套能力也齐：

- 参数修改全量审计（`settings_audit` + `parameter_changes`，`settings_api.rs:882`）；
- AI 可调参数目录与上下界（`tuning.rs`，含整数语义键 `tuning.rs:366-366`）；
- 参数组热更新、审计查询、分组重置（`settings_api.rs:21-31`）；
- 参数集（`ai_tuning_sets`）的激活/回滚（`ai_tuning_api.rs:21-25`）。

**对照**：Freqtrade 的 hyperopt（optuna NSGAIII + 多 space + 多 loss）、vn.py 的参数优化（穷举+遗传）、
LEAN 目前公开的只有 Grid Search —— 本项目的参数工程能力**在加密圈里属于第一梯队**。

### 1.5 回测层 —— 同样强，但有一处口径分叉

| 能力 | 本项目 |
|---|---|
| 网格搜索 | `/backtest/grid` |
| 走步验证 walk-forward | `/backtest/walkforward` |
| 样本外 OOS | `backtest_engine.rs:1389` 复合回撤 |
| 压力测试 | `backtest_engine.rs:1025` `stressed_expectancy` |
| 绩效指标 | win_rate / profit_factor / expectancy / max_drawdown_pct（`backtest_engine.rs:565-629`） |
| 成本模型 | 手续费 + 滑点 + 资金费结算 + 保证金 |

**分叉点（已收口）**：此前 `backtest_engine.rs` 的 `candle_metrics(slice, 14)` 把 ATR 周期写死，
而实盘扫描路径用的是参数 `atr_period`（`runtime.rs:1638`）——
**改 `atr_period` 时，实盘变、回测不变**，正是「回测/实盘口径分叉」教训的重演路径。
本轮已把 ATR 周期收成单一来源，见 `docs/FIX_VERIFICATION_2026-11-27.md` 第三节。

### 1.6 风控层 —— 事前闸门真实存在，这点领先多数同类

`risk.rs:81-123` 的 `global_gates`（账户级事前闸门）：

| 闸门 | 行为 |
|---|---|
| 权益下限 | `risk.rs:85` 拒绝开仓 |
| 日内亏损限额 | `risk.rs:93` **触发熔断并平仓** |
| 最大回撤 | `risk.rs:103` **触发熔断并平仓** |
| 连亏冷却 | `risk.rs:108` 拦本轮 |
| 交易时段 | `risk.rs:229` `trading_window_open` |

`runtime.rs:3990-4016` 的组合敞口闸门（开仓前拦截）：

```rust
let correlation_cap = cfg["correlation_limit"].as_i64().unwrap_or(2);
if correlation_cap > 0 && correlated >= correlation_cap { ... }   // 高相关同向持仓上限
if exposure_pct > max_exposure_pct { ... }                        // 总敞口上限
let direction_cap = cfg["direction_exposure_limit_pct"]...         // 单方向敞口
```

并且有对应测试（`runtime.rs:9988` `correlation_limit_rejects_second_major`、
`runtime.rs:10022` `direction_exposure_limit_rejects_oversized_side`）—— **是真闸门，不是摆设**。

**对照**：Freqtrade 的 Protections 是事后锁交易对/全局冷却，且回测默认不启用；
Hummingbot 没有集中式事前风控引擎；vn.py 有 5 条内置事前规则且插件化。
**本项目这一环不输任何一家**。

### 1.7 执行与持仓管理

- **交易所侧保护单**：`live.rs:508-509` 挂 `STOP_MARKET` + `TAKE_PROFIT_MARKET`，
  Algo 接口优先、明确拒绝才回落老接口（`live.rs:415-421`）；
- **对账**：`/system/exec-core/reconcile`（`system.rs:285`）+ `/system/exec-core/self-test`；
- **死信重放**：`/system/dead-letters/{id}/replay`（`system.rs:275`）—— 这是很多同类项目都没有的运维面；
- **双层持仓管理**：0.5 秒快平任务（`fast_risk.rs:46` 的 `Duration::from_millis(500)`）
  + 480 秒周期任务（`manage.rs`）。

### 1.8 分析复盘层 —— 审计轨迹最厚，但缺风险调整指标

**有的**：

- 信号级审计：`signals` 表带 `ai_reason` / `reject_reason` / `decision`（`signal_audit.rs:184`）；
- 决策级审计：`decision_journal` 带 `snapshot` / `decision` / `outcome` / `actual_pct`（`journal.rs:256`），
  有完整的标注（label）闭环；
- 归因：`exit_reason` 按符号聚合（`engine.rs:684`）；
- 权益曲线：`equity_points` 每周期采样（`runtime.rs:1619`）；
- 演化：`mantras` / `experiences` / `evolution_runs` + 有效性评估（`/evolution/effectiveness`）。

**没有的**：

**Sharpe / Sortino / Calmar 在后端完全不存在** —— 全仓库检索 `sharpe|sortino` 只命中前端设计稿里的静态占位数字，
其中 `frontend/src/views/LoginView.vue:52` 直接把 **「2.41」硬编码成 Sharpe 显示在登录页**。
回测输出的是 win_rate / profit_factor / expectancy / max_drawdown_pct，**没有任何风险调整后收益指标**。

> 对照：Hummingbot 的回测面板自带 Sharpe / Profit Factor；Backtrader 内置 SharpeRatio Analyzer；
> vn.py 回测统计图含夏普。**这是本项目分析层最容易被外部发现的一条缺口。**

---

## 二、同业横向对比（证据来自官方文档/官方仓库）

**已核实版本**：Freqtrade `2026.10` · NautilusTrader `v1.231.0` · Hummingbot `v2.18.0` · vn.py `4.5.0` · LEAN 文档站 v2。
Jesse / OctoBot / Backtrader / Zipline 版本号未验证。

| 系统 | 数据源与持久化 | 特征指标 | 参数体系 | 策略与回测 | 风控 | 执行 | 分析复盘 |
|---|---|---|---|---|---|---|---|
| **本项目** | 部分 / **缺失持久化** | 部分 | 完整 | 完整 | 完整 | 完整 | 部分 |
| Freqtrade | 部分 | 部分 | 完整 | 部分 | 部分 | 部分 | 完整 |
| NautilusTrader | 完整 | 部分 | 部分 | 完整 | 完整 | 完整 | 完整 |
| QuantConnect LEAN | 完整 | 完整 | 部分 | 完整 | 完整 | 完整 | 完整 |
| Hummingbot | 部分 | 部分 | 部分 | 部分 | 部分 | 完整 | 部分 |
| vn.py / VeighNa | 完整 | 部分 | 完整 | 完整 | 完整 | 部分 | 部分 |
| Zipline / Backtrader | 部分 | 部分 | 部分 | 完整 | 缺失 | 部分 | 完整 |
| Jesse / OctoBot | 部分 | 部分 | 完整 | 部分 | 部分 | 部分 | 完整 |

### 各系统关键事实（附官方来源）

**Freqtrade**

- `download-data` 的 `--candle-types` 含 `funding_rate`/`open_interest`/`mark`/`index`/`premiumIndex` —— https://www.freqtrade.io/en/stable/data-download/
- 回测明确「所有订单按请求价成交，**无滑点模型**」 —— https://www.freqtrade.io/en/stable/backtesting/
- hyperopt 用 optuna 的 NSGAIIISampler + 多内置 loss —— https://www.freqtrade.io/en/stable/hyperopt/
- 独立的 `lookahead-analysis` 与 `recursive-analysis` 偏差体检 —— https://www.freqtrade.io/en/stable/lookahead-analysis/
- Protections 仅 4 条，事后锁单/冷却，回测需 `--enable-protections` —— https://raw.githubusercontent.com/freqtrade/freqtrade/develop/docs/includes/protections.md

**NautilusTrader**

- `ParquetDataCatalog` + Arrow schema + 纳秒时间戳，streaming 走 Feather —— https://nautilustrader.io/docs/latest/concepts/data/catalog/
- 行为模型四族可插拔：Fill / Fee / Margin / Latency —— https://nautilustrader.io/docs/latest/concepts/behavioral_models/
- 回测与实盘**共用同一策略与执行代码** —— https://nautilustrader.io/docs/latest/
- 资金费在 funding 边界以 `FundingSettlement` 结算 —— https://nautilustrader.io/docs/latest/concepts/backtesting/accounts-and-margin/
- `RiskEngine` 事前风控 —— https://nautilustrader.io/docs/rust-api-nightly/nautilus_risk/engine/struct.RiskEngine.html
- 订单对账报告模型 `OrderStatusReport`/`FillReport`/`PositionStatusReport` —— https://nautilustrader.io/docs/latest/concepts/execution/reconciliation/

**QuantConnect LEAN**

- Algorithm Framework 五段式：Universe → Alpha → Portfolio → Risk → Execution —— https://www.quantconnect.com/docs/v2/writing-algorithms/algorithm-framework/overview
- Risk Management 在 Execution **之前改写 PortfolioTarget**（事前拦截） —— https://www.quantconnect.com/docs/v2/writing-algorithms/algorithm-framework/risk-management/key-concepts
- 内置 `MaximumDrawdownPercentPerSecurity`/`MaximumSectorExposure`/`FlashCrashDetection` —— https://www.quantconnect.com/docs/v2/writing-algorithms/algorithm-framework/risk-management/supported-models
- `Insight` 对象即信号级审计记录（方向/幅度/置信度/有效期） —— https://www.quantconnect.com/docs/v2/writing-algorithms/algorithm-framework/alpha/key-concepts
- 优化目前仅 Grid Search —— https://www.quantconnect.com/docs/v2/cloud-platform/optimization/strategies
- 数据为 flat files + 标准目录树 —— https://quantconnect.com/docs/v2/lean-cli/datasets/format-and-storage

**Hummingbot**

- V2：Controller → ExecutorAction → Executor —— https://www.hummingbot.org/strategies/v2-strategies/
- Candles 文档明确用 pandas_ta 追加指标 —— https://hummingbot.org/strategies/v2-strategies/candles/
- PositionExecutor 三重屏障（stop_loss / take_profit / time_limit / trailing_stop） —— https://www.hummingbot.org/strategies/v2-strategies/executors/positionexecutor/
- 回测面板指标集含 Sharpe / Profit Factor / 交易成本% —— https://hummingbot.org/dashboard/backtest/
- Quants Lab 官方标注「不再积极维护」 —— https://hummingbot.org/quants-lab/

**vn.py / VeighNa**

- DataRecorder 实时录制 Tick/K 线入库，**产出数据可直接供 CtaBacktester 回测与策略初始化** —— https://raw.githubusercontent.com/vnpy/vnpy/master/docs/community/app/data_recorder.md
- RiskManager 事前风控插件 + RuleTemplate 架构，5 条内置规则 —— https://raw.githubusercontent.com/vnpy/vnpy/master/docs/community/app/risk_manager.md
- CtaBacktester 显式建模滑点/百分比手续费/固定比手续费/合约乘数/价格跳动 —— https://raw.githubusercontent.com/vnpy/vnpy/master/docs/community/app/cta_backtester.md

**未验证清单**：Freqtrade 的 L2 深度持久化、LEAN 的 walk-forward/敏感性分析、Hummingbot 的内置优化器与行情持久化介质、
NautilusTrader 的交易所侧原生保护单（其 `OrderEmulator` 是本地触发转换，非交易所挂单）、vn.py 内置指标数量、Zipline bundle 格式。

---

## 三、缺口清单（按优先级）

> **状态（2026-11-27）：** `P0-1` / `P0-2` / `P0-3` / `P1-1` / `P1-2` / `P1-3` 以及 `P2` 中的对账事件化、Insight 对象化、退出原因枚举三项**已全部关闭**；
> 逐项提交、验证与偏差说明见第四节表格及对应任务文档。`P2` 的数据源扩展（社交情绪 / 链上）仍未立项。

### P0-1 行情零持久化 —— 分析与训练的底座缺失 ✅ 已关闭（`[M1]`/`[M2]`）

**证据**：28 张表无行情表；仓库无 parquet/csv；K 线仅内存 TTL 缓存（`public.rs:334`）。

**同业做法**：vn.py DataRecorder 录完直接进回测；NautilusTrader Parquet 目录 + consolidate 校验 gap；
LEAN flat files 标准目录树。

**建议**：新增 `market_candles(symbol, interval, open_time, ohlcv, volume, ...)` 主键表 + `market_book` 快照表，
写一条独立的录制任务（按周期落库，保留期由 `data.keep_days` 控制）。
**这一步做完，「复盘只能看快照、无法回放」「无法自训模型」「断线无历史可回补」三个问题同时解决。**

### P0-2 装饰参数：注册了、可改了，但计算侧写死 ✅ 已关闭（`[P1]`）

**证据**（全部 ripgrep 复核）：

```rust
// ai_analysis.rs:176-177  ← 已接入配置：ema_periods 来自 strategy，不再是字面量
"ema_fast": ema(&closes, ema_periods.fast).map(round4),
"ema_slow": ema(&closes, ema_periods.slow).map(round4),

```

而 `ema_fast`/`ema_slow` 在 `param_registry.rs:69-70` 注册、在 `param_registry.rs:703` 断言 `ai_writable` 为真，
却又被 `tuning.rs:366-366` 的 `INTEGER_STRATEGY_KEYS` **排除**出 AI 调参目录 ——
**参数治理做得越完整，这个空转越隐蔽**。

> **本轮已收口**：ATR 周期与 EMA 周期的写死问题全部修完 —— 信号内核、regime 判定、AI 特征快照、

**这是继 `atr_stop_mult` 之后的同一类病，且影响面更大**：`atr_stop_mult` 只是没人算它，
而 `ema_fast`/`ema_slow` 是**信号主因**（`trade_policy` 的 score 与 `ema_gap_pct` 直接吃这两个值）。

**建议（已在本轮全部落地）**：

1. `feature_snapshot` 增参 `ema_fast`/`ema_slow`，从 `strategy` 配置读（照 `rsi_period_of` 的写法）✅；
2. 信号内核 `trade_policy` 同步接入 EMA 周期（含 `regime_for` / `preview` 两条旁路）✅；
3. **加一条防回归断言**：遍历 param_registry 的 strategy 组整数参数，断言每个都在计算路径出现且不是字面量 ⬜（仍建议补）。

> **本轮已全部收口**：ATR 周期 `atr_period` 与 EMA 周期 `ema_fast`/`ema_slow` 的写死问题都修完了 ——
> 实盘扫描、AI 特征快照、信号内核、regime 判定、回测、证据门、标的扫描现在读同一份配置，
> 范围取自 `param_registry`；落地细节与验证见 `docs/FIX_VERIFICATION_2026-11-27.md` 第三、四节。
> 唯一残留的装饰参数是 `atr_stop_mult`：它只作为 AI 上下文里的提示值存在，引擎不据它计算止损价
> （引擎用的是 `stop_distance_atr_mult`）。
>
> 另有 **ADX 周期**在本轮也完成参数化（`strategy.adx_period`，默认 14，范围 2–100），
> 但它带来一条**未声明的耦合**：ADX 需要 `可用 K 线数 > adx_period`，否则内核直接不出信号 ——
> 详见 `docs/FIX_VERIFICATION_2026-11-27.md` 第 4.3.2 节。

### P0-3 后端无风险调整后收益指标，前端还展示着假 Sharpe ✅ 已关闭（`[R1]`）

**证据**：全仓库 `sharpe|sortino` 仅命中前端静态稿；`frontend/src/views/LoginView.vue:52` 把 `2.41` 写死显示为 Sharpe。

**建议**：后端补 Sharpe / Sortino / Calmar（用 `equity_points` 的周期收益率即可算），
回测 `summary` 同步输出；同时把登录页的硬编码 Sharpe 换成真实值或去掉。

### P1-1 指标体系过薄 + 无多周期 ✅ 已关闭（`[I1]`/`[I2]`/`[I3]`）

仅 7 个指标、无 MACD/BB/ADX/VWAP、且周期全局单一。
**建议**：先补 3–5 个高价值指标（MACD、布林、ADX、VWAP、ATR 通道），
再引入「高周期趋势过滤」作为开仓前置过滤（可复用现有 `regime` 模块）。

### P1-2 参数稳健性没有护栏 ✅ 已关闭（`[J1]`/`[J2]`/`[J3]`）

**证据**：有 grid / walkforward / OOS / stressed_expectancy，但**没有 lookahead 检测、没有递归指标方差检测、没有训练/测试期强制分离、没有 Monte Carlo**。

**同业**：Freqtrade 有 `lookahead-analysis` 与 `recursive-analysis` 两条独立体检命令；
Jesse 强制训练/测试期分离并做 Monte Carlo 与显著性检验。

**建议**：把这两条体检命令的实现移植过来，作为参数激活流程的**强制关卡**
（现在 `/ai-tuning/{id}/activate` 只需要证据门槛，没有偏差检测）。

### P1-3 回测行为模型未插件化 ✅ 已关闭（`[K1]`/`[K2]`）

NautilusTrader 把 Fill/Fee/Margin/Latency 做成可插拔接口，LEAN/vn.py 把成本模型逐项暴露为配置字段。
本系统的手续费/滑点/资金费写死在回测引擎里，改一次要动代码。
**建议**：抽成 `BacktestBehaviorConfig`，与 `ManageConfig` 同级暴露为参数。

### P2 其他（对账 / Insight / 退出原因 ✅ 已关闭；数据源扩展待立项）

| 项 | 说明 |
|---|---|
| 订单对账事件化 | NautilusTrader 的 `OrderStatusReport`/`FillReport`/`PositionStatusReport` 三类报告模型值得照搬 |
| 数据源扩展 | 社交情绪（OctoBot 接 Google Trends/Reddit）、链上数据目前完全空白 |
| 保护单语义 | 参照 Hummingbot 的三重屏障，把止损/止盈/时间退出统一为「退出原因」枚举，便于归因 |

---

## 四、值得直接借鉴的 8 条（按投入产出排序）

1. **行情落库 + 录制即回测**（vn.py DataRecorder）—— 一举解决 P0-1 的三个衍生问题 |
2. **接回 `ema_fast`/`ema_slow`/`atr_period` 并加防回归断言**（P0-2）—— 改动小、影响大 |
3. **后端补 Sharpe/Sortino/Calmar + 去掉登录页假 Sharpe**（P0-3）—— 半天工作量，直接补齐分析层 |
4. **移植 lookahead-analysis / recursive-analysis 作为参数激活关卡**（P1-2）—— 防过拟合 |
5. **回测行为模型插件化**（P1-3）—— 对齐 NautilusTrader |
6. **订单对账报告模型事件化** —— 把部分成交/状态漂移从异常分支变为一等公民事件 |
7. **Insight 对象化**：把每次决策落成「方向/幅度/置信度/有效期/被谁否决」结构（LEAN 思路），
   与现有 `decision_journal` 合并，形成真正的信号级+决策级双层审计 |
8. **多周期趋势过滤 + 补 3–5 个指标**（P1-1）—— 提升信号质量但工作量大，排后

### 已落成并完成的任务文档（2026-11-27）

| 对标条目 | 任务文档 | 状态 |
|---|---|---|
| 第 1 条 行情落库 + 录制即回测 | `docs/superpowers/plans/2026-11-27-chain-completion.md`（`[M1]`/`[M2]`） | ✅ 已完成 · `a26700b` / `5581366` |
| 第 2 条 接回周期参数 + 防回归断言 | `docs/superpowers/plans/2026-11-27-chain-completion.md`（`[P1]`） | ✅ 已完成 · `0699c74` |
| 第 3 条 Sharpe/Sortino/Calmar + 去掉假 Sharpe | `docs/superpowers/plans/2026-11-27-chain-completion.md`（`[R1]`） | ✅ 已完成 · `8ce05e0` |
| 第 4 条 lookahead / recursive 护栏 | `docs/superpowers/plans/2026-11-27-overfit-guard.md`（`[J1]`/`[J2]`/`[J3]`） | ✅ 已完成 · `9c4713d` / `94fa91c` / `64194b7` |
| 第 5 条 回测行为模型插件化 | `docs/superpowers/plans/2026-11-27-backtest-behavior-model.md`（`[K1]`/`[K2]`） | ✅ 已完成 · `9044caa` / `86dda7c` |
| 第 6 条 订单对账报告模型事件化 | `docs/superpowers/plans/2026-11-27-audit-insight-model.md`（`[L3]`） | ✅ 已完成 · `afb6105` |
| 第 7 条 Insight 对象化 | `docs/superpowers/plans/2026-11-27-audit-insight-model.md`（`[L2]`） | ✅ 已完成 · `e35b1b7` |
| 保护单语义 · 退出原因统一枚举 | `docs/superpowers/plans/2026-11-27-audit-insight-model.md`（`[L1]`） | ✅ 已完成 · `7bd649c` |
| 第 8 条 多周期趋势过滤 + 补指标 | `docs/superpowers/plans/2026-11-27-indicator-expansion.md`（`[I1]`/`[I2]`/`[I3]`） | ✅ 已完成 · `f07893e` / `d873abb` / `b25f5f7` |

> 验收口径：Rust **974** 个测试全绿，`cargo fmt --check` 与 `clippy -D warnings` 通过；
> 前端 **81** 个测试全绿，`vue-tsc --noEmit` 与 `npm run build` 通过。
> 计划与实现的两处主动偏差：`[I3]` 高周期 regime 由本周期 K 线聚合（不额外取数）；`[K2]` 行为模型用 `Arc` 而非 `Box` 以保留 `EngineConfig: Clone`。

---

## 五、直接回答两个问题

### Q1：本项目的链路是否完整？

**结构完整，内容有缺口。**

- **完整**（达到或超过同业）：参数体系（127 参数全消费 + 审计 + AI 调参 + 激活回滚）、
  风控（事前账户级 + 组合敞口双闸门，有测试）、执行（交易所侧保护腿 + 对账 + 死信重放）、
  回测（网格 + 走步 + OOS + 压力测试）、审计轨迹厚度（信号级 + 决策级 + 演化 + 归因）；
- **有缺口**：数据持久化（零留存）、指标体系（7 个、单周期）、风险调整指标（无 Sharpe）、
  参数稳健性体检（无 lookahead/recursive/Monte Carlo）、回测实盘口径（ATR 周期分叉）。

### Q2：是否能读取所有数据和参数进行交易与分析？

**不能，分三个层次答：**

| 问题 | 答案 |
|---|---|
| 交易时能读到所有**数据**吗？ | **基本能** —— 行情/资金费/OI/盘口L1/新闻都接进来了；但**只有 7 个特征**，深度与订单流数据缺失 |
| 交易时能读到所有**参数**吗？ | **不能** —— `ema_fast`/`ema_slow`/`atr_period` 在计算侧被写死，改了不生效 |
| 分析时能读到所有**数据**吗？ | **不能** —— 只有决策时刻的特征快照，**没有历史行情可回放**，无法复现当时盘口、无法做事件回测与自训模型 |

**一句话**：参数治理做得比多数同业扎实，但因为行情不留存、且三个核心指标参数在计算侧空转，
「参数 → 信号 → 交易 → 分析 → 再调参」这个闭环**目前是断在两头**：
**上游的行情留不住，下游的参数改不动。**

---

## 附：本次核查方法与可复现口径

- 本地行号一律用 ripgrep 复核 `HEAD=d1952d0` 的实际文件内容；
  **注意 PowerShell `Get-Content` 在这些 UTF-8 源文件上会合并行显示，导致行号整体偏移，不可用于核对行号**；
- 参数消费扫描：对 `param_registry.rs` 注册的每个参数名，在 `rust/nanoquant/src` 下做 `git grep -l -F`，
  排除 `param_registry.rs` 与 `default_settings.json`；
- 表清单：对全仓库 `CREATE TABLE` 模式去重枚举，得到 28 张表；
- 同业结论：2026-11-27 实际 `web_fetch` 官方文档与官方仓库 raw 原文，版本已标注；
  未获官方证据的一律标「未验证」，未做推断。
