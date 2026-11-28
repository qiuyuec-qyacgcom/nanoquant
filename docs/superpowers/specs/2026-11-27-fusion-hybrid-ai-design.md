# 合约 CTA 半 AI 融合方案

日期：2026-11-27
状态：待用户审查

## 1. 目标与边界

目标：把项目收敛为「合约方向性 CTA + 规则风控 + 半 AI 过滤器 + 低延迟执行」，同时完成三项延迟优化（WebSocket 本地盘口、减少 AI 调用、Rust 异步执行）。

半 AI 的硬边界：

- AI 不决定方向、杠杆、止损价、止盈价
- AI 只输出 `approve / reject / scale(0.5–1.0)`
- AI 不可用时 fail-open 到纯量化，仓位 ×0.5
- AI 不参与执行下单，只做决策前过滤

## 2. 范围决策

| 类型 | 决策 | 阶段 |
|---|---|---|
| 趋势/突破 CTA | 保留 | P0（已有） |
| 均值回归（单品种） | 保留 | P1 |
| 资金费率过滤 | 保留 | P0 |
| 事件否决 | 保留 | P0 |
| 组合风控 | 保留 | P0 |
| 半 AI 否决/降仓 | 保留 | P0 |
| WebSocket 本地盘口 | 保留 | P0 |
| 减少 AI 调用 | 保留 | P0 |
| Rust 异步执行 | 保留 | P0 |
| RL 执行优化 | 保留 | P1 |
| GA 离线参数搜索 | 保留 | P1 |
| 网格 | 舍去 | — |
| 配对统计套利 | 舍去 | — |
| 做市 / 跨所 / 期现套利 | 舍去 | — |
| 跟单 / HFT / 链上主策略 | 舍去 | — |

## 3. 目标架构

```
L0 数据：K线 WS（已有）+ bookTicker WS（新增）+ funding/OI/news
L1 Regime：trend / range / chaos
L2 信号：趋势/突破（现有）；range 均值回归（P1）
L3 硬风控：波动、点差、量能、成交额、仓位、杠杆、冷却
L4 本地杠杆：按 ATR/评分计算，不依赖 AI
L5 半 AI：approve/reject/scale；不可用降级
L6 执行：异步下单、保护单、对账
L7 组合：方向暴露、类别暴露、相关性
L8 回测/证据门/A-B
L9 审计/复盘
```

## 4. P0 设计

### 4.1 半 AI 复核

改动：

- `ai_client.rs`：`Decision` 只保留 `action`、`confidence`、`reason`、`risk_flags`、`invalidation` 与审计字段；`leverage/margin_pct/sl_pct/tp_pct` 已删除
- `review.rs`：`ReviewDecision` 新增 `scale: Option<f64>`；`apply_review` 固定使用量化提案的方向/杠杆/保证金/止损/止盈
- `ai_analysis.rs`：system prompt 固定只要求 `a/c/dq/rr/f`（可选 why/risk/inv），不再要求 `l/m/s/t`
- `ai_review.rs`：`decision_to_review` 中：
  - AI 只能同意/否决/降仓，方向/杠杆/保证金/止损/止盈全部取量化提案
  - `scale = (0.5 + 0.5 * confidence).clamp(0.5, 1.0)`
  - `reject` 只由 `action=H/C`、方向冲突、`dq/rr` 门槛触发
- `runtime.rs`：`estimate_qty` 得到 `qty` 后乘 `reviewed.scale.unwrap_or(1.0)`，再按最小名义额校正；AI 不可用时 `scale=0.5`。`scale` 不作用于 `margin_pct`，避免被 15–25% 下限抵消。

新增参数：

| 参数 | 默认 | 范围 |
|---|---:|---|
| `ai.scale_on_confidence` | true | bool |
| `ai.fail_open_scale` | 0.5 | 0.1–1.0 |

AI 复核固定启用，仅受 `ai.enabled` 总开关控制；不再有 `require_agreement` / `veto_only` 独立开关。

### 4.2 本地杠杆规则

半 AI 后 AI 不再决定杠杆，需要本地规则，避免全部卡在 `min_leverage`。

`runtime.rs` 的 `EntryDraft` 构造处：

```rust
fn local_leverage(atr_pct: f64, score: f64, min_lev: f64, max_lev: f64) -> f64 {
    let vol = if atr_pct > 0.0 { (2.0 / atr_pct).clamp(0.5, 2.0) } else { 1.0 };
    let base = 10.0 * vol;
    let scored = base + (score - 0.5) * 10.0;
    scored.clamp(min_lev, max_lev).round()
}
```

参数：

| 参数 | 默认 | 范围 |
|---|---:|---|
| `risk.local_leverage_base` | 10 | 1–25 |
| `risk.local_leverage_vol_target` | 2.0 | 0.5–10 |
| `risk.local_leverage_score_weight` | 10 | 0–20 |

### 4.3 资金费率过滤

`scan.rs` 的 `risk_gate` 增加：

```rust
if g.funding_veto_bps > 0.0 && c.funding_rate.abs() * 10000.0 > g.funding_veto_bps {
    if (c.side == "long" && c.funding_rate > 0.0) || (c.side == "short" && c.funding_rate < 0.0) {
        return format!("资金费率不利 {:.2}bps", c.funding_rate * 10000.0);
    }
}
```

参数：`strategy.funding_veto_bps`，默认 5，范围 0–50。

数据来源：`MarketSnapshot.funding_rate` 已存在（`journal_snapshot` 已写入）。

### 4.4 事件否决

`runtime.rs` 的候选评分后、AI 复核前增加：

- 读取最近 `news` 中与标的相关的负面事件（关键词表：下架、暂停、黑客、清算、监管）
- 命中则拒绝同向开仓，理由写入 `reject_reason`

参数：

| 参数 | 默认 | 范围 |
|---|---|---|
| `news.veto_enabled` | true | bool |
| `news.veto_window_min` | 30 | 5–240 |
| `news.veto_max_items` | 3 | 1–10 |

### 4.5 WebSocket 本地盘口

新增 `rust/nanoquant/src/book_stream.rs`：

- 订阅 `wss://fstream.binance.com/ws/<symbol>@bookTicker`
- 维护 `BookState { bid, ask, bid_qty, ask_qty, ts }`
- 断线指数退避重连
- REST `depth` 兜底，启动时预热一次

`providers.rs` 的 `snapshot()` 中，markets 的 bid/ask 优先取本地 `BookState`，超过 3 秒未更新则回退 REST。

参数：

| 参数 | 默认 | 范围 |
|---|---:|---|
| `data.book_ws_enabled` | true | bool |
| `data.book_stale_ms` | 3000 | 500–30000 |

验收：盘口新鲜度 ≤100ms；点差过滤仍生效。

### 4.6 减少 AI 调用

`runtime.rs` 的 `review_drafts` 前增加评分分流：

```rust
// score >= high：直接通过；score < low：直接拒绝；中间才调 AI
let high = cfg["ai_high_score"].as_f64().unwrap_or(0.75);
let low = cfg["ai_low_score"].as_f64().unwrap_or(0.45);
```

- 高分区：`apply_review(None, false)`
- 低分区：直接 `Reject`
- 中间区：调 AI

异步预计算：

- 每轮扫描结束后，对中间区候选并发调用 AI，结果写入内存缓存
- 缓存 key：`symbol + direction + score_bucket`
- TTL 60 秒；命中则不再调用

参数：

| 参数 | 默认 | 范围 |
|---|---:|---|
| `ai.high_score_threshold` | 0.75 | 0.5–1.0 |
| `ai.low_score_threshold` | 0.45 | 0.0–0.7 |
| `ai.review_cache_ttl_sec` | 60 | 0–600 |

### 4.7 Rust 异步执行

- `ai_review.rs`：把 `block_in_place + block_on` 改为 `tokio::task::spawn_blocking`，由 `apply_cycle` 的 async 段统一 `join_all`，不阻塞主循环
- `runtime.rs`：`refresh_snapshot` 与 `review` 并行；执行下单继续用 `reqwest` 异步连接池
- 移除复核里的 2 秒锁等待重试，改为异步队列串行化 DB 读取

验收：复核不再阻塞行情/持仓任务；下单请求排队 ≤1 个。

### 4.8 组合风控

现有参数已具备部分能力：`risk.max_positions`、`risk.max_same_direction`、`risk.category_exposure_limit_pct`、`risk.max_exposure_pct`。

新增：

- 同方向高相关标的合计仓位上限（BTC/ETH 等）
- 候选开仓前的相关性检查

`runtime.rs` 的 `entry_allowed` 增加：

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

参数：

| 参数 | 默认 | 范围 |
|---|---:|---|
| `risk.correlation_limit` | 2 | 1–5 |
| `risk.direction_exposure_limit_pct` | 200 | 100–1000 |

相关性先用静态分组（BTC/ETH/SOL 归为 major，其余按 category），不做动态相关系数矩阵。

## 5. P1 设计

### 5.1 均值回归（单品种）

- `regime=range` 时启用
- 信号：RSI 超买/超卖 + 布林带偏离 + 量能收敛
- 必须硬止损：`sl_pct=20`（保证金口径），`tp_pct=30`
- 独立参数组：`strategy.mr_rsi_high/low`、`strategy.mr_band_k`

### 5.2 RL 执行优化

- 只做执行层：下单时机、拆单、限价偏移
- 环境：离线回放 + 小资金实盘
- 输出：`wait / market / limit(offset) / split(n)`
- 奖励：`-滑点 - 手续费 - 机会成本`
- 不进入方向/杠杆/止损链路
- 先离线验证 3 个月，再考虑实盘

### 5.3 GA 离线参数搜索

- 只做离线 walk-forward
- 适应度：期望值、最大回撤、换手
- 结果走证据门（30 笔 OOS、bootstrap、压力测试）
- AI/GA 候选只写入 `ai_tuning`；证据门通过后自动激活（跨 regime 候选等待切换后生效）

## 6. 参数与默认值

| 组 | 参数 | 默认 |
|---|---|---:|

| ai | scale_on_confidence | true |
| ai | fail_open_scale | 0.5 |
| ai | high_score_threshold | 0.75 |
| ai | low_score_threshold | 0.45 |
| ai | review_cache_ttl_sec | 60 |
| risk | local_leverage_base | 10 |
| risk | local_leverage_vol_target | 2.0 |
| risk | local_leverage_score_weight | 10 |
| risk | correlation_limit | 2 |
| risk | direction_exposure_limit_pct | 200 |
| strategy | funding_veto_bps | 5 |
| news | veto_enabled | true |
| news | veto_window_min | 30 |
| news | veto_max_items | 3 |
| data | book_ws_enabled | true |
| data | book_stale_ms | 3000 |

全部登记到 `param_registry.rs`、`default_settings.json`、前端参数页。

## 7. 数据流

```
bookTicker WS ─┐
K线 WS ────────┼→ 本地行情/盘口 → 扫描 → 评分 → 资金费率/事件过滤
REST 兜底 ─────┘                                   ↓
                                          高分区 → 直接开仓
                                          低分区 → 拒绝
                                          中分区 → AI 异步复核 → scale
                                                   ↓
                                          本地杠杆 + 组合风控 → 异步下单
```

## 8. 验收断言

- AI 关闭或不可用时，量化信号按 `scale=0.5` 开仓，不再全部拒单
- AI 不再输出杠杆/止损/方向字段；量化提案始终生效
- 资金费率超过阈值时，同向信号被拒，反向不受影响
- 负面新闻命中时，同向信号被拒，原因可查询
- 盘口超过 `book_stale_ms` 时自动回退 REST
- 高分区候选不再调用 AI；中间区命中缓存时不调用
- 复核阻塞时间从平均 9.2 秒降到不影响主循环
- 端到端延迟（行情→下单）目标 ≤300ms（AI 异步时）
- 全量测试通过，clippy 无警告

## 9. 分期与代价

| 期 | 内容 | 代价 |
|---|---|---|
| P0 | 半 AI、本地杠杆、资金费率、事件否决、组合风控、三项延迟 | 改动集中，需重新校准 AI scale |
| P1 | 均值回归、RL 执行、GA 搜索 | 独立回测与证据门，周期长 |
| P2 | 观察/研究 | 不上实盘 |

主要风险：

- 半 AI 后杠杆由本地规则决定，可能与旧行为差异大，需要小资金验证
- 本地盘口需要处理断线/乱序，REST 兜底必须保留
- 事件否决依赖 news 质量，误杀需要可回滚开关
- AI 调用减少后，低分区误杀可能增加，需要 A/B 验证

## 10. 非目标

- 不做 HFT/FPGA
- 不做网格、配对套利、做市、跨所套利
- 不让 AI 决定方向/杠杆/止损
- GA/AI 候选必须先通过证据门，通过后自动激活，不绕过回测门