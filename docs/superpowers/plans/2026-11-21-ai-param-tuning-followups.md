# AI 调参闭环后续修复实现计划

> **面向 AI 代理的工作者：** 必需子技能：使用 subagent-driven-development（推荐）或 executing-plans 逐任务实现此计划。步骤使用复选框（`- [ ]`）语法来跟踪进度。

**目标：** 消除 AI 参数读写闭环在可写边界、写回事务、审计归因、效果计算、调参冷却与前端体验上的已知缺口。

**架构：** 沿用 `param_registry` / `settings_api` / `ai_review` / `ai_client` / `StrategyView` 现有结构：先在注册表收敛 AI 可写边界，再把写回改为单事务并精确关联调用日志，随后批量计算调参效果、增加冷却窗口，最后统一敏感词与范围校验的口径，并修复前端调参记录的显示与刷新。

**技术栈：** Rust（axum、rusqlite、serde_json）、Vue 3 + Pinia + Vitest。

**规格：** `docs/superpowers/specs/2026-11-21-ai-param-tuning-design.md`

**上一份计划：** `docs/superpowers/plans/2026-11-21-ai-param-tuning.md`

## 全局约束

- `ai.allow_param_tuning` 默认保持 `true`，关闭时必须 fail-closed。
- 敏感字段（`security` 组，或键名含 `secret/password/token/api_key/_enc/webhook/credential/authorization/chat_id/private_key`）永不进入目录、不可写、审计中必须遮蔽。
- 非法项跳过、合法项继续应用；数据库写失败必须整批回滚，不得留下半更新。
- AI 可写范围：`risk` 与 `strategy` 组全部非敏感字段可写；其余组只允许隐藏字段（`!is_visible`）。
- 调参冷却默认 `0`（关闭），不改变现有行为；配置为正值时窗口内不得写库。
- 提交信息使用中文，遵循现有 `feat/fix/refactor(scope): ...` 风格。
- 验证使用本地工具链：Rust `C:\Users\Administrator\.cargo\bin`，Node `F:\cb\.tools\node\node-v20.19.0-win-x64`。

## 审查重点（Review Focus）

- `parameter_changes` 或 `engine_events` 插入失败：参数不得已经写入。测试放进任务 2。
- 多标的并发复核：参数变更的 `decision_id` 必须是本次 AI 调用，不得取最大日志 id。测试放进任务 3。
- 页面可见且不属于 `risk/strategy` 的参数（如 `engine.cycle_interval_sec`）：AI 不得写入。测试放进任务 1。
- 冷却窗口内再次调参：不得写库，必须留痕。测试放进任务 6。
- 回滚当前分组：不得在用户未确认时静默覆盖未保存的编辑。测试放进任务 4。
- 敏感字段在上下文、写回、审计三处必须同一口径，不得出现“上下文隐藏但可写”或“可写但审计明文”。测试放进任务 7。

---

## 文件结构

- 修改 `rust/nanoquant/src/param_registry.rs`：收敛 `is_ai_writable`；统一敏感词与范围校验的数据源。
- 修改 `rust/nanoquant/src/settings_api.rs`：`apply_param_updates` 事务化；批量 effect 计算；`validate`/`mask`/`diff` 使用注册表口径。
- 修改 `rust/nanoquant/src/ai_client.rs`：`ChatOutcome.log_id` 与 `log_call` 返回值。
- 修改 `rust/nanoquant/src/ai_review.rs`：精确 `decision_id`；调参冷却；读路径移除 DDL；`strip_secrets` 按组调用注册表。
- 修改 `rust/nanoquant/src/ai_analysis.rs`：决策提示中的可写范围说明。
- 修改 `rust/nanoquant/src/default_settings.json`：新增 `ai.param_tuning_min_interval_sec=0`。
- 修改 `frontend/src/views/paramChanges.ts`：回滚提示文案与调参记录显示条件。
- 修改 `frontend/src/views/paramChanges.spec.ts`：纯函数测试。
- 修改 `frontend/src/views/StrategyView.vue`：回滚确认、调参记录固定位置与 60 秒刷新。
- 测试：Rust 内联测试；前端 `paramChanges.spec.ts`、`settingsCatalog.spec.ts`。

---

## 任务 1：收敛 AI 可写边界

**文件：**
- 修改：`rust/nanoquant/src/param_registry.rs`
- 修改：`rust/nanoquant/src/ai_analysis.rs`
- 测试：`rust/nanoquant/src/param_registry.rs`、`rust/nanoquant/src/ai_analysis.rs` 内联测试

- [x] **步骤 1：编写失败的测试**

```rust
#[test]
fn ai_writable_covers_hidden_and_core_groups_only() {
    assert!(find("strategy", "ema_fast").unwrap().ai_writable, "隐藏参数可写");
    assert!(find("strategy", "entry_threshold").unwrap().ai_writable, "入场核心可写");
    assert!(find("risk", "max_leverage").unwrap().ai_writable, "风控核心可写");
    assert!(!find("engine", "cycle_interval_sec").unwrap().ai_writable, "可见运行参数不可写");
    assert!(!find("system", "log_level").unwrap().ai_writable, "可见日志参数不可写");
    assert!(!find("ai", "allow_param_tuning").unwrap().ai_writable, "ai 组始终不可写");
}
```

- [x] **步骤 2：运行测试验证失败**

运行：`cargo test --bin nanoquant param_registry::tests::ai_writable_covers_hidden_and_core_groups_only`
预期：FAIL，`engine.cycle_interval_sec` 或 `system.log_level` 当前为 `ai_writable=true`。

- [x] **步骤 3：修改 `is_ai_writable`**

规则顺序：`ai` 组与运行开关黑名单先返回 `false`；`group == "risk" || group == "strategy"` 返回 `!is_sensitive`；其余组返回 `!is_sensitive && !is_visible(group, key)`。

- [x] **步骤 4：更新决策提示**

在 `ai_analysis.rs` 的 `p` 段落把可写范围改为：

```text
Visible fields are writable only in risk and strategy; all other groups allow hidden fields only.
Use parameters.writable_parameters as the authoritative list.
```

- [x] **步骤 5：运行测试验证通过**

运行：`cargo test --bin nanoquant param_registry::tests`
预期：PASS，6 个以上测试全绿。

- [x] **步骤 6：Commit**

`git commit -m "fix(ai): 收敛 AI 可写参数边界"`

---

## 任务 2：写回事务化

**文件：**
- 修改：`rust/nanoquant/src/settings_api.rs`
- 修改：`rust/nanoquant/src/ai_review.rs`
- 测试：`rust/nanoquant/src/settings_api.rs` 内联测试

- [ ] **步骤 1：编写失败的测试**

```rust
#[test]
fn apply_param_updates_is_atomic_when_audit_fails() {
    let conn = test_conn();
    conn.execute_batch(
        "CREATE TRIGGER fail_param_changes BEFORE INSERT ON parameter_changes \
         BEGIN SELECT RAISE(ABORT, 'audit down'); END;",
    )
    .unwrap();
    let result = apply_param_updates(
        &conn,
        &[crate::ai_client::ParamUpdate {
            group: "strategy".into(),
            key: "entry_threshold".into(),
            value: json!(0.35),
            reason: "测试".into(),
        }],
        1,
        "demo",
    );
    assert!(result.is_err(), "审计失败必须整体失败");
    let stored: String = conn
        .query_row("SELECT value FROM app_settings WHERE key='strategy'", [], |row| row.get(0))
        .unwrap();
    assert_eq!(serde_json::from_str::<Value>(&stored).unwrap()["entry_threshold"], 0.3);
}
```

- [ ] **步骤 2：运行测试验证失败**

运行：`cargo test --bin nanoquant settings_api::tests::apply_param_updates_is_atomic_when_audit_fails`
预期：FAIL，当前返回 `Value` 而非 `Result`，且参数已写入。

- [ ] **步骤 3：把 `apply_param_updates` 改为事务**

签名改为 `pub(crate) fn apply_param_updates(...) -> Result<Value, String>`；内部执行 `let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;`，`app_settings`、`parameter_changes`、`engine_events` 三处写入全部改用 `tx.execute(...)?`，审计与事件错误不得再用 `let _ =` 吞掉；最后 `tx.commit().map_err(|e| e.to_string())?;`，返回 `Ok(json!({ "applied": applied, "skipped": skipped }))`。非法项在写前 `continue`，不触发回滚。

- [ ] **步骤 4：更新调用方**

`ai_review::apply_decision_params` 改为 `match`：`Ok(report)` 时处理 `skipped`；`Err(error)` 时把 `参数写回失败: {error}` push 到 `risk_flags`，并尝试写 `engine_events`。

- [ ] **步骤 5：更新既有测试**

`apply_param_updates_writes_valid_and_skips_invalid` 等测试加 `.unwrap()`；断言 `applied`/`skipped` 数量不变。

- [ ] **步骤 6：运行测试验证通过**

运行：`cargo test --bin nanoquant settings_api::tests`
预期：PASS，且 `apply_param_updates_is_atomic_when_audit_fails` 为绿。

- [ ] **步骤 7：Commit**

`git commit -m "fix(ai): 参数写回与审计改为单事务"`

---

## 任务 3：精确关联 AI 调用日志

**文件：**
- 修改：`rust/nanoquant/src/ai_client.rs`
- 修改：`rust/nanoquant/src/ai_review.rs`
- 测试：`rust/nanoquant/src/ai_client.rs` 内联测试

- [ ] **步骤 1：编写失败的测试**

```rust
#[tokio::test]
async fn chat_outcome_carries_its_own_log_id() {
    let path = std::env::temp_dir().join("nq-ai-log-id.db");
    let db = fixture_db(&path);
    let body = "{\"choices\":[{\"message\":{\"content\":\"{\\\"a\\\":\\\"H\\\"}\"}}]}";
    let transport = FakeTransport::new(
        vec![HttpReply { status: 200, body: body.to_string() }],
        Vec::new(),
    );
    let messages = vec![ChatMessage::new("user", "decide")];
    let outcome = chat(&transport, &db, &provider(), ChatCall::new(&messages, "decision")).await;
    assert!(outcome.ok, "{}", outcome.error);
    assert!(outcome.log_id > 0);
    let conn = db.conn.lock().unwrap();
    let found: i64 = conn
        .query_row("SELECT COUNT(*) FROM ai_call_logs WHERE id=?1", [outcome.log_id], |row| row.get(0))
        .unwrap();
    assert_eq!(found, 1);
}
```

测试夹具 `fixture_db`、`FakeTransport`、`provider()` 均来自 `ai_client.rs` 现有 `mod tests`，构造方式参照 `chat_drops_reasoning_effort_after_400_and_retries_once`。

- [ ] **步骤 2：运行测试验证失败**

运行：`cargo test --bin nanoquant ai_client::tests::chat_outcome_carries_its_own_log_id`
预期：FAIL，`ChatOutcome` 没有 `log_id` 字段。

- [ ] **步骤 3：实现日志 id 回传**

`ChatOutcome` 增加 `pub log_id: i64`；`log_call` 返回 `i64`（无锁或插入失败时返回 `0`，插入后返回 `conn.last_insert_rowid()`）；`chat` 的每个 `log_call(...)` 调用点赋值 `outcome.log_id = ...`；`chat_with_fallback` 自然透传。

- [ ] **步骤 4：复核阶段使用该 id**

`ai_review::review_entry` 删除 `SELECT COALESCE(MAX(id), 0) FROM ai_call_logs`，改用 `outcome.log_id`。

- [ ] **步骤 5：运行测试验证通过**

运行：`cargo test --bin nanoquant ai_client::tests`、`cargo test --bin nanoquant ai_review::tests`
预期：PASS；`review_applies_param_updates_when_enabled` 中可加断言 `parameter_changes.decision_id > 0`。

- [ ] **步骤 6：Commit**

`git commit -m "fix(ai): 调参记录精确关联本次调用日志"`

---

## 任务 4：回滚保护与调参记录固定位置

**文件：**
- 修改：`frontend/src/views/paramChanges.ts`
- 修改：`frontend/src/views/paramChanges.spec.ts`
- 修改：`frontend/src/views/StrategyView.vue`

- [ ] **步骤 1：编写失败的测试**

```ts
import { rollbackWarning, showParamChanges } from './paramChanges'

it('回滚当前分组且存在未保存修改时给出覆盖警告', () => {
  expect(rollbackWarning(true, true)).toContain('未保存')
  expect(rollbackWarning(false, true)).toContain('确认回滚')
  expect(rollbackWarning(true, false)).toContain('确认回滚')
})

it('调参记录只在策略分组显示', () => {
  expect(showParamChanges('strategy')).toBe(true)
  expect(showParamChanges('risk')).toBe(false)
})
```

- [ ] **步骤 2：运行测试验证失败**

运行：`node node_modules/vitest/vitest.mjs run paramChanges`
预期：FAIL，`rollbackWarning`/`showParamChanges` 未导出。

- [ ] **步骤 3：实现纯函数**

```ts
export function rollbackWarning(dirty: boolean, sameGroup: boolean): string {
  if (sameGroup && dirty) return '当前分组有未保存的修改，回滚会覆盖它们。继续？'
  return '确认回滚这次 AI 调参？'
}

export function showParamChanges(group: string): boolean {
  return group === 'strategy'
}
```

- [ ] **步骤 4：接入 StrategyView**

`rollbackChange` 在写库前计算 `changedGroup` 与 `sameGroup`，未通过 `confirm(rollbackWarning(dirty.value, sameGroup))` 直接返回；把调参记录块从 `v-for="sec in currentSections"` 内部移到该循环之后的独立 `v-if="showParamChanges(current)"` 区块，使 `onlyDirty` 不影响显示；`onMounted` 增加 `window.setInterval(refreshParamChanges, 60_000)`，`onUnmounted` 清理定时器。

- [ ] **步骤 5：运行测试验证通过**

运行：`node node_modules/vitest/vitest.mjs run paramChanges settingsCatalog`；`node node_modules/vue-tsc/bin/vue-tsc.js --noEmit`
预期：PASS，类型检查无输出。

- [ ] **步骤 6：Commit**

`git commit -m "fix(ui): 回滚前确认并固定调参记录位置"`

---

## 任务 5：批量计算调参效果并移除读路径 DDL

**文件：**
- 修改：`rust/nanoquant/src/settings_api.rs`
- 修改：`rust/nanoquant/src/ai_review.rs`
- 测试：`rust/nanoquant/src/settings_api.rs` 内联测试

- [ ] **步骤 1：编写失败的测试**

```rust
#[test]
fn effects_are_computed_per_change_window() {
    let conn = test_conn();
    conn.execute_batch(
        "INSERT INTO trades(mode, closed_at, pnl) VALUES
           ('demo',150,10),('demo',250,-5),('demo',350,7),('live',150,99);
         INSERT INTO parameter_changes(ts,group_name,key,old_value,new_value,reason,decision_id,mode,source)
           VALUES(100,'strategy','entry_threshold','0.3','0.35','a',1,'demo','ai'),
                 (200,'strategy','ema_fast','9','12','b',2,'demo','ai'),
                 (300,'strategy','ema_fast','12','8','c',3,'demo','ai');",
    )
    .unwrap();
    let changes = vec![(1, 100, "demo".to_string()), (2, 200, "demo".to_string()), (3, 300, "demo".to_string())];
    let effects = effects_for_changes(&conn, &changes);
    assert_eq!(effects[&1]["trades"], 3);
    assert_eq!(effects[&1]["pnl"], 12.0);
    assert_eq!(effects[&2]["trades"], 2);
    assert_eq!(effects[&3]["trades"], 1);
}
```

- [ ] **步骤 2：运行测试验证失败**

运行：`cargo test --bin nanoquant settings_api::tests::effects_are_computed_per_change_window`
预期：FAIL，`effects_for_changes` 不存在。

- [ ] **步骤 3：实现批量 effect**

在 `settings_api.rs` 增加 `fn effects_for_changes(conn: &Connection, changes: &[(i64, i64, String)]) -> HashMap<i64, Value>`：一次查询 `SELECT mode, closed_at, pnl FROM trades WHERE closed_at >= ?1`，以最早变更 `ts` 为界；在内存中按 mode（`paper`/空映射为 `demo`）与 `closed_at >= ts` 过滤，升序取前 5 笔，返回 `{"trades": n, "pnl": 合计, "note": "仅供参考，不控制行情与其他变更"}`。

- [ ] **步骤 4：路由与上下文改用批量结果**

`parameter_changes` 路由先收集 `(id, ts, mode)`，调用 `effects_for_changes` 后填充每行 `effect`；`ai_review::parameter_change_context` 同样使用该函数，删除其中的 `ensure_parameter_changes`（表由 `schema::migrate` 或 `apply_param_updates` 确保）。

同时把 `review_parameters` 的 `writable_parameters` 从完整 `catalog()["groups"]` 改为新增的 `pub(crate) fn writable_catalog() -> Value`：只输出 `ai_writable=true` 的字段，每项只含 `key/kind/min/max/step`，不含 `hidden`、`visible`、`ai_writable` 标志。新增测试 `writable_catalog_only_lists_writable_fields`：断言包含 `strategy.entry_threshold`，不包含 `engine.cycle_interval_sec`（任务 1 后已不可写）。

- [ ] **步骤 5：运行测试验证通过**

运行：`cargo test --bin nanoquant settings_api::tests`、`cargo test --bin nanoquant ai_review::tests`
预期：PASS；现有 `effect` 字段测试保持通过。

- [ ] **步骤 6：Commit**

`git commit -m "perf(ai): 批量计算调参效果并移除热路径建表"`

---

## 任务 6：调参冷却窗口

**文件：**
- 修改：`rust/nanoquant/src/default_settings.json`
- 修改：`rust/nanoquant/src/ai_review.rs`
- 测试：`rust/nanoquant/src/ai_review.rs` 内联测试

- [ ] **步骤 1：编写失败的测试**

```rust
#[test]
fn cooldown_only_blocks_inside_the_window() {
    assert!(!tuning_cooldown_active(None, 1_000, 300));
    assert!(!tuning_cooldown_active(Some(800), 1_000, 0));
    assert!(tuning_cooldown_active(Some(800), 1_000, 300));
    assert!(!tuning_cooldown_active(Some(600), 1_000, 300));
}
```

- [ ] **步骤 2：运行测试验证失败**

运行：`cargo test --bin nanoquant ai_review::tests::cooldown_only_blocks_inside_the_window`
预期：FAIL，函数不存在。

- [ ] **步骤 3：实现冷却判断**

`fn tuning_cooldown_active(last_ts: Option<i64>, now: i64, interval_sec: i64) -> bool` 返回 `interval_sec > 0 && last_ts.is_some_and(|last| now - last < interval_sec)`。

- [ ] **步骤 4：接入写回前检查**

`default_settings.json` 的 `ai` 组新增 `"param_tuning_min_interval_sec": 0`；`apply_decision_params` 在锁库后查询 `SELECT MAX(ts) FROM parameter_changes`（查询失败或表不存在时按 `None` 处理，不能用 `?` 中断复核），窗口内时把 `"调参冷却中，已忽略 N 项更新"` push 到 `risk_flags`、写 `engine_events` 并返回；`review_parameters` 的 `ai` 上下文补该字段。

- [ ] **步骤 5：运行测试验证通过**

运行：`cargo test --bin nanoquant ai_review::tests`
预期：PASS；默认 0 时既有 `review_applies_param_updates_when_enabled` 不受影响。

- [ ] **步骤 6：Commit**

`git commit -m "feat(ai): 增加调参冷却窗口"`

---

## 任务 7：敏感词与范围校验单源化

**文件：**
- 修改：`rust/nanoquant/src/param_registry.rs`
- 修改：`rust/nanoquant/src/settings_api.rs`
- 修改：`rust/nanoquant/src/ai_review.rs`
- 测试：上述文件内联测试

- [ ] **步骤 1：编写失败的测试**

```rust
#[test]
fn validate_accepts_every_registered_bound() {
    for &(group, key, min, max, _) in RANGES {
        let mut lo = Map::new();
        lo.insert(key.to_string(), json!(min));
        assert!(validate(group, &Value::Object(lo)).is_ok(), "{group}.{key} 下界");
        let mut hi = Map::new();
        hi.insert(key.to_string(), json!(max));
        assert!(validate(group, &Value::Object(hi)).is_ok(), "{group}.{key} 上界");
    }
}

#[test]
fn sensitive_mask_uses_the_same_registry() {
    assert_eq!(mask("system", "alert_webhook_url", &json!("https://x")), json!("***"));
    assert_eq!(mask("strategy", "ema_fast", &json!(9)), json!(9));
}
```

- [ ] **步骤 2：运行测试验证失败**

运行：`cargo test --bin nanoquant settings_api::tests::validate_accepts_every_registered_bound`
预期：FAIL 或 `mask` 签名不匹配。

- [ ] **步骤 3：集中可写边界与敏感词**

在 `param_registry.rs` 定义 `pub(crate) const AI_FULL_WRITABLE_GROUPS: &[&str] = &["risk", "strategy"];` 与 `const AI_BLOCKED_KEYS: &[(&str, &str)] = &[("engine", "enabled"), ("engine", "close_on_halt"), ("signal_follow", "enabled"), ("backup", "encrypt")];`；`is_ai_writable` 使用这两个常量，不再内联字面量。`ai_analysis.rs` 的提示用 `AI_FULL_WRITABLE_GROUPS.join(", ")` 生成可写组名，并加测试断言提示包含 `risk, strategy`。

`param_registry::is_sensitive` 作为唯一词表；`ai_review::strip_secrets` 改为 `fn strip_secrets(value: &mut Value, group: &str)`，对象键用 `is_sensitive(group, key)` 过滤，递归传递 `group`；`settings_api::mask` 改为 `fn mask(group: &str, key: &str, value: &Value) -> Value`，`diff` 增加 `group` 参数，调用处传当前分组。

- [ ] **步骤 4：统一范围校验**

`validate` 改为遍历 `value` 的对象键：对每个键调用 `param_registry::find(group, key)`；有 `min/max` 时要求值为有限数字并在范围内，非数字返回 `"{key}: 必须是数字"`；无范围的键不校验。删除手写的 `match group` 分支。

- [ ] **步骤 5：运行测试验证通过**

运行：`cargo test --bin nanoquant param_registry::tests settings_api::tests ai_review::tests`
预期：PASS；既有 `out_of_range_leverage_is_rejected...` 保持通过。

- [ ] **步骤 6：Commit**

`git commit -m "refactor(ai): 统一敏感词与参数范围校验"`

---

## 任务 8：全量验证与收尾

**文件：**
- 修改：本计划文档的复选框
- 测试：全量门禁

- [ ] **步骤 1：Rust 门禁**

运行：

```powershell
$env:Path = 'C:\Users\Administrator\.cargo\bin;' + $env:Path
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

预期：fmt 无差异；clippy 无警告；`cargo test` 全绿（基线 482，新增测试后应更多）。

- [ ] **步骤 2：前端门禁**

运行：

```powershell
$env:Path = 'F:\cb\.tools\node\node-v20.19.0-win-x64;' + $env:Path
node node_modules/vue-tsc/bin/vue-tsc.js --noEmit
node node_modules/vitest/vitest.mjs run
node node_modules/vite/bin/vite.js build
```

预期：vue-tsc 无输出；vitest 全绿（基线 36）；构建成功，产物包含「AI 调参记录」。

- [ ] **步骤 3：勾选本计划复选框并 Commit**

```bash
git add docs/superpowers/plans/2026-11-21-ai-param-tuning-followups.md
git commit -m "test: 验证 AI 调参闭环后续修复"
```

---

## 延后项（不在本计划范围）

- 调参效果与行情/仓位大小的因果归因：当前只提供时间窗参考值，不构成因果结论。
- `mask` 之外的历史审计数据重写：只影响新写入记录。
- 组件级 E2E：本计划用纯函数覆盖回滚提示与显示条件，不引入 `@vue/test-utils`。- 敏感字段（webhook/chat_id）在页面显式可见但 AI 上下文隐藏：这是保留给人工维护的入口，不视为 AI 可写缺口。