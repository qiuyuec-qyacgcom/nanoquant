# 合约 CTA 半 AI 融合实现计划（全量）

> **执行约定：** 每完成一项，在 TodoWrite 打标并单独 commit；commit message 前缀 `[A1]`/`[B1]`/`[D2]` 等。测试统一用 `cargo build --tests --bin nanoquant` + `target/debug/build/nanoquant/*/out/nanoquant-*.exe`，**禁止执行 `cargo test --bin nanoquant`**。

**目标：** 按 `docs/superpowers/specs/2026-11-27-fusion-hybrid-ai-design.md` 落地半 AI、风控过滤、低延迟执行、均值回归、RL 执行优化与 GA 参数搜索。

**架构：** 量化信号为主，AI 只做否决/降仓；本地杠杆规则接管杠杆；WebSocket 盘口替代 REST 行情；RL/GA 均为离线/执行层，不进入方向决策。

**技术栈：** Rust、tokio、tokio-tungstenite、rusqlite、serde_json、Vue 3。

---

## 执行顺序与标记

| 批次 | 任务 | 标记 |
|---|---|---|
| P0-A | 资金费率过滤 | `[A1]` |
| P0-A | 事件否决 | `[A2]` |
| P0-A | 组合风控 | `[A3]` |
| P0-A | 半 AI 复核 + scale | `[A4]` |
| P0-A | 本地杠杆规则 | `[A5]` |
| P0-B | WebSocket 本地盘口 | `[B1]` |
| P0-C | 减少 AI 调用 | `[C1]` |
| P0-C | Rust 异步执行 | `[C2]` |
| P1 | 均值回归 | `[D1]` |
| P1 | RL 执行优化 | `[D2]` |
| P1 | GA 离线参数搜索 | `[D3]` |
| 清理 | 删除 AI 覆盖杠杆/止损/方向的死路径 | `[E1]` |

---

## A1 资金费率过滤

**文件：**
- 修改：`rust/nanoquant/src/scan.rs`（`Candidate`、`ScanGates`、`risk_gate`、测试）
- 修改：`rust/nanoquant/src/runtime.rs`（`scan_gates_of`、`Candidate` 构造）
- 修改：`rust/nanoquant/src/symbol_scan.rs`（`ScanGates` 构造、`Candidate` 构造）
- 修改：`rust/nanoquant/src/cycle.rs`（测试 gate）
- 修改：`rust/nanoquant/src/param_registry.rs`、`default_settings.json`、前端参数页

**步骤：**

1. 搜索 `Candidate {` 和 `ScanGates {` 全部构造点。
2. `Candidate` 增加 `pub funding_rate: f64`（来源 `MarketSnapshot.funding_rate`）。
3. `ScanGates` 增加 `pub funding_veto_bps: f64`。
4. `risk_gate` 增加：
   ```rust
   if g.funding_veto_bps > 0.0 && c.funding_rate.abs() * 10000.0 > g.funding_veto_bps {
       if (c.preview_side == Some("long") && c.funding_rate > 0.0)
           || (c.preview_side == Some("short") && c.funding_rate < 0.0)
       {
           return format!("资金费率不利 {:.2}bps", c.funding_rate * 10000.0);
       }
   }
   ```
   注意：`Candidate` 没有方向字段，`preview_side` 只是审计方向；若为空，则只按绝对值报警，不做方向否决。实现时先确认候选构造处是否已有方向信息，没有就把方向判断放到 `runtime.rs` 的候选过滤处。
5. `scan_gates_of` 读取 `strategy.funding_veto_bps`，缺省 5.0。
6. 参数注册：`strategy.funding_veto_bps`，范围 0–50，步长 0.5，默认 5。
7. 测试：
   - `funding_rate_veto_blocks_expensive_long`
   - `funding_rate_veto_blocks_expensive_short`
   - `funding_rate_veto_ignores_reverse_side`
   - `funding_veto_zero_disables_filter`
8. 全量测试 + clippy，commit `[A1] feat(strategy): 增加资金费率否决过滤`。

---

## A2 事件否决

**文件：**
- 修改：`rust/nanoquant/src/runtime.rs`（候选过滤处）
- 新增/修改：`rust/nanoquant/src/news.rs`（最近负面新闻查询）
- 修改：参数注册与前端

**步骤：**

1. 先读 `news.rs` 确认表名/字段（如 `news_items`：title/summary/symbol/ts/sentiment）。
2. 新增函数：
   ```rust
   pub(crate) fn negative_event_hit(
       conn: &rusqlite::Connection,
       symbol: &str,
       window_min: i64,
   ) -> Option<String>
   ```
   - 按 `symbol` 或标题包含 symbol 匹配
   - 关键词：下架、暂停、黑客、清算、监管、诉讼、退市
   - 只取最近 `window_min` 分钟
3. `runtime.rs` 在候选评分后、AI 复核前调用；命中则 `reject`，原因写入 `reject_reason`。
4. 参数：`news.veto_enabled=true`、`news.veto_window_min=30`、`news.veto_max_items=3`。
5. 测试：
   - `negative_news_blocks_same_direction`
   - `negative_news_ignores_unrelated_symbol`
   - `news_veto_disabled_allows_entry`
6. 全量测试 + clippy，commit `[A2] feat(risk): 增加负面事件否决`。

---

## A3 组合风控

**文件：**
- 修改：`rust/nanoquant/src/runtime.rs`（`entry_allowed` 或等价检查链）
- 修改：参数注册与前端

**步骤：**

1. 先搜索 `fn entry_allowed` 确认签名与现有检查。
2. 追加：
   ```rust
   let same_dir = positions.iter().filter(|p| p.side == side).count();
   if same_dir >= cfg["max_same_direction"].as_i64().unwrap_or(2) {
       return Ok(Some("同方向持仓已达上限".into()));
   }
   let correlated = positions.iter()
       .filter(|p| p.side == side && correlated_symbols(&p.symbol, &symbol))
       .count();
   if correlated >= cfg["correlation_limit"].as_i64().unwrap_or(2) {
       return Ok(Some("高相关同方向持仓过多".into()));
   }
   ```
3. 新增静态分组函数：
   ```rust
   fn correlated_symbols(a: &str, b: &str) -> bool
   ```
   - major：BTC/ETH/SOL
   - 其余按 quote 资产或 category 前缀分组
4. 参数：`risk.correlation_limit=2`（1–5）、`risk.direction_exposure_limit_pct=200`（100–1000）。
5. 测试：同方向 2 仓拒绝、相关性 2 仓拒绝、反向不受限。
6. commit `[A3] feat(risk): 增加组合方向与相关性限制`。

---

## A4 半 AI 复核 + scale

**文件：**
- 修改：`ai_review.rs`、`review.rs`、`runtime.rs`、`ai_analysis.rs`
- 修改：参数注册与前端

**步骤：**

1. `review.rs`：
   - `ReviewDecision`/`ReviewedEntry` 增加 `scale: Option<f64>`。
   - `apply_review` 不再让 AI 覆盖 `leverage/stop_price/take_profit/side`；`veto_only=true` 时只采用 `action/confidence/scale`。
2. `ai_review.rs`：
   - `decision_to_review` 增加 `scale = (0.5 + 0.5 * confidence).clamp(0.5, 1.0)`。
   - `fill_default_valid_bars` 保留。
   - 增加 `fill_scale`。
3. `runtime.rs`：
   - `review_drafts` 的 `required` 改为读取 `ai.enabled && ai.require_agreement`。
   - `ai.enabled=false` 或复核失败时走 `apply_review(None, false)`，`scale=0.5`。
   - `estimate_qty` 得到 `qty` 后乘 `scale`，再按最小名义额校正。
4. `ai_analysis.rs`：
   - `veto_only=true` 时 prompt 只要求 `a/c/dq/rr/f`，不再要求 `l/m/s/t`。
5. 参数：`ai.veto_only=true`、`ai.scale_on_confidence=true`、`ai.fail_open_scale=0.5`。
6. 测试：
   - AI 关闭时量化照常开仓、仓位 ×0.5
   - AI 的杠杆/止损/方向不生效
   - scale 不作用于 margin_pct
7. commit `[A4] feat(ai): 复核改为否决与降仓模式`。

---

## A5 本地杠杆规则

**文件：**
- 修改：`runtime.rs`（`EntryDraft` 构造）
- 修改：参数注册与前端

**步骤：**

1. 新增：
   ```rust
   fn local_leverage(atr_pct: f64, score: f64, base: f64, vol_target: f64, score_weight: f64, min_lev: f64, max_lev: f64) -> f64 {
       let vol = if atr_pct > 0.0 { (vol_target / atr_pct).clamp(0.5, 2.0) } else { 1.0 };
       (base * vol + (score - 0.5) * score_weight).clamp(min_lev, max_lev).round()
   }
   ```
2. `EntryDraft.leverage` 从 `lower` 改为 `local_leverage(...)`。
3. 参数：`risk.local_leverage_base=10`、`risk.local_leverage_vol_target=2.0`、`risk.local_leverage_score_weight=10`。
4. 测试：低 ATR 高评分杠杆上升、高 ATR 杠杆下降、clamp 上下限。
5. commit `[A5] feat(risk): 本地规则决定杠杆`。

---

## B1 WebSocket 本地盘口

**文件：**
- 新增：`rust/nanoquant/src/book_stream.rs`
- 修改：`providers.rs`（snapshot markets 优先读本地 book）
- 修改：`main.rs`（启动任务）
- 修改：参数注册与前端

**步骤：**

1. 参考 `kline_stream.rs` 的连接/重连/共享结构。
2. `BookState { bid, ask, bid_qty, ask_qty, ts }`，全局 `Arc<RwLock<HashMap<String, BookState>>>`。
3. 订阅 `wss://fstream.binance.com/ws/<symbol>@bookTicker`，断线指数退避。
4. `providers.rs` 的 `snapshot()` 中，markets 的 bid/ask 优先取 BookState；`now - ts > book_stale_ms` 时回退 REST。
5. 启动时 REST `depth` 预热一次。
6. 参数：`data.book_ws_enabled=true`、`data.book_stale_ms=3000`。
7. 测试：book 新鲜时优先、stale 回退 REST、断线重连状态。
8. commit `[B1] feat(data): 增加 bookTicker 本地盘口`。

---

## C1 减少 AI 调用

**文件：**
- 修改：`runtime.rs`
- 修改：参数注册与前端

**步骤：**

1. 评分分流：`score >= high` 直接通过，`score < low` 直接拒绝，中间调 AI。
2. 内存缓存：
   ```rust
   struct ReviewCache { key: String, decision: ReviewDecision, ts: i64 }
   ```
   key = `symbol + direction + score_bucket`，TTL 60 秒。
3. 每轮扫描后异步预计算中间区候选。
4. 参数：`ai.high_score_threshold=0.75`、`ai.low_score_threshold=0.45`、`ai.review_cache_ttl_sec=60`。
5. 测试：高分区不调 AI、缓存命中不调 AI、低分区直接拒绝。
6. commit `[C1] feat(ai): 评分分流与复核缓存`。

---

## C2 Rust 异步执行

**文件：**
- 修改：`ai_review.rs`（`spawn_blocking` 包装）
- 修改：`runtime.rs`（并发复核）
- 修改：`Cargo.toml`（如需 `futures`）

**步骤：**

1. 保留同步 `AiReviewer` trait，避免大规模改测试 mock。
2. `EngineAiReviewer::review` 改为在 `spawn_blocking` 中执行 `review_entry`，超时 30 秒。
3. `review_drafts` 改为并发收集：`futures::future::join_all`，最多 8 并发。
4. 复核失败按 fail-open（A4 已实现）降级。
5. 测试：并发复核不阻塞、超时降级、结果顺序一致。
6. commit `[C2] feat(ai): 复核异步并发执行`。

---

## D1 均值回归（单品种）

**文件：**
- 修改：`strategy.rs` / `scan.rs`（新信号源）
- 修改：`runtime.rs`（`regime=range` 路由）
- 修改：参数注册与前端

**步骤：**

1. 信号：RSI 超买/超卖 + 布林带偏离 + 量能收敛。
2. 只在 `regime=range` 时启用。
3. 独立止损止盈：`sl_pct=20`、`tp_pct=30`（保证金口径）。
4. 参数：`strategy.mr_enabled=true`、`mr_rsi_high=70`、`mr_rsi_low=30`、`mr_band_k=2.0`。
5. 回测验收：range 段样本外期望值 > 0，最大回撤 < 阈值。
6. commit `[D1] feat(strategy): 增加单品种均值回归`。

---

## D2 RL 执行优化

**文件：**
- 新增：`rust/nanoquant/src/rl_exec.rs`
- 修改：`runtime.rs`（执行层可选调用）
- 新增：离线回放工具（bin 或测试）

**步骤：**

1. 状态：盘口不平衡、spread、波动率、剩余时间、订单大小。
2. 动作：`market` / `limit(offset)` / `wait` / `split(n)`。
3. 奖励：`-slippage - fee - opportunity_cost`。
4. 算法：轻量表格 Q-learning（避免重型深度学习依赖）。
5. 训练：历史盘口/成交回放，离线生成 Q 表。
6. 实盘：仅 `rl_exec.enabled=true` 时启用，只影响下单时机，不进入方向/杠杆/止损。
7. 参数：`rl_exec.enabled=false`、`rl_exec.epsilon=0.05`、`rl_exec.max_wait_ms=3000`。
8. 验收：回放滑点相对基线下降 ≥10%，实盘小资金验证 2 周。
9. commit `[D2] feat(exec): 增加 RL 执行优化`。

---

## D3 GA 离线参数搜索

**文件：**
- 新增：`rust/nanoquant/src/ga_tune.rs`
- 新增：CLI/后台任务入口
- 修改：证据门调用

**步骤：**

1. 基因：`entry_threshold`、`atr_pct_min`、`atr_pct_max`、`volume_ratio_min`、`funding_veto_bps`。
2. 适应度：walk-forward 期望值 - 最大回撤惩罚 - 换手惩罚。
3. 算子：锦标赛选择、单点交叉、高斯变异。
4. 输出候选参数集，写入 `ai_tuning` 候选表，走证据门。
5. `auto_apply=false`，人工确认。
6. 验收：样本外不劣于基线，参数不越界。
7. commit `[D3] feat(tuning): 增加 GA 离线参数搜索`。

---

## E1 清理

**删除/删减：**

- `review.rs` 中 AI 覆盖 `leverage/stop_price/take_profit/side` 的消费路径（A4 已改，清理残留分支）。
- `ai_analysis.rs` 中 `veto_only` 下不再使用的 `l/m/s/t` 说明。
- 前端 `AiView` 中已失效的「AI 决定杠杆」文案。
- 未使用的 `ReviewDecision` 字段（若确认无消费）。
- 冗余参数（AI 调参旧键、已舍去策略的键）。

**步骤：**

1. 全仓 `grep` 确认无消费。
2. 删除并跑全量测试。
3. commit `[E1] chore(ai): 清理 AI 覆盖路径与冗余参数`。

---

## 验收总表

| 项 | 断言 |
|---|---|
| A1 | 资金费率超阈值时同向被拒，反向放行 |
| A2 | 负面新闻命中时同向被拒 |
| A3 | 同方向/相关性达上限时拒绝 |
| A4 | AI 关闭时量化照开、仓位 ×0.5；AI 杠杆/止损/方向不生效 |
| A5 | 杠杆按 ATR/评分变化，clamp 上下限 |
| B1 | 盘口新鲜 ≤100ms；stale 回退 REST |
| C1 | 高/低分区不调 AI；缓存命中不调 AI |
| C2 | 复核并发不阻塞；超时降级 |
| D1 | range 段样本外期望值 > 0 |
| D2 | 回放滑点下降 ≥10% |
| D3 | 样本外不劣于基线 |
| E1 | 无死代码、无未消费字段、全量测试通过 |

---

## 参考

- Freqtrade：funding rate 过滤、策略/风控分层
- Binance Futures：`bookTicker` / `depth` WebSocket
- Tokio：`spawn_blocking` 与并发任务
- Almgren-Chriss / Q-learning：执行成本与下单时机