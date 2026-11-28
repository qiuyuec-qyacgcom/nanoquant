# 修复核验报告：止损口径重构

> 对应方案：`docs/FIX_PLAN_stop_distance_2026-11-27.md`
> 核验时间：2026-11-27 10:23 ~ 10:35 (UTC+8)；ATR 周期收口 11:00 ~ 11:30；EMA 周期收口 11:30 ~ 11:45；
> EMA 实现统一与 ADX 周期参数化 11:45 ~ 12:00 (UTC+8)
> 核验基线：`HEAD=d1952d0`，改动全部位于工作区（未提交）

---

## 〇、结论

**修复已完整落地，方案 8 个条目全部实现，10 项验收用例全部通过，全部门禁绿灯。**
实施质量在两处高于方案要求：回测直接复用实盘的 `atr_stop_price` 函数（而非复制公式），
并额外修掉了方案 §四 提到的决策日志 `target_pct` 假 100% 问题。

**原先残留的 ATR 周期、EMA 周期分叉与两套 EMA 实现均已收口**，ADX 周期也已参数化（详见第四节）：
实盘扫描、AI 特征快照、信号内核、regime 判定、回测、证据门、标的扫描现在读同一份配置，EMA 实现全仓唯一。

**文档列出的本地遗留项已清零**：AI 调参物化路径已接跨组窗口校验，开仓 AI 复核结论已随 `features.ai_review`
落库，GA/证据门已加「止损距离 / ATR」维度，e2e 已跑通。仅线上部署与真实交易所联调不在本地范围。

---

## 一、门禁实测结果

| 门禁 | 命令 | 结果 |
|---|---|---|
| 编译 | `cargo check` | ✅ 通过（0 error） |
| Rust 测试 | `cargo test` | ✅ **930 passed / 0 failed** |
| 格式 | `cargo fmt --all --check` | ✅ 退出码 0 |
| Lint | `cargo clippy --all-targets -- -D warnings` | ✅ 退出码 0 |
| 前端类型 | `npm run typecheck`（vue-tsc --noEmit） | ✅ 无报错 |
| 前端测试 | `npm test`（vitest） | ✅ **81 passed / 12 files** |
| 前端构建 | `npm run build` | ✅ 退出码 0 |
| 浏览器 e2e | `npm run e2e`（Playwright + Edge） | ✅ **10 passed** |

> 与 `.github/workflows/ci.yml` 的必过项对齐（Rust fmt/clippy/test + 前端 typecheck/test/build + e2e）。

---

## 二、方案条目逐条对照

| 方案条目 | 状态 | 实现位置 |
|---|---|---|
| **F1** 开仓不再换算止损价 | ✅ | `runtime.rs:3151-3157`：趋势仓 `stop_price = risk::atr_stop_price(side, price, metric.atr_pct, atr_stop_mult)`，`take_profit = 0.0` |
| **F2** 新增 `stop_distance_atr_mult` | ✅ | `default_settings.json:43`（默认 1.3）、`param_registry.rs:62-63` 注册 + `:269` 进 VISIBLE |
| **F3** 快平任务判定顺序 | ✅ | `fast_risk.rs:86-123`：① 失效价 → ② ATR 保护价 → ③ ROI 兜底 → ④ 单仓止盈价 |
| **F3** `manage.rs` 同步 | ✅ | `manage.rs:86-92`：`sl_price` 判定前移至 ROI 兜底之前 |
| **F4** 参数页提示语 | ✅ | `settingsCatalog.ts:120`、`settings/meta.ts:87-91` 与 `:138-139` |
| **§4.2** review.rs **两处**校验放宽 | ✅ | `review.rs:269`（fail-open 分支）与 `review.rs:312`（主路径）**都已**改为只查 `stop_price` |
| **§五** 回测同步 | ✅ | `backtest_engine.rs:316`（出场顺序）、`:476-523`（开仓换算与闸门）、`:43-44` 新增 `stop_distance_atr_mult` 字段 |
| **§4.4** 决策日志 `target_pct` | ✅ | `runtime.rs` 新增 `journal_target_pct()`，`:3837` 与 `:4407` 两处调用点均已替换 |

### 10 项验收用例执行结果

```
test backtest_engine::tests::backtest_stop_matches_live_stop ... ok
test backtest_engine::tests::stop_distance_independent_of_leverage ... ok
test fast_risk::tests::atr_stop_precedes_roi_budget ... ok
test fast_risk::tests::invalidation_has_highest_priority ... ok
test fast_risk::tests::roi_budget_still_triggers ... ok
test live::tests::protection_legs_skip_zero_take_profit ... ok
test manage::tests::atr_stop_precedes_roi_budget_in_manage ... ok        （方案外补充）
test review::tests::fail_open_branch_allows_missing_take_profit ... ok
test review::tests::missing_stop_is_rejected ... ok
test review::tests::trend_entry_allowed_without_take_profit ... ok
test risk::tests::atr_stop_distance_is_multiple_of_atr_without_leverage ... ok  （方案外补充）
test runtime::tests::global_stop_not_used_at_entry ... ok
test runtime::tests::journal_target_pct_not_fake_when_take_profit_zero ... ok
```

原 `fast_risk::tests::global_stop_wins_over_invalidation` 已按新语义改写为 `invalidation_has_highest_priority`；
原 `review::tests::missing_stop_or_take_profit_is_rejected` 拆为 `missing_stop_is_rejected` + `trend_entry_allowed_without_take_profit`。

---

## 三、实施质量：两处优于方案要求

### 3.1 回测直接复用实盘函数，而非复制公式

方案 §五 只要求「回测同步 F1/F3 口径」。实施时把止损价算成了**同一个函数调用**：

```rust
// backtest_engine.rs 开仓路径
crate::risk::atr_stop_price(side, entry, metrics.atr_pct, self.config.stop_distance_atr_mult)

// runtime.rs 实盘开仓路径
crate::risk::atr_stop_price(&side, price, metric.atr_pct, atr_stop_mult)
```

这比「两边各写一遍公式再靠测试对齐」强一个量级 —— **口径分叉在结构上就不可能发生**。

### 3.2 额外修掉决策日志的假 100%

方案 §四.4 提到 `runtime.rs:4407` 会把 `target_pct` 记成 100%。本次一并修了：

```rust
fn journal_target_pct(entry_price: f64, take_profit: f64) -> f64 {
    if entry_price > 0.0 && take_profit > 0.0 { ... } else { 0.0 }
}
```

---

## 四、周期与指标口径的收口记录

**不属于本次方案范围，但被本次改动放大了**，需要单独立项。

**已于 2026-11-27 上午收口**（11:00–12:00），本节记录收口方式与验证。

#### 收口前的分叉

| 路径 | ATR 周期来源（收口前） | 影响的产物 |
|---|---|---|
| 实盘开仓 | ✅ 读 `strategy.atr_period`（`runtime.rs:1638`） | 止损价 |
| **回测开仓** | ❌ `candle_metrics(slice, 14)` 写死 | **止损价（上一轮新接入）** |
| **AI 上下文特征** | ❌ `candle_metrics(candles, 14)` 写死 | AI 看到的波动特征 |

默认值 `atr_period=14`（`default_settings.json:55`）三者一致，当时不产生偏差；
但用户一旦改动 `atr_period`，实盘止损距离会变而回测不会 —— 又是一次回测/实盘口径分叉。

#### 收口方式：ATR 周期收成单一来源

1. **`param_registry.rs`** 新增通用读取器 `strategy_int(strategy, key)`：
   取 `find()` 拿到的**注册范围**做区间校验，非法返回 `None`；
   范围不再由各模块各维护一份（原来 `runtime.rs` 写死 `2..=100`、`symbol_scan.rs` 用 `as_i64().max(1)`，两者本就不一致）。
2. **`ai_analysis.rs`** 新增 `atr_period_of(strategy)`（缺省 14），与既有的 `rsi_period_of` 并列；
   `feature_snapshot` 增参 `atr_period`，`candle_metrics(candles, atr_period)` 不再写死。
3. **`backtest_engine.rs`** 的 `EngineConfig` 新增 `atr_period: usize`，开仓处改用 `self.config.atr_period`；
   `backtest.rs` 与 `evidence_task.rs` 从 strategy 配置填入。
4. **`runtime.rs`** 实盘读取改用同一个 `strategy_int`，并保留「配置存在但非法 → 报错中止」的既有语义。
5. **`symbol_scan.rs`** 同步改用共享读取器，消除 `as_i64()` 对浮点失效、`max(1)` 放宽下限两处旧差异。

#### 验证

- 新增 `backtest_engine::tests::atr_period_drives_the_backtest_stop_and_matches_live`：
  用**振幅随位置变化**的行情夹具（`alternating_range_bars`），断言「周期 7 与 14 的止损距离必须不同」
  且「回测止损距离 == 实盘按同周期 ATR 口径算出的距离」。
  旧的均匀趋势夹具 `trending_bars` 每根振幅相同，ATR(7) 与 ATR(14) 恰好相等，**测不出这个 bug** —— 换夹具是必要的。
- 新增 `ai_analysis::tests::atr_period_setting_drives_the_feature_snapshot`：
  覆盖缺省 / 合法值 / 整数值的浮点写法 / 越界 / 非整数五种输入。
- 顺带修掉一个潜伏缺陷：实盘原来用 `as_u64()` 读周期，**JSON 里的 `14.0` 会读成 `None` 并让整轮实盘直接报错**；
  新读取器按「有限、整数、在注册区间内」判定，`14.0` 正常接受、`14.5` 仍然拒绝。
- 全仓库已无任何 `candle_metrics(..., 14)` 形式的硬编码 ATR 周期。

#### 4.2 EMA 周期收口（同批发现并修完）

收口 ATR 周期时顺带发现：**信号内核 `trade_policy` 也把 EMA 周期写死成 9/21**，
而且用的是**另一套 EMA 实现** —— `ai_analysis.rs` 用前 N 根的 SMA 播种，`trade_policy.rs` 用第一根收盘价播种。
写死的周期 + 两套实现 = 「装饰参数」+「两套口径」叠加。

| 位置 | 收口前 | 收口后 |
|---|---|---|
| `trade_policy.rs` 信号内核 | `ema(&closes, 9)` / `ema(&closes, 21)` 写死 | 读 `policy.ema.fast` / `policy.ema.slow` |
| `trade_policy.rs` 的 `regime_for` / `preview` | 拿不到策略参数 | 两个入口都改为接收 `TradePolicy` |
| `ai_analysis.rs` 特征快照 | `ema(&closes, 9)` / `ema(&closes, 21)` 写死 | 读 `ema_periods.fast` / `.slow` |
| `param_registry.rs` | 无读取器 | 新增 `ema_periods(strategy)`，范围取注册表 |

**防呆**：新增的 `param_registry::ema_periods()` 除范围校验外，还强制**快线必须短于慢线** ——
否则 `ema_gap_pct` 的符号会反转，信号方向被系统性带偏（`ema_fast=30, ema_slow=5` 就是这种配置）。
任一缺失、越界、非整数或快慢颠倒，一律整体回退 (9, 21)。

**顺带把 EMA 差接进了交易原因串**（`trade_policy.rs:269` 的 `fusion:...gap={:.4}...`）——
原来的 `fusion` 串里有 `regime=` 却没有 gap，看不出趋势强度是怎么来的；补上后每笔成交的原因里都能看到 EMA 差。

**验证**：

- `param_registry::tests::ema_periods_read_config_and_reject_inverted_pairs`：缺省 / 合法 / 浮点整数写法 / 快慢颠倒 / 越界 / 非整数 六种输入；
- `ai_analysis::tests::ema_period_setting_drives_the_feature_snapshot`：不同周期给出不同的 `ema_fast` 与 `ema_gap_pct`；
- `trade_policy::tests::ema_periods_drive_the_signal_kernel`：给私有 `Core` 加了 `gap_pct` 字段，断言不同周期给出不同的内核 gap。

#### 4.3 同源遗留项已处理

| 项 | 位置 | 处理结果 |
|---|---|---|
| **两套 EMA 实现** | `ai_analysis.rs` 的 `ema()` 与 `trade_policy.rs` 的 `ema()` | 已统一为 `trade_policy::ema`（第一根收盘价播种），`ai_analysis` 直接复用。保留信号内核口径，量化信号不变；AI 特征与内核不再分叉 |
| **ADX 周期写死 14** | `trade_policy.rs` 的 `adx(candles, 14)` | 已注册 `strategy.adx_period`（默认 14，范围 2–100），接入 `TradePolicy`、实盘、回测与 regime 判定；参数页仍按既有产品规则不展示该内部参数，可通过设置接口调整 |

> 验证：`param_registry::tests::adx_period_reads_config_and_falls_back_to_14`、
> `trade_policy::tests::adx_period_drives_the_signal_kernel`、
> `trade_policy::tests::ema_uses_first_close_seed`、
> `ai_analysis::tests::ema_period_setting_drives_the_feature_snapshot`（含与内核同实现的断言）。

##### 4.3.1 顺带核对：迁移与参数可见性

- **迁移安全**：`schema.rs:293` 的 `drop_legacy_removed_params` 只清理
  `sl_atr_mult` / `tp_atr_mult` / `min_risk_reward` / `risk_per_trade_pct`，
  **不含 `adx_period`** —— 重新注册它不会让用户设置被每次启动清掉。
- **参数可见性**：`adx_period` 不在 `param_registry` 的 VISIBLE 列表里 → 是隐藏参数，
  与 `ema_fast` / `atr_period` 一致；前端 `settingsCatalog.spec.ts` 的「不再暴露内部调参字段」用例仍然通过
  （前端 81 项测试全绿）。

##### 4.3.2 周期与数据窗口的静默失败（已处理）

复核后发现原描述把 `kline_limit` 与 ADX 绑定错了：引擎信号内核的 K 线来自
`provider.snapshot()` 的固定 `SNAPSHOT_KLINE_LIMIT = 250`（盘中未收盘的一根会被剔除，实际最多 249），
`kline_limit` 只用于换池扫描，不参与信号内核取数。真正可达的静默失败是：

| 耦合 | 结果 | 处置 |
|---|---|---|
| `ema_slow`（注册上限 400）> 249 | `trade_policy::ema` 返回 `None` → `core` 整体返回 `None` → 该标的零信号 | 保存时拒绝；启动时告警 |
| `adx_period`（注册上限 100）> 249 | 同上（当前范围不可达，仅做未来防护） | 同上 |
| `kline_limit` <= `atr_period` | `candle_metrics` 返回 `None` → 换池扫描所有标的 `data_ok=false` | 保存时拒绝 |
| 行情实际返回少于配置周期 | 同零信号，但属于数据稀疏而非配置错误 | `core` 限流 `warn`，带 available/required |

实现：

- `providers::SNAPSHOT_KLINE_LIMIT` 统一常量，替换 250 字面量；
- `trade_policy::required_candles()` 显式化「30 / ADX period+1 / EMA period」的耦合；
- `settings_api::validate_indicator_windows()` 跨组校验，`write_group` 与 `reset_group` 均拒绝非法组合；
- `bootstrap::initialize()` 对存量非法配置只告警、不阻断启动；
- **未做内核静默钳制，未改变任何指标数值**。

##### 4.3.3 AI 调参物化的跨组窗口校验（已处理）

`settings_api::write_group` / `reset_group` 与 `bootstrap::initialize` 都过了跨组校验；
`ai_tuning` 的物化路径原先只走单组校验。现在 `activate_in_savepoint` 与
`rollback_in_savepoint` 在写入 `strategy` 前都会调用
`settings_api::validate_indicator_windows(&after, data)`，非法指标窗口会在物化前被拒绝，
不会出现「AI 候选把 `ema_slow` 写成 250 后内核零信号」。

验证：

- `ai_tuning::tests::activate_rejects_indicator_window_beyond_snapshot`；
- `ai_tuning::tests::rollback_rejects_indicator_window_beyond_snapshot`。


---

## 五、部署与运维注意

| 项 | 状态 |
|---|---|
| 存量库兼容 | ✅ `bootstrap.rs` 的 `merge_missing` 会递归补齐缺失键并保留用户已有值：`stop_distance_atr_mult`（`risk` 组）与 `adx_period`（`strategy` 组）下次启动自动进组，无需手工迁移 |
| AI 能否放宽该参数 | ✅ 不能。该参数在 `risk` 组，而 `param_registry.rs:18` 的 `AI_FULL_WRITABLE_GROUPS` 只有 `strategy` —— 调参器碰不到止损距离 |
| 改动是否提交 | ✅ 本轮已提交并打 tag（见仓库 `git tag`） |
| 风险提示 | 已消除：改动已进入版本历史，可用标签回滚 |

### 行为变化提醒

- 趋势仓在交易所侧**只挂止损腿**（`STOP_MARKET`），止盈腿因价格为 0 被 `live.rs:462` 的 `push` 闭包跳过；
- 趋势仓的止盈出口改为由 **ROI 阶梯（480 秒周期任务）** 与 **0.5 秒快平任务（仅 live 模式，`fast_risk.rs:141`）** 承担；
- 交易所侧硬保护仍在（止损腿），但**止盈不再有交易所侧保底** —— 服务器不可用时盈利单可能回吐；
- demo 模式没有交易所保护单（本来就没有），止盈只剩 480 秒周期任务的 ROI 判定。

---

## 六、本次核验未覆盖项

- `npm run build`（vite 生产构建）与 playwright e2e 已执行：build 通过，e2e **10 passed**；
- 未做真实交易所联调（本次全程只读代码 + 本地编译测试，未触碰线上）；
- 线上部署与参数调整**不在本次范围**（`scripts/update.sh` 仍为手动运维项）；
- 方案 `docs/FIX_PLAN_stop_distance_2026-11-27.md` §八 的本地代码遗留项已处理：
  - `trades.reviewed` 语义保持不变（仍由 `evolution_review` 回写）；开仓 AI 结论改由
    `positions/trades.features.ai_review` 记录 `reviewed/action/confidence/reason`，不再不可追溯；
  - GA/证据门新增 `avg_stop_atr_ratio` / `min_stop_atr_ratio`，低于 1×ATR 会拒绝；
  - `veto_only` 仅作为 legacy 清理键存在，已不在任何运行时读取路径；
  - 部署仍靠手动 `update.sh`，属于运维流程，不在本地代码范围。
