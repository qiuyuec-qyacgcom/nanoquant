# AI 参数读写与动态调参实现计划

> **面向 AI 代理的工作者：** 必需子技能：使用 subagent-driven-development（推荐）或 executing-plans 逐任务实现此计划。步骤使用复选框（`- [x]`）语法来跟踪进度。

**目标：** 让页面隐藏参数由后端注册表决定，并让 AI 在每轮复核中读写全部隐藏参数、留审计、可回滚，形成调参自进化闭环。

**架构：** 新增 `param_registry` 作为唯一参数元数据源；`settings_api` 暴露目录、变更记录与回滚接口；`ai_client` 解析 AI 参数更新，`ai_review` 在开关允许时通过 `apply_param_updates` 写回；前端删除写死隐藏清单，改读目录并展示调参记录。

**技术栈：** Rust（axum、rusqlite、serde_json）、Vue 3 + Pinia + Vitest。

**规格：** `docs/superpowers/specs/2026-11-21-ai-param-tuning-design.md`

## 全局约束

- 敏感字段（键名含 `secret/password/token/api_key/_enc` 或属于 `security` 组）永不进入目录、不可写。
- `ai.allow_param_tuning` 默认 `true`。
- 全部隐藏参数可写，允许任意方向调整，不限制单次数量。
- 所有写入必须经过注册表类型/范围校验并记录 `parameter_changes`。
- 参数键使用 `group.key` 形式，例如 `strategy.entry_threshold`。
- 提交信息使用中文，遵循现有 `feat/fix/refactor(scope): ...` 风格。

## 审查重点（Review Focus）

- AI 返回 `security.api_key` 或键名含 `secret/password/token` 的参数：必须丢弃并记入 `risk_flags`，不得写库。测试放进任务 4。
- AI 返回非数值、`NaN`、`Infinity` 或类型不符：跳过该项，其他合法项继续应用。测试放进任务 4 与任务 5。
- `ai.allow_param_tuning=false`：任何 `params` 都不得写入。测试放进任务 6。
- `/settings/catalog` 请求失败：前端不得崩溃，也不得回落到显示全部隐藏字段。测试放进任务 3。
- 多项调整中部分非法：合法项仍应用，非法项留痕，不得整批失败。测试放进任务 4。

---

## 文件结构

- 创建 `rust/nanoquant/src/param_registry.rs`：参数元数据、目录、查找、类型与范围校验。
- 修改 `rust/nanoquant/src/settings_api.rs`：目录/变更记录/回滚路由，`apply_param_updates`。
- 修改 `rust/nanoquant/src/schema.rs`：确保 `parameter_changes` 表存在。
- 修改 `rust/nanoquant/src/ai_client.rs`：`ParamUpdate`、`Decision.params`、v1/v2 解析。
- 修改 `rust/nanoquant/src/ai_review.rs`：应用参数、注入最近调参上下文。
- 修改 `rust/nanoquant/src/default_settings.json`：`ai.allow_param_tuning=true`。
- 修改 `rust/nanoquant/src/main.rs`：注册 `mod param_registry;`。
- 创建 `frontend/src/stores/paramCatalog.ts`：目录缓存与 `hiddenFor`。
- 创建 `frontend/src/views/paramChanges.ts`：调参记录加载与回滚的纯函数封装。
- 修改 `frontend/src/views/settingsCatalog.ts`：删除 `HIDDEN_BY_GROUP` / `HIDDEN_SETTINGS`。
- 修改 `frontend/src/views/StrategyView.vue`：动态隐藏、调参记录与回滚。
- 修改 `frontend/src/views/settingsCatalog.spec.ts`，创建 `frontend/src/stores/paramCatalog.spec.ts`、`frontend/src/views/paramChanges.spec.ts`。

---

## 任务 1：参数注册表

**文件：**
- 创建：`rust/nanoquant/src/param_registry.rs`
- 修改：`rust/nanoquant/src/main.rs`
- 测试：`rust/nanoquant/src/param_registry.rs` 内联测试

- [x] **步骤 1：编写失败的测试**

```rust
#[test]
fn catalog_hides_sensitive_fields_and_covers_strategy() {
    let catalog = catalog();
    let strategy = &catalog["groups"]["strategy"];
    assert!(strategy["hidden"].as_array().unwrap().iter().any(|v| v == "ema_fast"));
    assert!(!catalog["groups"].get("security").is_some());
    assert!(!serde_json::to_string(&catalog).unwrap().contains("api_key"));
}

#[test]
fn coerce_clamps_numbers_and_rejects_wrong_types() {
    let meta = find("strategy", "entry_threshold").unwrap();
    assert_eq!(coerce(&meta, json!(9.0)).unwrap(), json!(1.0));
    assert!(coerce(&meta, json!("x")).is_err());
    assert!(coerce(&meta, json!(f64::NAN)).is_err());
}
```

- [x] **步骤 2：运行测试验证失败**

运行：`cargo test --bin nanoquant param_registry`
预期：FAIL，模块不存在或函数未定义。

- [x] **步骤 3：实现 `param_registry.rs`**

```rust
pub(crate) struct ParamMeta {
    pub group: &'static str,
    pub key: &'static str,
    pub kind: &'static str,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub step: Option<f64>,
    pub visible: bool,
    pub ai_writable: bool,
}

pub(crate) fn catalog() -> serde_json::Value;
pub(crate) fn find(group: &str, key: &str) -> Option<ParamMeta>;
pub(crate) fn is_sensitive(group: &str, key: &str) -> bool;
pub(crate) fn coerce(meta: &ParamMeta, value: &serde_json::Value) -> Result<serde_json::Value, String>;
```

- 注册表至少覆盖现有 `HIDDEN_BY_GROUP` 的全部策略/风控/交易所/资讯/系统字段，以及 `default_settings.json` 中其余可编辑数值/布尔字段。
- `catalog()` 输出 `{ "groups": { group: { "hidden": [...], "writable": [...], "meta": { key: {...} } } } }`。
- `security` 组整体不输出；任何敏感键名不输出。
- `coerce` 对 number 做 `min/max` clamp，拒绝 `NaN`/`Infinity` 与非数值；bool/string/json 按类型校验。

- [x] **步骤 4：运行测试验证通过**

运行：`cargo test --bin nanoquant param_registry`
预期：PASS。

- [x] **步骤 5：Commit**

```bash
git add rust/nanoquant/src/param_registry.rs rust/nanoquant/src/main.rs
git commit -m "feat(settings): 新增参数注册表与目录元数据"
```

---

## 任务 2：目录接口

**文件：**
- 修改：`rust/nanoquant/src/settings_api.rs`
- 测试：`rust/nanoquant/src/settings_api.rs` 内联测试

- [x] **步骤 1：编写失败的测试**

```rust
#[tokio::test]
async fn catalog_route_returns_hidden_and_writable_fields() {
    let router = app(test_db());
    let response = router
        .oneshot(Request::get("/settings/catalog").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = json_body(response).await;
    assert!(body["groups"]["strategy"]["hidden"].as_array().unwrap().iter().any(|v| v == "ema_fast"));
    assert!(!body.to_string().contains("api_key"));
}
```

- [x] **步骤 2：运行测试验证失败**

运行：`cargo test --bin nanoquant settings_api::tests::catalog_route`
预期：FAIL，404。

- [x] **步骤 3：实现路由**

在 `routes()` 增加 `.route("/settings/catalog", get(catalog))`，实现：

```rust
async fn catalog() -> Json<Value> {
    Json(crate::param_registry::catalog())
}
```

- [x] **步骤 4：运行测试验证通过**

运行：`cargo test --bin nanoquant settings_api::tests::catalog_route`
预期：PASS。

- [x] **步骤 5：Commit**

```bash
git add rust/nanoquant/src/settings_api.rs
git commit -m "feat(settings): 暴露参数目录接口"
```

---

## 任务 3：前端动态隐藏

**文件：**
- 创建：`frontend/src/stores/paramCatalog.ts`
- 修改：`frontend/src/views/settingsCatalog.ts`
- 修改：`frontend/src/views/StrategyView.vue`
- 测试：`frontend/src/stores/paramCatalog.spec.ts`、`frontend/src/views/settingsCatalog.spec.ts`

- [x] **步骤 1：编写失败的测试**

```ts
it('loads hidden fields from the catalog', async () => {
  const store = useParamCatalogStore()
  await store.load()
  expect(store.hiddenFor('strategy').has('ema_fast')).toBe(true)
})

it('returns an empty set and does not expose everything when the request fails', async () => {
  mockApi.mockRejectedValueOnce(new Error('offline'))
  const store = useParamCatalogStore()
  await store.load()
  expect(store.hiddenFor('strategy').size).toBe(0)
  expect(store.failed).toBe(true)
})
```

- [x] **步骤 2：运行测试验证失败**

运行：`node node_modules/vitest/vitest.mjs run paramCatalog`
预期：FAIL，store 不存在。

- [x] **步骤 3：实现 store 与页面接入**

- `paramCatalog.ts`：`hidden: Record<string, Set<string>>`、`failed`、`load()` 调 `api<{groups: Record<string,{hidden:string[]}>}>('/settings/catalog')`；失败置 `failed=true`，不填充任何隐藏集合。
- `settingsCatalog.ts`：删除 `HIDDEN_BY_GROUP` 与 `HIDDEN_SETTINGS` 导出。
- `StrategyView.vue`：`skipOf(group)` 合并 `catalog.hiddenFor(group)`；`onMounted` 调 `catalog.load()`；目录失败时只显示 section 显式字段。

- [x] **步骤 4：运行测试验证通过**

运行：`node node_modules/vitest/vitest.mjs run paramCatalog settingsCatalog`
预期：PASS。

- [x] **步骤 5：Commit**

```bash
git add frontend/src/stores/paramCatalog.ts frontend/src/stores/paramCatalog.spec.ts frontend/src/views/settingsCatalog.ts frontend/src/views/StrategyView.vue frontend/src/views/settingsCatalog.spec.ts
git commit -m "refactor(ui): 隐藏参数改由后端目录决定"
```

---

## 任务 4：写回、审计与回滚

**文件：**
- 修改：`rust/nanoquant/src/schema.rs`
- 修改：`rust/nanoquant/src/settings_api.rs`
- 修改：`rust/nanoquant/src/default_settings.json`
- 测试：`rust/nanoquant/src/settings_api.rs` 内联测试

- [x] **步骤 1：编写失败的测试**

在 `settings_api.rs` 测试模块新增 `test_conn()`：内存库建 `app_settings`、`parameter_changes`、`engine_events`、`settings_audit`，并写入 `strategy` 与 `risk` 初始 JSON。

```rust
#[test]
fn apply_param_updates_writes_valid_and_skips_invalid() {
    let conn = test_conn();
    let updates = vec![
        ParamUpdate { group: "strategy".into(), key: "entry_threshold".into(), value: json!(9.0), reason: "a".into() },
        ParamUpdate { group: "security".into(), key: "api_key".into(), value: json!("x"), reason: "b".into() },
        ParamUpdate { group: "risk".into(), key: "risk_per_trade_pct".into(), value: json!("bad"), reason: "c".into() },
    ];
    let result = apply_param_updates(&conn, &updates, 7, "demo");
    assert_eq!(result["applied"].as_array().unwrap().len(), 1);
    assert_eq!(result["skipped"].as_array().unwrap().len(), 2);
    let value: String = conn.query_row("SELECT value FROM app_settings WHERE key='strategy'", [], |r| r.get(0)).unwrap();
    let parsed: Value = serde_json::from_str(&value).unwrap();
    assert_eq!(parsed["entry_threshold"], 1.0);
    let changes: i64 = conn.query_row("SELECT COUNT(*) FROM parameter_changes", [], |r| r.get(0)).unwrap();
    assert_eq!(changes, 1);
}
```

- [x] **步骤 2：运行测试验证失败**

运行：`cargo test --bin nanoquant settings_api::tests::apply_param_updates`
预期：FAIL，函数/表不存在。

- [x] **步骤 3：实现**

- `schema.rs` 增加 `ensure_parameter_changes(conn)`，执行规格中的建表 SQL；在库初始化路径调用。
- `settings_api` 增加：

```rust
pub(crate) fn apply_param_updates(
    conn: &rusqlite::Connection,
    updates: &[crate::ai_client::ParamUpdate],
    decision_id: i64,
    mode: &str,
) -> Value;
```

按 `group` 合并候选、调用现有 `validate`、逐项写入、写 `parameter_changes` 与 `engine_events`；返回 `{applied, skipped}`。
- 增加 `GET /settings/parameter-changes?limit=50` 与 `POST /settings/parameter-changes/{id}/rollback`。
- `default_settings.json` 的 `ai` 组增加 `"allow_param_tuning": true`。

- [x] **步骤 4：运行测试验证通过**

运行：`cargo test --bin nanoquant settings_api::tests`
预期：PASS。

- [x] **步骤 5：Commit**

```bash
git add rust/nanoquant/src/schema.rs rust/nanoquant/src/settings_api.rs rust/nanoquant/src/default_settings.json
git commit -m "feat(settings): AI 参数写回、审计与回滚"
```

---

## 任务 5：AI 决策参数解析

**文件：**
- 修改：`rust/nanoquant/src/ai_client.rs`
- 测试：`rust/nanoquant/src/ai_client.rs` 内联测试

- [x] **步骤 1：编写失败的测试**

```rust
#[test]
fn decision_parses_v2_param_updates() {
    let text = r#"{"a":"L","c":0.7,"l":3,"s":1,"t":2,"r":[1],"k":[1],"x":1,"p":[{"g":"strategy","k":"entry_threshold","v":0.35,"w":"放量"}]}"#;
    let decision = parse_decision(text, None).unwrap();
    assert_eq!(decision.params.len(), 1);
    assert_eq!(decision.params[0].group, "strategy");
    assert_eq!(decision.params[0].key, "entry_threshold");
    assert_eq!(decision.params[0].value, 0.35);
}

#[test]
fn decision_drops_params_without_group_or_key() {
    let text = r#"{"a":"H","c":0.5,"p":[{"k":"entry_threshold","v":0.35}]}"#;
    assert!(parse_decision(text, None).unwrap().params.is_empty());
}
```

- [x] **步骤 2：运行测试验证失败**

运行：`cargo test --bin nanoquant ai_client::tests::decision_parses_v2_param_updates`
预期：FAIL，`params` 字段不存在。

- [x] **步骤 3：实现**

```rust
#[derive(Debug, Clone, PartialEq)]
pub struct ParamUpdate {
    pub group: String,
    pub key: String,
    pub value: serde_json::Value,
    pub reason: String,
}
```

`Decision` 增加 `pub params: Vec<ParamUpdate>`（`Default` 保持空）。在 `parse_decision` 末尾读取 `params` 或 `p` 数组，逐项要求 `group`/`g` 与 `key`/`k` 非空，`value`/`v` 必须存在；缺失项丢弃。

- [x] **步骤 4：运行测试验证通过**

运行：`cargo test --bin nanoquant ai_client::tests`
预期：PASS。

- [x] **步骤 5：Commit**

```bash
git add rust/nanoquant/src/ai_client.rs
git commit -m "feat(ai): 决策 JSON 支持参数更新"
```

---

## 任务 6：AI 应用参数与自进化上下文

**文件：**
- 修改：`rust/nanoquant/src/ai_review.rs`
- 修改：`rust/nanoquant/src/ai_analysis.rs`
- 测试：`rust/nanoquant/src/ai_review.rs` 内联测试

- [x] **步骤 1：编写失败的测试**

复用现有 `ai_review` 测试夹具 `fixture(&path)`、`StaticMarket` 与 `AiState::with_transport`。新增 `ParamTransport` 实现 `AiTransport`，返回内容为：

```json
{"a":"L","c":0.8,"l":0,"s":0,"t":0,"r":[1],"k":[9],"x":2,"p":[{"g":"strategy","k":"entry_threshold","w":"放量","v":0.35}]}
```

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_applies_param_updates_when_enabled() {
    let path = std::env::temp_dir().join("nq-ai-reviewer-params.db");
    let db = fixture(&path);
    db.conn.lock().unwrap().execute(
        "UPDATE app_settings SET value=json_set(value,'$.allow_param_tuning',1) WHERE key='ai'",
        [],
    ).unwrap();
    let state = AiState::with_transport(
        db.clone(),
        MASTER_KEY.as_bytes().to_vec(),
        std::sync::Arc::new(ParamTransport),
    )
    .with_market(std::sync::Arc::new(StaticMarket));
    let reviewer = EngineAiReviewer::new(state);
    let _ = reviewer.review(&proposal("long")).unwrap();
    let raw: String = db.conn.lock().unwrap()
        .query_row("SELECT value FROM app_settings WHERE key='strategy'", [], |r| r.get(0)).unwrap();
    let strategy: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(strategy["entry_threshold"], 0.35);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_skips_param_updates_when_disabled() {
    let path = std::env::temp_dir().join("nq-ai-reviewer-params-off.db");
    let db = fixture(&path);
    db.conn.lock().unwrap().execute(
        "UPDATE app_settings SET value=json_set(value,'$.allow_param_tuning',0) WHERE key='ai'",
        [],
    ).unwrap();
    let state = AiState::with_transport(
        db.clone(),
        MASTER_KEY.as_bytes().to_vec(),
        std::sync::Arc::new(ParamTransport),
    )
    .with_market(std::sync::Arc::new(StaticMarket));
    let reviewer = EngineAiReviewer::new(state);
    let _ = reviewer.review(&proposal("long")).unwrap();
    let raw: String = db.conn.lock().unwrap()
        .query_row("SELECT value FROM app_settings WHERE key='strategy'", [], |r| r.get(0)).unwrap();
    let strategy: Value = serde_json::from_str(&raw).unwrap();
    assert_ne!(strategy["entry_threshold"], 0.35);
}
```

- [x] **步骤 2：运行测试验证失败**

运行：`cargo test --bin nanoquant ai_review::tests::review_applies_param_updates`
预期：FAIL，参数未写入。

- [x] **步骤 3：实现**

- `review_parameters` 增加 `"parameter_changes"`：最近 10 条变更及每条之后 5 笔已平仓交易的净盈亏合计。
- `review_entry` 解析出 `decision.params` 后，读取 `ai.allow_param_tuning`；为真时调用 `settings_api::apply_param_updates(conn, &decision.params, decision_id, mode)`，为假时把非空 `params` 记入 `risk_flags` 并丢弃。
- `ai_analysis.rs` 决策系统提示增加 `p` 字段说明：`"p":[{"g":"group","k":"key","v":value,"w":"reason"}]`，并提示可引用 `parameter_changes` 做回滚。
- 应用失败只记录事件，不影响 `ReviewDecision` 返回。

- [x] **步骤 4：运行测试验证通过**

运行：`cargo test --bin nanoquant ai_review::tests`
预期：PASS。

- [x] **步骤 5：Commit**

```bash
git add rust/nanoquant/src/ai_review.rs rust/nanoquant/src/ai_analysis.rs
git commit -m "feat(ai): 复核阶段应用参数更新并注入调参历史"
```

---

## 任务 7：策略页调参记录与回滚

**文件：**
- 创建：`frontend/src/views/paramChanges.ts`
- 修改：`frontend/src/views/StrategyView.vue`
- 修改：`frontend/src/views/settingsCatalog.ts`
- 测试：`frontend/src/views/paramChanges.spec.ts`、`frontend/src/views/settingsCatalog.spec.ts`

- [x] **步骤 1：编写失败的测试**

项目未使用 `@vue/test-utils` 组件挂载测试，这里测试纯函数封装 `paramChanges.ts`：

```ts
it('loads parameter changes and calls rollback', async () => {
  const apiMock = vi.fn()
    .mockResolvedValueOnce({ items: [{ id: 1, group: 'strategy', key: 'ema_fast', old_value: '9', new_value: '12', reason: 'test', ts: 1 }] })
    .mockResolvedValueOnce({ ok: true })
  const list = await loadParamChanges(apiMock, 20)
  expect(list[0].key).toBe('ema_fast')
  await rollbackParamChange(apiMock, 1)
  expect(apiMock).toHaveBeenCalledWith(
    '/settings/parameter-changes/1/rollback',
    expect.objectContaining({ method: 'POST' }),
  )
})
```

- [x] **步骤 2：运行测试验证失败**

运行：`node node_modules/vitest/vitest.mjs run settingsCatalog`
预期：FAIL，记录区不存在。

- [x] **步骤 3：实现**

- `paramChanges.ts`：导出 `loadParamChanges(api, limit)` 与 `rollbackParamChange(api, id)`；前者 GET `/settings/parameter-changes?limit=N` 返回 `items`，后者 POST 回滚并返回响应。
- 策略页「信号模型 · NQ-Fusion」区块下增加「AI 调参记录」只读表：时间、字段、旧值→新值、原因、效果、回滚按钮。
- 页面加载时调用 `loadParamChanges(api, 20)`；回滚成功后重新加载列表。
- `settingsCatalog.spec.ts` 更新：删除对 `HIDDEN_SETTINGS` 的断言，新增「目录失败时只显示显式字段」的用例。

- [x] **步骤 4：运行测试验证通过**

运行：`node node_modules/vitest/vitest.mjs run settingsCatalog`
预期：PASS。

- [x] **步骤 5：Commit**

```bash
git add frontend/src/views/StrategyView.vue frontend/src/views/settingsCatalog.ts frontend/src/views/settingsCatalog.spec.ts
git commit -m "feat(ui): 展示 AI 调参记录并支持回滚"
```

---

## 任务 8：全量验证

- [x] **步骤 1：Rust 门禁**

运行：

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

预期：全部通过。

- [x] **步骤 2：前端门禁**

运行：

```bash
node node_modules/vue-tsc/bin/vue-tsc.js --noEmit
node node_modules/vitest/vitest.mjs run
node node_modules/vite/bin/vite.js build
```

预期：全部通过；构建产物包含 `NQ-Fusion` 与「AI 调参记录」，不包含写死的隐藏清单。

- [x] **步骤 3：Commit**

```bash
git add -A
git commit -m "test: 全量验证 AI 参数读写闭环"
```