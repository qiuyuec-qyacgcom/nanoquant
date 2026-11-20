//! 决策日志只读接口。列表与统计对齐 `journal/record.py`。

use axum::extract::{Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::db::Db;

pub fn routes() -> Router<Db> {
    Router::new()
        .route("/journal/archive", post(journal_archive))
        .route("/journal/label", post(label_pending_handler))
        .route("/journal/retrieve", get(retrieve_preview))
}

/// 手动触发 24h 回访打标。价格取当前交易环境快照，另一环境的 pending 保持原状。
async fn label_pending_handler(State(db): State<Db>) -> Json<Value> {
    let (mode, prices) = {
        let conn = db.conn.lock().expect("db");
        let mode = current_mode(&conn);
        (mode.clone(), prices_from_state(&conn, &mode))
    };
    let result = label_due(&db, &prices, &mode, crate::now());
    record_label_event(&db, &result);
    result.into()
}

pub(crate) fn current_mode(conn: &Connection) -> String {
    conn.query_row(
        "SELECT value FROM engine_state WHERE key = 'mode'",
        [],
        |row| row.get::<_, String>(0),
    )
    .ok()
    .filter(|mode| mode == "live" || mode == "demo")
    .unwrap_or_else(|| "demo".to_string())
}

/// 从周期落库的快照取同环境标记价；快照缺失时返回空表（保持 pending）。
pub(crate) fn prices_from_state(
    conn: &Connection,
    mode: &str,
) -> std::collections::BTreeMap<String, f64> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM engine_state WHERE key = ?1",
            [format!("snapshot.{mode}")],
            |row| row.get(0),
        )
        .ok();
    let mut prices = std::collections::BTreeMap::new();
    let Some(raw) = raw else {
        return prices;
    };
    let snapshot: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    for market in snapshot["markets"].as_array().into_iter().flatten() {
        let (Some(symbol), Some(price)) = (
            market["symbol"].as_str(),
            market["mark_price"].as_f64().filter(|price| *price > 0.0),
        ) else {
            continue;
        };
        prices.insert(symbol.to_string(), price);
    }
    prices
}

fn label_config(conn: &Connection) -> (bool, i64, f64) {
    let raw = conn
        .query_row(
            "SELECT value FROM app_settings WHERE key = 'journal'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .unwrap_or(Value::Null);
    (
        raw.get("enabled").and_then(Value::as_bool).unwrap_or(true),
        raw.get("label_after_sec")
            .and_then(Value::as_i64)
            .unwrap_or(86_400)
            .clamp(60, 30 * 86_400),
        raw.get("correct_threshold_pct")
            .and_then(Value::as_f64)
            .unwrap_or(0.15)
            .clamp(0.01, 50.0),
    )
}

pub(crate) fn signed_actual_pct(direction: &str, entry_price: f64, actual_price: f64) -> f64 {
    if !entry_price.is_finite() || entry_price <= 0.0 || !actual_price.is_finite() {
        return 0.0;
    }
    let raw = (actual_price - entry_price) / entry_price * 100.0;
    if direction == "long" {
        raw
    } else {
        -raw
    }
}

pub(crate) fn outcome_of(signed_pct: f64, threshold: f64) -> &'static str {
    if signed_pct >= threshold {
        "correct"
    } else if signed_pct <= -threshold {
        "wrong"
    } else {
        "partial"
    }
}

/// 回访所有到期的 pending 方向性决策。只标当前环境，价格缺失的保持 pending。
pub(crate) fn label_due(
    db: &Db,
    prices: &std::collections::BTreeMap<String, f64>,
    mode: &str,
    now: i64,
) -> Value {
    let conn = db.conn.lock().expect("db");
    let (enabled, after, threshold) = label_config(&conn);
    if !enabled {
        return json!({"ok": false, "error": "决策日志已关闭", "labeled": 0});
    }
    let cutoff = now - after;
    let mut stmt = conn
        .prepare(
            "SELECT id, symbol, direction, entry_price, mode FROM decision_journal \
             WHERE status = 'pending' AND direction IN ('long','short') AND ts <= ?1",
        )
        .expect("prepare label");
    let rows: Vec<(i64, String, String, f64, String)> = stmt
        .query_map([cutoff], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get::<_, Option<String>>(4)?.unwrap_or_default(),
            ))
        })
        .expect("query label")
        .collect::<Result<_, _>>()
        .expect("label rows");
    let due = rows
        .iter()
        .filter(|(_, _, _, _, row_mode)| row_mode == mode)
        .cloned()
        .collect::<Vec<_>>();
    let deferred = rows.len() - due.len();
    let mut labeled = 0;
    let mut missing = 0;
    for (id, symbol, direction, entry_price, _) in due {
        let Some(price) = prices.get(&symbol).copied().filter(|price| *price > 0.0) else {
            missing += 1;
            continue;
        };
        let signed = signed_actual_pct(&direction, entry_price, price);
        let outcome = outcome_of(signed, threshold);
        conn.execute(
            "UPDATE decision_journal SET status='labeled', outcome=?1, actual_price=?2, \
             actual_pct=?3, labeled_at=?4 WHERE id=?5",
            (
                outcome,
                price,
                (signed * 10_000.0).round() / 10_000.0,
                now,
                id,
            ),
        )
        .expect("update label");
        labeled += 1;
    }
    json!({
        "ok": true,
        "labeled": labeled,
        "due": labeled + missing,
        "deferred": deferred,
        "missing_price": missing,
        "threshold_pct": threshold,
        "mode": mode,
    })
}

fn record_label_event(db: &Db, result: &Value) {
    if let Ok(conn) = db.conn.lock() {
        let _ = conn.execute(
            "INSERT INTO engine_events(ts, level, category, message, payload) \
             VALUES(?1, 'info', 'journal', ?2, ?3)",
            (
                crate::now(),
                format!(
                    "决策打标: {} 条 (另一环境 {} 条待切换)",
                    result["labeled"].as_i64().unwrap_or(0),
                    result["deferred"].as_i64().unwrap_or(0)
                ),
                serde_json::to_string(result).unwrap_or_else(|_| "{}".to_string()),
            ),
        );
    }
}

/// 开仓时写决策快照，供 24h 回访打标与经验沉淀。
#[allow(clippy::too_many_arguments)]
pub(crate) fn record_decision(
    conn: &Connection,
    mode: &str,
    symbol: &str,
    direction: &str,
    confidence: f64,
    entry_price: f64,
    target_pct: f64,
    reason: &str,
    snapshot: &Value,
) -> Option<i64> {
    if !matches!(direction, "long" | "short")
        || !entry_price.is_finite()
        || entry_price <= 0.0
        || symbol.is_empty()
    {
        return None;
    }
    conn.execute(
        "INSERT INTO decision_journal(ts, symbol, direction, confidence, entry_price, target_pct, \
         reason, snapshot, decision, signal_id, injected_experience_ids, status, outcome, \
         actual_price, actual_pct, labeled_at, archived, mode) \
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,0,'[]','pending','',0,0,0,0,?10)",
        (
            crate::now(),
            symbol,
            direction,
            confidence.clamp(0.0, 1.0),
            entry_price,
            target_pct,
            reason.chars().take(900).collect::<String>(),
            serde_json::to_string(snapshot).unwrap_or_else(|_| "{}".to_string()),
            format!("open_{direction}"),
            mode,
        ),
    )
    .ok()
    .map(|_| conn.last_insert_rowid())
}

#[derive(Deserialize)]
struct RetrieveQuery {
    symbol: String,
    rsi: Option<f64>,
    adx: Option<f64>,
    atr_pct: Option<f64>,
    #[serde(default)]
    direction: String,
}

type JournalError = (axum::http::StatusCode, Json<Value>);

async fn retrieve_preview(
    State(db): State<Db>,
    Query(query): Query<RetrieveQuery>,
) -> Result<Json<Value>, JournalError> {
    for (field, value, max) in [
        ("rsi", query.rsi, 100.0),
        ("adx", query.adx, 100.0),
        ("atr_pct", query.atr_pct, 50.0),
    ] {
        if value.is_some_and(|value| !(0.0..=max).contains(&value)) {
            return Err((
                axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({"detail": format!("{field} 必须在 0 到 {max} 之间")})),
            ));
        }
    }
    let direction = match query.direction.as_str() {
        direction @ ("long" | "short") => direction,
        _ => "",
    };
    let snapshot = json!({"rsi": query.rsi, "adx": query.adx, "atr_pct": query.atr_pct});
    let scenario = derive_scenario(&snapshot);
    let tags = derive_tags(&query.symbol, &snapshot, direction);
    let (limit, inject, threshold) = journal_config(&db);
    let items = if !inject {
        Vec::new()
    } else {
        let conn = db.conn.lock().expect("db");
        ranked_experiences(
            &conn,
            &query.symbol,
            &scenario,
            &tags,
            direction,
            limit,
            threshold,
        )
    };
    Ok(Json(
        json!({"symbol": query.symbol, "direction": direction, "tags": tags, "scenario": scenario, "items": items}),
    ))
}

fn journal_config(db: &Db) -> (usize, bool, f64) {
    let config = db.setting("journal").ok().flatten().unwrap_or(Value::Null);
    let limit = config["retrieve_top_k"]
        .as_i64()
        .unwrap_or(5)
        .clamp(0, 2000) as usize;
    let inject = config["inject_experiences"].as_bool().unwrap_or(true);
    let threshold = config["min_similarity"].as_f64().unwrap_or(0.15);
    (limit, inject, threshold)
}

/// 决策注入：与当前场景最相似的经验 ID。检索失败/关闭时返回空，不阻断决策。
pub(crate) fn inject_ids(
    conn: &rusqlite::Connection,
    symbol: &str,
    snapshot: &Value,
    direction: &str,
) -> Vec<i64> {
    let config = conn
        .query_row(
            "SELECT value FROM app_settings WHERE key = 'journal'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .unwrap_or(Value::Null);
    let limit = config["retrieve_top_k"]
        .as_i64()
        .unwrap_or(5)
        .clamp(0, 2000) as usize;
    if !config["inject_experiences"].as_bool().unwrap_or(true) || limit == 0 {
        return Vec::new();
    }
    let threshold = config["min_similarity"].as_f64().unwrap_or(0.15);
    let tags = derive_tags(symbol, snapshot, direction);
    let scenario = derive_scenario(snapshot);
    ranked_experiences(conn, symbol, &scenario, &tags, direction, limit, threshold)
        .into_iter()
        .filter_map(|item| item["id"].as_i64())
        .collect()
}

fn ranked_experiences(
    conn: &rusqlite::Connection,
    symbol: &str,
    scenario: &Value,
    tags: &[String],
    direction: &str,
    limit: usize,
    threshold: f64,
) -> Vec<Value> {
    if limit == 0 {
        return Vec::new();
    }
    let mut stmt = conn
        .prepare(
            "SELECT id, symbol, tags, scenario, lesson, outcome, source_decision_id, source, \
             hits, wins, losses, enabled, created_at, updated_at FROM experiences \
             WHERE enabled = 1 AND symbol = ?1 ORDER BY id DESC LIMIT 2000",
        )
        .expect("prepare experiences");
    let rows = stmt
        .query_map([symbol], |row| {
            let tags: Option<String> = row.get(2)?;
            let scenario: Option<String> = row.get(3)?;
            Ok(json!({
                "id": row.get::<_, i64>(0)?, "symbol": row.get::<_, String>(1)?,
                "tags": tags.and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok()).unwrap_or_default(),
                "scenario": scenario.and_then(|raw| serde_json::from_str::<serde_json::Map<String, Value>>(&raw).ok()).unwrap_or_default(),
                "lesson": row.get::<_, String>(4)?, "outcome": row.get::<_, String>(5)?,
                "source_decision_id": row.get::<_, Option<i64>>(6)?, "source": row.get::<_, String>(7)?,
                "hits": row.get::<_, i64>(8)?, "wins": row.get::<_, i64>(9)?,
                "losses": row.get::<_, i64>(10)?, "enabled": row.get::<_, bool>(11)?,
                "created_at": row.get::<_, i64>(12)?, "updated_at": row.get::<_, i64>(13)?,
            }))
        })
        .expect("query experiences");
    let mut ranked = Vec::new();
    for row in rows {
        let Ok(item) = row else { continue };
        let score = scenario_similarity(symbol, scenario, tags, direction, &item);
        if score >= threshold {
            ranked.push((score, item));
        }
    }
    ranked.sort_by(|(left_score, left), (right_score, right)| {
        right_score
            .total_cmp(left_score)
            .then_with(|| left["id"].as_i64().cmp(&right["id"].as_i64()))
    });
    ranked
        .into_iter()
        .take(limit)
        .map(|(_, item)| item)
        .collect()
}

fn retrieve_error(error: impl std::fmt::Display) -> JournalError {
    tracing::warn!(%error, "读取场景经验失败");
    (
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"detail": "读取场景经验失败"})),
    )
}

fn scenario_similarity(
    symbol: &str,
    scenario: &Value,
    tags: &[String],
    direction: &str,
    item: &Value,
) -> f64 {
    use std::collections::HashSet;

    let query_tags: HashSet<&str> = tags.iter().map(String::as_str).collect();
    let item_tags: Vec<&str> = item["tags"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let stored_tags: HashSet<&str> = item_tags.iter().copied().collect();
    let mut score = if item["symbol"] == symbol { 0.35 } else { 0.0 };
    let union = query_tags.union(&stored_tags).count();
    if union > 0 {
        score += 0.45 * query_tags.intersection(&stored_tags).count() as f64 / union as f64;
    }
    let mut total = 0.0;
    let mut count = 0;
    for (key, spread) in [
        ("rsi", 70.0),
        ("adx", 60.0),
        ("atr_pct", 5.0),
        ("funding", 0.5),
    ] {
        let stored = &item["scenario"][key];
        let stored = stored
            .as_f64()
            .or_else(|| stored.as_str()?.parse::<f64>().ok());
        if let (Some(a), Some(b)) = (
            scenario[key].as_f64(),
            stored.filter(|value| value.is_finite()),
        ) {
            total += 1.0 - ((a - b).abs() / spread).min(1.0);
            count += 1;
        }
    }
    if count > 0 {
        score += 0.2 * total / count as f64;
    }
    if !direction.is_empty() {
        match item_tags.iter().find_map(|tag| tag.strip_prefix("dir:")) {
            Some(stored) if stored == direction => score += 0.08,
            Some("long" | "short") => score -= 0.05,
            _ => {}
        }
    }
    (score.clamp(0.0, 1.0) * 10_000.0).round_ties_even() / 10_000.0
}

/// 把已打标且未归档的决策沉淀为经验。正确算赢、错误算输，部分正确不归档。
/// 同币种、同结论、标签完全一致的并入已有条目，避免经验库被同一场景反复刷满。
async fn journal_archive(State(db): State<Db>) -> Json<Value> {
    let conn = db.conn.lock().expect("db");
    let mut stmt = conn
        .prepare(
            "SELECT id, symbol, direction, confidence, outcome, snapshot, actual_pct, target_pct \
             FROM decision_journal WHERE status = 'labeled' AND archived = 0 AND outcome IN ('correct','wrong') \
             ORDER BY labeled_at ASC LIMIT 200",
        )
        .expect("prepare due");
    let due: Vec<(i64, String, String, f64, String, String, f64, f64)> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
            ))
        })
        .expect("query due")
        .collect::<Result<_, _>>()
        .expect("due rows");
    drop(stmt);
    let now = crate::now();
    let mut created = 0;
    let mut merged = 0;
    for (id, symbol, direction, confidence, outcome, snapshot, actual_pct, target_pct) in due {
        let snap: Value = serde_json::from_str(&snapshot).unwrap_or(Value::Null);
        let tags = derive_tags(&symbol, &snap, &direction);
        let verdict = if outcome == "correct" { "win" } else { "loss" };
        let existing = find_same(&conn, &symbol, verdict, &tags);
        if let Some(exp_id) = existing {
            let column = if verdict == "win" { "wins" } else { "losses" };
            conn.execute(
                &format!("UPDATE experiences SET hits = hits + 1, {column} = {column} + 1, updated_at = ?1 WHERE id = ?2"),
                (now, exp_id),
            )
            .expect("merge experience");
            merged += 1;
        } else {
            conn.execute(
                "INSERT INTO experiences(symbol, tags, scenario, lesson, outcome, source_decision_id, source, \
                 hits, wins, losses, enabled, created_at, updated_at) VALUES(?1,?2,?3,?4,?5,?6,'journal',1,?7,?8,1,?9,?9)",
                (
                    &symbol,
                    serde_json::to_string(&tags).unwrap_or_else(|_| "[]".to_string()),
                    derive_scenario(&snap).to_string(),
                    lesson_text(&symbol, &snap, &direction, &outcome, actual_pct, target_pct, confidence),
                    verdict,
                    id,
                    i64::from(verdict == "win"),
                    i64::from(verdict == "loss"),
                    now,
                ),
            )
            .expect("insert experience");
            created += 1;
        }
        conn.execute(
            "UPDATE decision_journal SET archived = 1 WHERE id = ?1",
            [id],
        )
        .expect("mark archived");
    }
    let capacity = experience_capacity(&conn);
    let pruned = prune_experiences(&conn, capacity);
    json!({ "ok": true, "archived": created, "merged": merged, "pruned": pruned }).into()
}

fn experience_capacity(conn: &Connection) -> usize {
    conn.query_row(
        "SELECT value FROM app_settings WHERE key = 'journal'",
        [],
        |row| row.get::<_, String>(0),
    )
    .ok()
    .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
    .and_then(|value| value.get("experience_capacity").and_then(Value::as_i64))
    .unwrap_or(200)
    .clamp(1, 5_000) as usize
}

/// 超容量按健康度（胜率，样本不足取先验 0.5）与时间淘汰；手动停用的最先出局。
pub(crate) fn prune_experiences(conn: &Connection, capacity: usize) -> usize {
    if capacity == 0 {
        return 0;
    }
    let mut stmt = conn
        .prepare("SELECT id, wins, losses, enabled, updated_at, created_at FROM experiences")
        .expect("prepare prune");
    let rows: Vec<(i64, i64, i64, i64, i64, i64)> = stmt
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        })
        .expect("query prune")
        .collect::<Result<_, _>>()
        .expect("prune rows");
    if rows.len() <= capacity {
        return 0;
    }
    let mut scored: Vec<(f64, i64, i64)> = rows
        .iter()
        .map(|(id, wins, losses, enabled, updated, created)| {
            let judged = wins + losses;
            let health = if judged <= 0 {
                0.5
            } else {
                *wins as f64 / judged as f64
            };
            (
                if *enabled != 0 { health } else { -1.0 },
                (*updated).max(*created),
                *id,
            )
        })
        .collect();
    scored.sort_by(|left, right| {
        left.0
            .total_cmp(&right.0)
            .then_with(|| left.1.cmp(&right.1))
            .then_with(|| left.2.cmp(&right.2))
    });
    let excess = scored.len() - capacity;
    for (_, _, id) in scored.iter().take(excess) {
        let _ = conn.execute("DELETE FROM experiences WHERE id = ?1", [id]);
    }
    excess
}

fn find_same(
    conn: &rusqlite::Connection,
    symbol: &str,
    outcome: &str,
    tags: &[String],
) -> Option<i64> {
    let mut stmt = conn
        .prepare(
            "SELECT id, tags FROM experiences WHERE enabled = 1 AND symbol = ?1 AND outcome = ?2",
        )
        .expect("prepare dup");
    let rows: Vec<(i64, String)> = stmt
        .query_map((symbol, outcome), |r| Ok((r.get(0)?, r.get(1)?)))
        .expect("query dup")
        .collect::<Result<_, _>>()
        .expect("dup rows");
    let wanted: std::collections::BTreeSet<&str> = tags.iter().map(String::as_str).collect();
    rows.into_iter()
        .find(|(_, raw)| {
            let parsed: Vec<String> = serde_json::from_str(raw).unwrap_or_default();
            parsed
                .iter()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>()
                == wanted
        })
        .map(|(id, _)| id)
}

fn derive_tags(symbol: &str, snapshot: &Value, direction: &str) -> Vec<String> {
    let mut tags = vec![format!("sym:{symbol}")];
    if let Some(rsi) = snapshot.get("rsi").and_then(|v| v.as_f64()) {
        tags.push(
            if rsi >= 70.0 {
                "rsi_overbought"
            } else if rsi <= 30.0 {
                "rsi_oversold"
            } else {
                "rsi_neutral"
            }
            .to_string(),
        );
    }
    if let Some(adx) = snapshot.get("adx").and_then(|v| v.as_f64()) {
        tags.push(if adx >= 25.0 { "adx_trend" } else { "adx_chop" }.to_string());
    }
    if direction == "long" || direction == "short" {
        tags.push(format!("dir:{direction}"));
    }
    tags
}

fn derive_scenario(snapshot: &Value) -> Value {
    let mut out = serde_json::Map::new();
    for key in ["rsi", "adx", "atr_pct", "funding"] {
        if let Some(v) = snapshot.get(key).and_then(|v| v.as_f64()) {
            out.insert(key.to_string(), json!(v));
        }
    }
    Value::Object(out)
}

fn lesson_text(
    symbol: &str,
    snapshot: &Value,
    direction: &str,
    outcome: &str,
    actual_pct: f64,
    target_pct: f64,
    confidence: f64,
) -> String {
    let mut scene = Vec::new();
    if let Some(rsi) = snapshot.get("rsi").and_then(|v| v.as_f64()) {
        scene.push(if rsi <= 30.0 {
            "超卖"
        } else if rsi >= 70.0 {
            "超买"
        } else {
            "中性"
        });
    }
    if let Some(adx) = snapshot.get("adx").and_then(|v| v.as_f64()) {
        scene.push(if adx >= 25.0 { "趋势" } else { "震荡" });
    }
    let ctx = if scene.is_empty() {
        String::new()
    } else {
        format!("|{}", scene.join("|"))
    };
    let side = if direction == "long" { "L" } else { "S" };
    let verdict = if outcome == "correct" { "✓" } else { "✗" };
    format!("{symbol}{ctx} {side}{verdict} Δ{actual_pct:+.2}% →{target_pct:+.2}% c{confidence:.2}")
}

fn count_stats(
    conn: &rusqlite::Connection,
    where_sql: &str,
    args: &[Box<dyn rusqlite::types::ToSql>],
) -> Value {
    let sql = format!("SELECT status, outcome FROM decision_journal {where_sql}");
    let mut stmt = conn.prepare(&sql).expect("prepare stats");
    let rows: Vec<(String, String)> = stmt
        .query_map(rusqlite::params_from_iter(args.iter()), |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .expect("stats query")
        .collect::<Result<_, _>>()
        .expect("stats rows");
    let total = rows.len();
    let pending = rows.iter().filter(|(s, _)| s == "pending").count();
    let labeled = rows.iter().filter(|(s, _)| s == "labeled").count();
    let correct = rows.iter().filter(|(_, o)| o == "correct").count();
    let wrong = rows.iter().filter(|(_, o)| o == "wrong").count();
    let partial = rows.iter().filter(|(_, o)| o == "partial").count();
    let judged = correct + wrong;
    let accuracy = if judged == 0 {
        0.0
    } else {
        (correct as f64 / judged as f64 * 10000.0).round() / 100.0
    };
    json!({ "total": total, "pending": pending, "labeled": labeled, "correct": correct, "wrong": wrong, "partial": partial, "accuracy": accuracy })
}

pub fn seed(path: &std::path::Path) -> Db {
    let _ = std::fs::remove_file(path);
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE app_settings(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER); \
         CREATE TABLE decision_journal(id INTEGER PRIMARY KEY, ts INTEGER, symbol TEXT, direction TEXT, \
         confidence REAL, decision TEXT, status TEXT, outcome TEXT, mode TEXT, snapshot TEXT, \
         actual_pct REAL, target_pct REAL, archived INTEGER, labeled_at INTEGER); \
         CREATE TABLE experiences(id INTEGER PRIMARY KEY, symbol TEXT, tags TEXT, scenario TEXT, lesson TEXT, \
         outcome TEXT, source_decision_id INTEGER, source TEXT, hits INTEGER, wins INTEGER, losses INTEGER, \
         enabled INTEGER, created_at INTEGER, updated_at INTEGER); \
         INSERT INTO decision_journal VALUES(1,100,'BTCUSDT','long',0.8,'open_long','labeled','correct','live','{\"rsi\":25,\"adx\":15}',2.0,3.0,0,150); \
         INSERT INTO decision_journal VALUES(2,300,'BTCUSDT','short',0.6,'open_short','labeled','wrong','live','{}',-1.5,3.0,0,350); \
         INSERT INTO decision_journal VALUES(3,200,'ETHUSDT','long',0.4,'open_long','labeled','partial','live','{}',0.2,3.0,0,250); \
         INSERT INTO decision_journal VALUES(4,400,'BTCUSDT','long',0.9,'open_long','pending','','demo','{}',0,3.0,0,0);",
    )
    .unwrap();
    drop(conn);
    Db::open(path.to_str().unwrap()).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn retrieve_ranks_archived_experiences_by_scenario_and_direction() {
        let path = std::env::temp_dir().join("nq-journal-retrieve.db");
        let db = seed(&path);
        let _ = journal_archive(State(db.clone())).await;
        let app = Router::new().nest("/api/v1", routes()).with_state(db);
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/journal/retrieve?symbol=BTCUSDT&rsi=25&adx=15&direction=long")
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
        assert_eq!(body["scenario"], json!({"rsi": 25.0, "adx": 15.0}));
        assert_eq!(body["items"].as_array().unwrap().len(), 2);
        assert_eq!(body["items"][0]["outcome"], "win");
        assert_eq!(body["items"][1]["outcome"], "loss");
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/journal/retrieve?symbol=BTCUSDT&rsi=101")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn archiving_turns_a_correct_decision_into_a_win() {
        let path = std::env::temp_dir().join("nq-journal-archive.db");
        let db = seed(&path);
        let app = Router::new().nest("/api/v1", routes()).with_state(db);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/journal/archive")
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
        assert_eq!(
            body["archived"], 2,
            "正确与错误各沉淀一条，部分正确不归档: {body}"
        );
        assert_eq!(body["merged"], 0);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn the_same_scenario_merges_instead_of_duplicating() {
        let path = std::env::temp_dir().join("nq-journal-merge.db");

        let db = seed(&path);
        db.conn.lock().unwrap().execute(
            "INSERT INTO decision_journal VALUES(5,500,'BTCUSDT','long',0.7,'open_long','labeled','correct','live','{\"rsi\":25,\"adx\":15}',1.2,3.0,0,550)",
            [],
        ).unwrap();
        let app = Router::new().nest("/api/v1", routes()).with_state(db);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/journal/archive")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            body["archived"], 2,
            "同币种同结论同标签的第二条应并入第一条: {body}"
        );
        assert_eq!(body["merged"], 1);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn inject_ids_returns_similar_experiences_and_respects_the_switch() {
        let path = std::env::temp_dir().join("nq-journal-inject.db");
        let db = seed(&path);
        let _ = journal_archive(State(db.clone())).await;
        let ids = {
            let conn = db.conn.lock().unwrap();
            inject_ids(&conn, "BTCUSDT", &json!({"rsi":25,"adx":15}), "long")
        };
        assert!(ids.contains(&1), "同场景的赢家经验要注入: {ids:?}");
        {
            let conn = db.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO app_settings VALUES('journal','{\"inject_experiences\":false}',0)",
                [],
            )
            .unwrap();
        }
        let ids = {
            let conn = db.conn.lock().unwrap();
            inject_ids(&conn, "BTCUSDT", &json!({"rsi":25,"adx":15}), "long")
        };
        assert!(ids.is_empty(), "关闭注入后不得再检索");
        let _ = std::fs::remove_file(&path);
    }

    fn label_db(path: &std::path::Path) -> Db {
        let _ = std::fs::remove_file(path);
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE app_settings(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER); \
             CREATE TABLE engine_state(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER); \
             CREATE TABLE engine_events(id INTEGER PRIMARY KEY, ts INTEGER, level TEXT, category TEXT, message TEXT, payload TEXT); \
             CREATE TABLE decision_journal(id INTEGER PRIMARY KEY, ts INTEGER, symbol TEXT, direction TEXT, \
             confidence REAL, entry_price REAL, target_pct REAL, reason TEXT, snapshot TEXT, decision TEXT, \
             signal_id INTEGER, injected_experience_ids TEXT, status TEXT, outcome TEXT, actual_price REAL, \
             actual_pct REAL, labeled_at INTEGER, archived INTEGER, mode TEXT); \
             INSERT INTO app_settings VALUES('journal','{\"label_after_sec\":86400,\"correct_threshold_pct\":0.15}',0); \
             INSERT INTO engine_state VALUES('mode','demo',0);",
        )
        .unwrap();
        drop(conn);
        Db::open(path.to_str().unwrap()).unwrap()
    }

    fn insert_pending(conn: &Connection, id: i64, symbol: &str, entry: f64, mode: &str, ts: i64) {
        conn.execute(
            "INSERT INTO decision_journal(id,ts,symbol,direction,confidence,entry_price,target_pct,reason,\
             snapshot,decision,signal_id,injected_experience_ids,status,outcome,actual_price,actual_pct,\
             labeled_at,archived,mode) \
             VALUES(?1,?2,?3,'long',0.8,?4,1,'r','{}','open_long',0,'[]','pending','',0,0,0,0,?5)",
            (id, ts, symbol, entry, mode),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn labeling_is_environment_isolated_and_missing_price_stays_pending() {
        let path = std::env::temp_dir().join("nq-journal-label.db");
        let db = label_db(&path);
        let now = crate::now();
        {
            let conn = db.conn.lock().unwrap();
            insert_pending(&conn, 1, "BTCUSDT", 100.0, "demo", now - 90_000);
            insert_pending(&conn, 2, "BTCUSDT", 100.0, "live", now - 90_000);
            insert_pending(&conn, 3, "ETHUSDT", 200.0, "demo", now - 90_000);
            insert_pending(&conn, 4, "BTCUSDT", 100.0, "demo", now - 100);
            let snapshot = json!({"markets": [{"symbol": "BTCUSDT", "mark_price": 100.5}]});
            conn.execute(
                "INSERT INTO engine_state VALUES('snapshot.demo',?1,0)",
                [serde_json::to_string(&snapshot).unwrap()],
            )
            .unwrap();
        }
        let mut prices = std::collections::BTreeMap::new();
        prices.insert("BTCUSDT".to_string(), 100.5);
        let result = label_due(&db, &prices, "demo", now);
        assert_eq!(result["labeled"], 1, "{result}");
        assert_eq!(result["deferred"], 1, "live 快照不得被 demo 价格打标");
        assert_eq!(result["missing_price"], 1, "无行情的保持 pending");
        {
            let conn = db.conn.lock().unwrap();
            let (outcome, status, actual_pct): (String, String, f64) = conn
                .query_row(
                    "SELECT outcome, status, actual_pct FROM decision_journal WHERE id=1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap();
            assert_eq!(outcome, "correct");
            assert_eq!(status, "labeled");
            assert!((actual_pct - 0.5).abs() < 1e-6);
            for id in [2, 3, 4] {
                let status: String = conn
                    .query_row(
                        "SELECT status FROM decision_journal WHERE id=?1",
                        [id],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(status, "pending", "id={id} 不应被打标");
            }
        }
        let app = Router::new()
            .nest("/api/v1", routes())
            .with_state(db.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/journal/label")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "手动打标端点必须已实装");
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["labeled"], 0, "已打标/缺价的行不会被重复计数: {body}");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn raising_the_threshold_turns_a_correct_label_into_partial() {
        let path = std::env::temp_dir().join("nq-journal-threshold.db");
        let db = label_db(&path);
        let now = crate::now();
        let mut prices = std::collections::BTreeMap::new();
        prices.insert("BTCUSDT".to_string(), 100.5);
        {
            let conn = db.conn.lock().unwrap();
            insert_pending(&conn, 1, "BTCUSDT", 100.0, "demo", now - 90_000);
        }
        let first = label_due(&db, &prices, "demo", now);
        assert_eq!(first["labeled"], 1);
        {
            let conn = db.conn.lock().unwrap();
            conn.execute(
                "UPDATE app_settings SET value='{\"label_after_sec\":86400,\"correct_threshold_pct\":1.0}' \
                 WHERE key='journal'",
                [],
            )
            .unwrap();
            insert_pending(&conn, 2, "BTCUSDT", 100.0, "demo", now - 90_000);
        }
        let second = label_due(&db, &prices, "demo", now);
        assert_eq!(second["labeled"], 1);
        {
            let conn = db.conn.lock().unwrap();
            let first_outcome: String = conn
                .query_row(
                    "SELECT outcome FROM decision_journal WHERE id=1",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            let second_outcome: String = conn
                .query_row(
                    "SELECT outcome FROM decision_journal WHERE id=2",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(first_outcome, "correct", "0.5% 超过 0.15% 阈值");
            assert_eq!(second_outcome, "partial", "1.0% 阈值下 0.5% 只能算部分正确");
        }
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn raising_similarity_threshold_reduces_injected_experiences() {
        let path = std::env::temp_dir().join("nq-journal-inject-threshold.db");
        let db = seed(&path);
        {
            let conn = db.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO experiences VALUES(1,'BTCUSDT','[\"sym:BTCUSDT\",\"dir:long\"]',\
                 '{\"rsi\":25,\"adx\":15}','突破回踩开多','win',0,'manual',1,1,0,1,100,100)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO experiences VALUES(2,'BTCUSDT','[\"sym:BTCUSDT\",\"dir:long\"]','{}',\
                 '完全不同主题的规则文本','loss',0,'manual',1,0,1,1,90,90)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO app_settings VALUES('journal',\
                 '{\"inject_experiences\":true,\"retrieve_top_k\":5,\"min_similarity\":0.7}',0)",
                [],
            )
            .unwrap();
        }
        let snapshot = json!({"rsi":25,"adx":15});
        let strict = {
            let conn = db.conn.lock().unwrap();
            inject_ids(&conn, "BTCUSDT", &snapshot, "long")
        };
        assert_eq!(strict, vec![1], "高阈值只注入同场景且数值接近的那条");
        {
            let conn = db.conn.lock().unwrap();
            conn.execute(
                "UPDATE app_settings SET value=\
                 '{\"inject_experiences\":true,\"retrieve_top_k\":5,\"min_similarity\":0.5}' \
                 WHERE key='journal'",
                [],
            )
            .unwrap();
        }
        let loose = {
            let conn = db.conn.lock().unwrap();
            inject_ids(&conn, "BTCUSDT", &snapshot, "long")
        };
        assert_eq!(loose.len(), 2, "调低阈值后注入条数上升: {loose:?}");
        assert!(loose.contains(&1));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn capacity_prunes_the_lowest_health_then_oldest_and_is_idempotent() {
        let path = std::env::temp_dir().join("nq-journal-capacity.db");
        let db = seed(&path);
        {
            let conn = db.conn.lock().unwrap();
            conn.execute_batch(
                "DELETE FROM experiences; \
                 INSERT INTO experiences VALUES(1,'BTCUSDT','[]','{}','A','win',0,'manual',3,3,0,1,400,400); \
                 INSERT INTO experiences VALUES(2,'BTCUSDT','[]','{}','B','win',0,'manual',2,1,1,1,300,300); \
                 INSERT INTO experiences VALUES(3,'BTCUSDT','[]','{}','C','loss',0,'manual',3,0,3,1,200,200); \
                 INSERT INTO experiences VALUES(4,'BTCUSDT','[]','{}','D','win',0,'manual',1,1,0,1,100,100);",
            )
            .unwrap();
            assert_eq!(prune_experiences(&conn, 2), 2, "超额两条按健康度淘汰");
            let kept: Vec<i64> = {
                let mut stmt = conn
                    .prepare("SELECT id FROM experiences ORDER BY id")
                    .unwrap();
                stmt.query_map([], |row| row.get(0))
                    .unwrap()
                    .collect::<Result<_, _>>()
                    .unwrap()
            };
            assert_eq!(kept, vec![1, 4], "保留胜率最高、时间最新的两条");
            assert_eq!(prune_experiences(&conn, 2), 0, "再次清理无变化");
        }
        let _ = std::fs::remove_file(&path);
    }
}
