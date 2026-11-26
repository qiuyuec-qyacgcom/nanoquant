//! AI 分析策略。纯文本指令，按优先级注入决策上下文，改不了杠杆和止损。
//!
//! 一条启用的策略都没有时，标的走默认分析，这是设计而不是缺失。
//! 白名单为空表示全池适用；方向偏好只影响提示词，不决定这条策略管不管这个标的。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::db::Db;

pub fn routes() -> Router<Db> {
    Router::new()
        .route("/strategies", get(list_strategies).post(create_strategy))
        .route("/strategies/preview", get(preview))
        .route(
            "/strategies/{id}",
            delete(remove_strategy).patch(update_strategy),
        )
}

#[derive(Deserialize)]
struct StrategyBody {
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    rule: String,
    #[serde(default = "default_enabled")]
    enabled: bool,
    #[serde(default = "default_priority")]
    priority: i64,
    #[serde(default)]
    filters: Value,
}

fn default_enabled() -> bool {
    true
}

fn default_priority() -> i64 {
    100
}

async fn list_strategies(State(db): State<Db>) -> Json<Value> {
    let conn = db.lock();
    let rows = load(&conn);
    json!({
        "items": rows.iter().map(serialize).collect::<Vec<_>>(),
        "stats": stats(&rows),
        "last_applied": engine_value(&conn, "strategy_applied"),
    })
    .into()
}

async fn create_strategy(
    State(db): State<Db>,
    Json(body): Json<StrategyBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let name = body.name.trim().to_string();
    if name.chars().count() < 2 {
        return Err(bad("策略名称至少 2 个字"));
    }
    let rule = normalize_rule(&body.rule);
    if rule.chars().count() < 8 {
        return Err(bad("策略内容至少 8 个字（AI 需要可执行的判断依据）"));
    }
    let conn = db.lock();
    let taken: bool = conn
        .query_row(
            "SELECT 1 FROM strategy_profiles WHERE name = ?1",
            [&name],
            |_| Ok(()),
        )
        .is_ok();
    if taken {
        return Err(bad(&format!("同名策略已存在: {name}")));
    }
    let filters = serde_json::to_string(&normalize_filters(&body.filters))
        .unwrap_or_else(|_| "{}".to_string());
    let now = crate::now();
    conn.execute(
        "INSERT INTO strategy_profiles(name, description, rule, enabled, priority, filters, created_at, updated_at) \
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
        (&name, &body.description.chars().take(200).collect::<String>(), &rule, i64::from(body.enabled), body.priority, &filters, now),
    )
    .expect("insert strategy");
    let id = conn.last_insert_rowid();
    record(&conn, "info", &format!("新增策略: {name}"));
    let row = load(&conn)
        .into_iter()
        .find(|r| r.id == id)
        .expect("created");
    Ok(json!({ "ok": true, "item": serialize(&row) }).into())
}

#[derive(Deserialize, Default)]
struct StrategyPatch {
    name: Option<String>,
    description: Option<String>,
    rule: Option<String>,
    enabled: Option<bool>,
    priority: Option<i64>,
    filters: Option<Value>,
}

async fn update_strategy(
    State(db): State<Db>,
    Path(id): Path<i64>,
    Json(patch): Json<StrategyPatch>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = db.lock();
    let Some(mut row) = load(&conn).into_iter().find(|r| r.id == id) else {
        return Err((
            StatusCode::NOT_FOUND,
            json!({ "detail": "策略不存在" }).into(),
        ));
    };
    if let Some(name) = patch.name {
        let name = name.trim().to_string();
        if name.chars().count() < 2 {
            return Err(bad("策略名称至少 2 个字"));
        }
        let taken: bool = conn
            .query_row(
                "SELECT 1 FROM strategy_profiles WHERE name = ?1 AND id != ?2",
                (&name, id),
                |_| Ok(()),
            )
            .is_ok();
        if taken {
            return Err(bad(&format!("同名策略已存在: {name}")));
        }
        row.name = name;
    }
    if let Some(description) = patch.description {
        row.description = description.chars().take(200).collect();
    }
    if let Some(rule) = patch.rule {
        let rule = normalize_rule(&rule);
        if rule.chars().count() < 8 {
            return Err(bad("策略内容至少 8 个字（AI 需要可执行的判断依据）"));
        }
        row.rule = rule;
    }
    if let Some(enabled) = patch.enabled {
        row.enabled = enabled;
    }
    if let Some(priority) = patch.priority {
        row.priority = priority;
    }
    if let Some(filters) = patch.filters {
        row.filters = normalize_filters(&filters);
    }
    let filters = serde_json::to_string(&row.filters).unwrap_or_else(|_| "{}".to_string());
    conn.execute(
        "UPDATE strategy_profiles SET name=?1, description=?2, rule=?3, enabled=?4, priority=?5, filters=?6, updated_at=?7 WHERE id=?8",
        (&row.name, &row.description, &row.rule, i64::from(row.enabled), row.priority, &filters, crate::now(), id),
    )
    .expect("update strategy");
    record(&conn, "info", &format!("更新策略: {}", row.name));
    Ok(json!({ "ok": true, "item": serialize(&row) }).into())
}

async fn remove_strategy(
    State(db): State<Db>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = db.lock();
    let changed = conn
        .execute("DELETE FROM strategy_profiles WHERE id = ?1", [id])
        .unwrap_or(0);
    if changed == 0 {
        return Err((
            StatusCode::NOT_FOUND,
            json!({ "detail": "策略不存在" }).into(),
        ));
    }
    record(&conn, "warn", &format!("删除策略 #{id}"));
    Ok(json!({ "ok": true }).into())
}

async fn preview(State(db): State<Db>) -> Json<Value> {
    let conn = db.lock();
    let limit = engine_int(&conn, "max_symbols_per_cycle").max(1);
    let mut stmt = conn
        .prepare("SELECT symbol FROM trading_symbols WHERE enabled = 1 ORDER BY sort_order, symbol LIMIT ?1")
        .expect("prepare pool");
    let pool: Vec<String> = stmt
        .query_map([limit], |r| r.get(0))
        .expect("query pool")
        .collect::<Result<_, _>>()
        .unwrap_or_default();
    let active: Vec<Profile> = load(&conn).into_iter().filter(|r| r.enabled).collect();
    let items: Vec<Value> = pool
        .iter()
        .map(|symbol| {
            let picked = pick(symbol, &active);
            json!({
                "symbol": symbol,
                "profile": picked.map(|p| p.name.clone()).unwrap_or_default(),
                "side": picked.map(|p| p.filters["side"].as_str().unwrap_or("any").to_string()).unwrap_or_else(|| "any".to_string()),
                "default": picked.is_none(),
            })
        })
        .collect();
    json!({ "ok": true, "default": active.is_empty(), "profiles": active.len(), "items": items })
        .into()
}

fn bad(detail: &str) -> (StatusCode, Json<Value>) {
    (StatusCode::BAD_REQUEST, json!({ "detail": detail }).into())
}

struct Profile {
    id: i64,
    name: String,
    description: String,
    rule: String,
    enabled: bool,
    priority: i64,
    filters: Value,
    created_at: i64,
    updated_at: i64,
}

fn load(conn: &rusqlite::Connection) -> Vec<Profile> {
    let mut stmt = conn
        .prepare(
            "SELECT id, name, description, rule, enabled, priority, filters, created_at, updated_at \
             FROM strategy_profiles ORDER BY priority ASC, id ASC",
        )
        .expect("prepare strategies");
    stmt.query_map([], |r| {
        let filters: String = r.get::<_, String>(6).unwrap_or_default();
        Ok(Profile {
            id: r.get(0)?,
            name: r.get(1)?,
            description: r.get::<_, String>(2).unwrap_or_default(),
            rule: r.get::<_, String>(3).unwrap_or_default(),
            enabled: r.get::<_, i64>(4)? != 0,
            priority: r.get(5)?,
            filters: normalize_filters(&serde_json::from_str(&filters).unwrap_or(Value::Null)),
            created_at: r.get(7)?,
            updated_at: r.get(8)?,
        })
    })
    .expect("query strategies")
    .collect::<Result<_, _>>()
    .expect("strategy rows")
}

fn serialize(row: &Profile) -> Value {
    json!({
        "id": row.id,
        "name": row.name,
        "description": row.description,
        "rule": row.rule,
        "enabled": row.enabled,
        "priority": row.priority,
        "filters": row.filters,
        "summary": summarize(&row.filters),
        "created_at": row.created_at,
        "updated_at": row.updated_at,
    })
}

fn stats(rows: &[Profile]) -> Value {
    let enabled: Vec<&Profile> = rows.iter().filter(|r| r.enabled).collect();
    let count = |side: &str| {
        enabled
            .iter()
            .filter(|r| r.filters["side"].as_str() == Some(side))
            .count()
    };
    json!({
        "total": rows.len(),
        "enabled": enabled.len(),
        "default": enabled.is_empty(),
        "by_side": { "any": count("any"), "long": count("long"), "short": count("short") },
    })
}

fn normalize_filters(raw: &Value) -> Value {
    let symbols = match raw.get("symbols") {
        Some(Value::String(text)) => text
            .replace('，', ",")
            .split(',')
            .map(|s| s.trim().to_uppercase())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str())
            .map(|s| s.trim().to_uppercase())
            .filter(|s| !s.is_empty())
            .collect(),
        _ => Vec::new(),
    };
    let mut symbols = symbols;
    symbols.sort();
    symbols.dedup();
    let side = raw
        .get("side")
        .and_then(|v| v.as_str())
        .unwrap_or("any")
        .trim()
        .to_lowercase();
    let side = if ["any", "long", "short"].contains(&side.as_str()) {
        side
    } else {
        "any".to_string()
    };
    json!({ "symbols": symbols, "side": side })
}

fn normalize_rule(text: &str) -> String {
    text.trim().chars().take(2000).collect()
}

fn summarize(filters: &Value) -> String {
    let symbols = filters["symbols"].as_array().cloned().unwrap_or_default();
    let names: Vec<&str> = symbols.iter().filter_map(|v| v.as_str()).collect();
    let scope = if names.is_empty() {
        "全池".to_string()
    } else {
        let mut text = names.iter().take(4).cloned().collect::<Vec<_>>().join("/");
        if names.len() > 4 {
            text.push('…');
        }
        text
    };
    let side = match filters["side"].as_str() {
        Some("long") => "只做多",
        Some("short") => "只做空",
        _ => "不限方向",
    };
    format!("{scope} · {side}")
}

/// 白名单为空表示全池适用；停用的不参与。
fn pick<'a>(symbol: &str, profiles: &'a [Profile]) -> Option<&'a Profile> {
    profiles.iter().find(|p| {
        if !p.enabled {
            return false;
        }
        let scope = p.filters["symbols"].as_array().cloned().unwrap_or_default();
        scope.is_empty() || scope.iter().any(|v| v.as_str() == Some(symbol))
    })
}

pub(crate) fn profile_for(conn: &rusqlite::Connection, symbol: &str) -> Option<Value> {
    let active: Vec<Profile> = load(conn).into_iter().filter(|row| row.enabled).collect();
    pick(symbol, &active).map(serialize)
}

pub(crate) fn compact(profile: &Value) -> Value {
    let side = match profile
        .get("filters")
        .and_then(|filters| filters.get("side"))
        .and_then(Value::as_str)
    {
        Some("long") => "L",
        Some("short") => "S",
        _ => "",
    };
    json!({
        "n": profile.get("name").and_then(Value::as_str).unwrap_or(""),
        "r": profile.get("rule").and_then(Value::as_str).unwrap_or(""),
        "s": side,
    })
}

pub(crate) fn mark_applied(conn: &rusqlite::Connection, profile: &Value) {
    let name = profile.get("name").and_then(Value::as_str).unwrap_or("");
    if name.is_empty() {
        return;
    }
    conn.execute(
        "INSERT INTO engine_state(key,value,updated_at) VALUES('strategy_applied',?1,?2) \
         ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
        (name, crate::now()),
    )
    .ok();
}

fn engine_value(conn: &rusqlite::Connection, key: &str) -> String {
    conn.query_row(
        "SELECT value FROM engine_state WHERE key = ?1",
        [key],
        |r| r.get(0),
    )
    .unwrap_or_default()
}

fn engine_int(conn: &rusqlite::Connection, key: &str) -> i64 {
    let raw = conn
        .query_row(
            "SELECT value FROM app_settings WHERE key = 'engine'",
            [],
            |r| r.get::<_, String>(0),
        )
        .unwrap_or_default();
    serde_json::from_str::<Value>(&raw)
        .ok()
        .and_then(|v| v.get(key).and_then(|n| n.as_i64()))
        .unwrap_or(12)
}

fn record(conn: &rusqlite::Connection, level: &str, message: &str) {
    conn.execute(
        "INSERT INTO system_events(ts, level, source, message) VALUES(?1, ?2, 'strategy', ?3)",
        (crate::now(), level, message),
    )
    .ok();
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
            "CREATE TABLE strategy_profiles(id INTEGER PRIMARY KEY, name TEXT UNIQUE, description TEXT, rule TEXT, \
             enabled INTEGER, priority INTEGER, filters TEXT, created_at INTEGER, updated_at INTEGER); \
             CREATE TABLE trading_symbols(symbol TEXT PRIMARY KEY, enabled INTEGER, sort_order INTEGER); \
             CREATE TABLE engine_state(key TEXT PRIMARY KEY, value TEXT); \
             CREATE TABLE app_settings(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER); \
             CREATE TABLE system_events(id INTEGER PRIMARY KEY, ts INTEGER, level TEXT, source TEXT, message TEXT); \
             INSERT INTO strategy_profiles VALUES(1,'突破追多','','放量突破后等回踩再进',1,10,'{\"symbols\":[\"BTCUSDT\"],\"side\":\"long\"}',1,1); \
             INSERT INTO strategy_profiles VALUES(2,'全池兜底','','没有专属策略时用这条',1,50,'{\"symbols\":[],\"side\":\"any\"}',2,2); \
             INSERT INTO trading_symbols VALUES('BTCUSDT',1,1); \
             INSERT INTO trading_symbols VALUES('ETHUSDT',1,2); \
             INSERT INTO app_settings VALUES('engine','{\"max_symbols_per_cycle\":12}',0);",
        )
        .unwrap();
        drop(conn);
        Db::open(path.to_str().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn duplicate_name_is_rejected() {
        let path = std::env::temp_dir().join("nq-strategy-dup.db");
        let db = fixture(&path);
        let app = Router::new().nest("/api/v1", routes()).with_state(db);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/strategies")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"name":"突破追多","rule":"另一套完全不同的规则"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn preview_falls_back_to_the_pool_wide_strategy() {
        let path = std::env::temp_dir().join("nq-strategy-preview.db");
        let db = fixture(&path);
        let app = Router::new().nest("/api/v1", routes()).with_state(db);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/strategies/preview")
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
        let items = body["items"].as_array().unwrap();
        assert_eq!(items[0]["symbol"], "BTCUSDT");
        assert_eq!(items[0]["profile"], "突破追多", "优先级高的专属策略先命中");
        assert_eq!(items[0]["side"], "long");
        assert_eq!(items[1]["symbol"], "ETHUSDT");
        assert_eq!(
            items[1]["profile"], "全池兜底",
            "白名单为空的策略兜住其余标的"
        );
        assert_eq!(items[1]["default"], false);
        let _ = std::fs::remove_file(&path);
    }
}
