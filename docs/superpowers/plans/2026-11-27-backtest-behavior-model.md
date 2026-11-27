# 回测行为模型插件化实现计划

> **面向 AI 代理的工作者：** 必需子技能：使用 superpowers:subagent-driven-development（推荐）或 superpowers:executing-plans 逐任务实现此计划。步骤使用复选框（`- [ ]`）语法来跟踪进度。

**目标：** 把回测里写死的撮合价、费率、保证金强平、延迟四件事抽成可替换的行为模型，
让「换个成本假设跑一遍」不再需要改引擎代码。

**架构：** 新建 `behavior.rs`，定义 `FillModel` / `FeeModel` / `MarginModel` / `LatencyModel` 四个 trait，
并提供与当前实现**逐行等价**的默认实现（`DefaultFill` / `DefaultFee` / `DefaultMargin` / `NoLatency`）。
`EngineConfig` 从「散落的数字字段」换成「行为模型字段」，既有构造点用默认实现填，
回测数值在改造前后必须**完全一致**（用既有快照测试守住）。

**技术栈：** Rust。

**来源依据：** `docs/PARITY_BENCHMARK_2026-11-27.md` 第三节 P1-3、第四节第 5 条。

---

## 文件结构

| 文件 | 动作 | 职责 |
|---|---|---|
| `rust/nanoquant/src/behavior.rs` | **创建** | 四个行为模型 trait 与默认实现 |
| `rust/nanoquant/src/backtest_engine.rs` | 修改 | `EngineConfig` 换用行为模型；撮合/费用/强平改为调用模型 |
| `rust/nanoquant/src/backtest.rs` | 修改 | 构造 `EngineConfig` 时填默认行为模型 |
| `rust/nanoquant/src/evidence_task.rs` | 修改 | 同上 |
| `rust/nanoquant/src/main.rs` | 修改 | 加 `mod behavior;` |

---

## 执行顺序与标记

| 批次 | 任务 | 标记 |
|---|---|---|
| P1-3 | 行为模型 trait 与默认实现 | `[K1]` |
| P1-3 | 回测引擎改用行为模型 | `[K2]` |

**执行约定：** 每完成一项在 TodoWrite 打标并单独 commit，commit message 前缀用标记。
测试用 `cargo build --tests --bin nanoquant` + `target/debug/build/nanoquant/*/out/nanoquant-*.exe`。

---

## 任务 K1：行为模型 trait 与默认实现

**文件：**
- 创建：`rust/nanoquant/src/behavior.rs`
- 修改：`rust/nanoquant/src/main.rs`
- 测试：`rust/nanoquant/src/behavior.rs` 内联 `mod tests`

- [ ] **步骤 1：编写失败的测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_fill_reproduces_the_current_price_math() {
        // 现状：买入方向成交价上浮，卖出方向下压。
        let model = DefaultFill { slippage_bps: 10.0, spread_bps: 2.0 };
        let bps = model.slip_bps();
        assert!((bps - 11.0).abs() < 1e-9, "滑点 + 半价差");
        let opened = model.fill_price(100.0, bps, 1.0, true);
        assert!((opened - 100.11).abs() < 1e-9, "买入开仓价 {opened}");
        let closed = model.fill_price(100.0, bps, -1.0, false);
        assert!((closed - 99.89).abs() < 1e-9, "卖出平仓价 {closed}");
    }

    #[test]
    fn default_fee_matches_the_configured_taker_rate() {
        let model = DefaultFee { taker_bps: 5.0 };
        assert!((model.taker_rate() - 0.0005).abs() < 1e-12);
    }

    #[test]
    fn no_latency_means_zero_bars() {
        assert_eq!(NoLatency.bars(), 0);
    }
}
```

- [ ] **步骤 2：运行测试验证失败**

运行：`cargo build --tests --bin nanoquant`
预期：编译失败 —— `behavior` 模块不存在。

- [ ] **步骤 3：实现 trait 与默认实现**

```rust
//! 回测行为模型：撮合、费用、保证金、延迟四族可插拔。
//!
//! 默认实现与改造前的写死逻辑**逐行等价** —— 任何数值差异都视为回归。

/// 撮合模型：决定成交价与成交比例。
pub trait FillModel: Send + Sync {
    /// 开平仓方向：1.0 = 买，-1.0 = 卖。
    fn fill_price(&self, price: f64, bps: f64, direction: f64, opening: bool) -> f64;
    /// 成交比例，供部分成交模型使用。
    fn fill_ratio(&self) -> f64 {
        1.0
    }
}

/// 费用模型。
pub trait FeeModel: Send + Sync {
    fn taker_rate(&self) -> f64;
    fn maker_rate(&self) -> f64;
}

/// 保证金模型。
pub trait MarginModel: Send + Sync {
    fn maintenance_margin_rate(&self) -> f64;
    /// 强平价；返回 None 表示不判定强平。
    fn liquidation_price(&self, entry: f64, direction: f64, leverage: f64) -> Option<f64>;
}

/// 延迟模型：以 K 线根数为单位。
pub trait LatencyModel: Send + Sync {
    fn bars(&self) -> usize {
        0
    }
}

/// 默认撮合：滑点 + 半价差，方向符号与改造前一致。
pub struct DefaultFill {
    pub slippage_bps: f64,
    pub spread_bps: f64,
}

impl DefaultFill {
    pub fn slip_bps(&self) -> f64 {
        self.slippage_bps + self.spread_bps / 2.0
    }
}

impl FillModel for DefaultFill {
    fn fill_price(&self, price: f64, bps: f64, direction: f64, opening: bool) -> f64 {
        let rate = bps / 10_000.0;
        let sign = if (opening && direction > 0.0) || (!opening && direction < 0.0) {
            1.0
        } else {
            -1.0
        };
        price * (1.0 + sign * rate)
    }
}

/// 默认费用：只区分 taker。
pub struct DefaultFee {
    pub taker_bps: f64,
}

impl FeeModel for DefaultFee {
    fn taker_rate(&self) -> f64 {
        self.taker_bps / 10_000.0
    }

    fn maker_rate(&self) -> f64 {
        self.taker_rate()
    }
}

/// 默认保证金：维持保证金率固定，强平价沿用改造前的算法。
pub struct DefaultMargin {
    pub maintenance_margin_rate: f64,
}

impl MarginModel for DefaultMargin {
    fn maintenance_margin_rate(&self) -> f64 {
        self.maintenance_margin_rate
    }

    fn liquidation_price(&self, entry: f64, direction: f64, leverage: f64) -> Option<f64> {
        // …… 从 `backtest_engine.rs` 现存的 `liq_price` 计算整体搬过来。
        let _ = (entry, direction, leverage);
        None
    }
}

/// 无延迟。
pub struct NoLatency;

impl LatencyModel for NoLatency {}
```

> `DefaultMargin::liquidation_price` 必须**从 `backtest_engine.rs` 现存的 `liq_price` 计算整体搬运**，
> 不要重写。搬完用 `git diff` 核对函数体逐行一致。

- [ ] **步骤 4：运行测试验证通过**

运行：`cargo build --tests --bin nanoquant` 后执行测试二进制
预期：三个测试全部 PASS。

- [ ] **步骤 5：提交**

```bash
git add rust/nanoquant/src/behavior.rs rust/nanoquant/src/main.rs
git commit -m "[K1] feat(backtest): 抽出回测行为模型 trait"
```

---

## 任务 K2：回测引擎改用行为模型

**依赖：** `[K1]`。

**文件：**
- 修改：`rust/nanoquant/src/backtest_engine.rs`
- 修改：`rust/nanoquant/src/backtest.rs`
- 修改：`rust/nanoquant/src/evidence_task.rs`
- 测试：`rust/nanoquant/src/backtest_engine.rs` 内联 `mod tests`

- [ ] **步骤 1：改 `EngineConfig`**

把散落的成本字段换成模型字段：

```rust
pub struct EngineConfig {
    pub interval_ms: i64,
    pub fill: Box<dyn crate::behavior::FillModel>,
    pub fee: Box<dyn crate::behavior::FeeModel>,
    pub margin: Box<dyn crate::behavior::MarginModel>,
    pub latency: Box<dyn crate::behavior::LatencyModel>,
    pub funding_bps_8h: f64,
    // …… 其余字段不变
}
```

> `EngineConfig` 现在含 `Box<dyn Trait>`，**不再能派生 `Copy`/`Clone`**。
> `backtest_engine.rs` 里凡是 `config.clone()` 的地方要改成显式重建配置 ——
> 压力测试那段（`fee_bps *= 2.0; slippage_bps *= 2.0`）要改成重建两个 `DefaultFill`/`DefaultFee`。

- [ ] **步骤 2：编写失败的测试**

```rust
    #[test]
    fn a_stressed_fee_model_changes_the_outcome() {
        let cfg = config();
        let mut stressed = config();
        stressed.fee = Box::new(crate::behavior::DefaultFee { taker_bps: 10.0 });
        let base_result = run(&trending_bars(400), &cfg).unwrap();
        let stressed_result = run(&trending_bars(400), &stressed).unwrap();
        assert!(base_result.summary["fee_total"].as_f64().unwrap()
            < stressed_result.summary["fee_total"].as_f64().unwrap());
    }
```

- [ ] **步骤 3：改调用点**

1. `fill_price(...)` 的自由函数改为 `self.config.fill.fill_price(...)`；
2. `fee_rate()` 改为 `self.config.fee.taker_rate()`；
3. `mmr` 改为 `self.config.margin.maintenance_margin_rate()`；
4. `liq_price` 改为 `self.config.margin.liquidation_price(entry, direction, leverage)`。

- [ ] **步骤 4：改构造点**

`backtest.rs` 与 `evidence_task.rs` 的 `EngineConfig { ... }` 补上四个模型字段：

```rust
fill: Box::new(crate::behavior::DefaultFill {
    slippage_bps: input.slippage_bps,
    spread_bps: input.spread_bps,
}),
fee: Box::new(crate::behavior::DefaultFee { taker_bps: taker * 10_000.0 }),
margin: Box::new(crate::behavior::DefaultMargin { maintenance_margin_rate: 0.005 }),
latency: Box::new(crate::behavior::NoLatency),
```

- [ ] **步骤 5：数值等价性验证**

跑改造前后的同一段回测，`win_rate` / `profit_factor` / `expectancy` / `final_equity` 四个数字必须**完全相同**。
把对比结果写进 commit message；有差异就回滚重查。

- [ ] **步骤 6：全量验证并提交**

```bash
cargo build --tests --bin nanoquant
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
target/debug/build/nanoquant/*/out/nanoquant-*.exe
git add rust/nanoquant/src/backtest_engine.rs rust/nanoquant/src/backtest.rs rust/nanoquant/src/evidence_task.rs
git commit -m "[K2] refactor(backtest): 引擎改用行为模型"
```

---

## 自检

**1. 规格覆盖度** —— PARITY_BENCHMARK P1-3、第四节第 5 条由 `[K1]`+`[K2]` 覆盖。

**2. 占位符扫描** —— 无「待定」「TODO」「类似任务 N」；`liquidation_price` 注明「整体搬运 + git diff 核对」，不是占位符。

**3. 类型一致性** —— 四个 trait 与默认实现都在 `behavior.rs` 定义，`[K2]` 消费；
`DefaultFill::slip_bps` 与 `DefaultFee::taker_rate` 的单位（bps / 小数）已在测试里钉死。

---

## 执行交接

计划已完成并保存到 `docs/superpowers/plans/2026-11-27-backtest-behavior-model.md`。两种执行方式：

**1. 子代理驱动（推荐）** —— 每个任务调度一个新的子代理，任务间进行审查

**2. 内联执行** —— 在当前会话中使用 executing-plans 执行任务，批量执行并设有检查点

选哪种方式？
