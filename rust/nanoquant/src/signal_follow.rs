//! 外部信号跟单：状态 / 事件 / HTTP 直推 / 立即读一轮 / 游标重置。
//!
//! 与 legacy 内核（已退役） 侧同口径：同一套事件分类、唯一事件 ID 幂等、风控与本地记账。
//! `X-Signal-Token` 命中后由机器直推；否则必须带后台登录态。未知事件一律
//! 拒绝（400），不做猜测。live 环境只做审计，不在这里发主网订单。

use std::collections::{HashMap, VecDeque};
use std::sync::{LazyLock, Mutex};

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::db::Db;
use crate::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/signal-follow/status", get(follow_status))
        .route("/signal-follow/events", get(follow_events))
        .route("/signal-follow/ingest", post(follow_ingest))
        .route("/signal-follow/read-once", post(follow_read_once))
        .route("/signal-follow/reset-cursor", post(follow_reset_cursor))
}

#[derive(Deserialize)]
struct EventsQuery {
    #[serde(default = "default_limit")]
    limit: i64,
    #[serde(default)]
    status: String,
}

fn default_limit() -> i64 {
    50
}

async fn follow_events(State(state): State<AppState>, Query(q): Query<EventsQuery>) -> Json<Value> {
    let Some(db) = state.db.as_ref() else {
        return json!({"items":[]}).into();
    };
    let conn = db.conn.lock().expect("db");
    json!({ "items": load_events(&conn, q.limit.clamp(1, 500), &q.status) }).into()
}

/// 状态接口同时给出配置、计数和最近 20 条。直推密钥只告诉前端「已设置」，不回传原文。
async fn follow_status(State(state): State<AppState>) -> Json<Value> {
    let Some(db) = state.db.as_ref() else {
        return json!({"settings":{},"ingest_token_set":false,"counts":{},"items":[]}).into();
    };
    let cfg = load_settings(db);
    let token_set = !cfg.ingest_token.is_empty();
    let mut settings = cfg.to_json();
    if token_set {
        if let Some(obj) = settings.as_object_mut() {
            obj.insert("ingest_token".to_string(), json!("******"));
        }
    }
    let path = cfg.jsonl_path.clone();
    let path_exists = !path.is_empty() && std::path::Path::new(&path).is_file();
    let engine_enabled = engine_enabled(db);
    let conn = db.conn.lock().expect("db");
    let mut counts = serde_json::Map::new();
    for status in [
        "opened",
        "added",
        "reduced",
        "closed",
        "stop_moved",
        "dry_run",
        "skipped",
        "error",
        "duplicate",
        "processing",
    ] {
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM signal_follows WHERE status = ?1",
                [status],
                |r| r.get(0),
            )
            .unwrap_or(0);
        counts.insert(status.to_string(), json!(n));
    }
    json!({
        "settings": settings,
        "ingest_token_set": token_set,
        "running": false,
        "polling": false,
        "path": path,
        "path_exists": path_exists,
        "engine_enabled": engine_enabled,
        "counts": counts,
        "items": load_events(&conn, 20, ""),
    })
    .into()
}

fn load_events(conn: &rusqlite::Connection, limit: i64, status: &str) -> Vec<Value> {
    let (filter, args): (&str, Vec<Box<dyn rusqlite::types::ToSql>>) = if status.is_empty() {
        ("", vec![Box::new(limit)])
    } else {
        (
            "WHERE status = ?1",
            vec![Box::new(status.to_string()), Box::new(limit)],
        )
    };
    let sql = format!(
        "SELECT id, signal_id, symbol, side, channel, status, reason, leverage, qty, price, margin, mode, created_at \
         FROM signal_follows {filter} ORDER BY created_at DESC LIMIT ?"
    );
    let mut stmt = conn.prepare(&sql).expect("prepare events");
    stmt.query_map(rusqlite::params_from_iter(args.iter()), |r| {
        Ok(json!({
            "id": r.get::<_, i64>(0)?,
            "signal_id": r.get::<_, String>(1)?,
            "symbol": r.get::<_, String>(2)?,
            "side": r.get::<_, String>(3)?,
            "channel": r.get::<_, String>(4)?,
            "status": r.get::<_, String>(5)?,
            "reason": r.get::<_, String>(6)?,
            "leverage": r.get::<_, i64>(7)?,
            "qty": r.get::<_, f64>(8)?,
            "price": r.get::<_, f64>(9)?,
            "margin": r.get::<_, f64>(10)?,
            "mode": r.get::<_, String>(11)?,
            "created_at": r.get::<_, i64>(12)?,
        }))
    })
    .expect("query events")
    .collect::<Result<_, _>>()
    .expect("event rows")
}

// ------------------------------------------------------------------ settings

#[derive(Clone, Debug)]
struct SignalSettings {
    enabled: bool,
    ingest_token: String,
    jsonl_path: String,
    poll_sec: i64,
    leverage_bonus: i64,
    max_age_sec: i64,
    max_per_hour: i64,
    auto_add_symbol: bool,
    strict_events: bool,
    default_sl_pct: f64,
    sizing_mode: String,
    margin_pct_override: f64,
    dry_run: bool,
    max_requests_per_minute: i64,
}

impl Default for SignalSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            ingest_token: String::new(),
            jsonl_path: String::new(),
            poll_sec: 1,
            leverage_bonus: 10,
            max_age_sec: 180,
            max_per_hour: 6,
            auto_add_symbol: true,
            strict_events: false,
            default_sl_pct: 10.0,
            sizing_mode: "margin_pct".to_string(),
            margin_pct_override: 0.0,
            dry_run: true,
            max_requests_per_minute: 60,
        }
    }
}

impl SignalSettings {
    fn to_json(&self) -> Value {
        json!({
            "enabled": self.enabled,
            "ingest_token": self.ingest_token,
            "jsonl_path": self.jsonl_path,
            "poll_sec": self.poll_sec,
            "leverage_bonus": self.leverage_bonus,
            "max_age_sec": self.max_age_sec,
            "max_per_hour": self.max_per_hour,
            "auto_add_symbol": self.auto_add_symbol,
            "strict_events": self.strict_events,
            "default_sl_pct": self.default_sl_pct,
            "sizing_mode": self.sizing_mode,
            "margin_pct_override": self.margin_pct_override,
            "dry_run": self.dry_run,
            "max_requests_per_minute": self.max_requests_per_minute,
        })
    }
}

fn load_settings(db: &Db) -> SignalSettings {
    let mut cfg = SignalSettings::default();
    let Ok(Some(value)) = db.setting("signal_follow") else {
        return cfg;
    };
    let b = |key: &str, target: &mut bool| {
        if let Some(v) = value.get(key).and_then(Value::as_bool) {
            *target = v;
        }
    };
    let i = |key: &str, target: &mut i64| {
        if let Some(v) = value.get(key).and_then(Value::as_i64) {
            *target = v;
        }
    };
    let f = |key: &str, target: &mut f64| {
        if let Some(v) = value.get(key).and_then(Value::as_f64) {
            *target = v;
        }
    };
    let s = |key: &str, target: &mut String| {
        if let Some(v) = value.get(key).and_then(Value::as_str) {
            *target = v.to_string();
        }
    };
    b("enabled", &mut cfg.enabled);
    s("ingest_token", &mut cfg.ingest_token);
    s("jsonl_path", &mut cfg.jsonl_path);
    i("poll_sec", &mut cfg.poll_sec);
    i("leverage_bonus", &mut cfg.leverage_bonus);
    i("max_age_sec", &mut cfg.max_age_sec);
    i("max_per_hour", &mut cfg.max_per_hour);
    b("auto_add_symbol", &mut cfg.auto_add_symbol);
    b("strict_events", &mut cfg.strict_events);
    f("default_sl_pct", &mut cfg.default_sl_pct);
    s("sizing_mode", &mut cfg.sizing_mode);
    f("margin_pct_override", &mut cfg.margin_pct_override);
    b("dry_run", &mut cfg.dry_run);
    i("max_requests_per_minute", &mut cfg.max_requests_per_minute);
    cfg
}

// ------------------------------------------------------------------- ingest

static INGEST_WINDOW: LazyLock<Mutex<HashMap<String, VecDeque<i64>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn allow_ingest(key: &str, now: i64, limit: i64) -> bool {
    let limit = limit.max(1);
    let mut windows = INGEST_WINDOW.lock().expect("ingest window");
    let window = windows.entry(key.to_string()).or_default();
    while window.front().is_some_and(|ts| *ts <= now - 60) {
        window.pop_front();
    }
    if window.len() as i64 >= limit {
        return false;
    }
    window.push_back(now);
    true
}

fn token_fingerprint(token: &str) -> String {
    short_hash(token).chars().take(12).collect()
}

fn constant_time_equal(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

fn ingest_actor(state: &AppState, headers: &HeaderMap) -> Option<(&'static str, String)> {
    let db = state.db.as_ref()?;
    let cfg = load_settings(db);
    let token = headers
        .get("x-signal-token")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if !cfg.ingest_token.is_empty() && constant_time_equal(token, &cfg.ingest_token) {
        return Some(("signal-bot", token_fingerprint(token)));
    }
    if crate::auth::bearer_is_valid(&state.auth, headers) {
        return Some(("admin", "bearer".to_string()));
    }
    None
}

async fn follow_ingest(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let Some((actor, fingerprint)) = ingest_actor(&state, &headers) else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"detail":"信号直推鉴权失败","code":"unauthorized"})),
        );
    };
    let Some(db) = state.db.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"detail":"数据库未就绪"})),
        );
    };
    let cfg = load_settings(&db);
    if !allow_ingest(
        &format!("{actor}:{fingerprint}:{}", db.path),
        crate::now(),
        cfg.max_requests_per_minute,
    ) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"detail":"信号直推过于频繁，请稍后重试","retry_after_sec":60})),
        );
    }
    let parsed = match parse_signal(&payload, crate::now()) {
        Ok(parsed) => parsed,
        Err(detail) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"detail": detail, "code": "invalid_signal"})),
            );
        }
    };
    let outcome = process_signal(&db, &cfg, &parsed, actor, "http");
    let status = match outcome["status"].as_str().unwrap_or("error") {
        "error" => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::OK,
    };
    (status, Json(outcome))
}

#[derive(Clone, Debug)]
struct ParsedSignal {
    symbol: String,
    side: String,
    event: String,
    leverage: f64,
    entry: f64,
    stop: f64,
    tp: f64,
    reduce_pct: f64,
    ts: i64,
    signal_id: String,
    raw: Value,
}

fn first_value(row: &Value, keys: &[&str]) -> Option<Value> {
    keys.iter().find_map(|key| {
        let value = row.get(*key)?;
        if value.is_null() || value.as_str() == Some("") {
            None
        } else {
            Some(value.clone())
        }
    })
}

fn normalize_symbol_text(raw: &str) -> String {
    raw.chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect::<String>()
        .to_uppercase()
}

fn number(value: Option<Value>) -> f64 {
    match value {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(text)) => {
            let cleaned: String = text
                .chars()
                .filter(|ch| ch.is_ascii_digit() || *ch == '.' || *ch == '-')
                .collect();
            cleaned.parse().unwrap_or(0.0)
        }
        _ => 0.0,
    }
}

fn normalize_side(raw: &str) -> &'static str {
    let text = raw.trim().to_lowercase().replace(' ', "");
    match text.as_str() {
        "long" | "buy" | "多" | "做多" | "开多" | "看多" => "long",
        "short" | "sell" | "空" | "做空" | "开空" | "看空" => "short",
        _ => "",
    }
}

fn classify_event(text: &str) -> &'static str {
    let text = text.trim().to_lowercase().replace([' ', '-'], "");
    match text.as_str() {
        "open" | "entry" | "open_long" | "open_short" | "开仓" | "建仓" | "开多" | "开空"
        | "入场" | "开单" => "open",
        "add" | "加仓" | "补仓" | "加码" => "add",
        "reduce" | "partial" | "减仓" | "部分止盈" | "部分平仓" | "平一半" => "reduce",
        "close" | "close_all" | "closeall" | "平仓" | "全平" | "清仓" | "平多" | "平空"
        | "全部平仓" => "close",
        "move_stop" | "movestop" | "breakeven" | "trailing" | "移止损" | "移动止损" | "保本"
        | "推保本" | "止损上移" => "move_stop",
        "protect" | "protection" | "保护" | "保护单" => "protect",
        "status" | "hold" | "ping" | "状态" | "持仓提醒" => "status",
        "exit" | "止损" | "止盈" | "撤单" | "取消" => "exit",
        _ => "unknown",
    }
}

fn parse_signal(row: &Value, now: i64) -> Result<ParsedSignal, String> {
    if !row.is_object() {
        return Err("信号必须是 JSON 对象".to_string());
    }
    let symbol = normalize_symbol_text(
        first_value(row, &["symbol", "pair", "coin", "ticker", "标的"])
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default()
            .as_str(),
    );
    if symbol.is_empty() || !symbol.ends_with("USDT") {
        return Err("缺少或非法的 symbol（只接受 USDT 永续合约名）".to_string());
    }
    let event_raw = first_value(
        row,
        &[
            "event",
            "event_type",
            "type",
            "kind",
            "事件",
            "动作",
            "类型",
        ],
    )
    .and_then(|v| v.as_str().map(str::to_string))
    .unwrap_or_default();
    let side_raw = first_value(row, &["side", "direction", "action", "方向"])
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default();
    let mut side = normalize_side(&side_raw).to_string();
    if side.is_empty() && event_raw.contains("平多") {
        side = "long".to_string();
    }
    if side.is_empty() && event_raw.contains("平空") {
        side = "short".to_string();
    }
    let event = if event_raw.trim().is_empty() {
        if side.is_empty() {
            return Err("缺少 event 字段".to_string());
        }
        "open"
    } else {
        classify_event(&event_raw)
    };
    if event == "unknown" {
        return Err(format!("未知事件类型: {event_raw}"));
    }
    if event == "exit" {
        return Ok(ParsedSignal {
            side,
            symbol,
            event: event.to_string(),
            leverage: 0.0,
            entry: 0.0,
            stop: 0.0,
            tp: 0.0,
            reduce_pct: 0.0,
            ts: now,
            signal_id: String::new(),
            raw: row.clone(),
        });
    }
    if matches!(event, "open" | "add") && side.is_empty() {
        return Err("开仓类信号缺少 side（long/short）".to_string());
    }
    let ts_raw = number(first_value(
        row,
        &["ts", "time", "created_at", "timestamp", "时间"],
    )) as i64;
    let ts = if ts_raw > 10_000_000_000 {
        ts_raw / 1000
    } else if ts_raw > 0 {
        ts_raw
    } else {
        now
    };
    Ok(ParsedSignal {
        symbol,
        side,
        event: event.to_string(),
        leverage: number(first_value(row, &["leverage", "lev", "杠杆"])),
        entry: number(first_value(
            row,
            &["entry", "price", "entry_price", "入场价"],
        )),
        stop: number(first_value(row, &["sl", "stop", "stop_loss", "止损"])),
        tp: number(first_value(row, &["tp", "take_profit", "止盈"])),
        reduce_pct: number(first_value(
            row,
            &["reduce_pct", "reduce", "close_pct", "减仓比例"],
        )),
        ts,
        signal_id: first_value(row, &["id", "signal_id", "msg_id", "message_id"])
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default(),
        raw: row.clone(),
    })
}

fn short_hash(seed: &str) -> String {
    let digest = Sha256::digest(seed.as_bytes());
    digest.iter().take(12).map(|b| format!("{b:02x}")).collect()
}

fn signal_identity(p: &ParsedSignal) -> String {
    let id = p.signal_id.trim();
    if !id.is_empty() {
        return format!("{id}:{}:{}", p.symbol, p.side)
            .chars()
            .take(64)
            .collect();
    }
    format!(
        "h:{}",
        short_hash(&format!("{}|{}|{}|{}", p.symbol, p.side, p.entry, p.ts))
    )
}

fn current_mode(db: &Db) -> String {
    let conn = db.conn.lock().expect("db");
    let stored = conn
        .query_row(
            "SELECT value FROM engine_state WHERE key = 'mode'",
            [],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .filter(|mode| !mode.is_empty());
    stored.unwrap_or_else(|| "demo".to_string())
}

fn engine_enabled(db: &Db) -> bool {
    let conn = db.conn.lock().expect("db");
    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM engine_state WHERE key = 'enabled'",
            [],
            |r| r.get(0),
        )
        .ok();
    matches!(
        raw.as_deref(),
        Some("1") | Some("true") | Some("True") | Some("TRUE")
    )
}

fn account_equity(conn: &rusqlite::Connection, mode: &str) -> f64 {
    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM engine_state WHERE key = ?1",
            [format!("snapshot.{mode}")],
            |r| r.get(0),
        )
        .ok();
    raw.and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value["account"]["equity"].as_f64())
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or(0.0)
}

fn place_follow(
    conn: &rusqlite::Connection,
    key: &str,
    p: &ParsedSignal,
    mode: &str,
    channel: &str,
    status: &str,
    reason: &str,
) -> bool {
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO signal_follows(signal_id,symbol,side,channel,status,reason,leverage,qty,price,margin,mode,payload,created_at) \
         VALUES(?1,?2,?3,?4,?5,?6,?7,0,0,0,?8,?9,?10)",
        rusqlite::params![
            key,
            p.symbol,
            if p.side.is_empty() { "unknown" } else { p.side.as_str() },
            channel,
            status,
            reason,
            p.leverage as i64,
            mode,
            json!({"event": p.event, "signal_ts": p.ts, "raw": p.raw}).to_string(),
            crate::now()
        ],
    );
    matches!(inserted, Ok(1))
}

fn finish_follow(
    conn: &rusqlite::Connection,
    key: &str,
    status: &str,
    reason: &str,
    leverage: i64,
    qty: f64,
    price: f64,
    margin: f64,
) {
    let _ = conn.execute(
        "UPDATE signal_follows SET status=?1, reason=?2, leverage=?3, qty=?4, price=?5, margin=?6 WHERE signal_id=?7",
        rusqlite::params![status, reason, leverage, qty, price, margin, key],
    );
}

fn process_signal(
    db: &Db,
    cfg: &SignalSettings,
    p: &ParsedSignal,
    actor: &str,
    channel: &str,
) -> Value {
    if !cfg.enabled {
        return json!({"ok":false,"status":"skipped","reason":"信号跟单未开启","actor":actor});
    }
    let key = signal_identity(p);
    let mode = current_mode(db);
    let engine_running = engine_enabled(db);
    let conn = db.conn.lock().expect("db");
    if !place_follow(&conn, &key, p, &mode, channel, "processing", "处理中") {
        return json!({
            "ok": true, "status": "duplicate", "reason": "该信号已处理过",
            "signal_id": key, "actor": actor
        });
    }
    let skipped = |status: &str, reason: &str| {
        finish_follow(
            &conn,
            &key,
            status,
            reason,
            p.leverage as i64,
            0.0,
            0.0,
            0.0,
        );
        json!({"ok": status == "dry_run", "status": status, "reason": reason,
            "signal_id": key, "actor": actor, "mode": mode})
    };
    if mode != "demo" {
        return skipped(
            "skipped",
            "实盘信号执行由引擎运行时接管，这里只审计，不产生主网订单",
        );
    }
    if p.event == "exit" {
        return skipped("skipped", "非开仓事件(止损/止盈/更新)，忽略");
    }
    if !engine_running {
        return skipped("skipped", "交易引擎未启动");
    }
    let age = crate::now() - p.ts;
    if age > cfg.max_age_sec || age < -60 {
        return skipped(
            "skipped",
            &format!("信号时间异常或已过期 {age}s (上限 {}s)", cfg.max_age_sec),
        );
    }
    match p.event.as_str() {
        "open" => open_demo(&conn, cfg, p, &key, &mode),
        "close" => close_demo(&conn, cfg, p, &key, &mode),
        "protect" => protect_demo(&conn, p, &key, &mode),
        "add" => skipped("skipped", "加仓信号暂未接入引擎运行时，仅审计"),
        "reduce" => skipped(
            "skipped",
            &format!(
                "减仓 {}% 信号暂未接入引擎运行时，仅审计",
                if p.reduce_pct > 0.0 {
                    p.reduce_pct
                } else {
                    50.0
                }
            ),
        ),
        "move_stop" => skipped("skipped", "移止损信号暂未接入引擎运行时，仅审计"),
        "status" => skipped("skipped", "状态类事件仅记录"),
        _ => skipped("skipped", "未知事件类型仅记录"),
    }
}

fn risk_value(conn: &rusqlite::Connection, key: &str, default: f64) -> f64 {
    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM app_settings WHERE key = 'risk'",
            [],
            |r| r.get(0),
        )
        .ok();
    raw.and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value[key].as_f64())
        .unwrap_or(default)
}

fn open_demo(
    conn: &rusqlite::Connection,
    cfg: &SignalSettings,
    p: &ParsedSignal,
    key: &str,
    mode: &str,
) -> Value {
    let skipped = |status: &str, reason: &str| {
        finish_follow(conn, key, status, reason, p.leverage as i64, 0.0, 0.0, 0.0);
        json!({"ok": status == "dry_run", "status": status, "reason": reason,
            "signal_id": key, "mode": mode})
    };
    let held: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM positions WHERE symbol=?1 AND mode=?2 AND status='open')",
            (p.symbol.as_str(), mode),
            |r| r.get(0),
        )
        .unwrap_or(false);
    if held {
        return skipped("skipped", "同标的已有持仓，不重复开仓");
    }
    let symbol_row: Option<(i64, String, String, f64, i64)> = conn
        .query_row(
            "SELECT max_leverage, min_sz, tick_sz, min_notional, enabled FROM trading_symbols WHERE symbol=?1",
            [p.symbol.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .ok();
    let Some((symbol_leverage, min_sz, _tick_sz, min_notional, enabled)) = symbol_row else {
        return skipped(
            "skipped",
            if cfg.auto_add_symbol {
                "标的不在池内，自动加池需要合约表校验，当前仅审计"
            } else {
                "标的不在池内且未开启自动添加"
            },
        );
    };
    if enabled == 0 {
        return skipped("skipped", "标的在池内但已停用，不自动启用");
    }
    if !(p.entry.is_finite() && p.entry > 0.0) {
        return skipped(
            "skipped",
            "开仓信号缺少有效 entry/price，拒绝用本地价格代替",
        );
    }
    let equity = account_equity(conn, mode);
    if equity <= 0.0 {
        return skipped("skipped", "缺少账户权益快照，等待引擎完成一轮后重试");
    }
    let risk_max_leverage = risk_value(conn, "max_leverage", 100.0) as i64;
    let leverage = (p.leverage.max(0.0) as i64 + cfg.leverage_bonus)
        .clamp(1, risk_max_leverage.clamp(1, 100))
        .min(symbol_leverage.clamp(1, 100));
    let margin_pct = if cfg.margin_pct_override > 0.0 {
        cfg.margin_pct_override
    } else {
        risk_value(conn, "margin_pct", 50.0)
    };
    let step = min_sz.parse::<f64>().unwrap_or(0.001).max(0.0);
    let align = |qty: f64| {
        if step > 0.0 {
            (qty / step).floor() * step
        } else {
            qty
        }
    };
    let mut qty = align(equity * margin_pct / 100.0 * leverage as f64 / p.entry);
    if qty * p.entry < min_notional {
        qty = align(min_notional / p.entry + 1e-9);
    }
    if qty <= 0.0 {
        return skipped("skipped", "按权益与保证金比例计算出的仓位小于最小下单量");
    }
    let margin = qty * p.entry / leverage as f64;
    let sl_pct = if p.stop > 0.0 {
        ((p.entry - p.stop).abs() / p.entry * 100.0).clamp(0.05, 20.0)
    } else {
        // 缺省止损按保证金收益率：价格距离 = 保证金% ÷ 杠杆。
        (cfg.default_sl_pct / leverage as f64).clamp(0.05, 20.0)
    };
    let tp_pct = if p.tp > 0.0 {
        ((p.tp - p.entry).abs() / p.entry * 100.0).clamp(0.1, 100.0)
    } else {
        (sl_pct * 1.6).max(sl_pct + 0.1)
    };
    let sl_price = if p.side == "long" {
        p.entry * (1.0 - sl_pct / 100.0)
    } else {
        p.entry * (1.0 + sl_pct / 100.0)
    };
    let tp_price = if p.side == "long" {
        p.entry * (1.0 + tp_pct / 100.0)
    } else {
        p.entry * (1.0 - tp_pct / 100.0)
    };
    if cfg.dry_run {
        finish_follow(
            conn,
            key,
            "dry_run",
            &format!("演练模式：计划以 {leverage}x 开 {} {:.6}", p.side, qty),
            leverage,
            qty,
            p.entry,
            margin,
        );
        return json!({
            "ok": true, "status": "dry_run", "reason": "信号跟单处于演练模式",
            "signal_id": key, "mode": mode, "leverage": leverage,
            "qty": qty, "price": p.entry, "margin": margin
        });
    }
    let side = if p.side == "long" { "BUY" } else { "SELL" };
    let fee_rate = risk_value(conn, "fee_rate_taker", 0.0005);
    let now = crate::now();
    let tx = match conn.unchecked_transaction() {
        Ok(tx) => tx,
        Err(error) => return skipped("error", &format!("开启事务失败: {error}")),
    };
    let order = tx.execute(
        "INSERT INTO orders(ts,symbol,side,order_type,qty,price,status,purpose,client_order_id,exchange_order_id,reduce_only,mode,latency_ms,raw) \
         VALUES(?1,?2,?3,'MARKET',?4,?5,'FILLED','entry',?6,'',0,?7,0,?8)",
        rusqlite::params![
            now,
            p.symbol,
            side,
            qty,
            p.entry,
            key,
            mode,
            json!({"source": "signal_follow", "raw": p.raw}).to_string()
        ],
    );
    let position = order.and_then(|_| {
        tx.execute(
            "INSERT INTO positions(symbol,side,qty,entry_price,mark_price,leverage,margin,sl_price,tp_price,peak_price,initial_sl_price,signal_score,opened_at,closed_at,exit_price,exit_reason,realized_pnl,fees,status,mode,venue_unrealized,venue_ts,partial_taken_at,ai_confidence,ai_reason,entry_slippage_bps,trade_id) \
             VALUES(?1,?2,?3,?4,?4,?5,?6,?7,?8,?4,?7,0,?9,0,0,'',0,?10,'open',?11,0,0,0,0,'',0,0)",
            rusqlite::params![
                p.symbol,
                p.side,
                qty,
                p.entry,
                leverage,
                margin,
                sl_price,
                tp_price,
                now,
                qty * p.entry * fee_rate,
                mode
            ],
        )
    });
    match position.and_then(|_| tx.commit()) {
        Ok(()) => {
            finish_follow(
                conn,
                key,
                "opened",
                &format!("跟单开仓 {} {:.6} @ {:.6}", p.side, qty, p.entry),
                leverage,
                qty,
                p.entry,
                margin,
            );
            json!({
                "ok": true, "status": "opened", "reason": "",
                "signal_id": key, "mode": mode, "leverage": leverage,
                "qty": qty, "price": p.entry, "margin": margin
            })
        }
        Err(error) => skipped("error", &format!("开仓记账失败: {error}")),
    }
}

fn close_demo(
    conn: &rusqlite::Connection,
    cfg: &SignalSettings,
    p: &ParsedSignal,
    key: &str,
    mode: &str,
) -> Value {
    let _ = cfg;
    let row: Option<(i64, String, f64, f64, f64, f64, i64, f64)> = conn
        .query_row(
            "SELECT id, side, qty, entry_price, COALESCE(NULLIF(mark_price,0),entry_price), leverage, opened_at, fees \
             FROM positions WHERE symbol=?1 AND mode=?2 AND status='open' \
             AND (?3='' OR side=?3) ORDER BY id LIMIT 1",
            (p.symbol.as_str(), mode, p.side.as_str()),
            |r| {
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
            },
        )
        .ok();
    let Some((id, side, qty, entry_price, mark_price, leverage, opened_at, fees)) = row else {
        finish_follow(conn, key, "skipped", "没有可平仓的持仓", 0, 0.0, 0.0, 0.0);
        return json!({"ok":true,"status":"skipped","reason":"没有可平仓的持仓","signal_id":key,"mode":mode});
    };
    let exit_price = if p.entry > 0.0 { p.entry } else { mark_price };
    let now = crate::now();
    let pnl = (exit_price - entry_price) * qty * if side == "long" { 1.0 } else { -1.0 };
    let fee_rate = risk_value(conn, "fee_rate_taker", 0.0005);
    let exit_fee = exit_price * qty * fee_rate;
    let tx = match conn.unchecked_transaction() {
        Ok(tx) => tx,
        Err(error) => {
            finish_follow(
                conn,
                key,
                "error",
                &format!("开启事务失败: {error}"),
                0,
                0.0,
                0.0,
                0.0,
            );
            return json!({"ok":false,"status":"error","reason":error.to_string(),"signal_id":key,"mode":mode});
        }
    };
    let result = tx
        .execute(
            "INSERT INTO trades(symbol,side,qty,entry_price,exit_price,leverage,notional,pnl,pnl_pct,roi_pct,fees,opened_at,closed_at,hold_seconds,exit_reason,ai_confidence,ai_reason,signal_score,slippage_bps,mode,reviewed,review_note,features,mantra_ids) \
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,'signal_follow',0,'',0,0,?15,0,'',NULL,NULL)",
            rusqlite::params![
                p.symbol,
                side,
                qty,
                entry_price,
                exit_price,
                leverage,
                qty * entry_price,
                pnl,
                if entry_price > 0.0 { pnl / (qty * entry_price) * 100.0 } else { 0.0 },
                if entry_price > 0.0 { pnl / (qty * entry_price / leverage) * 100.0 } else { 0.0 },
                fees + exit_fee,
                opened_at,
                now,
                (now - opened_at).max(0),
                mode
            ],
        )
        .and_then(|_| {
            tx.execute(
                "UPDATE positions SET status='closed', closed_at=?1, exit_price=?2, exit_reason='signal_follow', realized_pnl=?3, fees=fees+?4 WHERE id=?5",
                rusqlite::params![now, exit_price, pnl, exit_fee, id],
            )
        })
        .and_then(|_| tx.commit());
    match result {
        Ok(()) => {
            finish_follow(
                conn,
                key,
                "closed",
                &format!("跟单平仓 {} @ {:.6}，盈亏 {:.4}", side, exit_price, pnl),
                leverage as i64,
                qty,
                exit_price,
                qty * entry_price / leverage.max(1.0),
            );
            json!({"ok":true,"status":"closed","reason":"","signal_id":key,"mode":mode,
                "qty":qty,"price":exit_price,"pnl":pnl})
        }
        Err(error) => {
            finish_follow(
                conn,
                key,
                "error",
                &format!("平仓记账失败: {error}"),
                0,
                0.0,
                0.0,
                0.0,
            );
            json!({"ok":false,"status":"error","reason":error.to_string(),"signal_id":key,"mode":mode})
        }
    }
}

fn protect_demo(conn: &rusqlite::Connection, p: &ParsedSignal, key: &str, mode: &str) -> Value {
    let row: Option<(i64, String, f64)> = conn
        .query_row(
            "SELECT id, side, entry_price FROM positions WHERE symbol=?1 AND mode=?2 AND status='open' ORDER BY id LIMIT 1",
            (p.symbol.as_str(), mode),
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .ok();
    let Some((id, side, entry)) = row else {
        finish_follow(
            conn,
            key,
            "skipped",
            "保护信号没有对应持仓",
            0,
            0.0,
            0.0,
            0.0,
        );
        return json!({"ok":true,"status":"skipped","reason":"保护信号没有对应持仓","signal_id":key,"mode":mode});
    };
    let sl = if p.stop > 0.0 {
        p.stop
    } else if side == "long" {
        entry * 0.98
    } else {
        entry * 1.02
    };
    let tp = if p.tp > 0.0 {
        p.tp
    } else if side == "long" {
        entry * 1.03
    } else {
        entry * 0.97
    };
    let _ = conn.execute(
        "UPDATE positions SET sl_price=?1, tp_price=?2, initial_sl_price=?1 WHERE id=?3",
        rusqlite::params![sl, tp, id],
    );
    finish_follow(
        conn,
        key,
        "protect_recorded",
        "已更新本地保护价（live 由交易所接管）",
        0,
        0.0,
        0.0,
        0.0,
    );
    json!({"ok":true,"status":"protect_recorded","reason":"","signal_id":key,"mode":mode,
        "sl_price":sl,"tp_price":tp})
}

// --------------------------------------------------------- read once / cursor

fn resolve_jsonl_path(cfg: &SignalSettings) -> String {
    let raw = if cfg.jsonl_path.trim().is_empty() {
        std::env::var("SIGNAL_FOLLOW_JSONL").unwrap_or_default()
    } else {
        cfg.jsonl_path.clone()
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return String::new();
    }
    let path = std::path::Path::new(raw);
    if path.is_dir() {
        path.join("signals.jsonl").to_string_lossy().to_string()
    } else {
        raw.to_string()
    }
}

fn cursor_key(path: &str) -> String {
    format!("signal_follow_cursor.{}", short_hash(path))
}

fn read_cursor(db: &Db, path: &str) -> Option<u64> {
    let conn = db.conn.lock().ok()?;
    conn.query_row(
        "SELECT value FROM app_settings WHERE key = ?1",
        [cursor_key(path)],
        |r| r.get::<_, String>(0),
    )
    .ok()
    .and_then(|value| value.parse().ok())
}

fn write_cursor(db: &Db, path: &str, offset: u64) {
    let Ok(conn) = db.conn.lock() else {
        return;
    };
    let _ = conn.execute(
        "INSERT INTO app_settings(key,value,updated_at) VALUES(?1,?2,?3) \
         ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
        rusqlite::params![cursor_key(path), offset.to_string(), crate::now()],
    );
}

fn read_new_lines(path: &str, offset: u64) -> (Vec<Value>, u64) {
    let Ok(metadata) = std::fs::metadata(path) else {
        return (Vec::new(), offset);
    };
    let size = metadata.len();
    if size <= offset {
        return (Vec::new(), offset);
    }
    let Ok(bytes) = std::fs::read(path) else {
        return (Vec::new(), offset);
    };
    if bytes.len() < offset as usize {
        return (Vec::new(), offset);
    }
    let end = (size as usize).min(bytes.len());
    let chunk = &bytes[offset as usize..end];
    let cut = match chunk.iter().rposition(|b| *b == b'\n') {
        Some(index) => index + 1,
        None => return (Vec::new(), offset),
    };
    let text = String::from_utf8_lossy(&chunk[..cut]);
    let rows = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect();
    (rows, offset + cut as u64)
}

async fn follow_read_once(State(state): State<AppState>) -> Json<Value> {
    let Some(db) = state.db.as_ref() else {
        return json!({"ok":false,"processed":0,"reason":"数据库未就绪"}).into();
    };
    let cfg = load_settings(&db);
    if !cfg.enabled {
        return json!({"ok":false,"processed":0,"reason":"信号跟单未开启"}).into();
    }
    let path = resolve_jsonl_path(&cfg);
    if path.is_empty() {
        return json!({"ok":false,"processed":0,"reason":"未配置 signals.jsonl 路径"}).into();
    }
    if !std::path::Path::new(&path).is_file() {
        return json!({"ok":false,"processed":0,"reason":format!("等待信号文件: {path}")}).into();
    }
    let offset = match read_cursor(&db, &path) {
        Some(offset) => offset,
        None => {
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            write_cursor(&db, &path, size);
            return json!({"ok":true,"processed":0,"path":path,"offset":size,
                "reason":"首次接入从文件尾部开始，不回放历史"})
            .into();
        }
    };
    let (rows, new_offset) = read_new_lines(&path, offset);
    let mut processed = 0;
    for row in &rows {
        if let Ok(parsed) = parse_signal(row, crate::now()) {
            let outcome = process_signal(&db, &cfg, &parsed, "signal-file", "file");
            if outcome["status"] != "error" {
                processed += 1;
            }
        }
    }
    if new_offset != offset {
        write_cursor(&db, &path, new_offset);
    }
    json!({"ok":true,"processed":processed,"path":path,"offset":new_offset}).into()
}

async fn follow_reset_cursor(State(state): State<AppState>) -> Json<Value> {
    let Some(db) = state.db.as_ref() else {
        return json!({"ok":false,"reason":"数据库未就绪"}).into();
    };
    let cfg = load_settings(&db);
    let path = resolve_jsonl_path(&cfg);
    if path.is_empty() || !std::path::Path::new(&path).is_file() {
        return json!({"ok":false,"reason":"信号文件尚不可用","path":path}).into();
    }
    let offset = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    write_cursor(&db, &path, offset);
    json!({"ok":true,"path":path,"offset":offset}).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use rusqlite::Connection;
    use tower::ServiceExt;

    fn fixture(path: &std::path::Path) -> Db {
        let _ = std::fs::remove_file(path);
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE app_settings(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER); \
             CREATE TABLE engine_state(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER); \
             CREATE TABLE trading_symbols(symbol TEXT PRIMARY KEY, max_leverage INTEGER, min_sz TEXT, tick_sz TEXT, min_notional REAL, enabled INTEGER); \
             CREATE TABLE orders(id INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER, symbol TEXT, side TEXT, order_type TEXT, qty REAL, price REAL, status TEXT, purpose TEXT, client_order_id TEXT, exchange_order_id TEXT, reduce_only INTEGER, mode TEXT, latency_ms INTEGER, raw TEXT); \
             CREATE TABLE positions(id INTEGER PRIMARY KEY AUTOINCREMENT, symbol TEXT, side TEXT, qty REAL, entry_price REAL, mark_price REAL, venue_unrealized REAL, venue_ts INTEGER, leverage REAL, margin REAL, sl_price REAL, tp_price REAL, peak_price REAL, opened_at INTEGER, closed_at INTEGER, exit_price REAL, exit_reason TEXT, realized_pnl REAL, fees REAL, ai_confidence REAL, ai_reason TEXT, signal_score REAL, features TEXT, entry_slippage_bps REAL, initial_sl_price REAL, partial_taken_at INTEGER, mode TEXT, status TEXT, trade_id INTEGER); \
             CREATE TABLE trades(id INTEGER PRIMARY KEY AUTOINCREMENT, symbol TEXT, side TEXT, qty REAL, entry_price REAL, exit_price REAL, leverage REAL, notional REAL, pnl REAL, pnl_pct REAL, roi_pct REAL, fees REAL, opened_at INTEGER, closed_at INTEGER, hold_seconds INTEGER, exit_reason TEXT, ai_confidence REAL, ai_reason TEXT, signal_score REAL, slippage_bps REAL, mode TEXT, reviewed INTEGER, review_note TEXT, features TEXT, mantra_ids TEXT); \
             CREATE TABLE signal_follows(id INTEGER PRIMARY KEY AUTOINCREMENT, signal_id TEXT NOT NULL UNIQUE, symbol TEXT, side TEXT, channel TEXT, status TEXT, reason TEXT, leverage INTEGER, qty REAL, price REAL, margin REAL, mode TEXT, payload TEXT, created_at INTEGER); \
             INSERT INTO app_settings VALUES('signal_follow','{\"enabled\":true,\"ingest_token\":\"secret-token\",\"max_requests_per_minute\":2,\"dry_run\":false}',0); \
             INSERT INTO app_settings VALUES('risk','{\"max_leverage\":100,\"margin_pct\":50,\"fee_rate_taker\":0.0005}',0); \
             INSERT INTO engine_state VALUES('mode','demo',0),('enabled','1',0),('snapshot.demo','{\"account\":{\"equity\":1000}}',0); \
             INSERT INTO trading_symbols VALUES('BTCUSDT',50,'0.001','0.1',5.0,1); \
             INSERT INTO signal_follows VALUES(1,'s1','BTCUSDT','long','file','opened','',5,0.1,60000,120,'demo','{}',100); \
             INSERT INTO signal_follows VALUES(2,'s2','ETHUSDT','short','file','skipped','不在币池',0,0,0,0,'demo','{}',300); \
             INSERT INTO signal_follows VALUES(3,'s3','BTCUSDT','long','file','error','保证金不足',0,0,0,0,'demo','{}',200);",
        )
        .unwrap();
        drop(conn);
        Db::open(path.to_str().unwrap()).unwrap()
    }

    fn app_for(db: Db) -> Router {
        let state = crate::test_state(Some(db));
        Router::new().nest("/api/v1", routes()).with_state(state)
    }

    fn post(_app: &Router, uri: &str, token: Option<&str>, payload: Value) -> Request<Body> {
        let mut builder = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json");
        if let Some(token) = token {
            builder = builder.header("x-signal-token", token);
        }
        builder.body(Body::from(payload.to_string())).unwrap()
    }

    async fn body_of(response: axum::response::Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn open_signal(id: &str) -> Value {
        json!({"id": id, "event": "open", "symbol": "BTCUSDT", "side": "long",
            "entry": 60000, "leverage": 5, "stop": 59400, "tp": 61200})
    }

    #[tokio::test]
    async fn events_filter_by_status_and_come_newest_first() {
        let path = std::env::temp_dir().join("nq-follow-events.db");
        let db = fixture(&path);
        let app = app_for(db);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/signal-follow/events?status=skipped")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_of(response).await;
        let items = body["items"].as_array().unwrap();
        assert_eq!(items.len(), 1, "只应返回被跳过的那条");
        assert_eq!(items[0]["reason"], "不在币池");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn status_hides_the_ingest_token_but_reports_it_is_set() {
        let path = std::env::temp_dir().join("nq-follow-status.db");
        let db = fixture(&path);
        let app = app_for(db);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/signal-follow/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_of(response).await;
        assert_eq!(body["ingest_token_set"], true);
        assert_eq!(
            body["settings"]["ingest_token"], "******",
            "直推密钥不能出现在观测接口里"
        );
        assert_eq!(body["settings"]["enabled"], true, "缺省配置必须合并后返回");
        assert_eq!(body["engine_enabled"], true);
        assert_eq!(body["counts"]["opened"], 1);
        assert_eq!(body["items"][0]["created_at"], 300, "最近的事件排在最前");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn ingest_requires_the_signal_token_and_writes_nothing_on_401() {
        let path = std::env::temp_dir().join("nq-follow-401.db");
        let db = fixture(&path);
        let app = app_for(db.clone());
        let response = app
            .oneshot(post(
                &Router::new(),
                "/api/v1/signal-follow/ingest",
                Some("wrong"),
                open_signal("nq-401"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let count: i64 = db
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM signal_follows", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 3, "鉴权失败不能落库");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn unknown_event_and_missing_side_are_rejected_with_400() {
        let path = std::env::temp_dir().join("nq-follow-400.db");
        let db = fixture(&path);
        let app = app_for(db.clone());
        let unknown =
            json!({"id":"nq-unknown","event":"lunar-eclipse","symbol":"BTCUSDT","side":"long"});
        let response = app
            .clone()
            .oneshot(post(
                &Router::new(),
                "/api/v1/signal-follow/ingest",
                Some("secret-token"),
                unknown,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let missing = json!({"id":"nq-missing","event":"open","symbol":"BTCUSDT"});
        let response = app
            .oneshot(post(
                &Router::new(),
                "/api/v1/signal-follow/ingest",
                Some("secret-token"),
                missing,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let count: i64 = db
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM signal_follows", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 3, "非法信号不能落库");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn a_duplicate_event_produces_only_one_order() {
        let path = std::env::temp_dir().join("nq-follow-idempotent.db");
        let db = fixture(&path);
        let app = app_for(db.clone());
        let first = app
            .clone()
            .oneshot(post(
                &Router::new(),
                "/api/v1/signal-follow/ingest",
                Some("secret-token"),
                open_signal("nq-dup-1"),
            ))
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let first_body = body_of(first).await;
        assert_eq!(first_body["status"], "opened", "{first_body}");
        let second = app
            .oneshot(post(
                &Router::new(),
                "/api/v1/signal-follow/ingest",
                Some("secret-token"),
                open_signal("nq-dup-1"),
            ))
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::OK);
        let second_body = body_of(second).await;
        assert_eq!(second_body["status"], "duplicate", "{second_body}");
        let conn = db.conn.lock().unwrap();
        let orders: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM orders WHERE client_order_id LIKE 'nq-dup-1:%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(orders, 1, "同一事件只能产生一单");
        let positions: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM positions WHERE status='open'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(positions, 1);
        let mode: String = conn
            .query_row("SELECT mode FROM positions LIMIT 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "demo", "demo 信号只落本地记账，不能碰主网");
        drop(conn);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn ingest_is_rate_limited_per_source() {
        let path = std::env::temp_dir().join("nq-follow-429.db");
        let db = fixture(&path);
        let app = app_for(db);
        for index in 0..2 {
            let response = app
                .clone()
                .oneshot(post(
                    &Router::new(),
                    "/api/v1/signal-follow/ingest",
                    Some("secret-token"),
                    open_signal(&format!("nq-rate-{index}")),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }
        let response = app
            .oneshot(post(
                &Router::new(),
                "/api/v1/signal-follow/ingest",
                Some("secret-token"),
                open_signal("nq-rate-over"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn read_once_starts_at_the_tail_and_only_processes_new_lines() {
        let path = std::env::temp_dir().join("nq-follow-read-once.db");
        let db = fixture(&path);
        let jsonl = std::env::temp_dir().join("nq-signals-read-once.jsonl");
        std::fs::write(
            &jsonl,
            format!("{}\n{}\n", open_signal("nq-r1"), open_signal("nq-r2")),
        )
        .unwrap();
        {
            let conn = db.conn.lock().unwrap();
            conn.execute(
                "UPDATE app_settings SET value = ?1 WHERE key = 'signal_follow'",
                [format!(
                    "{{\"enabled\":true,\"ingest_token\":\"secret-token\",\"jsonl_path\":{:?},\"dry_run\":false}}",
                    jsonl.to_string_lossy()
                )],
            )
            .unwrap();
        }
        let app = Router::new()
            .nest("/api/v1", routes())
            .with_state(crate::test_state(Some(db.clone())));
        let first = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/signal-follow/read-once")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let first_body = body_of(first).await;
        assert_eq!(
            first_body["processed"], 0,
            "首次接入不回放历史: {first_body}"
        );
        std::fs::write(
            &jsonl,
            format!(
                "{}\n{}\n{}\n",
                open_signal("nq-r1"),
                open_signal("nq-r2"),
                open_signal("nq-r3")
            ),
        )
        .unwrap();
        let second = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/signal-follow/read-once")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let second_body = body_of(second).await;
        assert_eq!(second_body["processed"], 1, "{second_body}");
        let _ = std::fs::remove_file(&jsonl);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn reset_cursor_moves_to_the_end_without_replaying() {
        let path = std::env::temp_dir().join("nq-follow-reset.db");
        let db = fixture(&path);
        let jsonl = std::env::temp_dir().join("nq-signals-reset.jsonl");
        std::fs::write(&jsonl, format!("{}\n", open_signal("nq-reset-1"))).unwrap();
        {
            let conn = db.conn.lock().unwrap();
            conn.execute(
                "UPDATE app_settings SET value = ?1 WHERE key = 'signal_follow'",
                [format!(
                    "{{\"enabled\":true,\"ingest_token\":\"secret-token\",\"jsonl_path\":{:?}}}",
                    jsonl.to_string_lossy()
                )],
            )
            .unwrap();
        }
        let app = Router::new()
            .nest("/api/v1", routes())
            .with_state(crate::test_state(Some(db.clone())));
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/signal-follow/reset-cursor")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = body_of(response).await;
        assert_eq!(
            body["offset"],
            std::fs::metadata(&jsonl).unwrap().len(),
            "{body}"
        );
        let _ = std::fs::remove_file(&jsonl);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn ingest_with_signal_token_passes_the_bearer_middleware() {
        let path = std::env::temp_dir().join("nq-follow-middleware.db");
        let db = fixture(&path);
        let app = crate::router(crate::test_state(Some(db.clone())), None);
        let response = app
            .oneshot(post(
                &Router::new(),
                "/api/v1/signal-follow/ingest",
                Some("secret-token"),
                open_signal("nq-mw-1"),
            ))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "X-Signal-Token 必须在 Bearer 中间件前放行"
        );
        let body = body_of(response).await;
        assert_eq!(body["status"], "opened", "{body}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn event_and_side_classification_is_fail_closed() {
        assert_eq!(classify_event("开仓"), "open");
        assert_eq!(classify_event("close_all"), "close");
        assert_eq!(
            classify_event("平仓 50%"),
            "unknown",
            "未知写法必须拒绝而不是猜"
        );
        assert_eq!(normalize_side("BUY"), "long");
        assert_eq!(normalize_side("开空"), "short");
        assert_eq!(normalize_side("moon"), "");
    }
}
