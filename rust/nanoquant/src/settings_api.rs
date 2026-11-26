//! 配置组读写。16 个组的默认值与取值范围对齐 `default_settings.json` 与 `param_registry`。
//!
//! 未知组返回 404，不静默建组：前端拼错组名时若返回空对象，
//! 用户会以为配置丢了再存一次，把别的组覆盖掉。
//! 审计只记真正变化的字段，字段名含 secret/password/token/api_key/_enc 的值一律写成 "***"。

use std::collections::BTreeMap;
use std::sync::OnceLock;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::db::Db;

pub fn routes() -> Router<Db> {
    Router::new()
        .route("/settings", get(read_all))
        .route("/settings/audit", get(audit))
        .route("/settings/catalog", get(catalog))
        .route("/settings/parameter-changes", get(parameter_changes))
        .route(
            "/settings/parameter-changes/{id}/rollback",
            post(rollback_parameter_change),
        )
        .route("/settings/{group}", get(read_group).put(write_group))
        .route("/settings/{group}/reset", post(reset_group))
        .route("/system/system-settings", get(system_settings))
}

pub(crate) const GROUPS: [&str; 15] = [
    "engine",
    "risk",
    "strategy",
    "ai_tuning",
    "rl_exec",
    "symbol_scan",
    "ai",
    "data",
    "news",
    "evolution",
    "backup",
    "security",
    "system",
    "public",
    "journal",
];

async fn read_all(State(db): State<Db>) -> Json<Value> {
    let conn = db.lock();
    let mut groups = Map::new();
    let mut defaults = Map::new();
    for name in GROUPS {
        groups.insert(name.to_string(), group_value(&conn, name));
        defaults.insert(name.to_string(), defaults_of(name));
    }
    json!({ "groups": groups, "defaults": defaults }).into()
}

async fn catalog() -> Json<Value> {
    Json(crate::param_registry::catalog())
}

#[derive(Deserialize)]
struct LimitQuery {
    #[serde(default)]
    limit: Option<i64>,
}

async fn parameter_changes(State(db): State<Db>, Query(query): Query<LimitQuery>) -> Json<Value> {
    let conn = db.lock();
    let _ = crate::schema::ensure_parameter_changes(&conn);
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let mut stmt = conn
        .prepare(
            "SELECT id,ts,group_name,key,old_value,new_value,reason,decision_id,mode,source \
             FROM parameter_changes ORDER BY ts DESC,id DESC LIMIT ?1",
        )
        .expect("prepare parameter_changes");
    let rows = stmt
        .query_map([limit], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, String>(9)?,
            ))
        })
        .expect("query parameter_changes")
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
    let changes = rows
        .iter()
        .map(|row| (row.0, row.1, row.8.clone()))
        .collect::<Vec<_>>();
    let effects = effects_for_changes(&conn, &changes);
    let items = rows
        .iter()
        .map(|row| {
            json!({
                "id": row.0,
                "ts": row.1,
                "group": row.2,
                "key": row.3,
                "old_value": row.4,
                "new_value": row.5,
                "reason": row.6,
                "decision_id": row.7,
                "mode": row.8,
                "source": row.9,
                "effect": effects.get(&row.0).cloned().unwrap_or_else(|| json!({"trades": 0, "pnl": 0.0, "note": EFFECT_NOTE})),
            })
        })
        .collect::<Vec<_>>();
    json!({ "items": items }).into()
}

/// 调参效果只是时间窗参考值，不构成因果结论。
const EFFECT_NOTE: &str = "仅供参考，不控制行情与其他变更";

/// 一次查询算出多条调参记录各自之后的首批成交，避免逐行查库。
pub(crate) fn effects_for_changes(
    conn: &rusqlite::Connection,
    changes: &[(i64, i64, String)],
) -> std::collections::HashMap<i64, Value> {
    let mut effects = std::collections::HashMap::new();
    let Some(earliest) = changes.iter().map(|(_, ts, _)| *ts).min() else {
        return effects;
    };
    let Ok(mut statement) = conn.prepare(
        "SELECT mode, closed_at, pnl FROM trades WHERE closed_at >= ?1 ORDER BY closed_at ASC",
    ) else {
        return effects;
    };
    let trades = statement
        .query_map([earliest], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                row.get::<_, i64>(1)?,
                row.get::<_, f64>(2)?,
            ))
        })
        .map(|rows| rows.filter_map(Result::ok).collect::<Vec<_>>())
        .unwrap_or_default();
    for (id, ts, mode) in changes {
        let mode = normalized_mode(mode);
        let mut count = 0;
        let mut pnl = 0.0_f64;
        for (trade_mode, closed_at, value) in &trades {
            if count == 5 {
                break;
            }
            if *closed_at < *ts || normalized_mode(trade_mode) != mode {
                continue;
            }
            count += 1;
            pnl += value;
        }
        effects.insert(
            *id,
            json!({
                "trades": count,
                "pnl": (pnl * 10_000.0).round() / 10_000.0,
                "note": EFFECT_NOTE,
            }),
        );
    }
    effects
}

fn normalized_mode(mode: &str) -> String {
    match mode {
        "paper" | "" => "demo".to_string(),
        other => other.to_string(),
    }
}

async fn rollback_parameter_change(
    State(db): State<Db>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = db.lock();
    rollback_param_change(&conn, id)
        .map_err(|detail| (StatusCode::BAD_REQUEST, json!({ "detail": detail }).into()))?;
    Ok(json!({ "ok": true }).into())
}

async fn read_group(
    State(db): State<Db>,
    Path(group): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !GROUPS.contains(&group.as_str()) {
        return Err(unknown(&group));
    }
    let conn = db.lock();
    Ok(json!({ "group": group, "data": group_value(&conn, &group) }).into())
}

#[derive(Deserialize)]
struct PatchBody {
    #[serde(default)]
    patch: Map<String, Value>,
}

async fn write_group(
    State(db): State<Db>,
    Path(group): Path<String>,
    Json(body): Json<PatchBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !GROUPS.contains(&group.as_str()) {
        return Err(unknown(&group));
    }
    let conn = db.lock();
    if group == "engine" {
        if let Some(mode) = body.patch.get("mode").and_then(|v| v.as_str()) {
            let current = group_value(&conn, "engine")
                .get("mode")
                .and_then(|v| v.as_str())
                .unwrap_or("demo")
                .to_string();
            if normalize_mode(mode) != normalize_mode(&current) {
                return Err((StatusCode::BAD_REQUEST, json!({ "detail": "切换交易环境请使用「交易环境」开关（实盘需要输入确认短语 LIVE）" }).into()));
            }
        }
    }
    let before = group_value(&conn, &group);
    let mut candidate = defaults_of(&group);
    deep_merge(&mut candidate, &stored(&conn, &group));
    deep_merge(&mut candidate, &Value::Object(body.patch.clone()));
    drop_removed_keys(&group, &mut candidate);
    if group == "ai_tuning" {
        if let Some(timeframes) = candidate.get_mut("auto_apply_timeframes") {
            if let Some(text) = timeframes.as_str() {
                *timeframes = json!(text.trim().to_ascii_lowercase());
            }
        }
    }
    if let Err(detail) = validate(&group, &candidate) {
        return Err((StatusCode::BAD_REQUEST, json!({ "detail": detail }).into()));
    }
    let payload = serde_json::to_string(&candidate).unwrap_or_else(|_| "{}".to_string());
    conn.execute(
        "INSERT INTO app_settings(key, value, updated_at) VALUES(?1, ?2, ?3) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        (&group, &payload, crate::now()),
    )
    .map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "detail": error.to_string() }).into(),
        )
    })?;
    let changes = diff(
        &group,
        before.as_object().unwrap_or(&Map::new()),
        candidate.as_object().unwrap_or(&Map::new()),
    );
    if !changes.is_empty() {
        let blob = serde_json::to_string(&changes).unwrap_or_else(|_| "{}".to_string());
        conn.execute(
            "INSERT INTO settings_audit(ts, user, group_name, changes, ip) VALUES(?1, '', ?2, ?3, '')",
            (crate::now(), &group, &blob),
        )
        .ok();
    }
    Ok(json!({ "ok": true, "group": group, "data": candidate, "changed": changes.keys().cloned().collect::<Vec<_>>() }).into())
}

async fn reset_group(
    State(db): State<Db>,
    Path(group): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !GROUPS.contains(&group.as_str()) {
        return Err(unknown(&group));
    }
    let conn = db.lock();
    let before = group_value(&conn, &group);
    let mut candidate = defaults_of(&group);
    if group == "security" {
        let current = stored(&conn, "security");
        if let Some(target) = candidate.as_object_mut() {
            for key in [
                "username",
                "password_hash",
                "token_version",
                "password_is_default",
            ] {
                if let Some(value) = current.get(key) {
                    target.insert(key.to_string(), value.clone());
                }
            }
        }
    }
    if let Err(detail) = validate(&group, &candidate) {
        return Err((StatusCode::BAD_REQUEST, json!({ "detail": detail }).into()));
    }
    let payload = serde_json::to_string(&candidate).unwrap_or_else(|_| "{}".to_string());
    conn.execute(
        "INSERT INTO app_settings(key, value, updated_at) VALUES(?1, ?2, ?3) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        (&group, &payload, crate::now()),
    )
    .map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "detail": error.to_string() }).into(),
        )
    })?;
    let changes = diff(
        &group,
        before.as_object().unwrap_or(&Map::new()),
        candidate.as_object().unwrap_or(&Map::new()),
    );
    if !changes.is_empty() {
        let blob = serde_json::to_string(&changes).unwrap_or_else(|_| "{}".to_string());
        conn.execute(
            "INSERT INTO settings_audit(ts, user, group_name, changes, ip) VALUES(?1, '', ?2, ?3, '')",
            (crate::now(), &group, &blob),
        )
        .ok();
    }
    Ok(json!({
        "ok": true,
        "group": group,
        "data": candidate,
        "changed": changes.keys().cloned().collect::<Vec<_>>(),
    })
    .into())
}

#[derive(Deserialize)]
struct AuditQuery {
    #[serde(default)]
    group: String,
    #[serde(default = "default_audit_limit")]
    limit: i64,
}

fn default_audit_limit() -> i64 {
    50
}

async fn audit(State(db): State<Db>, Query(q): Query<AuditQuery>) -> Json<Value> {
    let limit = q.limit.clamp(1, 200);
    let conn = db.lock();
    let mut stmt = conn
        .prepare(
            "SELECT id, ts, user, group_name, changes, ip FROM settings_audit \
             WHERE (?1 = '' OR group_name = ?1) ORDER BY ts DESC, id DESC LIMIT ?2",
        )
        .expect("prepare audit");
    let items = stmt
        .query_map((q.group.as_str(), limit), |r| {
            let changes: String = r.get::<_, String>(4).unwrap_or_default();
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "ts": r.get::<_, i64>(1)?,
                "user": r.get::<_, String>(2)?,
                "group": r.get::<_, String>(3)?,
                "changes": serde_json::from_str::<Value>(&changes).unwrap_or(json!({})),
                "ip": r.get::<_, String>(5)?,
            }))
        })
        .expect("query audit")
        .collect::<Result<Vec<_>, _>>()
        .expect("audit rows");
    json!({ "items": items }).into()
}

async fn system_settings(State(db): State<Db>) -> Json<Value> {
    let conn = db.lock();
    json!({ "data": group_value(&conn, "system") }).into()
}

fn unknown(group: &str) -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_FOUND,
        json!({ "detail": format!("未知配置组 {group}") }).into(),
    )
}

fn group_value(conn: &rusqlite::Connection, name: &str) -> Value {
    let mut merged = defaults_of(name);
    deep_merge(&mut merged, &migrate(name, &stored(conn, name)));
    merged
}

fn stored(conn: &rusqlite::Connection, name: &str) -> Value {
    conn.query_row(
        "SELECT value FROM app_settings WHERE key = ?1",
        [name],
        |r| r.get::<_, String>(0),
    )
    .ok()
    .and_then(|s| serde_json::from_str(&s).ok())
    .filter(|v: &Value| v.is_object())
    .unwrap_or(Value::Null)
}

fn migrate(name: &str, raw: &Value) -> Value {
    let mut out = raw.as_object().cloned().unwrap_or_default();
    match name {
        "engine" => {
            if out.get("mode").and_then(|v| v.as_str()) == Some("paper") {
                out.insert("mode".to_string(), json!("demo"));
            }
        }
        "strategy" => {
            if matches!(
                out.get("timeframe").and_then(|v| v.as_str()),
                Some("1m" | "3m" | "5m")
            ) {
                out.insert("timeframe".to_string(), json!("15m"));
            }
            out.insert("mode".to_string(), json!("fusion"));
            out.remove("max_hold_minutes");
            out.remove("max_hold_minutes_chop");
            out.remove("ai_review_flat");
            out.remove("ai_review_enabled");
            out.remove("ai_review_max_per_cycle");
            out.remove("leverage_min");
            out.remove("leverage_max");
            out.remove("htf_timeframe");
            out.remove("mr_window");
            out.remove("mr_z_entry");
            out.remove("mr_min_risk_reward");
            out.remove("mr_block_in_trend");
            out.remove("mr_trend_gap_pct");
            out.remove("weight_trend");
            out.remove("weight_momentum");
            out.remove("weight_volume");
            out.remove("weight_funding");
            out.remove("weight_oi");
            out.remove("adx_filter");
            out.remove("adx_min");
            out.remove("mr_exhaustion_confirm");
        }
        "risk" => {
            out.remove("htf_enabled");
            out.remove("htf_trend_pct");
        }
        "system" => {
            out.remove("timezone_offset");
            // 历史键清理：这些键随 legacy 内核退役后已无任何代码读取，
            // 但老库的 app_settings.system 里可能还留着，不剔除会原样透到参数页。
            out.remove("exec_mode");
            out.remove("exec_socket");
            out.remove("exec_sample_pct");
            out.remove("exec_timeout_ms");
            out.remove("exec_place_protection");
            out.remove("exec_ledger_authoritative");
        }
        "ai" => {
            out.remove("max_position_leverage");
        }
        _ => {}
    }
    Value::Object(out)
}

fn normalize_mode(mode: &str) -> &str {
    if mode == "live" {
        "live"
    } else {
        "demo"
    }
}

fn deep_merge(base: &mut Value, patch: &Value) {
    let Some(patch) = patch.as_object() else {
        return;
    };
    let Some(base) = base.as_object_mut() else {
        return;
    };
    for (key, value) in patch {
        if value.is_object() && base.get(key).is_some_and(|v| v.is_object()) {
            deep_merge(base.get_mut(key).unwrap(), value);
        } else {
            base.insert(key.clone(), value.clone());
        }
    }
}

/// 已下线的配置项：老客户端再次提交也不能把它写回库。
fn drop_removed_keys(group: &str, value: &mut Value) {
    if group == "risk" {
        if let Some(object) = value.as_object_mut() {
            object.remove("default_leverage");
        }
    }
}

/// 只记变化的字段。命中注册表敏感词的值写成 "***"，审计日志不能变成泄密源。
fn diff(
    group: &str,
    before: &Map<String, Value>,
    after: &Map<String, Value>,
) -> BTreeMap<String, Value> {
    let mut changes = BTreeMap::new();
    for (key, new) in after {
        let old = before.get(key).cloned().unwrap_or(Value::Null);
        if &old == new {
            continue;
        }
        changes.insert(
            key.clone(),
            json!({ "from": mask(group, key, &old), "to": mask(group, key, new) }),
        );
    }
    changes
}

fn mask(group: &str, key: &str, value: &Value) -> Value {
    if crate::param_registry::is_sensitive(group, key) {
        json!("***")
    } else {
        value.clone()
    }
}

static DEFAULT_SETTINGS: &str = include_str!("default_settings.json");
static DEFAULTS: OnceLock<Value> = OnceLock::new();

fn all_defaults() -> &'static Value {
    DEFAULTS.get_or_init(|| serde_json::from_str(DEFAULT_SETTINGS).unwrap_or_else(|_| json!({})))
}

pub(crate) fn defaults_of(name: &str) -> Value {
    all_defaults()
        .get(name)
        .cloned()
        .unwrap_or_else(|| json!({}))
}

fn validate(group: &str, value: &Value) -> Result<(), String> {
    // 字段缺失沿用默认值；字段存在却不是数字时必须拒绝：
    // 否则 "max_leverage":"3" 也能通过校验，运行期 as_f64 拿不到值会回落默认上限，
    // 风控参数被静默放宽，用户却以为已经收紧。
    // 范围来自注册表，避免这里和 AI 目录各维护一份边界。
    let Some(object) = value.as_object() else {
        return Ok(());
    };
    for (key, candidate) in object {
        let Some(meta) = crate::param_registry::find(group, key) else {
            continue;
        };
        if candidate.is_null() {
            if group == "ai_tuning" {
                return Err(format!("{key}: 不允许为 null"));
            }
            continue;
        }
        let actual = crate::param_registry::kind_of(candidate);
        if actual != meta.kind {
            return Err(format!("{key}: 类型应为 {}", meta.kind));
        }
        if group == "ai_tuning" && key == "auto_apply_timeframes" {
            let allowed = candidate.as_str().is_some_and(|text| {
                let text = text.trim();
                // 空串表示不自动切换；非空项仍只接受 1h。
                text.is_empty()
                    || text
                        .split(',')
                        .all(|part| part.trim().eq_ignore_ascii_case("1h"))
            });
            if !allowed {
                return Err(format!("{key}: v1 仅允许 1h 或空串"));
            }
        }
        let (Some(low), Some(high)) = (meta.min, meta.max) else {
            continue;
        };
        match candidate {
            Value::Number(number) => match number.as_f64() {
                Some(number) if number.is_finite() && (low..=high).contains(&number) => {}
                _ => return Err(format!("{key}: 超出取值范围")),
            },
            _ => return Err(format!("{key}: 必须是数字")),
        }
    }
    Ok(())
}

/// AI 调参候选校验复用同一套配置规则，避免另维护一份边界。
pub(crate) fn validate_group(group: &str, value: &Value) -> Result<(), String> {
    validate(group, value)
}

/// 供AI 调参物化时读取合并了默认值的完整配置快照。
pub(crate) fn group_value_public(conn: &rusqlite::Connection, name: &str) -> Value {
    group_value(conn, name)
}

/// 把原始配置归一化为完整值：补默认、剔除历史键并执行迁移。
///
/// AI 调参激活比较与物化都走这里，避免历史快照里的隐藏键（如 `weight_trend`）
/// 在每次切换时被重新写回 strategy。
pub(crate) fn normalized_group_value(group: &str, raw: &Value) -> Value {
    let mut merged = defaults_of(group);
    let migrated = migrate(group, raw);
    deep_merge(&mut merged, &migrated);
    merged
}

/// AI 调参激活/回滚时逐键记录到 `parameter_changes`，与人工/AI 调参共用审计表。
pub(crate) fn record_set_changes(
    conn: &rusqlite::Connection,
    before: &Value,
    after: &Value,
    regime: &str,
    set_id: i64,
    mode: &str,
    source: &str,
) -> Result<(), String> {
    let Some(after) = after.as_object() else {
        return Err("AI 调参 strategy 快照必须是对象".to_string());
    };
    let before = before.as_object().cloned().unwrap_or_default();
    let reason = if source == "ai_tuning_rollback" {
        "AI 调参回滚"
    } else {
        "AI 调参切换"
    };
    let now = crate::now();
    for (key, new) in after {
        let old = before.get(key).cloned().unwrap_or(Value::Null);
        if &old == new {
            continue;
        }
        conn.execute(
            "INSERT INTO parameter_changes(\
             ts, group_name, key, old_value, new_value, reason, decision_id, mode, source, set_id, regime) \
             VALUES(?1,'strategy',?2,?3,?4,?5,0,?6,?7,?8,?9)",
            rusqlite::params![
                now,
                key,
                serde_json::to_string(&old).unwrap_or_else(|_| "null".to_string()),
                serde_json::to_string(new).unwrap_or_default(),
                reason,
                mode,
                source,
                set_id,
                regime,
            ],
        )
        .map_err(|error| format!("写入AI 调参变更记录失败: {error}"))?;
    }
    Ok(())
}

pub(crate) fn rollback_param_change(conn: &rusqlite::Connection, id: i64) -> Result<(), String> {
    let _ = crate::schema::ensure_parameter_changes(conn);
    let (group, key, old_text, mode): (String, String, Option<String>, String) = conn
        .query_row(
            "SELECT group_name, key, old_value, mode FROM parameter_changes WHERE id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .map_err(|error| format!("调参记录不存在: {error}"))?;
    let mut candidate = group_value(conn, &group);
    let current;
    {
        let object = candidate
            .as_object_mut()
            .ok_or_else(|| format!("{group} 配置不是对象"))?;
        current = object.get(&key).cloned().unwrap_or(Value::Null);
        match old_text {
            Some(text) => {
                let value: Value = serde_json::from_str(&text)
                    .map_err(|error| format!("旧值无法解析: {error}"))?;
                object.insert(key.clone(), value);
            }
            None => {
                object.remove(&key);
            }
        }
    }
    validate(&group, &candidate)?;
    let payload = serde_json::to_string(&candidate).map_err(|error| error.to_string())?;
    conn.execute(
        "INSERT INTO app_settings(key, value, updated_at) VALUES(?1, ?2, ?3) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        (&group, &payload, crate::now()),
    )
    .map_err(|error| error.to_string())?;
    let restored = candidate.get(&key).cloned().unwrap_or(Value::Null);
    let _ = conn.execute(
        "INSERT INTO parameter_changes(ts, group_name, key, old_value, new_value, reason, decision_id, mode, source) \
         VALUES(?1, ?2, ?3, ?4, ?5, '回滚 AI 调参', 0, ?6, 'rollback')",
        (
            crate::now(),
            &group,
            &key,
            serde_json::to_string(&current).unwrap_or_default(),
            serde_json::to_string(&restored).unwrap_or_default(),
            &mode,
        ),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn fixture(path: &std::path::Path) -> Db {
        let _ = std::fs::remove_file(path);
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE app_settings(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER); \
             CREATE TABLE settings_audit(id INTEGER PRIMARY KEY, ts INTEGER, user TEXT, group_name TEXT, changes TEXT, ip TEXT); \
             INSERT INTO app_settings VALUES('risk','{\"max_leverage\":50}',0); \
             INSERT INTO app_settings VALUES('engine','{\"mode\":\"paper\"}',0); \
             INSERT INTO settings_audit VALUES(1,100,'admin','risk','{\"max_leverage\":{\"from\":20,\"to\":50}}',''); \
             INSERT INTO settings_audit VALUES(2,200,'admin','engine','{\"mode\":{\"from\":\"demo\",\"to\":\"live\"}}','');",
        )
        .unwrap();
        drop(conn);
        Db::open(path.to_str().unwrap()).unwrap()
    }

    async fn put_settings(app: Router, group: &str, patch: &str) -> (StatusCode, Value) {
        let response = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!("/api/v1/settings/{group}"))
                    .header("content-type", "application/json")
                    .body(Body::from(format!("{{\"patch\":{patch}}}")))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    fn test_conn() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::schema::migrate(&conn).unwrap();
        conn.execute(
            "INSERT INTO app_settings(key,value,updated_at) VALUES('strategy','{\"entry_threshold\":0.3}',0)",
            [],
        )
        .unwrap();
        conn
    }

    #[test]
    fn validate_accepts_every_registered_bound() {
        for &(group, key, min, max, _) in crate::param_registry::RANGES {
            let mut lo = Map::new();
            lo.insert(key.to_string(), json!(min));
            assert!(
                validate(group, &Value::Object(lo)).is_ok(),
                "{group}.{key} 下界"
            );
            let mut hi = Map::new();
            hi.insert(key.to_string(), json!(max));
            assert!(
                validate(group, &Value::Object(hi)).is_ok(),
                "{group}.{key} 上界"
            );
        }
    }

    #[test]
    fn ai_tuning_validation_rejects_invalid_values() {
        assert!(validate("ai_tuning", &defaults_of("ai_tuning")).is_ok());
        for (key, bad) in [
            ("enabled", json!("yes")),
            ("enabled", Value::Null),
            ("min_oos_trades", json!(-5)),
            ("auto_apply_timeframes", json!("4h")),
            ("auto_apply_timeframes", Value::Null),
        ] {
            let mut value = defaults_of("ai_tuning");
            value[key] = bad;
            assert!(validate("ai_tuning", &value).is_err(), "{key} 应被拒绝");
        }
    }

    #[tokio::test]
    async fn ai_tuning_validation_normalizes_and_rejects_null() {
        let path = std::env::temp_dir().join("nq-settings-parameter-set.db");
        let db = fixture(&path);
        let app = Router::new()
            .nest("/api/v1", routes())
            .with_state(db.clone());

        let (status, body) = put_settings(
            app.clone(),
            "ai_tuning",
            r#"{"auto_apply_timeframes":"1H"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["data"]["auto_apply_timeframes"], "1h");
        let stored: String = db
            .lock()
            .query_row(
                "SELECT value FROM app_settings WHERE key='ai_tuning'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&stored).unwrap()["auto_apply_timeframes"],
            "1h"
        );

        let (status, body) = put_settings(
            app.clone(),
            "ai_tuning",
            r#"{"auto_apply_timeframes":" 1h "}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["data"]["auto_apply_timeframes"], "1h");

        for patch in [
            r#"{"auto_apply_timeframes":"1h, 4h"}"#,
            r#"{"auto_apply_timeframes":null}"#,
            r#"{"enabled":null}"#,
        ] {
            let (status, body) = put_settings(app.clone(), "ai_tuning", patch).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{patch}: {body}");
        }

        let (status, body) =
            put_settings(app.clone(), "ai_tuning", r#"{"auto_apply_timeframes":""}"#).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["data"]["auto_apply_timeframes"], "");
        let stored: String = db
            .lock()
            .query_row(
                "SELECT value FROM app_settings WHERE key='ai_tuning'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&stored).unwrap()["auto_apply_timeframes"],
            ""
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn sensitive_mask_uses_the_same_registry() {
        assert_eq!(
            mask("system", "alert_webhook_url", &json!("https://x")),
            json!("***")
        );
        assert_eq!(mask("strategy", "ema_fast", &json!(9)), json!(9));
    }

    #[test]

    fn effects_are_computed_per_change_window() {
        let conn = test_conn();
        conn.execute_batch(
            "INSERT INTO trades(symbol,side,qty,entry_price,exit_price,leverage,notional,pnl,pnl_pct,roi_pct,fees,opened_at,closed_at,hold_seconds,exit_reason,ai_confidence,ai_reason,signal_score,slippage_bps,mode,reviewed,review_note) VALUES
               ('BTCUSDT','LONG',1,1,1,1,1,10,0,0,0,0,150,0,'tp',0,'',0,0,'demo',0,''),
               ('BTCUSDT','LONG',1,1,1,1,1,-5,0,0,0,0,250,0,'sl',0,'',0,0,'demo',0,''),
               ('BTCUSDT','LONG',1,1,1,1,1,7,0,0,0,0,350,0,'tp',0,'',0,0,'demo',0,''),
               ('BTCUSDT','LONG',1,1,1,1,1,99,0,0,0,0,150,0,'tp',0,'',0,0,'live',0,'');
             INSERT INTO parameter_changes(ts,group_name,key,old_value,new_value,reason,decision_id,mode,source)
               VALUES(100,'strategy','entry_threshold','0.3','0.35','a',1,'demo','ai'),
                     (200,'strategy','ema_fast','9','12','b',2,'demo','ai'),
                     (300,'strategy','ema_fast','12','8','c',3,'demo','ai');",
        )
        .unwrap();
        let changes = vec![
            (1, 100, "demo".to_string()),
            (2, 200, "demo".to_string()),
            (3, 300, "demo".to_string()),
        ];
        let effects = effects_for_changes(&conn, &changes);
        assert_eq!(effects[&1]["trades"], 3);
        assert_eq!(effects[&1]["pnl"], 12.0);
        assert_eq!(effects[&2]["trades"], 2);
        assert_eq!(effects[&3]["trades"], 1);
        assert_eq!(effects[&1]["note"], EFFECT_NOTE);
    }

    #[test]
    fn rollback_restores_the_previous_value() {
        let conn = test_conn();
        conn.execute(
            "UPDATE app_settings SET value=json_set(value,'$.entry_threshold',0.5) WHERE key='strategy'",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO parameter_changes(ts, group_name, key, old_value, new_value, reason, decision_id, mode, source) \
             VALUES(1,'strategy','entry_threshold','0.3','0.5','测试',1,'demo','manual')",
            [],
        )
        .unwrap();
        let id: i64 = conn
            .query_row(
                "SELECT id FROM parameter_changes ORDER BY id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        rollback_param_change(&conn, id).unwrap();
        let stored: String = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key='strategy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let parsed: Value = serde_json::from_str(&stored).unwrap();
        assert_eq!(parsed["entry_threshold"], 0.3);
    }

    #[tokio::test]
    async fn catalog_route_returns_hidden_and_writable_fields() {
        let path = std::env::temp_dir().join("nq-settings-catalog.db");
        let db = fixture(&path);
        let app = Router::new().nest("/api/v1", routes()).with_state(db);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/settings/catalog")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            body["groups"]["strategy"]["hidden"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == "ema_fast"),
            "{body}"
        );
        assert!(!body.to_string().contains("api_key"));
    }

    #[tokio::test]
    async fn unknown_group_is_not_found_and_audit_is_newest_first() {
        let path = std::env::temp_dir().join("nq-settings-read.db");
        let db = fixture(&path);
        let app = Router::new()
            .nest("/api/v1", routes())
            .with_state(db.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/settings/nope")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let app = Router::new().nest("/api/v1", routes()).with_state(db);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/settings/audit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["items"][0]["group"], "engine", "新的在前");
        assert_eq!(body["items"].as_array().unwrap().len(), 2);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn legacy_default_leverage_patch_is_dropped() {
        let path = std::env::temp_dir().join("nq-settings-drop-leverage.db");
        let db = fixture(&path);
        let app = Router::new()
            .nest("/api/v1", routes())
            .with_state(db.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/v1/settings/risk")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"patch":{"default_leverage":20,"min_leverage":2}}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            body["data"].get("default_leverage").is_none(),
            "已下线的默认杠杆不能被旧客户端写回"
        );
        assert_eq!(body["data"]["min_leverage"], 2);
        let stored: String = db
            .lock()
            .query_row("SELECT value FROM app_settings WHERE key='risk'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let risk: Value = serde_json::from_str(&stored).unwrap();
        assert!(risk.get("default_leverage").is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn patch_merges_defaults_and_masks_secrets_in_the_audit() {
        let path = std::env::temp_dir().join("nq-settings-write.db");
        let db = fixture(&path);
        let app = Router::new().nest("/api/v1", routes()).with_state(db);
        let response = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/v1/settings/risk")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"patch":{"max_leverage":80,"live_api_secret_enc":"super-secret"}}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["data"]["max_leverage"], 80);
        assert_eq!(body["data"]["min_leverage"], 1, "未改的字段应保留默认值");
        let changed = body["changed"].as_array().unwrap();
        assert!(changed.iter().any(|v| v == "live_api_secret_enc"));

        let conn = rusqlite::Connection::open(&path).unwrap();
        let audit: String = conn
            .query_row(
                "SELECT changes FROM settings_audit ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(audit.contains("***"), "密钥字段必须脱敏");
        assert!(!audit.contains("super-secret"), "明文密钥不能进审计");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn legacy_paper_mode_reads_back_as_demo() {
        let path = std::env::temp_dir().join("nq-settings-legacy.db");
        let db = fixture(&path);
        let app = Router::new().nest("/api/v1", routes()).with_state(db);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/settings/engine")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["data"]["mode"], "demo", "旧库 paper 应迁移为 demo");
        assert_eq!(body["data"]["max_positions"], 2, "缺字段回退默认值");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn out_of_range_leverage_is_rejected_and_value_stays() {
        let path = std::env::temp_dir().join("nq-settings-range.db");
        let db = fixture(&path);
        let app = Router::new()
            .nest("/api/v1", routes())
            .with_state(db.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/v1/settings/risk")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"patch":{"max_leverage":150}}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            body["detail"].as_str().unwrap().contains("超出取值范围"),
            "{body}"
        );
        let stored: String = db
            .lock()
            .query_row("SELECT value FROM app_settings WHERE key='risk'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(
            stored.contains("\"max_leverage\":50"),
            "非法值整体拒绝，库内不变: {stored}"
        );
        assert!(!stored.contains("150"));
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn a_valid_write_is_visible_immediately_and_audited() {
        let path = std::env::temp_dir().join("nq-settings-hot.db");
        let db = fixture(&path);
        let app = Router::new()
            .nest("/api/v1", routes())
            .with_state(db.clone());
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/v1/settings/risk")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"patch":{"max_leverage":80}}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/settings/risk")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["data"]["max_leverage"], 80, "写入后立即生效");
        let (ts, changes): (i64, String) = db
            .lock()
            .query_row(
                "SELECT ts, changes FROM settings_audit ORDER BY id DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(ts > 0, "审计必须带时间");
        assert!(
            changes.contains("50") && changes.contains("80"),
            "审计记录旧值与新值: {changes}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn every_default_group_is_exposed_by_the_settings_api() {
        let defaults = all_defaults().as_object().unwrap();
        for group in defaults.keys() {
            assert!(
                GROUPS.contains(&group.as_str()),
                "{group} 在 default_settings.json 中但未加入 GROUPS，接口会 404"
            );
        }
    }

    #[test]
    fn defaults_cover_every_group_and_risk_leverage_is_present() {
        assert_eq!(
            GROUPS.len(),
            all_defaults().as_object().unwrap().len(),
            "GROUPS 必须与 default_settings.json 的组数一致"
        );
        for group in GROUPS {
            let value = defaults_of(group);
            assert!(
                value.as_object().is_some_and(|object| !object.is_empty()),
                "{group} 必须有可见默认值"
            );
        }
        let strategy = defaults_of("strategy");
        assert!(strategy.get("ai_review_enabled").is_none());
        assert!(strategy.get("leverage_min").is_none());
        let risk = defaults_of("risk");
        assert_eq!(risk["min_leverage"], 1);
        assert_eq!(risk["max_leverage"], 100);
        assert!(
            risk.get("default_leverage").is_none(),
            "杠杆改由 AI 在上下限内给出，不再有默认杠杆"
        );
    }

    #[tokio::test]
    async fn reset_strategy_writes_defaults_and_security_keeps_credentials() {
        let path = std::env::temp_dir().join("nq-settings-reset.db");
        let db = fixture(&path);
        {
            let conn = db.lock();
            conn.execute(
                "INSERT INTO app_settings VALUES('strategy','{\"mode\":\"trend\"}',0)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO app_settings VALUES('security','{\"username\":\"root\",\"password_hash\":\"hash\",\"token_version\":7,\"password_is_default\":true}',0)",
                [],
            )
            .unwrap();
        }
        let app = Router::new()
            .nest("/api/v1", routes())
            .with_state(db.clone());
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/settings/strategy/reset")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["data"]["mode"], "fusion");
        assert_eq!(body["data"]["timeframe"], "4h");

        let stored: String = db
            .lock()
            .query_row(
                "SELECT value FROM app_settings WHERE key='strategy'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            stored.contains("\"fusion\""),
            "重置必须真正写回默认值: {stored}"
        );
        assert!(
            !stored.contains("\"mr_z_entry\""),
            "重置后不应残留已下线的均值回归字段: {stored}"
        );

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/settings/security/reset")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["data"]["username"], "root");
        assert_eq!(body["data"]["password_hash"], "hash");
        assert_eq!(body["data"]["token_version"], 7);
        assert_eq!(body["data"]["session_ttl_hours"], 12);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn non_numeric_risk_value_is_rejected_instead_of_silently_falling_back() {
        let path = std::env::temp_dir().join("nq-settings-type.db");
        let db = fixture(&path);
        let app = Router::new()
            .nest("/api/v1", routes())
            .with_state(db.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/v1/settings/risk")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"patch":{"max_leverage":"3"}}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "字符串形式的风控参数不能绕过数值校验"
        );
        let stored: String = db
            .lock()
            .query_row("SELECT value FROM app_settings WHERE key='risk'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(
            stored.contains("\"max_leverage\":50"),
            "非法类型整体拒绝，库内保持原值: {stored}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn a_failed_settings_write_returns_500_instead_of_panicking() {
        let path = std::env::temp_dir().join("nq-settings-write-fail.db");
        let db = fixture(&path);
        db.lock()
            .execute_batch(
                "CREATE TRIGGER reject_settings BEFORE UPDATE ON app_settings \
                 BEGIN SELECT RAISE(ABORT, 'write rejected'); END;",
            )
            .unwrap();
        let app = Router::new().nest("/api/v1", routes()).with_state(db);
        let response = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/v1/settings/risk")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"patch":{"max_leverage":80}}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "落库失败必须返回 500，不能用 expect 把请求打成 panic"
        );
        let _ = std::fs::remove_file(&path);
    }
}
