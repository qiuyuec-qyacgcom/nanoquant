//! AI 调用记录。对齐 `api/routes/ai.py` 的日志与用量两个只读接口。
//!
//! 用量窗口是滚动 30 天，不是自然月：预算对比要用同一段时间，否则月初看起来永远没花。
//! `purpose` 用前缀匹配，`decision` 能带出 `decision:scan` 这类细分用途。

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::db::Db;

pub fn routes() -> Router<Db> {
    Router::new()
        .route("/ai/logs", get(logs))
        .route("/ai/usage", get(usage))
}

#[derive(Deserialize)]
struct LogsQuery {
    #[serde(default = "default_limit")]
    limit: i64,
    #[serde(default)]
    purpose: String,
}

fn default_limit() -> i64 {
    60
}

async fn logs(State(db): State<Db>, Query(q): Query<LogsQuery>) -> Json<Value> {
    let limit = q.limit.clamp(1, 500);
    let Ok(conn) = db.read() else {
        return json!({"items": [], "error": "数据库不可用"}).into();
    };
    let (sql, args): (String, Vec<String>) = if q.purpose.is_empty() {
        ("SELECT id, ts, provider, model, purpose, ok, latency_ms, prompt_tokens, completion_tokens, cost_usd, error, raw_text \
          FROM ai_call_logs ORDER BY ts DESC LIMIT ?1".to_string(), vec![limit.to_string()])
    } else {
        ("SELECT id, ts, provider, model, purpose, ok, latency_ms, prompt_tokens, completion_tokens, cost_usd, error, raw_text \
          FROM ai_call_logs WHERE purpose LIKE ?1 || '%' ORDER BY ts DESC LIMIT ?2".to_string(), vec![q.purpose, limit.to_string()])
    };
    let mut stmt = conn.prepare(&sql).expect("prepare logs");
    let items: Vec<Value> = stmt
        .query_map(rusqlite::params_from_iter(args.iter()), |r| {
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "ts": r.get::<_, i64>(1)?,
                "provider": r.get::<_, String>(2)?,
                "model": r.get::<_, String>(3)?,
                "purpose": r.get::<_, String>(4)?,
                "ok": r.get::<_, i64>(5)? != 0,
                "latency_ms": r.get::<_, i64>(6)?,
                "prompt_tokens": r.get::<_, i64>(7)?,
                "completion_tokens": r.get::<_, i64>(8)?,
                "cost_usd": r.get::<_, f64>(9)?,
                "error": r.get::<_, String>(10)?,
                "raw_text": r.get::<_, String>(11)?,
            }))
        })
        .expect("query logs")
        .collect::<Result<_, _>>()
        .expect("log rows");
    json!({ "items": items }).into()
}

async fn usage(State(db): State<Db>) -> Json<Value> {
    let now = crate::now();
    let since_30d = now - 30 * 86400;
    let since_24h = now - 86400;
    let Ok(conn) = db.read() else {
        return json!({
            "by_day": [],
            "by_provider": [],
            "total_30d": {"calls": 0, "cost_usd": 0.0, "calls_24h": 0},
            "budget": {"monthly_usd": 0.0, "used_usd": 0.0, "max_calls_per_hour": 0},
            "error": "数据库不可用",
        })
        .into();
    };
    let mut stmt = conn
        .prepare("SELECT ts, provider, ok, prompt_tokens, completion_tokens, cost_usd FROM ai_call_logs WHERE ts >= ?1")
        .expect("prepare usage");
    let rows: Vec<(i64, String, i64, i64, i64, f64)> = stmt
        .query_map([since_30d], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })
        .expect("query usage")
        .collect::<Result<_, _>>()
        .expect("usage rows");

    let mut by_day: std::collections::BTreeMap<String, (i64, i64, f64, i64)> =
        std::collections::BTreeMap::new();
    let mut by_provider: std::collections::BTreeMap<String, (i64, i64, f64, i64)> =
        std::collections::BTreeMap::new();
    let mut total_cost = 0.0;
    let mut calls_24h = 0_i64;
    for (ts, provider, ok, prompt, completion, cost) in &rows {
        let day = local_day(&conn, *ts);
        let d = by_day.entry(day).or_insert((0, 0, 0.0, 0));
        d.0 += 1;
        d.1 += i64::from(*ok != 0);
        d.2 = round6(d.2 + cost);
        d.3 += prompt + completion;
        let name = if provider.is_empty() {
            "unknown"
        } else {
            provider
        };
        let p = by_provider
            .entry(name.to_string())
            .or_insert((0, 0, 0.0, 0));
        p.0 += 1;
        p.1 += i64::from(*ok != 0);
        p.2 = round6(p.2 + cost);
        p.3 += i64::from(*ok == 0);
        total_cost += cost;
        if *ts >= since_24h {
            calls_24h += 1;
        }
    }
    let days: Vec<Value> = by_day
        .iter()
        .map(|(date, (calls, ok, cost, tokens))| json!({"date": date, "calls": calls, "ok": ok, "cost": cost, "tokens": tokens}))
        .collect();
    let mut providers: Vec<(&String, &(i64, i64, f64, i64))> = by_provider.iter().collect();
    providers.sort_by(|a, b| b.1 .0.cmp(&a.1 .0).then_with(|| a.0.cmp(b.0)));
    let providers: Vec<Value> = providers
        .into_iter()
        .map(|(name, (calls, ok, cost, errors))| json!({"provider": name, "calls": calls, "ok": ok, "cost": cost, "errors": errors}))
        .collect();
    let budget = db_budget(&conn);
    json!({
        "by_day": days,
        "by_provider": providers,
        "total_30d": {"calls": rows.len(), "cost_usd": round6(total_cost), "calls_24h": calls_24h},
        "budget": {"monthly_usd": budget.0, "used_usd": round6(total_cost), "max_calls_per_hour": budget.1},
    })
    .into()
}

fn db_budget(conn: &rusqlite::Connection) -> (f64, i64) {
    conn.query_row("SELECT value FROM app_settings WHERE key = 'ai'", [], |r| {
        r.get::<_, String>(0)
    })
    .ok()
    .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
    .map(|v| {
        (
            v.get("monthly_budget_usd")
                .and_then(|x| x.as_f64())
                .unwrap_or(0.0),
            v.get("max_calls_per_hour")
                .and_then(|x| x.as_i64())
                .unwrap_or(0),
        )
    })
    .unwrap_or((0.0, 0))
}

fn round6(value: f64) -> f64 {
    (value * 1_000_000.0).round() / 1_000_000.0
}

/// 本地时区日期。交给 SQLite 的 localtime，和 legacy 内核（已退役） 的 localtime 用同一个系统时区。
fn local_day(conn: &rusqlite::Connection, ts: i64) -> String {
    conn.query_row(
        "SELECT strftime('%Y-%m-%d', ?1, 'unixepoch', 'localtime')",
        [ts],
        |r| r.get(0),
    )
    .unwrap_or_else(|_| "1970-01-01".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn seed(path: &std::path::Path) -> Db {
        let _ = std::fs::remove_file(path);
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE app_settings(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER); \
             CREATE TABLE ai_call_logs(id INTEGER PRIMARY KEY, ts INTEGER, provider TEXT, model TEXT, purpose TEXT, \
             ok INTEGER, latency_ms INTEGER, prompt_tokens INTEGER, completion_tokens INTEGER, cost_usd REAL, error TEXT, raw_text TEXT); \
             INSERT INTO app_settings VALUES('ai','{\"monthly_budget_usd\":20,\"max_calls_per_hour\":30}',0); \
             INSERT INTO ai_call_logs VALUES(1,strftime('%s','now')-1000,'alpha','m','decision:scan',1,10,100,50,0.125,'','{}'); \
             INSERT INTO ai_call_logs VALUES(2,strftime('%s','now')-500,'','m','decision:manage',0,20,10,0,0.0,'超时',''); \
             INSERT INTO ai_call_logs VALUES(3,strftime('%s','now')-40*86400,'alpha','m','decision',1,5,1,1,9.0,'','');",
        )
        .unwrap();
        drop(conn);
        Db::open(path.to_str().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn logs_filter_by_purpose_prefix_newest_first() {
        let path = std::env::temp_dir().join("nq-ai-logs.db");
        let app = Router::new()
            .nest("/api/v1", routes())
            .with_state(seed(&path));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/ai/logs?purpose=decision")
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
        assert_eq!(
            items.len(),
            3,
            "前缀 decision 应带出 decision:scan 与 decision:manage"
        );
        assert!(items[0]["ts"].as_i64().unwrap() >= items[1]["ts"].as_i64().unwrap());
        assert_eq!(items.iter().find(|i| i["id"] == 2).unwrap()["ok"], false);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn usage_drops_rows_older_than_thirty_days_and_sorts_providers_by_calls() {
        let path = std::env::temp_dir().join("nq-ai-usage.db");
        let app = Router::new()
            .nest("/api/v1", routes())
            .with_state(seed(&path));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/ai/usage")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["total_30d"]["calls"], 2, "40 天前的调用不进 30 天窗口");
        assert_eq!(body["total_30d"]["cost_usd"], 0.125);
        assert_eq!(body["total_30d"]["calls_24h"], 2);
        assert_eq!(
            body["by_provider"][0]["provider"], "alpha",
            "调用次数多的排前面"
        );
        assert_eq!(
            body["by_provider"][1]["provider"], "unknown",
            "空提供方归到 unknown"
        );
        assert_eq!(body["by_provider"][1]["errors"], 1);
        assert_eq!(body["budget"]["monthly_usd"], 20.0);
        assert_eq!(body["budget"]["max_calls_per_hour"], 30);
        let _ = std::fs::remove_file(&path);
    }
}
