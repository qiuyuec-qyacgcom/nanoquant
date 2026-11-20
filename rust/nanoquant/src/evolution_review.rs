//! 自进化复盘：批量复盘、全局复盘、健康分重算与效果对照。
//!
//! 对齐 legacy 内核（已退役） `evolution/review.py` 的语义：攒够样本才复盘；AI 失败或模型给不出
//! 有效提案时只输出「本地台账推导」的事实，恒 `NO_CHANGE`，绝不回填规则心法。

use std::collections::BTreeMap;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::ai_client::{self, ChatCall, ChatMessage};
use crate::ai_providers::{provider_chain, AiState};

use crate::evolution_api::{
    audit, evolution_cfg, lesson_similarity, load_mantras, round_n, sanitize, write_cfg,
    EvolutionCfg, MantraRow, WriteCfg,
};

const DETERMINISTIC_PREFIX: &str = "[本地台账推导]";
const MIN_STRUCTURAL_SAMPLE: i64 = 10;
const REPLACE_SIM: f64 = 0.62;
const MERGE_WINDOW_SEC: i64 = 6 * 3600;

pub fn routes() -> Router<AiState> {
    Router::new()
        .route("/evolution/review/{trade_id}", post(review_trade))
        .route("/evolution/review-batch", post(review_batch))
        .route("/evolution/health-refresh", post(health_refresh))
        .route("/evolution/global-review", post(global_review))
        .route("/evolution/effectiveness", get(effectiveness))
}

#[derive(Clone)]
struct TradeRow {
    id: i64,
    symbol: String,
    side: String,
    leverage: f64,
    qty: f64,
    entry_price: f64,
    exit_price: f64,
    pnl: f64,
    pnl_pct: f64,
    roi_pct: f64,
    fees: f64,
    hold_seconds: i64,
    exit_reason: String,
    ai_confidence: f64,
    ai_reason: String,
    signal_score: f64,
    mode: String,
    reviewed: bool,
    features: Value,
    mantra_ids: Value,
}

const TRADE_COLUMNS: &str =
    "id, symbol, side, leverage, qty, entry_price, exit_price, pnl, pnl_pct, \
     roi_pct, fees, hold_seconds, exit_reason, ai_confidence, ai_reason, signal_score, mode, \
     reviewed, features, mantra_ids";

fn load_trades(
    conn: &rusqlite::Connection,
    limit: i64,
    only_pending: bool,
    mode: &str,
) -> Vec<TradeRow> {
    let mut sql = format!("SELECT {TRADE_COLUMNS} FROM trades");
    let mut clauses: Vec<&str> = Vec::new();
    if only_pending {
        clauses.push("reviewed = 0");
    }
    if !mode.is_empty() {
        clauses.push("mode = ?");
    }
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    sql.push_str(" ORDER BY closed_at DESC LIMIT ?");
    let Ok(mut statement) = conn.prepare(&sql) else {
        return Vec::new();
    };
    let map = |row: &rusqlite::Row<'_>| -> rusqlite::Result<TradeRow> {
        Ok(TradeRow {
            id: row.get(0)?,
            symbol: row.get(1)?,
            side: row.get(2)?,
            leverage: row.get(3)?,
            qty: row.get(4)?,
            entry_price: row.get(5)?,
            exit_price: row.get(6)?,
            pnl: row.get(7)?,
            pnl_pct: row.get(8)?,
            roi_pct: row.get(9)?,
            fees: row.get(10)?,
            hold_seconds: row.get(11)?,
            exit_reason: row.get(12)?,
            ai_confidence: row.get(13)?,
            ai_reason: row.get(14)?,
            signal_score: row.get(15)?,
            mode: row.get(16)?,
            reviewed: row.get::<_, i64>(17)? != 0,
            features: row
                .get::<_, Option<String>>(18)?
                .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                .unwrap_or_else(|| json!({})),
            mantra_ids: row
                .get::<_, Option<String>>(19)?
                .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                .unwrap_or_else(|| json!([])),
        })
    };
    let rows = if mode.is_empty() {
        statement.query_map([limit], map)
    } else {
        statement.query_map((mode, limit), map)
    };
    match rows {
        Ok(rows) => rows.filter_map(Result::ok).collect(),
        Err(_) => Vec::new(),
    }
}

fn trade_payload(trade: &TradeRow, compact: bool) -> Value {
    let mut payload = json!({
        "symbol": trade.symbol,
        "side": trade.side,
        "leverage": trade.leverage,
        "qty": trade.qty,
        "entry_price": trade.entry_price,
        "exit_price": trade.exit_price,
        "pnl_usdt": round_n(trade.pnl, 6),
        "pnl_pct": round_n(trade.pnl_pct, 4),
        "roi_pct": round_n(trade.roi_pct, 4),
        "fees": round_n(trade.fees, 6),
        "hold_seconds": trade.hold_seconds,
        "exit_reason": trade.exit_reason,
        "signal_score": trade.signal_score,
        "ai_confidence": trade.ai_confidence,
        "ai_reason": trade.ai_reason.chars().take(if compact { 120 } else { 300 }).collect::<String>(),
    });
    if !compact {
        payload["features"] = trade.features.clone();
    }
    payload
}

fn batch_stats(rows: &[TradeRow]) -> Value {
    let total = rows.len() as i64;
    if total == 0 {
        return json!({"sample_size": 0, "do_not_assert": ["台账为空"]});
    }
    let wins = rows.iter().filter(|trade| trade.pnl > 0.0).count() as i64;
    let net: f64 = rows.iter().map(|trade| trade.pnl).sum();
    let fees: f64 = rows.iter().map(|trade| trade.fees).sum();
    let hold: i64 = rows.iter().map(|trade| trade.hold_seconds).sum();
    let mut by_reason: Map<String, Value> = Map::new();
    for trade in rows {
        let key = if trade.exit_reason.is_empty() {
            "unknown".to_string()
        } else {
            trade.exit_reason.clone()
        };
        let cell = by_reason
            .entry(key)
            .or_insert_with(|| json!({"n": 0, "pnl": 0.0, "wins": 0}));
        let n = cell["n"].as_i64().unwrap_or(0) + 1;
        let pnl = round_n(cell["pnl"].as_f64().unwrap_or(0.0) + trade.pnl, 6);
        let wins_cell = cell["wins"].as_i64().unwrap_or(0) + i64::from(trade.pnl > 0.0);
        cell["n"] = json!(n);
        cell["pnl"] = json!(pnl);
        cell["wins"] = json!(wins_cell);
    }
    json!({
        "sample_size": total,
        "win_rate_pct": round_n(wins as f64 / total as f64 * 100.0, 2),
        "net_pnl": round_n(net, 6),
        "fees": round_n(fees, 6),
        "avg_hold_seconds": hold / total,
        "by_reason": Value::Object(by_reason),
        "min_structural_sample": MIN_STRUCTURAL_SAMPLE,
        "do_not_assert": [
            format!("样本 < {MIN_STRUCTURAL_SAMPLE} 笔的分组结论"),
            "PRICE_ONLY / NONE 快照的因子因果归因",
            "单笔极值外推为长期规律",
        ],
    })
}

fn deterministic_insights(rows: &[TradeRow]) -> Vec<String> {
    if rows.is_empty() {
        return vec![format!(
            "{DETERMINISTIC_PREFIX} 台账内没有可观测的已平仓交易，无法形成认知。"
        )];
    }
    let n = rows.len() as i64;
    let wins = rows.iter().filter(|trade| trade.pnl > 0.0).count() as i64;
    let net: f64 = rows.iter().map(|trade| trade.pnl).sum();
    let fees: f64 = rows.iter().map(|trade| trade.fees).sum();
    let mut lines = vec![format!(
        "{DETERMINISTIC_PREFIX} 样本 {n} 笔：胜率 {:.1}%、净盈亏 {:+.4} USDT、手续费 {:.4} USDT。",
        wins as f64 / n as f64 * 100.0,
        net,
        fees
    )];
    if n < MIN_STRUCTURAL_SAMPLE {
        lines.push(format!(
            "{DETERMINISTIC_PREFIX} 样本仅 {n} 笔（< {MIN_STRUCTURAL_SAMPLE} 笔），证据不足，本轮不形成结构性结论。"
        ));
        return lines;
    }
    let mut by_reason: BTreeMap<String, (i64, f64)> = BTreeMap::new();
    for trade in rows {
        let key = if trade.exit_reason.is_empty() {
            "unknown".to_string()
        } else {
            trade.exit_reason.clone()
        };
        let cell = by_reason.entry(key).or_insert((0, 0.0));
        cell.0 += 1;
        cell.1 += trade.pnl;
    }
    let dist = by_reason
        .iter()
        .map(|(reason, (count, pnl))| format!("{reason} {count} 笔/净 {pnl:+.3}U"))
        .collect::<Vec<_>>()
        .join("、");
    lines.push(format!("{DETERMINISTIC_PREFIX} 离场机制分布：{dist}。"));
    let avg_hold = rows.iter().map(|trade| trade.hold_seconds).sum::<i64>() / n;
    lines.push(format!("{DETERMINISTIC_PREFIX} 平均持仓 {avg_hold} 秒。"));
    lines.push(format!(
        "{DETERMINISTIC_PREFIX} 以上仅为台账事实，未作因果归因，不代表可执行策略。"
    ));
    lines
}

fn normalize_change(data: &Map<String, Value>, lessons: &[Value]) -> String {
    if let Some(status) = data.get("change_status").and_then(Value::as_str) {
        let status = status.trim().to_uppercase();
        if status == "CHANGE" || status == "NO_CHANGE" {
            return status;
        }
    }
    if lessons.is_empty() {
        "NO_CHANGE".to_string()
    } else {
        "CHANGE".to_string()
    }
}

fn upsert_lesson(
    conn: &rusqlite::Connection,
    cfg: &WriteCfg,
    item: &Value,
    source: &str,
    sample_size: i64,
    run_evidence: &Value,
) -> Value {
    let title = sanitize(item.get("title").and_then(Value::as_str).unwrap_or(""));
    let rule = sanitize(item.get("rule").and_then(Value::as_str).unwrap_or(""));
    let category = item
        .get("category")
        .and_then(Value::as_str)
        .unwrap_or("entry")
        .to_string();
    let confidence = item
        .get("confidence")
        .and_then(|value| {
            value
                .as_f64()
                .or_else(|| value.as_str().and_then(|text| text.parse::<f64>().ok()))
        })
        .unwrap_or(0.55);
    if let Some(reason) = audit(&title, &rule, &category, confidence, cfg) {
        return json!({"action": "rejected", "reason": reason, "title": title});
    }
    let rows = load_mantras(conn);
    let active: Vec<&MantraRow> = rows.iter().filter(|row| row.superseded_by == 0).collect();
    let replaced_target = item
        .get("replaces")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty());

    let mut best: Option<(&MantraRow, f64)> = None;
    for row in &active {
        let score = lesson_similarity(&row.title, &row.rule, &title, &rule);
        if best.map(|(_, current)| score > current).unwrap_or(true) {
            best = Some((row, score));
        }
    }
    if let Some((row, score)) = best {
        let should_replace = replaced_target.is_some();
        if score >= REPLACE_SIM {
            let fresh = crate::now() - row.updated_at.max(row.created_at) < MERGE_WINDOW_SEC;
            if fresh && row.hits < 10 {
                conn.execute(
                    "UPDATE mantras SET hits = hits + ?2, wins = wins + ?3, losses = losses + ?4, \
                     updated_at = ?5 WHERE id = ?1",
                    (
                        row.id,
                        i64::from(score >= 0.32),
                        i64::from(score >= 0.32),
                        0_i64,
                        crate::now(),
                    ),
                )
                .ok();
                return json!({
                    "action": "merged",
                    "reason": "库内不足 6 小时且未验证，仅合并",
                    "mantra": {"id": row.id, "title": row.title},
                });
            }
            if confidence > row.confidence || should_replace {
                let evidence = merged_evidence(row, run_evidence, sample_size);
                return insert_lesson(
                    conn,
                    cfg,
                    &title,
                    &rule,
                    &category,
                    confidence,
                    source,
                    &evidence,
                    Some(row.id),
                    row.version + 1,
                );
            }
            return json!({
                "action": "rejected",
                "reason": format!("与已存在心法 #{}「{}」语义重复且置信度不更高", row.id, row.title),
                "title": title,
            });
        }
    }
    if active.iter().filter(|row| row.enabled).count() as i64 >= cfg.max_active {
        return json!({"action": "rejected", "reason": "有效心法已达容量上限", "title": title});
    }
    let evidence = json!({
        "source": source,
        "sample_size": sample_size,
        "detail": item.get("evidence").and_then(Value::as_str).unwrap_or("").chars().take(300).collect::<String>(),
        "batch": run_evidence,
    });
    insert_lesson(
        conn, cfg, &title, &rule, &category, confidence, source, &evidence, None, 1,
    )
}

fn merged_evidence(row: &MantraRow, run_evidence: &Value, sample_size: i64) -> Value {
    let mut evidence = serde_json::from_str::<Value>(&row.evidence).unwrap_or_else(|_| json!({}));
    evidence["sample_size"] = json!(sample_size);
    evidence["batch"] = run_evidence.clone();
    evidence
}

fn insert_lesson(
    conn: &rusqlite::Connection,
    cfg: &WriteCfg,
    title: &str,
    rule: &str,
    category: &str,
    confidence: f64,
    source: &str,
    evidence: &Value,
    supersedes: Option<i64>,
    version: i64,
) -> Value {
    let now = crate::now();
    let expires = now + (cfg.ttl_days as i64) * 86_400;
    conn.execute(
        "INSERT INTO mantras(title, rule, tags, category, source, confidence, health, hits, wins, \
         losses, enabled, visible, version, superseded_by, evidence, created_at, updated_at, expires_at) \
         VALUES(?1,'[]',?2,?3,?4,?5,0,0,0,0,1,1,?6,0,?7,?8,?8,?9)",
        (
            title,
            rule,
            category,
            source,
            confidence,
            version,
            serde_json::to_string(evidence).unwrap_or_else(|_| "{}".to_string()),
            now,
            expires,
        ),
    )
    .ok();
    let id = conn.last_insert_rowid();
    if let Some(old) = supersedes {
        conn.execute(
            "UPDATE mantras SET superseded_by = ?2, updated_at = ?3 WHERE id = ?1",
            (old, id, now),
        )
        .ok();
    }
    json!({
        "action": if supersedes.is_some() { "replaced" } else { "created" },
        "mantra": {"id": id, "title": title, "rule": rule, "confidence": confidence, "version": version},
    })
}

fn purge_superseded(conn: &rusqlite::Connection) -> Vec<String> {
    let rows = load_mantras(conn);
    let mut purged = Vec::new();
    for row in rows {
        if row.superseded_by != 0 && row.enabled {
            conn.execute("DELETE FROM mantras WHERE id = ?1", [row.id])
                .ok();
            purged.push(row.title);
        }
    }
    purged
}

fn refresh_health(conn: &rusqlite::Connection, cfg: &EvolutionCfg) -> Value {
    let trades = load_trades(conn, 2000, false, "");
    let now = crate::now();
    let mut updated = 0_i64;
    let mut archived = 0_i64;
    for row in load_mantras(conn) {
        let mut hits = 0_i64;
        let mut wins = 0_i64;
        for trade in &trades {
            let injected = trade
                .features
                .get("injected_mantra_ids")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let in_features = injected.iter().any(|value| value.as_i64() == Some(row.id));
            let in_trade = trade
                .mantra_ids
                .as_array()
                .map(|ids| ids.iter().any(|value| value.as_i64() == Some(row.id)))
                .unwrap_or(false);
            if in_features || in_trade {
                hits += 1;
                if trade.pnl > 0.0 {
                    wins += 1;
                }
            }
        }
        let losses = hits - wins;
        let health = if hits < cfg.min_health_sample {
            0.55
        } else {
            (wins as f64 + 1.0) / (hits as f64 + 2.0)
        };
        let mut evidence =
            serde_json::from_str::<Value>(&row.evidence).unwrap_or_else(|_| json!({}));
        evidence["health_basis"] = json!({
            "sample_size": hits,
            "basis": "outcome",
            "win_rate": if hits > 0 { round_n(wins as f64 / hits as f64, 4) } else { 0.0 },
        });
        conn.execute(
            "UPDATE mantras SET hits = ?2, wins = ?3, losses = ?4, health = ?5, updated_at = ?6, \
             evidence = ?7 WHERE id = ?1",
            (
                row.id,
                hits,
                wins,
                losses,
                round_n(health, 4),
                now,
                serde_json::to_string(&evidence).unwrap_or_else(|_| "{}".to_string()),
            ),
        )
        .ok();
        updated += 1;
        if hits >= cfg.min_health_sample && health < 0.45 && row.enabled {
            conn.execute(
                "UPDATE mantras SET enabled = 0, updated_at = ?2 WHERE id = ?1",
                (row.id, now),
            )
            .ok();
            archived += 1;
        }
    }
    json!({
        "updated": updated,
        "archived": archived,
        "items": load_mantras(conn)
            .into_iter()
            .filter(|row| row.superseded_by == 0)
            .map(|row| json!({
                "id": row.id,
                "title": row.title,
                "hits": row.hits,
                "wins": row.wins,
                "losses": row.losses,
                "health": row.health,
                "enabled": row.enabled,
            }))
            .collect::<Vec<_>>(),
    })
}

fn insert_run(
    conn: &rusqlite::Connection,
    kind: &str,
    input_count: i64,
    produced: i64,
    replaced: i64,
    ok: bool,
    summary: &str,
    raw: &Value,
) {
    conn.execute(
        "INSERT INTO evolution_runs(ts, kind, input_count, produced, replaced, ok, summary, raw) \
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        (
            crate::now(),
            kind,
            input_count,
            produced,
            replaced,
            i64::from(ok),
            summary.chars().take(500).collect::<String>(),
            serde_json::to_string(raw).unwrap_or_else(|_| "{}".to_string()),
        ),
    )
    .ok();
}

fn review_messages(stats: &Value, trades: &[Value], existing: &[String]) -> Vec<ChatMessage> {
    vec![
        ChatMessage::new(
            "system",
            "你是交易复盘引擎。只输出一个 JSON 对象：{\"change_status\":\"CHANGE|NO_CHANGE\",\
             \"verdict\":\"...\",\"score\":0,\"diagnosis\":[\"...\"],\
             \"lessons\":[{\"title\":\"...\",\"rule\":\"...\",\"category\":\"entry|exit|risk|timing|position|market\",\
             \"confidence\":0.0,\"evidence\":\"...\",\"replaces\":\"\"}]}。\
             规则必须含触发条件（当/若/如果/一旦）。样本不足或证据不足时必须返回 NO_CHANGE 且 lessons 为空。\
             不得给出杠杆建议，不得把单笔极值外推为规律。",
        ),
        ChatMessage::new(
            "user",
            &serde_json::to_string(&json!({
                "stats": stats,
                "trades": trades,
                "existing_titles": existing,
            }))
            .unwrap_or_else(|_| "{}".to_string()),
        ),
    ]
}

fn global_messages(stats: &Value, trades: &[Value], existing: &[Value]) -> Vec<ChatMessage> {
    vec![
        ChatMessage::new(
            "system",
            "你是跨交易全局复盘引擎。只输出一个 JSON 对象：{\"change_status\":\"CHANGE|NO_CHANGE\",\
             \"diagnosis\":[\"...\"],\"lessons\":[{\"title\":\"...\",\"rule\":\"...\",\
             \"category\":\"entry|exit|risk|timing|position|market\",\"confidence\":0.0,\
             \"evidence\":\"...\",\"replaces\":\"\"}],\"retire\":[{\"title\":\"...\",\"reason\":\"...\"}]}。\
             规则必须含触发条件；证据不足返回 NO_CHANGE。",
        ),
        ChatMessage::new(
            "user",
            &serde_json::to_string(&json!({
                "stats": stats,
                "trades": trades,
                "existing": existing,
            }))
            .unwrap_or_else(|_| "{}".to_string()),
        ),
    ]
}

async fn call_ai(
    state: &AiState,
    messages: &[ChatMessage],
    purpose: &str,
) -> Result<Option<Value>, String> {
    let (cfg, chain) = provider_chain(state);
    if chain.is_empty() {
        return Ok(None);
    }
    let mut call = ChatCall::new(messages, purpose);
    call.temperature = Some(0.3);
    call.max_tokens = Some(8000);
    let outcome =
        ai_client::chat_with_fallback(state.transport.as_ref(), &state.db, &chain, call, &cfg)
            .await;
    if !outcome.ok {
        return Err(outcome.error);
    }
    ai_client::extract_json(&outcome.text).map(|data| Some(Value::Object(data)))
}

#[derive(Deserialize)]
struct ReviewQuery {
    #[serde(default = "default_true")]
    force: bool,
}

fn default_true() -> bool {
    true
}

async fn review_trade(
    State(state): State<AiState>,
    Path(trade_id): Path<i64>,
    Query(query): Query<ReviewQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    review_trade_inner(&state, trade_id, query.force)
        .await
        .map(Json)
        .map_err(|(status, body)| (status, Json(body)))
}

/// 平仓触发与 HTTP 复盘共用同一条链路：攒够样本 → 调模型 → 审计入库。
pub(crate) async fn review_trade_inner(
    state: &AiState,
    trade_id: i64,
    force: bool,
) -> Result<Value, (StatusCode, Value)> {
    let (cfg, pending, injected, pnl) = {
        let conn = state.db.conn.lock().expect("db");
        let trade = load_trades(&conn, 5000, false, "")
            .into_iter()
            .find(|trade| trade.id == trade_id);
        let Some(trade) = trade else {
            return Err((StatusCode::BAD_REQUEST, json!({"detail": "交易不存在"})));
        };
        let injected = trade
            .features
            .get("injected_mantra_ids")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(Value::as_i64)
            .collect::<Vec<_>>();
        let pending: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM trades WHERE reviewed = 0",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        (evolution_cfg(&conn), pending, injected, trade.pnl)
    };
    if !force && pending < cfg.batch_review_size {
        return Ok(json!({
            "ok": true,
            "skipped": true,
            "change_status": "NO_CHANGE",
            "reason": format!("等待批量样本（{pending}/{}）", cfg.batch_review_size),
            "pending": pending,
            "applied": [],
            "insights": [],
        }));
    }
    if !injected.is_empty() {
        let conn = state.db.conn.lock().expect("db");
        for id in injected {
            conn.execute(
                "UPDATE mantras SET hits = hits + 1, wins = wins + ?2, losses = losses + ?3, \
                 updated_at = ?4 WHERE id = ?1",
                (
                    id,
                    i64::from(pnl > 0.0),
                    i64::from(pnl <= 0.0),
                    crate::now(),
                ),
            )
            .ok();
        }
    }
    let Json(value) = run_batch(state.clone(), force, None)
        .await
        .map_err(|(status, body)| (status, body.0))?;
    Ok(value)
}

/// 平仓事务提交后的复盘入口：进程内去重，并把结果/跳过原因写进事件流。
pub(crate) async fn review_trade_and_record(
    state: AiState,
    reviewing: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<i64>>>,
    trade_id: i64,
) {
    {
        let mut guard = reviewing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !guard.insert(trade_id) {
            return;
        }
    }
    let enabled = {
        let conn = state.db.conn.lock().expect("db");
        let cfg = evolution_cfg(&conn);
        cfg.enabled && cfg.review_on_close
    };
    if !enabled {
        reviewing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&trade_id);
        return;
    }
    let result = review_trade_inner(&state, trade_id, false).await;
    let (level, message, payload) = match result {
        Ok(value) if value["skipped"].as_bool().unwrap_or(false) => (
            "warn",
            format!(
                "交易 #{trade_id} 复盘跳过: {}",
                value["reason"].as_str().unwrap_or("样本不足")
            ),
            json!({"trade_id": trade_id, "skipped": true, "result": value}),
        ),
        Ok(value) => (
            "info",
            format!(
                "交易 #{trade_id} 复盘完成: {} 笔样本, change_status={}",
                value["sample_size"].as_i64().unwrap_or(0),
                value["change_status"].as_str().unwrap_or("NO_CHANGE")
            ),
            json!({"trade_id": trade_id, "result": value}),
        ),
        Err((_, body)) => (
            "warn",
            format!(
                "交易 #{trade_id} 复盘失败: {}",
                body["detail"].as_str().unwrap_or("未知错误")
            ),
            json!({"trade_id": trade_id, "error": body}),
        ),
    };
    if let Ok(conn) = state.db.conn.lock() {
        let _ = conn.execute(
            "INSERT INTO engine_events(ts,level,category,message,payload) VALUES(?1,?2,'evolution',?3,?4)",
            (
                crate::now(),
                level,
                message.chars().take(400).collect::<String>(),
                serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string()),
            ),
        );
    }
    reviewing
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&trade_id);
}

#[derive(Deserialize)]
struct BatchQuery {
    #[serde(default)]
    limit: i64,
    #[serde(default)]
    force: bool,
}

async fn review_batch(
    State(state): State<AiState>,
    Query(query): Query<BatchQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    run_batch(state, query.force, Some(query.limit)).await
}

async fn run_batch(
    state: AiState,
    force: bool,
    limit: Option<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let (cfg, rows) = {
        let conn = state.db.conn.lock().expect("db");
        let cfg = evolution_cfg(&conn);
        let enabled: bool = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key = 'evolution'",
                [],
                |row| row.get::<_, String>(0),
            )
            .ok()
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .and_then(|value| value.get("enabled").and_then(Value::as_bool))
            .unwrap_or(true);
        if !enabled {
            return Err((
                StatusCode::BAD_REQUEST,
                json!({"detail": "自进化已关闭"}).into(),
            ));
        }
        let size = cfg.min_lesson_sample.max(
            limit
                .filter(|value| *value > 0)
                .unwrap_or(cfg.batch_review_size),
        );
        (cfg, load_trades(&conn, size, !force, ""))
    };
    if (rows.len() as i64) < cfg.min_lesson_sample {
        return Ok(json!({
            "ok": true,
            "skipped": true,
            "change_status": "NO_CHANGE",
            "reason": format!("样本不足（{}/{}），NO_CHANGE", rows.len(), cfg.min_lesson_sample),
            "pending": rows.len(),
            "sample_size": rows.len(),
            "applied": [],
            "insights": [],
        })
        .into());
    }
    let stats = batch_stats(&rows);
    let payloads: Vec<Value> = rows
        .iter()
        .map(|trade| trade_payload(trade, false))
        .collect();
    let (existing, write_cfg) = {
        let conn = state.db.conn.lock().expect("db");
        let existing: Vec<String> = load_mantras(&conn)
            .into_iter()
            .filter(|row| row.superseded_by == 0)
            .map(|row| row.title)
            .collect();
        (existing, write_cfg(&conn))
    };
    let _ = existing;
    let (ai_data, ai_error) =
        match call_ai(&state, &review_messages(&stats, &payloads, &[]), "review").await {
            Ok(data) => (data, String::new()),
            Err(error) => (None, error),
        };
    let mut lessons: Vec<Value> = Vec::new();
    let mut diagnosis: Vec<String> = Vec::new();
    let mut verdict = String::new();
    let mut score = 0_i64;
    let mut used_ai = false;
    let mut change_status = "NO_CHANGE".to_string();
    if let Some(data) = &ai_data {
        if let Some(map) = data.as_object() {
            lessons = map
                .get("lessons")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter(|item| item.is_object())
                        .take(3)
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            diagnosis = map
                .get("diagnosis")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .take(4)
                        .map(|text| text.chars().take(200).collect::<String>())
                        .collect()
                })
                .unwrap_or_default();
            verdict = map
                .get("verdict")
                .and_then(Value::as_str)
                .unwrap_or("")
                .chars()
                .take(300)
                .collect();
            score = map.get("score").and_then(Value::as_i64).unwrap_or(0);
            used_ai = true;
            change_status = normalize_change(map, &lessons);
            if change_status == "NO_CHANGE" {
                lessons.clear();
            }
        }
    }
    let insights = if !used_ai || lessons.is_empty() {
        deterministic_insights(&rows)
    } else {
        Vec::new()
    };
    let applied = {
        let conn = state.db.conn.lock().expect("db");
        let mut applied: Vec<Value> = Vec::new();
        for item in &lessons {
            let result = upsert_lesson(
                &conn,
                &write_cfg,
                item,
                "batch_review",
                rows.len() as i64,
                &stats,
            );
            let action = result["action"].as_str().unwrap_or("");
            if action == "rejected" {
                applied.push(json!({
                    "action": "rejected",
                    "reason": result["reason"],
                    "title": result["title"],
                }));
            } else {
                applied.push(json!({
                    "action": action,
                    "title": result["mantra"]["title"],
                    "reason": "",
                }));
            }
        }
        applied
    };
    let produced = applied
        .iter()
        .filter(|item| matches!(item["action"].as_str(), Some("created") | Some("replaced")))
        .count() as i64;
    let replaced = applied
        .iter()
        .filter(|item| item["action"] == "replaced")
        .count() as i64;
    let note = if verdict.is_empty() {
        if insights.is_empty() {
            format!("NO_CHANGE（{} 笔台账样本）", rows.len())
        } else {
            insights.join("; ")
        }
    } else {
        verdict.clone()
    };
    {
        let conn = state.db.conn.lock().expect("db");
        for trade in &rows {
            conn.execute(
                "UPDATE trades SET reviewed = 1, review_note = ?2 WHERE id = ?1",
                (trade.id, note.chars().take(900).collect::<String>()),
            )
            .ok();
        }
        let archived = purge_superseded(&conn);
        let health = refresh_health(&conn, &cfg);
        let raw = json!({
            "change_status": change_status,
            "applied": applied,
            "diagnosis": diagnosis,
            "insights": insights,
            "used_ai": used_ai,
            "error": ai_error,
            "score": score,
            "sample_size": rows.len(),
            "stats": stats,
            "archived": archived,
            "health": health,
        });
        insert_run(
            &conn,
            "batch_review",
            rows.len() as i64,
            produced,
            replaced,
            true,
            &note,
            &raw,
        );
    }
    Ok(json!({
        "ok": true,
        "used_ai": used_ai,
        "error": ai_error,
        "change_status": change_status,
        "sample_size": rows.len(),
        "verdict": verdict,
        "score": score,
        "diagnosis": diagnosis,
        "insights": insights,
        "stats": stats,
        "applied": applied,
    })
    .into())
}

async fn health_refresh(State(state): State<AiState>) -> Json<Value> {
    let conn = state.db.conn.lock().expect("db");
    let cfg = evolution_cfg(&conn);
    let purged = purge_superseded(&conn);
    let result = refresh_health(&conn, &cfg);
    json!({
        "purged": purged,
        "health": result,
    })
    .into()
}

async fn global_review(
    State(state): State<AiState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let (cfg, stats, rows, existing) = {
        let conn = state.db.conn.lock().expect("db");
        let cfg = evolution_cfg(&conn);
        let rows = load_trades(&conn, 60, false, "");
        let stats = batch_stats(&rows);
        let existing: Vec<Value> = load_mantras(&conn)
            .into_iter()
            .filter(|row| row.superseded_by == 0)
            .filter(|row| row.enabled)
            .take(40)
            .map(|row| json!({"title": row.title, "health": row.health}))
            .collect();
        (cfg, stats, rows, existing)
    };
    if (rows.len() as i64) < cfg.min_lesson_sample {
        return Err((
            StatusCode::BAD_REQUEST,
            json!({"detail": format!("样本不足 (至少 {} 笔闭环交易)", cfg.min_lesson_sample)})
                .into(),
        ));
    }
    let payloads: Vec<Value> = rows
        .iter()
        .map(|trade| trade_payload(trade, true))
        .collect();
    let (ai_data, ai_error) = match call_ai(
        &state,
        &global_messages(&stats, &payloads, &existing),
        "global_review",
    )
    .await
    {
        Ok(data) => (data, String::new()),
        Err(error) => (None, error),
    };
    let mut diagnosis: Vec<String> = Vec::new();
    let mut applied: Vec<Value> = Vec::new();
    let mut retired: Vec<String> = Vec::new();
    let mut used_ai = false;
    let mut change_status = "NO_CHANGE".to_string();
    if let Some(data) = &ai_data {
        if let Some(map) = data.as_object() {
            used_ai = true;
            diagnosis = map
                .get("diagnosis")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .take(6)
                        .map(|text| text.chars().take(200).collect::<String>())
                        .collect()
                })
                .unwrap_or_default();
            let proposals: Vec<Value> = map
                .get("lessons")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter(|item| item.is_object())
                        .take(5)
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            change_status = normalize_change(map, &proposals);
            let conn = state.db.conn.lock().expect("db");
            let cfg_write = write_cfg(&conn);
            if change_status != "NO_CHANGE" {
                for item in &proposals {
                    let result = upsert_lesson(
                        &conn,
                        &cfg_write,
                        item,
                        "global_review",
                        rows.len() as i64,
                        &stats,
                    );
                    applied.push(json!({
                        "action": if result["action"] == "rejected" { json!("rejected") } else { result["action"].clone() },
                        "title": result["mantra"]["title"],
                        "reason": result["reason"],
                    }));
                }
            }
            if let Some(items) = map.get("retire").and_then(Value::as_array) {
                for item in items.iter().take(5) {
                    let title = item.get("title").and_then(Value::as_str).unwrap_or("");
                    if title.is_empty() {
                        continue;
                    }
                    if let Some(row) = load_mantras(&conn).into_iter().find(|row| {
                        lesson_similarity(&row.title, "", title, "") >= 0.55
                            && row.superseded_by == 0
                    }) {
                        conn.execute(
                            "UPDATE mantras SET enabled = 0, updated_at = ?2 WHERE id = ?1",
                            (row.id, crate::now()),
                        )
                        .ok();
                        retired.push(row.title);
                    }
                }
            }
        }
    }
    if !used_ai {
        change_status = "NO_CHANGE".to_string();
        applied.clear();
        diagnosis = stats
            .get("by_reason")
            .and_then(Value::as_object)
            .map(|reasons| {
                reasons
                    .iter()
                    .map(|(reason, cell)| {
                        format!(
                            "{DETERMINISTIC_PREFIX} {reason}: {} 笔, 净 {:.4}U",
                            cell["n"].as_i64().unwrap_or(0),
                            cell["pnl"].as_f64().unwrap_or(0.0)
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
    }
    {
        let conn = state.db.conn.lock().expect("db");
        let archived = purge_superseded(&conn);
        let health = refresh_health(&conn, &cfg);
        let produced = applied
            .iter()
            .filter(|item| matches!(item["action"].as_str(), Some("created") | Some("replaced")))
            .count() as i64;
        let replaced = applied
            .iter()
            .filter(|item| item["action"] == "replaced")
            .count() as i64;
        let summary = if diagnosis.is_empty() {
            format!("全局复盘 NO_CHANGE（{} 笔样本）", rows.len())
        } else {
            diagnosis.join("; ")
        };
        insert_run(
            &conn,
            "global_review",
            rows.len() as i64,
            produced,
            replaced,
            true,
            &summary,
            &json!({
                "change_status": change_status,
                "applied": applied,
                "retired": retired,
                "stats": stats,
                "used_ai": used_ai,
                "error": ai_error,
                "archived": archived,
                "health": health,
            }),
        );
    }
    Ok(json!({
        "ok": true,
        "used_ai": used_ai,
        "error": ai_error,
        "change_status": change_status,
        "diagnosis": diagnosis,
        "applied": applied,
        "retired": retired,
        "stats": stats,
    })
    .into())
}

#[derive(Deserialize)]
struct EffectivenessQuery {
    #[serde(default = "default_limit")]
    limit: i64,
    #[serde(default)]
    mode: String,
}

fn default_limit() -> i64 {
    500
}

async fn effectiveness(
    State(state): State<AiState>,
    Query(query): Query<EffectivenessQuery>,
) -> Json<Value> {
    let limit = query.limit.clamp(1, 2000);
    let mode = query.mode.as_str();
    let mode = if matches!(mode, "demo" | "live") {
        mode
    } else {
        ""
    };
    let conn = state.db.conn.lock().expect("db");
    let rows = load_trades(&conn, limit, false, mode);
    drop(conn);
    let mut adopted: Vec<&TradeRow> = Vec::new();
    let mut control: Vec<&TradeRow> = Vec::new();
    for row in &rows {
        let injected = row
            .features
            .get("injected_mantra_ids")
            .and_then(Value::as_array)
            .map(|items| !items.is_empty())
            .unwrap_or(false);
        let ids = row
            .mantra_ids
            .as_array()
            .map(|items| !items.is_empty())
            .unwrap_or(false);
        if injected || ids {
            adopted.push(row);
        } else {
            control.push(row);
        }
    }
    let summarize = |group: &[&TradeRow]| -> Value {
        let total = group.len() as i64;
        if total == 0 {
            return json!({"trades": 0, "win_rate": 0.0, "pnl": 0.0, "avg_pnl": 0.0});
        }
        let wins = group.iter().filter(|trade| trade.pnl > 0.0).count() as i64;
        let pnl: f64 = group.iter().map(|trade| trade.pnl).sum();
        json!({
            "trades": total,
            "win_rate": round_n(wins as f64 / total as f64 * 100.0, 2),
            "pnl": round_n(pnl, 6),
            "avg_pnl": round_n(pnl / total as f64, 6),
        })
    };
    let adopted_view = summarize(&adopted);
    let control_view = summarize(&control);
    let sample_enough = adopted.len() >= 10 && control.len() >= 10;
    let conclusion = if !sample_enough {
        format!(
            "样本不足（采用 {} 笔 / 对照 {} 笔，各需 10 笔），仅展示数据，不下结论",
            adopted.len(),
            control.len()
        )
    } else {
        let a = adopted_view["avg_pnl"].as_f64().unwrap_or(0.0);
        let c = control_view["avg_pnl"].as_f64().unwrap_or(0.0);
        if a > c {
            "采用组平均盈亏高于对照组（观察性，不代表因果）".to_string()
        } else if a < c {
            "采用组平均盈亏低于对照组（观察性，不代表因果）".to_string()
        } else {
            "采用组与对照组平均盈亏相同（观察性，不代表因果）".to_string()
        }
    };
    json!({
        "adopted": adopted_view,
        "control": control_view,
        "min_sample": 10,
        "sample_enough": sample_enough,
        "conclusion": conclusion,
    })
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai_client::{AiTransport, BoxFuture, HttpReply};
    use crate::ai_market::UnavailableMarket;
    use crate::ai_providers::AiState;
    use crate::db::Db;
    use axum::body::Body;
    use axum::http::Request;
    use std::sync::Arc;
    use std::sync::Mutex;
    use tower::ServiceExt;

    const MASTER_KEY: &str = "MDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDA=";

    struct FakeTransport {
        replies: Mutex<Vec<HttpReply>>,
    }

    impl AiTransport for FakeTransport {
        fn post_json<'a>(
            &'a self,
            _url: &'a str,
            _headers: &'a [(String, String)],
            _body: &'a Value,
            _timeout_secs: u64,
        ) -> BoxFuture<'a, Result<HttpReply, String>> {
            let reply = self
                .replies
                .lock()
                .unwrap()
                .drain(..1)
                .next()
                .unwrap_or(HttpReply {
                    status: 500,
                    body: "fixture exhausted".to_string(),
                });
            Box::pin(async move { Ok(reply) })
        }

        fn get_json<'a>(
            &'a self,
            _url: &'a str,
            _headers: &'a [(String, String)],
            _timeout_secs: u64,
        ) -> BoxFuture<'a, Result<HttpReply, String>> {
            Box::pin(async move {
                Ok(HttpReply {
                    status: 404,
                    body: "not found".to_string(),
                })
            })
        }
    }

    fn fixture(path: &std::path::Path, with_ai: bool) -> Db {
        let _ = std::fs::remove_file(path);
        let conn = rusqlite::Connection::open(path).unwrap();
        let provider_sql = if with_ai {
            "INSERT INTO ai_providers VALUES(1,'alpha','openai_chat','https://api.example.com/v1', \
             'k', 'deepseek-v4', 0.2, 1024, 'auto', 5, 1, 1, 10, 'primary', '{}', 1, 1);"
        } else {
            ""
        };
        let cipher = crate::secret::encrypt_secret(MASTER_KEY, "sk-test").unwrap();
        let sql = format!(
            "CREATE TABLE app_settings(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER); \
             CREATE TABLE engine_state(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER); \
             CREATE TABLE ai_call_logs(id INTEGER PRIMARY KEY, ts INTEGER, provider TEXT, model TEXT, purpose TEXT, \
             ok INTEGER, latency_ms INTEGER, prompt_tokens INTEGER, completion_tokens INTEGER, cost_usd REAL, error TEXT, raw_text TEXT); \
             CREATE TABLE ai_providers(id INTEGER PRIMARY KEY, name TEXT, protocol TEXT, base_url TEXT, \
             api_key_enc TEXT, model TEXT, temperature REAL, max_tokens INTEGER, reasoning_effort TEXT, \
             timeout_sec INTEGER, json_mode INTEGER, enabled INTEGER, priority INTEGER, role TEXT, \
             extra TEXT, created_at INTEGER, updated_at INTEGER); \
             CREATE TABLE mantras(id INTEGER PRIMARY KEY, title TEXT, rule TEXT, tags TEXT, category TEXT, \
             source TEXT, confidence REAL, health REAL, hits INTEGER, wins INTEGER, losses INTEGER, \
             enabled INTEGER, visible INTEGER, version INTEGER, superseded_by INTEGER, evidence TEXT, \
             created_at INTEGER, updated_at INTEGER, expires_at INTEGER); \
             CREATE TABLE trades(id INTEGER PRIMARY KEY, symbol TEXT, side TEXT, qty REAL, entry_price REAL, \
             exit_price REAL, leverage REAL, notional REAL, pnl REAL, pnl_pct REAL, roi_pct REAL, fees REAL, \
             opened_at INTEGER, closed_at INTEGER, hold_seconds INTEGER, exit_reason TEXT, ai_confidence REAL, \
             ai_reason TEXT, signal_score REAL, slippage_bps REAL, mode TEXT, reviewed INTEGER, review_note TEXT, \
             features TEXT, mantra_ids TEXT); \
             CREATE TABLE evolution_runs(id INTEGER PRIMARY KEY, ts INTEGER, kind TEXT, input_count INTEGER, \
             produced INTEGER, replaced INTEGER, ok INTEGER, summary TEXT, raw TEXT); \
             INSERT INTO app_settings VALUES('evolution','{{\"enabled\":true,\"min_lesson_sample\":2,\"batch_review_size\":5,\"min_health_sample\":10,\"min_confidence\":0.5,\"guard_strict\":true,\"max_active_mantras\":40,\"ttl_days\":7,\"max_rule_chars\":420}}',0); \
             INSERT INTO app_settings VALUES('ai','{{\"enabled\":true,\"mode\":\"single\",\"provider_id\":1,\"fallback_provider_ids\":[],\"min_confidence\":0.45,\"max_calls_per_hour\":30,\"monthly_budget_usd\":3,\"timeout_sec\":5,\"decision_schema_version\":2}}',0); \
             INSERT INTO engine_state VALUES('mode','demo',0); \
             INSERT INTO mantras VALUES(1,'追高','当突破前高时追多','[]','entry','manual',0.8,0.7,0,0,0,1,1,1,0,'{{}}',100,100,0); \
             {provider_sql}"
        );
        conn.execute_batch(&sql).unwrap();
        if with_ai {
            conn.execute("UPDATE ai_providers SET api_key_enc = ?1", [&cipher])
                .unwrap();
        }
        conn.execute_batch(
            "INSERT INTO trades VALUES(1,'BTCUSDT','long',0.1,100,110,5,0,10,10,50,0.2,900,1000,100,'tp',0.7,'ok',0.8,0,'demo',0,'', \
             '{\"injected_mantra_ids\":[1]}','[1]'); \
             INSERT INTO trades VALUES(2,'BTCUSDT','short',0.1,100,90,5,0,-10,-10,-50,0.2,800,900,100,'sl',0.6,'bad',0.5,0,'demo',0,'', \
             '{\"injected_mantra_ids\":[1]}','[]'); \
             INSERT INTO trades VALUES(3,'ETHUSDT','long',0.1,100,120,5,0,20,20,100,0.2,700,800,100,'tp',0.8,'ok',0.9,0,'demo',0,'', \
             '{}','[]');",
        )
        .unwrap();
        drop(conn);
        Db::open(path.to_str().unwrap()).unwrap()
    }

    fn app(db: Db, transport: Arc<dyn AiTransport>) -> Router {
        Router::new().nest("/api/v1", routes()).with_state(
            AiState::with_transport(db, MASTER_KEY.as_bytes().to_vec(), transport)
                .with_market(Arc::new(UnavailableMarket)),
        )
    }

    async fn request(router: Router, method: &str, uri: &str) -> (StatusCode, Value) {
        let response = router
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn effectiveness_splits_adopted_and_control_and_refuses_conclusions() {
        let path = std::env::temp_dir().join("nq-evo-effectiveness.db");
        let db = fixture(&path, false);
        let router = app(
            db.clone(),
            Arc::new(FakeTransport {
                replies: Mutex::new(Vec::new()),
            }),
        );
        let (status, body) =
            request(router, "GET", "/api/v1/evolution/effectiveness?mode=demo").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["adopted"]["trades"], 2);
        assert_eq!(body["control"]["trades"], 1);
        assert_eq!(body["sample_enough"], false);
        assert!(body["conclusion"].as_str().unwrap().contains("样本不足"));
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn health_refresh_recomputes_counts_from_injected_trades() {
        let path = std::env::temp_dir().join("nq-evo-health.db");
        let db = fixture(&path, false);
        let router = app(
            db.clone(),
            Arc::new(FakeTransport {
                replies: Mutex::new(Vec::new()),
            }),
        );
        let (status, body) = request(router, "POST", "/api/v1/evolution/health-refresh").await;
        assert_eq!(status, StatusCode::OK);
        let item = body["health"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["id"] == 1)
            .unwrap();
        assert_eq!(item["hits"], 2);
        assert_eq!(item["wins"], 1);
        assert_eq!(item["losses"], 1);
        assert_eq!(item["health"], 0.55, "样本不足 10 笔时用先验分");
        let conn = db.conn.lock().unwrap();
        let updated: i64 = conn
            .query_row("SELECT updated_at FROM mantras WHERE id = 1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(updated > 100);
        drop(conn);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn review_batch_without_ai_stays_no_change_but_marks_reviewed() {
        let path = std::env::temp_dir().join("nq-evo-batch.db");
        let db = fixture(&path, false);
        let router = app(
            db.clone(),
            Arc::new(FakeTransport {
                replies: Mutex::new(Vec::new()),
            }),
        );
        let (status, body) =
            request(router, "POST", "/api/v1/evolution/review-batch?force=true").await;
        assert_eq!(status, StatusCode::OK, "{}", body);
        assert_eq!(body["used_ai"], false);
        assert_eq!(body["change_status"], "NO_CHANGE");
        assert!(!body["insights"].as_array().unwrap().is_empty());
        assert!(body["insights"][0]
            .as_str()
            .unwrap()
            .contains(DETERMINISTIC_PREFIX));
        let conn = db.conn.lock().unwrap();
        let pending: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM trades WHERE reviewed = 0",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pending, 0, "复盘后全部标记已读");
        let runs: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM evolution_runs WHERE kind = 'batch_review'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(runs, 1);
        drop(conn);
        let _ = std::fs::remove_file(&path);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn review_with_ai_creates_lesson_and_missing_trade_is_400() {
        let path = std::env::temp_dir().join("nq-evo-ai.db");
        let db = fixture(&path, true);
        let transport = Arc::new(FakeTransport {
            replies: Mutex::new(vec![HttpReply {
                status: 200,
                body: "{\"choices\":[{\"message\":{\"content\":\"{\\\"change_status\\\":\\\"CHANGE\\\",\\\"verdict\\\":\\\"样本显示止盈有效\\\",\\\"score\\\":70,\\\"diagnosis\\\":[\\\"tp 组表现更好\\\"],\\\"lessons\\\":[{\\\"title\\\":\\\"止盈优先\\\",\\\"rule\\\":\\\"当浮盈达到 2R 时先落袋一半\\\",\\\"category\\\":\\\"exit\\\",\\\"confidence\\\":0.7,\\\"evidence\\\":\\\"3 笔样本\\\"}]}\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}".to_string(),
            }]),
        });
        let router = app(db.clone(), transport);
        let (status, body) = request(
            router.clone(),
            "POST",
            "/api/v1/evolution/review/1?force=true",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{}", body);
        assert_eq!(body["used_ai"], true);
        assert_eq!(body["change_status"], "CHANGE");
        assert_eq!(body["applied"].as_array().unwrap().len(), 1);
        let conn = db.conn.lock().unwrap();
        let created: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM mantras WHERE source = 'batch_review'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(created, 1);
        drop(conn);

        let (status, _) = request(router, "POST", "/api/v1/evolution/review/999?force=true").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let _ = std::fs::remove_file(&path);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn global_review_requires_samples_then_reports_ledger_facts() {
        let path = std::env::temp_dir().join("nq-evo-global.db");
        let db = fixture(&path, false);
        let router = app(
            db.clone(),
            Arc::new(FakeTransport {
                replies: Mutex::new(Vec::new()),
            }),
        );
        let (status, body) =
            request(router.clone(), "POST", "/api/v1/evolution/global-review").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["used_ai"], false);
        assert_eq!(body["change_status"], "NO_CHANGE");
        assert!(body["diagnosis"][0]
            .as_str()
            .unwrap()
            .contains(DETERMINISTIC_PREFIX));
        let conn = db.conn.lock().unwrap();
        let runs: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM evolution_runs WHERE kind = 'global_review'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(runs, 1);
        drop(conn);

        let empty_path = std::env::temp_dir().join("nq-evo-global-empty.db");
        let empty = fixture(&empty_path, false);
        empty
            .conn
            .lock()
            .unwrap()
            .execute("DELETE FROM trades", [])
            .unwrap();
        let router = app(
            empty.clone(),
            Arc::new(FakeTransport {
                replies: Mutex::new(Vec::new()),
            }),
        );
        let (status, body) = request(router, "POST", "/api/v1/evolution/global-review").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["detail"].as_str().unwrap().contains("样本不足"));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&empty_path);
    }

    #[tokio::test]
    async fn review_batch_rejects_when_evolution_is_disabled() {
        let path = std::env::temp_dir().join("nq-evo-disabled.db");
        let db = fixture(&path, false);
        db.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE app_settings SET value = '{\"enabled\":false}' WHERE key = 'evolution'",
                [],
            )
            .unwrap();
        let router = app(
            db.clone(),
            Arc::new(FakeTransport {
                replies: Mutex::new(Vec::new()),
            }),
        );
        let (status, body) =
            request(router, "POST", "/api/v1/evolution/review-batch?force=true").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["detail"].as_str().unwrap().contains("自进化已关闭"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn fresh_lesson_merges_within_six_hours_then_replaces_after_the_window() {
        let path = std::env::temp_dir().join("nq-evo-merge-window.db");
        let db = fixture(&path, false);
        let conn = db.conn.lock().unwrap();
        conn.execute(
            "UPDATE mantras SET title='突破确认', rule='当突破前高并且量比放大时追多', updated_at=?1 WHERE id=1",
            [crate::now()],
        )
        .unwrap();
        let cfg = crate::evolution_api::write_cfg(&conn);
        let item = json!({
            "title": "突破确认",
            "rule": "当突破前高并且量比放大时追多",
            "category": "entry",
            "confidence": 0.9,
            "evidence": "同场景再次验证",
        });
        let merged = upsert_lesson(&conn, &cfg, &item, "test", 3, &json!({}));
        assert_eq!(
            merged["action"], "merged",
            "6 小时内未验证的同场景结论只能合并: {merged}"
        );
        conn.execute(
            "UPDATE mantras SET updated_at=?1 WHERE id=1",
            [crate::now() - 7 * 3600],
        )
        .unwrap();
        let replaced = upsert_lesson(&conn, &cfg, &item, "test", 3, &json!({}));
        assert_eq!(
            replaced["action"], "replaced",
            "过了 6 小时且置信度更高才允许替代: {replaced}"
        );
        let old_id: i64 = replaced["mantra"]["id"].as_i64().unwrap();
        drop(conn);
        let conn = db.conn.lock().unwrap();
        let superseded: i64 = conn
            .query_row("SELECT superseded_by FROM mantras WHERE id=1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(superseded, old_id);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn purge_is_idempotent_and_keeps_manually_disabled_rows() {
        let path = std::env::temp_dir().join("nq-evo-purge.db");
        let db = fixture(&path, false);
        let conn = db.conn.lock().unwrap();
        conn.execute_batch(
            "INSERT INTO mantras VALUES(2,'新规则','当回踩时开多','[]','entry','manual',0.7,0.6,0,0,0,1,1,1,0,'{}',200,200,0); \
             UPDATE mantras SET superseded_by=2 WHERE id=1; \
             INSERT INTO mantras VALUES(3,'手动停用','当趋势反转时离场','[]','exit','manual',0.7,0.4,0,0,0,0,1,1,9,'{}',100,100,0);",
        )
        .unwrap();
        let first = purge_superseded(&conn);
        assert_eq!(
            first,
            vec!["追高".to_string()],
            "只清理被替代且仍启用的条目"
        );
        let second = purge_superseded(&conn);
        assert!(second.is_empty(), "连续两次清理结果一致");
        let kept: i64 = conn
            .query_row("SELECT COUNT(*) FROM mantras WHERE id=3", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kept, 1, "手动停用的条目不参与物理删除");
        let _ = std::fs::remove_file(&path);
    }
}
