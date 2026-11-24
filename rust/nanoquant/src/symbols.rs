//! 标的池只读接口。列表形状对齐 `api/routes/symbols.py` 的 `_row`。

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
#[cfg(test)]
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::db::Db;

pub fn routes() -> Router<Db> {
    Router::new()
        .route("/symbols", get(list_symbols).post(add_symbol))
        .route("/symbols/bulk", post(add_symbols_bulk))
        .route("/symbols/available", get(available_symbols))
        .route(
            "/symbols/{symbol}",
            delete(delete_symbol).patch(patch_symbol),
        )
}

#[derive(Deserialize)]
struct SymbolInput {
    symbol: String,
}

#[derive(Deserialize)]
struct SymbolsBulkInput {
    symbols: Vec<String>,
}

#[derive(Deserialize)]
struct AvailableQuery {
    #[serde(default)]
    q: String,
    #[serde(default = "default_available_limit")]
    limit: i64,
}

fn default_available_limit() -> i64 {
    50
}

async fn available_symbols(
    State(db): State<Db>,
    Query(query): Query<AvailableQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let limit = query.limit.clamp(1, 200) as usize;
    let q = normalize_symbol(&query.q);
    let rows = exchange_contracts(&db).await?;
    let cap = leverage_cap(&db);
    let items: Vec<Value> = rows
        .iter()
        .filter(|row| {
            let symbol = row["symbol"].as_str().unwrap_or("");
            let searchable = row["symbol"].as_str().unwrap_or("").contains(&q)
                || row["baseAsset"].as_str().unwrap_or("").contains(&q);
            (q.is_empty() || searchable) && valid_contract(row, symbol)
        })
        .take(limit)
        .filter_map(|row| build_pool_row(row, cap))
        .map(|mut row| {
            row["tier_label"] = json!(if row["tier"] == "tier_1_bluechip" {
                "蓝筹主流"
            } else {
                "高弹性动量"
            });
            row
        })
        .collect();
    Ok(Json(
        json!({"items":items,"query":query.q,"hint":if items.is_empty() {"未匹配到可交易的币安 USDT 永续合约；中文名可直接加入为自定义标的（仅记录）"} else {""}}),
    ))
}

async fn add_symbol(
    State(db): State<Db>,
    Json(input): Json<SymbolInput>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let symbol = normalize_symbol(&input.symbol);
    let contracts = exchange_contracts(&db).await?;
    add_with_contracts(&db, &contracts, &symbol).map(Json)
}

/// 把一只已知的币安合约写入标的池。`symbol` 必须存在于 `contracts` 且当前可交易；
/// 调用方负责池上限与重复检查。函数只做纯插入，主键冲突由 SQLite 报错，绝不改动既有行。
pub(crate) fn upsert_contract_row_conn(
    conn: &rusqlite::Connection,
    contracts: &[Value],
    symbol: &str,
) -> Result<Value, String> {
    if !plausible_symbol(symbol) {
        return Err("标的名称无效".into());
    }
    let raw = contracts
        .iter()
        .find(|row| row["symbol"].as_str() == Some(symbol))
        .ok_or_else(|| format!("{symbol} 不在币安合约表中"))?;
    if !valid_contract(raw, symbol) {
        return Err("仅允许当前可交易的 USDT 永续合约".into());
    }
    let cap = conn
        .query_row(
            "SELECT value FROM app_settings WHERE key='risk'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .and_then(|value| serde_json::from_str::<Value>(&value).ok())
        .and_then(|risk| risk["max_leverage"].as_i64())
        .unwrap_or(20)
        .clamp(1, 100);
    let mut row = build_pool_row(raw, cap).ok_or_else(|| "币安合约规格无效".to_string())?;
    let underlying = raw["underlyingType"].as_str().unwrap_or("").to_uppercase();
    let note = if underlying == "EQUITY" || underlying == "COMMODITY" {
        "股票/商品类永续".to_string()
    } else {
        String::new()
    };
    let order: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(sort_order),0)+1 FROM trading_symbols",
            [],
            |r| r.get(0),
        )
        .unwrap_or(1);
    conn.execute(
        "INSERT INTO trading_symbols(symbol,name,tier,sl_atr_mult,enabled,max_leverage,precision,tick_sz,min_sz,min_notional,ct_val,min_score,note,sort_order,created_at,updated_at) VALUES(?1,?2,?3,?4,1,?5,?6,?7,?8,?9,1,0,?10,?11,?12,?12)",
        rusqlite::params![symbol, row["name"].as_str().unwrap_or(symbol), row["tier"].as_str().unwrap_or("tier_2_momentum"), row["sl_atr_mult"].as_f64().unwrap_or(2.2), row["max_leverage"].as_i64().unwrap_or(20), row["precision"].as_i64().unwrap_or(3), row["tick_sz"].as_str().unwrap_or("0.0001"), row["min_sz"].as_str().unwrap_or("0.001"), row["min_notional"].as_f64().unwrap_or(5.0), note, order, crate::now()],
    )
    .map_err(|_| "标的写入失败".to_string())?;
    row["enabled"] = json!(true);
    row["custom"] = json!(false);
    Ok(row)
}

/// 同 [`upsert_contract_row_conn`]，自行加锁。
pub(crate) fn upsert_contract_row(
    db: &Db,
    contracts: &[Value],
    symbol: &str,
) -> Result<Value, String> {
    let conn = db.conn.lock().map_err(|_| "数据库锁不可用".to_string())?;
    upsert_contract_row_conn(&conn, contracts, symbol)
}

/// 添加一只标的的全部校验与落库。合约表由调用方注入，便于离线回归。
fn add_with_contracts(
    db: &Db,
    contracts: &[Value],
    symbol: &str,
) -> Result<Value, (StatusCode, Json<Value>)> {
    if !plausible_symbol(symbol) {
        return Err(pool_error(
            StatusCode::BAD_REQUEST,
            "请输入合约名或以 USDT 结尾的自定义标的（如 牛来USDT）",
        ));
    }
    let raw = contracts
        .iter()
        .find(|row| row["symbol"].as_str() == Some(symbol));
    let is_custom = raw.is_none() && !symbol.is_ascii();
    if let Some(raw) = raw {
        if !valid_contract(raw, symbol) {
            return Err(pool_error(
                StatusCode::BAD_REQUEST,
                "仅允许添加当前可交易的 USDT 永续合约",
            ));
        }
    } else if !is_custom {
        return Err(pool_error(
            StatusCode::BAD_REQUEST,
            &format!("{symbol} 不在当前环境的币安合约表中"),
        ));
    }
    let custom_row = is_custom.then(|| custom_pool_row(symbol, leverage_cap(db)));
    let maximum = pool_maximum(db);
    let conn = db.conn.lock().expect("db");
    let exists: bool = conn
        .query_row(
            "SELECT 1 FROM trading_symbols WHERE symbol=?1",
            [symbol],
            |_| Ok(()),
        )
        .is_ok();
    if exists {
        return Err(pool_error(StatusCode::CONFLICT, "该币种已在交易池中"));
    }
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM trading_symbols", [], |r| r.get(0))
        .unwrap_or(0);
    if count >= maximum {
        return Err(pool_error(
            StatusCode::CONFLICT,
            &format!("交易池最多允许 {maximum} 个币种；请先删除一个无持仓币种"),
        ));
    }
    if let Some(row) = custom_row {
        let order: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(sort_order),0)+1 FROM trading_symbols",
                [],
                |r| r.get(0),
            )
            .unwrap_or(1);
        conn.execute("INSERT INTO trading_symbols(symbol,name,tier,sl_atr_mult,enabled,max_leverage,precision,tick_sz,min_sz,min_notional,ct_val,min_score,note,sort_order,created_at,updated_at) VALUES(?1,?2,?3,?4,0,?5,?6,?7,?8,?9,1,0,?10,?11,?12,?12)",
            rusqlite::params![symbol, row["name"].as_str().unwrap_or(symbol), row["tier"].as_str().unwrap_or("custom"), row["sl_atr_mult"].as_f64().unwrap_or(2.2), row["max_leverage"].as_i64().unwrap_or(20), row["precision"].as_i64().unwrap_or(3), row["tick_sz"].as_str().unwrap_or("0.0001"), row["min_sz"].as_str().unwrap_or("0.001"), row["min_notional"].as_f64().unwrap_or(5.0), "自定义标的（无币安行情，不参与交易）", order, crate::now()])
            .map_err(|_| pool_error(StatusCode::INTERNAL_SERVER_ERROR, "标的写入失败"))?;
        let mut added = row;
        added["enabled"] = json!(false);
        added["custom"] = json!(true);
        return Ok(json!({"ok":true,"added":added,"count":count + 1,"custom":true}));
    }
    drop(conn);
    let added = upsert_contract_row(db, contracts, symbol)
        .map_err(|_| pool_error(StatusCode::INTERNAL_SERVER_ERROR, "标的写入失败"))?;
    Ok(json!({"ok":true,"added":added,"count":count + 1,"custom":false}))
}

async fn add_symbols_bulk(
    State(db): State<Db>,
    Json(input): Json<SymbolsBulkInput>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if input.symbols.len() > 100 {
        return Err(pool_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "批量添加最多 100 个标的",
        ));
    }
    let contracts = exchange_contracts(&db).await?;
    Ok(Json(bulk_with_contracts(&db, &contracts, input.symbols)))
}

/// 批量添加逐项校验，单项失败只影响该项。返回结构与 legacy 内核（已退役） 一致。
fn bulk_with_contracts(db: &Db, contracts: &[Value], symbols: Vec<String>) -> Value {
    let mut added = Vec::new();
    let mut skipped = Vec::new();
    let mut invalid = Vec::new();
    for raw in symbols {
        let symbol = normalize_symbol(&raw);
        if symbol.is_empty() {
            continue;
        }
        match add_with_contracts(db, contracts, &symbol) {
            Ok(_) => added.push(symbol),
            Err((status, Json(body)))
                if status == StatusCode::CONFLICT
                    && body["detail"].as_str().unwrap_or("").contains("已在交易池") =>
            {
                skipped.push(symbol)
            }
            Err((_, Json(body))) => invalid.push(json!({"symbol":symbol,"reason":body["detail"]})),
        }
    }
    let count: i64 = db
        .conn
        .lock()
        .expect("db")
        .query_row("SELECT COUNT(*) FROM trading_symbols", [], |r| r.get(0))
        .unwrap_or(0);
    json!({"added":added,"skipped":skipped,"invalid":invalid,"ok":true,"count":count})
}

async fn exchange_contracts(db: &Db) -> Result<Vec<Value>, (StatusCode, Json<Value>)> {
    static CLIENT: std::sync::LazyLock<Result<reqwest::Client, reqwest::Error>> =
        std::sync::LazyLock::new(|| {
            reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
        });
    let url = contracts_url(&current_mode(db));
    let body = CLIENT
        .as_ref()
        .map_err(|_| pool_error(StatusCode::BAD_GATEWAY, "币安合约表客户端不可用"))?
        .get(&url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|_| pool_error(StatusCode::BAD_GATEWAY, "无法读取币安合约表"))?
        .json::<Value>()
        .await
        .map_err(|_| pool_error(StatusCode::BAD_GATEWAY, "币安合约表响应无效"))?;
    body["symbols"]
        .as_array()
        .cloned()
        .ok_or_else(|| pool_error(StatusCode::BAD_GATEWAY, "币安合约表缺少 symbols"))
}

/// 合约表域名必须跟随交易环境：demo 只碰测试网，live 才用主网。
fn contracts_url(mode: &str) -> String {
    let host = if mode == "live" {
        "fapi.binance.com"
    } else {
        "testnet.binancefuture.com"
    };
    format!("https://{host}/fapi/v1/exchangeInfo")
}

fn pool_error(status: StatusCode, detail: &str) -> (StatusCode, Json<Value>) {
    (status, Json(json!({"detail":detail})))
}

fn custom_pool_row(symbol: &str, leverage_cap: i64) -> Value {
    let base = symbol.strip_suffix("USDT").unwrap_or(symbol);
    json!({
        "symbol": symbol,
        "name": base,
        "tier": "custom",
        "sl_atr_mult": 2.2,
        "max_leverage": leverage_cap.clamp(1, 100),
        "precision": 3,
        "tick_sz": "0.0001",
        "min_sz": "0.001",
        "min_notional": 5.0,
        "ct_val": 1.0
    })
}

fn plausible_symbol(symbol: &str) -> bool {
    let length = symbol.chars().count();
    (6..=32).contains(&length)
        && symbol
            .strip_suffix("USDT")
            .is_some_and(|body| !body.is_empty() && body.chars().any(|c| c.is_alphabetic()))
        && symbol.chars().all(|c| c.is_alphanumeric())
}

fn valid_contract(row: &Value, symbol: &str) -> bool {
    row["symbol"].as_str() == Some(symbol)
        && matches!(
            row["contractType"].as_str(),
            Some("PERPETUAL") | Some("TRADIFI_PERPETUAL")
        )
        && (row["status"] == "TRADING"
            || (row["contractType"] == "TRADIFI_PERPETUAL" && row["status"] == "BREAK"))
        && row["quoteAsset"] == "USDT"
}

fn filter_value(row: &Value, kind: &str, key: &str, default: &str) -> String {
    row["filters"]
        .as_array()
        .and_then(|filters| filters.iter().find(|f| f["filterType"] == kind))
        .and_then(|f| f[key].as_str())
        .filter(|v| !v.is_empty())
        .unwrap_or(default)
        .to_owned()
}

fn decimal_precision(step: &str) -> i64 {
    let value = step.trim_end_matches('0');
    value
        .split_once('.')
        .map(|(_, digits)| digits.len() as i64)
        .unwrap_or(0)
}

fn build_pool_row(raw: &Value, leverage_cap: i64) -> Option<Value> {
    let symbol = raw["symbol"].as_str()?;
    let base = raw["baseAsset"]
        .as_str()
        .unwrap_or_else(|| symbol.strip_suffix("USDT").unwrap_or(symbol));
    let tier = if matches!(base, "BTC" | "ETH") {
        "tier_1_bluechip"
    } else {
        "tier_2_momentum"
    };
    let sl = if tier == "tier_1_bluechip" { 1.8 } else { 2.2 };
    let step = filter_value(raw, "LOT_SIZE", "stepSize", "0.001");
    let min_qty = filter_value(raw, "LOT_SIZE", "minQty", &step);
    let tick = filter_value(raw, "PRICE_FILTER", "tickSize", "0.0001");
    let min_notional = filter_value(raw, "MIN_NOTIONAL", "notional", "5")
        .parse::<f64>()
        .ok()?;
    Some(
        json!({"symbol":symbol,"name":base,"tier":tier,"sl_atr_mult":sl,
        "max_leverage":leverage_cap.clamp(1, 100),"precision":decimal_precision(&step),"tick_sz":tick,
        "min_sz":min_qty,"min_notional":min_notional,"ct_val":1.0}),
    )
}

/// 单标的杠杆上限跟随全局风控，legacy 内核（已退役） 基线是 `max(1, min(risk.max_leverage, 100))`。
fn leverage_cap(db: &Db) -> i64 {
    db.setting("risk")
        .ok()
        .flatten()
        .and_then(|v| v["max_leverage"].as_i64())
        .unwrap_or(100)
        .clamp(1, 100)
}

fn pool_maximum(db: &Db) -> i64 {
    db.setting("data")
        .ok()
        .flatten()
        .and_then(|v| v["max_pool_size"].as_i64())
        .unwrap_or(20)
        .clamp(1, 200)
}

#[derive(Deserialize, Default)]
struct DeleteQuery {
    #[serde(default)]
    confirmation: String,
}

#[derive(Deserialize, Default)]
struct DeleteBody {
    #[serde(default)]
    confirmation: String,
}

/// 删除必须带精确的 `REMOVE <SYMBOL>`。当前模式下有未平仓持仓一律拒绝：
/// 删掉之后止损止盈就没人接管了。交易所实时持仓的确认要等凭证接入，这里只判本地。
async fn delete_symbol(
    State(db): State<Db>,
    Path(symbol): Path<String>,
    Query(query): Query<DeleteQuery>,
    body: Option<Json<DeleteBody>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let symbol = normalize_symbol(&symbol);
    let phrase = body
        .as_ref()
        .map(|b| b.confirmation.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or(query.confirmation.trim())
        .to_uppercase();
    if phrase != format!("REMOVE {symbol}") {
        return Err((
            StatusCode::BAD_REQUEST,
            json!({ "detail": format!("必须提供确认短语（精确为：REMOVE {symbol}）") }).into(),
        ));
    }
    let conn = db.conn.lock().expect("db");
    let exists: bool = conn
        .query_row(
            "SELECT 1 FROM trading_symbols WHERE symbol = ?1",
            [&symbol],
            |_| Ok(()),
        )
        .is_ok();
    if !exists {
        return Err((
            StatusCode::NOT_FOUND,
            json!({ "detail": "该币种不在交易池中" }).into(),
        ));
    }
    let mode = mode_from_conn(&conn);
    let held: bool = conn
        .query_row(
            "SELECT 1 FROM positions WHERE status = 'open' AND symbol = ?1 AND mode = ?2",
            (&symbol, &mode),
            |_| Ok(()),
        )
        .is_ok();
    if held {
        return Err((
            StatusCode::CONFLICT,
            json!({ "detail": "该标的仍有持仓，删除后将失去止损/止盈接管，禁止删除" }).into(),
        ));
    }
    conn.execute("DELETE FROM trading_symbols WHERE symbol = ?1", [&symbol])
        .expect("delete symbol");
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM trading_symbols", [], |r| r.get(0))
        .unwrap_or(0);
    Ok(json!({ "ok": true, "removed": symbol, "count": count }).into())
}

#[derive(Deserialize, Default)]
struct SymbolPatch {
    enabled: Option<bool>,
    max_leverage: Option<i64>,
    min_score: Option<f64>,
    note: Option<String>,
}

async fn patch_symbol(
    State(db): State<Db>,
    Path(symbol): Path<String>,
    Json(patch): Json<SymbolPatch>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let symbol = normalize_symbol(&symbol);
    let conn = db.conn.lock().expect("db");
    let exists: bool = conn
        .query_row(
            "SELECT 1 FROM trading_symbols WHERE symbol = ?1",
            [&symbol],
            |_| Ok(()),
        )
        .is_ok();
    if !exists {
        return Err((
            StatusCode::NOT_FOUND,
            json!({ "detail": "标的未在池中" }).into(),
        ));
    }
    let tier: String = conn
        .query_row(
            "SELECT tier FROM trading_symbols WHERE symbol = ?1",
            [&symbol],
            |r| r.get(0),
        )
        .unwrap_or_default();
    if patch.enabled == Some(true) && tier == "custom" {
        return Err((
            StatusCode::BAD_REQUEST,
            json!({ "detail": "自定义标的没有币安行情，不能启用交易；请添加对应的 ASCII 合约名" })
                .into(),
        ));
    }
    if let Some(enabled) = patch.enabled {
        conn.execute(
            "UPDATE trading_symbols SET enabled = ?1 WHERE symbol = ?2",
            (i64::from(enabled), &symbol),
        )
        .expect("patch enabled");
    }
    if let Some(leverage) = patch.max_leverage {
        conn.execute(
            "UPDATE trading_symbols SET max_leverage = ?1 WHERE symbol = ?2",
            (leverage, &symbol),
        )
        .expect("patch leverage");
    }
    if let Some(score) = patch.min_score {
        conn.execute(
            "UPDATE trading_symbols SET min_score = ?1 WHERE symbol = ?2",
            (score, &symbol),
        )
        .expect("patch score");
    }
    if let Some(note) = patch.note {
        conn.execute(
            "UPDATE trading_symbols SET note = ?1 WHERE symbol = ?2",
            (&note, &symbol),
        )
        .expect("patch note");
    }
    Ok(json!({ "ok": true }).into())
}

fn normalize_symbol(raw: &str) -> String {
    raw.chars()
        .filter(|ch| ch.is_alphanumeric())
        .collect::<String>()
        .to_uppercase()
}

fn current_mode(db: &Db) -> String {
    let conn = db.conn.lock().expect("db");
    mode_from_conn(&conn)
}

fn mode_from_conn(conn: &rusqlite::Connection) -> String {
    let stored = conn
        .query_row(
            "SELECT value FROM engine_state WHERE key = 'mode'",
            [],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .filter(|mode| !mode.is_empty());
    if let Some(mode) = stored {
        return mode;
    }
    let raw: String = conn
        .query_row(
            "SELECT value FROM app_settings WHERE key = 'engine'",
            [],
            |r| r.get(0),
        )
        .unwrap_or_default();
    let mode = serde_json::from_str::<Value>(&raw)
        .ok()
        .and_then(|v| v.get("mode").and_then(|m| m.as_str()).map(str::to_string))
        .unwrap_or_default();
    if mode == "live" {
        "live".to_string()
    } else {
        "demo".to_string()
    }
}

async fn list_symbols(State(db): State<Db>) -> Json<Value> {
    let items = read_symbols(&db);
    let maximum = pool_maximum(&db);
    let symbols: Vec<Value> = items.iter().map(|item| item["symbol"].clone()).collect();
    json!({
        "items": items,
        "count": items.len(),
        "state": {
            "status": if items.is_empty() { "empty" } else { "ok" },
            "count": items.len(),
            "detail": if items.is_empty() { "标的池为空, 请在「标的池」页添加币安 USDT 永续合约" } else { "" },
            "symbols": symbols,
            "limits": {"minimum": 0, "maximum": maximum, "update_interval_hint": "每次保存即时生效"},
        },
        "limits": {"minimum": 0, "maximum": maximum, "update_interval_hint": "每次保存即时生效"},
    })
    .into()
}

fn read_symbols(db: &Db) -> Vec<Value> {
    let conn = db.conn.lock().expect("db");
    let mut stmt = conn
        .prepare(
            "SELECT symbol, name, tier, sl_atr_mult, enabled, max_leverage, precision, tick_sz, \
             min_sz, min_notional, ct_val, min_score, note, sort_order, created_at, updated_at \
             FROM trading_symbols ORDER BY sort_order, symbol",
        )
        .expect("prepare symbols");
    let rows = stmt
        .query_map([], |r| {
            Ok(json!({
                "symbol": r.get::<_, String>(0)?,
                "name": r.get::<_, String>(1)?,
                "tier": r.get::<_, String>(2)?,
                "sl_atr_mult": r.get::<_, f64>(3)?,
                "enabled": r.get::<_, i64>(4)? != 0,
                "max_leverage": r.get::<_, i64>(5)?,
                "precision": r.get::<_, i64>(6)?,
                "tick_sz": r.get::<_, String>(7)?,
                "min_sz": r.get::<_, String>(8)?,
                "min_notional": r.get::<_, f64>(9)?,
                "ct_val": r.get::<_, f64>(10)?,
                "min_score": r.get::<_, f64>(11)?,
                "note": r.get::<_, String>(12)?,
                "sort_order": r.get::<_, i64>(13)?,
                "created_at": r.get::<_, i64>(14)?,
                "updated_at": r.get::<_, i64>(15)?,
            }))
        })
        .expect("query symbols")
        .collect::<Result<Vec<_>, _>>()
        .expect("rows");
    rows
}

/// 测试夹具：建一张只含本模块用到的表的最小库。
///
/// 仅测试可见；正式构建不编译，避免把夹具当成可用 API。
#[cfg(test)]
fn seed(path: &std::path::Path) -> Db {
    let _ = std::fs::remove_file(path);
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE app_settings(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER); \
         CREATE TABLE trading_symbols(symbol TEXT PRIMARY KEY, name TEXT, tier TEXT, sl_atr_mult REAL, \
         enabled INTEGER, max_leverage INTEGER, precision INTEGER, tick_sz TEXT, min_sz TEXT, \
         min_notional REAL, ct_val REAL, min_score REAL, note TEXT, sort_order INTEGER, \
         created_at INTEGER, updated_at INTEGER); \
         CREATE TABLE positions(symbol TEXT, status TEXT, mode TEXT); \
         INSERT INTO trading_symbols VALUES('BTCUSDT','BTC','tier_1',2.0,1,20,2,'0.1','0.001',5.0,1.0,0.0,'',1,100,100);",
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

    fn contract(symbol: &str, contract_type: &str, status: &str, quote: &str) -> Value {
        json!({
            "symbol": symbol,
            "baseAsset": symbol.trim_end_matches(quote),
            "contractType": contract_type,
            "status": status,
            "quoteAsset": quote,
            "filters": [
                {"filterType": "LOT_SIZE", "stepSize": "0.0010", "minQty": "0.0010"},
                {"filterType": "PRICE_FILTER", "tickSize": "0.010"},
                {"filterType": "MIN_NOTIONAL", "notional": "5"}
            ]
        })
    }

    #[test]
    fn upsert_contract_row_inserts_known_contract() {
        let path = std::env::temp_dir().join("nq-symbol-scan-upsert.db");
        let db = seed(&path);
        db.conn
            .lock()
            .unwrap()
            .execute("DELETE FROM trading_symbols", [])
            .unwrap();
        let contracts = vec![contract("ETHUSDT", "PERPETUAL", "TRADING", "USDT")];
        let row = upsert_contract_row(&db, &contracts, "ETHUSDT").unwrap();
        assert_eq!(row["symbol"], "ETHUSDT");
        assert_eq!(row["enabled"], true);
        assert_eq!(row["custom"], false);
        let count: i64 = db
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM trading_symbols", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn upsert_contract_row_does_not_overwrite_existing_configuration() {
        let path = std::env::temp_dir().join("nq-symbol-scan-upsert-conflict.db");
        let db = seed(&path);
        db.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE trading_symbols SET enabled=0, min_score=9 WHERE symbol='BTCUSDT'",
                [],
            )
            .unwrap();
        let contracts = vec![contract("BTCUSDT", "PERPETUAL", "TRADING", "USDT")];
        let err = upsert_contract_row(&db, &contracts, "BTCUSDT").unwrap_err();
        assert!(err.contains("写入失败"), "主键冲突必须报错，实际错误：{err}");
        let (enabled, min_score): (i64, f64) = db
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT enabled, min_score FROM trading_symbols WHERE symbol='BTCUSDT'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(enabled, 0, "冲突不能把用户手动禁用的标的重新启用");
        assert_eq!(min_score, 9.0, "冲突不能覆盖既有评分门槛");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn upsert_contract_row_rejects_symbol_missing_from_contracts() {
        let path = std::env::temp_dir().join("nq-symbol-scan-upsert-missing.db");
        let db = seed(&path);
        let contracts = vec![contract("ETHUSDT", "PERPETUAL", "TRADING", "USDT")];
        let err = upsert_contract_row(&db, &contracts, "SOLUSDT").unwrap_err();
        assert!(
            err.contains("不在币安合约表中"),
            "不在合约表中的标的必须拒绝，实际错误：{err}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn contracts_url_follows_the_trading_environment() {
        assert_eq!(
            contracts_url("demo"),
            "https://testnet.binancefuture.com/fapi/v1/exchangeInfo",
            "demo 只能读测试网合约表，不能碰主网"
        );
        assert_eq!(
            contracts_url("live"),
            "https://fapi.binance.com/fapi/v1/exchangeInfo"
        );
    }

    #[test]
    fn valid_contract_rejects_non_perpetual_delisted_and_non_usdt() {
        assert!(valid_contract(
            &contract("BTCUSDT", "PERPETUAL", "TRADING", "USDT"),
            "BTCUSDT"
        ));
        assert!(!valid_contract(
            &contract("BTCUSDT", "CURRENT_QUARTER", "TRADING", "USDT"),
            "BTCUSDT"
        ));
        assert!(!valid_contract(
            &contract("BTCUSDT", "PERPETUAL", "SETTLING", "USDT"),
            "BTCUSDT"
        ));
        assert!(!valid_contract(
            &contract("BTCUSDC", "PERPETUAL", "TRADING", "USDC"),
            "BTCUSDC"
        ));
        assert!(!valid_contract(
            &contract("ETHUSDT", "PERPETUAL", "TRADING", "USDT"),
            "BTCUSDT"
        ));
    }

    #[test]
    fn build_pool_row_derives_precision_and_follows_the_leverage_cap() {
        let raw = contract("ETHUSDT", "PERPETUAL", "TRADING", "USDT");
        let row = build_pool_row(&raw, 50).expect("row");
        assert_eq!(row["precision"], 3, "stepSize 0.0010 应派生 3 位");
        assert_eq!(row["tick_sz"], "0.010");
        assert_eq!(row["min_sz"], "0.0010");
        assert_eq!(row["min_notional"], 5.0);
        assert_eq!(row["max_leverage"], 50, "单标的杠杆上限跟随全局风控");
        assert_eq!(
            build_pool_row(&raw, 500).unwrap()["max_leverage"],
            100,
            "单标的杠杆上限封顶 100"
        );
    }

    #[tokio::test]
    async fn list_symbols_reports_pool_limits_for_the_admin_view() {
        let path = std::env::temp_dir().join("nq-symbols-state.db");
        let db = seed(&path);
        let app = Router::new().nest("/api/v1", routes()).with_state(db);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/symbols")
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
        assert_eq!(body["state"]["status"], "ok");
        assert_eq!(body["state"]["limits"]["maximum"], 20);
        assert_eq!(body["limits"]["maximum"], 20);
        assert_eq!(body["state"]["symbols"][0], "BTCUSDT");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn add_with_contracts_rejects_duplicates_and_stops_at_capacity() {
        let path = std::env::temp_dir().join("nq-symbols-add.db");
        let db = seed(&path);
        {
            let conn = db.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO app_settings(key,value) VALUES('risk','{\"max_leverage\":50}')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO app_settings(key,value) VALUES('data','{\"max_pool_size\":2}')",
                [],
            )
            .unwrap();
        }
        let contracts = vec![
            contract("ETHUSDT", "PERPETUAL", "TRADING", "USDT"),
            contract("SOLUSDT", "PERPETUAL", "TRADING", "USDT"),
        ];
        let added = add_with_contracts(&db, &contracts, "ETHUSDT").expect("eth add");
        assert_eq!(added["count"], 2);
        assert_eq!(added["added"]["max_leverage"], 50);
        let duplicate = add_with_contracts(&db, &contracts, "ETHUSDT").unwrap_err();
        assert_eq!(duplicate.0, StatusCode::CONFLICT);
        let over = add_with_contracts(&db, &contracts, "SOLUSDT").unwrap_err();
        assert_eq!(over.0, StatusCode::CONFLICT);
        assert!(over.1["detail"].as_str().unwrap().contains("最多允许 2"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn bulk_add_reports_every_item_and_keeps_insertion_order() {
        let path = std::env::temp_dir().join("nq-symbols-bulk.db");
        let db = seed(&path);
        let contracts = vec![
            contract("BTCUSDT", "PERPETUAL", "TRADING", "USDT"),
            contract("ETHUSDT", "PERPETUAL", "TRADING", "USDT"),
            contract("SOLUSDT", "PERPETUAL", "TRADING", "USDT"),
        ];
        let result = bulk_with_contracts(
            &db,
            &contracts,
            vec![
                "ethusdt".into(),
                "BTCUSDT".into(),
                "NOPEUSDT".into(),
                "SOLUSDT".into(),
            ],
        );
        assert_eq!(result["added"], json!(["ETHUSDT", "SOLUSDT"]));
        assert_eq!(result["skipped"], json!(["BTCUSDT"]));
        assert_eq!(result["invalid"][0]["symbol"], "NOPEUSDT");
        assert_eq!(result["count"], 3);
        let symbols: Vec<String> = read_symbols(&db)
            .iter()
            .map(|row| row["symbol"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(symbols, vec!["BTCUSDT", "ETHUSDT", "SOLUSDT"]);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn lists_symbols_in_pool_order() {
        let path = std::env::temp_dir().join("nq-symbols.db");
        let db = seed(&path);
        let app = Router::new().nest("/api/v1", routes()).with_state(db);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/symbols")
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
        assert_eq!(body["count"], 1);
        assert_eq!(body["items"][0]["symbol"], "BTCUSDT");
        assert_eq!(body["items"][0]["enabled"], true);
        assert_eq!(body["items"][0]["max_leverage"], 20);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn deleting_with_the_wrong_phrase_is_rejected() {
        let path = std::env::temp_dir().join("nq-symbols-delete.db");
        let db = seed(&path);
        let app = Router::new().nest("/api/v1", routes()).with_state(db);
        let response = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/v1/symbols/BTCUSDT?confirmation=DELETE%20BTCUSDT")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "确认短语必须是 REMOVE，换个动词不行"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn a_lowercase_phrase_still_removes_the_symbol() {
        let path = std::env::temp_dir().join("nq-symbols-removed.db");
        let db = seed(&path);
        let app = Router::new().nest("/api/v1", routes()).with_state(db);
        let response = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/v1/symbols/btc-usdt?confirmation=remove%20btcusdt")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "短语忽略大小写，合约名里的分隔符会被去掉"
        );
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["removed"], "BTCUSDT");
        assert_eq!(body["count"], 0);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn an_open_position_blocks_removal() {
        let path = std::env::temp_dir().join("nq-symbols-held.db");
        let db = seed(&path);
        db.conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO positions(symbol, status, mode) VALUES('BTCUSDT','open','demo')",
                [],
            )
            .unwrap();
        let app = Router::new().nest("/api/v1", routes()).with_state(db);
        let response = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/v1/symbols/btcusdt?confirmation=REMOVE%20BTCUSDT")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::CONFLICT,
            "有持仓删掉就没人接管止损了"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn chinese_symbol_present_in_exchange_table_is_normal_enabled() {
        let path = std::env::temp_dir().join("nq-symbols-chinese-contract.db");
        let db = seed(&path);
        let contracts = vec![contract("牛来USDT", "PERPETUAL", "TRADING", "USDT")];
        let added =
            add_with_contracts(&db, &contracts, "牛来USDT").expect("真实中文合约应正常加入");
        assert_eq!(added["added"]["symbol"], "牛来USDT");
        assert_eq!(added["added"]["custom"], false);
        assert_eq!(added["added"]["enabled"], true);
        assert_eq!(added["added"]["tier"], "tier_2_momentum");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn chinese_symbol_normalization_preserves_unicode() {
        assert_eq!(normalize_symbol(" 牛来-usdt "), "牛来USDT");
        assert!(plausible_symbol("牛来USDT"));
        assert!(plausible_symbol("币安人生USDT"));
    }

    #[test]
    fn custom_chinese_symbol_is_added_as_a_disabled_pool_row() {
        let path = std::env::temp_dir().join("nq-symbols-custom-cn.db");
        let db = seed(&path);
        let added = add_with_contracts(&db, &[], "牛来USDT").expect("中文自定义标的应可加入");
        assert_eq!(added["added"]["tier"], "custom");
        assert_eq!(added["added"]["enabled"], false);
        let added2 = add_with_contracts(&db, &[], "币安人生USDT").expect("中文自定义标的应可加入");
        assert_eq!(added2["added"]["tier"], "custom");
        let rows = read_symbols(&db);
        assert!(rows.iter().any(|row| {
            row["symbol"] == "牛来USDT" && row["tier"] == "custom" && row["enabled"] == false
        }));
        assert!(rows.iter().any(|row| {
            row["symbol"] == "币安人生USDT" && row["tier"] == "custom" && row["enabled"] == false
        }));
        let _ = std::fs::remove_file(&path);
    }
}
