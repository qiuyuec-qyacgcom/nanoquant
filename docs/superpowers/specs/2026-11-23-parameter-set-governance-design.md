# AI 参数集治理与 Regime 自适应设计

- 状态：已批准设计（2026-11-23）
- 范围：AI 调参解耦、回测证据门、regime 参数集、自动切换与回滚
- 关联文档：
  - `docs/superpowers/specs/2026-11-21-ai-param-tuning-design.md`
  - `docs/superpowers/plans/2026-11-21-ai-param-tuning-followups.md`
  - `docs/superpowers/specs/2026-11-23-exec-core-design.md`
- 实现计划：
  - A 期：`docs/superpowers/plans/2026-11-23-parameter-sets-a-decouple-tuning.md`
  - B 期：`docs/superpowers/plans/2026-11-23-parameter-sets-b-backtest-evidence.md`
  - C 期：`docs/superpowers/plans/2026-11-23-parameter-sets-c-regime-switching.md`

## 1. 背景

2026-11-23 线上实盘出现连续近一天零开仓：

- 最近 24 小时约 1200 次扫描、1159 条拒绝、0 开仓、0 成交。
- 最近 500 条信号中 493 条在行情闸门被拒，只有 7 条进入 AI 复核。
- 最近 500 条中 458 条原因是「波动率过低 < 1.5%」，27 条「量能不足 < 1.1」，8 条「波动率过高 > 8%」。
- BTC 1h ATR 0.69%、ETH 0.82%、SOL 0.83%，全部低于 `atr_pct_min=1.5`；EVAA ATR 8.3%，高于 `atr_pct_max=8`。
- 唯一进入 AI 复核的 EVAA 在 7 次复核中全部 `hold`，置信度 0.62–0.72。
- `parameter_changes` 显示 AI 在一天内连续收紧：
  - `atr_pct_min`: 0.5 → 0.8 → 1.0 → 1.2 → 1.5
  - `volume_ratio_min`: 0.6 → 0.8 → 1.0 → 1.1
  - `atr_pct_max`: 15 → 10 → 8
- `ai.allow_param_tuning=true`、`ai.param_tuning_min_interval_sec=0`，没有冷却；每次变更 `effect` 的 `trades=0/pnl=0`，没有实盘样本验证。
- 08:35 最后一次收紧后，没有任何标的能进入 AI 复核，AI 不再被调用，参数无法自动恢复，系统进入自锁状态。

根因不是引擎或 AI 调用故障，而是：

1. 调参发生在每轮开仓复核内部，AI 同时决定「不开仓」和「调高不开仓门槛」。
2. 没有样本证据门，单只标的的局部特征被用来收紧全局参数。
3. 没有 regime 概念，同一套参数同时服务低波动、趋势和高波动市场。
4. 没有冷却、没有自动回滚、没有候选数下限保护。

## 2. 目标与非目标

### 目标

- 把 AI 调参与开仓复核解耦，调参不再发生在每轮扫描里。
- 调参候选必须通过扩展后的 walk-forward 回测证据门，才允许进入自动启用流程。
- 按 `trend / range / chaos` 三套 regime 参数集运行；低波动市场不再被高波动参数锁死。
- 参数集版本化、可审计、可回滚；自动切换有滚动 24 小时限速和 30 分钟无候选回滚。
- C 期上线时以人工批准的 bootstrap 低波动参数集解除当前锁死。

### 非目标

- 不把 `risk` 组（杠杆、单笔风险、仓位）纳入 regime 参数集；风险参数继续全局生效。
- 不做影子账户或实时影子成交；证据门只使用历史回测。
- 不新增均值回归等新策略模式；本次只治理现有 fusion 策略的参数。
- 不让 AI 调整 regime 判定阈值；本次沿用现有硬编码阈值，只暴露 regime 结果。
- 不在 A/B 期提前修改线上策略参数；C 期上线时才启用 bootstrap 参数集。

## 3. 核心概念

### 参数集（Parameter Set）

一套完整的 `strategy` JSON 快照，带版本、regime、来源、状态和回测证据。

- 只包含 `strategy` 组，不包含 `risk`、`engine`、`ai` 等组。
- 运行时物化到 `app_settings.strategy`；交易引擎仍从 `app_settings.strategy` 读取参数，不感知参数集表。
- 状态：
  - `candidate`：AI 生成，等待回测证据。
  - `approved`：证据门通过，等待启用。
  - `active`：当前某个 regime 的有效参数集；每个 regime 同时只有一个。
  - `retired`：曾被启用、后来被替换。
  - `rejected`：证据门失败或参数校验失败。
- 来源：
  - `bootstrap`：部署时人工批准的初始参数集。
  - `ai`：独立调参任务生成。
  - `manual`：用户在后台手工创建或复制。

### Regime

复用 `trade_policy::core` 的现有判定：

- `trend`：`adx >= 25` 且 `|gap_pct| >= 0.15`
- `chaos`：`atr_pct >= 2.0` 或 `volume_ratio >= 2.5`
- `range`：其余情况

阈值本次保持硬编码，不允许 AI 修改。

- `symbol_regime`：单个标的的 regime，由 `trade_policy::core` 计算，用于诊断。
- `market_regime`：当前启用标的池的等权多数 regime；至少 6 个有效标的才有结果，平票取 `range`，有效标的不足 6 个时保持上一个 `market_regime` 并告警。
- 参数集选择、regime 切换、无候选回滚守卫都使用 `market_regime`；`symbol_regime` 只做展示和归因。
- `market_regime` 与 `symbol_regime` 都写入信号、持仓、成交的 `features`，并在后台展示。

### Timeframe 口径

本文所有 ATR%、volume_ratio、ADX、regime 阈值和证据门区间都以 `strategy.timeframe` 为周期口径：

- 线上事故期间 `strategy.timeframe=1h`、`engine.cycle_interval_sec=600`；仓库默认是 `4h`，两者不同，不能把 1h 的绝对阈值直接搬到 4h。
- 事故中的 ATR 数值、`atr_pct_min=1.5`、`atr_pct_max=8`、`chaos.atr_pct>=2.0` 都是 1h 口径。
- `parameter_sets` 增加 `timeframe` 列；候选必须携带生成时的 timeframe，激活时必须等于当前 `strategy.timeframe`，否则拒绝自动切换。
- 证据门回测周期、K 线数量、ATR 硬边界、regime chaos 阈值都随 `strategy.timeframe` 解释；JSON evidence 的 `interval` 不再硬编码 1h。
- v1 只为 `1h` 提供校准过的硬边界与自动切换；若线上切到 4h 或其他周期，自动切换必须关闭（`auto_apply=false`）并重新校准，人工切换需二次确认。

### 证据门

对候选参数集执行固定参数集的 walk-forward 回测，只有满足全部阈值才允许 `candidate → approved`：

| 条件 | 阈值 |
|---|---|
| timeframe 匹配 | 候选 timeframe 必须等于当前 `strategy.timeframe`；v1 自动切换只接受 `1h` |
| 数据量 | 每个参与标的至少 2000 根当前 timeframe K 线（线上 1h） |
| 覆盖标的 | 至少 6 个有效标的 |
| 折数 / 训练长度 | 3 折，训练 800 根 |
| 目标 regime 的样本外成交数 | ≥ 30（按候选 `market_regime` 过滤） |
| 盈利折数 | ≥ 2 / 3 |
| 样本外平均单笔收益 | > 0（等风险权重，单位 %） |
| 过拟合差距 | ≤ 1.0pp（训练集与样本外平均单笔收益之差） |
| Bootstrap 置信 | `ci_low > 0` 且 `p_positive >= 0.90`（样本外交易收益） |
| 2 倍手续费/滑点压力后平均单笔收益 | ≥ 0（单位 %） |
| 样本外最大回撤 | ≤ 15%（等风险累计收益序列） |
| 相对同 regime 的当前 active 集 | 样本外平均单笔收益至少高 0.10pp |
| 参数改动范围 | 最多 3 个键，单键相对变化 ≤ 25% |
| 参数硬边界（1h profile） | `atr_pct_min ≤ 1.0`、`atr_pct_max ≥ 6`、`volume_ratio_min ≤ 1.0`、`entry_threshold ≤ 0.5` |

`bootstrap` 和 `manual` 参数集不要求证据门；`ai` 来源必须通过。硬边界对 `bootstrap`/`manual` 同样生效，免证据门不等于免边界。

### Bootstrap 参数集

A 期迁移时写入两套 active 参数集（只建记录，不物化到 `strategy`；C 期才启用选择）：

- `range-bootstrap`
  - 基于当前 `strategy` 完整快照，`timeframe='1h'`
  - 覆盖：`atr_pct_min=0.6`、`atr_pct_max=10`、`volume_ratio_min=0.8`、`entry_threshold=0.45`
- `chaos-bootstrap`
  - 基于当前 `strategy` 完整快照，`timeframe='1h'`
  - 取硬边界内的 chaos 参数：`atr_pct_min=1.0`、`atr_pct_max=8`、`volume_ratio_min=1.0`、`entry_threshold=0.5`
- `chaos-live-2026-11-23`（只做历史审计，`status='retired'`）
  - 保存事故当天的线上值：`1.5 / 8 / 1.1 / 0.55`
  - 不参与自动切换；需要人工确认才能启用

`trend` 初始没有独立参数集，回退到 `range`，并发出告警。

## 4. 架构

### 组件

1. **`parameter_sets` 注册表**
   - 新模块 `src/parameter_sets.rs`。
   - 负责参数集查询、候选校验、状态迁移、激活、物化、回滚、证据读写。
   - 不包含 AI 调用和回测执行。

2. **独立调参任务**
   - 新模块 `src/tuning.rs`。
   - 按 `ai.param_tuning_min_interval_sec`（默认 86400 秒）运行，受 `ai.allow_param_tuning` 控制。
   - 生成一个 `candidate` 参数集，绝不直接写 `app_settings.strategy`。

3. **回测证据门**
   - 扩展 `src/backtest_engine.rs`：固定参数集 walk-forward、ATR/量能/阈值参数、证据摘要。
   - 新函数 `run_walkforward_basket_fixed` 与 `evidence_for_candidate`。
   - 由调参任务在 `spawn_blocking` 中执行，避免阻塞运行时。

4. **Regime 选择器**
   - 复用 `trade_policy::core` 的 regime 判定，新增纯函数 `classify_regime`。
   - 新模块 `src/regime.rs` 或在 `trade_policy.rs` 中暴露。
   - 负责确认窗口、冷却、active 集选择、切换事务、无候选回滚。

5. **运行时**
   - `runtime.rs` 每轮记录 regime、参数集 id、候选数。
   - A 期移除 `apply_decision_params` 调用；B/C 期不改交易引擎读取参数的方式。

6. **审计与 UI**
   - `parameter_sets`、`parameter_set_activations`、`parameter_changes`、`engine_events`。
   - 新后台页面「参数集」：当前 regime、active 集、候选/已通过集、证据、启用/回滚历史。

### 数据流

```
每轮扫描
  └─ market_regime 判定 ──> signals.features.market_regime / symbol_regime / parameter_set_id
  └─ gate 候选数 ──> 无候选回滚守卫

每 24h 调参任务
  └─ AI 生成参数增量 ──> 校验 ──> parameter_sets(candidate)
                                      │
                                      ▼
run_walkforward_basket_fixed
                                      │
                        证据门 pass ──> approved
                                      │
                     auto_apply=true ──> active
                                      │
                   当前 regime 匹配 ──> app_settings.strategy
                                      │
                   30 分钟无候选（实跑轮次） ──> 回滚上一套 active
```

### 边界

- `risk` 不进入参数集；AI 不能改风险参数。
- AI 不能改 `parameter_set` 配置组、`engine` 组、`ai` 组。
- 参数集切换只写 `strategy`，不影响 `engine`、`risk`、`ai` 配置。
- 旧交易和信号没有参数集 id 时留空，不做回填。

## 5. 数据模型

### 5.1 `parameter_sets`

```sql
CREATE TABLE parameter_sets (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL,
  regime TEXT NOT NULL,
  timeframe TEXT NOT NULL,
  version INTEGER NOT NULL,
  params TEXT NOT NULL,
  status TEXT NOT NULL,
  source TEXT NOT NULL,
  evidence TEXT NOT NULL DEFAULT '{}',
  parent_id INTEGER,
  created_at INTEGER NOT NULL,
  activated_at INTEGER
);

CREATE UNIQUE INDEX ix_parameter_sets_active_regime
  ON parameter_sets(regime) WHERE status = 'active';

CREATE INDEX ix_parameter_sets_regime_status
  ON parameter_sets(regime, status);
```

字段约束：

- `regime IN ('trend','range','chaos')`
- `timeframe` 必须与 `params.strategy.timeframe` 一致；激活时必须等于当前 `strategy.timeframe`
- `status IN ('candidate','approved','active','retired','rejected')`
- `source IN ('bootstrap','ai','manual')`
- `params` 是完整 strategy JSON 对象，不是增量。
- `evidence` 是证据摘要 JSON；`bootstrap`/`manual` 可为 `{"kind":"bootstrap"}` 或 `{"kind":"manual"}`。
- 与既有 `strategy_profiles` 表区分：`strategy_profiles` 是给 AI 的文本规则/策略画像；`parameter_sets` 是数值参数快照。

### 5.2 `parameter_set_activations`

```sql
CREATE TABLE parameter_set_activations (
  id INTEGER PRIMARY KEY,
  set_id INTEGER NOT NULL,
  regime TEXT NOT NULL,
  previous_set_id INTEGER,
  ts INTEGER NOT NULL,
  trigger TEXT NOT NULL
);

CREATE INDEX ix_parameter_set_activations_ts
  ON parameter_set_activations(ts);
```

`trigger IN ('bootstrap','manual','regime','auto_apply','no_candidate','rollback')`。

### 5.3 `parameter_changes` 扩展

新增列：

- `set_id INTEGER`（可空）
- `regime TEXT`（可空）

原有列保持不变。参数集切换时，按实际变化的 strategy 键逐行写入，必须补齐现有 NOT NULL 字段：

- `group_name='strategy'`
- `key=<变化的键>`
- `old_value` / `new_value`
- `reason` 为参数集名称与触发原因
- `decision_id=0`（参数集切换没有 AI 开仓决策 id）
- `mode=<当前引擎模式 live/demo>`
- `set_id` / `regime`
- `source='parameter_set'`（自动/手动激活）或 `'parameter_set_rollback'`（参数集回滚）

既有单键回滚继续使用 `source='rollback'`，与参数集回滚区分。没有实际键变化时只写激活记录和事件，不写 `parameter_changes`。

### 5.4 `trades` 与 `signals`

- `trades` 增加可空列 `parameter_set_id INTEGER`，开仓时写入当前 active 集 id。
- `signals.features` 增加 `parameter_set_id`、`market_regime` 与 `symbol_regime`，无需改表。
- `positions.features` 同步记录，平仓后随成交回填。

### 5.5 配置组 `parameter_set`

新增配置组，AI 不可写：

| 键 | 默认值 | 说明 |
|---|---|---|
| `enabled` | `true` | 参数集与 regime 切换总开关 |
| `auto_apply` | `false` | 证据通过后自动启用；C 期部署时才设 `true` |
| `max_auto_apply_per_24h` | `1` | 滚动 24 小时自动启用限速，只统计 auto_apply |
| `no_candidate_rollback_sec` | `1800` | 生效后连续无 gate 候选的实跑时长 |
| `regime_confirm_sec` | `1800` | market_regime 连续一致的实跑时长 |
| `regime_cooldown_sec` | `1800` | 两次 regime 切换最小间隔 |
| `min_oos_trades` | `30` | 证据门目标 regime 样本外成交数下限 |
| `auto_apply_timeframes` | `1h` | 允许自动切换的 timeframe 白名单，v1 仅 1h |

AI 组新增：

| 键 | 默认值 | 说明 |
|---|---|---|
| `param_tuning_min_interval_sec` | `86400` | 独立调参任务最小间隔；A 期起语义变更，不再用于每轮复核冷却 |
| `allow_param_tuning` | `true` | 既有总开关，关闭后停止候选生成 |

A 期迁移复用既有的 `ai.param_tuning_min_interval_sec`，不再新增 `param_tuning_interval_sec`。迁移时若该值为 0，改为 86400；其它显式值保留。

### 5.6 迁移

- 使用 `schema.rs` 的幂等迁移：`CREATE TABLE IF NOT EXISTS`、列存在检查后 `ALTER TABLE ADD COLUMN`。
- 新表和新列全部可空或有默认值，不删除、不重命名旧列。
- A 期迁移时种子化（只建记录，不改 `app_settings.strategy`）：
  - `chaos-bootstrap`：当前 `app_settings.strategy` 完整快照，覆盖为 `1.0 / 8 / 1.0 / 0.5`，`timeframe='1h'`，`status='active'`，`source='bootstrap'`。
  - `range-bootstrap`：当前快照覆盖为 `0.6 / 10 / 0.8 / 0.45`，`timeframe='1h'`，`status='active'`，`source='bootstrap'`。
  - `chaos-live-2026-11-23`：事故当天线上快照 `1.5 / 8 / 1.1 / 0.55`，`timeframe='1h'`，`status='retired'`，`source='manual'`，仅作审计。
  - 两条 `parameter_set_activations`，分别对应两套 active 集，`trigger='bootstrap'`，`previous_set_id=NULL`。
- C 期迁移只启用 regime 选择与物化，不重复种子化。
- 迁移幂等：`parameter_sets` 已有数据时跳过种子化。
- 仅当当前 `strategy.timeframe` 在 `auto_apply_timeframes` 白名单（v1 仅 1h）时种子化 range/chaos bootstrap；其它 timeframe 只保存 `status='retired'` 的当前快照审计集，把 `auto_apply_timeframes` 置空并告警，等待人工校准。
- 迁移失败整体回滚；`app_settings.strategy` 保持原值。
- v1 不自动清理 `parameter_sets` / `parameter_set_activations`；候选、证据、激活历史全部保留，预计行数很小；后续再制定归档策略。

## 6. A 期：调参解耦

### 6.1 入口复核不再调参

- 移除 `ai_review::review_entry` 中对 `apply_decision_params` 的调用。
- `Decision.params` 仍可解析以兼容旧模型输出，但开仓复核路径忽略。
- 开仓复核 prompt 不再注入参数目录、参数变更历史和可写参数列表，只保留行情、持仓、经验/心法、AI 复核所需上下文。
- `parameter_changes` 在 A 期只允许来自人工、回滚或参数集激活，不再来自每轮复核。
- A 期同时把 `risk` 移出 `AI_FULL_WRITABLE_GROUPS`，更新 `param_registry` 断言；即使旧路径残留也无法写 risk。

### 6.2 独立调参任务

- 新模块 `src/tuning.rs`，由 `main.rs` 在运行时启动后 `tokio::spawn`。
- 周期读取 `ai.param_tuning_min_interval_sec`，使用 `tokio::time::interval`；停止信号复用 runtime 的 stop channel 或独立 shutdown。
- 触发条件：
  - `ai.allow_param_tuning=true`
  - `parameter_set.enabled=true`
  - 距上次任务至少 `ai.param_tuning_min_interval_sec`
  - 服务启动后延迟 5 分钟首次检查，之后按该间隔运行
  - 当前没有正在执行的调参任务（进程内 Mutex 去重）
  - 当前 regime 能找到 active 父集（`trend` 回退 `range` 后仍需存在）；否则跳过并写 `parameter_set.parent_missing` 告警
- 输入：
  - 当前 `market_regime` 与 active 参数集；`trend` 无 active 集时使用 `range` 集作为父集
  - 最近 30 天已平仓交易聚合：笔数、胜率、profit factor、expectancy、平均持仓、按 regime 分组
  - 最近 7 天信号审计：gate 拒绝数、AI hold 数、候选数、按 regime 分组
  - 最近参数集证据与变更历史
  - 当前可写参数目录（仅 strategy，使用 `param_registry::ai_bounds`）
- 输出：
  - 一个 JSON 参数增量，最多 3 个 strategy 键
  - 每个变更的理由
- 校验：
  - 只允许 `group='strategy'` 且 `param_registry::is_ai_writable`
  - 键必须存在
  - 单键相对变化 ≤ 25%
  - 硬边界（1h profile）：`atr_pct_min ≤ 1.0`、`atr_pct_max ≥ 6`、`volume_ratio_min ≤ 1.0`、`entry_threshold ≤ 0.5`
  - 合并到当前 active 集后通过 `settings_api::validate`
  - `param_registry::ai_bounds(group,key)` 是 AI 专用边界：调参 prompt 只展示该范围，候选校验用同一范围；页面/人工仍用宽 `RANGES`
- 写入：
  - 生成完整 strategy 快照
  - `parameter_sets(status='candidate', source='ai', parent_id=当前 active 集 id, timeframe=当前 strategy.timeframe, params=完整快照, evidence='{}')`
  - 写 `engine_events(category='parameter_set', level='info', message='生成参数集候选')`
  - **不写 `app_settings.strategy`**

### 6.3 A 期验收

- 开仓复核调用 100 次后，`parameter_changes` 不增加。
- 调参任务按 24h 产生 candidate；`app_settings.strategy` 前后完全一致。
- 迁移后存在 chaos-bootstrap 与 range-bootstrap 两条 active 记录，但 `app_settings.strategy` 未被改写。
- 候选参数超出边界、超过 3 个键或改动幅度过大时被拒绝，并写事件。
- `ai.allow_param_tuning=false` 时任务不运行。

## 7. B 期：回测证据门

### 7.1 统一 ATR 口径

- `TradePolicy` 新增：
  - `atr_pct_min: f64`
  - `atr_pct_max: f64`
- `trade_policy::evaluate` 使用 policy 的 ATR 上下界，替换硬编码 `0.05..=4.0`。
- `runtime::trade_policy(strategy)` 从 `strategy.atr_pct_min/atr_pct_max` 读取，缺省 `0.05/4.0`。
- 回测和实盘共用同一 `TradePolicy` 构造。

### 7.2 回测扩展

- `backtest_engine::EngineConfig` 已包含 `policy: TradePolicy`，新增字段自动生效。
- 新增 `run_walkforward_basket_fixed(basket, config, folds, train_bars, target_regime)`：
  - `basket` 是当前启用标的池的 `(symbol, bars)` 列表；逐标的跑固定参数集 walk-forward。
  - 每折在训练段直接使用固定候选参数集，不在训练段做网格选优。
  - 测试段使用同一固定集，`BacktestTrade` 记录开仓时的 `market_regime`；只统计与 `target_regime` 一致的样本外交易。
  - 跨标的按开仓时间合并 OOS 交易；`oos_avg_return_pct` 为等风险权重下的平均单笔收益，`max_drawdown_pct` 为合并交易序列的累计收益最大回撤。
  - 聚合 `oos_avg_return_pct`、`positive_folds`、`overfit_gap_pp`、`oos_trades`、`max_drawdown_pct`、`bootstrap_ci_low`、`bootstrap_p_positive`、`symbols_used`、`symbols_skipped`、`relative_advantage_pp`。
  - 有效标的少于 6 个或目标 regime 的 OOS 成交少于 `min_oos_trades` 时，证据门直接失败。
- 保留现有 `run_walkforward` 网格行为，不影响已有回测页面。
- 新增证据摘要结构：

```json
{
  "version": 1,
  "generated_at": 0,
  "data": {
    "timeframe": "1h",
    "interval": "1h",
    "symbols_used": ["BTCUSDT"],
    "symbols_skipped": [],
    "target_regime": "range",
    "bars_per_symbol": 2000,
    "folds": 3,
    "train_bars": 800
  },
  "candidate_regime": "range",
  "candidate_timeframe": "1h",
  "baseline_set_id": 1,
  "candidate": {
    "oos_trades": 0,
    "positive_folds": 0,
    "oos_avg_return_pct": 0.0,
    "overfit_gap_pp": 0.0,
    "stressed_avg_return_pct": 0.0,
    "max_drawdown_pct": 0.0,
    "bootstrap_ci_low": 0.0,
    "bootstrap_p_positive": 0.0
  },
  "baseline": {
    "oos_trades": 0,
    "positive_folds": 0,
    "oos_avg_return_pct": 0.0,
    "overfit_gap_pp": 0.0,
    "stressed_avg_return_pct": 0.0,
    "max_drawdown_pct": 0.0,
    "bootstrap_ci_low": 0.0,
    "bootstrap_p_positive": 0.0
  },
  "relative_advantage_pp": 0.0,
  "gate": {
    "pass": false,
    "reasons": []
  }
}
```

### 7.3 证据门

- 使用当前 `trades`/`positions` 相同的参数口径与费率。
- 候选 `timeframe` 必须等于当前 `strategy.timeframe`，且在当前 `auto_apply_timeframes` 白名单；否则证据门直接失败。
- 只统计开仓时 `market_regime` 等于 `candidate.regime` 的样本外交易；基线也按同一 market regime 过滤。
- `market_regime` 序列由标的池逐标的 regime 按时间对齐后等权多数生成；有效标的不足 6 个的时间段不产生可计入交易。
- 目标 regime OOS 成交少于 `min_oos_trades`（默认 30）时失败。
- Bootstrap 必须 `ci_low > 0` 且 `p_positive >= 0.90`，否则失败。
- 候选和同 regime 的当前 active 集各跑一次固定 walk-forward；候选必须相对 active 至少高 0.10pp。该 regime 没有 active 集时，使用对应 bootstrap 作为基线。
- 通过：`status='approved'`，写 `evidence`，事件 `parameter_set.evidence_pass`。
- 失败：`status='rejected'`，写 `evidence.gate.reasons`，事件 `parameter_set.evidence_fail`。
- B 期 `parameter_set.auto_apply=false`，approved 不激活。

### 7.4 证据门局限

回测不包含：

- AI 复核
- 盘口点差闸门
- 真实 OI 变化（固定 NaN）

因此证据门验证的是量化信号和出场管理，不是最终实盘 PnL。通过成本压力、样本外折数、相对基线比较和自动回滚降低风险；这一点作为已知边界接受。

### 7.5 B 期验收

- `atr_pct_min/max` 在 `evaluate` 和回测中生效，单测覆盖边界。
- `run_walkforward_basket_fixed` 对固定参数集、标的池合并和目标 regime 过滤输出完整聚合。
- 有效标的少于 6 个、目标 regime 样本不足、成本压力失败时，证据门均能正确失败。
- 证据门每个阈值都有 pass/fail 边界测试。
- `parameter_set.auto_apply=false` 时，approved 集不会写 `app_settings.strategy`。

## 8. C 期：Regime 参数集与自动切换

### 8.1 Regime 暴露

- 新增 `Regime` 枚举与 `classify_regime(adx, gap_pct, atr_pct, volume_ratio) -> Regime`。
- `trade_policy::core` 调用该函数，不再内联字符串判断。
- 新增 `market_regime(regimes: &[Regime]) -> Option<Regime>`：至少 6 个有效标的，等权多数；平票取 `range`。
- 新增 `trade_policy::regime_series(candles, timeframe) -> Vec<Option<Regime>>`：
  - 逐 bar 计算 adx/gap/atr_pct/volume_ratio，复用 `core` 的指标函数，不另写一套。
  - 序列与 bar 的 `close_time` 对齐；每根 bar 的 regime 只用于该 bar 收盘后的决策。
  - 回测 basket 用同一函数生成每个标的的 regime 序列。
- 新增 `market_regime_series(series_by_symbol, timeframe)`：
  - 以 `close_time` 按 timeframe 网格分桶；每个桶取各标的最新 bar，超过 1 个 timeframe 的视为 stale。
  - 至少 6 个有效标的；等权多数；平票取 `range`；不足 quorum 的桶为 `None`。
  - 实盘每轮取当前桶；回测与证据门共用同一序列。
- `FeatureSnapshot`、`signals.features`、`positions.features`、`trades.features` 记录 `symbol_regime` 与 `market_regime`。
- `regime.current` 存 `market_regime`；后台同时展示 market regime 与各标的 symbol regime。

### 8.2 Regime 确认与冷却

`engine_state` 保存：

- `regime.current`
- `regime.pending`
- `regime.pending_count`
- `regime.changed_at`

每轮：

- 启动/迁移后 `regime.current` 为空时：首个有 quorum 的 `market_regime` 立即生效，`regime.changed_at=now`，不做确认窗口。
- 之后新的 `market_regime` 与 `regime.current` 相同：清空 pending。
- 不同：更新 pending；pending 连续保持达到 `regime_confirm_sec` 后允许切换。
- 距 `regime.changed_at` 小于 `regime_cooldown_sec` 时只累计 pending，不切换。
- 有效标的不足 6 个、`market_regime` 无结果时保持当前 regime 并告警，不重置 pending。

### 8.3 Active 集选择与物化

- 每个 regime 至多一个 `status='active'` 参数集。
- C 期首次启用时，如果 `regime.current` 已有值且 `parameter_set.current_set_id` 为空，立即物化该 regime 的 active 集，不等待确认窗口。
- 当前 `market_regime` 无 active 集时：
  - `trend` 回退到 `range` active 集。
  - 其它 regime 保持当前 `app_settings.strategy`，发 `parameter_set.regime_missing` 告警。
- 切换事务：
  1. 查询目标 active 集。
  2. 校验完整 strategy JSON。
  3. 比较当前 `app_settings.strategy`，逐键生成 `parameter_changes`。
  4. 写 `app_settings.strategy`。
  5. 写 `parameter_set_activations(trigger='regime')`。
  6. 写 `engine_events(category='parameter_set', level='info')`。
  7. 提交；任一步失败整体回滚。
- `engine_state` 记录 `parameter_set.active.{regime}` 与 `parameter_set.current_set_id`，便于观测。

### 8.4 自动启用候选集

- B 期 approved 的候选，在 C 期由激活器处理。
- 仅当 `parameter_set.enabled=true` 且 `auto_apply=true`。
- 仅当候选 `timeframe` 等于当前 `strategy.timeframe`，且当前 timeframe 在 `auto_apply_timeframes` 白名单（v1 仅 `1h`）；不匹配时保持 approved 并发告警。
- C 期上线时在 `engine_state` 写入 `parameter_set.auto_apply_since`；只自动启用 `created_at > auto_apply_since` 的 approved 候选，避免 A/B 期旧候选在 C 期抢先覆盖 bootstrap。
- 全局限速：滚动 24 小时内最多自动启用 1 次（只统计 `trigger='auto_apply'` 的 activation 记录）。
- 启用：
  - 同一事务内先评估旧 active 集再落新集：旧 active `status='retired'`，候选 `status='active'`，避免同 regime 唯一 active 约束冲突。
  - 写 `parameter_set_activations(trigger='auto_apply', previous_set_id=旧 id)`。
  - 若目标 regime 等于当前 `market_regime`，立即物化到 `app_settings.strategy`；否则只更新 active 状态，等待 regime 切换。
- 失败：候选保持 approved，写事件，下一周期重试；不阻塞引擎。

### 8.5 无候选回滚

- 每次 active 集切换（regime 或 auto_apply）后，记录守卫：
  - `engine_state['parameter_set.guard.{regime}'] = { set_id, last_candidate_at, started_at }`；激活时 `last_candidate_at=started_at=now`
- 只有 `report.skipped.is_empty()` 的实跑轮次参与守卫：
  - `ai_symbols.len() > 0`：`last_candidate_at=now`。
  - `ai_symbols.len() == 0` 且 `now - last_candidate_at >= no_candidate_rollback_sec`（默认 1800 秒）：触发回滚。
  - `report.skipped` 非空（不在交易时段、日亏/回撤/连亏/冷却熔断、止损熔断等）：不累计零候选，`last_candidate_at=now` 暂停计时，并写 `parameter_set.guard_paused` 事件（同一原因按小时限流）。
- 触发回滚：
  - 回滚到 `parameter_set_activations` 中该 regime 的上一套 active 集。
  - 新集 `status='retired'`，写 `parameter_set_activations(trigger='no_candidate')`。
  - 写 warn 事件 `parameter_set.no_candidate_rollback`。
  - 回滚后守卫归零；若上一套也连续无候选，只告警，不再自动回滚，避免来回抖动。
  - 若该 regime 没有上一套 active（例如首次 bootstrap），只写告警，不回滚。
- 手动回滚同样可用，写 `trigger='rollback'`。

### 8.6 后台页面

新页面 `参数集`（`/admin/parameter-sets`）：

- 当前 `market_regime`、当前 active 集、当前 strategy 摘要。
- A/B 期 active 集尚未物化时，页面标记「待 C 期启用」，并展示 active 集与当前 strategy 的键差异。
- 按 regime 分组的参数集列表：状态、来源、版本、证据摘要。
- 证据详情：折数、OOS 成交、收益、回撤、压力测试、gate reasons。
- 激活历史：触发原因、前后 set id、时间。
- 手动操作：
  - 启用某个 approved/manual 集（确认弹窗）
  - 回滚当前 active 集到上一套
- 所有手动操作走同一事务与审计。

### 8.7 C 期验收

- A 期迁移已种子化 bootstrap；C 期启用后，若当前 `market_regime`=`range`，`app_settings.strategy` 等于 range-bootstrap 快照；若当前不是 range，保持对应 regime 的 bootstrap 并记录告警，不强制改参数。
- 首个有 quorum 的实跑轮次完成 regime 初始化；range 情况下首个实跑轮次 gate 候选数 >0（首个 regime 不做确认窗口）。
- 单元/集成测试覆盖：
  - 30 分钟 regime 确认与 30 分钟冷却
  - 滚动 24 小时自动启用限速
  - 30 分钟无候选回滚（skipped 轮次不计）
  - trend fallback 到 range
  - 切换事务原子性
- 线上观察 30 分钟（至少 2 个实跑周期）后确认：候选数、AI hold 率、regime、active 集正常。

## 9. 安全与回滚

### 9.1 权限边界

- `AI_FULL_WRITABLE_GROUPS` 从 `["risk","strategy"]` 改为 `["strategy"]`；risk 组全部不可写，A 期一并更新既有断言。
- 新增组级黑名单 `AI_BLOCKED_GROUPS = &["parameter_set"]`；`is_ai_writable` 先检查该列表，再走可见性兜底，避免新组默认变成 AI 可写。
- `parameter_set` 组加入 `VISIBLE`/设置目录，使键在参数页可见，但不出现在 AI 可写目录。
- `parameter_set` 组所有键 `ai_writable=false`，并有目录级测试断言。
- `risk` 组不进入参数集，AI 不能通过调参路径改风险参数。
- `engine.mode/enabled`、`ai.allow_param_tuning` 等关键开关不允许 AI 写。
- 自动切换只写 `strategy`；其它配置组不动。

### 9.2 Kill switches

- `ai.allow_param_tuning=false`：停止候选生成。
- `parameter_set.enabled=false`：停止 regime 选择、自动启用和无候选回滚；保留手动启用/回滚。
- `parameter_set.auto_apply=false`：候选可通过证据门，但不会自动启用。
- `engine.enabled=false`：停止交易引擎（既有开关，不作为参数治理专用手段）。

### 9.3 审计

- `parameter_sets` 保留全部候选、通过、拒绝、激活、退役记录。
- `parameter_set_activations` 记录每次切换及触发原因。
- `parameter_changes` 记录每次实际键变化，含 set_id/regime/mode/source；`source` 区分 `parameter_set`、`parameter_set_rollback` 与既有单键 `rollback`。
- `engine_events` 记录候选生成、证据通过/失败、激活、regime 变更、无候选回滚、回滚失败。
- 前端所有手动操作需确认，并显示目标集与预期变化。

### 9.4 回滚策略

- 代码回滚：A/B/C 各自独立提交，可单独 revert；新表为附加表，旧代码启动不受影响。
- 运行回滚：
  - 关闭 `parameter_set.enabled` 停止自动切换。
  - 手动回滚当前 active 集到上一套。
  - 关闭 `ai.allow_param_tuning` 停止候选生成。
- 数据回滚：
  - 不删除 `parameter_sets` 历史；回滚通过新增 activation 记录完成。
  - 迁移失败不改变 `app_settings.strategy`。

## 10. 配置项汇总

### `parameter_set` 组

| 键 | 默认 | 所有者 | 说明 |
|---|---|---|---|
| `enabled` | true | 人工 | 参数集与 regime 总开关 |
| `auto_apply` | false | 人工 | 自动启用 approved 候选；C 期部署时设 true |
| `max_auto_apply_per_24h` | 1 | 人工 | 滚动 24 小时自动启用限速，只统计 auto_apply |
| `no_candidate_rollback_sec` | 1800 | 人工 | 实跑轮次中连续无 gate 候选的时长 |
| `regime_confirm_sec` | 1800 | 人工 | market_regime 连续一致的实跑时长 |
| `regime_cooldown_sec` | 1800 | 人工 | regime 冷却秒数 |
| `min_oos_trades` | 30 | 人工 | 证据门目标 regime 样本外成交下限 |
| `auto_apply_timeframes` | `1h` | 人工 | 允许自动切换的 timeframe 白名单（v1 仅 1h） |

### `ai` 组新增

| 键 | 默认 | 所有者 | 说明 |
|---|---|---|---|
| `param_tuning_min_interval_sec` | 86400 | 人工 | 独立调参任务最小间隔；复用既有键，A 期起语义变更 |
| `allow_param_tuning` | true | 人工 | 既有总开关 |

### 证据门常量

以下阈值固定在代码中，不允许 AI 或页面修改；如需调整走代码变更：

- 3 折 / 800 训练根 / 每个参与标的至少 2000 根当前 timeframe K 线
- 有效标的 ≥ 6；目标 regime OOS 成交 ≥ 30
- 盈利折数 ≥ 2
- 过拟合差距 ≤ 1.0pp
- Bootstrap `ci_low > 0` 且 `p_positive >= 0.90`
- 压力测试平均单笔收益 ≥ 0
- 最大回撤 ≤ 15%
- 相对同 regime active 集平均单笔收益 ≥ +0.10pp
- 最多 3 个键、单键相对变化 ≤ 25%
- 参数硬边界（1h profile）：`atr_pct_min ≤ 1.0`、`atr_pct_max ≥ 6`、`volume_ratio_min ≤ 1.0`、`entry_threshold ≤ 0.5`
- timeframe 必须匹配当前 `strategy.timeframe`，且在当前 `auto_apply_timeframes` 白名单

## 11. 上线顺序与验收

实现计划按 A/B/C 拆成三份，每份独立验收、提交、部署；避免一个计划跨越三套行为变更。

### A 期

- 部署：移除入口调参、加入独立调参任务、迁移参数集表/配置组并种子化 bootstrap（不物化 strategy）。
- 同时部署只读的 `market_regime` / `symbol_regime` 计算与审计，供调参任务选择父集；C 期才启用 regime 选择与切换。
- 线上策略：保持不变。
- 验收：
  - 入口复核不再增加 `parameter_changes`
  - 24h 内出现至少一条 candidate
  - `app_settings.strategy` 未变化

### B 期

- 部署：ATR 口径统一、`run_walkforward_basket_fixed`、证据门、参数集 API/页面（只读+证据）。
- 配置：`parameter_set.auto_apply=false`。
- 验收：
  - 候选产生 evidence
  - approved/rejected 状态正确
  - `app_settings.strategy` 未变化
  - 全部证据门边界测试通过

### C 期

- 部署：启用 regime 选择/自动启用/无候选回滚、前端完整页面；不重复种子化。
- 配置：`parameter_set.enabled=true`、`auto_apply=true`；写入 `parameter_set.auto_apply_since`。
- 上线时点：`market_regime=range` 时物化 range-bootstrap `0.6/10/0.8/0.45`；若首个有 quorum 的 regime 不是 range，使用对应 bootstrap 并告警。
- 验收：
  - 首个有 quorum 的实跑轮次完成 regime 初始化；range 情况下首个实跑轮次 gate 候选数 > 0（首个 regime 不做确认窗口）
  - regime/active 集展示正确
  - 自动启用、滚动 24h 限速、30 分钟无候选回滚（仅实跑轮次）均有测试
  - 线上观察 30 分钟（至少 2 个实跑周期）无异常

### 发布门禁

每个阶段合并前运行：

- `cargo fmt --all --manifest-path rust/nanoquant/Cargo.toml --check`
- `cargo clippy --all-targets --manifest-path rust/nanoquant/Cargo.toml -- -D warnings`
- `cargo test --release --locked --manifest-path rust/nanoquant/Cargo.toml`
- `npm test`（frontend）
- `npm run typecheck`（frontend）
- `npm run build`（frontend）

## 12. 测试策略

### 单元测试

- `parameter_sets`：
  - `param_registry`：`parameter_set` 与 `risk` 组所有键 `ai_writable=false`，AI 可写目录仅含 strategy。
  - 候选校验：risk 组拒绝、非 strategy 键拒绝、>3 键拒绝、>25% 步长拒绝、硬边界拒绝、validate 失败拒绝。
  - 状态迁移：candidate→approved→active→retired；rejected 不可激活。
  - 每 regime 唯一 active 约束；旧 active 退役与新 active 写入同事务不冲突。
- `tuning`：
  - `ai.param_tuning_min_interval_sec` 周期、去重、总开关。
  - 候选只写 parameter_sets，不写 strategy。
  - 候选 `timeframe` 必须与当前 `strategy.timeframe` 一致。
  - 证据失败不阻塞下一轮。
- `backtest_engine`：
  - `run_walkforward_basket_fixed` 固定参数集、标的池合并、目标 regime 过滤。
  - ATR/volume/entry 参数生效，且 ATR% 周期等于 `strategy.timeframe`。
  - 证据门各阈值边界，包括 30 笔 OOS、bootstrap CI、pp 单位。
  - 样本不足、timeframe 不匹配直接失败。
- `regime`：
  - 三个 symbol regime 边界；`regime_series` 与 bar `close_time` 对齐。
  - `market_regime_series` 分桶、stale 排除、等权多数、平票 range、少于 6 个无效。
  - `regime_confirm_sec`、冷却、pending 重置、无效 quorum 保持当前 regime。
  - 首次 `regime.current` 为空时立即初始化，不走确认窗口。
  - trend fallback。
- 激活：
  - 事务原子性（注入写入失败，strategy 不变）。
  - 滚动 24h 限速只统计 auto_apply。
  - `auto_apply_since` 之前的 approved 候选不自动启用。
  - `timeframe` 不在白名单时不自动启用。
  - 30 分钟无候选回滚；`report.skipped` 非空轮次不计零候选并暂停计时；回滚后不再反复回滚。
- 迁移：
  - 幂等；旧库兼容；A 期种子化 chaos/range bootstrap 且不物化 strategy；C 期不重复种子化。
- 审查反馈回归：
  - B1：ATR% 计算周期 == `strategy.timeframe`；`parameter_sets.timeframe` 不匹配拒绝。
  - B2：熔断/交易时段/止损熔断的 skipped 轮次不累计零候选。
  - B3：chaos-bootstrap 四值均在 1h 硬边界内；AI 在 chaos 父集下可合法下调 `atr_pct_min` 到 ≤1.0。
  - B4：`parameter_set` 组所有键 `ai_writable=false`，且不出现在 AI 可写目录。
  - B5：参数集激活写入 `parameter_changes` 时 `mode`、`decision_id`、`source` 完整，`source` 与单键 rollback 区分。

### 集成测试

- 测试库中模拟：
  - 候选生成 → 证据 pass → 自动启用 → 当前 regime 物化
  - regime range→chaos→range，验证 strategy 切换
  - 切换后 30 分钟无候选，验证回滚到上一套；期间插入 skipped 轮次，验证不误判
  - `parameter_set.enabled=false` / `auto_apply=false` / `ai.allow_param_tuning=false`
  - timeframe 从 1h 切到 4h 时，自动启用被拒绝并告警
- 现有 runtime/entry 测试全部通过，确认 A 期移除调参未破坏开仓/审计。

### 前端测试

- 参数集列表/证据摘要/状态标签的纯逻辑单测。
- 手动启用/回滚的确认与请求参数单测。
- `vue-tsc` 与生产构建。

## 13. 风险与缓解

| 风险 | 影响 | 缓解 |
|---|---|---|
| 回测不含 AI/盘口/OI | 证据门高估实盘表现 | 成本压力、相对基线、3 折 OOS、滚动 24h 限速、30 分钟无候选回滚 |
| 自动启用仍然可能选错参数 | 实盘回撤 | 参数硬边界、步长限制、全局 kill switch、手动回滚 |
| 30 笔 OOS 样本偏小 | 过拟合 | 3 折、压力测试、bootstrap CI、相对 active 优势、后续可提高阈值 |
| 目标 regime 交易稀疏 / 有效标的不足 | 证据门长期失败，无法自动优化 | 证据不足直接失败并保留原因；bootstrap 仍可用；人工可提升数据覆盖或调整阈值 |
| Regime 阈值硬编码且与周期耦合 | 误分类 | 3 个 regime 边界按 timeframe 解释；30 分钟确认/冷却；trend fallback；v1 仅 1h 自动切换 |
| 只治理 strategy，risk 仍全局 | 风险不随 regime 调整 | 明确非目标；risk 移出 AI 可写组，人工维护 |
| 旧数据无参数集 id | 无法精确归因历史 | 仅对部署后新交易归因；旧数据留空 |
| C 期上线即改变参数 | 可能立即开仓 | 已由用户确认；部署前备份；30 分钟观察；无候选回滚 |
| CPU 回测任务 | 影响运行时 | `spawn_blocking`、24h 一次、进程内去重、失败降级 |

## 14. 接口契约

### 后端 API

- `GET /api/v1/parameter-sets?regime=&status=`：列表，含证据摘要。
- `GET /api/v1/parameter-sets/current`：当前 `market_regime`、当前 active 集、guard 状态。
- `GET /api/v1/parameter-sets/{id}/evidence`：完整证据。
- `POST /api/v1/parameter-sets/{id}/activate`：手动启用，确认参数集后物化。
- `POST /api/v1/parameter-sets/{id}/rollback`：将当前 regime 的 active 集回滚到上一套。
- `GET /api/v1/regime/current`：当前 `market_regime`、各标的 `symbol_regime` 与确认状态。
- 既有 `GET /api/v1/settings/parameter-changes` 返回值增加 `set_id`、`regime`。

### 模块边界

- `parameter_sets.rs`：注册表、状态、激活、回滚、物化；不调 AI、不跑回测。
- `tuning.rs`：AI 候选生成、校验、落 candidate；依赖 `parameter_sets` 与 `ai_client`。
- `backtest_engine.rs`：纯回测与证据摘要；不写数据库。
- `regime.rs`：纯分类与状态机；不写数据库；运行时负责持久化。
- `runtime.rs`：记录 regime/候选数/guard；调用 regime 选择器；不直接调参。
- `engine.rs`：参数集 API。
- `settings_api.rs`：配置校验、parameter_changes、rollback。
- 前端 `ParameterSetsView.vue` + `paramSets.ts`：展示与操作。

## 15. 决策记录

- 2026-11-23：用户选择「三项全做」，采用总体设计 + A→B→C 三期实现。
- 2026-11-23：参数集只包含 `strategy`；`risk` 不自动调。
- 2026-11-23：调参从每轮复核剥离，独立任务 24h 一次，只生成 candidate。
- 2026-11-23：不使用影子账户；证据门只使用历史 walk-forward 回测。
- 2026-11-23：证据通过后自动切换实盘参数，接受“仅凭回测证据”的风险。
- 2026-11-23：C 期上线即启用人工批准的 bootstrap：range `0.6/10/0.8/0.45`；chaos 使用硬边界内 `1.0/8/1.0/0.5`；事故线上值 `1.5/8/1.1/0.55` 保留为 retired 审计集。
- 2026-11-23：自动切换滚动 24h 最多 1 次；regime 连续 30 分钟确认/30 分钟冷却；无候选回滚按 30 分钟实跑时长，skipped 轮次不计。
- 2026-11-23：所有 ATR%/regime/证据门阈值以 `strategy.timeframe` 为口径；线上为 1h，仓库默认 4h 不适用；v1 自动切换只接受 1h。
- 2026-11-23：bootstrap 在 A 期迁移时种子化，C 期才物化到 `strategy`；C 期不重复种子化。
- 2026-11-23：证据门按启用标的池逐标的回测，只统计候选 `market_regime` 的目标样本外交易；有效标的少于 6 个直接失败。
- 2026-11-23：`market_regime` 取启用标的池的等权多数，平票取 `range`，少于 6 个有效标的保持上一个 regime 并告警；`symbol_regime` 仅用于诊断。
- 2026-11-23：C 期用 `parameter_set.auto_apply_since` 隔离 A/B 期旧候选，避免覆盖 bootstrap。
- 2026-11-23：实现计划按 A/B/C 拆成三份，每份独立上线。
- 2026-11-23：改造期间不做临时参数变更，接受持续空仓，直到 C 期上线。

## 16. 审查反馈处理

### 阻断级

- **B1 周期口径**：线上确认 `strategy.timeframe=1h`、`engine.cycle_interval_sec=600`；仓库默认 4h 仅为默认值，不用于事故阈值。新增 `parameter_sets.timeframe`，所有 ATR%/regime/证据门绑定当前 timeframe；v1 自动切换仅接受 1h，4h 需重新校准并关闭自动切换。
- **B2 守卫误判熔断**：无候选回滚只统计 `report.skipped.is_empty()` 的实跑轮次；交易时段外、日亏/回撤/连亏/冷却、止损熔断等 skipped 轮次不累计，暂停计时并写 `parameter_set.guard_paused`。
- **B3 chaos 硬边界冲突**：chaos-bootstrap 改为边界内 `1.0 / 8 / 1.0 / 0.5`；事故线上值 `1.5 / 8 / 1.1 / 0.55` 保存为 `chaos-live-2026-11-23` retired 审计集。AI 在 chaos 父集下可合法把 `atr_pct_min` 下调到 ≤1.0，不会锁死。
- **B4 新组默认 AI 可写**：新增 `AI_BLOCKED_GROUPS = ["parameter_set"]`，并把该组加入 `VISIBLE`；`is_ai_writable` 先检查组级黑名单，目录测试断言所有键为 false。
- **B5 parameter_changes 必填字段**：参数集切换写入补齐 `mode`、`decision_id=0`、`source=parameter_set` / `parameter_set_rollback`；既有单键回滚继续用 `rollback`，审计可区分。

### 澄清项

- **C1 risk 可写目录**：`AI_FULL_WRITABLE_GROUPS` 从 `["risk","strategy"]` 改为 `["strategy"]`，A 期更新 `param_registry` 断言；risk 不再进入 AI 可写目录。
- **C2 “轮”单位漂移**：配置改为 `regime_confirm_sec`、`no_candidate_rollback_sec`、`regime_cooldown_sec`，统一秒；24h 限速改名为滚动窗口 `max_auto_apply_per_24h`。
- **C3 证据门单位/样本**：`overfit_gap_pp` 明确为 pp；OOS 成交下限提高到 30；新增 bootstrap `ci_low > 0` 与 `p_positive >= 0.90`；相对基线优势保持 ≥0.10pp。
- **C4 market_regime 对齐**：新增 `regime_series` 与 `market_regime_series`，按 `close_time` 网格分桶、排除 stale、至少 6 个标的、等权多数、平票 range；实盘与回测共用同一序列。
- **C5 目录边界不一致**：新增 `param_registry::ai_bounds`，调参 prompt 与候选校验共用 AI 专用边界；页面/人工仍用宽 `RANGES`。
- **C6 旧间隔键**：复用 `ai.param_tuning_min_interval_sec`，默认改为 86400；不再新增 `param_tuning_interval_sec`。迁移时仅把 0 改为 86400。
- **C7 C 期前 30 分钟仍锁死**：`regime.current` 为空时首个有 quorum 的 regime 立即初始化并物化，不走 30 分钟确认；确认窗口只约束后续切换。
- **C8 命名与留存**：`max_auto_apply_per_24h` 表示滚动 24h；v1 不清理 `parameter_sets`/activation 历史；文档明确 `strategy_profiles`（AI 文本规则）与 `parameter_sets`（数值快照）是不同对象。
## 17. A 期实施记录（已落地）

A 期已在 feature 分支完成，实施时对通用设计做了以下收紧；B/C 实施必须以本节为准：

- A1：`parameter_set.auto_apply` 默认 `false`，C 期部署时才设 `true`；`AI_BLOCKED_GROUPS = ["parameter_set","risk"]`；`parameter_set` 八键有类型/范围/null/空串/仅 1h 校验；存量 `ai.param_tuning_min_interval_sec=0` 迁移为 86400。
- A2：`seed_bootstrap` 由 `bootstrap::initialize` 在默认配置写入**之后**调用，全部写入同一事务；非 1h 分支把 `parameter_set.auto_apply=false`、`auto_apply_timeframes=""` 并写 warn 事件；`chaos-live-2026-11-23` evidence 标记 `historical/partial`，只固定四个已知事故值；新增 activation/changes/trades 索引。
- A3：`active_for_regime` 返回 `Result<Option<ParameterSet>, String>`，仅 `QueryReturnedNoRows → Ok(None)`；`insert_candidate` 校验对象快照、`params.timeframe`、父集存在且同 regime；候选缺基线键直接拒绝；`settings_api::apply_param_updates` 已删除（A 期起 AI 不再直接写 app_settings）。
- A4：只读 regime 写入 `engine_state['market_regime.{mode}']` 与 `symbol_regime.{mode}`；<6 有效标的保留上一 `market_regime` 并 warn；`regime_series`/`market_regime` 纯函数已存在，B 期复用。
- A5：开仓复核 context/prompt 不再注入参数目录/历史/`p` 字段；AI 返回 `p` 只解析不应用；入口不写 `parameter_changes`；hold 置信度保留。
- A6：`tuning.rs` 独立任务按 `ai.param_tuning_min_interval_sec` 运行；读当前 mode 的 `market_regime.{mode}`；候选/时间戳/事件同事务；遵守 `ai.enabled`；调参目录只含 strategy 连续数值键，排除 `timeframe` 与整数语义键（`ema_fast/ema_slow/rsi_period/atr_period/max_data_gaps`）；`p:[]` 写 `parameter_set.no_candidate` 并推进时间戳。
- A7：Rust `cargo test --release --locked` 556 passed；clippy `-D warnings`、前端 44 tests、typecheck、build 通过。`cargo fmt --all --check` 仍有仓库基线已有的 21 处差异（`cli.rs`/`engine.rs`/`exec_client.rs`/`main.rs`/`ops_tools.rs`/`runtime.rs`/`system.rs`），本次 A 期改动文件无格式差异；未顺手格式化无关文件。

## 18. C 期实施记录（已落地）

- C1/C2：`regime` 状态机纯内存实现，runtime 周期尾按 mode 读写 `regime.current.{mode}`/`regime.pending.{mode}`/`regime.pending_since.{mode}`/`regime.changed_at.{mode}`；切换时调用 `parameter_sets::activate_for_regime(.., trigger='regime', mode)`，失败只 warn 并写事件、不阻断周期。`activate` 用 SAVEPOINT 包裹以兼容周期内外事务；`rollback` 供 C3/C4 使用。
- C2 收尾：`activate/rollback` 使用归一化 strategy 快照比较与物化（`settings_api::normalized_group_value`），历史键不会复活；active 集 params 与当前 strategy 不一致时会真正物化，一致才 no-op；激活失败恢复 `regime.current` 并按小时限流事件。
- C3：`auto_apply_approved` 遵守 `enabled/auto_apply`、`auto_apply_since`、1h 白名单、滚动 24h 配额；当前 regime 物化，非当前 regime 只切 active 不写 strategy。守卫仅统计 `report.skipped.is_empty()` 的实跑轮次；`rollback_done` 防止 30 分钟来回回滚；自动回滚 trigger 为 `no_candidate`，手动为 `rollback`。
- C4/C5：`POST /parameter-sets/{id}/activate` 仅允许当前 regime，手动 trigger=manual；`POST /parameter-sets/{id}/rollback` 仅允许 active 且当前 regime，trigger=rollback；`/current` 含 guard/auto_apply_since。前端新增 `ParameterSetsView`、路由与「参数集」菜单，支持证据摘要、确认启用/回滚。
- C6：`parameter_sets::enable_c_phase` 在 bootstrap seed 后调用；仅 1h 启用 `auto_apply=true`、写 `auto_apply_since` 并物化当前 regime bootstrap；非 1h 关闭 auto_apply 并告警；幂等、SAVEPOINT 回滚。
- C6 最终门禁：Rust `cargo test --release --locked` **642 passed**；clippy `-D warnings`、前端 53 tests、typecheck、build 通过；A 期以来改动文件无新增格式差异。
- 已知残余：证据门“6 标的×2000 根 fake history → 候选/baseline 回测 → approved”的完整成功路径集成测试尚未补；gate 边界、`mark_evidence`、候选/基线回测、失败路径均有定向测试。
- 未部署：线上仍执行旧 `strategy`（1.5/8/1.1/0.55），C 期 bootstrap 只有在部署新代码并重启后才生效。