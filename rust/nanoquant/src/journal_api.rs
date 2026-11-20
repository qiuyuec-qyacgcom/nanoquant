//! 决策日志与经验库的只读接口。
//!
//! 准确率只拿「正确 / 错误」做分母，`partial` 是噪声区间，算进去会把准确率
//! 抬高。经验库的相似度是字符二元组的 Dice 系数，和 legacy 内核（已退役） 的检索口径一致；
//! 低于阈值的不返回，否则面板会把毫不相关的旧经验排上来。

use axum::extract::{Path, Query, State};
use axum::routing::{delete, get};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::db::Db;

pub fn routes() -> Router<Db> {
    Router::new()
        .route("/journal", get(list_journal))
        .route("/journal/stats", get(journal_stats))
        .route("/journal/experiences", get(list_experiences))
        .route("/journal/experiences/stats", get(experience_stats))
        .route("/journal/experiences/search", get(search_experiences))
        .route("/journal/experiences/{id}", delete(remove_experience))
}

#[derive(Deserialize)]
struct JournalQuery {
    #[serde(default = "default_journal_limit")]
    limit: i64,
    #[serde(default)]
    status: String,
    #[serde(default)]
    symbol: String,
    #[serde(default)]
    mode: String,
}

fn default_journal_limit() -> i64 {
    200
}

async fn list_journal(State(db): State<Db>, Query(q): Query<JournalQuery>) -> Json<Value> {
    let limit = q.limit.clamp(1, 1000);
    let conn = db.conn.lock().expect("db");
    let mut stmt = conn
        .prepare(
            "SELECT id, ts, symbol, direction, confidence, entry_price, target_pct, reason, snapshot, \
             decision, signal_id, injected_experience_ids, status, outcome, actual_price, actual_pct, \
             labeled_at, archived, mode FROM decision_journal \
             WHERE (?1 = '' OR status = ?1) AND (?2 = '' OR symbol = ?2) AND (?3 = '' OR mode = ?3) \
             ORDER BY ts DESC LIMIT ?4",
        )
        .expect("prepare journal");
    let items = stmt
        .query_map(
            (q.status.as_str(), q.symbol.as_str(), q.mode.as_str(), limit),
            serialize_journal,
        )
        .expect("query journal")
        .collect::<Result<Vec<_>, _>>()
        .expect("journal rows");
    json!({ "items": items }).into()
}

fn serialize_journal(r: &rusqlite::Row) -> rusqlite::Result<Value> {
    let snapshot: String = r.get::<_, String>(8).unwrap_or_default();
    let injected: String = r.get::<_, String>(11).unwrap_or_default();
    Ok(json!({
        "id": r.get::<_, i64>(0)?,
        "ts": r.get::<_, i64>(1)?,
        "symbol": r.get::<_, String>(2)?,
        "direction": r.get::<_, String>(3)?,
        "confidence": r.get::<_, f64>(4)?,
        "entry_price": r.get::<_, f64>(5)?,
        "target_pct": r.get::<_, f64>(6)?,
        "reason": r.get::<_, String>(7)?,
        "snapshot": serde_json::from_str::<Value>(&snapshot).unwrap_or(json!({})),
        "decision": r.get::<_, String>(9)?,
        "signal_id": r.get::<_, i64>(10)?,
        "injected_experience_ids": serde_json::from_str::<Value>(&injected).unwrap_or(json!([])),
        "status": r.get::<_, String>(12)?,
        "outcome": r.get::<_, String>(13)?,
        "actual_price": r.get::<_, f64>(14)?,
        "actual_pct": r.get::<_, f64>(15)?,
        "labeled_at": r.get::<_, i64>(16)?,
        "archived": r.get::<_, i64>(17)? != 0,
        "mode": r.get::<_, String>(18)?,
    }))
}

async fn journal_stats(State(db): State<Db>, Query(q): Query<JournalQuery>) -> Json<Value> {
    let conn = db.conn.lock().expect("db");
    let mut stmt = conn
        .prepare("SELECT status, outcome FROM decision_journal WHERE ?1 = '' OR mode = ?1")
        .expect("prepare journal stats");
    let rows: Vec<(String, String)> = stmt
        .query_map([q.mode.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))
        .expect("query journal stats")
        .collect::<Result<_, _>>()
        .expect("journal stat rows");
    let correct = rows.iter().filter(|(_, o)| o == "correct").count();
    let wrong = rows.iter().filter(|(_, o)| o == "wrong").count();
    let judged = correct + wrong;
    json!({
        "total": rows.len(),
        "pending": rows.iter().filter(|(s, _)| s == "pending").count(),
        "labeled": rows.iter().filter(|(s, _)| s == "labeled").count(),
        "correct": correct,
        "wrong": wrong,
        "partial": rows.iter().filter(|(_, o)| o == "partial").count(),
        "accuracy": if judged == 0 { 0.0 } else { round_n(correct as f64 / judged as f64 * 100.0, 2) },
    })
    .into()
}

#[derive(Deserialize)]
struct ExperienceQuery {
    #[serde(default = "default_experience_limit")]
    limit: i64,
    #[serde(default)]
    symbol: String,
    #[serde(default)]
    outcome: String,
    #[serde(default)]
    q: String,
}

fn default_experience_limit() -> i64 {
    100
}

async fn list_experiences(State(db): State<Db>, Query(q): Query<ExperienceQuery>) -> Json<Value> {
    let limit = q.limit.clamp(1, 500);
    let conn = db.conn.lock().expect("db");
    let cfg = journal_cfg(&conn);
    let mut rows = load_experiences(&conn);
    rows.retain(|r| {
        (q.symbol.is_empty() || r.symbol == q.symbol)
            && (q.outcome.is_empty() || r.outcome == q.outcome)
    });
    if !q.q.trim().is_empty() {
        let needle = q.q.trim().to_lowercase();
        rows.retain(|r| {
            format!("{} {}", r.lesson, r.tags)
                .to_lowercase()
                .contains(&needle)
        });
        rows.sort_by_key(|r| std::cmp::Reverse(r.created_at));
    } else {
        rows.sort_by(|a, b| {
            let key = |r: &Experience| (hit_rate(r), r.updated_at);
            key(b)
                .partial_cmp(&key(a))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }
    let items: Vec<Value> = rows
        .into_iter()
        .take(limit as usize)
        .map(|r| serialize_experience(&r, cfg.min_similarity))
        .collect();
    json!({ "items": items }).into()
}

async fn experience_stats(State(db): State<Db>) -> Json<Value> {
    let conn = db.conn.lock().expect("db");
    let rows = load_experiences(&conn);
    let mut by_outcome = serde_json::Map::new();
    let mut by_symbol = serde_json::Map::new();
    for row in &rows {
        bump(&mut by_outcome, &row.outcome);
        bump(&mut by_symbol, &row.symbol);
    }
    let avg = if rows.is_empty() {
        0.0
    } else {
        rows.iter().map(hit_rate).sum::<f64>() / rows.len() as f64
    };
    json!({
        "total": rows.len(),
        "by_outcome": by_outcome,
        "by_symbol": by_symbol,
        "avg_hit_rate": round_n(avg, 3),
        "judged": rows.iter().map(|r| r.wins + r.losses).sum::<i64>(),
    })
    .into()
}

#[derive(Deserialize)]
struct SearchQuery {
    #[serde(default)]
    q: String,
    #[serde(default = "default_search_limit")]
    limit: i64,
}

fn default_search_limit() -> i64 {
    5
}

async fn search_experiences(State(db): State<Db>, Query(q): Query<SearchQuery>) -> Json<Value> {
    let conn = db.conn.lock().expect("db");
    let cfg = journal_cfg(&conn);
    let limit = q.limit.clamp(1, 20).min(cfg.retrieve_top_k.max(1));
    if !cfg.inject_experiences || q.q.trim().is_empty() {
        return json!({ "items": [] }).into();
    }
    let mut scored: Vec<(f64, Experience)> = load_experiences(&conn)
        .into_iter()
        .map(|r| (similarity(&q.q, &r.lesson), r))
        .filter(|(score, _)| *score >= cfg.min_similarity)
        .collect();
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| hit_rate(&b.1).total_cmp(&hit_rate(&a.1)))
    });
    let items: Vec<Value> = scored
        .into_iter()
        .take(limit as usize)
        .map(|(score, r)| {
            let mut item = serialize_experience(&r, cfg.min_similarity);
            item["similarity"] = json!(round_n(score, 4));
            item
        })
        .collect();
    json!({ "items": items }).into()
}

async fn remove_experience(State(db): State<Db>, Path(id): Path<i64>) -> Json<Value> {
    let conn = db.conn.lock().expect("db");
    let changed = conn
        .execute("DELETE FROM experiences WHERE id = ?1", [id])
        .unwrap_or(0);
    json!({ "ok": changed > 0 }).into()
}

struct JournalCfg {
    retrieve_top_k: i64,
    inject_experiences: bool,
    min_similarity: f64,
}

fn journal_cfg(conn: &rusqlite::Connection) -> JournalCfg {
    let raw = conn
        .query_row(
            "SELECT value FROM app_settings WHERE key = 'journal'",
            [],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or(Value::Null);
    JournalCfg {
        retrieve_top_k: raw
            .get("retrieve_top_k")
            .and_then(|v| v.as_i64())
            .unwrap_or(5),
        inject_experiences: raw
            .get("inject_experiences")
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
        min_similarity: raw
            .get("min_similarity")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.15),
    }
}

struct Experience {
    id: i64,
    created_at: i64,
    updated_at: i64,
    symbol: String,
    tags: String,
    lesson: String,
    outcome: String,
    source: String,
    hits: i64,
    wins: i64,
    losses: i64,
    enabled: bool,
}

fn load_experiences(conn: &rusqlite::Connection) -> Vec<Experience> {
    let mut stmt = conn
        .prepare(
            "SELECT id, created_at, updated_at, symbol, tags, lesson, outcome, source, hits, wins, \
             losses, enabled FROM experiences",
        )
        .expect("prepare experiences");
    stmt.query_map([], |r| {
        Ok(Experience {
            id: r.get(0)?,
            created_at: r.get(1)?,
            updated_at: r.get(2)?,
            symbol: r.get(3)?,
            tags: r.get::<_, String>(4).unwrap_or_default(),
            lesson: r.get(5)?,
            outcome: r.get(6)?,
            source: r.get(7)?,
            hits: r.get(8)?,
            wins: r.get(9)?,
            losses: r.get(10)?,
            enabled: r.get::<_, i64>(11)? != 0,
        })
    })
    .expect("query experiences")
    .collect::<Result<_, _>>()
    .expect("experience rows")
}

fn hit_rate(r: &Experience) -> f64 {
    let judged = r.wins + r.losses;
    if judged <= 0 {
        0.5
    } else {
        r.wins as f64 / judged as f64
    }
}

fn serialize_experience(r: &Experience, min_similarity: f64) -> Value {
    json!({
        "id": r.id,
        "created_at": r.created_at,
        "updated_at": r.updated_at,
        "symbol": r.symbol,
        "outcome": r.outcome,
        "lesson": r.lesson,
        "tags": serde_json::from_str::<Value>(&r.tags).unwrap_or(json!([])),
        "source": r.source,
        "hits": r.hits,
        "wins": r.wins,
        "losses": r.losses,
        "hit_rate": round_n(hit_rate(r), 3),
        "enabled": r.enabled,
        "min_similarity": min_similarity,
    })
}

fn bump(map: &mut serde_json::Map<String, Value>, key: &str) {
    let n = map.get(key).and_then(|v| v.as_i64()).unwrap_or(0) + 1;
    map.insert(key.to_string(), json!(n));
}

/// 字符二元组的 Dice 系数，对齐 legacy 内核（已退役） 经验检索。
fn similarity(query: &str, text: &str) -> f64 {
    let left = bigrams(&query.to_lowercase());
    let right = bigrams(&text.to_lowercase());
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    let shared = left
        .iter()
        .filter(|(k, _)| right.contains_key(k))
        .map(|(k, n)| n.min(right.get(k).unwrap_or(&0)))
        .sum::<usize>();
    2.0 * shared as f64 / (left.values().sum::<usize>() + right.values().sum::<usize>()) as f64
}

fn bigrams(text: &str) -> std::collections::HashMap<(char, char), usize> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = std::collections::HashMap::new();
    for pair in chars.windows(2) {
        *out.entry((pair[0], pair[1])).or_insert(0) += 1;
    }
    out
}

fn round_n(value: f64, digits: u32) -> f64 {
    let scale = 10_f64.powi(digits as i32);
    (value * scale).round() / scale
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn fixture(path: &std::path::Path) -> Db {
        let _ = std::fs::remove_file(path);
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE app_settings(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER); \
             CREATE TABLE decision_journal(id INTEGER PRIMARY KEY, ts INTEGER, symbol TEXT, direction TEXT, \
             confidence REAL, entry_price REAL, target_pct REAL, reason TEXT, snapshot TEXT, decision TEXT, \
             signal_id INTEGER, injected_experience_ids TEXT, status TEXT, outcome TEXT, actual_price REAL, \
             actual_pct REAL, labeled_at INTEGER, archived INTEGER, mode TEXT); \
             CREATE TABLE experiences(id INTEGER PRIMARY KEY, symbol TEXT, tags TEXT, scenario TEXT, lesson TEXT, \
             outcome TEXT, source_decision_id INTEGER, source TEXT, hits INTEGER, wins INTEGER, losses INTEGER, \
             enabled INTEGER, created_at INTEGER, updated_at INTEGER); \
             INSERT INTO app_settings VALUES('journal','{\"retrieve_top_k\":5,\"inject_experiences\":true,\"min_similarity\":0.5}',0); \
             INSERT INTO decision_journal VALUES(1,300,'BTCUSDT','long',0.8,100,1.5,'突破','{}','open_long',0,'[]','labeled','correct',101,1.0,400,0,'demo'); \
             INSERT INTO decision_journal VALUES(2,200,'ETHUSDT','short',0.6,50,-1,'回落','{}','open_short',0,'[]','labeled','wrong',51,-2.0,350,0,'demo'); \
             INSERT INTO decision_journal VALUES(3,100,'BTCUSDT','long',0.4,90,0.2,'观望','{}','open_long',0,'[]','labeled','partial',90,0.05,300,0,'demo'); \
             INSERT INTO decision_journal VALUES(4,50,'BTCUSDT','long',0.9,80,2,'实盘','{}','open_long',0,'[]','pending','','',0,0,0,'live'); \
             INSERT INTO experiences VALUES(1,'BTCUSDT','[\"sym:BTCUSDT\"]','{}','放量突破追多，突破要等回踩','win',0,'trade',2,2,0,1,500,500); \
             INSERT INTO experiences VALUES(2,'ETHUSDT','[]','{}','横盘缩量不做','loss',0,'trade',1,0,1,1,400,400);",
        )
        .unwrap();
        drop(conn);
        Db::open(path.to_str().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn accuracy_ignores_partial_and_filters_by_mode() {
        let path = std::env::temp_dir().join("nq-journal-stats.db");
        let db = fixture(&path);
        let app = Router::new().nest("/api/v1", routes()).with_state(db);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/journal/stats?mode=demo")
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
        assert_eq!(body["total"], 3, "实盘记录不能混进模拟盘");
        assert_eq!(body["correct"], 1);
        assert_eq!(body["wrong"], 1);
        assert_eq!(body["partial"], 1);
        assert_eq!(body["accuracy"], 50.0, "partial 不进分母");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn search_drops_experiences_below_the_similarity_floor() {
        let path = std::env::temp_dir().join("nq-journal-search.db");
        let db = fixture(&path);
        let app = Router::new().nest("/api/v1", routes()).with_state(db);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/journal/experiences/search?q=%E6%94%BE%E9%87%8F%E7%AA%81%E7%A0%B4%E8%BF%BD%E5%A4%9A")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        let items = body["items"].as_array().unwrap();
        assert_eq!(items.len(), 1, "横盘那条相似度低于 0.5，不该返回");
        assert_eq!(items[0]["symbol"], "BTCUSDT");
        assert_eq!(items[0]["outcome"], "win");
        assert_eq!(items[0]["hit_rate"], 1.0);
        assert!(items[0]["similarity"].as_f64().unwrap() >= 0.5);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn experiences_are_listed_from_the_real_schema_and_filtered() {
        let path = std::env::temp_dir().join("nq-journal-experiences.db");
        let db = fixture(&path);
        let app = Router::new().nest("/api/v1", routes()).with_state(db);
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/journal/experiences?symbol=BTCUSDT")
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
        assert_eq!(items.len(), 1, "{body}");
        assert_eq!(items[0]["lesson"], "放量突破追多，突破要等回踩");
        assert_eq!(items[0]["wins"], 2);
        assert_eq!(items[0]["hit_rate"], 1.0);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/journal/experiences/stats")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["total"], 2);
        assert_eq!(body["by_outcome"]["win"], 1);
        assert_eq!(body["by_symbol"]["BTCUSDT"], 1);
        assert_eq!(body["avg_hit_rate"], 0.5, "1.0 与 0.0 的平均");
        let _ = std::fs::remove_file(&path);
    }
}
