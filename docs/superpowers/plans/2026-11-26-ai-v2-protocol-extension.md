# AI v2 协议扩展实现计划

> **面向 AI 代理的工作者：** 必需子技能：使用 `subagent-driven-development`（推荐）或 `executing-plans` 逐任务实现此计划。步骤使用复选框（`- [ ]`）语法跟踪进度。

**目标：** 在不改协议版本号的前提下，让 v2 支持自由中文文本与 5 个新数值字段，并接入复核拒单、失效平仓和建议限价。

**架构：** 解析层扩展 `Decision` 与 `localize`；消费层通过 `ReviewDecision → ReviewedEntry → EntryTerms → LivePlan` 逐层传递；存储层用 `signals.features` JSON 与 `positions.invalidation_price` 新列。

**技术栈：** Rust、rusqlite、axum、serde_json、Vue 3、Vitest。

---

## 构建与测试命令

本项目 `cargo test --bin nanoquant` 会被 FSM 拦截，统一使用：

```bash
cargo build --tests --bin nanoquant
TEST_EXE=$(ls -t target/debug/deps/nanoquant-*.exe | head -1)
"$TEST_EXE" <filter>
```

前端测试：

```bash
npm --prefix frontend test
```

---

## 文件结构

| 文件 | 职责 |
|---|---|
| `rust/nanoquant/src/ai_client.rs` | `Decision` 字段、`localize`、文本预算、`ip/ep` 校验、`AiConfig` 读取 |
| `rust/nanoquant/src/ai_analysis.rs` | system prompt 模板、新字段说明 |
| `rust/nanoquant/src/ai_review.rs` | 复核入口、`dq/rr` 拒单、`max_tokens` |
| `rust/nanoquant/src/review.rs` | `ReviewDecision` / `ReviewedEntry` 新字段 |
| `rust/nanoquant/src/signal_audit.rs` | `AuditSignal` 新字段、features JSON 落库 |
| `rust/nanoquant/src/param_registry.rs` | 新参数登记 |
| `rust/nanoquant/src/default_settings.json` | 默认值 |
| `rust/nanoquant/src/schema.rs` / `schema_baseline.json` | `positions.invalidation_price` 迁移 |
| `rust/nanoquant/src/runtime.rs` | 持仓落库、失效平仓、限价开仓 |
| `frontend/src/views/settingsCatalog.ts` | 参数页分区与顺序 |
| `frontend/src/views/settingsCatalog.spec.ts` | 分区断言 |
| `frontend/src/views/StrategyView.vue` | 中文标签与提示 |

---

### 任务 1：v2 扩展字段解析

**文件：**
- 修改：`rust/nanoquant/src/ai_client.rs:1007-1040`（`Decision` 与 `normalized`）
- 修改：`rust/nanoquant/src/ai_client.rs:1087-1235`（`parse_decision` / `localize`）
- 修改：`rust/nanoquant/src/ai_review.rs:382-395`（测试构造点）

- [ ] **步骤 1：编写失败的测试**

在 `ai_client.rs` 的 `mod tests` 末尾加入：

```rust
#[test]
fn v2_extended_fields_are_parsed() {
    let ctx = json!({"price": 100.0});
    let text = r#"{"a":"L","c":0.7,"l":5,"m":20,"s":30,"t":60,
        "why":"趋势向上，量能放大","risk":"上方套牢盘密集","inv":"跌破 EMA21",
        "ip":95.0,"vb":6,"dq":0.8,"rr":1.5,"ep":100.2}"#;
    let d = parse_decision(text, Some(&ctx)).unwrap();
    assert_eq!(d.reason, "趋势向上，量能放大");
    assert_eq!(d.invalidation, "跌破 EMA21");
    assert_eq!(d.invalidation_price, Some(95.0));
    assert_eq!(d.valid_bars, Some(6));
    assert_eq!(d.data_quality, Some(0.8));
    assert_eq!(d.rr_target, Some(1.5));
    assert_eq!(d.entry_price, Some(100.2));
}

#[test]
fn free_text_fields_keep_chinese_and_respect_total_budget() {
    let ctx = json!({"price": 100.0});
    let long = "很".repeat(3000);
    let text = format!(
        r#"{{"a":"H","c":0.5,"why":"{}","risk":"{}","inv":"{}","f":"中文补充"}}"#,
        long, long, long
    );
    let d = parse_decision(&text, Some(&ctx)).unwrap();
    assert!(d.reason.contains('很'));
    assert!(!d.reason.contains("中文补充") || d.reason.chars().count() <= 2000);
    assert!(d.invalidation.chars().count() <= 500);
    assert!(d.risk_flags.iter().map(|s| s.chars().count()).sum::<usize>() <= 500);
    let total = d.reason.chars().count() + d.invalidation.chars().count();
    assert!(total <= 2000);
}

#[test]
fn invalidation_and_entry_price_outside_range_become_none() {
    let ctx = json!({"price": 100.0});
    let text = r#"{"a":"L","c":0.7,"ip":50.0,"ep":101.0}"#;
    let d = parse_decision(text, Some(&ctx)).unwrap();
    assert_eq!(d.invalidation_price, None);
    assert_eq!(d.entry_price, None);
}

#[test]
fn v1_payload_still_parses() {
    let ctx = json!({"price": 100.0});
    let text = r#"{"action":"hold","confidence":0.4,"reason":"观望"}"#;
    let d = parse_decision(text, Some(&ctx)).unwrap();
    assert_eq!(d.action, "hold");
    assert_eq!(d.data_quality, None);
}
```

- [ ] **步骤 2：运行测试验证失败**

```bash
cargo build --tests --bin nanoquant
TEST_EXE=$(ls -t target/debug/deps/nanoquant-*.exe | head -1)
"$TEST_EXE" v2_extended_fields_are_parsed
```

预期：编译失败，`Decision` 没有 `invalidation_price` 等字段。

- [ ] **步骤 3：实现字段与解析**

`Decision`（`ai_client.rs:1002`）增加：

```rust
pub invalidation_price: Option<f64>,
pub valid_bars: Option<i64>,
pub data_quality: Option<f64>,
pub rr_target: Option<f64>,
pub entry_price: Option<f64>,
```

`normalized()`（`ai_client.rs:1036-1038`）改为：

```rust
self.reason = self.reason.chars().take(2000).collect();
self.risk_flags.truncate(6);
self.invalidation = self.invalidation.chars().take(500).collect();
```

`localize()`（`ai_client.rs:1151`）在组装 `reason` 前加入预算分配：

```rust
fn take_budget(text: &str, max: usize, left: &mut usize) -> String {
    let out: String = text.chars().take(max.min(*left)).collect();
    *left -= out.chars().count();
    out
}

const TEXT_BUDGET: usize = 2000;
let mut left = TEXT_BUDGET;
let why = take_budget(data.get("why").and_then(Value::as_str).unwrap_or("").trim(), 800, &mut left);
let inv_text = take_budget(data.get("inv").and_then(Value::as_str).unwrap_or("").trim(), 500, &mut left);
let risk_text = take_budget(data.get("risk").and_then(Value::as_str).unwrap_or("").trim(), 500, &mut left);
let free_source: String = data.get("f").and_then(Value::as_str).unwrap_or("")
    .chars().filter(|ch| !ch.is_control()).collect();
let free = take_budget(&free_source, 200, &mut left);
```

`reason` 组装改为：

```rust
let mut reason = if why.is_empty() { reasons.join("；") } else { why.clone() };
```

`risk_flags` 改为：

```rust
let risks = if risk_text.is_empty() {
    codes_text(data.get("k"), &CODE_TEXT, 3)
} else {
    vec![risk_text]
};
```

`invalidation` 改为：

```rust
let invalidation = if inv_text.is_empty() {
    INVALIDATION_CODES.iter()
        .find(|(code, _)| *code == x_code)
        .map(|(_, text)| text.to_string())
        .unwrap_or_else(|| "数据或行情结构发生变化时本判断失效".to_string())
} else {
    inv_text
};
```

`out` 追加：

```rust
let opt = |key: &str| data.get(key).and_then(Value::as_f64).filter(|v| v.is_finite());
out.insert("invalidation_price".to_string(), json!(opt("ip")));
out.insert("valid_bars".to_string(), json!(data.get("vb").and_then(Value::as_i64)));
out.insert("data_quality".to_string(), json!(opt("dq")));
out.insert("rr_target".to_string(), json!(opt("rr")));
out.insert("entry_price".to_string(), json!(opt("ep")));
```

`parse_decision()`（`ai_client.rs:1132`）的 `Ok(Decision {` 增加：

```rust
let mark = ctx.and_then(|c| c.get("price")).and_then(Value::as_f64).unwrap_or(0.0);
let within = |v: f64, pct: f64| mark > 0.0 && v > 0.0 && (v - mark).abs() / mark <= pct;
let opt_float = |key: &str| localized.get(key).and_then(Value::as_f64).filter(|v| v.is_finite());
```

字段赋值：

```rust
invalidation_price: opt_float("invalidation_price").filter(|v| within(*v, 0.10)),
valid_bars: localized.get("valid_bars").and_then(Value::as_i64).map(|v| v.clamp(1, 24)),
data_quality: opt_float("data_quality").map(|v| v.clamp(0.0, 1.0)),
rr_target: opt_float("rr_target").map(|v| v.clamp(0.0, 10.0)),
entry_price: opt_float("entry_price").filter(|v| within(*v, 0.005)),
```

`Decision { ... }` 的全部构造点补 5 个 `None`：
`ai_analysis.rs:670`、`ai_review.rs:383/715/738/763/779/787/798`。

- [ ] **步骤 4：运行测试验证通过**

```bash
cargo build --tests --bin nanoquant
TEST_EXE=$(ls -t target/debug/deps/nanoquant-*.exe | head -1)
"$TEST_EXE" v2_extended_fields_are_parsed
"$TEST_EXE" free_text_fields_keep_chinese_and_respect_total_budget
"$TEST_EXE" invalidation_and_entry_price_outside_range_become_none
"$TEST_EXE" v1_payload_still_parses
```

预期：全部 PASS。

- [ ] **步骤 5：Commit**

```bash
git add rust/nanoquant/src/ai_client.rs rust/nanoquant/src/ai_review.rs
git commit -m "feat(ai): v2 协议支持中文文本与扩展数值字段"
```

---

### 任务 2：prompt 模板与 max_tokens

**文件：**
- 修改：`rust/nanoquant/src/ai_analysis.rs:306-337`
- 修改：`rust/nanoquant/src/ai_review.rs:200-203`

- [ ] **步骤 1：编写失败的测试**

在 `ai_analysis.rs` 的 `mod tests` 加入：

```rust
#[test]
fn decision_prompt_lists_extended_fields() {
    let context = json!({"symbol":"BTCUSDT","price":100.0});
    let messages = decision_messages_with_personas(&context, &[]);
    let system = &messages[0].content;
    for key in ["why", "risk", "inv", "ip", "vb", "dq", "rr", "ep"] {
        assert!(system.contains(key), "prompt 缺少字段 {key}");
    }
}
```

- [ ] **步骤 2：运行测试验证失败**

```bash
cargo build --tests --bin nanoquant
TEST_EXE=$(ls -t target/debug/deps/nanoquant-*.exe | head -1)
"$TEST_EXE" decision_prompt_lists_extended_fields
```

预期：FAIL，缺少 `why` 等字段。

- [ ] **步骤 3：更新 prompt 与 max_tokens**

`ai_analysis.rs` 的 JSON 模板替换为：

```rust
"{\"a\":\"L|S|H|C\",\"c\":0..1,\"l\":1..100,\"m\":0..100,\"s\":5..50,\"t\":5..100,\
\"why\":\"中文理由<=800字\",\"risk\":\"中文风险<=500字\",\"inv\":\"中文失效条件<=500字\",\
\"ip\":0,\"vb\":0,\"dq\":0..1,\"rr\":0..10,\"ep\":0,\
\"r\":[int],\"k\":[int],\"x\":int,\"f\":\"中文补充<=200字\"} \
why=理由; risk=风险; inv=失效条件; ip=失效价(0=未给); vb=有效期(K线根数); \
dq=数据质量0..1; rr=预期盈亏比; ep=建议入场价(0=未给)。四个中文文本合计不超过 2000 字。"
```

`ai_review.rs` 的 `ChatCall::new` 之后加入：

```rust
call.max_tokens = Some(call.max_tokens.unwrap_or(0).max(16000));
```

- [ ] **步骤 4：运行测试验证通过**

```bash
cargo build --tests --bin nanoquant
TEST_EXE=$(ls -t target/debug/deps/nanoquant-*.exe | head -1)
"$TEST_EXE" decision_prompt_lists_extended_fields
```

预期：PASS。

- [ ] **步骤 5：Commit**

```bash
git add rust/nanoquant/src/ai_analysis.rs rust/nanoquant/src/ai_review.rs
git commit -m "feat(ai): v2 提示词说明扩展字段并放宽输出预算"
```

---

### 任务 3：参数注册与默认值

**文件：**
- 修改：`rust/nanoquant/src/param_registry.rs:32-112`（`RANGES`）
- 修改：`rust/nanoquant/src/param_registry.rs:140-260`（`VISIBLE`）
- 修改：`rust/nanoquant/src/default_settings.json`

- [ ] **步骤 1：编写失败的测试**

在 `param_registry.rs` 的 `mod tests` 加入：

```rust
#[test]
fn v2_threshold_params_are_registered() {
    let d = defaults();
    assert_eq!(d["strategy"]["min_entry_rr"], json!(0.8));
    assert_eq!(d["strategy"]["valid_bars"], json!(6));
    assert_eq!(d["ai"]["use_invalidation_price"], json!(false));
    assert_eq!(d["ai"]["use_entry_price"], json!(false));
    assert!(is_visible("strategy", "valid_bars"));
    assert!(is_visible("strategy", "min_entry_rr"));
}
```

- [ ] **步骤 2：运行测试验证失败**

```bash
cargo build --tests --bin nanoquant
TEST_EXE=$(ls -t target/debug/deps/nanoquant-*.exe | head -1)
"$TEST_EXE" v2_threshold_params_are_registered
```

预期：FAIL，`find` 返回 `None`。

- [ ] **步骤 3：登记参数**

`RANGES` 的 strategy 段加入：

```rust
("strategy", "min_entry_rr", 0.0, 10.0, 0.1),
("strategy", "valid_bars", 1.0, 24.0, 1.0),
```

`VISIBLE` 的 strategy 基础段加入 `"valid_bars"`（紧跟 `"timeframe"`），入场过滤段加入 `"min_entry_rr"`；ai 段加入 `"use_invalidation_price"`、`"use_entry_price"`。

`default_settings.json`：

```json
"strategy": { "timeframe": "1h", "valid_bars": 6, "min_entry_rr": 0.8 }
```

```json
"ai": { "use_invalidation_price": false, "use_entry_price": false }
```

保留两份配置中已有字段，仅追加。

- [ ] **步骤 4：运行测试验证通过**

```bash
cargo build --tests --bin nanoquant
TEST_EXE=$(ls -t target/debug/deps/nanoquant-*.exe | head -1)
"$TEST_EXE" v2_threshold_params_are_registered
```

预期：PASS。

- [ ] **步骤 5：Commit**

```bash
git add rust/nanoquant/src/param_registry.rs rust/nanoquant/src/default_settings.json
git commit -m "feat(params): 注册 v2 门槛与有效期参数"
```

---

### 任务 4：前端参数页

**文件：**
- 修改：`frontend/src/views/settingsCatalog.ts:33`
- 修改：`frontend/src/views/StrategyView.vue:159-175`
- 测试：`frontend/src/views/settingsCatalog.spec.ts`

- [ ] **步骤 1：编写失败的测试**

在 `settingsCatalog.spec.ts` 加入：

```ts
it('valid_bars 排在 K 线周期之后', () => {
  const basic = SETTINGS_SECTIONS.strategy.find((s) => s.title === '基础')
  expect(basic?.fields.slice(0, 2)).toEqual(['timeframe', 'valid_bars'])
})

it('min_entry_rr 出现在策略入场过滤', () => {
  const filter = SETTINGS_SECTIONS.strategy.find((s) => s.title === '入场过滤')
  expect(filter?.fields).toContain('min_entry_rr')
})
```

- [ ] **步骤 2：运行测试验证失败**

```bash
npm --prefix frontend test -- --run settingsCatalog.spec.ts
```

预期：FAIL。

- [ ] **步骤 3：实现前端登记**

`settingsCatalog.ts:33`：

```ts
{ title: '基础', fields: ['timeframe', 'valid_bars', 'ema_fast', 'ema_slow', 'rsi_period', 'atr_period'] },
```

入场过滤 `fields` 追加 `'min_entry_rr'`。

`StrategyView.vue` 的标签映射加入：

```ts
valid_bars: { label: '信号有效期', unit: '根 K 线', hint: 'AI 未给出有效期时使用；1 根 = 1 个当前 K 线周期，超过后该信号不再作为开仓依据' },
min_entry_rr: { label: '最低开仓盈亏比', unit: 'R', hint: 'AI 预期盈亏比低于该值时不采纳' },
use_invalidation_price: { label: '启用 AI 失效价平仓', hint: '开启后持仓触及 AI 失效价即平仓' },
use_entry_price: { label: '启用 AI 建议限价', hint: '仅只做 Maker 模式生效；偏离现价超过 0.5% 时忽略' },
```

- [ ] **步骤 4：运行测试验证通过**

```bash
npm --prefix frontend test -- --run settingsCatalog.spec.ts
```

预期：PASS。

- [ ] **步骤 5：Commit**

```bash
git add frontend/src/views/settingsCatalog.ts frontend/src/views/settingsCatalog.spec.ts frontend/src/views/StrategyView.vue
git commit -m "feat(ui): 参数页展示 v2 有效期与盈亏比门槛"
```

---

### 任务 5：复核前置拒单

**文件：**
- 修改：`rust/nanoquant/src/ai_review.rs:234-300`

- [ ] **步骤 1：编写失败的测试**

在 `ai_review.rs` 的 `mod tests` 加入：

```rust
#[test]
fn low_data_quality_is_rejected() {
    let mut decision = decision("open_long", 0.9);
    decision.data_quality = Some(0.2);
    let result = decision_to_review(&proposal("long"), "long", &decision, 0.45, true, true, 0.8)
        .unwrap();
    assert_eq!(result.action, ReviewAction::Reject);
    assert!(result.reason.contains("数据质量"));
}

#[test]
fn low_rr_target_is_rejected() {
    let mut decision = decision("open_long", 0.9);
    decision.rr_target = Some(0.4);
    let result = decision_to_review(&proposal("long"), "long", &decision, 0.45, true, true, 0.8)
        .unwrap();
    assert_eq!(result.action, ReviewAction::Reject);
    assert!(result.reason.contains("盈亏比"));
}
```

- [ ] **步骤 2：运行测试验证失败**

```bash
cargo build --tests --bin nanoquant
TEST_EXE=$(ls -t target/debug/deps/nanoquant-*.exe | head -1)
"$TEST_EXE" low_data_quality_is_rejected
```

预期：编译失败，`decision_to_review` 参数数量不匹配。

- [ ] **步骤 3：实现门槛**

`decision_to_review` 增加参数 `min_rr_target: f64`，函数开头加入：

```rust
if let Some(dq) = decision.data_quality {
    if dq < min_confidence {
        return Ok(reject(format!("AI 数据质量分 {dq:.2} 低于 {min_confidence:.2}")));
    }
}
if let Some(rr) = decision.rr_target {
    if rr < min_rr_target {
        return Ok(reject(format!("AI 盈亏比 {rr:.2} 低于 {min_rr_target:.2}")));
    }
}
```

`review_entry` 调用点读取 strategy 配置：

```rust
let min_rr = strategy_cfg.get("min_entry_rr").and_then(Value::as_f64).unwrap_or(0.8);
```

并传入 `decision_to_review(..., min_rr)`。同步修改 `ai_review.rs` 内所有既有 `decision_to_review(` 调用（含测试），最后一个参数补 `0.8`。

- [ ] **步骤 4：运行测试验证通过**

```bash
cargo build --tests --bin nanoquant
TEST_EXE=$(ls -t target/debug/deps/nanoquant-*.exe | head -1)
"$TEST_EXE" low_data_quality_is_rejected
"$TEST_EXE" low_rr_target_is_rejected
```

预期：PASS。

- [ ] **步骤 5：Commit**

```bash
git add rust/nanoquant/src/ai_review.rs
git commit -m "feat(ai): 数据质量与盈亏比低于门槛时拒单"
```

---

### 任务 6：审计落库

**文件：**
- 修改：`rust/nanoquant/src/signal_audit.rs:16-31`、`130-175`
- 修改：`rust/nanoquant/src/runtime.rs`（`audit_row` 调用点）

- [ ] **步骤 1：编写失败的测试**

在 `signal_audit.rs` 的 `mod tests` 加入：

```rust
#[test]
fn audit_signal_persists_ai_meta_in_features() {
    let mut row = AuditSignal::new("BTCUSDT").with_ai("open_long", 0.8, "趋势向上");
    row.data_quality = Some(0.77);
    row.rr_target = Some(1.6);
    row.valid_bars = Some(6);
    let features = row.features_with_meta();
    assert_eq!(features["ai_dq"], 0.77);
    assert_eq!(features["ai_rr"], 1.6);
    assert_eq!(features["ai_vb"], 6);
}
```

- [ ] **步骤 2：运行测试验证失败**

```bash
cargo build --tests --bin nanoquant
TEST_EXE=$(ls -t target/debug/deps/nanoquant-*.exe | head -1)
"$TEST_EXE" audit_signal_persists_ai_meta_in_features
```

预期：FAIL，没有 `data_quality` 字段。

- [ ] **步骤 3：实现字段与 JSON**

`AuditSignal` 增加：

```rust
pub data_quality: Option<f64>,
pub rr_target: Option<f64>,
pub valid_bars: Option<i64>,
pub invalidation_price: Option<f64>,
pub entry_price: Option<f64>,
```

`impl AuditSignal` 增加：

```rust
pub fn features_with_meta(&self) -> Value {
    let mut features = self.features.clone();
    if let Some(object) = features.as_object_mut() {
        if let Some(v) = self.data_quality { object.insert("ai_dq".into(), json!(v)); }
        if let Some(v) = self.rr_target { object.insert("ai_rr".into(), json!(v)); }
        if let Some(v) = self.valid_bars { object.insert("ai_vb".into(), json!(v)); }
        if let Some(v) = self.invalidation_price { object.insert("ai_ip".into(), json!(v)); }
        if let Some(v) = self.entry_price { object.insert("ai_ep".into(), json!(v)); }
    }
    features
}
```

写入时用 `row.features_with_meta()` 替换 `row.features.clone()`。

- [ ] **步骤 4：运行测试验证通过**

```bash
cargo build --tests --bin nanoquant
TEST_EXE=$(ls -t target/debug/deps/nanoquant-*.exe | head -1)
"$TEST_EXE" audit_signal_persists_ai_meta_in_features
```

预期：PASS。

- [ ] **步骤 5：Commit**

```bash
git add rust/nanoquant/src/signal_audit.rs rust/nanoquant/src/runtime.rs
git commit -m "feat(audit): 信号审计记录 AI 扩展字段"
```

---

### 任务 7：持仓表迁移与结构传递

**文件：**
- 修改：`rust/nanoquant/src/schema_baseline.json`（positions columns）
- 修改：`rust/nanoquant/src/review.rs:7-70`
- 修改：`rust/nanoquant/src/runtime.rs:105-128`
- 测试：`rust/nanoquant/src/schema.rs` 的迁移测试

- [ ] **步骤 1：编写失败的测试**

在 `schema.rs` 的 `mod tests` 加入：

```rust
#[test]
fn migrate_adds_invalidation_price_to_positions() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE positions(id INTEGER PRIMARY KEY, symbol TEXT);").unwrap();
    migrate(&conn).unwrap();
    let columns: Vec<String> = conn
        .prepare("PRAGMA table_info('positions')")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(columns.contains(&"invalidation_price".to_string()));
}
```

- [ ] **步骤 2：运行测试验证失败**

```bash
cargo build --tests --bin nanoquant
TEST_EXE=$(ls -t target/debug/deps/nanoquant-*.exe | head -1)
"$TEST_EXE" migrate_adds_invalidation_price_to_positions
```

预期：FAIL，列不存在。

- [ ] **步骤 3：实现迁移与结构**

`schema_baseline.json` 的 positions `columns` 数组追加：

```json
{ "cid": 30, "name": "invalidation_price", "type": "FLOAT", "notnull": false, "default": null, "pk": false }
```

`ReviewDecision` 与 `ReviewedEntry` 增加：

```rust
pub invalidation_price: Option<f64>,
pub valid_bars: Option<i64>,
pub data_quality: Option<f64>,
pub rr_target: Option<f64>,
pub entry_price: Option<f64>,
```

`EntryTerms` 增加：

```rust
invalidation_price: Option<f64>,
```

`decision_to_review` 返回时填充，`apply_review` 传递。`ReviewDecision { ... }` 构造点补 5 个 `None`：`ai_review.rs:222/255/345`、`review.rs:263/302`。`EntryTerms { ... }` 只有一处构造点 `runtime.rs:3040`，补 `invalidation_price: reviewed.invalidation_price`；`ReviewedEntry { ... }` 的构造点（`review.rs` 内搜索）同步补 5 个字段。

- [ ] **步骤 4：运行测试验证通过**

```bash
cargo build --tests --bin nanoquant
TEST_EXE=$(ls -t target/debug/deps/nanoquant-*.exe | head -1)
"$TEST_EXE" migrate_adds_invalidation_price_to_positions
```

预期：PASS。

- [ ] **步骤 5：Commit**

```bash
git add rust/nanoquant/src/schema_baseline.json rust/nanoquant/src/review.rs rust/nanoquant/src/runtime.rs rust/nanoquant/src/ai_review.rs
git commit -m "feat(positions): 增加失效价字段并贯通复核结构"
```

---

### 任务 8：开仓写入失效价

**文件：**
- 修改：`rust/nanoquant/src/runtime.rs:3178-3200`

- [ ] **步骤 1：编写失败的测试**

在 `runtime.rs` 的 `mod tests` 加入：

```rust
#[test]
fn entry_position_persists_invalidation_price() {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE positions(id INTEGER PRIMARY KEY, symbol TEXT, side TEXT, qty REAL,
         entry_price REAL, mark_price REAL, leverage REAL, margin REAL, sl_price REAL,
         tp_price REAL, peak_price REAL, initial_sl_price REAL, signal_score REAL,
         opened_at INTEGER, closed_at INTEGER, exit_price REAL, exit_reason TEXT,
         realized_pnl REAL, fees REAL, status TEXT, mode TEXT, venue_unrealized REAL,
         venue_ts INTEGER, partial_taken_at INTEGER, ai_confidence REAL, ai_reason TEXT,
         entry_slippage_bps REAL, trade_id INTEGER, invalidation_price REAL, features TEXT);",
    )
    .unwrap();
    let tx = conn.transaction().unwrap();
    let terms = EntryTerms { leverage: 5.0, stop: 95.0, take_profit: 110.0, score: 0.8, invalidation_price: Some(97.0) };
    insert_entry_position(&tx, "live", "BTCUSDT", "long", 2.0, 100.0, &terms, 0.0005, 1, None).unwrap();
    tx.commit().unwrap();
    let stored: Option<f64> = conn
        .query_row("SELECT invalidation_price FROM positions WHERE symbol='BTCUSDT'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(stored, Some(97.0));
}
```

- [ ] **步骤 2：运行测试验证失败**

```bash
cargo build --tests --bin nanoquant
TEST_EXE=$(ls -t target/debug/deps/nanoquant-*.exe | head -1)
"$TEST_EXE" entry_position_persists_invalidation_price
```

预期：FAIL，SQL 未写该列。

- [ ] **步骤 3：实现落库**

`insert_entry_position` 的 INSERT 列清单追加 `invalidation_price`，VALUES 追加 `?13`，参数追加 `entry.invalidation_price`。

同时给 `runtime.rs` 测试 fixture 的 positions 建表语句补 `invalidation_price REAL`。

- [ ] **步骤 4：运行测试验证通过**

```bash
cargo build --tests --bin nanoquant
TEST_EXE=$(ls -t target/debug/deps/nanoquant-*.exe | head -1)
"$TEST_EXE" entry_position_persists_invalidation_price
```

预期：PASS。

- [ ] **步骤 5：Commit**

```bash
git add rust/nanoquant/src/runtime.rs
git commit -m "feat(positions): 开仓落库 AI 失效价"
```

---

### 任务 9：快速风控失效平仓

**文件：**
- 修改：`rust/nanoquant/src/runtime.rs:5416-5479`（`fast_risk_cycle`）

- [ ] **步骤 1：编写失败的测试**

在 `runtime.rs` 的 `mod tests` 加入：

```rust
#[test]
fn invalidation_price_triggers_fast_close() {
    let mark = 96.0;
    let entry = 100.0;
    let invalidation = Some(97.0);
    let reason = fast_risk_reason("long", entry, mark, 95.0, 110.0, 5.0, invalidation, 10.0, 15.0);
    assert_eq!(reason, Some("invalidation"));
}
```

- [ ] **步骤 2：运行测试验证失败**

```bash
cargo build --tests --bin nanoquant
TEST_EXE=$(ls -t target/debug/deps/nanoquant-*.exe | head -1)
"$TEST_EXE" invalidation_price_triggers_fast_close
```

预期：FAIL，函数不存在。

- [ ] **步骤 3：实现判定函数与 SQL**

```rust
fn fast_risk_reason(
    side: &str,
    entry: f64,
    mark: f64,
    sl: f64,
    tp: f64,
    leverage: f64,
    invalidation_price: Option<f64>,
    global_stop: f64,
    global_tp: f64,
) -> Option<&'static str> {
    let direction = if side == "long" { 1.0 } else { -1.0 };
    let margin_pct = (mark - entry) * direction / entry * 100.0 * leverage.max(1.0);
    if global_stop > 0.0 && margin_pct <= -global_stop {
        return Some("global_stop_loss");
    }
    if global_tp > 0.0 && margin_pct >= global_tp {
        return Some("global_take_profit");
    }
    if let Some(ip) = invalidation_price.filter(|v| *v > 0.0 && v.is_finite()) {
        if (direction > 0.0 && mark <= ip) || (direction < 0.0 && mark >= ip) {
            return Some("invalidation");
        }
    }
    if sl > 0.0 && ((direction > 0.0 && mark <= sl) || (direction < 0.0 && mark >= sl)) {
        return Some("stop_loss");
    }
    if tp > 0.0 && ((direction > 0.0 && mark >= tp) || (direction < 0.0 && mark <= tp)) {
        return Some("take_profit");
    }
    None
}
```

`fast_risk_cycle` 的 SQL 改为：

```sql
SELECT id,symbol,side,entry_price,sl_price,tp_price,leverage,qty,invalidation_price
FROM positions WHERE mode='live' AND status='open' AND qty > 0
```

行元组追加 `Option<f64>`，循环内改为调用 `fast_risk_reason`。开关不在此处读取，统一在复核出口处理。

`AiConfig`（`ai_client.rs:745`）增加：

```rust
pub use_invalidation_price: bool,
pub use_entry_price: bool,
```

`Default` 补 `false`，`read_ai_config_conn` 补：

```rust
if let Some(item) = value.get("use_invalidation_price").and_then(Value::as_bool) {
    cfg.use_invalidation_price = item;
}
if let Some(item) = value.get("use_entry_price").and_then(Value::as_bool) {
    cfg.use_entry_price = item;
}
```

`review_entry` 在 `decision_to_review(...)` 之后统一应用开关：

```rust
let mut reviewed = decision_to_review(...)?;
if !cfg.use_invalidation_price {
    reviewed.invalidation_price = None;
}
if !cfg.use_entry_price {
    reviewed.entry_price = None;
}
Ok(reviewed)
```

- [ ] **步骤 4：运行测试验证通过**

```bash
cargo build --tests --bin nanoquant
TEST_EXE=$(ls -t target/debug/deps/nanoquant-*.exe | head -1)
"$TEST_EXE" invalidation_price_triggers_fast_close
```

预期：PASS。

- [ ] **步骤 5：Commit**

```bash
git add rust/nanoquant/src/runtime.rs rust/nanoquant/src/ai_client.rs
git commit -m "feat(risk): 持仓触及 AI 失效价时快速平仓"
```

---

### 任务 10：建议入场价限价

**文件：**
- 修改：`rust/nanoquant/src/runtime.rs:3060-3076`

- [ ] **步骤 1：编写失败的测试**

在 `runtime.rs` 的 `mod tests` 加入：

```rust
#[test]
fn preferred_entry_price_used_only_when_valid() {
    assert_eq!(preferred_limit_price("BUY", 100.0, 99.8), Some(99.8));
    assert_eq!(preferred_limit_price("BUY", 100.0, 100.5), None);
    assert_eq!(preferred_limit_price("SELL", 100.0, 100.2), Some(100.2));
    assert_eq!(preferred_limit_price("SELL", 100.0, 99.8), None);
    assert_eq!(preferred_limit_price("BUY", 100.0, 99.0), None);
}
```

- [ ] **步骤 2：运行测试验证失败**

```bash
cargo build --tests --bin nanoquant
TEST_EXE=$(ls -t target/debug/deps/nanoquant-*.exe | head -1)
"$TEST_EXE" preferred_entry_price_used_only_when_valid
```

预期：FAIL，函数不存在。

- [ ] **步骤 3：实现**

```rust
fn preferred_limit_price(side: &str, mark: f64, preferred: f64) -> Option<f64> {
    if !preferred.is_finite() || preferred <= 0.0 || mark <= 0.0 {
        return None;
    }
    if (preferred - mark).abs() / mark > 0.005 {
        return None;
    }
    if (side == "BUY" && preferred > mark) || (side == "SELL" && preferred < mark) {
        return None;
    }
    Some(preferred)
}
```

`LimitPostOnly` 分支改为：

```rust
let fallback = Self::limit_entry_price(side, bid, ask, draft.mark_price);
let price = reviewed
    .entry_price
    .and_then(|ep| preferred_limit_price(side, draft.mark_price, ep))
    .unwrap_or(fallback);
Some(price)
```

开关已在 `review_entry` 出口处理，`runtime` 无需读取配置。

- [ ] **步骤 4：运行测试验证通过**

```bash
cargo build --tests --bin nanoquant
TEST_EXE=$(ls -t target/debug/deps/nanoquant-*.exe | head -1)
"$TEST_EXE" preferred_entry_price_used_only_when_valid
```

预期：PASS。

- [ ] **步骤 5：Commit**

```bash
git add rust/nanoquant/src/runtime.rs
git commit -m "feat(entry): 只做 Maker 时采用 AI 建议限价"
```

---

## 自检

- 规格章节 3/4/5/6/7/8/9 均对应到任务 1-10。
- 无占位符；每个代码步骤给出可编译片段。
- 类型一致性：`Decision`、`ReviewDecision`、`ReviewedEntry`、`EntryTerms` 的新字段在任务 1/7/8 中同名。
- 未覆盖项：P3 两个开关的默认值已在任务 3 登记，任务 9/10 读取；`valid_bars` 仅落库展示，与规格 6.2 一致。