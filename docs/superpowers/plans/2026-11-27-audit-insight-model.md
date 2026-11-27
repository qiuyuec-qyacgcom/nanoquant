# 决策审计与 Insight 对象化实现计划

> **面向 AI 代理的工作者：** 必需子技能：使用 superpowers:subagent-driven-development（推荐）或 superpowers:executing-plans 逐任务实现此计划。步骤使用复选框（`- [ ]`）语法来跟踪进度。

**目标：** 把散落在各处的退出原因收敛成一个枚举，把每次决策落成结构化的 Insight 对象，
并把订单对账升级为一等公民事件 —— 让「为什么这笔单没开、为什么平了、谁否决的」全部可查可归因。

**架构：** 新建 `exit_reason.rs`（唯一退出原因来源）与 `insight.rs`（决策洞察结构）；
`reconcile.rs` 在既有 `ReconcileAction` 之上补三类报告模型，并把结果写成 `engine_events`。
三者互不耦合，可以分任务独立合入。

**技术栈：** Rust、serde_json、rusqlite。

**来源依据：** `docs/PARITY_BENCHMARK_2026-11-27.md` 第三节 P2、第四节第 6、7 条。

---

## 文件结构

| 文件 | 动作 | 职责 |
|---|---|---|
| `rust/nanoquant/src/exit_reason.rs` | **创建** | 退出原因枚举与字符串互转，唯一来源 |
| `rust/nanoquant/src/insight.rs` | **创建** | 决策 Insight 结构与落库格式 |
| `rust/nanoquant/src/reconcile.rs` | 修改 | 补三类对账报告模型并产出事件 |
| `rust/nanoquant/src/manage.rs` | 修改 | 退出原因改用枚举 |
| `rust/nanoquant/src/fast_risk.rs` | 修改 | 退出原因改用枚举 |
| `rust/nanoquant/src/backtest_engine.rs` | 修改 | 退出原因改用枚举 |
| `rust/nanoquant/src/signal_audit.rs` | 修改 | 写 Insight 到 `signals.features` |
| `rust/nanoquant/src/journal.rs` | 修改 | 写 Insight 到 `decision_journal` |
| `rust/nanoquant/src/main.rs` | 修改 | 加两个 `mod` |

---

## 执行顺序与标记

| 批次 | 任务 | 标记 |
|---|---|---|
| P2 | 退出原因统一枚举 + 漂移守卫 | `[L1]` |
| P2 | 决策 Insight 对象化 | `[L2]` |
| P2 | 订单对账报告模型事件化 | `[L3]` |

**依赖：** 三者互不依赖，可并行。

**执行约定：** 每完成一项在 TodoWrite 打标并单独 commit，commit message 前缀用标记。
测试用 `cargo build --tests --bin nanoquant` + `target/debug/build/nanoquant/*/out/nanoquant-*.exe`。

---

## 任务 L1：退出原因统一枚举

**文件：**
- 创建：`rust/nanoquant/src/exit_reason.rs`
- 修改：`rust/nanoquant/src/manage.rs`、`rust/nanoquant/src/fast_risk.rs`、`rust/nanoquant/src/backtest_engine.rs`、`rust/nanoquant/src/cycle.rs`
- 修改：`rust/nanoquant/src/main.rs`
- 测试：`rust/nanoquant/src/exit_reason.rs` 内联 `mod tests`

**现状（散落的字符串字面量）：**

| 产生位置 | 取值 |
|---|---|
| `manage.rs` | `stop_loss`、`global_stop_loss`、`global_take_profit`、`take_profit`、`roi_target`、`trailing_stop`、`signal_flip` |
| `fast_risk.rs` | `invalidation`、`stop_loss`、`global_stop_loss`、`global_take_profit`、`take_profit` |
| `backtest_engine.rs` | `liquidation`、`timeout` |
| `cycle.rs` / `runtime.rs` | `stop_loss`、`halt_flatten` |

- [ ] **步骤 1：编写失败的测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_reason_round_trips_through_its_string() {
        for reason in ExitReason::all() {
            assert_eq!(ExitReason::from_str(reason.as_str()), Some(reason));
        }
    }

    #[test]
    fn the_string_set_is_frozen() {
        // 退出原因是历史数据的一部分：新增取值要显式改这里，否则旧统计口径会漂移。
        let mut names: Vec<&str> = ExitReason::all().iter().map(|r| r.as_str()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec![
                "global_stop_loss", "global_take_profit", "halt_flatten", "invalidation",
                "liquidation", "manual", "partial_take_profit", "roi_target", "signal_flip",
                "stop_loss", "take_profit", "timeout", "trailing_stop",
            ]
    }
}
```

- [ ] **步骤 2：运行测试验证失败**

运行：`cargo build --tests --bin nanoquant`
预期：编译失败 —— `exit_reason` 模块不存在。

- [ ] **步骤 3：实现枚举**

```rust
//! 退出原因枚举：管仓、快平、回测、风控平仓共用同一套取值。
//!
//! 这些字符串会落进 `trades.exit_reason` 与 `orders.purpose`，是历史统计的口径，
//! **只增不改**：改名字会让旧交易的归因对不上。

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    StopLoss,
    GlobalStopLoss,
    GlobalTakeProfit,
    TakeProfit,
    RoiTarget,
    TrailingStop,
    SignalFlip,
    Invalidation,
    Liquidation,
    Timeout,
    RiskHalt,
    PartialTakeProfit,
    Manual,
}

impl ExitReason {
    pub fn as_str(self) -> &'static str {
        match self {
            ExitReason::StopLoss => "stop_loss",
            ExitReason::GlobalStopLoss => "global_stop_loss",
            ExitReason::GlobalTakeProfit => "global_take_profit",
            ExitReason::TakeProfit => "take_profit",
            ExitReason::RoiTarget => "roi_target",
            ExitReason::TrailingStop => "trailing_stop",
            ExitReason::SignalFlip => "signal_flip",
            ExitReason::Invalidation => "invalidation",
            ExitReason::Liquidation => "liquidation",
            ExitReason::Timeout => "timeout",
            ExitReason::RiskHalt => "halt_flatten",
            ExitReason::PartialTakeProfit => "partial_take_profit",
            ExitReason::Manual => "manual",
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        Some(match value {
            "stop_loss" => ExitReason::StopLoss,
            "global_stop_loss" => ExitReason::GlobalStopLoss,
            "global_take_profit" => ExitReason::GlobalTakeProfit,
            "take_profit" => ExitReason::TakeProfit,
            "roi_target" => ExitReason::RoiTarget,
            "trailing_stop" => ExitReason::TrailingStop,
            "signal_flip" => ExitReason::SignalFlip,
            "invalidation" => ExitReason::Invalidation,
            "liquidation" => ExitReason::Liquidation,
            "timeout" => ExitReason::Timeout,
            "halt_flatten" => ExitReason::RiskHalt,
            "partial_take_profit" => ExitReason::PartialTakeProfit,
            "manual" => ExitReason::Manual,
            _ => return None,
        })
    }

    pub fn all() -> &'static [ExitReason] {
        &[
            ExitReason::StopLoss,
            ExitReason::GlobalStopLoss,
            ExitReason::GlobalTakeProfit,
            ExitReason::TakeProfit,
            ExitReason::RoiTarget,
            ExitReason::TrailingStop,
            ExitReason::SignalFlip,
            ExitReason::Invalidation,
            ExitReason::Liquidation,
            ExitReason::Timeout,
            ExitReason::RiskHalt,
            ExitReason::PartialTakeProfit,
            ExitReason::Manual,
        ]
    }
}
```

- [ ] **步骤 4：把字面量换成枚举**

1. `manage.rs` 的每个 `ManageAction::Close { reason: "xxx".to_string() }` 改成
   `reason: ExitReason::Xxx.as_str().to_string()`；
2. `fast_risk.rs` 的 `fast_risk_reason` 返回 `Option<&'static str>`，改成返回 `Option<ExitReason>`；
3. `backtest_engine.rs` 的 `"liquidation"` / `"timeout"` 同理。

- [ ] **步骤 5：加漂移守卫测试**

在 `exit_reason.rs` 的测试模块追加一条 —— 扫描源码里所有 `reason: "` 字面量，
确保每个都能被 `ExitReason::from_str` 解析：

```rust
    #[test]
    fn no_unknown_reason_literal_is_left_in_the_source() {
        let source = include_str!("manage.rs");
        for line in source.lines() {
            let Some(start) = line.find("reason: \"") else { continue };
            let rest = &line[start + 9..];
            let Some(end) = rest.find('"') else { continue };
            let value = &rest[..end];
            assert!(
                ExitReason::from_str(value).is_some(),
                "manage.rs 出现了枚举外的退出原因 {value}");
        }
    }
```

- [ ] **步骤 6：全量验证并提交**

```bash
cargo build --tests --bin nanoquant
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
target/debug/build/nanoquant/*/out/nanoquant-*.exe
git add rust/nanoquant/src/exit_reason.rs rust/nanoquant/src/manage.rs \
        rust/nanoquant/src/fast_risk.rs rust/nanoquant/src/backtest_engine.rs \
        rust/nanoquant/src/cycle.rs rust/nanoquant/src/main.rs
git commit -m "[L1] refactor(audit): 退出原因统一为枚举"
```

---

## 任务 L2：决策 Insight 对象化

**文件：**
- 创建：`rust/nanoquant/src/insight.rs`
- 修改：`rust/nanoquant/src/signal_audit.rs`
- 修改：`rust/nanoquant/src/journal.rs`
- 修改：`rust/nanoquant/src/main.rs`
- 测试：`rust/nanoquant/src/insight.rs` 内联 `mod tests`

**目标结构（LEAN 的 Insight 思路）：** 方向、幅度、置信度、有效期、被谁否决。
本系统已有 `signals`（信号级）与 `decision_journal`（决策级）两张表，缺的是「结构化的一眼可读视图」。

- [ ] **步骤 1：编写失败的测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insight_serializes_with_all_five_facets() {
        let insight = Insight {
            direction: "long".to_string(),
            magnitude: 0.42,
            confidence: 0.77,
            valid_bars: Some(6),
            vetoed_by: None,
            action: "open_long".to_string(),
            reason: "趋势确认".to_string(),
        };
        let value = serde_json::to_value(&insight).unwrap();
        for key in ["direction", "magnitude", "confidence", "valid_bars", "vetoed_by", "action", "reason"] {
            assert!(value.get(key).is_some(), "缺少字段 {key}: {value}");
        }
    }
}
```

- [ ] **步骤 2：实现结构**

```rust
//! 决策洞察对象：一次决策的「方向、幅度、置信度、有效期、被谁否决」。

#[derive(Debug, Clone, serde::Serialize)]
pub struct Insight {
    /// long / short / flat。
    pub direction: String,
    /// 信号强度绝对值（0–1）。
    pub magnitude: f64,
    /// AI 置信度；未经过复核时为 0。
    pub confidence: f64,
    /// 决策有效根数。
    pub valid_bars: Option<u64>,
    /// 否决方：`ai_veto` / `news_veto` / `funding_veto` / `risk_gate` / `score_split_low` / `ai_fail_open`。
    pub vetoed_by: Option<String>,
    /// 归一化动作：open_long / open_short / hold / reject。
    pub action: String,
    pub reason: String,
}
```

- [ ] **步骤 3：写入 `signals.features`**

在 `signal_audit.rs` 记录信号的地方，把 Insight 以 `{"insight": {...}}` 写进 `features` JSON。
注意 `features` 已有内容（注入的心法/经验 ID、AI 元数据），要**合并**而不是覆盖。

```rust
let insight = Insight {
    direction: direction.to_string(),
    magnitude: score.abs(),
    confidence: ai_confidence,
    valid_bars,
    vetoed_by: veto_source,
    action: action.to_string(),
    reason: reason.clone(),
};
features["insight"] = serde_json::to_value(&insight)?;
```

- [ ] **步骤 4：写入 `decision_journal`**

`journal.rs` 的 `record_decision` 已经写 `snapshot` / `decision` / `outcome`，
在同一处补一个 `insight` 字段，字段名与 `signals` 里保持一致。

- [ ] **步骤 5：查询接口暴露**

`signal_audit.rs` 的 `half_ai_stats` 里加一项 `insight_coverage`：
有 insight 的信号数 / 总信号数。前端据此判断「这批信号是不是新的结构化格式」。

- [ ] **步骤 6：全量验证并提交**

```bash
cargo build --tests --bin nanoquant
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
target/debug/build/nanoquant/*/out/nanoquant-*.exe
git add rust/nanoquant/src/insight.rs rust/nanoquant/src/signal_audit.rs \
        rust/nanoquant/src/journal.rs rust/nanoquant/src/main.rs
git commit -m "[L2] feat(audit): 决策 Insight 对象化"
```

---

## 任务 L3：订单对账报告模型事件化

**文件：**
- 修改：`rust/nanoquant/src/reconcile.rs`
- 修改：`rust/nanoquant/src/runtime.rs`（对账结果落事件）
- 测试：`rust/nanoquant/src/reconcile.rs` 内联 `mod tests`

**现状：** `reconcile.rs` 已有 `ReconcileAction` 枚举（`Adopt` / `CloseLocal`）与
`VenuePosition` / `LocalPosition` 结构，但结果只体现在控制流里，没有留下可查询的「谁对不上、差多少」记录。

- [ ] **步骤 1：编写失败的测试**

```rust
    #[test]
    fn reconcile_emits_position_status_reports() {
        let venue = vec![VenuePosition {
            symbol: "BTCUSDT".into(), side: "long".into(), qty: 1.0,
            mark_price: 100.0, sl_price: 90.0, tp_price: 0.0,
        }];
        let local = vec![LocalPosition {
            symbol: "BTCUSDT".into(), mode: "live".into(), side: "long".into(), qty: 0.5,
            // …… 其余字段按真实结构填
        }];
        let reports = position_status_reports(&venue, &local);
        assert_eq!(reports.len(), 1);
        assert!(!reports[0].reconciled, "数量不一致必须标为未对账");
        assert!((reports[0].qty_diff - 0.5).abs() < 1e-9);
    }
```

- [ ] **步骤 2：实现三类报告模型**

```rust
/// 订单状态报告：交易所侧看到的订单状态。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct OrderStatusReport {
    pub client_order_id: String,
    pub venue_order_id: String,
    pub status: String,
    pub executed_qty: f64,
    pub avg_price: f64,
    pub updated_at: i64,
}

/// 成交报告：一次成交的事实。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct FillReport {
    pub client_order_id: String,
    pub qty: f64,
    pub price: f64,
    pub fee: f64,
    pub filled_at: i64,
}

/// 持仓状态报告：本地与交易所的差异。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct PositionStatusReport {
    pub symbol: String,
    pub side: String,
    pub venue_qty: f64,
    pub local_qty: f64,
    pub qty_diff: f64,
    pub reconciled: bool,
    pub naked: bool,
}

/// 逐个标的比对本地与交易所持仓。
pub fn position_status_reports(
    venue: &[VenuePosition],
    local: &[LocalPosition],
) -> Vec<PositionStatusReport> {
    venue
        .iter()
        .map(|venue| {
            let matched = local.iter().find(|item| item.symbol == venue.symbol && item.side == venue.side);
            let local_qty = matched.map(|item| item.qty).unwrap_or(0.0);
            let qty_diff = venue.qty - local_qty;
            PositionStatusReport {
                symbol: venue.symbol.clone(),
                side: venue.side.clone(),
                venue_qty: venue.qty,
                local_qty,
                qty_diff,
                reconciled: qty_diff.abs() < 1e-9,
                naked: venue.sl_price <= 0.0,
            }
        })
        .collect()
}
```

- [ ] **步骤 3：落成引擎事件**

在 `runtime.rs` 的对账调用处，把报告写成 `engine_events`：

```rust
for report in crate::reconcile::position_status_reports(&venue, &local) {
    let payload = serde_json::to_value(&report).unwrap_or_default();
    crate::events::record(
        &tx,
        "warn",
        "reconcile",
        &format!("{} {} 本地 {:.8} / 交易所 {:.8}", report.symbol, report.side, report.local_qty, report.venue_qty),
        &payload,
    );
}
```

事件类别固定为 `reconcile`，只在对不上或有裸仓时记录 —— 不要每轮刷屏。

- [ ] **步骤 4：全量验证并提交**

```bash
cargo build --tests --bin nanoquant
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
target/debug/build/nanoquant/*/out/nanoquant-*.exe
git add rust/nanoquant/src/reconcile.rs rust/nanoquant/src/runtime.rs
git commit -m "[L3] feat(audit): 订单对账报告模型事件化"
```

---

## 自检

**1. 规格覆盖度** —— PARITY_BENCHMARK P2（订单对账事件化、保护单语义）与第四节第 6、7 条
由 `[L1]`（退出原因统一 → 保护单语义的基础）、`[L2]`（Insight）、`[L3]`（对账报告）覆盖。

**2. 占位符扫描** —— 无「待定」「TODO」「类似任务 N」；每处改动都有代码块与测试名。

**3. 类型一致性** —— `ExitReason` 定义在 `exit_reason.rs`，`[L1]` 内消费；
`Insight` 定义在 `insight.rs`，`[L2]` 内消费；
三个报告模型定义在 `reconcile.rs`，`[L3]` 内消费。互不交叉，无同名歧义。

---

## 执行交接

计划已完成并保存到 `docs/superpowers/plans/2026-11-27-audit-insight-model.md`。两种执行方式：

**1. 子代理驱动（推荐）** —— 每个任务调度一个新的子代理，任务间进行审查

**2. 内联执行** —— 在当前会话中使用 executing-plans 执行任务，批量执行并设有检查点

选哪种方式？
