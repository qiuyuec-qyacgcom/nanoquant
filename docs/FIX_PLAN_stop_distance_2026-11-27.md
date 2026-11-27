# 本地修复方案：止损口径重构 —— 「单仓止盈止损率」退出量化决策，链接到 0.5 秒快平任务

> 目标：修复 L1 病根 —— 止损价格距离随杠杆线性收缩，落到 0.31×ATR 的噪音区。
> 约束：严格保持本系统 **半 AI + 半量化** 模式 —— 量化负责方向/杠杆/止损/止盈/仓位，AI 只做否决与降仓。
> 范围：仅本地代码 F:\cb\nanoquant，不涉及线上部署与线上参数。
> 基线：d1952d0（[P4] chore: 清理旧 AI 配置与参数建议残留）
>
> ⚠️ **基线说明**：本方案是**修复前**的点位规格，文中所有代码行号对应 `d1952d0` 的**修复前状态**，
> 描述的是当时「哪里错了」。修复已实施，代码已移动，这些行号现在指向的是修复后的代码。
> 请以 `docs/FIX_VERIFICATION_2026-11-27.md` 为准（含最新行号、逐条核验结果与仍未覆盖面）。
> 行号口径：本方案所有代码引用均以 **ripgrep 复核 HEAD=d1952d0 的实际文件内容**为准。
> 注意：PowerShell `Get-Content` 在这些 UTF-8 源文件上会把部分行合并显示，导致行号整体偏移，**不可用于核对行号**。

---

## 〇、本方案的核心主张

**参数页「仓位止盈止损」（`global_stop_loss_pct` / `global_take_profit_pct`）不再参与量化决策** ——
它不再在开仓时被换算成价格距离写进 `stop_price` / `take_profit`，而是**链接到已有的 0.5 秒快平任务**，
由快平任务按「保证金收益率」口径实时判定强制平仓。

这样切分的原因是：

1. **该参数的语义本就是「运行时强制平仓线」**，不是开仓参数。参数页自己的提示写得很清楚（`settingsCatalog.ts:120`）：
   「你的心理预期（保证金收益率）：盈利/亏损达到该比例就强制平仓；与 AI 失效价先触发者生效，0=关闭」
   ——「达到就强制平仓」是运行时行为，被放在开仓时换算成价格，是历史实现与参数语义的错位。
2. **开仓时换算 = 杠杆收缩止损距离的元凶**。`stop_price = price × (1 ∓ global_stop ÷ leverage)`，杠杆越高止损越窄。
   而这个换算出来的价格又被写进 `positions.sl_price`，运行时反过来又用它当第一优先级判定，形成「双重口径」。
3. **链接到快平任务后，全局止损线回到「账户风险预算」的本位**，与 ATR 波动止损（保护性止损）分层，各管各的。

### 分层后的止损体系

| 层 | 参数 | 口径 | 触发时机 | 职责 |
|---|---|---|---|---|
| **外层（账户风险预算）** | `global_stop_loss_pct` / `global_take_profit_pct` | 保证金收益率 | **0.5 秒快平任务**实时判定 | 单笔亏损/盈利上限，整仓强制平仓 |
| **内层（波动保护）** | `stop_distance_atr_mult` × ATR% | 价格距离 | 同在快平任务内判定 | 保证止损不落在噪音区，避免被扫 |
| **补充** | AI 失效价 `invalidation_price` | 价格 | 同在快平任务内判定 | AI 的否决意见落地 |

> 三层在**同一个 0.5 秒任务**里按「先到者生效」判定，与参数页「与 AI 失效价先触发者生效」的描述一致。

---

## 一、当前实现的问题（代码证据）

### 1.1 参数在开仓时被换算成价格（这就是杠杆压缩止损的元凶）

`rust/nanoquant/src/runtime.rs:3127-3128`：

~~~rust
let global_stop = cfg["global_stop_loss_pct"].as_f64().unwrap_or(10.0);
let stop_price = if global_stop > 0.0 {
    price * (1.0 - direction * global_stop / leverage / 100.0)   // 10% ÷ 9 = 1.11%
} else { 0.0 };
~~~

**杠杆 9 时，价格止损距离 = 10% ÷ 9 = 1.11%**。

### 1.2 换算结果写进 positions.sl_price，运行时又当第一优先级

`rust/nanoquant/src/manage.rs:90-91`（ROI 兜底，第 1 优先级）与 `manage.rs:112`（单仓止损价）：

~~~rust
let margin_pct = price_pct * pos.leverage.max(1.0);
if stop_loss_pct > 0.0 && margin_pct <= -stop_loss_pct { ... }   // 第 1 优先级：ROI 口径
...
if pos.sl_price > 0.0 && hit(direction, pos.mark_price, pos.sl_price, true) { ... }  // 第 4 优先级
~~~

`rust/nanoquant/src/fast_risk.rs:89` 完全同构，且 `global_stop_pct` 也排在 `sl`（第 103 行）之前。

### 1.3 后果：止损距离被杠杆压缩到噪音区

| 标的 | 杠杆 | ATR% | 止损价距 | 止损/ATR | 实际成交 |
|---|---|---|---|---|---|
| CFGUSDT | 8 | 2.51 | 1.25% | 0.50×ATR | −16.33% ROI |
| 2ZUSDT | 9 | 2.22 | 1.11% | 0.50×ATR | −10.08% ROI |
| BEAMXUSDT | 7 | 1.84 | 1.43% | 0.78×ATR | −10.53% ROI |
| RIFUSDT | 9 | 3.55 | 1.11% | **0.31×ATR** | −10.00% ROI |

### 1.4 `atr_stop_mult` 已退化成「只给 AI 看的装饰」

全仓库仅剩三处：`ai_analysis.rs:581`（塞 prompt）、`ai_review.rs:251`（塞复核上下文）、`param_registry.rs:61`（参数注册表）。

而 `ai_analysis.rs:791` 的断言（属于 `decision_prompt_has_no_parameter_tuning` 测试，第 782 行起）断言：

~~~rust
assert!(!system.contains("constraints.atr_stop_mult"));
assert!(!system.contains("s ≥ constraints.atr_stop_mult"));
~~~

同一条约束在**复核 prompt** 里也被删掉了 —— `ai_analysis.rs:827` 断言复核 prompt 不再出现 `constraints.atr_stop_mult`（属于 `review_prompt_omits_leverage_and_stop_fields` 测试，第 813 行起）。

**P4 主动把「止损必须 ≥ 1.3×ATR」从决策 prompt 与复核 prompt 两处同时删掉，而引擎自己又不算 ATR 止损 —— 这条约束在整条链路上彻底消失。**
本方案把它重新落到**引擎侧**（F2），而不是还给 AI。

---

## 二、修复方案（F1 / F2 / F3 / F4）

### F1 【核心】开仓不再换算止损价 —— 参数退出量化决策

**改动点：`runtime.rs:3127` 一带，把趋势仓的止损/止盈价改为由 ATR 决定，不再用 `global_stop` / `global_take_profit`。**

~~~rust
let direction = if side == "long" { 1.0 } else { -1.0 };
let global_stop = cfg["global_stop_loss_pct"].as_f64().unwrap_or(10.0);
let global_take_profit = cfg["global_take_profit_pct"].as_f64().unwrap_or(20.0);

// 波动保护止损：价格距离 = stop_distance_atr_mult × ATR%
// 「单仓止盈止损率」不再参与量化决策 —— 它由 0.5 秒快平任务按 ROI 口径判定。
let atr_stop_mult = cfg["stop_distance_atr_mult"].as_f64().unwrap_or(1.3);
let atr_floor_pct = if metric.atr_pct > 0.0 { atr_stop_mult * metric.atr_pct } else { 0.0 };

let (stop_price, take_profit) = if mean_reversion {
    crate::mean_reversion::mean_reversion_stops(&side, price, leverage, strategy)
} else {
    let stop_price = if atr_floor_pct > 0.0 {
        price * (1.0 - direction * atr_floor_pct / 100.0)
    } else { 0.0 };
    // 趋势仓止盈同样交给 ROI 阶梯与快平任务；开仓不预置固定止盈价。
    let take_profit = 0.0;
    (stop_price, take_profit)
};
~~~

**要点**：

- `stop_price` 现在**只**由 ATR 决定 → **杠杆不再影响止损距离**（这是病根的直接切断）；
- `take_profit` 趋势仓置 0 → 止盈交给 ROI 阶梯（`roi_table`）与快平任务的 ROI 止盈判定；
- 均值回归仓路径不变（它有自己的 `mr_sl_pct` / `mr_tp_pct` 独立阈值，且已通过 `margin_stop_take` 落库）。

> **注意**：`take_profit = 0` 会让 `apply_review` 的「缺少有效止损/止盈价，无法开仓」检查失败。
> 需同步放宽该检查（见 F3 末尾的「校验同步」小节）：开仓只要求 `stop_price > 0`，`take_profit` 可为 0。

### F2 【核心】波动保护下限接回引擎（替代被删掉的 prompt 约束）

F1 引入 `stop_distance_atr_mult`（新参数，`risk` 分组），语义：**止损价格距离 ≥ 该倍数 × ATR%**。

**新增配置**（`default_settings.json` + `param_registry.rs`）：

~~~json
"stop_distance_atr_mult": 1.3
~~~

`param_registry.rs` 注册（参照 `atr_stop_mult` 写法）：

~~~rust
("risk", "stop_distance_atr_mult", 1.0, 4.0, 0.1),
~~~

**效果验算**：

| 标的 | ATR% | 止损价距（1.3×ATR） | 旧价距（10%÷lev） | 改善 |
|---|---|---|---|---|
| CFGUSDT | 2.51 | 3.27% | 1.25%（lev 8） | 2.6 倍 |
| 2ZUSDT | 2.22 | 2.89% | 1.11%（lev 9） | 2.6 倍 |
| BEAMXUSDT | 1.84 | 2.39% | 1.43%（lev 7） | 1.7 倍 |
| RIFUSDT | 3.55 | 4.62% | 1.11%（lev 9） | **4.2 倍** |

---

### F3 【核心】0.5 秒快平任务接管「单仓止盈止损率」判定

这是本次要求的落点。`fast_risk.rs` 已经每 0.5 秒跑一次，且**已经从 `app_settings` 实时读 `global_stop_loss_pct` / `global_take_profit_pct`**（`fast_risk.rs:145-146`）——
**这条链路本来就通**，只是被开仓换算出来的 `sl_price` 抢了优先级。

**改动 1：`fast_risk.rs:89` 一带，把判定顺序改为「先单仓价，再全局 ROI 兜底」，并让 ATR 保护价优先。**

~~~rust
// 判定输入扩展：带上该仓位的 ATR 快照与 ROI 预算。
pub(crate) struct FastRiskInput<'a> {
    side: &'a str,
    entry: f64,
    mark: f64,
    sl: f64,          // ATR 保护价（F1 写入）
    tp: f64,
    leverage: f64,
    invalidation_price: Option<f64>,
    global_stop_pct: f64,   // 单仓止盈止损率：ROI 口径，强制平仓线
    global_tp_pct: f64,
}

fn fast_risk_reason(input: &FastRiskInput) -> Option<&'static str> {
    // ... 非法价格放行 ...
    let direction = if input.side == "long" { 1.0 } else { -1.0 };

    // ① AI 失效价（AI 的否决意见，最优先）
    if let Some(ip) = input.invalidation_price.filter(|v| *v > 0.0 && v.is_finite()) {
        if (direction > 0.0 && input.mark <= ip) || (direction < 0.0 && input.mark >= ip) {
            return Some("invalidation");
        }
    }

    // ② ATR 保护价（F1 写入的价格距离，不随杠杆收缩）
    if input.sl > 0.0
        && ((direction > 0.0 && input.mark <= input.sl) || (direction < 0.0 && input.mark >= input.sl))
    {
        return Some("stop_loss");
    }

    // ③ 单仓止盈止损率：ROI 口径的账户风险预算，整仓强制平仓
    let margin_pct = (input.mark - input.entry) * direction / input.entry * 100.0
        * input.leverage.max(1.0);
    if input.global_stop_pct > 0.0 && margin_pct <= -input.global_stop_pct {
        return Some("global_stop_loss");
    }
    if input.global_tp_pct > 0.0 && margin_pct >= input.global_tp_pct {
        return Some("global_take_profit");
    }

    // ④ 单仓止盈价（趋势仓 F1 置 0，均值回归仓仍有值）
    if input.tp > 0.0
        && ((direction > 0.0 && input.mark >= input.tp) || (direction < 0.0 && input.mark <= input.tp))
    {
        return Some("take_profit");
    }
    None
}
~~~

**为什么这样排优先级**：

- **② ATR 保护价必须排在 ③ ROI 兜底之前** —— 否则 ROI 线（1.11%）永远先于 ATR 线（4.62%）触发，F2 的修复等于没做。这是 F2/F3 必须同批改的原因。
- **① 失效价最优先** —— 保持参数页「与 AI 失效价先触发者生效」的原有语义。

**改动 2：`fast_risk.rs:193` 一带，取阈值的来源统一。**

~~~rust
// 当前：均值回归仓用 limits 覆盖全局，趋势仓用全局。
let limits = crate::mean_reversion::position_margin_limits(features.as_deref());
let reason = fast_risk_reason(&FastRiskInput {
    ...
    global_stop_pct: limits.map_or(global_stop, |limits| limits.0),
    global_tp_pct: limits.map_or(global_tp, |limits| limits.1),
});
~~~

**这段代码本身无需改动** —— 它已经正确地从 `app_settings` 实时读取参数，且均值回归仓的独立阈值覆盖机制也在这里。
**关键在于 F1 让趋势仓的 `sl_price` 变成 ATR 保护价后，②③ 的顺序才正确。**

> **一处必须同步**：`runtime.rs:2290` 的 `margin_stop_take`（均值回归独立阈值）机制保持不变。
> 但 F1 之后，趋势仓不再有全局阈值落库 —— 若需要「阈值随仓位冻结」语义，见 F4。

### F4 【建议】单仓止盈止损率的语义澄清

F1 之后，`global_stop_loss_pct` / `global_take_profit_pct` **完全由快平任务实时读取**，这带来两个语义变化，需要在参数页写清楚：

| | 改前 | 改后 |
|---|---|---|
| 参数改动对**已开仓位**的影响 | 立即生效（快平任务每 0.5 秒重读） | **立即生效** |
| 参数与开仓时的关系 | 同时决定 `stop_price` | **不再参与开仓** |
| 触发后的平仓范围 | 整仓 | 整仓（不变） |

**参数页提示语建议更新**（`settingsCatalog.ts:120`）：

~~~ts
hint: '账户风险预算（保证金收益率）：亏/盈达到该比例即强制平仓，由 0.5 秒快平任务实时判定；
       止损价另由波动保护（stop_distance_atr_mult × ATR%）给出，两者先到者生效；0=关闭'
~~~

---

## 三、改动清单（文件级）

| 文件 | 改动 | 优先级 |
|---|---|---|
| `rust/nanoquant/src/runtime.rs` | **F1** 开仓不再换算止损/止盈价；`take_profit` 趋势仓置 0；放宽止盈价校验 | **P0** |
| `rust/nanoquant/src/fast_risk.rs` | **F3** 判定顺序改为 失效价 → ATR 保护价 → ROI 兜底 → 单仓止盈价 | **P0** |
| `rust/nanoquant/src/manage.rs` | **F3** 480 秒周期任务同步新顺序（否则两套判定不一致） | **P0** |
| `rust/nanoquant/src/default_settings.json` | 新增 `risk.stop_distance_atr_mult: 1.3` | **P0** |
| `rust/nanoquant/src/param_registry.rs` | 注册 `stop_distance_atr_mult`（1.0–4.0，步长 0.1） | **P0** |
| `rust/nanoquant/src/backtest_engine.rs` | 回测同步 F1/F3：开仓换算（479-480）、开仓闸门（483）、平仓顺序（274-284） | **P0** |
| `frontend/src/views/settingsCatalog.ts` | F4 参数页提示语更新 | P1 |
| `rust/nanoquant/src/review.rs` | **两处**「缺少有效止损/止盈价」校验（269 / 312）放宽为只查 `stop_price` | **P0** |
| `rust/nanoquant/src/live.rs` | 无需改动（已验证 0 价腿被跳过），但需回归确认保护单只挂止损腿 | P1（验证） |

---

## 四、必须同步的四处（否则会出事）

### 4.1 `manage.rs` 的 480 秒周期任务

`manage.rs:90-91` 一带的 `manage()` 与快平任务**是两套独立判定**。F3 只改快平任务的话，
480 秒周期任务仍会按「ROI 兜底第 1 优先级」平仓 → 两套判定打架。**必须同批改**：

~~~rust
// manage.rs：与 fast_risk_reason 保持同一顺序
if let Some(ip) = ... { /* 失效价 */ }
if pos.sl_price > 0.0 && hit(direction, pos.mark_price, pos.sl_price, true) {
    return ManageAction::Close { reason: "stop_loss".to_string() };
}
if stop_loss_pct > 0.0 {
    let margin_pct = price_pct * pos.leverage.max(1.0);
    if margin_pct <= -stop_loss_pct {
        return ManageAction::Close { reason: "global_stop_loss".to_string() };
    }
}
~~~

> `ManagedPosition` 目前**没有** `invalidation_price` 字段（`manage.rs:28`），失效价只在快平任务里判。
> 若要把失效价也移进周期任务，需给 `ManagedPosition` 加字段并从 `positions.features` 读 —— **建议本次不做**，
> 周期任务只负责 sl/tp 与 ROI 兜底，失效价继续由快平任务独占（原本就是这样）。

### 4.2 `review.rs` 的开仓校验 —— **有两处，必须同批改**

同一条校验在 `review.rs` 里出现了**两次**，分属两条互斥路径：

~~~rust
// review.rs:269 —— AI 复核不可用、且未要求复核时的 fail-open 分支
if proposal.stop_price <= 0.0 || proposal.take_profit <= 0.0 {
    return Err(ReviewError::new("缺少有效止损/止盈价，无法开仓", "reject"));
}

// review.rs:312 —— apply_decision 主路径（AI 复核正常返回后）
if proposal.stop_price <= 0.0 || proposal.take_profit <= 0.0 {
    return Err(
        ReviewError::new("缺少有效止损/止盈价，无法开仓", "reject").with_meta(&decision)
    );
}
~~~

F1 把趋势仓 `take_profit` 置 0 后，**这两条都会拒掉趋势仓开仓**。必须都改成只查止损价：

~~~rust
if proposal.stop_price <= 0.0 {
    return Err(ReviewError::new("缺少有效止损价，无法开仓", "reject"));
}
~~~

> **只改 312 漏改 269 是隐蔽的坑**：AI 供应商故障时复核不可用，流程退到 269 的 fail-open 分支，
> 同样被拒 —— 表现为「AI 一超时就不开仓」，而不是「完全不开仓」，极难定位。

**这是 F1 最容易踩的坑 —— 漏改会导致引擎完全不交易（表现为"没有任何新开仓"）。**

### 4.3 交易所侧保护单 —— 已验证无需改动，但必须回归

`runtime.rs:1096` 在保护单腿为空时报「仓位缺少有效止损/止盈价」。经查 `live.rs:508-509`：

~~~rust
push("STOP_MARKET", sl_price, "sl");
push("TAKE_PROFIT_MARKET", tp_price, "tp");
~~~

`push` 闭包内 `if !price.is_finite() || price <= 0.0 { return; }` —— **价格为 0 的腿被直接跳过**。
所以 `take_profit = 0` 时只挂止损腿、跳过止盈腿，`legs` 非空，`runtime.rs:1096` 不会触发。

> 这带来一个必须写进文档的后果：F1 之后趋势仓**只有交易所侧止损腿、没有止盈腿**，
> 止盈完全依赖 0.5 秒快平任务与 ROI 阶梯。
> 这正是「单仓止盈止损率退出量化决策、链接到 0.5 秒任务」的必然结果，
> 但也意味着**快平任务必须真的在跑** —— `fast_risk.rs:133` 明确 `mode != "live"` 直接返回，
> 任务本体在 `fast_risk.rs:31` 由 `main.rs:330` 拉起，循环间隔 `fast_risk.rs:46` 为 500 毫秒。
> demo 模式下趋势仓将**完全没有止盈出口**，这一点必须在参数页与运维文档里写明。

### 4.4 F1 的连带影响（落库与日志）

F1 改的是 `stop_price` / `take_profit` 的**生成口径**，下游有三处跟着变，必须逐条回归：

| 位置 | 现状 | F1 后 | 是否要改代码 |
|---|---|---|---|
| `runtime.rs:3867` `INSERT INTO positions` | `sl_price=entry.stop`、`tp_price=entry.take_profit`、`initial_sl_price=entry.stop` | `tp_price` 落库为 0 | 否 —— `manage.rs:117` 与 `fast_risk.rs:109` 都是 `tp > 0.0` 才判定，0 自动跳过 |
| `runtime.rs:4388` 决策日志 `target_pct` | `(entry.take_profit - avg_price).abs() / avg_price * 100` | take_profit=0 → **target_pct 被记成 100%** | **是** —— 否则日志与箴言学习会吃到假的 100% 目标 |
| `runtime.rs:2321` `initial_sl` | `if initial_sl > 0.0 { initial_sl } else { sl }` | initial_sl = ATR 止损价 | 否 —— 反而更好：R 单位 = ATR 风险，与 F2 的 R 口径一致 |

> `runtime.rs:4388` 这条容易漏：它不报错、不崩，只是把目标价算错，**污染决策日志与 mantra / experience 的学习输入**，
> 属于「看起来没问题、但长期失真」那类问题。建议改为：止盈价缺失时 `target_pct` 落空或按 ROI 预算折算，
> 不要用 0 价硬算。

---

## 五、回测必须同步

项目已有明确教训：`backtest_engine.rs` 里 `partial_take_profit_r_matches_the_live_manage_rule` 测试，就是为了防止回测与实盘管仓口径漂移。

本次 F1/F3 会让回测与实盘**再次分叉**，必须同步：

1. **开仓换算**：`backtest_engine.rs:479-480` 与 `runtime.rs:3127-3128` 完全同构（同样是 `÷ leverage`），
   必须一并改为 ATR 保护价 —— 否则回测仍在复现「杠杆越高止损越窄」的病根；
2. **开仓闸门**：`backtest_engine.rs:483` 的 `if sl_pct <= 0.0 || tp_pct <= 0.0 { continue; }` 会让
   `take_profit = 0` 的趋势仓在回测里**被全部跳过** —— 必须同步放宽为只查 `sl_pct`；
3. **平仓判定**：回测的 ROI 兜底取自 `backtest_engine.rs:274-284`（趋势仓取 `manage.global_*`、
   均值回归仓取 `mr_*`），需复制 F3 的「失效价 → ATR 保护价 → ROI 兜底 → 单仓止盈价」顺序；
4. `backtest_engine.rs:1519` 的 `margin_stop_take: None` 按 features 填（均值回归）；
5. 新增断言：`backtest_stop_matches_live_stop`。

---

## 六、验收用例（10 项）

~~~rust
// 1) 止损距离与杠杆无关
#[test]
fn stop_distance_independent_of_leverage() {
    // ATR 3.55% → 止损价距恒为 1.3×3.55% = 4.615%，lev 5 与 lev 9 相同
}

// 2) 单仓止盈止损率不参与开仓
#[test]
fn global_stop_not_used_at_entry() {
    // 改 risk.global_stop_loss_pct 不应改变 stop_price
}

// 3) ATR 保护价优先于 ROI 兜底
#[test]
fn atr_stop_precedes_roi_budget() {
    // sl=95、entry=100、lev=9 → mark=96 时应触发 stop_loss 而非 global_stop_loss
}

// 4) ROI 兜底仍然生效
#[test]
fn roi_budget_still_triggers() {
    // sl=0（无 ATR 保护价）时按 global_stop_loss_pct 平仓
}

// 5) 失效价最优先
#[test]
fn invalidation_has_highest_priority() {
    // 同时满足失效价与 ATR 价时返回 invalidation
}

// 6) 回测与实盘一致
#[test]
fn backtest_stop_matches_live_stop() {
}

// 7) 趋势仓 take_profit=0 仍可开仓
#[test]
fn trend_entry_allowed_without_take_profit() {
    // 防止漏改 review.rs 校验导致完全不开仓
}

// 8) fail-open 分支同样放行（review.rs:269）
#[test]
fn fail_open_branch_allows_missing_take_profit() {
    // AI 不可用时的分支也必须放行，否则表现为「AI 一超时就不开仓」
}

// 9) 交易所保护单：趋势仓只挂止损腿
#[test]
fn protection_legs_skip_zero_take_profit() {
    // tp=0 时 legs 只含 STOP_MARKET，不含 TAKE_PROFIT_MARKET
}

// 10) 决策日志不落假 target_pct
#[test]
fn journal_target_pct_not_fake_when_take_profit_zero() {
    // take_profit=0 时不得记成 100%
}
~~~

---

## 七、执行顺序

~~~
P0-1  F1 + review.rs 两处校验放宽（269 / 312）  ← 必须同批，否则引擎不开仓
P0-2  F3 快平任务判定顺序 + manage.rs 同步  ← 必须同批，否则两套判定打架
P0-3  default_settings + param_registry + backtest_engine（含 479-480 / 483）+ 10 个断言测试
P0-4  参数页提示语更新（F4）
P0-5  runtime.rs:4388 决策日志 target_pct 修正（take_profit=0 时不落假 100%）
~~~

> **F1 与 review.rs 两处校验必须同批**：漏改 → 引擎一单不开；只改 312 漏改 269 → AI 超时时不开仓。
> **F1 与 F3 必须同批**：只改 F1 不改 F3 → ROI 兜底仍抢在 ATR 保护价前面，修复等于没做。
> **F1 与 backtest_engine 必须同批**：回测不同步则又是一次口径分叉，回测结果不可信。

---

## 八、遗留项（不在本次范围）

| 项 | 位置 | 说明 |
|---|---|---|
| reviewed 落库常量 | `execution.rs:270` | `VALUES(...,0,'',...)` 写死 0/''，开仓的 AI 结论不可追溯。P0–P4 未覆盖。 |
| GA 证据门维度 | `[D3]` 未提交 | GA 用保证金收益率口径衡量，看不到「价格止损被压缩」，需加「止损距离/ATR」维度。 |
| 部署滞后 | `scripts/update.sh` 手动 | 8 小时的修复未上线。建议 CI 加部署步骤或至少在提交后提示。 |
| veto_only 语义 | `review.rs` | P4 移除 AI 覆盖后 `veto_only` 已成空壳，可在 P5 清理（`d1952d0` 已在做类似清理）。 |

---

## 九、方案要点速览

1. **「单仓止盈止损率」退出量化决策** —— 开仓不再用它换算 `stop_price` / `take_profit`（F1）；
2. **链接到已有的 0.5 秒快平任务** —— 该任务本就每 0.5 秒实时读 `app_settings`，链路已通，只需调判定顺序（F3）；
3. **止损距离改由 ATR 决定，与杠杆解耦** —— `stop_distance_atr_mult × ATR%`，把被删掉的 1.3×ATR 约束接回引擎侧（F2）；
4. **三层止损同处一个任务**：失效价 → ATR 保护价 → ROI 兜底，先到者生效；
5. **不新增任何 AI 权限** —— 完全符合半 AI + 半量化：AI 仍只否决与降仓，风险参数全部由量化计算。
