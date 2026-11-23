# B 期：回测证据门实现计划

> **面向 AI 代理的工作者：** 必需子技能：使用 superpowers:subagent-driven-development（推荐）或 superpowers:executing-plans 逐任务实现此计划。步骤使用复选框（`- [ ]`）语法来跟踪进度。

**目标：** 让候选参数集通过“标的池固定参数 walk-forward + 目标 market regime 过滤 + 成本压力 + bootstrap”的证据门，通过后才进入 `approved`；B 期不写 `app_settings.strategy`。

**架构：** `TradePolicy` 接管 ATR 上下界；`BacktestTrade` 记录开仓时 `market_regime`；新增 `run_walkforward_basket_fixed` 按启用标的池聚合 OOS 交易；`evidence.rs` 独立判定阈值；调参任务在 candidate 落库后异步跑证据门。

**技术栈：** Rust 2021、rusqlite、serde_json、tokio、现有 `backtest_engine` / `history`。

**前置阅读：** `docs/superpowers/specs/2026-11-23-parameter-set-governance-design.md`（尤其 §17 A 期实施记录）与 A 期计划 `docs/superpowers/plans/2026-11-23-parameter-sets-a-decouple-tuning.md`。A 期必须已完成。

**A 期实施后的接口变化（B 期必须适配）：**
- `parameter_sets::active_for_regime(conn, regime) -> Result<Option<ParameterSet>, String>`，不是 Option。
- regime 诊断键为 `engine_state['market_regime.{mode}']` / `symbol_regime.{mode}`；先读 `engine_state.mode`。
- `settings_api::apply_param_updates` 已删除；入口复核不再调参。
- `tuning.rs` 已存在，`run_once` 生成 candidate；B 期在其后异步跑证据门。
- `parameter_sets::insert_candidate` 已校验父集/timeframe；证据门只做 `mark_evidence`/状态迁移。
- `strategy.timeframe` 线上 1h；v1 自动切换只接受 1h。

---

## 文件结构与职责

- 修改：`rust/nanoquant/src/trade_policy.rs` — `TradePolicy` 增加 ATR 上下界，`evaluate` 使用 policy 值。
- 修改：`rust/nanoquant/src/runtime.rs` — `trade_policy()` 读写 `strategy.atr_pct_min/max`。
- 修改：`rust/nanoquant/src/backtest_engine.rs` — `BacktestTrade.market_regime`、`run_walkforward_basket_fixed`、`apply_params` 扩展。
- 创建：`rust/nanoquant/src/evidence.rs` — 证据门判定与证据 JSON。
- 修改：`rust/nanoquant/src/tuning.rs` — candidate 落库后触发证据任务（或新增独立 `evidence_task`）。
- 创建：`rust/nanoquant/src/parameter_sets_api.rs` — 只读参数集/证据 API。
- 修改：`rust/nanoquant/src/main.rs` — 模块与路由挂载。
- 测试：各模块测试 + `evidence.rs` 单元测试。

---

## 任务 1：ATR 口径统一到 TradePolicy

**文件：**
- 修改：`rust/nanoquant/src/trade_policy.rs:1-8, 184-234`
- 修改：`rust/nanoquant/src/runtime.rs:1162-1198`
- 修改：`rust/nanoquant/src/backtest.rs:448-468`（无需改，复用 `trade_policy`）
- 测试：`trade_policy.rs`、`runtime.rs`

- [ ] **步骤 1：编写失败测试**

在 `trade_policy.rs` 添加：

```rust
#[test]
fn evaluate_uses_policy_atr_bounds() {
    let candles = fixture_with_atr_pct(1.5);
    let policy = TradePolicy {
        base_threshold: 0.0,
        volume_ratio_min: 0.0,
        sl_atr_mult: 2.0,
        tp_atr_mult: 4.0,
        min_risk_reward: 2.0,
        atr_pct_min: 1.4,
        atr_pct_max: 1.6,
    };
    assert!(evaluate(&candles, 100.0, 1.5, policy, MarketFeatures::default()).is_some());

    let too_low = TradePolicy { atr_pct_max: 1.6, ..policy };
    let candles_low = fixture_with_atr_pct(0.8);
    assert!(evaluate(&candles_low, 100.0, 0.8, too_low, MarketFeatures::default()).is_none());
}
```

`fixture_with_atr_pct` 用现有测试里的 K 线构造方式生成 80 根、ATR% 已知的数据；若现有测试已有 `candles()` 辅助函数，复用它并调整 ATR。

- [ ] **步骤 2：运行测试确认失败**

```bash
cargo test --manifest-path rust/nanoquant/Cargo.toml evaluate_uses_policy_atr_bounds -- --nocapture
```

预期：编译失败，`TradePolicy` 没有 ATR 字段。

- [ ] **步骤 3：实现**

`TradePolicy` 增加：

```rust
pub atr_pct_min: f64,
pub atr_pct_max: f64,
```

`evaluate` 把：

```rust
if !(0.05..=4.0).contains(&core.atr_pct) {
    return None;
}
```

改为：

```rust
if core.atr_pct < policy.atr_pct_min || core.atr_pct > policy.atr_pct_max {
    return None;
}
```

`runtime::trade_policy()`：

```rust
atr_pct_min: num(strategy, "atr_pct_min", 0.05),
atr_pct_max: num(strategy, "atr_pct_max", 4.0),
```

`backtest_engine` 的 `EngineConfig` 不需要新增字段，因为它已有 `policy`。

同步更新所有 `TradePolicy { ... }` 构造（搜索 `TradePolicy {`），补齐两个字段。默认测试夹具使用 `0.05 / 4.0`，保持现有行为。

- [ ] **步骤 4：运行测试确认通过**

```bash
cargo test --manifest-path rust/nanoquant/Cargo.toml evaluate_uses_policy_atr_bounds -- --nocapture
cargo test --manifest-path rust/nanoquant/Cargo.toml trade_policy::tests:: -- --nocapture
```

预期：PASS。

- [ ] **步骤 5：Commit**

```bash
git add rust/nanoquant/src/trade_policy.rs rust/nanoquant/src/runtime.rs
git commit -m "feat(策略): 统一 ATR 上下界到 TradePolicy"
```

---

## 任务 2：BacktestTrade 记录 market_regime，新增标的池固定参数 walk-forward

**文件：**
- 修改：`rust/nanoquant/src/backtest_engine.rs:45-63, 180-200, 726-1023`
- 测试：`backtest_engine.rs`

- [ ] **步骤 1：编写失败测试**

```rust
#[test]
fn basket_fixed_filters_target_market_regime_and_aggregates() {
    let basket = synthetic_basket(6, 700);
    let config = fixture_config();
    let metrics = run_walkforward_basket_fixed(
        &basket,
        &config,
        2,
        300,
        crate::regime::Regime::Range,
        "1h",
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(metrics.target_regime, "range");
    assert!(metrics.symbols_used.len() >= 6);
    assert!(metrics.folds >= 2);
    assert!(metrics.oos_trades > 0);
    assert!(metrics.max_drawdown_pct >= 0.0);
}

#[test]
fn basket_fixed_rejects_insufficient_coverage() {
    let basket = synthetic_basket(5, 700);
    let result = run_walkforward_basket_fixed(
        &basket,
        &fixture_config(),
        2,
        300,
        crate::regime::Regime::Range,
        "1h",
        &AtomicBool::new(false),
    );
    assert!(result.is_err());
}
```

`synthetic_basket` 生成带 `HistoryBar` 的 6/5 个标的；每根 K 线有递增/震荡特征，确保 regime 序列有值。

- [ ] **步骤 2：运行测试确认失败**

```bash
cargo test --manifest-path rust/nanoquant/Cargo.toml basket_fixed_filters_target_market_regime_and_aggregates -- --nocapture
```

预期：编译失败。

- [ ] **步骤 3：实现**

`BacktestTrade` 增加：

```rust
pub market_regime: String,
```

在 `Sim` 构造 `BacktestTrade` 时初始化：

```rust
market_regime: String::new(),
```

新增类型：

```rust
#[derive(Debug, Clone, Default)]
pub struct BasketMetrics {
    pub folds: usize,
    pub oos_trades: usize,
    pub positive_folds: usize,
    pub oos_avg_return_pct: f64,
    pub overfit_gap_pp: f64,
    pub stressed_avg_return_pct: f64,
    pub max_drawdown_pct: f64,
    pub bootstrap_ci_low: f64,
    pub bootstrap_p_positive: f64,
    pub symbols_used: Vec<String>,
    pub symbols_skipped: Vec<String>,
    pub target_regime: String,
    pub timeframe: String,
}
```

新增函数：

```rust
pub fn run_walkforward_basket_fixed(
    basket: &[(String, Vec<HistoryBar>)],
    base: &EngineConfig,
    folds: i64,
    train_bars: i64,
    target_regime: crate::regime::Regime,
    timeframe: &str,
    cancel: &AtomicBool,
) -> Result<BasketMetrics, String> {
    if basket.len() < 6 {
        return Err("有效标的少于 6 个".to_string());
    }
    let requested_folds = folds.clamp(MIN_FOLDS as i64, MAX_FOLDS as i64) as usize;
    let regime_map = build_market_regime_map(basket, timeframe);
    let mut all_oos: Vec<f64> = Vec::new();
    let mut all_is: Vec<f64> = Vec::new();
    let mut all_stressed: Vec<f64> = Vec::new();
    let mut fold_pnl = vec![0.0_f64; requested_folds];
    let mut used = Vec::new();
    let mut skipped = Vec::new();

    for (symbol, bars) in basket {
        if bars.len() < 2000 {
            skipped.push(symbol.clone());
            continue;
        }
        used.push(symbol.clone());
        let candles = candles_from_history(bars);
        let series = crate::regime::regime_series(&candles, 14);
        let train_bars = (train_bars as usize).clamp(WARMUP * 2, bars.len().saturating_sub(WARMUP * 2));
        let mut step = (bars.len() - WARMUP - train_bars) / requested_folds;
        if step <= WARMUP {
            skipped.push(symbol.clone());
            continue;
        }
        for index in 0..requested_folds {
            if cancel.load(Ordering::Relaxed) {
                return Err(CANCELLED.to_string());
            }
            let train_end = WARMUP + train_bars + index * step;
            let test_end = (train_end + step).min(bars.len());
            let train = &bars[train_end.saturating_sub(train_bars)..train_end];
            let test = &bars[train_end.saturating_sub(WARMUP)..test_end];
            if train.len() <= WARMUP || test.len() <= WARMUP {
                continue;
            }
            let train_result = run(train, base)?;
            let test_result = run(test, base)?;
            let mut stressed_config = base.clone();
            stressed_config.fee_bps *= 2.0;
            stressed_config.slippage_bps *= 2.0;
            let stressed = run(test, &stressed_config)?;

            let target = target_regime.as_str();
            let is_returns = tag_and_filter(&train_result.trades, target, &regime_map, &series);
            let oos_returns = tag_and_filter(&test_result.trades, target, &regime_map, &series);
            let stressed_returns = tag_and_filter(&stressed.trades, target, &regime_map, &series);
            all_is.extend(is_returns.iter().copied());
            all_oos.extend(oos_returns.iter().copied());
            all_stressed.extend(stressed_returns.iter().copied());
            fold_pnl[index] += oos_returns.iter().sum::<f64>();
        }
    }
    if used.len() < 6 {
        return Err("有效标的少于 6 个".to_string());
    }
    if all_oos.len() < 30 {
        return Err("目标 regime 的 OOS 成交少于 30 笔".to_string());
    }
    let positive_folds = fold_pnl.iter().filter(|value| **value > 0.0).count();
    let avg = |values: &[f64]| values.iter().sum::<f64>() / values.len() as f64;
    let oos_avg = avg(&all_oos);
    let is_avg = if all_is.is_empty() { oos_avg } else { avg(&all_is) };
    let stressed_avg = avg(&all_stressed);
    let mut sorted = all_oos.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mut equity = 0.0_f64;
    let mut peak = 0.0_f64;
    let mut max_dd = 0.0_f64;
    for value in &sorted {
        equity += value;
        peak = peak.max(equity);
        if peak > 0.0 {
            max_dd = max_dd.max((peak - equity) / peak * 100.0);
        }
    }
    let bootstrap = bootstrap_ci(&all_oos);
    Ok(BasketMetrics {
        folds: requested_folds,
        oos_trades: all_oos.len(),
        positive_folds,
        oos_avg_return_pct: round(oos_avg, 4),
        overfit_gap_pp: round(is_avg - oos_avg, 4),
        stressed_avg_return_pct: round(stressed_avg, 4),
        max_drawdown_pct: round(max_dd, 4),
        bootstrap_ci_low: bootstrap["ci_low"].as_f64().unwrap_or_default(),
        bootstrap_p_positive: bootstrap["p_positive"].as_f64().unwrap_or_default(),
        symbols_used: used,
        symbols_skipped: skipped,
        target_regime: target_regime.as_str().to_string(),
        timeframe: timeframe.to_string(),
    })
}
```

辅助函数（签名固定，按以下实现）：

```rust
fn candles_from_history(bars: &[HistoryBar]) -> Vec<crate::providers::CandleSnapshot>;
fn build_market_regime_map(
    basket: &[(String, Vec<HistoryBar>)],
    timeframe: &str,
) -> std::collections::BTreeMap<i64, crate::regime::Regime>;
fn market_regime_at(
    map: &std::collections::BTreeMap<i64, crate::regime::Regime>,
    ts: i64,
) -> Option<crate::regime::Regime>;
fn tag_and_filter(
    trades: &[BacktestTrade],
    target: &str,
    map: &std::collections::BTreeMap<i64, crate::regime::Regime>,
) -> Vec<f64>;
```

`candles_from_history` 映射 `open/close/high/low/volume/close_time`；`build_market_regime_map` 对每个标的算 `regime_series`，按 `close_time` 生成桶，至少 6 个有效标的、等权多数、平票 `range`；`tag_and_filter` 对每笔 `entry_ts` 调 `market_regime_at`，只返回目标 regime 的 `pnl_pct`。`bootstrap_ci` 已有，直接复用。

**注意：** `BacktestTrade` 是 `Serialize`；新增字段会出现在现有 API 输出中，这是兼容的追加字段。

- [ ] **步骤 4：运行测试确认通过**

```bash
cargo test --manifest-path rust/nanoquant/Cargo.toml basket_fixed -- --nocapture
```

预期：PASS。

- [ ] **步骤 5：Commit**

```bash
git add rust/nanoquant/src/backtest_engine.rs
git commit -m "feat(回测): 增加标的池固定参数 walk-forward 与 market regime 过滤"
```

---

## 任务 3：证据门判定模块

**文件：**
- 创建：`rust/nanoquant/src/evidence.rs`
- 修改：`rust/nanoquant/src/main.rs`（`mod evidence;`）
- 测试：`evidence.rs`

- [ ] **步骤 1：编写失败测试**

```rust
fn metrics() -> BasketMetrics {
    BasketMetrics {
        folds: 3,
        oos_trades: 30,
        positive_folds: 2,
        oos_avg_return_pct: 0.4,
        overfit_gap_pp: 0.5,
        stressed_avg_return_pct: 0.1,
        max_drawdown_pct: 10.0,
        bootstrap_ci_low: 0.01,
        bootstrap_p_positive: 0.95,
        symbols_used: vec!["BTCUSDT".into(), "ETHUSDT".into(), "SOLUSDT".into(), "BNBUSDT".into(), "ARBUSDT".into(), "NEARUSDT".into()],
        symbols_skipped: vec![],
        target_regime: "range".into(),
        timeframe: "1h".into(),
    }
}

#[test]
fn evidence_gate_passes_complete_candidate() {
    let report = evaluate_gate(&metrics(), 30, "1h", &["1h"], Some(0.2));
    assert!(report.pass, "{:?}", report.reasons);
}

#[test]
fn evidence_gate_rejects_short_sample_ci_and_wrong_timeframe() {
    let mut m = metrics();
    m.oos_trades = 29;
    assert!(!evaluate_gate(&m, 30, "1h", &["1h"], Some(0.2)).pass);
    let mut m = metrics();
    m.bootstrap_ci_low = 0.0;
    assert!(!evaluate_gate(&m, 30, "1h", &["1h"], Some(0.2)).pass);
    let mut m = metrics();
    m.timeframe = "4h".into();
    assert!(!evaluate_gate(&m, 30, "1h", &["1h"], Some(0.2)).pass);
}
```

- [ ] **步骤 2：运行测试确认失败**

```bash
cargo test --manifest-path rust/nanoquant/Cargo.toml evidence_gate -- --nocapture
```

预期：编译失败。

- [ ] **步骤 3：实现**

```rust
use serde_json::{json, Value};
use crate::backtest_engine::BasketMetrics;

pub(crate) struct EvidenceReport {
    pub pass: bool,
    pub reasons: Vec<String>,
}

pub(crate) fn evaluate_gate(
    metrics: &BasketMetrics,
    min_oos_trades: usize,
    live_timeframe: &str,
    auto_apply_timeframes: &[&str],
    baseline_oos_avg: Option<f64>,
) -> EvidenceReport {
    let mut reasons = Vec::new();
    if metrics.timeframe != live_timeframe {
        reasons.push(format!("timeframe 不匹配: {} != {live_timeframe}", metrics.timeframe));
    }
    if !auto_apply_timeframes.contains(&metrics.timeframe.as_str()) {
        reasons.push(format!("timeframe {} 不在自动切换白名单", metrics.timeframe));
    }
    if metrics.oos_trades < min_oos_trades {
        reasons.push(format!("OOS 成交不足: {} < {min_oos_trades}", metrics.oos_trades));
    }
    if metrics.folds < 2 || metrics.positive_folds * 2 < metrics.folds {
        reasons.push(format!("盈利折数不足: {}/{}", metrics.positive_folds, metrics.folds));
    }
    if metrics.oos_avg_return_pct <= 0.0 {
        reasons.push("OOS 平均单笔收益 <= 0".to_string());
    }
    if metrics.overfit_gap_pp > 1.0 {
        reasons.push(format!("过拟合差距过大: {}pp > 1.0pp", metrics.overfit_gap_pp));
    }
    if metrics.bootstrap_ci_low <= 0.0 {
        reasons.push("bootstrap ci_low <= 0".to_string());
    }
    if metrics.bootstrap_p_positive < 0.90 {
        reasons.push(format!("bootstrap p_positive < 0.90: {}", metrics.bootstrap_p_positive));
    }
    if metrics.stressed_avg_return_pct < 0.0 {
        reasons.push("2 倍成本压力后收益 < 0".to_string());
    }
    if metrics.max_drawdown_pct > 15.0 {
        reasons.push(format!("最大回撤过大: {}% > 15%", metrics.max_drawdown_pct));
    }
    if let Some(baseline) = baseline_oos_avg {
        if metrics.oos_avg_return_pct - baseline < 0.10 {
            reasons.push(format!("相对基线优势不足: {:.4}pp < 0.10pp", metrics.oos_avg_return_pct - baseline));
        }
    }
    EvidenceReport { pass: reasons.is_empty(), reasons }
}

pub(crate) fn evidence_json(metrics: &BasketMetrics, report: &EvidenceReport, baseline_set_id: Option<i64>) -> Value {
    json!({
        "version": 1,
        "generated_at": crate::now(),
        "data": {
            "timeframe": metrics.timeframe,
            "target_regime": metrics.target_regime,
            "symbols_used": metrics.symbols_used,
            "symbols_skipped": metrics.symbols_skipped,
            "folds": metrics.folds,
        },
        "baseline_set_id": baseline_set_id,
        "candidate": {
            "oos_trades": metrics.oos_trades,
            "positive_folds": metrics.positive_folds,
            "oos_avg_return_pct": metrics.oos_avg_return_pct,
            "overfit_gap_pp": metrics.overfit_gap_pp,
            "stressed_avg_return_pct": metrics.stressed_avg_return_pct,
            "max_drawdown_pct": metrics.max_drawdown_pct,
            "bootstrap_ci_low": metrics.bootstrap_ci_low,
            "bootstrap_p_positive": metrics.bootstrap_p_positive,
        },
        "relative_advantage_pp": baseline_set_id.map(|_| metrics.oos_avg_return_pct),
        "gate": {"pass": report.pass, "reasons": report.reasons},
    })
}
```

- [ ] **步骤 4：运行测试确认通过**

```bash
cargo test --manifest-path rust/nanoquant/Cargo.toml evidence_gate -- --nocapture
```

预期：PASS。

- [ ] **步骤 5：Commit**

```bash
git add rust/nanoquant/src/evidence.rs rust/nanoquant/src/main.rs
git commit -m "feat(参数集): 增加回测证据门判定"
```

---

## 任务 4：candidate 落库后异步跑证据门

**文件：**
- 修改：`rust/nanoquant/src/tuning.rs`
- 修改：`rust/nanoquant/src/parameter_sets.rs`
- 测试：`tuning.rs`、`parameter_sets.rs`

- [ ] **步骤 1：编写失败测试**

```rust
#[tokio::test]
async fn candidate_gets_evidence_status() {
    let (db, transport) = tuning_fixture();
    let state = AiState::with_transport(db.clone(), MASTER_KEY.as_bytes().to_vec(), Arc::new(transport));
    // 伪证据门：直接写 approved/rejected，验证状态迁移链路
    let id = crate::parameter_sets::insert_candidate(
        &db.conn.lock().unwrap(),
        "range-ai-test",
        "range",
        "1h",
        &json!({"timeframe":"1h"}),
        None,
    ).unwrap();
    crate::parameter_sets::mark_evidence(&db.conn.lock().unwrap(), id, true, &json!({"gate":{"pass":true,"reasons":[]}})).unwrap();
    let (status, evidence): (String, String) = db.conn.lock().unwrap().query_row(
        "SELECT status,evidence FROM parameter_sets WHERE id=?1", [id], |r| Ok((r.get(0)?, r.get(1)?))
    ).unwrap();
    assert_eq!(status, "approved");
    assert!(evidence.contains("\"pass\":true"));
}
```

- [ ] **步骤 2：运行测试确认失败**

```bash
cargo test --manifest-path rust/nanoquant/Cargo.toml candidate_gets_evidence_status -- --nocapture
```

预期：缺少 `mark_evidence`。

- [ ] **步骤 3：实现**

在 `parameter_sets.rs` 添加：

```rust
pub(crate) fn mark_evidence(
    conn: &Connection,
    id: i64,
    pass: bool,
    evidence: &Value,
) -> Result<(), String> {
    let status = if pass { "approved" } else { "rejected" };
    conn.execute(
        "UPDATE parameter_sets SET status=?1, evidence=?2 WHERE id=?3 AND source='ai' AND status='candidate'",
        params![status, serde_json::to_string(evidence).unwrap(), id],
    ).map_err(|e| e.to_string())?;
    Ok(())
}

pub(crate) fn active_for_regime(conn: &Connection, regime: &str) -> Option<ParameterSet> {
    conn.query_row(
        "SELECT id,name,regime,timeframe,version,params,status,source,evidence,parent_id,created_at,activated_at \
         FROM parameter_sets WHERE regime=?1 AND status='active' ORDER BY id DESC LIMIT 1",
        [regime],
        row_to_set,
    ).ok()
}
```

`ParameterSet` 结构：

```rust
#[derive(Debug, Clone)]
pub(crate) struct ParameterSet {
    pub id: i64,
    pub name: String,
    pub regime: String,
    pub timeframe: String,
    pub version: i64,
    pub params: Value,
    pub status: String,
    pub source: String,
    pub evidence: Value,
    pub parent_id: Option<i64>,
    pub created_at: i64,
    pub activated_at: Option<i64>,
}
```

在 `tuning.rs` 的 `run_once` 中，candidate 写入后：

```rust
tokio::spawn(crate::evidence_task::evaluate_and_mark(state.clone(), id));
```

新增 `evidence_task.rs`（或放在 `tuning.rs`）：

```rust
pub(crate) async fn evaluate_and_mark(state: AiState, candidate_id: i64) -> Result<(), String> {
    let (candidate, baseline, live, symbols, timeoutframes, min_oos) = load_context(&state, candidate_id)?;
    let basket = load_basket(&live, &symbols, &candidate.timeframe).await?;
    let config = candidate_config(&state, &candidate.params)?;
    let baseline_metrics = if let Some(baseline) = &baseline {
        run_metrics(&basket, &candidate_config(&state, &baseline.params)?, &candidate.timeframe).await?
    } else { None };
    let metrics = run_metrics(&basket, &config, &candidate.timeframe).await?;
    let report = crate::evidence::evaluate_gate(&metrics, min_oos, &live.timeframe, &live.auto_timeframes, baseline_metrics.as_ref().map(|m| m.oos_avg_return_pct));
    let evidence = crate::evidence::evidence_json(&metrics, &report, baseline.map(|b| b.id));
    let conn = state.db.conn.lock().map_err(|_| "数据库锁不可用".to_string())?;
    crate::parameter_sets::mark_evidence(&conn, candidate_id, report.pass, &evidence)?;
    Ok(())
}
```

辅助函数签名（按现有 `history` / `backtest` 实现）：

```rust
fn load_context(
    state: &AiState,
    candidate_id: i64,
) -> Result<(ParameterSet, Option<ParameterSet>, LiveParams, Vec<String>, Vec<String>, usize), String>;
async fn load_basket(
    live: &crate::history::BinanceHistory,
    symbols: &[String],
    timeframe: &str,
) -> Result<Vec<(String, Vec<HistoryBar>)>, String>;
fn candidate_config(state: &AiState, params: &Value) -> Result<EngineConfig, String>;
async fn run_metrics(
    basket: &[(String, Vec<HistoryBar>)],
    config: &EngineConfig,
    timeframe: &str,
) -> Result<BasketMetrics, String>;
```

`load_basket` 用 `history::BinanceHistory::new(live)` 拉取每个启用标的的 2000 根当前 timeframe K 线；`run_metrics` 用 `spawn_blocking` 调 `run_walkforward_basket_fixed`，目标 regime 从候选集 `regime` 读取。`LiveParams` 是包含 `timeframe` 与 `auto_timeframes` 的小结构，直接在 `evidence_task.rs` 定义。

- [ ] **步骤 4：运行测试确认通过**

```bash
cargo test --manifest-path rust/nanoquant/Cargo.toml candidate_gets_evidence_status -- --nocapture
cargo test --manifest-path rust/nanoquant/Cargo.toml evidence_gate -- --nocapture
```

预期：PASS。

- [ ] **步骤 5：Commit**

```bash
git add rust/nanoquant/src/parameter_sets.rs rust/nanoquant/src/tuning.rs rust/nanoquant/src/evidence_task.rs rust/nanoquant/src/main.rs
git commit -m "feat(参数集): 候选异步回测并落证据状态"
```

---

## 任务 5：只读参数集 API

**文件：**
- 创建：`rust/nanoquant/src/parameter_sets_api.rs`
- 修改：`rust/nanoquant/src/main.rs`（路由挂载）
- 测试：`parameter_sets_api.rs`

- [ ] **步骤 1：编写失败测试**

```rust
#[tokio::test]
async fn list_parameter_sets_returns_candidate_and_evidence() {
    let (db, _) = fixture();
    let app = crate::parameter_sets_api::routes().with_state(db);
    let response = app
        .oneshot(Request::builder().uri("/parameter-sets?status=candidate").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert!(body["items"].as_array().is_some());
}
```

- [ ] **步骤 2：运行测试确认失败**

```bash
cargo test --manifest-path rust/nanoquant/Cargo.toml list_parameter_sets_returns_candidate_and_evidence -- --nocapture
```

预期：编译失败。

- [ ] **步骤 3：实现**

```rust
use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use crate::db::Db;

pub fn routes() -> Router<Db> {
    Router::new()
        .route("/parameter-sets", get(list))
        .route("/parameter-sets/current", get(current))
        .route("/parameter-sets/{id}/evidence", get(evidence))
}

#[derive(Deserialize)]
struct ListQuery { regime: Option<String>, status: Option<String> }

async fn list(State(db): State<Db>, Query(q): Query<ListQuery>) -> Json<Value> {
    let conn = db.conn.lock().expect("db");
    let mut stmt = conn.prepare(
        "SELECT id,name,regime,timeframe,version,params,status,source,evidence,parent_id,created_at,activated_at \
         FROM parameter_sets WHERE (?1 IS NULL OR regime=?1) AND (?2 IS NULL OR status=?2) ORDER BY id DESC LIMIT 100"
    ).expect("prepare");
    let items: Vec<Value> = stmt.query_map([&q.regime, &q.status], |row| {
        Ok(json!({
            "id": row.get::<_, i64>(0)?,
            "name": row.get::<_, String>(1)?,
            "regime": row.get::<_, String>(2)?,
            "timeframe": row.get::<_, String>(3)?,
            "version": row.get::<_, i64>(4)?,
            "status": row.get::<_, String>(6)?,
            "source": row.get::<_, String>(7)?,
            "evidence": serde_json::from_str::<Value>(&row.get::<_, String>(8)?).unwrap_or(Value::Null),
            "parent_id": row.get::<_, Option<i64>>(9)?,
            "created_at": row.get::<_, i64>(10)?,
            "activated_at": row.get::<_, Option<i64>>(11)?,
        }))
    }).expect("query").flatten().collect();
    json!({ "items": items }).into()
}

async fn current(State(db): State<Db>) -> Json<Value> {
    let conn = db.conn.lock().expect("db");
    let current_regime = crate::runtime::engine_state_public(&conn, "market_regime");
    json!({ "market_regime": current_regime }).into()
}

async fn evidence(State(db): State<Db>, axum::extract::Path(id): axum::extract::Path<i64>) -> Json<Value> {
    let conn = db.conn.lock().expect("db");
    let raw: Option<String> = conn.query_row("SELECT evidence FROM parameter_sets WHERE id=?1", [id], |r| r.get(0)).ok();
    let evidence = raw.and_then(|text| serde_json::from_str::<Value>(&text).ok()).unwrap_or(Value::Null);
    json!({ "id": id, "evidence": evidence }).into()
}
```

在 `main.rs` 的 `router()` 中挂载 `crate::parameter_sets_api::routes()`（与 `engine::routes()` 同级，受认证中间件保护）。

- [ ] **步骤 4：运行测试确认通过**

```bash
cargo test --manifest-path rust/nanoquant/Cargo.toml list_parameter_sets_returns_candidate_and_evidence -- --nocapture
```

预期：PASS。

- [ ] **步骤 5：Commit**

```bash
git add rust/nanoquant/src/parameter_sets_api.rs rust/nanoquant/src/main.rs
git commit -m "feat(参数集): 增加只读参数集与证据 API"
```

---

## 任务 6：B 期验收

- [ ] **步骤 1：运行后端门禁**

```bash
cargo fmt --all --manifest-path rust/nanoquant/Cargo.toml --check
cargo clippy --all-targets --manifest-path rust/nanoquant/Cargo.toml -- -D warnings
cargo test --release --locked --manifest-path rust/nanoquant/Cargo.toml
```

- [ ] **步骤 2：前端门禁**

```bash
cd frontend
npm test
npm run typecheck
npm run build
```

- [ ] **步骤 3：验收断言**

- candidate 落库后，证据任务只写 `status` / `evidence`，不写 `strategy`。
- timeframe 不匹配、OOS < 30、ci_low <= 0、p_positive < 0.90、压力收益 < 0、回撤 > 15%、相对基线 < 0.10pp 均能失败。
- `parameter_set.auto_apply=false` 时 approved 不会激活。

- [ ] **步骤 4：Commit**

```bash
git add -A
git commit -m "test(参数集): 完成 B 期证据门验收"
```

---

## 自检

- 规格覆盖：§7 B 期、§10 证据门常量、§12 C3/C4/C5 相关测试。
- 类型一致性：`BasketMetrics`、`EvidenceReport`、`mark_evidence` 被 C 期复用。
- 所有阈值都绑定 `strategy.timeframe`，1h 硬边界不适用于 4h。