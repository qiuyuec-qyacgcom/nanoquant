# C 期：Regime 参数集、自动切换与回滚实现计划

> **面向 AI 代理的工作者：** 必需子技能：使用 superpowers:subagent-driven-development（推荐）或 superpowers:executing-plans 逐任务实现此计划。步骤使用复选框（`- [ ]`）跟踪进度。

**目标：** C 期上线后按 `market_regime` 物化对应 active 参数集；证据通过的候选在滚动 24h 限速下自动启用；实跑轮次 30 分钟无 gate 候选时回滚上一套；提供后台参数集页面与手动启用/回滚。

**架构：** `RegimeTracker` 负责首次初始化、30 分钟确认与冷却；`parameter_sets::activate` 在一个事务里写 `strategy`、参数变更审计、激活历史和事件；运行时在周期尾调用自动启用与无候选守卫；前端新增 `ParameterSetsView`。

**技术栈：** Rust 2021、rusqlite、axum、serde_json、tokio；Vue 3 + TypeScript + Vitest。

**前置阅读：** 规格 §8、§9、§11 C 期、§17 A 期实施记录；A 计划 `...-a-decouple-tuning.md`；B 计划 `...-b-backtest-evidence.md`。A/B 必须已完成。

**A 期实施后的接口变化（C 期必须适配）：**
- `active_for_regime` 返回 `Result<Option<ParameterSet>, String>`；仅无行时 `Ok(None)`。
- regime 诊断/选择读取 `engine_state['market_regime.{mode}']`；先读 `engine_state.mode`；`regime.current` 只保存当前活动 mode 的 tracker 状态。
- `parameter_set.auto_apply` 默认 `false`；C 期部署时才设 `true`。
- `settings_api::apply_param_updates` 已删除；参数集激活用 `parameter_changes(source=parameter_set/parameter_set_rollback)`。
- `tuning.rs` 已生成 candidate；B 期负责 `approved`，C 期只负责激活/回滚，不得复用 A6 生成逻辑。
- `regime.current` 为空时首次立即初始化；后续才用 30 分钟确认/冷却。

---

## 文件结构与职责

- 修改：`rust/nanoquant/src/regime.rs` — `RegimeTracker` 状态机。
- 修改：`rust/nanoquant/src/runtime.rs` — 周期内调用 tracker、自动启用、守卫。
- 修改：`rust/nanoquant/src/parameter_sets.rs` — 激活、物化、回滚、自动启用、守卫。
- 修改：`rust/nanoquant/src/parameter_sets_api.rs` — activate/rollback/current 路由。
- 修改：`rust/nanoquant/src/settings_api.rs` — 参数变更写入 helper（mode/source）。
- 修改：`frontend/src/router/index.ts`、`frontend/src/layouts/ShellLayout.vue` — 新页面入口。
- 创建：`frontend/src/views/ParameterSetsView.vue`、`frontend/src/utils/paramSets.ts`、`frontend/src/utils/paramSets.spec.ts`。
- 测试：Rust 单元/集成测试 + 前端 Vitest。

---

## 任务 1：RegimeTracker 状态机

**文件：**
- 修改：`rust/nanoquant/src/regime.rs`
- 修改：`rust/nanoquant/src/runtime.rs`（周期尾观测）
- 测试：`regime.rs`

- [ ] **步骤 1：编写失败测试**

```rust
#[test]
fn first_regime_is_immediate_then_requires_confirm_and_cooldown() {
    let mut tracker = RegimeTracker::new(1800, 1800);
    assert!(tracker.observe(Regime::Range, 1_000).is_none());
    assert_eq!(tracker.current(), Some(Regime::Range));

    // 1800 秒确认窗口内不切换
    assert!(tracker.observe(Regime::Chaos, 2_000).is_none());
    assert!(tracker.observe(Regime::Chaos, 3_000).is_none());
    assert!(tracker.observe(Regime::Chaos, 3_799).is_none());
    assert!(tracker.observe(Regime::Chaos, 3_800).is_some());
    assert_eq!(tracker.current(), Some(Regime::Chaos));

    // 冷却期内再次变化不切换
    assert!(tracker.observe(Regime::Range, 4_000).is_none());
    assert!(tracker.observe(Regime::Range, 5_800).is_none());
    assert!(tracker.observe(Regime::Range, 5_600).is_none());
}
```

- [ ] **步骤 2：运行测试确认失败**

```bash
cargo test --manifest-path rust/nanoquant/Cargo.toml first_regime_is_immediate_then_requires_confirm_and_cooldown -- --nocapture
```

预期：编译失败。

- [ ] **步骤 3：实现**

```rust
#[derive(Debug, Clone)]
pub(crate) struct RegimeTracker {
    current: Option<Regime>,
    pending: Option<Regime>,
    pending_since: Option<i64>,
    changed_at: i64,
    confirm_sec: i64,
    cooldown_sec: i64,
}

impl RegimeTracker {
    pub(crate) fn new(confirm_sec: i64, cooldown_sec: i64) -> Self {
        Self {
            current: None,
            pending: None,
            pending_since: None,
            changed_at: 0,
            confirm_sec: confirm_sec.max(1),
            cooldown_sec: cooldown_sec.max(0),
        }
    }

    pub(crate) fn current(&self) -> Option<Regime> { self.current }

    /// 返回 Some(new) 表示允许切换；None 表示保持不变。
    pub(crate) fn observe(&mut self, next: Regime, now: i64) -> Option<Regime> {
        if self.current.is_none() {
            self.current = Some(next);
            self.changed_at = now;
            self.pending = None;
            self.pending_since = None;
            return None;
        }
        let current = self.current.unwrap();
        if next == current {
            self.pending = None;
            self.pending_since = None;
            return None;
        }
        if self.pending != Some(next) {
            self.pending = Some(next);
            self.pending_since = Some(now);
            return None;
        }
        let since = self.pending_since.unwrap_or(now);
        if now - since < self.confirm_sec || now - self.changed_at < self.cooldown_sec {
            return None;
        }
        self.current = Some(next);
        self.changed_at = now;
        self.pending = None;
        self.pending_since = None;
        Some(next)
    }
}
```

在 `runtime.rs` 周期内，读取 `engine_state` 的 `regime.current` / `regime.pending` / `regime.pending_since` / `regime.changed_at`，构造 tracker；`observe(market_regime, now)`；把四个键写回。仅当返回 Some 时调用 `parameter_sets::activate_for_regime`（任务 2）。A 期已有的 `market_regime` 继续作为输入。

- [ ] **步骤 4：运行测试确认通过**

```bash
cargo test --manifest-path rust/nanoquant/Cargo.toml first_regime -- --nocapture
```

- [ ] **步骤 5：Commit**

```bash
git add rust/nanoquant/src/regime.rs rust/nanoquant/src/runtime.rs
git commit -m "feat(regime): 增加 30 分钟确认与冷却状态机"
```

---

## 任务 2：参数集激活、物化与回滚

**文件：**
- 修改：`rust/nanoquant/src/parameter_sets.rs`
- 修改：`rust/nanoquant/src/settings_api.rs`（`validate_group` 已由 A 期提供；补 `write_parameter_changes` helper）
- 测试：`parameter_sets.rs`

- [ ] **步骤 1：编写失败测试**

```rust
#[test]
fn activate_materializes_strategy_and_writes_parameter_changes_with_mode() {
    let conn = fixture_with_two_sets();
    let id = active_id(&conn, "range");
    activate(&conn, id, "manual", "live").unwrap();
    let strategy: Value = serde_json::from_str(
        &conn.query_row("SELECT value FROM app_settings WHERE key='strategy'", [], |r| r.get::<_, String>(0)).unwrap()
    ).unwrap();
    assert_eq!(strategy["atr_pct_min"], json!(0.6));
    let (mode, source, set_id): (String, String, Option<i64>) = conn.query_row(
        "SELECT mode,source,set_id FROM parameter_changes ORDER BY id DESC LIMIT 1", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))
    ).unwrap();
    assert_eq!(mode, "live");
    assert_eq!(source, "parameter_set");
    assert_eq!(set_id, Some(id));
}

#[test]
fn rollback_restores_previous_active_set() {
    let conn = fixture_with_two_sets();
    let old = active_id(&conn, "range");
    let new = insert_candidate(&conn, "range-new", "range", "1h", &json!({"timeframe":"1h","atr_pct_min":0.7}), Some(old)).unwrap();
    activate(&conn, new, "auto_apply", "demo").unwrap();
    rollback(&conn, new, "demo").unwrap();
    let strategy: Value = serde_json::from_str(
        &conn.query_row("SELECT value FROM app_settings WHERE key='strategy'", [], |r| r.get::<_, String>(0)).unwrap()
    ).unwrap();
    assert_eq!(strategy["atr_pct_min"], json!(0.6));
    let source: String = conn.query_row("SELECT source FROM parameter_changes ORDER BY id DESC LIMIT 1", [], |r| r.get(0)).unwrap();
    assert_eq!(source, "parameter_set_rollback");
}
```

- [ ] **步骤 2：运行测试确认失败**

```bash
cargo test --manifest-path rust/nanoquant/Cargo.toml activate_materializes_strategy_and_writes_parameter_changes_with_mode -- --nocapture
```

- [ ] **步骤 3：实现**

```rust
pub(crate) fn activate(
    conn: &Connection,
    set_id: i64,
    trigger: &str,
    mode: &str,
) -> Result<(), String> {
    let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
    let target = load_set(&tx, set_id)?;
    if target.status != "approved" && target.status != "active" && target.source != "bootstrap" && target.source != "manual" {
        return Err(format!("参数集状态不可激活: {}", target.status));
    }
    let current = crate::settings_api::group_value_public(&tx, "strategy");
    crate::settings_api::validate_group("strategy", &target.params)?;
    let previous = active_for_regime(&tx, &target.regime).map(|set| set.id);
    if let Some(previous_id) = previous {
        tx.execute("UPDATE parameter_sets SET status='retired' WHERE id=?1 AND status='active'", [previous_id])
            .map_err(|e| e.to_string())?;
    }
    tx.execute(
        "UPDATE parameter_sets SET status='active', activated_at=?1 WHERE id=?2",
        params![crate::now(), set_id],
    ).map_err(|e| e.to_string())?;
    let payload = serde_json::to_string(&target.params).map_err(|e| e.to_string())?;
    tx.execute(
        "INSERT INTO app_settings(key,value,updated_at) VALUES('strategy',?1,?2) \
         ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
        params![payload, crate::now()],
    ).map_err(|e| e.to_string())?;
    crate::settings_api::record_set_changes(&tx, &current, &target.params, &target.regime, set_id, mode, "parameter_set")?;
    tx.execute(
        "INSERT INTO parameter_set_activations(set_id,regime,previous_set_id,ts,trigger) VALUES(?1,?2,?3,?4,?5)",
        params![set_id, target.regime, previous, crate::now(), trigger],
    ).map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    Ok(())
}

pub(crate) fn activate_for_regime(conn: &Connection, regime: &str, trigger: &str, mode: &str) -> Result<(), String> {
    if let Some(set) = active_for_regime(conn, regime) {
        activate(conn, set.id, trigger, mode)
    } else if regime == "trend" {
        if let Some(set) = active_for_regime(conn, "range") {
            return activate(conn, set.id, trigger, mode);
        }
        Err("trend 无 active，range 也无 active".to_string())
    } else {
        Err(format!("regime {regime} 没有 active 参数集"))
    }
}

pub(crate) fn rollback(conn: &Connection, current_id: i64, mode: &str) -> Result<(), String> {
    let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
    let (regime, previous): (String, Option<i64>) = tx.query_row(
        "SELECT regime,previous_set_id FROM parameter_set_activations WHERE set_id=?1 ORDER BY id DESC LIMIT 1",
        [current_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    ).map_err(|e| e.to_string())?;
    let Some(previous) = previous else {
        return Err("没有上一套 active 参数集，只告警不回滚".to_string());
    };
    let target = load_set(&tx, previous)?;
    let current = crate::settings_api::group_value_public(&tx, "strategy");
    crate::settings_api::validate_group("strategy", &target.params)?;
    tx.execute("UPDATE parameter_sets SET status='retired' WHERE id=?1", [current_id]).map_err(|e| e.to_string())?;
    tx.execute("UPDATE parameter_sets SET status='active', activated_at=?1 WHERE id=?2", params![crate::now(), previous]).map_err(|e| e.to_string())?;
    let payload = serde_json::to_string(&target.params).map_err(|e| e.to_string())?;
    tx.execute("UPDATE app_settings SET value=?1, updated_at=?2 WHERE key='strategy'", params![payload, crate::now()]).map_err(|e| e.to_string())?;
    crate::settings_api::record_set_changes(&tx, &current, &target.params, &regime, previous, mode, "parameter_set_rollback")?;
    tx.execute(
        "INSERT INTO parameter_set_activations(set_id,regime,previous_set_id,ts,trigger) VALUES(?1,?2,?3,?4,'rollback')",
        params![previous, regime, current_id, crate::now()],
    ).map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    Ok(())
}
```

在 `settings_api.rs` 添加：

```rust
pub(crate) fn group_value_public(conn: &rusqlite::Connection, name: &str) -> Value {
    group_value(conn, name)
}

pub(crate) fn record_set_changes(
    conn: &rusqlite::Connection,
    before: &Value,
    after: &Value,
    regime: &str,
    set_id: i64,
    mode: &str,
    source: &str,
) -> Result<(), String> {
    let (Some(before), Some(after)) = (before.as_object(), after.as_object()) else {
        return Err("strategy 不是对象".to_string());
    };
    for (key, new_value) in after {
        if before.get(key) == Some(new_value) {
            continue;
        }
        let old = before.get(key);
        conn.execute(
            "INSERT INTO parameter_changes(ts,group_name,key,old_value,new_value,reason,decision_id,mode,source,set_id,regime) \
             VALUES(?1,'strategy',?2,?3,?4,?5,0,?6,?7,?8,?9)",
            params![
                crate::now(), key,
                old.and_then(|v| serde_json::to_string(v).ok()),
                serde_json::to_string(new_value).unwrap_or_default(),
                format!("参数集切换 set_id={set_id}"),
                mode, source, set_id, regime
            ],
        ).map_err(|e| e.to_string())?;
    }
    Ok(())
}
```

`load_set` 使用任务 3 的 `row_to_set`。`activate` 对 bootstrap 源的 active 集允许重复物化（C 期首次）。

- [ ] **步骤 4：运行测试确认通过**

```bash
cargo test --manifest-path rust/nanoquant/Cargo.toml activate_materializes -- --nocapture
cargo test --manifest-path rust/nanoquant/Cargo.toml rollback_restores_previous_active_set -- --nocapture
```

- [ ] **步骤 5：Commit**

```bash
git add rust/nanoquant/src/parameter_sets.rs rust/nanoquant/src/settings_api.rs
git commit -m "feat(参数集): 激活/物化/回滚并补齐审计"
```

---

## 任务 3：自动启用与无候选守卫

**文件：**
- 修改：`rust/nanoquant/src/parameter_sets.rs`
- 修改：`rust/nanoquant/src/runtime.rs`
- 测试：`parameter_sets.rs`、`runtime.rs`

- [ ] **步骤 1：编写失败测试**

```rust
#[test]
fn auto_apply_respects_since_and_rolling_24h_limit() {
    let conn = fixture_with_approved_candidates();
    let first = auto_apply(&conn, "1h", &["1h"], 1, 3_000).unwrap();
    assert_eq!(first, 1);
    let second = auto_apply(&conn, "1h", &["1h"], 1, 4_000).unwrap();
    assert_eq!(second, 0);
}

#[test]
fn guard_ignores_skipped_cycles_and_rolls_back_after_30_minutes() {
    let (db, _) = fixture_runtime();
    let set = active_id(&db.conn.lock().unwrap(), "range");
    set_guard(&db.conn.lock().unwrap(), "range", set, 1_000).unwrap();
    // 熔断轮次：不计零候选
    update_guard(&db.conn.lock().unwrap(), "range", set, false, true, 1_100).unwrap();
    update_guard(&db.conn.lock().unwrap(), "range", set, false, true, 4_000).unwrap();
    assert_eq!(active_id(&db.conn.lock().unwrap(), "range"), set);
    // 实跑零候选 30 分钟后回滚
    update_guard(&db.conn.lock().unwrap(), "range", set, false, false, 4_001).unwrap();
    update_guard(&db.conn.lock().unwrap(), "range", set, false, false, 5_801).unwrap();
    assert_ne!(active_id(&db.conn.lock().unwrap(), "range"), set);
}
```

- [ ] **步骤 2：运行测试确认失败**

```bash
cargo test --manifest-path rust/nanoquant/Cargo.toml guard_ignores_skipped -- --nocapture
```

- [ ] **步骤 3：实现**

```rust
pub(crate) fn auto_apply(
    conn: &Connection,
    live_timeframe: &str,
    whitelist: &[&str],
    per_24h: i64,
    now: i64,
) -> Result<usize, String> {
    if !whitelist.contains(&live_timeframe) {
        return Ok(0);
    }
    let since: i64 = engine_state(conn, "parameter_set.auto_apply_since")
        .and_then(|v| v.parse().ok()).unwrap_or(now);
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM parameter_set_activations WHERE trigger='auto_apply' AND ts > ?1",
        [now - 86_400],
        |r| r.get(0),
    ).unwrap_or(0);
    if count >= per_24h {
        return Ok(0);
    }
    let candidates: Vec<(i64, String, String)> = conn.prepare(
        "SELECT id,regime,timeframe FROM parameter_sets WHERE status='approved' AND source='ai' AND created_at > ?1 ORDER BY id ASC"
    )?.query_map([since], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.flatten().collect();
    let Some((id, regime, timeframe)) = candidates.into_iter().find(|(_, _, tf)| tf == live_timeframe) else {
        return Ok(0);
    };
    activate(conn, id, "auto_apply", &engine_mode(conn))?;
    let current = engine_state(conn, "regime.current").unwrap_or_else(|| regime.clone());
    if current == regime {
        set_guard(conn, &regime, id, now)?;
    }
    Ok(1)
}

pub(crate) fn set_guard(conn: &Connection, regime: &str, set_id: i64, now: i64) -> Result<(), String> {
    upsert_engine_state(conn, &format!("parameter_set.guard.{regime}"), &json!({
        "set_id": set_id, "last_candidate_at": now, "started_at": now
    }).to_string())
}

pub(crate) fn update_guard(
    conn: &Connection,
    regime: &str,
    set_id: i64,
    has_candidate: bool,
    skipped: bool,
    now: i64,
) -> Result<bool, String> {
    let key = format!("parameter_set.guard.{regime}");
    let mut guard: Value = engine_state(conn, &key)
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| json!({"set_id": set_id, "last_candidate_at": now, "started_at": now}));
    if guard["set_id"].as_i64() != Some(set_id) {
        guard = json!({"set_id": set_id, "last_candidate_at": now, "started_at": now});
    }
    if skipped || has_candidate {
        guard["last_candidate_at"] = json!(now);
        upsert_engine_state(conn, &key, &guard.to_string())?;
        return Ok(false);
    }
    let last = guard["last_candidate_at"].as_i64().unwrap_or(now);
    if now - last < 1800 {
        upsert_engine_state(conn, &key, &guard.to_string())?;
        return Ok(false);
    }
    rollback(conn, set_id, &engine_mode(conn))?;
    guard["last_candidate_at"] = json!(now);
    upsert_engine_state(conn, &key, &guard.to_string())?;
    Ok(true)
}
```

在 `parameter_sets.rs` 补齐以下辅助函数（签名固定）：

```rust
fn engine_state(conn: &rusqlite::Connection, key: &str) -> Option<String>;
fn upsert_engine_state(conn: &rusqlite::Connection, key: &str, value: &str) -> Result<(), String>;
fn engine_mode(conn: &rusqlite::Connection) -> String;
```

并在 `runtime.rs` 暴露：

```rust
pub(crate) fn engine_mode_public(conn: &rusqlite::Connection) -> String {
    conn.query_row("SELECT value FROM engine_state WHERE key='mode'", [], |r| r.get::<_, String>(0))
        .unwrap_or_else(|_| "demo".to_string())
}
```

在 `runtime.rs` 周期尾（`run_cycle_state` 拿到 `report` 后）：

```rust
if let Some(regime) = crate::regime::market_regime(&regimes) {
    let skipped = !report.skipped.is_empty();
    let has_candidate = !report.ai_symbols.is_empty();
    let current_set = crate::parameter_sets::active_for_regime(&tx, regime.as_str());
    if let Some(set) = current_set {
        let rolled = crate::parameter_sets::update_guard(
            &tx, regime.as_str(), set.id, has_candidate, skipped, crate::now(),
        )?;
        if rolled {
            tracing::warn!(%regime, "无候选回滚触发");
        }
    }
}
```

自动启用放在 regime tracker 切换之后、guard 之前；C 期上线时写 `engine_state['parameter_set.auto_apply_since']=now`，并立即物化当前 regime 的 active 集：

```rust
if engine_state(&tx, "parameter_set.current_set_id").is_none() {
    crate::parameter_sets::activate_for_regime(&tx, current_regime.as_str(), "bootstrap", &mode)?;
}
```

- [ ] **步骤 4：运行测试确认通过**

```bash
cargo test --manifest-path rust/nanoquant/Cargo.toml auto_apply_respects_since_and_rolling_24h_limit -- --nocapture
cargo test --manifest-path rust/nanoquant/Cargo.toml guard_ignores_skipped_cycles_and_rolls_back_after_30_minutes -- --nocapture
```

- [ ] **步骤 5：Commit**

```bash
git add rust/nanoquant/src/parameter_sets.rs rust/nanoquant/src/runtime.rs
git commit -m "feat(参数集): 自动启用限速与无候选回滚"
```

---

## 任务 4：参数集 API（activate/rollback/current）

**文件：**
- 修改：`rust/nanoquant/src/parameter_sets_api.rs`
- 测试：`parameter_sets_api.rs`

- [ ] **步骤 1：编写失败测试**

```rust
#[tokio::test]
async fn activate_and_rollback_routes_require_token_and_audit() {
    let (db, token) = api_fixture();
    let app = crate::parameter_sets_api::routes().with_state(db);
    let response = app.clone().oneshot(
        Request::post("/parameter-sets/1/activate")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty()).unwrap()
    ).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}
```

- [ ] **步骤 2：运行测试确认失败**

```bash
cargo test --manifest-path rust/nanoquant/Cargo.toml activate_and_rollback_routes -- --nocapture
```

- [ ] **步骤 3：实现**

在 `parameter_sets_api.rs` 增加：

```rust
.route("/parameter-sets/{id}/activate", post(activate_set))
.route("/parameter-sets/{id}/rollback", post(rollback_set))
```

```rust
async fn activate_set(State(db): State<Db>, Path(id): Path<i64>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = db.conn.lock().map_err(|_| internal("数据库锁不可用"))?;
    let mode = crate::runtime::engine_mode_public(&conn);
    crate::parameter_sets::activate(&conn, id, "manual", &mode)
        .map_err(|detail| bad(StatusCode::BAD_REQUEST, &detail))?;
    Ok(json!({"ok": true, "id": id}).into())
}

async fn rollback_set(State(db): State<Db>, Path(id): Path<i64>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = db.conn.lock().map_err(|_| internal("数据库锁不可用"))?;
    let mode = crate::runtime::engine_mode_public(&conn);
    crate::parameter_sets::rollback(&conn, id, &mode)
        .map_err(|detail| bad(StatusCode::BAD_REQUEST, &detail))?;
    Ok(json!({"ok": true, "id": id}).into())
}
```

`/parameter-sets/current` 返回：

```json
{
  "market_regime": "range",
  "active": [{"regime":"range","set_id":1,"name":"range-bootstrap","timeframe":"1h"}],
  "guard": {"range": {"set_id":1,"last_candidate_at":0}},
  "auto_apply_since": 0
}
```

- [ ] **步骤 4：运行测试确认通过**

```bash
cargo test --manifest-path rust/nanoquant/Cargo.toml activate_and_rollback_routes -- --nocapture
```

- [ ] **步骤 5：Commit**

```bash
git add rust/nanoquant/src/parameter_sets_api.rs
git commit -m "feat(参数集): 增加激活与回滚 API"
```

---

## 任务 5：参数集后台页面

**文件：**
- 创建：`frontend/src/views/ParameterSetsView.vue`
- 创建：`frontend/src/utils/paramSets.ts`
- 创建：`frontend/src/utils/paramSets.spec.ts`
- 修改：`frontend/src/router/index.ts`
- 修改：`frontend/src/layouts/ShellLayout.vue`

- [ ] **步骤 1：编写失败测试**

`paramSets.spec.ts`：

```ts
import { describe, expect, it } from 'vitest'
import { evidenceSummary, statusTag } from './paramSets'

describe('paramSets formatting', () => {
  it('summarizes evidence gate', () => {
    expect(evidenceSummary({
      data: { folds: 3, symbols_used: ['BTCUSDT'] },
      candidate: { oos_trades: 31, oos_avg_return_pct: 0.42, bootstrap_ci_low: 0.01 },
      gate: { pass: true, reasons: [] },
    })).toContain('31 笔')
  })
  it('maps status tags', () => {
    expect(statusTag('active').text).toBe('生效中')
    expect(statusTag('candidate').text).toBe('候选')
    expect(statusTag('rejected').text).toBe('已拒绝')
  })
})
```

- [ ] **步骤 2：运行测试确认失败**

```bash
cd frontend
npx vitest run src/utils/paramSets.spec.ts
```

预期：模块不存在。

- [ ] **步骤 3：实现**

`paramSets.ts`：

```ts
export interface EvidenceLike {
  data?: { folds?: number; symbols_used?: string[]; target_regime?: string }
  candidate?: { oos_trades?: number; oos_avg_return_pct?: number; bootstrap_ci_low?: number }
  gate?: { pass?: boolean; reasons?: string[] }
}

export function evidenceSummary(evidence: EvidenceLike): string {
  const trades = evidence.candidate?.oos_trades ?? 0
  const avg = (evidence.candidate?.oos_avg_return_pct ?? 0).toFixed(2)
  const folds = evidence.data?.folds ?? 0
  return `${folds} 折 · ${trades} 笔 · 均收益 ${avg}%`
}

export function statusTag(status: string): { text: string; cls: string } {
  const map: Record<string, { text: string; cls: string }> = {
    active: { text: '生效中', cls: 'tag-long' },
    approved: { text: '已通过', cls: 'tag-blue' },
    candidate: { text: '候选', cls: 'tag-dim' },
    rejected: { text: '已拒绝', cls: 'tag-short' },
    retired: { text: '已退役', cls: 'tag-dim' },
  }
  return map[status] || { text: status, cls: 'tag-dim' }
}
```

`ParameterSetsView.vue` 使用 `http.get('/parameter-sets')`、`/parameter-sets/current`，展示当前 regime、active 集、候选列表、证据、激活历史；手动启用/回滚走 `http.post` 并确认。页面结构与 `SignalAuditView` 一致：`panel`、`table`、`hint`，不要新增全局样式。

路由：

```ts
{ path: 'parameter-sets', name: 'parameter-sets', component: () => import('../views/ParameterSetsView.vue'), meta: { title: '参数集' } },
```

菜单：

```ts
{ path: '/admin/parameter-sets', label: '参数集', ico: '⚙' },
```

放在「研究」分组、`参数配置` 之前或之后。

- [ ] **步骤 4：运行测试确认通过**

```bash
cd frontend
npx vitest run src/utils/paramSets.spec.ts
npm run typecheck
npm run build
```

- [ ] **步骤 5：Commit**

```bash
git add frontend/src/views/ParameterSetsView.vue frontend/src/utils/paramSets.ts frontend/src/utils/paramSets.spec.ts frontend/src/router/index.ts frontend/src/layouts/ShellLayout.vue
git commit -m "feat(参数集): 增加后台参数集页面"
```

---

## 任务 6：C 期迁移与验收

**文件：**
- 修改：`rust/nanoquant/src/parameter_sets.rs`（C 期初始化 auto_apply_since）
- 修改：`rust/nanoquant/src/runtime.rs`
- 测试：集成测试

- [ ] **步骤 1：编写失败测试**

```rust
#[test]
fn c_phase_enables_auto_apply_since_and_materializes_bootstrap() {
    let (db, _) = fixture_c_phase();
    let conn = db.conn.lock().unwrap();
    crate::parameter_sets::enable_c_phase(&conn, "range").unwrap();
    let since = engine_state(&conn, "parameter_set.auto_apply_since");
    assert!(since.is_some());
    let strategy: Value = serde_json::from_str(
        &conn.query_row("SELECT value FROM app_settings WHERE key='strategy'", [], |r| r.get::<_,String>(0)).unwrap()
    ).unwrap();
    assert_eq!(strategy["atr_pct_min"], json!(0.6));
    assert_eq!(strategy["atr_pct_max"], json!(10.0));
    assert_eq!(strategy["volume_ratio_min"], json!(0.8));
    assert_eq!(strategy["entry_threshold"], json!(0.45));
}
```

- [ ] **步骤 2：运行测试确认失败**

```bash
cargo test --manifest-path rust/nanoquant/Cargo.toml c_phase_enables_auto_apply_since -- --nocapture
```

- [ ] **步骤 3：实现**

```rust
pub(crate) fn enable_c_phase(conn: &Connection, current_regime: &str) -> Result<(), String> {
    let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
    tx.execute(
        "INSERT INTO engine_state(key,value,updated_at) VALUES('parameter_set.auto_apply_since',?1,?2) \
         ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
        params![crate::now().to_string(), crate::now()],
    ).map_err(|e| e.to_string())?;
    let set = active_for_regime(&tx, current_regime)
        .or_else(|| active_for_regime(&tx, "range"))
        .ok_or_else(|| "没有可初始化的 active 参数集".to_string())?;
    let current = crate::settings_api::group_value_public(&tx, "strategy");
    crate::settings_api::validate_group("strategy", &set.params)?;
    tx.execute("UPDATE app_settings SET value=?1, updated_at=?2 WHERE key='strategy'",
        params![serde_json::to_string(&set.params).unwrap(), crate::now()]).map_err(|e| e.to_string())?;
    crate::settings_api::record_set_changes(&tx, &current, &set.params, current_regime, set.id, &crate::runtime::engine_mode_public(&tx), "parameter_set")?;
    tx.execute("INSERT INTO parameter_set_activations(set_id,regime,previous_set_id,ts,trigger) VALUES(?1,?2,NULL,?3,'bootstrap')",
        params![set.id, current_regime, crate::now()]).map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    Ok(())
}
```

C 期部署时在 migrate 后调用 `enable_c_phase(&conn, current_regime)`；`current_regime` 从 `engine_state['market_regime']` 读取，缺省 `range`。

- [ ] **步骤 4：运行测试确认通过**

```bash
cargo test --manifest-path rust/nanoquant/Cargo.toml c_phase_enables_auto_apply_since -- --nocapture
cargo test --release --locked --manifest-path rust/nanoquant/Cargo.toml
```

- [ ] **步骤 5：Commit**

```bash
git add rust/nanoquant/src/parameter_sets.rs rust/nanoquant/src/runtime.rs
git commit -m "feat(参数集): C 期启用 bootstrap 与自动切换"
```

---

## 验收清单

- [ ] 首个有 quorum 的 regime 立即初始化，range 时首个实跑轮次 gate 候选 > 0。
- [ ] 自动启用只处理 `created_at > auto_apply_since`、timeframe 匹配、滚动 24h 最多 1 次。
- [ ] 30 分钟无候选回滚只在 `report.skipped.is_empty()` 的实跑轮次累计。
- [ ] 激活事务失败时 `strategy` 不变；`parameter_changes` 含 mode/decision_id/source/set_id/regime。
- [ ] 手动启用/回滚页面确认弹窗与 API 审计一致。
- [ ] `cargo fmt --check`、`cargo clippy -- -D warnings`、`cargo test --release --locked`、`npm test`、`npm run typecheck`、`npm run build` 全绿。

---

## 自检

- 规格覆盖：§8 C 期、§9 权限与回滚、§11 C 期验收、§12 C2/C7/C8/B2/B5 测试。
- 类型一致性：`RegimeTracker`、`activate`、`rollback`、`auto_apply`、`update_guard` 与 A/B 计划的 `ParameterSet`/`BasketMetrics` 对齐。
- 未引入 risk 自动切换；所有切换只写 `strategy`。