//! 引擎控制。启停状态双写 `engine` 配置组与 `engine_state` 表，
//! 对齐 `api/routes/trade.py` 的 `control`。

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::db::Db;

pub fn routes() -> Router<Db> {
    Router::new()
        .route("/engine/control", post(control))
        .route("/engine/status", get(status))
        .route("/engine/stats", get(stats))
        .route("/engine/positions", get(positions))
        .route("/engine/orders", get(orders))
        .route("/engine/trades", get(trades))
        .route("/engine/signals", get(signals))
        .route("/engine/equity", get(equity))
        .route("/engine/execution", get(execution))
        .route("/engine/reset-anchors", post(reset_anchors))
        .route("/engine/admission", get(admission))
}

#[derive(Deserialize)]
struct TradesQuery {
    #[serde(default = "default_trades_limit")]
    limit: i64,
}

fn default_trades_limit() -> i64 {
    60
}

async fn trades(State(db): State<Db>, Query(q): Query<TradesQuery>) -> Json<Value> {
    let limit = q.limit.clamp(1, 500);
    let mode = current_mode(&db);
    let conn = db.conn.lock().expect("db");
    let mut stmt = conn
        .prepare(
            "SELECT id, symbol, side, qty, entry_price, exit_price, leverage, notional, pnl, pnl_pct, roi_pct, \
             fees, opened_at, closed_at, hold_seconds, exit_reason, ai_confidence, ai_reason, signal_score, \
             mode, reviewed, review_note, mantra_ids FROM trades WHERE mode = ?1 ORDER BY closed_at DESC LIMIT ?2",
        )
        .expect("prepare trades");
    let items: Vec<Value> = stmt
        .query_map(rusqlite::params![mode, limit], |r| {
            let mantra_raw: String = r.get(22)?;
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "symbol": r.get::<_, String>(1)?,
                "side": r.get::<_, String>(2)?,
                "qty": r.get::<_, f64>(3)?,
                "entry_price": r.get::<_, f64>(4)?,
                "exit_price": r.get::<_, f64>(5)?,
                "leverage": r.get::<_, f64>(6)?,
                "notional": r.get::<_, f64>(7)?,
                "pnl": round_n(r.get(8)?, 8),
                "pnl_pct": round_n(r.get(9)?, 4),
                "roi_pct": round_n(r.get(10)?, 4),
                "fees": round_n(r.get(11)?, 8),
                "opened_at": r.get::<_, i64>(12)?,
                "closed_at": r.get::<_, i64>(13)?,
                "hold_seconds": r.get::<_, i64>(14)?,
                "exit_reason": r.get::<_, String>(15)?,
                "ai_confidence": r.get::<_, f64>(16)?,
                "ai_reason": r.get::<_, String>(17)?,
                "signal_score": r.get::<_, f64>(18)?,
                "mode": r.get::<_, String>(19)?,
                "reviewed": r.get::<_, i64>(20)? != 0,
                "review_note": r.get::<_, String>(21)?,
                "mantra_ids": serde_json::from_str::<Value>(&mantra_raw).unwrap_or(json!([])),
            }))
        })
        .expect("query trades")
        .collect::<Result<_, _>>()
        .expect("trade rows");
    let stats = trade_stats(&conn, &mode);
    json!({ "items": items, "stats": stats }).into()
}

/// 成交统计，口径对齐 `evolution/review.py::trade_stats`：只看当前环境最近 200 笔。
fn trade_stats(conn: &Connection, mode: &str) -> Value {
    let mut stmt = conn
        .prepare(
            "SELECT symbol, pnl, exit_reason, hold_seconds FROM trades WHERE mode = ?1 \
             ORDER BY closed_at DESC LIMIT 200",
        )
        .expect("prepare trade stats");
    let rows: Vec<(String, f64, String, i64)> = stmt
        .query_map([mode], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .expect("query trade stats")
        .collect::<Result<_, _>>()
        .expect("trade stat rows");
    let total = rows.len();
    if total == 0 {
        return json!({"trades": 0, "win_rate": 0.0, "pnl": 0.0, "profit_factor": 0.0, "by_reason": {}, "by_symbol": {}, "avg_hold": 0});
    }
    let wins = rows.iter().filter(|(_, pnl, _, _)| *pnl > 0.0).count();
    let gross_win: f64 = rows
        .iter()
        .filter(|(_, pnl, _, _)| *pnl > 0.0)
        .map(|(_, pnl, _, _)| pnl)
        .sum();
    let gross_loss: f64 = rows
        .iter()
        .filter(|(_, pnl, _, _)| *pnl <= 0.0)
        .map(|(_, pnl, _, _)| pnl.abs())
        .sum();
    let pnl_sum: f64 = rows.iter().map(|(_, pnl, _, _)| pnl).sum();
    let hold_sum: i64 = rows.iter().map(|(_, _, _, hold)| hold).sum();
    let mut by_reason = serde_json::Map::new();
    let mut by_symbol = serde_json::Map::new();
    for (symbol, pnl, reason, _) in &rows {
        bump(
            &mut by_reason,
            if reason.is_empty() { "unknown" } else { reason },
            *pnl,
        );
        bump(&mut by_symbol, symbol, *pnl);
    }
    let profit_factor = if gross_loss > 0.0 {
        round_n(gross_win / gross_loss, 3)
    } else if gross_win > 0.0 {
        999.0
    } else {
        0.0
    };
    json!({
        "trades": total,
        "wins": wins,
        "losses": total - wins,
        "win_rate": round_n(wins as f64 / total as f64 * 100.0, 2),
        "pnl": round_n(pnl_sum, 6),
        "avg_pnl": round_n(pnl_sum / total as f64, 6),
        "profit_factor": profit_factor,
        "avg_hold": hold_sum / total as i64,
        "by_reason": by_reason,
        "by_symbol": by_symbol,
    })
}

fn bump(map: &mut serde_json::Map<String, Value>, key: &str, pnl: f64) {
    let entry = map
        .entry(key.to_string())
        .or_insert_with(|| json!({"n": 0, "pnl": 0.0, "wins": 0}));
    let n = entry["n"].as_i64().unwrap_or(0) + 1;
    let total = round_n(entry["pnl"].as_f64().unwrap_or(0.0) + pnl, 6);
    let wins = entry["wins"].as_i64().unwrap_or(0) + if pnl > 0.0 { 1 } else { 0 };
    *entry = json!({"n": n, "pnl": total, "wins": wins});
}

fn round_n(value: f64, digits: u32) -> f64 {
    let scale = 10_f64.powi(digits as i32);
    (value * scale).round() / scale
}

#[derive(Deserialize)]
struct SignalsQuery {
    #[serde(default = "default_signals_limit")]
    limit: i64,
    #[serde(default)]
    symbol: String,
    #[serde(default)]
    decision: String,
    #[serde(default)]
    direction: String,
}

fn default_signals_limit() -> i64 {
    120
}

async fn signals(State(db): State<Db>, Query(q): Query<SignalsQuery>) -> Json<Value> {
    let limit = q.limit.clamp(1, 1000);
    let mode = current_mode(&db);
    let conn = db.conn.lock().expect("db");
    let mut where_sql = "WHERE mode = ?1".to_string();
    let mut args: Vec<String> = vec![mode];
    if !q.symbol.is_empty() {
        args.push(q.symbol.to_uppercase());
        where_sql.push_str(&format!(" AND symbol = ?{}", args.len()));
    }
    if !q.direction.is_empty() {
        args.push(q.direction.clone());
        where_sql.push_str(&format!(" AND direction = ?{}", args.len()));
    }
    let summary = signal_summary(&conn, &where_sql, &args);
    if !q.decision.is_empty() {
        args.push(q.decision.clone());
        where_sql.push_str(&format!(" AND decision = ?{}", args.len()));
    }
    args.push(limit.to_string());
    let sql = format!(
        "SELECT id, ts, symbol, direction, score, features, ai_action, ai_confidence, ai_reason, decision, \
         reject_reason, repeats FROM signals {where_sql} ORDER BY ts DESC LIMIT ?{}",
        args.len()
    );
    let mut stmt = conn.prepare(&sql).expect("prepare signals");
    let params = rusqlite::params_from_iter(args.iter());
    let items: Vec<Value> = stmt
        .query_map(params, |r| {
            let features: String = r.get(5)?;
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "ts": r.get::<_, i64>(1)?,
                "symbol": r.get::<_, String>(2)?,
                "direction": r.get::<_, String>(3)?,
                "score": r.get::<_, f64>(4)?,
                "features": serde_json::from_str::<Value>(&features).unwrap_or(json!({})),
                "ai_action": r.get::<_, String>(6)?,
                "ai_confidence": r.get::<_, f64>(7)?,
                "ai_reason": r.get::<_, String>(8)?,
                "decision": r.get::<_, String>(9)?,
                "reject_reason": r.get::<_, String>(10)?,
                "repeats": r.get::<_, i64>(11)?.max(1),
            }))
        })
        .expect("query signals")
        .collect::<Result<_, _>>()
        .expect("signal rows");
    json!({ "items": items, "summary": summary, "limit": limit }).into()
}

fn signal_summary(conn: &Connection, where_sql: &str, args: &[String]) -> Value {
    let count_of = |decision: &str| -> i64 {
        let sql = format!(
            "SELECT COUNT(*) FROM signals {where_sql} AND decision = ?{}",
            args.len() + 1
        );
        let mut params = args.to_vec();
        params.push(decision.to_string());
        conn.query_row(&sql, rusqlite::params_from_iter(params.iter()), |r| {
            r.get(0)
        })
        .unwrap_or(0)
    };
    let total: i64 = conn
        .query_row(
            &format!("SELECT COUNT(*) FROM signals {where_sql}"),
            rusqlite::params_from_iter(args.iter()),
            |r| r.get(0),
        )
        .unwrap_or(0);
    let scans: i64 = conn
        .query_row(
            &format!("SELECT COALESCE(SUM(repeats), 0) FROM signals {where_sql}"),
            rusqlite::params_from_iter(args.iter()),
            |r| r.get(0),
        )
        .unwrap_or(0);
    let mut reason_stmt = conn
        .prepare(&format!(
            "SELECT reject_reason, repeats FROM signals {where_sql} AND decision = 'rejected' AND reject_reason != ''"
        ))
        .expect("prepare reasons");
    let reasons: Vec<(String, i64)> = reason_stmt
        .query_map(rusqlite::params_from_iter(args.iter()), |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .expect("query reasons")
        .collect::<Result<_, _>>()
        .expect("reason rows");
    let mut weighted: std::collections::BTreeMap<String, i64> = std::collections::BTreeMap::new();
    for (reason, repeats) in reasons {
        *weighted.entry(reason).or_insert(0) += repeats.max(1);
    }
    let mut top: Vec<(&String, &i64)> = weighted.iter().collect();
    top.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    let top_reasons: Vec<Value> = top
        .into_iter()
        .take(8)
        .map(|(reason, count)| json!({"reason": reason, "count": count}))
        .collect();
    let since = crate::now() - 7 * 86400;
    let mut hour_stmt = conn
        .prepare(&format!(
            "SELECT CAST(strftime('%H', ts, 'unixepoch') AS INTEGER), COALESCE(SUM(repeats), 0) \
             FROM signals {where_sql} AND ts >= ?{} GROUP BY 1",
            args.len() + 1
        ))
        .expect("prepare hours");
    let mut hour_args = args.to_vec();
    hour_args.push(since.to_string());
    let mut buckets = [0_i64; 24];
    let hour_rows = hour_stmt
        .query_map(rusqlite::params_from_iter(hour_args.iter()), |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
        })
        .expect("query hours")
        .collect::<Result<Vec<_>, _>>()
        .expect("hour rows");
    for (hour, count) in hour_rows {
        if (0..24).contains(&hour) {
            buckets[hour as usize] = count;
        }
    }
    let by_hour: Vec<Value> = buckets
        .iter()
        .enumerate()
        .map(|(hour, count)| json!({"hour": hour, "count": count}))
        .collect();
    json!({
        "total": total,
        "opened": count_of("opened"),
        "rejected": count_of("rejected"),
        "flat": count_of("flat"),
        "pending": count_of("pending"),
        "scans": scans,
        "top_reasons": top_reasons,
        "by_hour": by_hour,
    })
}

#[derive(Deserialize)]
struct EquityQuery {
    #[serde(default = "default_hours")]
    hours: i64,
}

fn default_hours() -> i64 {
    72
}

async fn equity(State(db): State<Db>, Query(q): Query<EquityQuery>) -> Json<Value> {
    let hours = q.hours.clamp(1, 720);
    let since = crate::now() - hours * 3600;
    let mode = current_mode(&db);
    let conn = db.conn.lock().expect("db");
    let mut stmt = conn
        .prepare(
            "SELECT ts, wallet, available, unrealized, margin_used, positions, mode \
             FROM equity_points WHERE ts >= ?1 AND mode = ?2 ORDER BY ts ASC",
        )
        .expect("prepare equity");
    let items: Vec<Value> = stmt
        .query_map(rusqlite::params![since, mode], |r| {
            Ok(json!({
                "ts": r.get::<_, i64>(0)?,
                "wallet": round_n(r.get(1)?, 6),
                "available": round_n(r.get(2)?, 6),
                "unrealized": round_n(r.get(3)?, 6),
                "margin_used": round_n(r.get(4)?, 6),
                "positions": r.get::<_, i64>(5)?,
                "mode": r.get::<_, String>(6)?,
            }))
        })
        .expect("query equity")
        .collect::<Result<_, _>>()
        .expect("equity rows");
    json!({ "items": items, "hours": hours }).into()
}

#[derive(Deserialize)]
struct OrdersQuery {
    #[serde(default = "default_orders_limit")]
    limit: i64,
}

fn default_orders_limit() -> i64 {
    80
}

async fn orders(State(db): State<Db>, Query(q): Query<OrdersQuery>) -> Json<Value> {
    let limit = q.limit.clamp(1, 500);
    let mode = current_mode(&db);
    let conn = db.conn.lock().expect("db");
    let mut stmt = conn
        .prepare(
            "SELECT id, ts, symbol, side, order_type, qty, price, status, purpose, client_order_id, \
             exchange_order_id, reduce_only, mode FROM orders \
             WHERE mode = ?1 AND purpose = 'entry' ORDER BY ts DESC LIMIT ?2",
        )
        .expect("prepare orders");
    let items: Vec<Value> = stmt
        .query_map(rusqlite::params![mode, limit], |r| {
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "ts": r.get::<_, i64>(1)?,
                "symbol": r.get::<_, String>(2)?,
                "side": r.get::<_, String>(3)?,
                "order_type": r.get::<_, String>(4)?,
                "qty": r.get::<_, f64>(5)?,
                "price": r.get::<_, f64>(6)?,
                "status": r.get::<_, String>(7)?,
                "purpose": r.get::<_, String>(8)?,
                "client_order_id": r.get::<_, String>(9)?,
                "exchange_order_id": r.get::<_, String>(10)?,
                "reduce_only": r.get::<_, i64>(11)? != 0,
                "mode": r.get::<_, String>(12)?,
            }))
        })
        .expect("query orders")
        .collect::<Result<_, _>>()
        .expect("order rows");
    json!({ "items": items, "source": "local" }).into()
}

/// 交易所仓位快照超过这个秒数就回退到按标记价估算，对齐 `position_view.VENUE_FRESH_SEC`。
const VENUE_FRESH_SEC: i64 = 90;
const MMR: f64 = 0.005;

#[derive(Deserialize)]
struct PositionsQuery {
    #[serde(default)]
    include_closed: bool,
    #[serde(default = "default_positions_limit")]
    limit: i64,
}

fn default_positions_limit() -> i64 {
    50
}

async fn positions(State(db): State<Db>, Query(q): Query<PositionsQuery>) -> Json<Value> {
    let limit = q.limit.clamp(1, 500);
    let mode = current_mode(&db);
    let conn = db.conn.lock().expect("db");
    let status_filter = if q.include_closed {
        String::new()
    } else {
        "AND status = 'open'".to_string()
    };
    let sql = format!(
        "SELECT id, symbol, side, qty, entry_price, mark_price, leverage, margin, sl_price, tp_price, \
         opened_at, status, mode, venue_unrealized, venue_ts, partial_taken_at \
         FROM positions WHERE mode = ?1 {status_filter} ORDER BY opened_at DESC LIMIT ?2"
    );
    let now = crate::now();
    let mut stmt = conn.prepare(&sql).expect("prepare positions");
    let items: Vec<Value> = stmt
        .query_map(rusqlite::params![mode, limit], |r| {
            let side: String = r.get(2)?;
            let qty: f64 = r.get(3)?;
            let entry: f64 = r.get(4)?;
            let mark: f64 = r.get(5)?;
            let leverage: f64 = r.get(6)?;
            let margin: f64 = r.get(7)?;
            let venue_unrealized: f64 = r.get(13)?;
            let venue_ts: i64 = r.get(14)?;
            let partial_at: i64 = r.get(15)?;
            position_row(
                r,
                &side,
                qty,
                entry,
                mark,
                leverage,
                margin,
                venue_unrealized,
                venue_ts,
                partial_at,
                now,
            )
        })
        .expect("query positions")
        .collect::<Result<_, _>>()
        .expect("position rows");
    json!({ "items": items }).into()
}

#[allow(clippy::too_many_arguments)]
fn position_row(
    r: &rusqlite::Row<'_>,
    side: &str,
    qty: f64,
    entry: f64,
    mark: f64,
    leverage: f64,
    margin: f64,
    venue_unrealized: f64,
    venue_ts: i64,
    partial_at: i64,
    now: i64,
) -> rusqlite::Result<Value> {
    let direction = if side == "long" { 1.0 } else { -1.0 };
    let mark = if mark == 0.0 { entry } else { mark };
    let fresh = venue_ts > 0 && now - venue_ts < VENUE_FRESH_SEC;
    let unrealized = if fresh {
        venue_unrealized
    } else {
        (mark - entry) * qty * direction
    };
    let leverage = leverage.max(1.0);
    let liq = if entry == 0.0 {
        0.0
    } else if side == "long" {
        (entry * (1.0 - 1.0 / leverage + MMR)).max(0.0)
    } else {
        entry * (1.0 + 1.0 / leverage - MMR)
    };
    Ok(json!({
        "id": r.get::<_, i64>(0)?,
        "symbol": r.get::<_, String>(1)?,
        "side": side,
        "qty": qty,
        "entry_price": entry,
        "mark_price": mark,
        "leverage": leverage,
        "margin": margin,
        "sl_price": r.get::<_, f64>(8)?,
        "tp_price": r.get::<_, f64>(9)?,
        "opened_at": r.get::<_, i64>(10)?,
        "status": r.get::<_, String>(11)?,
        "mode": r.get::<_, String>(12)?,
        "unrealized": (unrealized * 1e8).round() / 1e8,
        "unrealized_source": if fresh { "exchange" } else { "estimated" },
        "notional": ((qty * mark) * 1e6).round() / 1e6,
        "roi_pct": if margin == 0.0 { 0.0 } else { (unrealized / margin * 1000.0).round() / 10.0 },
        "liquidation_price": (liq * 1e8).round() / 1e8,
        "liquidation_estimated": true,
        "partial_taken_at": partial_at,
        "partial_tp": partial_at > 0,
    }))
}

#[derive(Deserialize)]
struct ControlIn {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    confirm: String,
}

async fn control(State(db): State<Db>, Json(body): Json<ControlIn>) -> (StatusCode, Json<Value>) {
    if let Some(mode) = &body.mode {
        let target = mode.trim().to_lowercase();
        if target != "demo" && target != "live" {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"detail": "交易环境只能是 demo(模拟盘) 或 live(实盘)"})),
            );
        }
        if target == "live" && body.confirm.trim().to_uppercase() != "LIVE" {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"detail": "切换到实盘需要输入确认短语 LIVE"})),
            );
        }
        write_state(&db, "mode", &target);
    }
    if let Some(enabled) = body.enabled {
        write_state(&db, "enabled", if enabled { "true" } else { "false" });
    }
    (
        StatusCode::OK,
        Json(json!({"ok": true, "status": read_status(&db)})),
    )
}

/// 把日亏基准和回撤峰值对齐到当前权益。
///
/// 充值或换账户后旧基准会算出假的日内亏损并熔断，这个坑真实发生过两次。
/// 只解除「日内亏损」和「回撤」两类熔断；连亏、止损熔断是定时自愈的，不碰。
/// 权益取本地最近一次采样，交易所实时权益要等凭证接入。
async fn reset_anchors(State(db): State<Db>) -> (StatusCode, Json<Value>) {
    let conn = db.conn.lock().expect("db");
    let equity: f64 = conn
        .query_row(
            "SELECT value FROM engine_state WHERE key = 'equity'",
            [],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.0);
    if equity <= 0.0 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"detail": "当前权益为 0，不能作为日亏基准"})),
        );
    }
    let read = |key: &str| -> String {
        conn.query_row(
            "SELECT value FROM engine_state WHERE key = ?1",
            [key],
            |r| r.get(0),
        )
        .unwrap_or_default()
    };
    let before = json!({
        "day_start_equity": read("day_start_equity"),
        "equity_peak": read("equity_peak"),
        "halted": read("halted"),
    });
    let now = crate::now();
    let value = format!("{equity:.8}");
    for key in ["day_start_equity", "equity_peak"] {
        conn.execute(
            "INSERT INTO engine_state(key, value, updated_at) VALUES(?1, ?2, ?3) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            (key, &value, now),
        )
        .expect("reset anchor");
    }
    conn.execute(
        "INSERT INTO engine_state(key, value, updated_at) VALUES('peak_anchor_ts', ?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        (now.to_string(), now),
    )
    .expect("reset peak anchor");
    let halted = before["halted"].as_str().unwrap_or("");
    let cleared = if halted.contains("日内亏损") || halted.contains("回撤") {
        halted
    } else {
        ""
    };
    if !cleared.is_empty() {
        conn.execute(
            "UPDATE engine_state SET value = '', updated_at = ?1 WHERE key = 'halted'",
            [now],
        )
        .expect("clear halt");
    }
    (
        StatusCode::OK,
        Json(json!({"ok": true, "equity": equity, "before": before, "cleared_halt": cleared})),
    )
}

async fn status(State(db): State<Db>) -> Json<Value> {
    Json(read_status(&db))
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// 执行质量视图：延迟读数来自执行内核七段指标（未连接时保持 null，前端显示「—」）。
fn execution_view(segments: &Value) -> Value {
    let total = &segments["total"];
    json!({
        "latency_p50_ms": total["p50"].clone(),
        "latency_p95_ms": total["p95"].clone(),
        "latency_p99_ms": total["p99"].clone(),
        "order_count": total["count"].as_u64().unwrap_or(0),
        "segments": segments,
    })
}

async fn stats(State(db): State<Db>) -> Json<Value> {
    let mode = current_mode(&db);
    let segments = match crate::exec_client::global() {
        Some(client) => client
            .query_segments()
            .await
            .ok()
            .unwrap_or(Value::Null),
        None => Value::Null,
    };
    let rows: Vec<(String, f64, i64, String)> = {
        let conn = db.conn.lock().expect("db");
        let mut stmt = conn
            .prepare(
                "SELECT symbol, pnl, hold_seconds, exit_reason FROM trades \
                 WHERE mode=?1 ORDER BY closed_at DESC LIMIT 50",
            )
            .expect("prepare stats");
        stmt.query_map([&mode], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .expect("query stats")
        .flatten()
        .collect()
    };
    let mut wins = 0_i64;
    let mut losses = 0_i64;
    let mut gross_profit = 0.0;
    let mut gross_loss = 0.0;
    let mut hold = 0_i64;
    let mut by_symbol: std::collections::BTreeMap<String, (i64, i64, f64)> =
        std::collections::BTreeMap::new();
    let mut by_reason: std::collections::BTreeMap<String, (i64, f64)> =
        std::collections::BTreeMap::new();
    for (symbol, pnl, seconds, reason) in &rows {
        if *pnl > 0.0 {
            wins += 1;
            gross_profit += pnl;
        } else if *pnl < 0.0 {
            losses += 1;
            gross_loss += pnl.abs();
        }
        hold += seconds;
        let symbol_row = by_symbol.entry(symbol.clone()).or_insert((0, 0, 0.0));
        symbol_row.0 += 1;
        symbol_row.1 += i64::from(*pnl > 0.0);
        symbol_row.2 += pnl;
        let reason_row = by_reason.entry(reason.clone()).or_insert((0, 0.0));
        reason_row.0 += 1;
        reason_row.1 += pnl;
    }
    let count = rows.len() as f64;
    let win_rate = if count == 0.0 {
        0.0
    } else {
        wins as f64 / count * 100.0
    };
    let profit_factor = if gross_loss > 0.0 {
        Value::from(round2(gross_profit / gross_loss))
    } else {
        Value::Null
    };
    let symbols = by_symbol
        .into_iter()
        .map(|(symbol, (n, symbol_wins, pnl))| {
            (
                symbol,
                json!({"n": n, "wins": symbol_wins, "pnl": round2(pnl)}),
            )
        })
        .collect::<serde_json::Map<String, Value>>();
    let reasons = by_reason
        .into_iter()
        .map(|(reason, (n, pnl))| (reason, json!({"n": n, "pnl": round2(pnl)})))
        .collect::<serde_json::Map<String, Value>>();
    Json(json!({
        "trades": rows.len(),
        "wins": wins,
        "losses": losses,
        "win_rate": round2(win_rate),
        "pnl": round2(rows.iter().map(|row| row.1).sum()),
        "avg_hold_seconds": if count == 0.0 { 0.0 } else { round2(hold as f64 / count) },
        "profit_factor": profit_factor,
        "by_symbol": symbols,
        "by_reason": reasons,
        "engine": read_status(&db),
        "execution": execution_view(&segments),
    }))
}

async fn execution(State(_db): State<Db>) -> Json<Value> {
    let segments = match crate::exec_client::global() {
        Some(client) => client
            .query_segments()
            .await
            .ok()
            .unwrap_or(Value::Null),
        None => Value::Null,
    };
    let total = &segments["total"];
    Json(json!({
        "count": total["count"].as_u64().unwrap_or(0),
        "latency_ms": {
            "p50": total["p50"].clone(),
            "p95": total["p95"].clone(),
            "p99": total["p99"].clone(),
            "max": total["max"].clone(),
        },
        "segments": segments,
    }))
}

fn write_state(db: &Db, key: &str, value: &str) {
    let conn = db.conn.lock().expect("db");
    let now = crate::now();
    conn.execute(
        "INSERT INTO engine_state(key, value, updated_at) VALUES(?1, ?2, ?3) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        (key, value, now),
    )
    .expect("upsert engine_state");
    // 已持有连接锁，不能再走 db.setting()（它会再次加锁，把请求卡死）。
    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM app_settings WHERE key = 'engine'",
            [],
            |r| r.get(0),
        )
        .ok();
    let mut group: Value = raw
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(json!({}));
    let json_value = match value {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        other => Value::String(other.to_string()),
    };
    if let Some(obj) = group.as_object_mut() {
        obj.insert(key.to_string(), json_value);
    }
    conn.execute(
        "INSERT INTO app_settings(key, value, updated_at) VALUES('engine', ?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        (group.to_string(), now),
    )
    .expect("upsert engine settings");
}

fn current_mode(db: &Db) -> String {
    let conn = db.conn.lock().expect("db");
    conn.query_row(
        "SELECT value FROM engine_state WHERE key = 'mode'",
        [],
        |r| r.get::<_, String>(0),
    )
    .ok()
    .filter(|m| !m.is_empty())
    .unwrap_or_else(|| "demo".to_string())
}

async fn admission(State(db): State<Db>) -> Json<Value> {
    Json(evaluate_admission(&db))
}

fn evaluate_admission(db: &Db) -> Value {
    let conn = db.conn.lock().expect("db");
    let group = |name: &str| -> Value {
        let raw: Option<String> = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key=?1",
                [name],
                |row| row.get(0),
            )
            .ok();
        let stored = raw
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .unwrap_or(Value::Null);
        let mut merged = crate::settings_api::defaults_of(name);
        if let (Some(base), Some(extra)) = (merged.as_object_mut(), stored.as_object()) {
            for (key, value) in extra {
                base.insert(key.clone(), value.clone());
            }
        }
        merged
    };
    let state = |key: &str| -> String {
        conn.query_row(
            "SELECT value FROM engine_state WHERE key=?1",
            [key],
            |row| row.get(0),
        )
        .unwrap_or_default()
    };
    let engine = group("engine");
    let risk = group("risk");
    let ai = group("ai");
    let strategy = group("strategy");
    let data = group("data");
    let mode = {
        let stored = state("mode");
        if stored == "live" || stored == "demo" {
            stored
        } else {
            engine["mode"].as_str().unwrap_or("demo").to_string()
        }
    };
    let snapshot: Value =
        serde_json::from_str(&state(&format!("snapshot.{mode}"))).unwrap_or(Value::Null);
    let equity = snapshot["account"]["equity"].as_f64().unwrap_or(0.0);
    let available = snapshot["account"]["available_balance"]
        .as_f64()
        .unwrap_or(0.0);
    let account_ok = snapshot["account"].is_object();
    let prefix = if mode == "live" { "live" } else { "demo" };
    let credential = |name: &str| {
        data.get(&format!("{prefix}_api_{name}_enc"))
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
    };
    let key_ok = credential("key") && credential("secret");
    let label = if mode == "demo" {
        "模拟盘（币安测试网）"
    } else {
        "实盘（币安主网）"
    };
    let mut steps = vec![step(
        "channel",
        "交易通道",
        key_ok && account_ok,
        if !key_ok {
            format!("{label} 缺少 API Key/Secret")
        } else if account_ok {
            format!("{label} 凭证已配置，最近一次账户快照可用")
        } else {
            format!("{label} 凭证已配置，但还没有可用的账户快照")
        },
        "fail",
        "#/strategy?group=data",
        "配置当前环境密钥",
    )];
    let ai_enabled = ai["enabled"].as_bool().unwrap_or(false);
    let models: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM ai_providers WHERE enabled != 0",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    steps.push(step(
        "ai",
        "AI 复核",
        !ai_enabled || models > 0,
        if !ai_enabled {
            "AI 已关闭: 纯量化决策, 不影响开仓".into()
        } else if models > 0 {
            "已接入可用模型, 开仓前会做方向/杠杆复核".into()
        } else {
            "AI 已开启但未配置模型: 当前自动回退纯量化 (不会阻塞开仓)".into()
        },
        "warn",
        "#/ai",
        "接入模型",
    ));
    let symbols = load_symbol_rows(&conn);
    let enabled: Vec<_> = symbols.into_iter().filter(|row| row.enabled).collect();
    steps.push(step(
        "pool",
        "标的池",
        !enabled.is_empty(),
        if enabled.is_empty() {
            "标的池为空, 引擎不会扫描任何品种".into()
        } else {
            format!(
                "已启用 {} 个标的: {}",
                enabled.len(),
                enabled
                    .iter()
                    .take(6)
                    .map(|row| row.symbol.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        },
        "fail",
        "#/symbols",
        "管理标的池",
    ));
    let margin_pct = risk["margin_pct"].as_f64().unwrap_or(50.0);
    let leverage_floor = risk["min_leverage"].as_f64().unwrap_or(1.0).max(1.0);
    let leverage_cap = risk["max_leverage"]
        .as_f64()
        .unwrap_or(100.0)
        .max(leverage_floor);
    let allocatable_low = equity * margin_pct / 100.0 * leverage_floor;
    let allocatable_high = equity * margin_pct / 100.0 * leverage_cap;
    let min_equity = risk["min_equity_usdt"].as_f64().unwrap_or(0.5);
    steps.push(step(
        "funds",
        "资金与风控",
        account_ok && equity >= min_equity && available > 0.0,
        if account_ok {
            format!(
                "权益 {equity:.4} USDT (下限 {min_equity}), 单笔可分配名义额约 {allocatable_low:.2}–{allocatable_high:.2} USDT（杠杆由 AI 在区间内选择）"
            )
        } else {
            "账户读取失败: 还没有可用的账户快照".into()
        },
        "fail",
        "#/strategy?group=risk",
        "调整风控参数",
    ));
    let enabled_flag = state("enabled") == "true";
    // 后台是否真的在跑：只看 enabled 会把「开关开着但任务已经死了」说成运行中。
    let last_cycle = state("last_cycle_ts").parse::<i64>().unwrap_or(0);
    let cycle_interval_sec = crate::runtime::cycle_interval_sec(&conn);
    let liveness_threshold_sec = crate::runtime::liveness_threshold_sec(cycle_interval_sec);
    let heartbeat_age = if last_cycle > 0 {
        (crate::now() - last_cycle).max(0)
    } else {
        -1
    };
    let engine_alive = (0..liveness_threshold_sec).contains(&heartbeat_age);
    let windows = engine["trading_windows"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|| vec!["00:00-24:00".into()]);
    let offset = engine["timezone_offset"]
        .as_i64()
        .unwrap_or(8)
        .clamp(-12, 14);
    let local = crate::now() + offset * 3600;
    let minutes = local.rem_euclid(86_400) / 60;
    let window_ok = crate::risk::trading_window_open(&windows, minutes);
    let consecutive: i64 = state("consecutive_losses").parse().unwrap_or(0);
    let loss_limit = risk["consecutive_loss_limit"].as_i64().unwrap_or(4);
    let halted = state("halted");
    let halted_on = !halted.is_empty() && halted != "0" && halted != "false";
    // 全局冷却：任何标的亏损平仓后写一条 kind='global' 的 cooldown，冷却内禁止新开仓。
    let global_cooldown_until = conn
        .query_row(
            "SELECT until_ts FROM cooldowns WHERE kind='global' AND until_ts>?1 \
             ORDER BY until_ts DESC LIMIT 1",
            [crate::now()],
            |row| row.get::<_, i64>(0),
        )
        .ok();
    let global_cooldown = global_cooldown_until.is_some();
    let engine_ok = enabled_flag
        && engine_alive
        && window_ok
        && consecutive < loss_limit
        && !halted_on
        && !global_cooldown;
    let mut details = Vec::new();
    if !enabled_flag {
        details.push("引擎未启动 (总览页点「启动引擎」)".to_string());
    } else if !engine_alive {
        // 这是 warn 级别：不拦开仓，但必须说清楚「开关开着 ≠ 引擎在跑」。
        details.push(if last_cycle > 0 {
            format!(
                "引擎开关已开但后台没有心跳（上次扫描 {heartbeat_age} 秒前，判活阈值 {liveness_threshold_sec} 秒，扫描周期 {cycle_interval_sec} 秒）：服务重启后引擎未恢复，或后台任务已退出"
            )
        } else {
            format!(
                "引擎开关已开但后台从未跑过一轮（判活阈值 {liveness_threshold_sec} 秒，扫描周期 {cycle_interval_sec} 秒）：服务重启后引擎未恢复，或后台任务已退出"
            )
        });
    }
    if !window_ok {
        details.push("当前不在允许开仓的时段".to_string());
    }
    if consecutive >= loss_limit {
        details.push(format!("连亏 {consecutive} 笔触发冷静期"));
    }
    if halted_on {
        details.push(format!("已熔断: {}", &halted[..halted.len().min(60)]));
    }
    if let Some(until) = global_cooldown_until {
        details.push(format!("全局冷却中，剩余 {}s", (until - crate::now()).max(0)));
    }
    steps.push(step(
        "engine",
        "引擎与交易时段",
        engine_ok,
        if details.is_empty() {
            "引擎运行中, 处于允许开仓的时段".into()
        } else {
            details.join("; ")
        },
        "warn",
        "#/strategy?group=engine",
        "调整时段/间隔",
    ));
    let params = crate::risk::SizeParams {

        margin_pct,
        min_notional_buffer: risk["min_notional_buffer"].as_f64().unwrap_or(1.05),
    };
    let max_leverage = risk["max_leverage"].as_i64().unwrap_or(100);
    let min_leverage = risk["min_leverage"]
        .as_i64()
        .unwrap_or(1)
        .clamp(1, max_leverage);
    let mut rows = Vec::new();
    for symbol in &enabled {
        let market = snapshot["markets"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|item| item["symbol"].as_str() == Some(symbol.symbol.as_str()));
        let Some(price) = market
            .and_then(|item| item["mark_price"].as_f64())
            .filter(|v| *v > 0.0)
        else {
            rows.push(json!({
                "symbol": symbol.symbol, "tradeable": false, "reason": "缺少交易规则或行情",
                "max_leverage": 0, "min_notional": Value::Null, "leverage_used": 0,
                "required_margin": 0.0, "suggested_leverage": 0,
            }));
            continue;
        };
        let filter = symbol.filter();
        let upper = 1.max(max_leverage.min(symbol.max_leverage).min(100));
        // 自检按 AI 未给杠杆时的回退值（下限）估算，避免夸大仓位
        let leverage = min_leverage.min(upper).max(1);
        let available_for_size = if account_ok { Some(available) } else { None };
        let (qty, note) = crate::risk::estimate_qty(
            equity,
            price,
            leverage as f64,
            &params,
            &filter,
            available_for_size,
        );
        let need =
            filter.effective_min_notional(price) * params.min_notional_buffer.clamp(1.0, 3.0);
        rows.push(json!({
            "symbol": symbol.symbol,
            "tradeable": qty > 0.0,
            "reason": if qty > 0.0 { "" } else { note.as_str() },
            "price": price,
            "max_leverage": symbol.max_leverage,
            "leverage_used": leverage,
            "min_qty": filter.effective_min_qty(price),
            "min_notional": (need * 10_000.0).round() / 10_000.0,
            "required_margin": (need / leverage as f64 * 10_000.0).round() / 10_000.0,
            "allocatable_notional": (equity * margin_pct / 100.0 * leverage as f64 * 10_000.0).round() / 10_000.0,
            "suggested_leverage": 0,
            "qty_if_open": qty,
            "notional_if_open": if qty > 0.0 { (qty * price * 10_000.0).round() / 10_000.0 } else { 0.0 },
        }));
    }
    let tradeable = rows.iter().filter(|row| row["tradeable"] == true).count();
    steps.push(step(
        "symbols",
        "可交易标的",
        tradeable > 0,
        if tradeable > 0 {
            format!("{tradeable}/{} 个标的在当前权益下可下单", rows.len())
        } else if rows.is_empty() {
            "标的池为空".into()
        } else {
            "当前权益不足以满足任何标的的最小名义额".into()
        },
        "warn",
        "#/symbols",
        "优化标的池",
    ));
    let blocking: Vec<_> = steps
        .iter()
        .filter(|item| item["ok"] == false && item["level"] == "fail")
        .map(|item| item["key"].clone())
        .collect();
    let warnings: Vec<_> = steps
        .iter()
        .filter(|item| item["ok"] == false && item["level"] == "warn")
        .map(|item| item["key"].clone())
        .collect();
    let passed = steps.iter().filter(|item| item["ok"] == true).count();
    let open_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM positions WHERE status='open' AND mode=?1",
            [&mode],
            |row| row.get(0),
        )
        .unwrap_or(0);
    json!({
        "ok": blocking.is_empty(),
        "can_open": tradeable > 0 && blocking.is_empty() && enabled_flag && !global_cooldown,
        "score": (passed as f64 / steps.len() as f64 * 1000.0).round() / 1000.0,
        "mode": mode,
        "steps": steps,
        "blocking": blocking,
        "warnings": warnings,
        "account": {
            "equity": (equity * 1_000_000.0).round() / 1_000_000.0,
            "available": (available * 1_000_000.0).round() / 1_000_000.0,
            "positions": open_count,
            "max_positions": engine["max_positions"].as_i64().unwrap_or(2),
        },
        "params": {

            "margin_pct": margin_pct,
            "min_leverage": min_leverage,
            "max_leverage": max_leverage,
            "sl_estimate_pct": 0.5,
            "timeframe": strategy["timeframe"].as_str().unwrap_or("15m"),
            "entry_threshold": strategy["entry_threshold"].as_f64().unwrap_or(0.3),
        },
        "symbols": rows,
        "tradeable_count": tradeable,
        "evaluated_at": crate::now(),
    })
}

struct SymbolRow {
    symbol: String,
    enabled: bool,
    max_leverage: i64,
    tick_sz: String,
    min_sz: String,
    min_notional: f64,
}

impl SymbolRow {
    fn filter(&self) -> crate::live::SymbolFilter {
        crate::live::SymbolFilter {
            qty_step: self.min_sz.parse().unwrap_or(0.001),
            min_qty: self.min_sz.parse().unwrap_or(0.001),
            price_tick: self.tick_sz.parse().unwrap_or(0.01),
            min_notional: self.min_notional,
        }
    }
}

fn load_symbol_rows(conn: &Connection) -> Vec<SymbolRow> {
    let mut stmt = match conn.prepare(
        "SELECT symbol, enabled, max_leverage, tick_sz, min_sz, min_notional FROM trading_symbols",
    ) {
        Ok(stmt) => stmt,
        Err(_) => return Vec::new(),
    };
    stmt.query_map([], |row| {
        Ok(SymbolRow {
            symbol: row.get(0)?,
            enabled: row.get::<_, i64>(1)? != 0,
            max_leverage: row.get(2)?,
            tick_sz: row.get(3)?,
            min_sz: row.get(4)?,
            min_notional: row.get(5)?,
        })
    })
    .map(|rows| rows.filter_map(Result::ok).collect())
    .unwrap_or_default()
}

fn step(
    key: &str,
    title: &str,
    ok: bool,
    detail: String,
    level: &str,
    action: &str,
    action_text: &str,
) -> Value {
    json!({
        "key": key,
        "title": title,
        "ok": ok,
        "level": if ok { "ok" } else { level },
        "detail": detail,
        "action": action,
        "action_text": action_text,
    })
}

/// 引擎运行态。前端总览、引擎控制都读这一份，
/// 账户权益、持仓、统计必须在这里齐活——少一个字段页面就显示 0。
fn read_status(db: &Db) -> Value {
    let conn = db.conn.lock().expect("db");
    let mut stmt = conn
        .prepare("SELECT key, value FROM engine_state")
        .expect("prepare state");
    let rows: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    let get = |k: &str| {
        rows.iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    let mode = get("mode");
    let mode = if mode.is_empty() {
        "demo".to_string()
    } else {
        mode
    };
    let last_cycle = get("last_cycle_ts").parse::<i64>().unwrap_or(0);
    let now = crate::now();
    // 心跳年龄：后台任务还在跑就是新鲜的。引擎停用时后台仍然心跳，
    // 所以 running 与 enabled 要分开看。
    let heartbeat_age = if last_cycle > 0 {
        (now - last_cycle).max(0)
    } else {
        -1
    };
    // 判活阈值跟着扫描周期走：周期 10 分钟时，两轮之间的间歇期长达近 10 分钟，
    // 阈值写死 180 秒会把间歇期误报成「引擎停了」。
    let cycle_interval_sec = crate::runtime::cycle_interval_sec(&conn);
    let liveness_threshold_sec = crate::runtime::liveness_threshold_sec(cycle_interval_sec);
    let running = (0..liveness_threshold_sec).contains(&heartbeat_age);
    let started_at = get("service_started_at").parse::<i64>().unwrap_or(0);
    json!({
        "enabled": get("enabled") == "true",
        "mode": mode,
        "env": mode,
        "env_label": if mode == "live" { "实盘 · 币安主网" } else { "模拟盘 · 币安测试网" },
        "running": running,
        "heartbeat_age": heartbeat_age,
        "cycle_interval_sec": cycle_interval_sec,
        "liveness_threshold_sec": liveness_threshold_sec,
        // 后台任务自己的状态：alive / stopped / panic / lock_failed
        "runtime_task": get("runtime_task"),
        "uptime": if started_at > 0 { (now - started_at).max(0) } else { 0 },
        // 前端只靠 enabled 分辨不出「后台任务是否真的跑过周期」，带上心跳便于判断
        "last_cycle_ts": last_cycle,
        "last_cycle_ms": get("last_cycle_ms").parse::<i64>().unwrap_or(0),
        "last_error": get("last_error"),
        "halted": get("halted"),
        "account": account_view(&conn, &mode),
        "positions": open_positions(&conn, &mode),
        "stats": stats_view(&conn, &mode),
    })
}

/// 账户视图：权益 = 钱包 + 浮动盈亏。
/// 优先用最近一次账户快照；没有快照就用权益采样；再没有就用配置的初始资金，
/// 不能返回 0——0 会被当成「账户爆仓」。
fn account_view(conn: &rusqlite::Connection, mode: &str) -> Value {
    let now = crate::now();
    let mut margin_used = 0.0;
    let mut unrealized = 0.0;
    let mut position_count = 0_i64;
    if let Ok(mut stmt) = conn.prepare(
        "SELECT side, qty, entry_price, mark_price, margin, venue_unrealized, venue_ts \
         FROM positions WHERE mode=?1 AND status='open'",
    ) {
        if let Ok(rows) = stmt.query_map([mode], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, f64>(1)?,
                r.get::<_, f64>(2)?,
                r.get::<_, f64>(3)?,
                r.get::<_, f64>(4)?,
                r.get::<_, f64>(5)?,
                r.get::<_, i64>(6)?,
            ))
        }) {
            for row in rows.flatten() {
                let (side, qty, entry, mark, margin, venue_unrealized, venue_ts) = row;
                let direction = if side == "long" { 1.0 } else { -1.0 };
                let mark = if mark == 0.0 { entry } else { mark };
                let fresh = venue_ts > 0 && now - venue_ts < VENUE_FRESH_SEC;
                unrealized += if fresh {
                    venue_unrealized
                } else {
                    (mark - entry) * qty * direction
                };
                margin_used += margin;
                position_count += 1;
            }
        }
    }
    let snapshot: Option<Value> = conn
        .query_row(
            "SELECT value FROM engine_state WHERE key=?1",
            [format!("snapshot.{mode}")],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok());
    let snapshot_equity = snapshot
        .as_ref()
        .and_then(|value| value["account"]["equity"].as_f64());
    let live = conn
        .query_row(
            "SELECT value FROM engine_state WHERE key='account.live'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok());
    let live_fresh = live
        .as_ref()
        .and_then(|value| value["ts"].as_i64())
        .map(|ts| ts > 0 && now - ts < 10)
        .unwrap_or(false);
    let snapshot_wallet = live
        .as_ref()
        .filter(|_| live_fresh)
        .and_then(|value| value["wallet"].as_f64())
        .or_else(|| {
            snapshot
                .as_ref()
                .and_then(|value| value["account"]["wallet"].as_f64())
        });
    let snapshot_available = live
        .as_ref()
        .filter(|_| live_fresh)
        .and_then(|value| value["available"].as_f64())
        .or_else(|| {
            snapshot
                .as_ref()
                .and_then(|value| value["account"]["available_balance"].as_f64())
        });
    let point: Option<(f64, f64)> = conn
        .query_row(
            "SELECT wallet, available FROM equity_points WHERE mode=?1 ORDER BY ts DESC LIMIT 1",
            [mode],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok();
    let initial = conn
        .query_row(
            "SELECT value FROM app_settings WHERE key='engine'",
            [],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|value| value["initial_equity"].as_f64())
        .unwrap_or(1000.0);
    let (wallet, available, source) = match (snapshot_equity, point) {
        (Some(equity), _) => (
            snapshot_wallet.unwrap_or_else(|| (equity - unrealized).max(0.0)),
            snapshot_available.unwrap_or(0.0),
            if live_fresh { "live" } else { "snapshot" },
        ),
        (None, Some((wallet, available))) => (wallet, available, "ledger"),
        (None, None) => (initial, initial, "initial"),
    };
    let equity = wallet + unrealized;
    json!({
        "equity": round4(equity),
        "wallet": round4(wallet),
        "available": round4(available),
        "unrealized": round4(unrealized),
        "margin_used": round4(margin_used),
        "positions": position_count,
        "mode": mode,
        "source": source,
        "stale": false,
        "error": "",
    })
}

fn round4(value: f64) -> f64 {
    if !value.is_finite() {
        return 0.0;
    }
    (value * 10_000.0).round() / 10_000.0
}

/// 当前环境的在仓持仓，字段与 /engine/positions 一致。
fn open_positions(conn: &rusqlite::Connection, mode: &str) -> Vec<Value> {
    let now = crate::now();
    let Ok(mut stmt) = conn.prepare(
        "SELECT id, symbol, side, qty, entry_price, mark_price, leverage, margin, sl_price, tp_price, \
         opened_at, status, mode, venue_unrealized, venue_ts, partial_taken_at \
         FROM positions WHERE mode = ?1 AND status='open' ORDER BY opened_at DESC LIMIT 100",
    ) else {
        return Vec::new();
    };
    let rows = stmt.query_map([mode], |r| {
        let side: String = r.get(2)?;
        let qty: f64 = r.get(3)?;
        let entry: f64 = r.get(4)?;
        let mark: f64 = r.get(5)?;
        let leverage: f64 = r.get(6)?;
        let margin: f64 = r.get(7)?;
        let venue_unrealized: f64 = r.get(13)?;
        let venue_ts: i64 = r.get(14)?;
        let partial_at: i64 = r.get(15)?;
        position_row(
            r,
            &side,
            qty,
            entry,
            mark,
            leverage,
            margin,
            venue_unrealized,
            venue_ts,
            partial_at,
            now,
        )
    });
    match rows {
        Ok(rows) => rows.flatten().collect(),
        Err(_) => Vec::new(),
    }
}

/// 总览用的统计：已实现盈亏、胜率、连亏、冷却、死信、执行质量。
fn stats_view(conn: &rusqlite::Connection, mode: &str) -> Value {
    let mut trades = 0_i64;
    let mut wins = 0_i64;
    let mut losses = 0_i64;
    let mut realized_pnl = 0.0;
    if let Ok(mut stmt) =
        conn.prepare("SELECT pnl FROM trades WHERE mode=?1 ORDER BY closed_at DESC LIMIT 200")
    {
        if let Ok(rows) = stmt.query_map([mode], |r| r.get::<_, f64>(0)) {
            for pnl in rows.flatten() {
                trades += 1;
                realized_pnl += pnl;
                if pnl > 0.0 {
                    wins += 1;
                } else if pnl < 0.0 {
                    losses += 1;
                }
            }
        }
    }
    let win_rate = if trades == 0 {
        0.0
    } else {
        (wins as f64 / trades as f64 * 100.0 * 100.0).round() / 100.0
    };
    let consecutive_losses = conn
        .query_row(
            "SELECT value FROM engine_state WHERE key='consecutive_losses'",
            [],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(0);
    let guard_until = conn
        .query_row(
            "SELECT value FROM engine_state WHERE key='sl_guard_until'",
            [],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(0);
    let stoploss_guard_remaining = (guard_until - crate::now()).max(0);
    let mut cooldowns = serde_json::Map::new();
    if let Ok(mut stmt) =
        conn.prepare("SELECT symbol, until_ts, reason FROM cooldowns WHERE until_ts > ?1")
    {
        if let Ok(rows) = stmt.query_map([crate::now()], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
            ))
        }) {
            for row in rows.flatten() {
                let (symbol, until_ts, reason) = row;
                cooldowns.insert(
                    symbol,
                    json!({
                        "until_ts": until_ts,
                        "remaining": (until_ts - crate::now()).max(0),
                        "reason": reason,
                    }),
                );
            }
        }
    }
    let dead_pending = conn
        .query_row(
            "SELECT COUNT(*) FROM write_failures WHERE resolved=0",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0);
    let dead_stuck = conn
        .query_row(
            "SELECT COUNT(*) FROM write_failures WHERE resolved=0 AND auto_attempts >= 5",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0);
    let dead_last_ts = conn
        .query_row("SELECT COALESCE(MAX(ts),0) FROM write_failures", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap_or(0);
    let mut latencies: Vec<f64> = Vec::new();
    let mut slippage: Vec<f64> = Vec::new();
    if let Ok(mut stmt) =
        conn.prepare("SELECT key, value FROM engine_state WHERE key LIKE 'execution.%'")
    {
        if let Ok(rows) =
            stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        {
            for row in rows.flatten() {
                let (key, value) = row;
                let Ok(parsed) = value.parse::<f64>() else {
                    continue;
                };
                if !parsed.is_finite() {
                    continue;
                }
                if key.contains("slippage") {
                    slippage.push(parsed);
                } else {
                    latencies.push(parsed);
                }
            }
        }
    }
    latencies.sort_by(f64::total_cmp);
    slippage.sort_by(f64::total_cmp);
    let percentile = |values: &[f64], q: f64| -> f64 {
        if values.is_empty() {
            return 0.0;
        }
        let index = ((values.len() as f64 * q).floor() as usize).min(values.len() - 1);
        (values[index] * 100.0).round() / 100.0
    };
    let average = |values: &[f64]| -> f64 {
        if values.is_empty() {
            return 0.0;
        }
        let sum: f64 = values.iter().sum();
        (sum / values.len() as f64 * 100.0).round() / 100.0
    };
    json!({
        "trades": trades,
        "wins": wins,
        "losses": losses,
        "win_rate": win_rate,
        "realized_pnl": round4(realized_pnl),
        "consecutive_losses": consecutive_losses,
        "cooldowns": cooldowns,
        "stoploss_guard_remaining": stoploss_guard_remaining,
        "dead_letters": {
            "pending": dead_pending,
            "stuck": dead_stuck,
            "last_ts": dead_last_ts,
        },
        "execution": {
            "latency_p50_ms": percentile(&latencies, 0.50),
            "latency_p95_ms": percentile(&latencies, 0.95),
            "avg_slippage_bps": average(&slippage),
            "order_count": latencies.len() as i64,
        },
    })
}

/// 测试夹具：建一张最小可用的引擎库（positions / orders / trades / engine_state）。
///
/// 仅测试可见；正式构建不编译，避免把夹具当成可用 API。
#[cfg(test)]
fn seed(path: &std::path::Path) -> Db {
    let _ = std::fs::remove_file(path);
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE app_settings(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER); \
         CREATE TABLE engine_state(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER); \
         CREATE TABLE positions(id INTEGER PRIMARY KEY, symbol TEXT, side TEXT, qty REAL, entry_price REAL, \
         mark_price REAL, leverage REAL, margin REAL, sl_price REAL, tp_price REAL, peak_price REAL, \
         initial_sl_price REAL, signal_score REAL, opened_at INTEGER, closed_at INTEGER, exit_price REAL, \
         exit_reason TEXT, realized_pnl REAL, fees REAL, \
         status TEXT, mode TEXT, venue_unrealized REAL, venue_ts INTEGER, partial_taken_at INTEGER); \
         CREATE TABLE orders(id INTEGER PRIMARY KEY, ts INTEGER, symbol TEXT, side TEXT, order_type TEXT, \
         qty REAL, price REAL, status TEXT, purpose TEXT, client_order_id TEXT, exchange_order_id TEXT, \
         reduce_only INTEGER, mode TEXT); \
         CREATE TABLE trades(id INTEGER PRIMARY KEY, symbol TEXT, side TEXT, qty REAL, entry_price REAL, \
         exit_price REAL, leverage REAL, notional REAL, pnl REAL, pnl_pct REAL, roi_pct REAL, fees REAL, \
         opened_at INTEGER, closed_at INTEGER, hold_seconds INTEGER, exit_reason TEXT, ai_confidence REAL, \
         ai_reason TEXT, signal_score REAL, mode TEXT, reviewed INTEGER, review_note TEXT, mantra_ids TEXT); \
         INSERT INTO engine_state VALUES('enabled','false',0); \
         INSERT INTO engine_state VALUES('mode','demo',0); \
         INSERT INTO app_settings VALUES('engine','{\"enabled\":false,\"mode\":\"demo\"}',0);",
    )
    .unwrap();
    drop(conn);
    Db::open(path.to_str().unwrap()).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn post(app: Router, uri: &str, payload: &str) -> (StatusCode, Value) {
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(payload.to_string()))
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

    #[tokio::test]
    async fn execution_without_kernel_returns_nulls_instead_of_zeroes() {
        let path = std::env::temp_dir().join("nq-execution.db");
        let db = seed(&path);
        let app = Router::new().nest("/api/v1", routes()).with_state(db);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/engine/execution")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["count"], 0);
        assert!(
            body["latency_ms"]["p50"].is_null(),
            "没有内核样本时不能用 0 冒充: {body}"
        );
        assert!(body["latency_ms"]["p95"].is_null());
        assert!(body["latency_ms"]["p99"].is_null());
        assert!(body["latency_ms"]["max"].is_null());
        assert!(body["segments"].is_null());
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]

    async fn live_switch_requires_confirm_and_persists_both_stores() {
        let path = std::env::temp_dir().join("nq-engine.db");
        let db = seed(&path);
        let app = || {
            Router::new()
                .nest("/api/v1", routes())
                .with_state(db.clone())
        };

        let (status, body) = post(app(), "/api/v1/engine/control", r#"{"mode":"live"}"#).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "缺确认短语应拒绝: {body}");

        let (status, body) = post(
            app(),
            "/api/v1/engine/control",
            r#"{"mode":"live","confirm":"LIVE","enabled":true}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"]["enabled"], true);
        assert_eq!(body["status"]["mode"], "live");

        let persisted = db.setting("engine").unwrap().unwrap();
        assert_eq!(persisted["mode"], "live", "engine 配置组应同步更新");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn status_returns_a_success_payload_for_the_current_mode() {
        let path = std::env::temp_dir().join("nq-status.db");
        let db = seed(&path);
        let app = Router::new().nest("/api/v1", routes()).with_state(db);
        let (status, body) = get(app, "/api/v1/engine/status").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.is_object());
        assert_eq!(body["enabled"], false);
        assert!(
            body.get("last_cycle_ts").is_some(),
            "状态里要能看到周期心跳: {body}"
        );
        // 总览页直接读这份状态：权益、持仓、统计缺一个就会显示 0 或空表。
        assert!(
            body["account"]["equity"].as_f64().unwrap_or(0.0) > 0.0,
            "账户权益不能是 0: {body}"
        );
        assert!(body["account"]["wallet"].is_number(), "{body}");
        assert!(body["account"]["available"].is_number(), "{body}");
        assert!(body["positions"].is_array(), "{body}");
        assert!(body["stats"]["trades"].is_number(), "{body}");
        assert!(body["stats"]["win_rate"].is_number(), "{body}");
    }

    #[tokio::test]
    async fn engine_stats_cover_only_the_current_mode() {
        let path = std::env::temp_dir().join("nq-stats.db");
        let db = seed(&path);
        db.conn
            .lock()
            .unwrap()
            .execute_batch(
                "INSERT INTO trades(symbol,pnl,hold_seconds,exit_reason,mode,closed_at) VALUES \
             ('BTCUSDT',10,100,'tp','demo',300), \
             ('ETHUSDT',-4,50,'sl','demo',200), \
             ('BTCUSDT',90,80,'tp','live',400);",
            )
            .unwrap();
        let app = Router::new().nest("/api/v1", routes()).with_state(db);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/engine/stats")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["trades"], 2);
        assert_eq!(body["wins"], 1);
        assert_eq!(body["losses"], 1);
        assert_eq!(body["win_rate"], 50.0);
        assert_eq!(body["pnl"], 6.0);
        assert_eq!(body["profit_factor"], 2.5);
        assert_eq!(body["by_symbol"]["BTCUSDT"]["n"], 1);
        assert_eq!(body["engine"]["mode"], "demo");
        let _ = std::fs::remove_file(&path);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn stale_venue_snapshot_falls_back_to_estimate_and_skips_closed() {
        let path = std::env::temp_dir().join("nq-positions.db");
        let db = seed(&path);
        let conn = db.conn.lock().unwrap();
        // 多头：标记价 110、开仓价 100、数量 2 → 估算浮盈 20；快照时间戳为 0，应视为过期。
        conn.execute(
            "INSERT INTO positions(id,symbol,side,qty,entry_price,mark_price,leverage,margin,\
             sl_price,tp_price,opened_at,status,mode,venue_unrealized,venue_ts,partial_taken_at) \
             VALUES(1,'BTCUSDT','long',2,100,110,10,20,90,130,200,'open','demo',999,0,0)",
            [],
        )
        .unwrap();
        // 已平仓，默认列表不应出现。
        conn.execute(
            "INSERT INTO positions(id,symbol,side,qty,entry_price,mark_price,leverage,margin,\
             sl_price,tp_price,opened_at,status,mode,venue_unrealized,venue_ts,partial_taken_at) \
             VALUES(2,'ETHUSDT','short',1,100,100,10,10,0,0,100,'closed','demo',0,0,0)",
            [],
        )
        .unwrap();
        drop(conn);
        let app = Router::new().nest("/api/v1", routes()).with_state(db);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/engine/positions")
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
        assert_eq!(items.len(), 1, "已平仓位不应出现");
        assert_eq!(
            items[0]["unrealized_source"], "estimated",
            "过期快照应回退估算"
        );
        assert_eq!(items[0]["unrealized"], 20.0);
        assert_eq!(items[0]["roi_pct"], 100.0);
        // 10 倍杠杆多头强平价 ≈ 100 * (1 - 0.1 + 0.005) = 90.5
        assert_eq!(items[0]["liquidation_price"], 90.5);
        let _ = std::fs::remove_file(&path);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn orders_only_list_entries_scoped_to_current_mode_newest_first() {
        let path = std::env::temp_dir().join("nq-orders.db");
        let db = seed(&path);
        let conn = db.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO orders VALUES(1,100,'BTCUSDT','BUY','MARKET',1,100,'FILLED','entry','c1','e1',0,'demo')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO orders VALUES(2,300,'BTCUSDT','SELL','MARKET',1,110,'FILLED','entry','c2','e2',1,'demo')",
            [],
        )
        .unwrap();
        // 实盘单，当前模式是 demo，不应混入。
        conn.execute(
            "INSERT INTO orders VALUES(3,400,'ETHUSDT','BUY','LIMIT',1,90,'NEW','entry','c3','e3',0,'live')",
            [],
        )
        .unwrap();
        // 保护单不属于开仓流水，同样不应出现。
        conn.execute(
            "INSERT INTO orders VALUES(4,200,'BTCUSDT','SELL','MARKET',1,95,'NEW','stop_loss','c4','e4',1,'demo')",
            [],
        )
        .unwrap();
        drop(conn);
        let app = Router::new().nest("/api/v1", routes()).with_state(db);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/engine/orders")
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
        assert_eq!(items.len(), 2, "实盘订单与保护单不应出现在开仓流水");
        assert_eq!(items[0]["ts"], 300, "应按时间倒序");
        assert_eq!(items[0]["reduce_only"], true);
        assert_eq!(body["source"], "local");
        let _ = std::fs::remove_file(&path);
    }

    fn ledger(path: &std::path::Path) -> Db {
        let db = seed(path);
        let conn = db.conn.lock().unwrap();
        conn.execute_batch(
            "CREATE TABLE signals(id INTEGER PRIMARY KEY, ts INTEGER, symbol TEXT, direction TEXT, score REAL, \
             features TEXT, ai_action TEXT, ai_confidence REAL, ai_reason TEXT, decision TEXT, reject_reason TEXT, \
             mode TEXT, repeats INTEGER); \
             CREATE TABLE equity_points(id INTEGER PRIMARY KEY, ts INTEGER, mode TEXT, wallet REAL, available REAL, \
             unrealized REAL, margin_used REAL, positions INTEGER); \
             INSERT INTO trades VALUES(1,'BTCUSDT','long',1,100,110,10,100,10,10,100,0.2,1000,2000,1000,'tp',0.8,'ok',0.7,'demo',0,'','[]'); \
             INSERT INTO trades VALUES(2,'ETHUSDT','short',1,100,105,5,100,-5,-5,-25,0.1,1100,2100,1000,'sl',0.4,'bad',0.3,'demo',1,'note','[1]'); \
             INSERT INTO trades VALUES(3,'BTCUSDT','long',1,100,120,10,100,20,20,200,0.1,1200,2200,1000,'tp',0.9,'ok',0.8,'live',0,'','[]'); \
             INSERT INTO signals VALUES(1,100,'BTCUSDT','long',0.8,'{}','open',0.7,'r','rejected','波动过大','demo',2); \
             INSERT INTO signals VALUES(2,200,'ETHUSDT','flat',0.1,'{}','','0','','flat','','demo',1); \
             INSERT INTO signals VALUES(3,300,'BTCUSDT','long',0.9,'{}','open',0.9,'r','opened','','live',1); \
             INSERT INTO equity_points VALUES(1,strftime('%s','now')-1000,'demo',1000,800,20,200,1); \
             INSERT INTO equity_points VALUES(2,strftime('%s','now')-100,'demo',1010,810,10,200,1); \
             INSERT INTO equity_points VALUES(3,strftime('%s','now')-500,'live',5000,4000,0,0,0);",
        )
        .unwrap();
        drop(conn);
        db
    }

    #[tokio::test]
    async fn trades_stay_in_current_mode_and_stats_ignore_the_other() {
        let path = std::env::temp_dir().join("nq-trades.db");
        let db = ledger(&path);
        let app = Router::new().nest("/api/v1", routes()).with_state(db);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/engine/trades")
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
        assert_eq!(items.len(), 2, "实盘成交不能混进模拟盘");
        assert_eq!(items[0]["closed_at"], 2100, "应按平仓时间倒序");
        assert_eq!(items[0]["reviewed"], true);
        assert_eq!(items[0]["mantra_ids"][0], 1);
        assert_eq!(body["stats"]["trades"], 2);
        assert_eq!(body["stats"]["wins"], 1);
        assert_eq!(body["stats"]["pnl"], 5.0);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn signals_summary_weights_repeats_and_filters_decision() {
        let path = std::env::temp_dir().join("nq-signals.db");
        let db = ledger(&path);
        let app = Router::new().nest("/api/v1", routes()).with_state(db);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/engine/signals?decision=rejected")
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
            body["items"].as_array().unwrap().len(),
            1,
            "列表按 decision 过滤"
        );
        assert_eq!(
            body["summary"]["total"], 2,
            "汇总仍按当前环境，不受列表过滤影响"
        );
        assert_eq!(body["summary"]["scans"], 3, "扫描次数按 repeats 累加");
        assert_eq!(body["summary"]["top_reasons"][0]["reason"], "波动过大");
        assert_eq!(body["summary"]["top_reasons"][0]["count"], 2);
        assert_eq!(
            body["summary"]["by_hour"].as_array().unwrap().len(),
            24,
            "近 7 天应按小时铺满 24 桶"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn equity_points_are_oldest_first_and_scoped_to_mode() {
        let path = std::env::temp_dir().join("nq-equity.db");
        let db = ledger(&path);
        let app = Router::new().nest("/api/v1", routes()).with_state(db);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/engine/equity?hours=72")
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
        assert_eq!(items.len(), 2, "实盘采样点不应出现");
        assert!(
            items[0]["ts"].as_i64().unwrap() < items[1]["ts"].as_i64().unwrap(),
            "曲线按时间正序"
        );
        assert_eq!(items[1]["wallet"], 1010.0);
        assert_eq!(body["hours"], 72);
        let _ = std::fs::remove_file(&path);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn resetting_anchors_clears_a_stale_drawdown_halt() {
        let path = std::env::temp_dir().join("nq-anchors.db");
        let db = seed(&path);
        let conn = db.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO engine_state VALUES('day_start_equity','100',0)",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO engine_state VALUES('equity_peak','500',0)", [])
            .unwrap();
        conn.execute(
            "INSERT INTO engine_state VALUES('halted','最大回撤触发熔断',0)",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO engine_state VALUES('equity','250.5',0)", [])
            .unwrap();
        drop(conn);
        let app = Router::new()
            .nest("/api/v1", routes())
            .with_state(db.clone());
        let (status, body) = post(app, "/api/v1/engine/reset-anchors", "{}").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["equity"], 250.5);
        assert!(
            body["cleared_halt"].as_str().unwrap().contains("回撤"),
            "回撤熔断应被解除"
        );
        let conn = db.conn.lock().unwrap();
        let peak: String = conn
            .query_row(
                "SELECT value FROM engine_state WHERE key='equity_peak'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(peak, "250.50000000", "峰值应改写为当前权益");
        let halted: String = conn
            .query_row(
                "SELECT value FROM engine_state WHERE key='halted'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(halted, "", "熔断标记应清空");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn zero_equity_cannot_become_the_anchor() {
        let path = std::env::temp_dir().join("nq-anchors-zero.db");
        let db = seed(&path);
        let app = Router::new().nest("/api/v1", routes()).with_state(db);
        let (status, body) = post(app, "/api/v1/engine/reset-anchors", "{}").await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "权益为 0 时不能当基准: {body}"
        );
        let _ = std::fs::remove_file(&path);
    }

    fn control_db(name: &str) -> Db {
        let path = std::env::temp_dir().join(name);
        let _ = std::fs::remove_file(&path);
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE app_settings(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER); \
             CREATE TABLE engine_state(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER); \
             CREATE TABLE trading_symbols(symbol TEXT PRIMARY KEY, enabled INTEGER, max_leverage INTEGER, \
             tick_sz TEXT, min_sz TEXT, min_notional REAL); \
             CREATE TABLE positions(symbol TEXT, side TEXT, status TEXT, mode TEXT); \
             CREATE TABLE ai_providers(id INTEGER PRIMARY KEY, enabled INTEGER); \
             INSERT INTO engine_state VALUES('enabled','true',0),('mode','demo',0); \
             INSERT INTO app_settings VALUES('engine','{\"enabled\":true,\"mode\":\"demo\",\"trading_windows\":[\"00:00-24:00\"],\"timezone_offset\":8}',0); \
             INSERT INTO app_settings VALUES('risk','{\"margin_pct\":50,\"min_leverage\":10,\"max_leverage\":20,\"min_equity_usdt\":0.5,\"min_notional_buffer\":1.05,\"consecutive_loss_limit\":4}',0); \
             INSERT INTO app_settings VALUES('strategy','{\"timeframe\":\"15m\",\"entry_threshold\":0.3}',0); \
             INSERT INTO app_settings VALUES('ai','{\"enabled\":false}',0); \
             INSERT INTO app_settings VALUES('data','{\"demo_api_key_enc\":\"configured\",\"demo_api_secret_enc\":\"configured\"}',0); \
             INSERT INTO trading_symbols VALUES('BTCUSDT',1,50,'0.01','0.001',5); \
             INSERT INTO engine_state VALUES('snapshot.demo','{\"account\":{\"equity\":100,\"available_balance\":80},\"markets\":[{\"symbol\":\"BTCUSDT\",\"mark_price\":100,\"quote_volume\":100000000}]}',0);",
        )
        .unwrap();
        drop(conn);
        Db::open(path.to_str().unwrap()).unwrap()
    }

    async fn get(app: Router, uri: &str) -> (StatusCode, Value) {
        let response = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
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

    #[tokio::test]
    async fn admission_uses_the_local_snapshot_and_marks_a_tradeable_symbol() {
        let db = control_db("nq-admission.db");
        let app = Router::new().nest("/api/v1", routes()).with_state(db);
        let (status, body) = get(app, "/api/v1/engine/admission").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["blocking"], json!([]), "{body}");
        assert_eq!(body["mode"], "demo");
        assert_eq!(body["account"]["equity"], 100.0);
        assert_eq!(body["tradeable_count"], 1);
        assert_eq!(body["symbols"][0]["symbol"], "BTCUSDT");
        assert_eq!(body["symbols"][0]["tradeable"], true);
        assert!(body["can_open"].as_bool().unwrap());
        let steps = body["steps"].as_array().unwrap();
        assert!(steps
            .iter()
            .any(|step| step["key"] == "symbols" && step["ok"] == true));
    }

    #[tokio::test]
    async fn admission_blocks_when_credentials_and_snapshot_are_missing() {
        let db = control_db("nq-admission-blocked.db");
        {
            let conn = db.conn.lock().unwrap();
            conn.execute_batch(
                "DELETE FROM engine_state WHERE key='snapshot.demo'; \
                 DELETE FROM app_settings WHERE key='data';",
            )
            .unwrap();
        }
        let app = Router::new().nest("/api/v1", routes()).with_state(db);
        let (status, body) = get(app, "/api/v1/engine/admission").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["can_open"], false);
        assert!(body["blocking"]
            .as_array()
            .unwrap()
            .iter()
            .any(|key| key == "channel" || key == "funds"));
        assert_eq!(body["tradeable_count"], 0);
    }

    #[tokio::test]
    async fn the_engine_stays_running_between_two_long_scans() {
        // 回归：扫描周期 10 分钟时，两轮之间的间歇期长达近 10 分钟。
        // 判活阈值写死 180 秒的话，面板每 10 分钟有 7 分钟误报「引擎未运行」。
        let db = control_db("nq-status-long-interval.db");
        {
            let conn = db.conn.lock().unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO app_settings(key, value, updated_at) VALUES('engine', ?1, 0)",
                [json!({
                    "enabled": true,
                    "mode": "demo",
                    "cycle_interval_sec": 600,
                    "trading_windows": ["00:00-24:00"],
                    "timezone_offset": 8
                })
                .to_string()],
            )
            .unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO engine_state(key, value, updated_at) VALUES('last_cycle_ts', ?1, 0)",
                [(crate::now() - 300).to_string()],
            )
            .unwrap();
        }
        let app = Router::new().nest("/api/v1", routes()).with_state(db);
        let (status, body) = get(app, "/api/v1/engine/status").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["cycle_interval_sec"], json!(600));
        assert_eq!(body["liveness_threshold_sec"], json!(1260));
        assert_eq!(
            body["running"],
            json!(true),
            "间歇期不该被判成未运行: {body}"
        );
        let age = body["heartbeat_age"].as_i64().unwrap();
        assert!((299..=302).contains(&age), "心跳年龄应约等于 300: {body}");
    }

    #[tokio::test]
    async fn the_admission_reports_a_missing_heartbeat_instead_of_claiming_the_engine_runs() {
        let db = control_db("nq-admission-no-heartbeat.db");
        {
            let conn = db.conn.lock().unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO app_settings(key, value, updated_at) VALUES('engine', ?1, 0)",
                [json!({
                    "enabled": true,
                    "mode": "demo",
                    "cycle_interval_sec": 600,
                    "trading_windows": ["00:00-24:00"],
                    "timezone_offset": 8
                })
                .to_string()],
            )
            .unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO engine_state(key, value, updated_at) VALUES('last_cycle_ts', ?1, 0)",
                [(crate::now() - 5000).to_string()],
            )
            .unwrap();
        }
        let app = Router::new().nest("/api/v1", routes()).with_state(db);
        let (status, body) = get(app, "/api/v1/engine/admission").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let engine_step = body["steps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|step| step["key"] == "engine")
            .expect("准入检查必须有引擎项")
            .clone();
        assert_eq!(engine_step["ok"], json!(false), "{engine_step}");
        let detail = engine_step["detail"].as_str().unwrap();
        assert!(detail.contains("没有心跳"), "必须说清楚没有心跳: {detail}");
        // 只是提示，不能把人工开仓也堵死。
        assert!(
            !body["blocking"]
                .as_array()
                .unwrap()
                .iter()
                .any(|key| key == "engine"),
            "心跳问题只能提示，不能拦开仓: {body}"
        );
    }
}
