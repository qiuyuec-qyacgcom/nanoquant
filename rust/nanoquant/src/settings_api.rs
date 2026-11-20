//! 配置组读写。13 个组的默认值与取值范围对齐 `services/settings.py`。
//!
//! 未知组返回 404，不静默建组：前端拼错组名时若返回空对象，
//! 用户会以为配置丢了再存一次，把别的组覆盖掉。
//! 审计只记真正变化的字段，字段名含 secret/password/token/api_key/_enc 的值一律写成 "***"。

use std::collections::BTreeMap;

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
        .route("/settings/{group}", get(read_group).put(write_group))
        .route("/settings/{group}/reset", post(reset_group))
        .route("/system/system-settings", get(system_settings))
}

const GROUPS: [&str; 13] = [
    "engine",
    "risk",
    "strategy",
    "ai",
    "data",
    "news",
    "evolution",
    "backup",
    "security",
    "system",
    "public",
    "signal_follow",
    "journal",
];

async fn read_all(State(db): State<Db>) -> Json<Value> {
    let conn = db.conn.lock().expect("db");
    let mut groups = Map::new();
    let mut defaults = Map::new();
    for name in GROUPS {
        groups.insert(name.to_string(), group_value(&conn, name));
        defaults.insert(name.to_string(), defaults_of(name));
    }
    json!({ "groups": groups, "defaults": defaults }).into()
}

async fn read_group(
    State(db): State<Db>,
    Path(group): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !GROUPS.contains(&group.as_str()) {
        return Err(unknown(&group));
    }
    let conn = db.conn.lock().expect("db");
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
    let conn = db.conn.lock().expect("db");
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
    if let Err(detail) = validate(&group, &candidate) {
        return Err((StatusCode::BAD_REQUEST, json!({ "detail": detail }).into()));
    }
    let payload = serde_json::to_string(&candidate).unwrap_or_else(|_| "{}".to_string());
    conn.execute(
        "INSERT INTO app_settings(key, value, updated_at) VALUES(?1, ?2, ?3) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        (&group, &payload, crate::now()),
    )
    .expect("save settings");
    let changes = diff(
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
    let conn = db.conn.lock().expect("db");
    conn.execute("DELETE FROM app_settings WHERE key = ?1", [&group])
        .ok();
    Ok(json!({ "ok": true, "group": group, "data": defaults_of(&group) }).into())
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
    let conn = db.conn.lock().expect("db");
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
    let conn = db.conn.lock().expect("db");
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
            out.remove("max_hold_minutes");
            out.remove("max_hold_minutes_chop");
        }
        "system" => {
            out.remove("timezone_offset");
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

/// 只记变化的字段。命中敏感词的值写成 "***"，审计日志不能变成泄密源。
fn diff(before: &Map<String, Value>, after: &Map<String, Value>) -> BTreeMap<String, Value> {
    let mut changes = BTreeMap::new();
    for (key, new) in after {
        let old = before.get(key).cloned().unwrap_or(Value::Null);
        if &old == new {
            continue;
        }
        changes.insert(
            key.clone(),
            json!({ "from": mask(key, &old), "to": mask(key, new) }),
        );
    }
    changes
}

fn mask(key: &str, value: &Value) -> Value {
    let lower = key.to_lowercase();
    if ["secret", "password", "token", "api_key", "_enc"]
        .iter()
        .any(|hint| lower.contains(hint))
    {
        json!("***")
    } else {
        value.clone()
    }
}

pub(crate) fn defaults_of(name: &str) -> Value {
    match name {
        "engine" => {
            json!({"enabled": false, "mode": "demo", "cycle_interval_sec": 15, "max_positions": 2, "max_symbols_per_cycle": 12, "scan_concurrency": 3, "trading_windows": ["00:00-24:00"], "timezone_offset": 8, "close_on_halt": false, "order_type": "market", "recv_window_ms": 5000})
        }
        "risk" => {
            json!({"risk_per_trade_pct": 10.0, "max_leverage": 100, "default_leverage": 20, "margin_pct": 50.0, "max_exposure_pct": 2000.0, "max_same_direction": 0, "category_exposure_limit_pct": 500.0, "vol_target_atr_pct": 0.0, "daily_loss_limit_pct": 25.0, "max_drawdown_pct": 50.0, "consecutive_loss_limit": 4, "cooldown_minutes": 10, "symbol_cooldown_minutes": 5, "min_notional_buffer": 1.05, "min_equity_usdt": 0.5, "fee_rate_taker": 0.0005, "fee_rate_maker": 0.0002, "auto_protect_external": false, "global_stop_loss_pct": 0.0, "global_take_profit_pct": 0.0})
        }
        "backup" => {
            json!({"retention": 10, "encrypt": true, "auto_daily": true, "include_logs": false, "last_run_ts": 0})
        }
        "journal" => {
            json!({"enabled": true, "min_record_confidence": 0.45, "label_after_sec": 86400, "correct_threshold_pct": 0.15, "retrieve_top_k": 5, "inject_experiences": true, "max_experiences": 200, "min_similarity": 0.15})
        }
        "public" => {
            json!({"site_enabled": true, "site_title": "NanoQuant", "site_tagline": "小资金 · 币安 U 本位永续 · 超短线量化终端", "hero_note": "", "show_engine_status": true, "show_mantras": true, "show_performance": false})
        }
        "system" => {
            json!({"log_level": "INFO", "event_retention_days": 14, "signal_retention_days": 14, "equity_retention_days": 30, "risk_disclaimer_accepted": false, "alert_enabled": false, "alert_min_level": "warn", "alert_webhook_url": "", "alert_error_webhook_url": "", "alert_telegram_token": "", "alert_telegram_chat_id": "", "alert_cooldown_sec": 120, "alert_quiet_hours": "", "alert_template": "", "alert_digest_enabled": false, "alert_digest_minutes": 60, "exec_mode": "off", "exec_socket": "", "exec_sample_pct": 100, "exec_timeout_ms": 500, "exec_place_protection": false, "exec_ledger_authoritative": false, "api_rate_limit_per_min": 0, "metrics_enabled": true})
        }
        _ => json!({}),
    }
}

fn validate(group: &str, value: &Value) -> Result<(), String> {
    let num = |key: &str| value.get(key).and_then(|v| v.as_f64());
    let check = |key: &str, lo: f64, hi: f64| -> Result<(), String> {
        match num(key) {
            Some(n) if (lo..=hi).contains(&n) => Ok(()),
            Some(_) => Err(format!("{key}: 超出取值范围")),
            None => Ok(()),
        }
    };
    match group {
        "engine" => check("cycle_interval_sec", 15.0, 3600.0)?,
        "risk" => {
            check("max_leverage", 1.0, 100.0)?;
            check("daily_loss_limit_pct", 0.01, 100.0)?;
            check("global_stop_loss_pct", 0.0, 100.0)?;
            check("global_take_profit_pct", 0.0, 100.0)?;
        }
        "journal" => check("min_similarity", 0.0, 1.0)?,
        "backup" => check("retention", 1.0, 200.0)?,
        _ => {}
    }
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
        assert_eq!(
            body["data"]["default_leverage"], 20,
            "未改的字段应保留默认值"
        );
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
            .conn
            .lock()
            .unwrap()
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
            .conn
            .lock()
            .unwrap()
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
}
