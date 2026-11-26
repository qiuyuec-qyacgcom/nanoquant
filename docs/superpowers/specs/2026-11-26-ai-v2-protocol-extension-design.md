# AI v2 协议扩展设计

日期：2026-11-26
状态：待用户审查

## 1. 背景与目标

当前 AI 决策协议为 v2（`decision_schema_version = 2`），模型只能输出固定短键，且：

- 理由 `r`、风险 `k`、失效条件 `x` 只能用预设整数码，模型无法自由表达；
- `f` 在提示词里要求中文，但 `localize` 只保留 ASCII，中文字符被全部过滤（`ai_client.rs:1194`）；
- 无法回传失效价、有效期、数据质量、盈亏比、建议入场价这些对复核和持仓管理有价值的信息。

目标：在不改变协议版本号（仍为 v2）的前提下扩展字段，放开中文文本表达，修复 `f` 过滤 bug，并让新增数值字段真正接入消费闭环。

## 2. 非目标

- 不新增 v3，`decision_schema_version` 保持 `2`。
- 不改动 v1 长字段的解析路径，旧数据继续兼容。
- 不改动信号跟单、回测、AI 调参链路。
- 不做与本次字段无关的重构。

## 3. 协议字段

模型仍只输出一个 JSON 对象，新增字段全部可选，缺失不报错。

```json
{
  "a": "L|S|H|C", "c": 0.62, "l": 8, "m": 20, "s": 30, "t": 60,
  "why": "EMA 多头排列，量比 1.4，OI 同步上升",
  "risk": "上方 0.25 有密集套牢盘",
  "inv": "跌破 EMA21 或量比回落到 0.8 以下",
  "ip": 0.2385, "vb": 4, "dq": 0.78, "rr": 1.8, "ep": 0.2412,
  "r": [1, 2], "k": [21], "x": 3, "f": "回踩确认后入场"
}
```

| 键 | 含义 | 边界 | 缺省行为 |
|---|---|---|---|
| `why` | 中文理由 | ≤800 字符 | 回退到 `r` 码表拼接 |
| `risk` | 中文风险 | ≤500 字符 | 回退到 `k` 码表 |
| `inv` | 中文失效条件 | ≤500 字符 | 回退到 `x` 码 |
| `f` | 中文补充 | ≤200 字符 | 允许中文（修复 bug） |
| `ip` | 失效价 | >0 且相对 mark 偏离 ≤10% | 视为未给 |
| `vb` | 有效期（K 线根数） | 1–24 | 视为未给，用 `strategy.valid_bars` |
| `dq` | 数据质量分 | 0–1 | 视为未给，不做门槛 |
| `rr` | 预期盈亏比 | 0–10 | 视为未给，不做门槛 |
| `ep` | 建议入场价 | >0 且相对 mark 偏离 ≤0.5% | 视为未给 |

保留规则：

- `r/k/x` 码表保留用于结构化统计；文本字段优先，码表仅作回退。
- `p` 参数更新维持现状。
- 四个文本字段共用总预算 2000 字符：`why ≤800`、`risk ≤500`、`inv ≤500`、`f ≤200`；逐个截断后按 `why → inv → risk → f` 顺序占用预算，超出部分丢弃。
- 现有 800/400/120 的旧单字段上限一并提升（见 4.4），总预算兜底为 2000。

## 4. 解析规则

### 4.1 结构体

`Decision`（`ai_client.rs:1007`）新增：

```rust
pub invalidation_price: Option<f64>, // ip
pub valid_bars: Option<i64>,         // vb
pub data_quality: Option<f64>,       // dq
pub rr_target: Option<f64>,          // rr
pub entry_price: Option<f64>,        // ep
```

`Option` 用于区分「未给」与「给了 0」。

### 4.2 映射

- `why` → `reason`，缺失回退 `r` 码表
- `risk` → `risk_flags`（单条文本），缺失回退 `k` 码表
- `inv` → `invalidation`，缺失回退 `x` 码
- `f` → 追加到 `reason`，允许中文

### 4.3 校验

在带 `ctx` 的 `parse_decision` 内完成：

- `ip`：`>0` 且 `|ip - mark| / mark <= 10%`，否则 `None`
- `vb`：clamp 到 `1..=24`
- `dq`：clamp 到 `0..=1`
- `rr`：clamp 到 `0..=10`
- `ep`：`>0` 且 `|ep - mark| / mark <= 0.5%`，否则 `None`

校验失败一律置 `None`，不阻断复核。

### 4.4 截断调整

现有 `normalized()` 会把长文本提前截短（`ai_client.rs:1036-1038`），必须同步调整：

- `reason`：800 → 2000（总预算兜底）
- `invalidation`：400 → 500（`inv` 单字段上限）
- `risk_flags` 单项：120（`ai_client.rs:1119`）→ 500（`risk` 单字段上限）
- `risk_flags.truncate(6)` 保持不变，`risk` 作为单条文本只占 1 条

## 5. 存储

| 数据 | 存储位置 |
|---|---|
| `dq/rr/vb/ip/ep` | `signals.features` JSON：`ai_dq / ai_rr / ai_vb / ai_ip / ai_ep` |
| 持仓失效价 | `positions.invalidation_price REAL`（`schema.rs` 通用 `ADD COLUMN` 迁移） |
| 原始输出 | `ai_call_logs.raw_text`，维持现有 2000 字符截断 |

选择 `features` JSON 而非新增 5 列：`signal_audit.rs:134` 已在写 `reason_key/stage`，无需改表基线即可落地；查询用 `json_extract(features,'$.ai_dq')`。

## 6. 消费接线

### 6.1 复核前置拒单（P2）

位置：`ai_review.rs` 的 `decision_to_review`，在方向一致性检查之前。

- `dq` 有值且 `< ai.min_confidence` → `reject("AI 数据质量分 0.35 低于 0.45")`
- `rr` 有值且 `< strategy.min_entry_rr` → `reject("AI 盈亏比 0.80 低于 1.00")`
- 缺省不判

### 6.2 valid_bars 落库（P2）

- `SignalAuditRow` 扩展 5 个 `Option` 字段
- `signal_audit.rs` 写入 `features` JSON
- 仅审计与展示，不改变下单行为

### 6.3 invalidation_price 失效平仓（P3）

- `positions` 新增 `invalidation_price` 列
- `ReviewDecision`、`EntryTerms`、`LivePlan` 逐层传递
- 写入点 `runtime.rs:3191` 的 INSERT 列清单增加该列
- `fast_risk_cycle`（`runtime.rs:5446`）多读一列，判定顺序：
  `global_stop/tp → invalidation → sl → tp`
- long `mark <= ip`、short `mark >= ip` → `exit_reason="invalidation"`
- 沿用现有 `closing` 集合防重复平仓

### 6.4 entry_price 限价（P3）

- 仅 `LimitPostOnly` 分支生效（`runtime.rs:3062`）
- long `ep <= mark`、short `ep >= mark`，且偏离 ≤0.5%
- 不满足条件回退现有 `limit_entry_price(side, bid, ask, mark)`
- 市价模式忽略 `ep`

### 6.5 开关

- `ai.use_invalidation_price`，默认 `false`
- `ai.use_entry_price`，默认 `false`

两个开关关闭时，字段只解析入库，不改变交易行为；验证后再开启。

## 7. 配置与默认值

本次不硬编码业务门槛，全部来自可调配置。

| 用途 | 配置项 | 来源 | 默认 | 范围 |
|---|---|---|---|---|
| 数据质量门槛 | `ai.min_confidence` | AI 模型页（已有） | 0.45 | 0–1 |
| 开仓盈亏比门槛 | `strategy.min_entry_rr` | 参数配置-策略（新增） | 0.8 | 0–10 |
| 信号有效期 | `strategy.valid_bars` | 参数配置-策略-基础（新增，排在 `timeframe` 下方） | 6 | 1–24 |
| 失效价开关 | `ai.use_invalidation_price` | 参数配置-AI（新增） | false | bool |
| 建议入场价开关 | `ai.use_entry_price` | 参数配置-AI（新增） | false | bool |

`strategy.valid_bars` 在参数页显示为「信号有效期（K 线根数）」，提示文案：AI 未给出有效期时使用；1 根 = 1 个当前 K 线周期，超过有效期后该信号不再作为开仓依据。前端插入位置为 `settingsCatalog.ts` 策略「基础」分区 `timeframe` 之后。

同步登记：

- `param_registry.rs` 的 `RANGES` 与 `VISIBLE`
- `default_settings.json`
- 前端 `settingsCatalog.ts` 与 `StrategyView.vue` 分组

`ai` 组参数在 `is_ai_writable` 中被统一禁止 AI 自调（`param_registry.rs:296`），门槛不会被模型自己降低。

### max_tokens

四个文本字段总预算 2000 字符，最坏输出约 3000–4000 tokens。复核调用前取：

```
max_tokens = max(provider.max_tokens, 16000)
```

16000 为协议安全下限；provider 配置更高时以 provider 为准。这是输出容量常量，不是业务参数。

## 8. 兼容性与回滚

- `decision_schema_version` 保持 `2`
- v1 长字段继续可解析
- 新增字段缺失时全部 `None`，旧模型/旧数据不受影响
- `ai_review.rs` 5 处 `Decision` 测试构造点（388/720/743/768/803）补 `None`
- P1 / P2 / P3 独立提交，可单独回滚
- P3 行为默认关闭，配置开启后才生效

## 9. 测试与验收

### 单元测试

- `f` 含中文时完整保留，不再被 ASCII 过滤
- `why/risk/inv` 存在时优先于 `r/k/x` 码表
- `why/risk/inv/f` 合计超 2000 字符时，按 `why → inv → risk → f` 顺序截断
- `ip` 偏离 mark 超过 10% 时置 `None`
- `ep` 偏离 mark 超过 0.5% 时置 `None`
- `vb/dq/rr` 越界被 clamp
- v1 长字段解析不受影响

### 行为测试

- `dq < ai.min_confidence` → 复核返回 `reject`，原因包含「数据质量分」
- `rr < strategy.min_entry_rr` → 复核返回 `reject`，原因包含「盈亏比」
- `ip` 触发时 `fast_risk_cycle` 平仓且 `exit_reason="invalidation"`
- `ep` 不满足方向/偏离条件时回退 `limit_entry_price`
- 开关关闭时 P3 行为不生效

### 验收断言

- 相同输入下，`why` 文本出现在 `signals.ai_reason`，且 `features.ai_dq/ai_rr/ai_vb` 可查询
- 总预算 2000 字符内的 `why/inv/risk` 完整落库，不被旧上限截断
- `strategy.valid_bars` 在参数页位于「策略-基础」的 K 线周期下方，中文说明完整
- 复核日志 `raw_text` 中中文不再丢失
- 全量测试通过，无新增 clippy 警告

## 10. 分期计划

| 期 | 范围 | 是否改变交易 |
|---|---|---|
| P1 | 协议字段、解析、中文 bug、单测 | 否 |
| P2 | 门槛拒单、signals 落库、参数登记 | 是（拒单） |
| P3 | 失效平仓、建议限价、开关 | 是（可关闭） |

每期独立提交、独立验证、独立回滚。