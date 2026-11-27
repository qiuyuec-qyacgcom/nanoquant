# NanoQuant 窗口期改动分析：胜率塌陷与亏损归因

> 分析窗口：2026-11-25 23:00 → 2026-11-27 00:00（北京时间）
> 代码范围：`F:\cb\nanoquant` @ `ff007f7`（分支 main，领先 origin/main 1 个提交）
> 线上实例：http://185.242.232.210:8899（实盘 · 币安 U 本位 · live 模式）

---

## 一、结论速览

窗口内共 **41 个提交**、**3440 行新增**。胜率塌陷与亏损并非单一 bug，而是**三层改动叠加**的结果：

| 层级 | 根因 | 关键证据 |
|---|---|---|
| **L1 主因** | 止损口径 = 保证金收益率 ÷ 杠杆，价格止损被压缩到 **0.5×ATR 以内**，必然被噪音扫掉 | 5 笔亏损单的止损价全部落在 0.50–1.23×ATR 区间 |
| **L2 放大器** | 杠杆被 AI 上调到 7–9x，等于把止损距离再压缩 40% | 窗口前平均 5.0x → 窗口内 7.6x，最大 9x |
| **L3 掩护** | `b28196f` 在 4 分钟内回滚了 `a294c31`（低波动时不再放大保证金），把放大效应**保留**了下来 | 提交时间 02:29 → 02:33 |

**核心机制**：`risk.global_stop_loss_pct = 10` 是**保证金收益率**口径。真实价格止损距离 = `10% ÷ 杠杆`：

| 杠杆 | 价格止损距离 | 标的 ATR% | 止损/ATR | 结果 |
|---|---|---|---|---|
| 5x（窗口前） | 2.00% | ~1.8–2.5% | ~1.0×ATR | 可存活 |
| 8x（CFGUSDT） | 1.25% | 2.51% | **0.50×ATR** | 必被扫 |
| 9x（2ZUSDT） | 1.11% | 2.22% | **0.50×ATR** | 必被扫 |
| 7x（BEAMXUSDT） | 1.43% | 1.84% | **0.78×ATR** | 必被扫 |

---

## 二、硬数据：胜率断崖

窗口边界按**开仓时间**切分（`opened_at`）：

| 分组 | 笔数 | 胜 | 负 | 胜率 | 净盈亏 | 均盈利 | 均亏损 | 平均杠杆 |
|---|---|---|---|---|---|---|---|---|
| **窗口前**（11-25 23:00 前开仓） | 16 | 12 | 4 | **75%** | **+9.679** | +1.491 | −2.053 | 5.00 |
| **窗口内**（11-25 23:00 ~ 11-27 00:00） | 5 | 1 | 4 | **20%** | **−7.632** | +0.081 | −1.928 | 7.60 |
| 合计 | 21 | 13 | 8 | 61.9% | +2.047 | +1.383 | −1.991 | 6.19 |

窗口内 5 笔明细：

| # | 标的 | 方向 | 开仓 | 平仓 | 杠杆 | 名义额 | 价格% | ROI% | 盈亏 | 平仓原因 | reviewed |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 17 | BLURUSDT | long | 01:46 | 02:05 | 5 | 5.26 | +1.54% | +7.68% | **+0.081** | manual | True |
| 18 | XAUUSDT | short | 02:11 | 04:16 | 9 | 187.43 | −1.20% | −10.81% | **−2.250** | manual | True |
| 19 | CFGUSDT | long | 05:01 | 05:11 | 8 | 151.50 | −2.04% | −16.33% | **−3.092** | venue_sync | True |
| 20 | BEAMXUSDT | short | 16:28 | 17:44 | 7 | 145.25 | −1.50% | −10.53% | **−2.186** | global_stop_loss | **False** |
| 21 | 2ZUSDT | long | 23:38 | 23:40 | 9 | 16.44 | −1.12% | −10.08% | **−0.184** | global_stop_loss | **False** |

**亏损形态高度一致**：4 笔亏损的 ROI 全部落在 −10.08% ~ −16.33%，即**保证金收益率口径的全局止损线附近**（`global_stop_loss_pct = 10`）。这不是"判断错了方向"，而是**被止损线机械扫出**。

---

## 三、L1 主因：止损口径与杠杆的错配

### 3.1 代码路径

`rust/nanoquant/src/manage.rs:78`（`manage()`）：

```rust
let margin_pct = price_pct * pos.leverage.max(1.0);
if stop_loss_pct > 0.0 && margin_pct <= -stop_loss_pct {
    return ManageAction::Close { reason: "global_stop_loss".to_string() };
}
```

`global_stop_loss_pct` 线上值 = **10**（`/api/v1/settings` → risk 分组）。因此：

**真实价格止损距离 = 10% ÷ 杠杆**

而 `fast_risk_reason()`（runtime.rs 约 5410 行，窗口内由 `1a25eac` 引入）用的是**同一套口径**，且被排到**第一优先级**：

```rust
let margin_pct = (mark - entry) * direction / entry * 100.0 * leverage.max(1.0);
if global_stop > 0.0 && margin_pct <= -global_stop { return Some("global_stop_loss"); }
if global_tp > 0.0 && margin_pct >= global_tp { return Some("global_take_profit"); }
if let Some(ip) = invalidation_price { ... }      // 新增
if sl > 0.0 && ... { return Some("stop_loss"); }   // ATR 单仓止损排在第 4 位
if tp > 0.0 && ... { return Some("take_profit"); }
```

### 3.2 与标的波动的比值（决定性证据）

用线上 signals 的真实 ATR% 算「止损距离 ÷ ATR」：

| 标的 | 杠杆 | ATR% | 全局止损价距 | 比值 | 实际亏损价% | 结论 |
|---|---|---|---|---|---|---|
| XAUUSDT | 9 | 0.906 | 1.111% | 1.23×ATR | −1.20% | 勉强 |
| CFGUSDT | 8 | 2.512 | 1.250% | **0.50×ATR** | −2.04% | **必扫** |
| BEAMXUSDT | 7 | 1.836 | 1.429% | **0.78×ATR** | −1.50% | **必扫** |
| 2ZUSDT | 9 | 2.222 | 1.111% | **0.50×ATR** | −1.12% | **必扫** |

**比值 < 1 意味着止损线落在单根 K 线噪音之内**。3 笔亏损单的比值都在 0.5–0.78，任何正常回撤都会触发。而窗口前 5x 杠杆时比值约 1.0×ATR，尚能存活。

### 3.3 关键设计缺陷

代码里**已有**正确的 ATR 止损逻辑，但它被全局止损**抢先**：

- `risk.atr_stop_mult = 1.3`，`sl_atr_mult`/`tp_atr_mult` 也存在 → 期望是「1.3×ATR 的价格止损」
- 但 `manage()` 里 `global_stop_loss` 在第 1 位、ATR `sl_price` 在第 4 位
- AI 提示词也明确要求：「止损保证金比例 s 必须满足 s ≥ constraints.atr_stop_mult × features.atr_pct × l」（ai_analysis.rs:359）

**AI 按 1.3×ATR 算出的止损，根本没机会生效——全局 10% 保证金止损先触发。** AI 的止损建议与引擎实际执行的不是同一套口径。

---

## 四、L2 放大器：杠杆被 AI 抬到 7–9x

### 4.1 杠杆口径的变更链

| 时间 | 提交 | 效果 |
|---|---|---|
| 11-22 02:10 | `3584ca4` | 杠杆改由 AI 在全局上下限内决定（`proposal.leverage` 退化为下限） |
| 11-25 16:23 | `2087c6a` | 提示词加「相对杠杆三档引导」：**禁止无条件取 min_leverage**，要求按置信度取区间中上段 |
| 11-26 02:44 | `d84ba8d` | 本地规则决定杠杆（窗口后，尚未上线） |

提示词原文（ai_analysis.rs:351-360）：

```
杠杆 l 必须在 constraints.min_leverage ~ constraints.max_leverage 内，按置信度 c 分三档：
c ≥ 0.70：取区间上半段（建议 l ≥ min + 70% 区间宽度）；
0.50 ≤ c < 0.70：取区间中段；
c < 0.50：观望（H），不得开仓。
禁止无条件取 min_leverage；min_leverage 只是硬下限，不是建议值
```

线上 `min_leverage=5, max_leverage=16` → 「取区间上半段」= **8–16x**。

### 4.2 AI 实际输出证实

线上 `ai_call_logs` 的 `raw_text`（11-26 23:37–23:38，恰好是 2ZUSDT 开仓时刻）：

```json
{"a":"L","c":0.55,"l":9,"m":16,"s":26,"t":52,...}
{"a":"L","c":0.56,"l":8,"m":15,"s":45,"t":90,...}
{"a":"L","c":0.62,"l":8,"m":18,"s":22,"t":45,...}
```

AI 直接下发 `l=9`（2ZUSDT 那笔，11-26 23:38 开仓）、`l=8`。**与成交记录完全吻合**（trade 21 杠杆 9、trade 19 杠杆 8）。

### 4.3 杠杆放大如何转化为亏损放大

止损距离 = 10% ÷ 杠杆，所以：

- 5x → 价格止损 2.00%
- 8x → 价格止损 1.25%（**再缩 37%**）
- 9x → 价格止损 1.11%（**再缩 44%**）

**杠杆翻倍，止损距离腰斩**。这是 1.23×ATR → 0.50×ATR 的直接来源。

> 注：`clamp_leverage`（review.rs:260）只夹上限，不给下限做合理性收敛；`local_leverage`（d84ba8d，窗口后）才把杠杆收归本地规则。

---

## 五、L3 掩护：4 分钟内回滚的保证金放大修复

窗口内最值得注意的一对提交：

```
02:29  a294c31  fix(risk): 单仓保证金不再因低波动放大
         → draft.scale 上限 2.0 → 1.0（禁止低波动时放大保证金）
         → margin_pct 乘法 clamp(0.25, 2.0) → clamp(0.25, 1.0)

02:33  b28196f  Revert "fix(risk): 单仓保证金不再因低波动放大"     ← 4 分钟后回滚
```

回滚后，`33304da`（02:51）又引入 AI 自主调保证金：

```rust
let margin_pct = reviewed.margin_pct.unwrap_or_else(|| {
    (cfg["margin_pct"].as_f64().unwrap_or(50.0) * draft.scale.clamp(0.25, 2.0)).clamp(0.0, 100.0)
});
```

夹取区间由 `margin_base ± 5%` 决定（线上 `margin_pct=20` → AI 可在 15–25% 间浮动）。**这条链路净效果 = AI 拿到的 scale 上限仍是 2.0，且 AI 还能再叠加一层自己的 margin_pct。**

---

## 六、其余窗口内改动的定性归类

41 个提交按影响面归类（**只有前 4 类真正改变了交易行为**）：

### A. 改变开仓/持仓决策（真凶）

| 提交 | 时间 | 说明 |
|---|---|---|
| `b28196f` | 02:33 | 回滚保证金放大修复（L3） |
| `33304da` | 02:51 | AI 自主调单仓保证金 ±5% |
| `1a25eac` | 23:19 | AI 失效价接入快速平仓（新增第 3 优先级判定） |
| `715f22f` | 23:57 | 修正开关作用域 + 预算下限语义 |

### B. 改变止损/平仓触发

| 提交 | 时间 | 说明 |
|---|---|---|
| `bdc7e62` | 05:41 | **新增 0.5 秒快速止盈止损任务** |
| `f2760d8` | 23:25 | 快速风控输入具名化（重构，无语义变化） |
| `1a25eac` | 23:19 | 失效价快平 |

### C. 改变 AI 协议与门槛（间接影响胜率）

| 提交 | 时间 | 说明 |
|---|---|---|
| `61de345` | 22:08 | v2 协议支持中文文本与扩展数值字段 |
| `c8cd5ad` | 22:16 | v2 解析的空白/数字容错与预算顺序 |
| `59f089f` | 22:20 | v2 提示词说明扩展字段并放宽输出预算 |
| `3760ec8` | 22:25 | 按 provider 上限计算 max_tokens |
| `2d87a20` | 22:43 | 数据质量与盈亏比低于门槛时拒单 |
| `829406d` | 22:50 | 门槛拒单保留置信度且不影响 hold/close |

### D. 界面 / 审计 / 文档（无交易影响）

`c4d17d0`、`8c49f0f`、`ddd1fea`、`bd64c3c`、`7e4eacd`、`595e439`、`9d8c1dc`、`121de90`、`8bd10d4`、`2f1802f`、`0a74712`、`134e667`、`c50f6e3`、`19a850f`、`8bdf7b5`、`8143d43`、`5e57745`、`8cadbe9`、`5ac25e3`、`d59641e`、`f3b6b03`、`03cb539`、`19fbe2c`、`9863c05`、`2ee7341`、`3c6d979`、`1eec602`、`a294c31`、`877b227`

> 其中 `3c6d979`/`1eec602`（02:20–02:25）重构了钱包/可用余额的刷新口径，`f3b6b03`（04:20）引入全局逐仓开关 —— 这两个改了**权益与保证金快照来源**，但线上 risk 分组显示 `isolated=true`、`margin_pct=20`，不构成亏损主因。

### E. 窗口后（27日0点后，已在仓库但不属于本次窗口）

`0714af8`、`6588de8`、`c179e2e`、`e972b3f`、`2fb8d64`、`eb64c9e`、`29a0351`、`a06b796`、`678c05b`、`3a07c8f`、`667e52f`、`0370088`、`6c5e733`、`d84ba8d`、`34a74e2`、`6ce417b`、`ff007f7`

其中 **`0370088`（复核改为否决与降仓模式）** 与 **`d84ba8d`（本地规则决定杠杆）** 正面命中了本报告的 L1/L2 根因 —— 但它们在窗口之后。

---

## 七、两个必须单独指出的系统性问题

### 7.1 窗口内最后两笔开仓 `reviewed=False` —— 审计字段缺失

| trade | 开仓 | reviewed | 说明 |
|---|---|---|---|
| 20 BEAMXUSDT | 11-26 16:28 | **False** | audit 无 AI 字段 |
| 21 2ZUSDT | 11-26 23:38 | **False** | signals 表中 `ai_review` 为空 |

代码证据：`execution.rs:270` 的 INSERT 写死了 `reviewed` 常量 **0**、`review_note` 常量 **''**：

```sql
INSERT INTO trades(...,mode,reviewed,review_note,features,mantra_ids)
VALUES(?1,...,?20,0,'',?21,'[]')
```

即：**成交落库时 `reviewed` 永远为 0**，窗口前那 16 笔的 `reviewed=True` 是后续流程回写的，窗口内后两笔（16:28 之后开的）尚未回写完成。**这是审计字段缺失，不是 AI 复核被跳过** —— 但它导致「窗口内开仓决策的 AI 复核结论不可追溯」。

### 7.2 AI 供应商在 23:46 后大面积失败，开仓闸门实际处于失效状态

`ai_call_logs` 错误统计：

| 时段 | 错误 |
|---|---|
| 11-26 23:16 | 3 次 `输出 max_tokens=3000 截断 (finish_reason=length)` |
| 11-26 23:46 → 11-27 00:29 | **连续 100+ 次** `HTTP 503 no_account`（上游无可用账号） |
| 11-27 00:53 | `HTTP 429 上游限流` + `503 no_account` |

注意线上 provider `cb` 的 `max_tokens=9000`，而 `715f22f` 之后复核走 `min_max_tokens=true`（按 provider 上限取大）。**`max_tokens=3000 截断` 说明部分请求仍以 3000 预算发出并被硬截断**，导致 JSON 解析失败 → 该次复核失败。

后果链条：

1. 复核失败 → `apply_review` 返回 `ReviewError("AI 复核失败，按 fail-open 降仓")`
2. runtime 收到 Err → **跳过该笔开仓**（`continue`），本就不会开仓
3. 但 23:38 那笔 2ZUSDT **开出来了** —— 说明它走的是 23:38 之前那次**成功**的复核（`act=L c=0.55 l=9`）

所以供应商故障没有直接制造亏损，但**它使得「AI 复核」这一最后防线在窗口后半段事实上失效** —— 剩下的唯一防线是量化信号 + 全局止损，而后者正是 L1 病灶。

---

## 八、修复建议（按优先级）

### P0：立刻止血 —— 把止损口径与杠杆解耦

止损距离不应随杠杆线性收缩。三个可选方案：

1. **全局止损改为价格口径**（推荐）：新增 `global_stop_loss_price_pct`，直接按标的价格距离止损，杠杆只影响仓位大小不影响止损距离。
2. **止损距离下限**：`price_stop_distance >= atr_stop_mult × atr_pct`，即 ATR 止损优先于全局止损 —— 调整 `manage()` 与 `fast_risk_reason()` 的判定顺序，把 `sl_price`（ATR 止损）提到 `global_stop_loss` 之前。
3. **保底比值约束**：`止损距离 / ATR ≥ 1.0`，不足则自动下调杠杆。

代码位置：

- `rust/nanoquant/src/manage.rs:78` `manage()` — 判定顺序
- `rust/nanoquant/src/runtime.rs` `fast_risk_reason()` — 判定顺序
- `rust/nanoquant/src/runtime.rs:3138` — `stop_price` 默认值按 `global_stop / leverage` 换算

### P1：杠杆收归本地规则，不再由 AI 决定

`d84ba8d`（窗口后）已引入 `local_leverage`，但线上尚未生效（catalog 中无 `local_leverage_base`）。需要确认部署，并把 `local_leverage_score_weight` 调低，避免 AI 一步跳到 9x。

同时，`clamp_leverage`（review.rs:260）应增加**上限收敛**或**止损距离联动校验**：AI 给的 `l` 必须满足 `s >= atr_stop_mult × atr_pct × l`（提示词已要求，代码未强制）。

### P2：把「AI 建议止损」与「引擎执行止损」统一到同一口径

当前 AI 按 `s`（保证金收益率）出价，引擎把它换算成价格距离存进 `sl_price`，但 `global_stop_loss` 用另一套口径且优先级更高。**两套口径并存 = AI 的风控建议形同虚设。**

建议：AI 直接出**价格止损**，或引擎在采纳 AI 止损后**上抬** `global_stop` 阈值以容纳 ATR 止损。

### P3：恢复保证金放大修复

撤回 `b28196f` 的回滚（重放 `a294c31`），使低波动标的不会被放大 2 倍保证金。

### P4：修 AI 预算与供应商能力

- `715f22f` 引入的 `min_max_tokens` 取大逻辑，在 provider 实际能力只有 3000 时会硬截断 → 应**取小**或按 provider 实际上限夹紧
- `no_account` / `429` 需要 provider 侧账号池治理；
- 复核失败当前是「跳过开仓」，但**持仓的 AI 复核（hold/close）失败会静默丢失**，应落 engine_events 并可告警。

### P5：修复成交审计字段

`execution.rs:270` 的 `reviewed`/`review_note` 写死 0/''，导致窗口内开仓的 AI 复核结论不可追溯。应在 INSERT 时直接落 `reviewed` 与 `review_note`。

---

## 九、数据来源与可复核性

| 数据 | 来源 |
|---|---|
| 21 笔成交明细 | `GET /api/v1/engine/trades?limit=500` |
| 引擎统计（胜率/分标的/分原因） | `GET /api/v1/engine/stats` |
| 权益曲线（96h） | `GET /api/v1/engine/equity?hours=96` |
| 信号与 ATR/价差 | `GET /api/v1/engine/signals?limit=1000&symbol=...` |
| AI 复核原始输出 | `GET /api/v1/ai/logs?purpose=engine_review` |
| 线上参数真值 | `GET /api/v1/settings`（groups = 线上真值，defaults = 仓库默认） |
| 配置变更审计 | `GET /api/v1/settings/audit?limit=100` |
| 提交与代码 | `git log/diff/show` @ `F:\cb\nanoquant` |

**线上与仓库默认值的关键差异**（分析时以线上为准）：

| 参数 | 仓库默认 | 线上真值 |
|---|---|---|
| `risk.margin_pct` | 50 | **20** |
| `risk.max_leverage` | 100 | **16** |
| `risk.min_leverage` | 1 | **5** |
| `risk.global_stop_loss_pct` | 10 | 10 |
| `risk.global_take_profit_pct` | 20 | **15** |
| `risk.atr_stop_mult` | 1.3 | 1.3 |
| `strategy.timeframe` | 4h | **1h** |
| `strategy.atr_pct_max` | 4.0 | **10** |
| `ai.min_confidence` | 0.45 | **0.46** |
| `ai.allow_ai_override` | false | **true** |
| `ai.veto_only` | — | **未启用**（窗口内不存在该字段） |
| `engine.order_type` | market | market |
| `engine.isolated` | false | **true** |
| `engine.max_positions` | 2 | **3** |

> 线上 `strategy.roi_table` 为 `[]`，`min_entry_rr=0.8`；`risk.vol_target_atr_pct=2`。

---

## 十、诚实声明

1. **窗口只有 5 笔成交**，样本小。但 4 笔亏损的 ROI 全部聚在 −10% 止损线附近、止损/ATR 比值全部 < 1，机制解释是自洽的。
2. **部署时点未完全确定**：仓库窗口提交（11-26 02:03–23:57）与线上二进制版本无法 1:1 对齐。线上 catalog 含 `use_invalidation_price`（`c50f6e3`，11-26 22:28）、不含 `local_leverage_base`（`d84ba8d`，11-27 02:44）→ 线上二进制**大致对应 11-26 22:28 ~ 11-27 02:21 之间的提交**，即窗口内的 A/B/C 三类改动基本已上线，而窗口后的 `0370088`/`d84ba8d` 尚未上线。
3. **ATR 对照值**取自对应时间窗的 signals 记录（同一标的同一时刻），个别标的使用临近时点值。
4. `reviewed=False` 已核实为**落库常量缺陷**（非 AI 复核被跳过），但确实导致窗口内开仓的 AI 结论不可追溯。
5. 本报告未修改任何代码或线上配置，仅做只读分析。
