//! 引擎控制。启停状态双写 `engine` 配置组与 `engine_state` 表，
//! 对齐 `api/routes/trade.py` 的 `control`。

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use rusqlite::{Connection, OptionalExtension};
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
             exchange_order_id, reduce_only, mode FROM orders WHERE mode = ?1 ORDER BY ts DESC LIMIT ?2",
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

#[derive(Deserialize)]
struct FlattenBody {
    confirm: String,
    #[serde(default = "default_true")]
    stop_engine: bool,
}

fn default_true() -> bool {
    true
}

async fn flatten(State(db): State<Db>, Json(body): Json<FlattenBody>) -> (StatusCode, Json<Value>) {
    if body.confirm != "FLATTEN" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"detail": "需要 confirm=FLATTEN"})),
        );
    }
    let mode = current_mode(&db);
    let mut closed = Vec::new();
    let mut failures = Vec::new();
    let positions: Vec<(i64, String, f64)> = {
        let conn = db.conn.lock().expect("db");
        let mut stmt = conn
            .prepare("SELECT id, symbol, mark_price FROM positions WHERE status='open' AND mode=?1")
            .expect("prepare flatten");
        stmt.query_map([&mode], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get::<_, f64>(2)?))
        })
        .expect("query flatten")
        .flatten()
        .collect()
    };
    for (id, symbol, price) in positions {
        match crate::execution::close_position(&db, id, price, "flatten", mode == "live") {
            Ok(()) => closed.push(json!({"id": id, "symbol": symbol, "price": price})),
            Err(error) => failures.push(json!({"id": id, "symbol": symbol, "error": error})),
        }
    }
    if body.stop_engine {
        write_state(&db, "enabled", "false");
    }
    (
        StatusCode::OK,
        Json(json!({
            "ok": failures.is_empty(),
            "mode": mode,
            "engine_stopped": body.stop_engine,
            "closed": closed,
            "failures": failures,
        })),
    )
}

async fn close_position(State(db): State<Db>, Path(id): Path<i64>) -> (StatusCode, Json<Value>) {
    let row: Option<(String, String, f64)> = {
        let conn = db.conn.lock().expect("db");
        conn.query_row(
            "SELECT mode, symbol, mark_price FROM positions WHERE id=?1 AND status='open'",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .expect("query position")
    };
    let Some((mode, symbol, price)) = row else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"detail": "仓位不存在或已关闭"})),
        );
    };
    match crate::execution::close_position(&db, id, price, "manual", mode == "live") {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({"ok": true, "id": id, "symbol": symbol, "exit_price": price})),
        ),
        Err(error) => (StatusCode::CONFLICT, Json(json!({"detail": error}))),
    }
}

async fn status(State(db): State<Db>) -> Json<Value> {
    Json(read_status(&db))
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

async fn stats(State(db): State<Db>) -> Json<Value> {
    let mode = current_mode(&db);
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
    }))
}

async fn execution(State(db): State<Db>) -> Json<Value> {
    let conn = db.conn.lock().expect("db");
    let mut stmt = conn
        .prepare("SELECT value FROM engine_state WHERE key LIKE 'execution.%'")
        .expect("prepare execution");
    let mut samples = Vec::new();
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query execution");
    for row in rows.flatten() {
        if let Ok(value) = row.parse::<f64>() {
            if value.is_finite() && value >= 0.0 {
                samples.push(value);
            }
        }
    }
    samples.sort_by(f64::total_cmp);
    let percentile = |q: f64| -> f64 {
        if samples.is_empty() {
            return 0.0;
        }
        let index = ((samples.len() as f64 * q).floor() as usize).min(samples.len() - 1);
        round_n(samples[index], 2)
    };
    Json(json!({
        "count": samples.len(),
        "latency_ms": {"p50": percentile(0.50), "p95": percentile(0.95), "p99": percentile(0.99), "max": samples.last().copied().map(|v| round_n(v, 2)).unwrap_or(0.0)}
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
    let engine_ok = enabled_flag && window_ok && consecutive < loss_limit && !halted_on;
    let mut details = Vec::new();
    if !enabled_flag {
        details.push("引擎未启动 (总览页点「启动引擎」)".to_string());
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
        risk_per_trade_pct: risk["risk_per_trade_pct"].as_f64().unwrap_or(10.0),
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
            0.005,
            &filter,
            1.0,
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
        "can_open": tradeable > 0 && blocking.is_empty() && enabled_flag,
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
            "risk_per_trade_pct": params.risk_per_trade_pct,
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
    json!({
        "enabled": get("enabled") == "true",
        "mode": if mode.is_empty() { "demo".to_string() } else { mode },
    })
}

pub fn seed(path: &std::path::Path) -> Db {
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
    async fn execution_reports_percentiles_and_ignores_invalid_samples() {
        let path = std::env::temp_dir().join("nq-execution.db");
        let db = seed(&path);
        {
            let conn = db.conn.lock().unwrap();
            conn.execute_batch("INSERT INTO engine_state VALUES ('execution.a','10.0',0), ('execution.b','20.0',0), ('execution.c','40.0',0), ('execution.bad','nan',0), ('other','999',0);").unwrap();
        }
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
        assert_eq!(body["count"], 3);
        assert_eq!(body["latency_ms"]["p50"], 20.0);
        assert_eq!(body["latency_ms"]["p95"], 40.0);
        assert_eq!(body["latency_ms"]["max"], 40.0);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn execution_with_no_samples_returns_zeroes() {
        let path = std::env::temp_dir().join("nq-execution-empty.db");
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
        assert_eq!(body["latency_ms"]["p99"], 0.0);
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
    async fn orders_are_scoped_to_current_mode_newest_first() {
        let path = std::env::temp_dir().join("nq-orders.db");
        let db = seed(&path);
        let conn = db.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO orders VALUES(1,100,'BTCUSDT','BUY','MARKET',1,100,'FILLED','open','c1','e1',0,'demo')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO orders VALUES(2,300,'BTCUSDT','SELL','MARKET',1,110,'FILLED','close','c2','e2',1,'demo')",
            [],
        )
        .unwrap();
        // 实盘单，当前模式是 demo，不应混入。
        conn.execute(
            "INSERT INTO orders VALUES(3,400,'ETHUSDT','BUY','LIMIT',1,90,'NEW','open','c3','e3',0,'live')",
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
        assert_eq!(items.len(), 2, "实盘订单不应出现在模拟盘列表");
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
             INSERT INTO app_settings VALUES('risk','{\"risk_per_trade_pct\":10,\"margin_pct\":50,\"min_leverage\":10,\"max_leverage\":20,\"min_equity_usdt\":0.5,\"min_notional_buffer\":1.05,\"consecutive_loss_limit\":4}',0); \
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
}
