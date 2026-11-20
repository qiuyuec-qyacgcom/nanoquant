//! 公开前台只读接口。`/config` 对齐 `api/routes/public.py` 的 `public_config`，
//! 站点关闭时也允许读取，以便展示维护页。

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::db::Db;

pub fn routes() -> Router<Db> {
    Router::new()
        .route("/public/overview", get(home))
        .route("/public/config", get(config))
        .route("/public/news", get(public_news))
        .route("/public/performance", get(performance))
        .route("/public/pool", get(pool))
        .route("/public/mantras", get(mantras))
        .route("/public/klines", get(klines))
        .route("/public/klines/batch", get(klines_batch))
        .route("/public/quotes", get(quotes))
}

fn field<'a>(group: &'a Value, key: &str) -> Option<&'a Value> {
    group.get(key)
}

/// 站点关闭时前台数据一律不给，只保留 `/config` 供维护页读取。
fn require_site(db: &Db) -> Result<(), (StatusCode, Json<Value>)> {
    let public = db.setting("public").ok().flatten().unwrap_or(Value::Null);
    let enabled = public
        .get("site_enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    if enabled {
        Ok(())
    } else {
        Err((
            StatusCode::SERVICE_UNAVAILABLE,
            json!({ "detail": "公开站点已关闭" }).into(),
        ))
    }
}

async fn home(State(db): State<Db>) -> Result<Json<Value>, MarketError> {
    require_site(&db)?;
    let public = db
        .setting("public")
        .map_err(|_| public_unavailable())?
        .unwrap_or(Value::Null);
    let symbols = enabled_symbols(&db)?;
    let mut payload = json!({"updated_at": crate::now(), "symbol_count": symbols.len(),
        "pool": symbols, "pool_empty": symbols.is_empty(), "news": crate::news::sentiment_overview(&db)});
    if public["show_engine_status"].as_bool().unwrap_or(true) {
        let engine = db
            .setting("engine")
            .map_err(|_| public_unavailable())?
            .unwrap_or(Value::Null);
        let conn = db.conn.lock().expect("db");
        let last: i64 = conn
            .query_row(
                "SELECT value FROM engine_state WHERE key='last_cycle_ts'",
                [],
                |r| r.get::<_, String>(0),
            )
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        payload["engine"] = json!({"enabled": engine["enabled"].as_bool().unwrap_or(false),
            "mode": if engine["mode"] == "live" { "live" } else { "demo" }, "last_cycle_ts": last});
    }
    Ok(Json(payload))
}

#[derive(Deserialize)]
struct PublicNewsQuery {
    limit: Option<i64>,
    #[serde(default)]
    source: String,
}

async fn public_news(
    State(db): State<Db>,
    Query(query): Query<PublicNewsQuery>,
) -> Result<Json<Value>, MarketError> {
    require_site(&db)?;
    let limit = bounded(query.limit.unwrap_or(30), 1, 100, "limit")?;
    Ok(Json(
        json!({"items": crate::news::items(&db, limit, &query.source, ""),
        "sentiment": crate::news::sentiment_overview(&db)}),
    ))
}

#[derive(Deserialize)]
struct PerformanceQuery {
    window: Option<i64>,
}

async fn performance(
    State(db): State<Db>,
    Query(query): Query<PerformanceQuery>,
) -> Result<Json<Value>, MarketError> {
    require_site(&db)?;
    let window = bounded(query.window.unwrap_or(200), 10, 1000, "window")?;
    let public = db
        .setting("public")
        .map_err(|_| public_unavailable())?
        .unwrap_or(Value::Null);
    if !public["show_performance"].as_bool().unwrap_or(false) {
        return Ok(Json(json!({"enabled": false})));
    }
    let mode = if market_live(&db)? { "live" } else { "demo" };
    let conn = db.conn.lock().expect("db");
    let (total, wins, gross_win, gross_loss, hold): (i64, i64, f64, f64, f64) = conn.query_row(
        "SELECT COUNT(*), COUNT(CASE WHEN pnl > 0 THEN 1 END),
                COALESCE(SUM(MAX(pnl,0)),0), COALESCE(-SUM(MIN(pnl,0)),0), COALESCE(AVG(hold_seconds),0)
         FROM (SELECT pnl, hold_seconds FROM trades WHERE mode=?1 ORDER BY closed_at DESC LIMIT ?2)",
        (mode, window), |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
    ).map_err(|_| public_unavailable())?;
    let signals: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM signals WHERE mode=?1",
            [mode],
            |row| row.get(0),
        )
        .map_err(|_| public_unavailable())?;
    let win_rate = if total == 0 {
        0.0
    } else {
        (wins as f64 / total as f64 * 10_000.0).round() / 100.0
    };
    let profit_factor = if gross_loss > 0.0 {
        (gross_win / gross_loss * 1000.0).round() / 1000.0
    } else if gross_win > 0.0 {
        999.0
    } else {
        0.0
    };
    Ok(Json(
        json!({"enabled":true,"trades":total,"signals":signals,"win_rate":win_rate,
        "profit_factor":profit_factor,"avg_hold_seconds":hold as i64,"window":window}),
    ))
}

fn bounded(value: i64, min: i64, max: i64, field: &str) -> Result<i64, MarketError> {
    if (min..=max).contains(&value) {
        Ok(value)
    } else {
        Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"detail": format!("{field} 必须在 {min} 到 {max} 之间")})),
        ))
    }
}

fn public_unavailable() -> MarketError {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"detail": "公开数据暂不可用"})),
    )
}

/// 前台标的池，严格按添加时间排序，停用的不展示。
async fn pool(State(db): State<Db>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    require_site(&db)?;
    let conn = db.conn.lock().expect("db");
    let mut stmt = conn
        .prepare("SELECT symbol, name, tier, created_at FROM trading_symbols WHERE enabled = 1 ORDER BY sort_order, created_at, symbol")
        .expect("prepare pool");
    let rows: Vec<Value> = stmt
        .query_map([], |r| {
            Ok(json!({
                "symbol": r.get::<_, String>(0)?,
                "name": r.get::<_, String>(1)?,
                "tier": r.get::<_, String>(2)?,
                "tier_label": match r.get::<_, String>(2)?.as_str() { "tier_1" | "tier_1_bluechip" => "蓝筹主流", "tier_2" | "tier_2_momentum" => "高弹性动量", "tier_3" => "观察", _ => "自定义" },
                "added_at": r.get::<_, i64>(3)?,
            }))
        })
        .expect("query pool")
        .collect::<Result<_, _>>()
        .expect("pool rows");
    let symbols: Vec<Value> = rows
        .into_iter()
        .enumerate()
        .map(|(index, mut row)| {
            if let Some(obj) = row.as_object_mut() {
                obj.insert("order".to_string(), json!(index + 1));
            }
            row
        })
        .collect();
    let count = symbols.len();
    Ok(json!({ "symbols": symbols, "count": count, "empty": count == 0, "updated_at": crate::now() }).into())
}

#[derive(Deserialize)]
struct MantraQuery {
    limit: Option<i64>,
    category: Option<String>,
}

/// 脱敏心法：规则正文不出门。
async fn mantras(
    State(db): State<Db>,
    Query(query): Query<MantraQuery>,
) -> Result<Json<Value>, MarketError> {
    require_site(&db)?;
    let limit = bounded(query.limit.unwrap_or(8), 1, 30, "limit")?;
    let public = db
        .setting("public")
        .map_err(|_| public_unavailable())?
        .unwrap_or(Value::Null);
    if !public["show_mantras"].as_bool().unwrap_or(true) {
        return Ok(Json(json!({"enabled": false, "items": [], "stats": null})));
    }
    let conn = db.conn.lock().expect("db");
    let mut payload = crate::evolution_api::public_mantras(&conn, limit as usize);
    if let Some(category) = query.category.as_deref().filter(|value| !value.is_empty()) {
        if let Some(items) = payload["items"].as_array_mut() {
            items.retain(|item| item["category"].as_str() == Some(category));
        }
    }
    Ok(Json(payload))
}

#[derive(Deserialize)]
struct KlineQuery {
    symbol: String,
    #[serde(default = "default_interval")]
    interval: String,
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default = "default_fresh")]
    fresh: bool,
}

#[derive(Deserialize)]
struct BatchQuery {
    #[serde(default = "default_interval")]
    interval: String,
    #[serde(default = "default_batch_limit")]
    limit: usize,
    #[serde(default = "default_max_symbols")]
    max_symbols: usize,
    #[serde(default)]
    fresh: bool,
}

fn default_batch_limit() -> usize {
    120
}
fn default_max_symbols() -> usize {
    12
}

fn default_interval() -> String {
    "15m".to_string()
}
fn default_limit() -> usize {
    600
}
fn default_fresh() -> bool {
    true
}

fn interval_ms(interval: &str) -> Option<i64> {
    Some(match interval {
        "1m" => 60_000,
        "3m" => 180_000,
        "5m" => 300_000,
        "15m" => 900_000,
        "30m" => 1_800_000,
        "1h" => 3_600_000,
        "2h" => 7_200_000,
        "4h" => 14_400_000,
        "6h" => 21_600_000,
        "8h" => 28_800_000,
        "12h" => 43_200_000,
        "1d" => 86_400_000,
        "3d" => 259_200_000,
        "1w" => 604_800_000,
        "1M" => 2_592_000_000,
        _ => return None,
    })
}

async fn klines(
    State(db): State<Db>,
    Query(query): Query<KlineQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    require_site(&db)?;
    if !(20..=1000).contains(&query.limit) {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"detail": "limit 必须在 20 到 1000 之间"})),
        ));
    }
    let symbol = query.symbol.to_uppercase();
    let pool = enabled_symbols(&db)?;
    if pool.is_empty() {
        return Ok(
            json!({"symbol":symbol,"interval":query.interval,"items":[],"empty_pool":true}).into(),
        );
    }
    if !pool.iter().any(|s| s.eq_ignore_ascii_case(&symbol)) {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({"detail":format!("{symbol} 不在标的池中, 前台不展示池外行情")})),
        ));
    }
    let step = interval_ms(&query.interval).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"detail": "不支持的 K 线周期"})),
        )
    })?;
    let items = fetch_klines(
        market_live(&db)?,
        &symbol,
        &query.interval,
        query.limit,
        query.fresh,
    )
    .await?;
    let last = items.last().and_then(|v| v["t"].as_i64());
    let now = crate::now_millis();
    Ok(Json(json!({
        "symbol": symbol, "interval": query.interval, "items": items, "count": items.len(),
        "last_open_time": last.unwrap_or(0),
        "next_close_at": last.and_then(|t| t.checked_add(step)).unwrap_or(0),
        "server_time": now, "updated_at": now / 1000,
    })))
}

type MarketError = (StatusCode, Json<Value>);

fn enabled_symbols(db: &Db) -> Result<Vec<String>, MarketError> {
    let conn = db.conn.lock().expect("db");
    let mut stmt = conn
        .prepare("SELECT symbol FROM trading_symbols WHERE enabled = 1 ORDER BY sort_order, created_at, symbol")
        .map_err(|_| market_unavailable())?;
    let rows = stmt
        .query_map([], |r| r.get(0))
        .map_err(|_| market_unavailable())?;
    rows.collect::<Result<_, _>>()
        .map_err(|_| market_unavailable())
}

/// 当前交易环境与引擎同口径：`engine_state.mode` 优先，其次 `engine` 配置组。
pub(crate) fn market_live(db: &Db) -> Result<bool, MarketError> {
    {
        let conn = db.conn.lock().expect("db");
        if let Ok(mode) = conn.query_row(
            "SELECT value FROM engine_state WHERE key = 'mode'",
            [],
            |row| row.get::<_, String>(0),
        ) {
            if !mode.is_empty() {
                return Ok(mode == "live");
            }
        }
    }
    Ok(db
        .setting("engine")
        .map_err(|_| market_unavailable())?
        .and_then(|cfg| cfg.get("mode").map(|mode| mode == "live"))
        .unwrap_or(false))
}

fn market_url(live: bool, path: &str) -> String {
    let host = if live {
        "fapi.binance.com"
    } else {
        "testnet.binancefuture.com"
    };
    format!("https://{host}/fapi/v1/{path}")
}

async fn fetch_klines(
    live: bool,
    symbol: &str,
    interval: &str,
    limit: usize,
    fresh: bool,
) -> Result<Vec<Value>, MarketError> {
    let key = (live, symbol.to_owned(), interval.to_owned(), limit);
    let ttl = std::time::Duration::from_millis(if fresh { 1500 } else { 20_000 });
    let cached = KLINE_CACHE
        .lock()
        .expect("kline cache")
        .get(&key)
        .filter(|(at, _)| at.elapsed() < ttl)
        .map(|(_, items)| items.clone());
    if let Some(items) = cached {
        return Ok(items);
    }
    let items = {
        let response = MARKET_CLIENT
            .as_ref()
            .map_err(|_| market_unavailable())?
            .get(market_url(live, "klines"))
            .query(&[("symbol", symbol), ("interval", interval)])
            .query(&[("limit", limit)])
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|_| market_unavailable())?;
        let rows = response
            .json::<Vec<Vec<Value>>>()
            .await
            .map_err(|_| market_unavailable())?;
        let items = parse_klines(&rows).ok_or_else(market_unavailable)?;
        let mut cache = KLINE_CACHE.lock().expect("kline cache");
        cache.retain(|_, (at, _)| at.elapsed() < std::time::Duration::from_secs(20));
        if cache.len() < 64 {
            cache.insert(key, (std::time::Instant::now(), items.clone()));
        }
        items
    };
    Ok(items)
}

pub(crate) async fn fetch_tickers(
    live: bool,
    symbols: &[String],
) -> Result<serde_json::Map<String, Value>, MarketError> {
    let rows = MARKET_CLIENT
        .as_ref()
        .map_err(|_| market_unavailable())?
        .get(market_url(live, "ticker/24hr"))
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|_| market_unavailable())?
        .json::<Vec<Value>>()
        .await
        .map_err(|_| market_unavailable())?;
    let mut result = serde_json::Map::new();
    for symbol in symbols {
        let row = rows
            .iter()
            .find(|row| row["symbol"].as_str() == Some(symbol))
            .ok_or_else(market_unavailable)?;
        result.insert(
            symbol.clone(),
            parse_ticker(symbol, row).ok_or_else(market_unavailable)?,
        );
    }
    Ok(result)
}

fn parse_ticker(symbol: &str, row: &Value) -> Option<Value> {
    if row.get("symbol")?.as_str()? != symbol {
        return None;
    }
    let number = |key: &str| {
        row.get(key)?
            .as_str()?
            .parse::<f64>()
            .ok()
            .filter(|n| n.is_finite())
    };
    Some(json!({
        "last": number("lastPrice")?, "change_pct": number("priceChangePercent")?,
        "high": number("highPrice")?, "low": number("lowPrice")?,
        "quote_volume": number("quoteVolume")?, "ts": crate::now(),
    }))
}

async fn quotes(State(db): State<Db>) -> Result<Json<Value>, MarketError> {
    require_site(&db)?;
    let symbols = enabled_symbols(&db)?;
    let rows = if symbols.is_empty() {
        serde_json::Map::new()
    } else {
        fetch_tickers(market_live(&db)?, &symbols).await?
    };
    Ok(Json(
        json!({"symbols":symbols,"tickers":rows,"updated_at":crate::now()}),
    ))
}

async fn klines_batch(
    State(db): State<Db>,
    Query(query): Query<BatchQuery>,
) -> Result<Json<Value>, MarketError> {
    require_site(&db)?;
    if !(20..=300).contains(&query.limit) || !(1..=20).contains(&query.max_symbols) {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"detail":"limit 必须在 20 到 300 之间，max_symbols 必须在 1 到 20 之间"})),
        ));
    }
    interval_ms(&query.interval).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"detail":"不支持的 K 线周期"})),
        )
    })?;
    let symbols: Vec<String> = enabled_symbols(&db)?
        .into_iter()
        .take(query.max_symbols)
        .collect();
    let mut items = serde_json::Map::new();
    let mut errors = serde_json::Map::new();
    let mut tickers = serde_json::Map::new();
    if !symbols.is_empty() {
        let live = market_live(&db)?;
        for symbol in &symbols {
            match fetch_klines(live, symbol, &query.interval, query.limit, query.fresh).await {
                Ok(rows) => {
                    items.insert(symbol.clone(), Value::Array(rows));
                }
                Err((_, Json(error))) => {
                    errors.insert(symbol.clone(), error["detail"].clone());
                }
            }
        }
        tickers = fetch_tickers(live, &symbols).await.unwrap_or_default();
    }
    Ok(Json(
        json!({"symbols":symbols,"items":items,"tickers":tickers,"errors":errors,
        "interval":query.interval,"empty":symbols.is_empty(),"updated_at":crate::now()}),
    ))
}

fn parse_klines(rows: &[Vec<Value>]) -> Option<Vec<Value>> {
    rows.iter()
        .map(|row| {
            let ts = row.first()?.as_i64()?;
            let number = |i: usize| {
                row.get(i)?
                    .as_str()?
                    .parse::<f64>()
                    .ok()
                    .filter(|n| n.is_finite())
            };
            Some(
                json!({"t": ts, "o": number(1)?, "h": number(2)?, "l": number(3)?,
            "c": number(4)?, "v": number(5)?}),
            )
        })
        .collect()
}

fn market_unavailable() -> (StatusCode, Json<Value>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"detail": "币安行情暂不可用"})),
    )
}

static MARKET_CLIENT: std::sync::LazyLock<Result<reqwest::Client, reqwest::Error>> =
    std::sync::LazyLock::new(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
    });

type KlineCache =
    std::collections::HashMap<(bool, String, String, usize), (std::time::Instant, Vec<Value>)>;
static KLINE_CACHE: std::sync::LazyLock<std::sync::Mutex<KlineCache>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(KlineCache::new()));

async fn config(State(db): State<Db>) -> Json<Value> {
    let public = db.setting("public").ok().flatten().unwrap_or(Value::Null);
    let engine = db.setting("engine").ok().flatten().unwrap_or(Value::Null);
    let text = |g: &Value, k: &str, d: &str| {
        field(g, k)
            .and_then(|v| v.as_str())
            .unwrap_or(d)
            .to_string()
    };
    let flag = |g: &Value, k: &str, d: bool| field(g, k).and_then(|v| v.as_bool()).unwrap_or(d);
    json!({
        "site_enabled": flag(&public, "site_enabled", true),
        "site_title": text(&public, "site_title", "NanoQuant"),
        "site_tagline": text(&public, "site_tagline", "小资金 · 币安 U 本位永续 · 超短线量化终端"),
        "hero_note": text(&public, "hero_note", ""),
        "show_engine_status": flag(&public, "show_engine_status", true),
        "show_mantras": flag(&public, "show_mantras", true),
        "show_performance": flag(&public, "show_performance", false),
        "mode": text(&engine, "mode", "demo"),
        "version": env!("CARGO_PKG_VERSION"),
    })
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use rusqlite::Connection;
    use tower::ServiceExt;

    #[tokio::test]
    async fn config_readable_when_site_disabled_and_fills_defaults() {
        let path = std::env::temp_dir().join("nq-public.db");
        let _ = std::fs::remove_file(&path);
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE app_settings(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER); \
             INSERT INTO app_settings VALUES('public','{\"site_enabled\":false,\"site_title\":\"维护中\"}',0); \
             INSERT INTO app_settings VALUES('engine','{\"mode\":\"live\"}',0);",
        )
        .unwrap();
        drop(conn);
        let db = Db::open(path.to_str().unwrap()).unwrap();
        let app = Router::new().nest("/api/v1", routes()).with_state(db);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/public/config")
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
        assert_eq!(body["site_enabled"], false, "关闭的站点仍应能读到配置");
        assert_eq!(body["site_title"], "维护中");
        assert_eq!(body["mode"], "live");
        assert_eq!(body["show_performance"], false, "缺省字段应回退默认值");
        let _ = std::fs::remove_file(&path);
    }

    fn site(path: &std::path::Path, enabled: bool) -> Db {
        let _ = std::fs::remove_file(path);
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE app_settings(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER); \
CREATE TABLE trading_symbols(symbol TEXT, name TEXT, tier TEXT, enabled INTEGER, created_at INTEGER, sort_order INTEGER); \
             CREATE TABLE mantras(id INTEGER PRIMARY KEY, title TEXT, rule TEXT, tags TEXT, category TEXT, source TEXT, confidence REAL, health REAL, hits INTEGER, wins INTEGER, losses INTEGER, enabled INTEGER, visible INTEGER, version INTEGER, superseded_by INTEGER, evidence TEXT, created_at INTEGER, updated_at INTEGER, expires_at INTEGER); \
             INSERT INTO app_settings VALUES('public','{{\"site_enabled\":{enabled},\"show_mantras\":true}}',0); \
             INSERT INTO trading_symbols VALUES('ETHUSDT','ETH','tier_2',1,200,2); \
             INSERT INTO trading_symbols VALUES('BTCUSDT','BTC','tier_1',1,100,1); \
             INSERT INTO trading_symbols VALUES('XRPUSDT','XRP','tier_3',0,300,3); \
             INSERT INTO mantras VALUES(1,'突破追涨确认','当价格突破后追涨','[]','entry','manual',0.7,0.8,4,3,1,1,1,2,0,'{{}}',100,100,0); \
             INSERT INTO mantras VALUES(2,'内部规则','不要外泄的正文','[]','risk','manual',0.9,0.9,1,1,0,1,1,1,0,'{{}}',200,200,0);",
        ))
        .unwrap();
        drop(conn);
        Db::open(path.to_str().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn public_overview_news_and_performance_respect_visibility_and_mode() {
        let path = std::env::temp_dir().join("nq-public-overview-perf.db");
        let db = site(&path, true);
        db.conn.lock().unwrap().execute_batch("CREATE TABLE news_items(id INTEGER, ts INTEGER, source TEXT, title TEXT, url TEXT, summary TEXT, sentiment REAL, symbols TEXT);
            CREATE TABLE trades(mode TEXT, pnl REAL, hold_seconds INTEGER, closed_at INTEGER);
            CREATE TABLE signals(mode TEXT);
            INSERT INTO trades VALUES('demo', 10, 60, 1), ('demo', -4, 120, 2), ('live', 90, 999, 3);
            INSERT INTO signals VALUES('demo'), ('live');
            UPDATE app_settings SET value = '{\"show_engine_status\":false}' WHERE key = 'public';").unwrap();
        let app = routes().with_state(db.clone());
        let get = |uri: &'static str| {
            let app = app.clone();
            async move {
                let response = app
                    .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                let bytes = axum::body::to_bytes(response.into_body(), 4096)
                    .await
                    .unwrap();
                serde_json::from_slice::<Value>(&bytes).unwrap()
            }
        };
        let home = get("/public/overview").await;
        assert_eq!(home["pool"], json!(["BTCUSDT", "ETHUSDT"]));
        assert!(home.get("engine").is_none());
        assert_eq!(get("/public/news").await["sentiment"]["total_24h"], 0);
        assert_eq!(get("/public/performance").await, json!({"enabled":false}));
        db.conn.lock().unwrap().execute("UPDATE app_settings SET value = '{\"show_performance\":true}' WHERE key = 'public'", []).unwrap();
        let perf = get("/public/performance").await;
        assert_eq!(
            perf,
            json!({"enabled":true,"trades":2,"signals":1,"win_rate":50.0,
            "profit_factor":2.5,"avg_hold_seconds":90,"window":200})
        );
        drop(app);
        drop(db);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn pool_follows_insertion_order_and_skips_disabled() {
        let path = std::env::temp_dir().join("nq-public-pool.db");
        let db = site(&path, true);
        let app = Router::new().nest("/api/v1", routes()).with_state(db);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/public/pool")
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
        let symbols: Vec<&str> = body["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["symbol"].as_str().unwrap())
            .collect();
        assert_eq!(
            symbols,
            ["BTCUSDT", "ETHUSDT"],
            "按添加时间排序，停用的不展示"
        );
        assert_eq!(body["symbols"][0]["order"], 1);
        assert_eq!(body["empty"], false);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn a_closed_site_refuses_the_pool() {
        let path = std::env::temp_dir().join("nq-public-closed.db");
        let db = site(&path, false);
        let app = Router::new().nest("/api/v1", routes()).with_state(db);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/public/pool")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "站点关闭后前台数据不再提供"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn public_klines_rejects_closed_site_and_invalid_query_before_network() {
        let path = std::env::temp_dir().join("nq-public-klines-gates.db");
        let db = site(&path, false);
        let app = routes().with_state(db.clone());
        let request = Request::builder()
            .uri("/public/klines?symbol=BTCUSDT&interval=bogus")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(request).await.unwrap().status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        db.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE app_settings SET value = '{\"site_enabled\":true}' WHERE key='public'",
                [],
            )
            .unwrap();
        let request = Request::builder()
            .uri("/public/klines?symbol=BTCUSDT&interval=bogus")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.oneshot(request).await.unwrap().status(),
            StatusCode::BAD_REQUEST
        );
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn public_klines_reject_disabled_symbols_and_handle_empty_pool() {
        let path = std::env::temp_dir().join("nq-public-klines.db");
        let db = site(&path, true);
        let app = routes().with_state(db.clone());
        let request = || {
            Request::builder()
                .uri("/public/klines?symbol=xrpusdt")
                .body(Body::empty())
                .unwrap()
        };
        assert_eq!(
            app.clone().oneshot(request()).await.unwrap().status(),
            StatusCode::NOT_FOUND
        );
        db.conn
            .lock()
            .unwrap()
            .execute("UPDATE trading_symbols SET enabled = 0", [])
            .unwrap();
        let response = app.oneshot(request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            body,
            json!({"symbol":"XRPUSDT","interval":"15m","items":[],"empty_pool":true})
        );
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn batch_and_quotes_return_empty_objects_without_market_requests() {
        let path = std::env::temp_dir().join("nq-public-empty-market.db");
        let db = site(&path, true);
        db.conn
            .lock()
            .unwrap()
            .execute("UPDATE trading_symbols SET enabled = 0", [])
            .unwrap();
        let app = routes().with_state(db);
        for uri in ["/public/klines/batch", "/public/quotes"] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["symbols"], json!([]));
            assert_eq!(body["tickers"], json!({}));
            if uri.ends_with("batch") {
                assert_eq!(body["empty"], true);
                assert_eq!(body["items"], json!({}));
                assert_eq!(body["errors"], json!({}));
            }
        }
        let request = Request::builder()
            .uri("/public/klines/batch?max_symbols=21")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.oneshot(request).await.unwrap().status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn ticker_parser_requires_the_requested_symbol_and_finite_prices() {
        let mut row = json!({"symbol":"BTCUSDT","lastPrice":"123.5","priceChangePercent":"-1.2",
            "highPrice":"125","lowPrice":"120","quoteVolume":"3000"});
        let quote = parse_ticker("BTCUSDT", &row).unwrap();
        assert_eq!(quote["last"], 123.5);
        assert_eq!(quote["change_pct"], -1.2);
        assert!(parse_ticker("ETHUSDT", &row).is_none());
        row["lastPrice"] = json!("NaN");
        assert!(parse_ticker("BTCUSDT", &row).is_none());
    }

    #[test]
    fn malformed_exchange_kline_fails_the_entire_payload() {
        let good = vec![
            json!(1_700_000_000_000_i64),
            json!("1.0"),
            json!("2.0"),
            json!("0.5"),
            json!("1.5"),
            json!("10"),
        ];
        let bad = vec![
            json!("bad timestamp"),
            json!("1.0"),
            json!("2.0"),
            json!("0.5"),
            json!("1.5"),
            json!("10"),
        ];
        assert_eq!(
            parse_klines(&[good.clone()]).unwrap()[0]["t"],
            1_700_000_000_000_i64
        );
        assert!(
            parse_klines(&[good, bad]).is_none(),
            "错误行不能被静默丢弃，避免返回不完整行情"
        );
    }

    #[tokio::test]
    async fn public_mantras_hide_the_rule_text() {
        let path = std::env::temp_dir().join("nq-public-mantras.db");
        let db = site(&path, true);
        let app = Router::new().nest("/api/v1", routes()).with_state(db);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/public/mantras")
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
        let raw = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            !raw.contains("不要外泄的正文"),
            "规则正文绝不能出现在公开接口: {raw}"
        );
        assert_eq!(body["enabled"], true);
        assert_eq!(body["items"][0]["title"], "内部规则", "健康度高的排前面");
        assert_eq!(
            body["items"][0]["health"], 0.2122,
            "公开健康度使用有效健康度，而不是原始字段"
        );
        assert_eq!(body["stats"]["total"], 2);
        assert!(
            body["items"][0].get("rule").is_none(),
            "响应里不应有规则字段"
        );
        let _ = std::fs::remove_file(&path);
    }

    fn empty_public_site(path: &std::path::Path) -> Db {
        let _ = std::fs::remove_file(path);
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE app_settings(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER); \
             CREATE TABLE trading_symbols(symbol TEXT, name TEXT, tier TEXT, enabled INTEGER, created_at INTEGER, sort_order INTEGER); \
             CREATE TABLE mantras(id INTEGER PRIMARY KEY, title TEXT, rule TEXT, tags TEXT, category TEXT, source TEXT, confidence REAL, health REAL, hits INTEGER, wins INTEGER, losses INTEGER, enabled INTEGER, visible INTEGER, version INTEGER, superseded_by INTEGER, evidence TEXT, created_at INTEGER, updated_at INTEGER, expires_at INTEGER); \
             CREATE TABLE news_items(id INTEGER, ts INTEGER, source TEXT, title TEXT, url TEXT, summary TEXT, sentiment REAL, symbols TEXT); \
             CREATE TABLE trades(mode TEXT, pnl REAL, hold_seconds INTEGER, closed_at INTEGER); \
             CREATE TABLE signals(mode TEXT); \
             CREATE TABLE engine_state(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER); \
             INSERT INTO app_settings VALUES('public','{\"site_enabled\":true,\"show_mantras\":true,\"show_performance\":false}',0); \
             INSERT INTO app_settings VALUES('engine','{\"mode\":\"demo\"}',0); \
             INSERT INTO engine_state VALUES('mode','demo',0);",
        )
        .unwrap();
        drop(conn);
        Db::open(path.to_str().unwrap()).unwrap()
    }

    fn collect_keys(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                for (key, inner) in map {
                    out.push(key.clone());
                    collect_keys(inner, out);
                }
            }
            Value::Array(items) => {
                for item in items {
                    collect_keys(item, out);
                }
            }
            _ => {}
        }
    }

    #[tokio::test]
    async fn public_payloads_never_expose_account_secrets_or_rules() {
        let path = std::env::temp_dir().join("nq-public-sensitive.db");
        let db = empty_public_site(&path);
        let app = Router::new().nest("/api/v1", routes()).with_state(db);
        let uris = [
            "/api/v1/public/config",
            "/api/v1/public/overview",
            "/api/v1/public/news",
            "/api/v1/public/performance",
            "/api/v1/public/pool",
            "/api/v1/public/mantras",
            "/api/v1/public/klines?symbol=BTCUSDT",
            "/api/v1/public/klines/batch",
            "/api/v1/public/quotes",
        ];
        let forbidden = [
            "api_key",
            "apikey",
            "secret",
            "password",
            "token",
            "balance",
            "balances",
            "position",
            "positions",
            "orders",
            "rule",
            "rule_text",
            "ingest_token",
            "master_key",
            "credential",
            "credentials",
        ];
        for uri in uris {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{uri} 应可匿名读取");
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            let mut keys = Vec::new();
            collect_keys(&body, &mut keys);
            for key in &keys {
                assert!(
                    !forbidden.contains(&key.as_str()),
                    "{uri} 泄露敏感字段 {key}: {body}"
                );
            }
        }
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn klines_batch_is_structurally_limited_to_the_enabled_pool() {
        let path = std::env::temp_dir().join("nq-public-batch-pool.db");
        let db = empty_public_site(&path);
        let app = Router::new().nest("/api/v1", routes()).with_state(db);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/public/klines/batch?symbol=DOGEUSDT")
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
        assert_eq!(body["empty"], true, "池为空时不返回任何池外行情");
        assert!(
            body["symbols"].as_array().unwrap().is_empty(),
            "batch 只按池内顺序取标的，不接受池外 symbol 参数"
        );
        assert!(body["items"].as_object().unwrap().is_empty());
        let _ = std::fs::remove_file(&path);
    }
}
