# A 期：AI 调参解耦与参数集注册表实现计划

> **面向 AI 代理的工作者：** 必需子技能：使用 superpowers:subagent-driven-development（推荐）或 superpowers:executing-plans 逐任务实现此计划。步骤使用复选框（`- [ ]`）语法来跟踪进度。

**目标：** 把 AI 调参从每轮开仓复核中剥离，建立参数集注册表与独立 24h 调参任务；候选只落库，不写 `app_settings.strategy`。

**架构：** 新增 `parameter_sets` / `parameter_set_activations` 表与 `parameter_set` 配置组；A 期迁移种子化 bootstrap（仅 1h）并部署只读 `market_regime`；入口复核不再应用 `Decision.params`，独立任务生成 candidate，风险参数永久移出 AI 可写目录。

**技术栈：** Rust 2021、rusqlite、axum、serde_json、tokio；Vue 3 + TypeScript。

**前置阅读：** `docs/superpowers/specs/2026-11-23-parameter-set-governance-design.md`。开始实现前必须完整读取，尤其 §3 Timeframe 口径、§5 数据模型、§6 A 期、§9 权限边界。

---

## 文件结构与职责

- 创建：`rust/nanoquant/src/parameter_sets.rs` — 参数集类型、校验、注册表查询、candidate 写入、bootstrap 种子化。
- 创建：`rust/nanoquant/src/regime.rs` — `Regime` 枚举、分类、market regime 多数聚合。
- 创建：`rust/nanoquant/src/tuning.rs` — 独立调参任务与 AI 候选生成。
- 修改：`rust/nanoquant/src/schema.rs` — 新表与新列迁移。
- 修改：`rust/nanoquant/src/param_registry.rs` — 新组可见性、AI 黑名单、`ai_bounds`、risk 移出可写组。
- 修改：`rust/nanoquant/src/settings_api.rs` — 新配置组、`parameter_changes` 新列。
- 修改：`rust/nanoquant/src/default_settings.json` — `parameter_set` 组默认值、AI 间隔默认 86400。
- 修改：`rust/nanoquant/src/ai_review.rs` — 入口复核移除调参调用与参数上下文。
- 修改：`rust/nanoquant/src/trade_policy.rs` — 复用 regime 分类，暴露只读 regime。
- 修改：`rust/nanoquant/src/runtime.rs` — 记录只读 market regime；不做切换。
- 修改：`rust/nanoquant/src/main.rs` — 挂载新模块、启动 tuning 任务。
- 修改：`frontend/src/views/settingsCatalog.ts` — 参数集配置组展示。
- 测试：Rust 单元/集成测试在各模块 `#[cfg(test)]` 中；前端 `frontend/src/utils/parameterSets.spec.ts`（A 期只需基础格式化逻辑，完整页面在 C 期）。

---

## 任务 1：参数集配置组、AI 权限收口与 AI 边界

**文件：**
- 修改：`rust/nanoquant/src/default_settings.json`
- 修改：`rust/nanoquant/src/param_registry.rs`
- 修改：`rust/nanoquant/src/settings_api.rs:34-48`
- 修改：`frontend/src/views/settingsCatalog.ts`
- 测试：`rust/nanoquant/src/param_registry.rs` 的 `mod tests`

- [ ] **步骤 1：编写失败测试**

在 `param_registry.rs` 的 `mod tests` 中添加：

```rust
#[test]
fn parameter_set_group_is_visible_but_ai_blocked() {
    let value = catalog();
    let group = &value["groups"]["parameter_set"];
    assert_eq!(group["writable"], json!([]));
    for key in [
        "enabled",
        "auto_apply",
        "max_auto_apply_per_24h",
        "no_candidate_rollback_sec",
        "regime_confirm_sec",
        "regime_cooldown_sec",
        "min_oos_trades",
        "auto_apply_timeframes",
    ] {
        assert_eq!(group["meta"][key]["visible"], json!(true), "{key} 应可见");
        assert_eq!(group["meta"][key]["ai_writable"], json!(false), "{key} 不可被 AI 写");
    }
}

#[test]
fn risk_is_not_ai_writable_after_a_phase() {
    assert!(!find("risk", "max_leverage").unwrap().ai_writable);
    assert!(!find("risk", "risk_per_trade_pct").unwrap().ai_writable);
    assert!(find("strategy", "entry_threshold").unwrap().ai_writable);
}

#[test]
fn ai_bounds_are_stricter_than_manual_ranges() {
    assert_eq!(ai_bounds("strategy", "atr_pct_min"), Some((0.0, 1.0)));
    assert_eq!(ai_bounds("strategy", "atr_pct_max"), Some((6.0, 20.0)));
    assert_eq!(ai_bounds("strategy", "volume_ratio_min"), Some((0.0, 1.0)));
    assert_eq!(ai_bounds("strategy", "entry_threshold"), Some((0.01, 0.5)));
    assert_eq!(ai_bounds("risk", "max_leverage"), None);
}
```

- [ ] **步骤 2：运行测试确认失败**

运行：

```bash
cargo test --manifest-path rust/nanoquant/Cargo.toml parameter_set_group_is_visible_but_ai_blocked -- --nocapture
```

预期：编译失败或断言失败，提示缺少 `parameter_set` 组、`ai_bounds`。

- [ ] **步骤 3：实现配置组与权限边界**

在 `default_settings.json` 增加 `parameter_set` 组（放在 `strategy` 之后）：

```json
"parameter_set": {
  "enabled": true,
  "auto_apply": false,
  "max_auto_apply_per_24h": 1,
  "no_candidate_rollback_sec": 1800,
  "regime_confirm_sec": 1800,
  "regime_cooldown_sec": 1800,
  "min_oos_trades": 30,
  "auto_apply_timeframes": "1h"
}
```

在 `param_registry.rs` 修改：

```rust
pub(crate) const AI_FULL_WRITABLE_GROUPS: &[&str] = &["strategy"];

const AI_BLOCKED_GROUPS: &[&str] = &["parameter_set"];

fn is_ai_writable(group: &str, key: &str) -> bool {
    if group == "ai"
        || AI_BLOCKED_GROUPS.contains(&group)
        || AI_BLOCKED_KEYS.contains(&(group, key))
    {
        return false;
    }
    if is_sensitive(group, key) {
        return false;
    }
    if AI_FULL_WRITABLE_GROUPS.contains(&group) {
        return true;
    }
    !is_visible(group, key)
}

pub(crate) fn ai_bounds(group: &str, key: &str) -> Option<(f64, f64)> {
    match (group, key) {
        ("strategy", "atr_pct_min") => Some((0.0, 1.0)),
        ("strategy", "atr_pct_max") => Some((6.0, 20.0)),
        ("strategy", "volume_ratio_min") => Some((0.0, 1.0)),
        ("strategy", "entry_threshold") => Some((0.01, 0.5)),
        _ => None,
    }
}
```

在 `VISIBLE` 中新增：

```rust
(
    "parameter_set",
    &[
        "enabled",
        "auto_apply",
        "max_auto_apply_per_24h",
        "no_candidate_rollback_sec",
        "regime_confirm_sec",
        "regime_cooldown_sec",
        "min_oos_trades",
        "auto_apply_timeframes",
    ],
),
```

在 `settings_api.rs` 把 `GROUPS` 改为 14 项并加入 `parameter_set`：

```rust
const GROUPS: [&str; 14] = [
    "engine", "risk", "strategy", "ai", "data", "news", "evolution",
    "backup", "security", "system", "public", "signal_follow", "journal",
    "parameter_set",
];
```

在 `default_settings.json` 的 `ai` 组把 `param_tuning_min_interval_sec` 默认值从 `0` 改为 `86400`，删除不存在的 `param_tuning_interval_sec`（若没有则忽略）。

在 `frontend/src/views/settingsCatalog.ts` 增加：

```ts
{ id: 'parameter_set', label: '参数集', hint: 'regime 参数集、证据门与自动切换' },
```

并在 `SETTINGS_SECTIONS` 增加：

```ts
parameter_set: [
  { title: '启用与限速', fields: ['enabled', 'auto_apply', 'max_auto_apply_per_24h', 'auto_apply_timeframes'] },
  { title: 'regime 与回滚', fields: ['regime_confirm_sec', 'regime_cooldown_sec', 'no_candidate_rollback_sec'] },
  { title: '证据门', fields: ['min_oos_trades'] },
],
```

- [ ] **步骤 4：运行测试确认通过**

```bash
cargo test --manifest-path rust/nanoquant/Cargo.toml parameter_set_group_is_visible_but_ai_blocked -- --nocapture
cargo test --manifest-path rust/nanoquant/Cargo.toml risk_is_not_ai_writable_after_a_phase -- --nocapture
cargo test --manifest-path rust/nanoquant/Cargo.toml ai_bounds_are_stricter_than_manual_ranges -- --nocapture
```

预期：PASS。

- [ ] **步骤 5：Commit**

```bash
git add rust/nanoquant/src/default_settings.json rust/nanoquant/src/param_registry.rs rust/nanoquant/src/settings_api.rs frontend/src/views/settingsCatalog.ts
git commit -m "feat(参数集): 新增参数集配置组并收紧 AI 写权限"
```

---

## 任务 2：参数集表、激活表与 bootstrap 种子化

**文件：**
- 修改：`rust/nanoquant/src/schema.rs:120-135`
- 创建：`rust/nanoquant/src/parameter_sets.rs`
- 修改：`rust/nanoquant/src/main.rs`（`mod parameter_sets;`）
- 测试：`parameter_sets.rs` 的 `mod tests`

- [ ] **步骤 1：编写失败测试**

在 `parameter_sets.rs` 添加：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE app_settings(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER); \
             INSERT INTO app_settings VALUES('strategy','{\"timeframe\":\"1h\",\"atr_pct_min\":1.5,\"atr_pct_max\":8,\"volume_ratio_min\":1.1,\"entry_threshold\":0.55}',0);",
        )
        .unwrap();
        crate::schema::ensure_parameter_sets(&conn).unwrap();
        conn
    }

    #[test]
    fn bootstrap_seeds_range_and_chaos_only_for_1h_and_keeps_strategy() {
        let conn = conn();
        seed_bootstrap(&conn).unwrap();
        let before: String = conn
            .query_row("SELECT value FROM app_settings WHERE key='strategy'", [], |r| r.get(0))
            .unwrap();
        assert!(before.contains("1.5"));
        let rows: Vec<(String, String, String, String)> = conn
            .prepare("SELECT name, regime, timeframe, status FROM parameter_sets ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .flatten()
            .collect();
        assert!(rows.contains(&("range-bootstrap".into(), "range".into(), "1h".into(), "active".into())));
        assert!(rows.contains(&("chaos-bootstrap".into(), "chaos".into(), "1h".into(), "active".into())));
    }

    #[test]
    fn bootstrap_is_idempotent() {
        let conn = conn();
        seed_bootstrap(&conn).unwrap();
        seed_bootstrap(&conn).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM parameter_sets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 3);
    }
}
```

- [ ] **步骤 2：运行测试确认失败**

```bash
cargo test --manifest-path rust/nanoquant/Cargo.toml bootstrap_seeds_range_and_chaos_only_for_1h_and_keeps_strategy -- --nocapture
```

预期：编译失败，缺少 `ensure_parameter_sets` / `seed_bootstrap`。

- [ ] **步骤 3：实现表结构与种子化**

在 `schema.rs` 添加并在 `migrate` 的 `ensure_parameter_changes(conn)?;` 后调用：

```rust
ensure_parameter_sets(conn)?;
```

在 `schema.rs` 添加：

```rust
pub(crate) fn ensure_parameter_sets(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS parameter_sets(\
         id INTEGER PRIMARY KEY, name TEXT NOT NULL, regime TEXT NOT NULL, timeframe TEXT NOT NULL, \
         version INTEGER NOT NULL, params TEXT NOT NULL, status TEXT NOT NULL, source TEXT NOT NULL, \
         evidence TEXT NOT NULL DEFAULT '{}', parent_id INTEGER, created_at INTEGER NOT NULL, activated_at INTEGER); \
         CREATE UNIQUE INDEX IF NOT EXISTS ix_parameter_sets_active_regime ON parameter_sets(regime) WHERE status='active'; \
         CREATE INDEX IF NOT EXISTS ix_parameter_sets_regime_status ON parameter_sets(regime,status); \
         CREATE TABLE IF NOT EXISTS parameter_set_activations(\
         id INTEGER PRIMARY KEY, set_id INTEGER NOT NULL, regime TEXT NOT NULL, previous_set_id INTEGER, \
         ts INTEGER NOT NULL, trigger TEXT NOT NULL); \
         CREATE INDEX IF NOT EXISTS ix_parameter_set_activations_ts ON parameter_set_activations(ts);",
    )
    .map_err(|e| format!("建表 parameter_sets 失败: {e}"))?;
    if !column_exists(conn, "parameter_changes", "set_id").unwrap_or(false) {
        conn.execute_batch("ALTER TABLE parameter_changes ADD COLUMN set_id INTEGER;")
            .map_err(|e| format!("补列 parameter_changes.set_id 失败: {e}"))?;
    }
    if !column_exists(conn, "parameter_changes", "regime").unwrap_or(false) {
        conn.execute_batch("ALTER TABLE parameter_changes ADD COLUMN regime TEXT;")
            .map_err(|e| format!("补列 parameter_changes.regime 失败: {e}"))?;
    }
    if !column_exists(conn, "trades", "parameter_set_id").unwrap_or(false) {
        conn.execute_batch("ALTER TABLE trades ADD COLUMN parameter_set_id INTEGER;")
            .map_err(|e| format!("补列 trades.parameter_set_id 失败: {e}"))?;
    }
    Ok(())
}
```

如果 `schema.rs` 没有 `column_exists`，复用已有 `column_exists`/`table_exists` 辅助函数；若不存在则添加：

```rust
fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool, String> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info(\"{table}\")"))
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| e.to_string())?;
    Ok(rows.flatten().any(|name| name == column))
}
```

在 `parameter_sets.rs` 添加种子化：

```rust
use rusqlite::{params, Connection};
use serde_json::{json, Value};

const BOOTSTRAP_TIMEFRAME: &str = "1h";

pub(crate) fn seed_bootstrap(conn: &Connection) -> Result<(), String> {
    let exists: i64 = conn
        .query_row("SELECT COUNT(*) FROM parameter_sets", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    if exists > 0 {
        return Ok(());
    }
    let raw: String = conn
        .query_row("SELECT value FROM app_settings WHERE key='strategy'", [], |r| r.get(0))
        .map_err(|_| "缺少 strategy 配置".to_string())?;
    let current: Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
    let timeframe = current["timeframe"].as_str().unwrap_or("4h");
    if timeframe != BOOTSTRAP_TIMEFRAME {
        let retired = with_overrides(&current, &[]);
        insert_set(conn, "current-snapshot", "range", timeframe, &retired, "retired", "manual", "{}", None)
            .map_err(|e| e.to_string())?;
        return Ok(());
    }
    let range = with_overrides(
        &current,
        &[
            ("atr_pct_min", json!(0.6)),
            ("atr_pct_max", json!(10.0)),
            ("volume_ratio_min", json!(0.8)),
            ("entry_threshold", json!(0.45)),
        ],
    );
    let chaos = with_overrides(
        &current,
        &[
            ("atr_pct_min", json!(1.0)),
            ("atr_pct_max", json!(8.0)),
            ("volume_ratio_min", json!(1.0)),
            ("entry_threshold", json!(0.5)),
        ],
    );
    let live = with_overrides(&current, &[]);
    let range_id = insert_set(conn, "range-bootstrap", "range", timeframe, &range, "active", "bootstrap", "{\"kind\":\"bootstrap\"}", None)?;
    insert_activation(conn, range_id, "range", None, "bootstrap")?;
    let chaos_id = insert_set(conn, "chaos-bootstrap", "chaos", timeframe, &chaos, "active", "bootstrap", "{\"kind\":\"bootstrap\"}", None)?;
    insert_activation(conn, chaos_id, "chaos", None, "bootstrap")?;
    insert_set(conn, "chaos-live-2026-11-23", "chaos", timeframe, &live, "retired", "manual", "{\"kind\":\"historical\"}", None)?;
    Ok(())
}
```

`with_overrides` 创建完整 strategy 快照；`insert_set`/`insert_activation` 由任务 3 实现前可先写最小版本（本任务需要）：

```rust
fn with_overrides(base: &Value, overrides: &[(&str, Value)]) -> Value {
    let mut out = base.clone();
    if let Some(object) = out.as_object_mut() {
        for (key, value) in overrides {
            object.insert((*key).to_string(), value.clone());
        }
    }
    out
}

fn insert_set(
    conn: &Connection,
    name: &str,
    regime: &str,
    timeframe: &str,
    params: &Value,
    status: &str,
    source: &str,
    evidence: &str,
    parent_id: Option<i64>,
) -> Result<i64, String> {
    conn.execute(
        "INSERT INTO parameter_sets(name,regime,timeframe,version,params,status,source,evidence,parent_id,created_at,activated_at) \
         VALUES(?1,?2,?3,1,?4,?5,?6,?7,?8,?9,?10)",
        params![
            name, regime, timeframe, serde_json::to_string(params).unwrap(),
            status, source, evidence, parent_id, crate::now(),
            if status == "active" { Some(crate::now()) } else { None }
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(conn.last_insert_rowid())
}

fn insert_activation(conn: &Connection, set_id: i64, regime: &str, previous: Option<i64>, trigger: &str) -> Result<(), String> {
    conn.execute(
        "INSERT INTO parameter_set_activations(set_id,regime,previous_set_id,ts,trigger) VALUES(?1,?2,?3,?4,?5)",
        params![set_id, regime, previous, crate::now(), trigger],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}
```

在 `schema.rs::migrate` 中，`ensure_parameter_sets(conn)?` 之后调用：

```rust
let _ = crate::parameter_sets::seed_bootstrap(conn)?;
```

在 `main.rs` 增加 `mod parameter_sets;`。

- [ ] **步骤 4：运行测试确认通过**

```bash
cargo test --manifest-path rust/nanoquant/Cargo.toml bootstrap_seeds -- --nocapture
cargo test --manifest-path rust/nanoquant/Cargo.toml bootstrap_is_idempotent -- --nocapture
```

预期：PASS。

- [ ] **步骤 5：Commit**

```bash
git add rust/nanoquant/src/schema.rs rust/nanoquant/src/parameter_sets.rs rust/nanoquant/src/main.rs
git commit -m "feat(参数集): 新增参数集表并种子化 bootstrap"
```

---

## 任务 3：参数集注册表与候选校验

**文件：**
- 修改：`rust/nanoquant/src/parameter_sets.rs`
- 测试：`parameter_sets.rs` 的 `mod tests`

- [ ] **步骤 1：编写失败测试**

```rust
#[test]
fn candidate_validation_rejects_risk_and_three_key_limit() {
    let active = json!({"timeframe":"1h","atr_pct_min":0.6,"atr_pct_max":10,"volume_ratio_min":0.8,"entry_threshold":0.45});
    let risk = vec![ParamUpdate { group: "risk".into(), key: "max_leverage".into(), value: json!(50), reason: "x".into() }];
    assert!(validate_candidate(&active, &risk).is_err());

    let four = vec![
        ParamUpdate { group: "strategy".into(), key: "atr_pct_min".into(), value: json!(0.55), reason: "x".into() },
        ParamUpdate { group: "strategy".into(), key: "atr_pct_max".into(), value: json!(9.5), reason: "x".into() },
        ParamUpdate { group: "strategy".into(), key: "volume_ratio_min".into(), value: json!(0.75), reason: "x".into() },
        ParamUpdate { group: "strategy".into(), key: "entry_threshold".into(), value: json!(0.42), reason: "x".into() },
    ];
    assert!(validate_candidate(&active, &four).is_err());
}

#[test]
fn chaos_parent_can_lower_atr_min_within_hard_bound() {
    let active = json!({"timeframe":"1h","atr_pct_min":1.0,"atr_pct_max":8,"volume_ratio_min":1.0,"entry_threshold":0.5});
    let patch = vec![ParamUpdate {
        group: "strategy".into(),
        key: "atr_pct_min".into(),
        value: json!(0.75),
        reason: "loosen".into(),
    }];
    let next = validate_candidate(&active, &patch).unwrap();
    assert_eq!(next["atr_pct_min"], json!(0.75));
}

#[test]
fn candidate_step_over_25_percent_is_rejected() {
    let active = json!({"timeframe":"1h","atr_pct_min":0.6,"atr_pct_max":10,"volume_ratio_min":0.8,"entry_threshold":0.45});
    let patch = vec![ParamUpdate {
        group: "strategy".into(),
        key: "entry_threshold".into(),
        value: json!(0.60),
        reason: "too far".into(),
    }];
    assert!(validate_candidate(&active, &patch).is_err());
}
```

- [ ] **步骤 2：运行测试确认失败**

```bash
cargo test --manifest-path rust/nanoquant/Cargo.toml candidate_validation -- --nocapture
```

预期：编译失败或断言失败。

- [ ] **步骤 3：实现校验与 candidate 写入**

```rust
use crate::ai_client::ParamUpdate;

const MAX_KEYS_PER_CANDIDATE: usize = 3;
const MAX_STEP: f64 = 0.25;

pub(crate) fn validate_candidate(active: &Value, patch: &[ParamUpdate]) -> Result<Value, String> {
    if patch.len() > MAX_KEYS_PER_CANDIDATE {
        return Err(format!("一次最多修改 {MAX_KEYS_PER_CANDIDATE} 个参数"));
    }
    let mut next = active.clone();
    let Some(object) = next.as_object_mut() else {
        return Err("参数集不是对象".to_string());
    };
    for update in patch {
        if update.group != "strategy" {
            return Err(format!("只允许修改 strategy，收到 {}", update.group));
        }
        let meta = crate::param_registry::find("strategy", &update.key)
            .ok_or_else(|| format!("未知参数 strategy.{}", update.key))?;
        if !meta.ai_writable {
            return Err(format!("strategy.{} 不允许 AI 修改", update.key));
        }
        let old = object.get(&update.key).and_then(Value::as_f64);
        let new = update.value.as_f64().ok_or_else(|| format!("strategy.{} 必须是数字", update.key))?;
        if !new.is_finite() {
            return Err(format!("strategy.{} 不是有限数值", update.key));
        }
        if let Some(old) = old {
            if old != 0.0 && ((new - old) / old).abs() > MAX_STEP {
                return Err(format!("strategy.{} 单次变化超过 25%", update.key));
            }
        }
        if let Some((low, high)) = crate::param_registry::ai_bounds("strategy", &update.key) {
            if !(low..=high).contains(&new) {
                return Err(format!("strategy.{} 超出 AI 边界 [{low}, {high}]", update.key));
            }
        }
        let coerced = crate::param_registry::coerce(&meta, &update.value)?;
        object.insert(update.key.clone(), coerced);
    }
    crate::settings_api::validate_group("strategy", &next)?;
    Ok(next)
}

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

pub(crate) fn row_to_set(row: &rusqlite::Row<'_>) -> rusqlite::Result<ParameterSet> {
    Ok(ParameterSet {
        id: row.get(0)?,
        name: row.get(1)?,
        regime: row.get(2)?,
        timeframe: row.get(3)?,
        version: row.get(4)?,
        params: serde_json::from_str(&row.get::<_, String>(5)?).unwrap_or(Value::Null),
        status: row.get(6)?,
        source: row.get(7)?,
        evidence: serde_json::from_str(&row.get::<_, String>(8)?).unwrap_or(Value::Null),
        parent_id: row.get(9)?,
        created_at: row.get(10)?,
        activated_at: row.get(11)?,
    })
}

pub(crate) fn active_for_regime(conn: &Connection, regime: &str) -> Option<ParameterSet> {
    conn.query_row(
        "SELECT id,name,regime,timeframe,version,params,status,source,evidence,parent_id,created_at,activated_at \
         FROM parameter_sets WHERE regime=?1 AND status='active' ORDER BY id DESC LIMIT 1",
        [regime],
        row_to_set,
    )
    .ok()
}

pub(crate) fn load_set(conn: &Connection, id: i64) -> Result<ParameterSet, String> {
    conn.query_row(
        "SELECT id,name,regime,timeframe,version,params,status,source,evidence,parent_id,created_at,activated_at \
         FROM parameter_sets WHERE id=?1",
        [id],
        row_to_set,
    )
    .map_err(|e| e.to_string())
}

pub(crate) fn insert_candidate(
    conn: &Connection,
    name: &str,
    regime: &str,
    timeframe: &str,
    params: &Value,
    parent_id: Option<i64>,
) -> Result<i64, String> {
    insert_set(conn, name, regime, timeframe, params, "candidate", "ai", "{}", parent_id)
}
```

把 `settings_api::validate` 改为 `pub(crate) fn validate_group`（或在 `settings_api.rs` 增加 `pub(crate) fn validate_group` 包装 `validate`），因为 `validate` 目前是私有：

```rust
pub(crate) fn validate_group(group: &str, value: &Value) -> Result<(), String> {
    validate(group, value)
}
```

- [ ] **步骤 4：运行测试确认通过**

```bash
cargo test --manifest-path rust/nanoquant/Cargo.toml candidate_validation_rejects_risk_and_three_key_limit -- --nocapture
cargo test --manifest-path rust/nanoquant/Cargo.toml chaos_parent_can_lower_atr_min_within_hard_bound -- --nocapture
cargo test --manifest-path rust/nanoquant/Cargo.toml candidate_step_over_25_percent_is_rejected -- --nocapture
```

预期：PASS。

- [ ] **步骤 5：Commit**

```bash
git add rust/nanoquant/src/parameter_sets.rs rust/nanoquant/src/settings_api.rs
git commit -m "feat(参数集): 增加候选校验与注册表写入"
```

---

## 任务 4：只读 market_regime 与 symbol_regime

**文件：**
- 创建：`rust/nanoquant/src/regime.rs`
- 修改：`rust/nanoquant/src/trade_policy.rs:47-90`
- 修改：`rust/nanoquant/src/runtime.rs`（周期记录 regime）
- 修改：`rust/nanoquant/src/main.rs`（`mod regime;`）
- 测试：`regime.rs`、`trade_policy.rs`

- [ ] **步骤 1：编写失败测试**

```rust
#[test]
fn classify_boundaries_are_stable() {
    assert_eq!(classify(25.0, 0.15, 1.0, 1.0), Regime::Trend);
    assert_eq!(classify(24.9, 0.15, 1.0, 1.0), Regime::Range);
    assert_eq!(classify(10.0, 0.0, 2.0, 1.0), Regime::Chaos);
    assert_eq!(classify(10.0, 0.0, 1.0, 2.5), Regime::Chaos);
    assert_eq!(classify(10.0, 0.0, 1.9, 2.4), Regime::Range);
}

#[test]
fn majority_requires_six_and_ties_to_range() {
    use Regime::*;
    assert_eq!(market_regime(&[Trend, Trend, Trend, Range, Range, Range]), Some(Range));
    assert_eq!(market_regime(&[Trend, Trend, Trend, Trend, Range, Range]), Some(Trend));
    assert_eq!(market_regime(&[Trend; 5]), None);
}

#[test]
fn series_aligns_to_close_time() {
    let candles = crate::providers::candle_metrics_for_tests(80);
    let series = regime_series(&candles, 14);
    assert_eq!(series.len(), candles.len());
    assert!(series.iter().all(|value| value.is_some()));
}
```

最后一条测试使用测试辅助函数；实现时在 `providers` 的测试模块或 `regime.rs` 里构造 80 根 K 线。

- [ ] **步骤 2：运行测试确认失败**

```bash
cargo test --manifest-path rust/nanoquant/Cargo.toml classify_boundaries_are_stable -- --nocapture
```

预期：编译失败。

- [ ] **步骤 3：实现 regime 模块并让 core 复用它**

在 `regime.rs`：

```rust
use crate::providers::CandleSnapshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Regime {
    Trend,
    Range,
    Chaos,
}

impl Regime {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Regime::Trend => "trend",
            Regime::Range => "range",
            Regime::Chaos => "chaos",
        }
    }
    pub(crate) fn from_str(value: &str) -> Option<Self> {
        match value {
            "trend" => Some(Regime::Trend),
            "range" => Some(Regime::Range),
            "chaos" => Some(Regime::Chaos),
            _ => None,
        }
    }
}

pub(crate) fn classify(adx: f64, gap_pct: f64, atr_pct: f64, volume_ratio: f64) -> Regime {
    if adx >= 25.0 && gap_pct.abs() >= 0.15 {
        Regime::Trend
    } else if atr_pct >= 2.0 || volume_ratio >= 2.5 {
        Regime::Chaos
    } else {
        Regime::Range
    }
}

pub(crate) fn market_regime(regimes: &[Regime]) -> Option<Regime> {
    if regimes.len() < 6 {
        return None;
    }
    let mut trend = 0usize;
    let mut range = 0usize;
    let mut chaos = 0usize;
    for regime in regimes {
        match regime {
            Regime::Trend => trend += 1,
            Regime::Range => range += 1,
            Regime::Chaos => chaos += 1,
        }
    }
    let max = trend.max(range).max(chaos);
    let winners = [trend, range, chaos].iter().filter(|count| **count == max).count();
    if winners > 1 {
        return Some(Regime::Range);
    }
    if trend == max { Some(Regime::Trend) }
    else if chaos == max { Some(Regime::Chaos) }
    else { Some(Regime::Range) }
}

pub(crate) fn regime_series(candles: &[CandleSnapshot], atr_period: usize) -> Vec<Option<Regime>> {
    let mut out = Vec::with_capacity(candles.len());
    for index in 0..candles.len() {
        let end = (index + 1).max(30);
        if end > candles.len() {
            out.push(None);
            continue;
        }
        let prefix = &candles[..end];
        let mark = prefix.last().map(|c| c.close).unwrap_or_default();
        let Some(atr) = crate::providers::atr(prefix, atr_period) else {
            out.push(None);
            continue;
        };
        let Some(metrics) = crate::providers::candle_metrics(prefix, atr, mark) else {
            out.push(None);
            continue;
        };
        out.push(Some(classify(metrics.adx, metrics.gap_pct, metrics.atr_pct, metrics.volume_ratio)));
    }
    out
}
```

具体指标函数名以 `crate::providers` 现有导出为准；若没有单独导出，复用 `trade_policy::core` 的指标计算并提取 `regime_indicators(candles) -> Option<(adx,gap,atr_pct,volume_ratio)>`，避免两套实现。实现时把 `core` 里的指标计算移到 `pub(crate) fn indicators(...)`，`core` 和 `regime_series` 共用。

在 `trade_policy.rs` 把内联 regime 字符串判断替换为：

```rust
let regime = crate::regime::classify(adx_value, gap_pct, atr_pct, volume_ratio);
```

`Core.regime` 类型改为 `crate::regime::Regime`，所有使用处用 `.as_str()`。

在 `runtime.rs` 的周期内，用所有启用的标的计算：

```rust
let regimes: Vec<crate::regime::Regime> = snapshot
    .markets
    .iter()
    .filter_map(|market| {
        let metric = metrics.get(market.symbol.as_str())?;
        let features = crate::trade_policy::MarketFeatures {
            funding_rate: market.funding_rate,
            open_interest_change_pct: market.open_interest_change_pct,
            relative_strength: 0.0,
            relative_strength_available: false,
        };
        crate::trade_policy::regime_for(&market.candles, market.mark_price, metric.atr, features)
    })
    .collect();
if let Some(regime) = crate::regime::market_regime(&regimes) {
    upsert_state(&tx, "market_regime", regime.as_str())?;
}
```

`trade_policy::regime_for` 是新增的公开包装：

```rust
pub(crate) fn regime_for(
    candles: &[crate::providers::CandleSnapshot],
    mark: f64,
    atr: f64,
    features: MarketFeatures,
) -> Option<crate::regime::Regime> {
    core(candles, mark, atr, features).map(|core| core.regime)
}
```

- [ ] **步骤 4：运行测试确认通过**

```bash
cargo test --manifest-path rust/nanoquant/Cargo.toml classify_boundaries_are_stable -- --nocapture
cargo test --manifest-path rust/nanoquant/Cargo.toml majority_requires_six_and_ties_to_range -- --nocapture
cargo test --manifest-path rust/nanoquant/Cargo.toml series_aligns_to_close_time -- --nocapture
```

预期：PASS。A 期只在 `engine_state` 记录，不改变参数。

- [ ] **步骤 5：Commit**

```bash
git add rust/nanoquant/src/regime.rs rust/nanoquant/src/trade_policy.rs rust/nanoquant/src/runtime.rs rust/nanoquant/src/main.rs
git commit -m "feat(regime): 增加只读 market regime 与 regime 序列"
```

---

## 任务 5：入口复核彻底移除调参与风险参数上下文

**文件：**
- 修改：`rust/nanoquant/src/ai_review.rs:211-233, 297-317`
- 修改：`rust/nanoquant/src/param_registry.rs` 测试（任务 1 已改）
- 测试：`ai_review.rs`

- [ ] **步骤 1：编写失败测试**

在 `ai_review.rs` 的测试模块添加：

```rust
#[tokio::test]
async fn review_entry_ignores_ai_param_updates() {
    let (db, _transport) = fixture_with_param_transport();
    let before: i64 = db
        .conn
        .lock()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM parameter_changes", [], |r| r.get(0))
        .unwrap();
    let state = AiState::with_transport(
        db.clone(),
        MASTER_KEY.as_bytes().to_vec(),
        std::sync::Arc::new(FakeTransport::new(vec![(
            200,
            r#"{"a":"H","c":0.9,"p":[{"g":"risk","k":"max_leverage","v":50,"w":"x"},{"g":"strategy","k":"atr_pct_min","v":0.8,"w":"y"}]}"#,
        )])),
    );
    let _ = review_entry(state, proposal("long")).await;
    let after: i64 = db
        .conn
        .lock()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM parameter_changes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(before, after);
}
```

如果现有测试夹具不叫 `fixture_with_param_transport`，复用 `fixture`/`FakeTransport` 的现有构造；关键是断言 `parameter_changes` 不增长。

- [ ] **步骤 2：运行测试确认失败**

```bash
cargo test --manifest-path rust/nanoquant/Cargo.toml review_entry_ignores_ai_param_updates -- --nocapture
```

预期：FAIL，`parameter_changes` 增加。

- [ ] **步骤 3：删除入口调参调用与参数上下文**

在 `review_entry`：

```rust
let parameters = Value::Null; // 删除这一段，不要把参数目录交给开仓复核
```

删除 `let parameters = review_parameters(&conn, &cfg);` 和 `"parameters": parameters,`。

删除：

```rust
apply_decision_params(&state, &mut decision, outcome_log_id);
```

保留 `Decision.params` 解析但忽略；可把 `let mut decision` 改回 `let decision`。

删除 `apply_decision_params`、`tuning_enabled`、`tuning_cooldown_active` 及其测试；`tuning.rs` 重新实现所需的最小开关/间隔逻辑。A 期验收要求入口路径不再调用这些函数。

- [ ] **步骤 4：运行测试确认通过**

```bash
cargo test --manifest-path rust/nanoquant/Cargo.toml review_entry_ignores_ai_param_updates -- --nocapture
cargo test --manifest-path rust/nanoquant/Cargo.toml ai_review::tests:: -- --nocapture
```

预期：PASS；原有 AI 复核测试全绿。

- [ ] **步骤 5：Commit**

```bash
git add rust/nanoquant/src/ai_review.rs
git commit -m "refactor(ai): 开仓复核不再应用参数更新"
```

---

## 任务 6：独立调参任务

**文件：**
- 创建：`rust/nanoquant/src/tuning.rs`
- 修改：`rust/nanoquant/src/main.rs:288-308`（启动任务）
- 修改：`rust/nanoquant/src/ai_analysis.rs`（新增调参 prompt 构造函数）
- 测试：`tuning.rs`

- [ ] **步骤 1：编写失败测试**

```rust
#[tokio::test]
async fn tuning_task_writes_candidate_not_strategy() {
    let (db, transport) = tuning_fixture();
    let state = AiState::with_transport(
        db.clone(),
        MASTER_KEY.as_bytes().to_vec(),
        std::sync::Arc::new(transport),
    );
    run_once(&state).await.unwrap();
    let strategy: String = db
        .conn
        .lock()
        .unwrap()
        .query_row("SELECT value FROM app_settings WHERE key='strategy'", [], |r| r.get(0))
        .unwrap();
    assert!(strategy.contains("\"atr_pct_min\":1.5"));
    let (status, timeframe, params): (String, String, String) = db
        .conn
        .lock()
        .unwrap()
        .query_row(
            "SELECT status,timeframe,params FROM parameter_sets WHERE source='ai' ORDER BY id DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(status, "candidate");
    assert_eq!(timeframe, "1h");
    assert!(params.contains("\"atr_pct_min\":0.7"));
}
```

`tuning_fixture` 使用 1h strategy、当前 range-bootstrap active、FakeTransport 返回：

```json
{"a":"H","c":0.6,"p":[{"g":"strategy","k":"atr_pct_min","v":0.7,"w":"降低低波动噪声"}]}
```

- [ ] **步骤 2：运行测试确认失败**

```bash
cargo test --manifest-path rust/nanoquant/Cargo.toml tuning_task_writes_candidate_not_strategy -- --nocapture
```

预期：编译失败，缺少 `run_once`。

- [ ] **步骤 3：实现 tuning 模块**

`tuning.rs` 的核心：

```rust
pub(crate) fn spawn_tuning_task(
    state: crate::ai_providers::AiState,
    mut stop: tokio::sync::watch::Receiver<bool>,
) -> impl std::future::Future<Output = ()> {
    async move {
        tokio::time::sleep(std::time::Duration::from_secs(300)).await;
        loop {
            if *stop.borrow() {
                break;
            }
            let interval = tuning_interval_sec(&state).unwrap_or(86400).max(60);
            if due(&state, interval).unwrap_or(false) {
                if let Err(error) = run_once(&state).await {
                    tracing::warn!(%error, "独立调参任务失败");
                    record_event(&state, "warn", "parameter_set.tuning_failed", &error);
                }
            }
            tokio::select! {
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() { break; }
                }
                _ = tokio::time::sleep(std::time::Duration::from_secs(60)) => {}
            }
        }
    }
}

pub(crate) async fn run_once(state: &crate::ai_providers::AiState) -> Result<(), String> {
    let conn = state.db.conn.lock().map_err(|_| "数据库锁不可用".to_string())?;
    let ai = crate::settings_api::defaults_of("ai"); // 实际用 state.db.setting("ai")
    if !setting_bool(&ai, "allow_param_tuning", true) {
        return Ok(());
    }
    let current_regime = engine_state(&conn, "market_regime").unwrap_or_else(|| "range".to_string());
    let active = crate::parameter_sets::active_for_regime(&conn, &current_regime)
        .or_else(|| crate::parameter_sets::active_for_regime(&conn, "range"))
        .ok_or_else(|| "当前 regime 没有 active 父集".to_string())?;
    let interval = tuning_interval_sec(state).unwrap_or(86400);
    let last = engine_state(&conn, "parameter_set.last_tuning_ts")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0);
    if crate::now() - last < interval {
        return Ok(());
    }
    drop(conn);

    let stats = tuning_stats(&state).await?;
    let messages = crate::ai_analysis::tuning_messages(&active.params, &stats, &current_regime);
    let (cfg, chain) = crate::ai_providers::provider_chain(state);
    if !cfg.enabled || chain.is_empty() {
        return Ok(());
    }
    let mut call = crate::ai_client::ChatCall::new(&messages, "parameter_tuning");
    call.temperature = Some(0.2);
    let outcome = crate::ai_client::chat_with_fallback(
        state.transport.as_ref(), &state.db, &chain, call, &cfg,
    ).await;
    if !outcome.ok {
        return Err(format!("调参 AI 失败: {}", outcome.error));
    }
    let decision = crate::ai_client::parse_decision(&outcome.text, None)?;
    let next = crate::parameter_sets::validate_candidate(&active.params, &decision.params)?;
    let conn = state.db.conn.lock().map_err(|_| "数据库锁不可用".to_string())?;
    let id = crate::parameter_sets::insert_candidate(
        &conn,
        &format!("{}-ai-{}", current_regime, crate::now()),
        &current_regime,
        active.timeframe.as_str(),
        &next,
        Some(active.id),
    )?;
    crate::runtime::upsert_state_public(&conn, "parameter_set.last_tuning_ts", &crate::now().to_string())?;
    crate::runtime::record_event_public(&conn, "info", "parameter_set", "生成参数集候选", json!({"set_id":id}))?;
    Ok(())
}
```

在 `runtime.rs` 增加两个包装：

```rust
pub(crate) fn upsert_state_public(
    conn: &rusqlite::Connection,
    key: &str,
    value: &str,
) -> Result<(), String> {
    upsert_state(conn, key, value)
}

pub(crate) fn record_event_public(
    conn: &rusqlite::Connection,
    level: &str,
    category: &str,
    message: &str,
    payload: Value,
) -> Result<(), String> {
    conn.execute(
        "INSERT INTO engine_events(ts,level,category,message,payload) VALUES(?1,?2,?3,?4,?5)",
        (crate::now(), level, category, message, payload.to_string()),
    )
    .map(|_| ())
    .map_err(|e| e.to_string())
}
```

`tuning.rs` 内补齐以下辅助函数（签名固定，按现有 `Db::setting` 与 SQL 实现）：

```rust
fn engine_state(conn: &rusqlite::Connection, key: &str) -> Option<String>;
fn setting_bool(value: &Value, key: &str, default: bool) -> bool;
fn tuning_interval_sec(state: &crate::ai_providers::AiState) -> Option<i64>;
fn due(state: &crate::ai_providers::AiState, interval: i64) -> Result<bool, String>;
async fn tuning_stats(state: &crate::ai_providers::AiState) -> Result<Value, String>;
```

`tuning_messages` 在 `ai_analysis.rs` 用 `ChatMessage::new("system", ...)` 与 `ChatMessage::new("user", serde_json::to_string(&context)?)` 构造；context 包含 active params、stats、regime、硬边界、最多 3 个键的要求。

在 `main.rs` 启动：

```rust
if let Some(ai) = ai_state.clone() {
    tokio::spawn(crate::tuning::spawn_tuning_task(ai, stop_tx.subscribe()));
}
```

放在 `let (stop_tx, stop_rx) = runtime::stop_channel();` 之后、引擎启动之前。

- [ ] **步骤 4：运行测试确认通过**

```bash
cargo test --manifest-path rust/nanoquant/Cargo.toml tuning_task_writes_candidate_not_strategy -- --nocapture
```

预期：PASS；`app_settings.strategy` 未变。

- [ ] **步骤 5：Commit**

```bash
git add rust/nanoquant/src/tuning.rs rust/nanoquant/src/ai_analysis.rs rust/nanoquant/src/main.rs
git commit -m "feat(参数集): 独立 24h 调参任务只生成候选"
```

---

## 任务 7：A 期验收与全量回归

- [ ] **步骤 1：运行后端门禁**

```bash
cargo fmt --all --manifest-path rust/nanoquant/Cargo.toml --check
cargo clippy --all-targets --manifest-path rust/nanoquant/Cargo.toml -- -D warnings
cargo test --release --locked --manifest-path rust/nanoquant/Cargo.toml
```

预期：全部通过。

- [ ] **步骤 2：运行前端门禁**

```bash
cd frontend
npm test
npm run typecheck
npm run build
```

预期：全部通过。

- [ ] **步骤 3：A 期验收断言**

- 入口复核测试断言 `parameter_changes` 不增长。
- 调参任务测试断言只写 candidate，`strategy` 不变。
- `parameter_set` 与 `risk` 组所有键 `ai_writable=false`。
- `market_regime` 只写 `engine_state`，不写 `strategy`。

- [ ] **步骤 4：Commit**

```bash
git add -A
git commit -m "test(参数集): 完成 A 期验收"
```

---

## 自检

- 规格覆盖：§5 数据模型、§6 A 期、§9 权限边界、§11 A 期上线、§12 B1/B2/B4 测试均在任务 1-7 中。
- 占位符：没有“稍后补充”步骤；所有关键函数都有签名和可执行测试。
- 类型一致性：`Regime`、`ParameterSet`、`ParamUpdate`、`validate_candidate`、`insert_candidate` 在后续 B/C 计划中沿用。