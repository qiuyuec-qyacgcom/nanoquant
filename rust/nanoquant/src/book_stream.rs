//! 币安 `bookTicker` WebSocket 本地盘口：一条合并流订阅全部启用标的，
//! 内存维护买一/卖一，供 `providers::snapshot()` 优先读取，REST 只在过期时兜底。
//!
//! 为什么不轮询 REST：`/fapi/v1/ticker/bookTicker` 每标的每次权重不低，
//! 且 480 秒周期内盘口早已漂移；WebSocket 推送无权重、延迟更低。
//!
//! 设计：
//! - 合并流 `wss://fstream.binance.com/stream?streams=btcusdt@bookTicker/...`；
//! - demo 走测试网域名，live 走主网；
//! - 断线指数退避（1s 起翻倍，封顶 15s），重连后整批重新订阅；
//! - 每 10 秒检查开关、环境与启用标的，变化就重连；
//! - 启动/重连前先 REST 预热一次，避免首条推送到达前 snapshot 没有本地盘口。
//!
//! 测试不碰真实网络：消息解析与新鲜度判定都是纯函数，直接喂 JSON 断言。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{OnceLock, RwLock};
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::Value;
use tokio::sync::watch;

use crate::db::Db;
use crate::providers::BinanceProvider;

/// 币安 U 本位永续合并流地址（模拟盘=测试网，实盘=主网）。
pub const STREAM_HOSTS: &[(&str, &str)] = &[
    ("demo", "wss://stream.binancefuture.com/stream"),
    ("live", "wss://fstream.binance.com/stream"),
];

/// 退避上限，与 K 线流一致。
const MAX_BACKOFF: Duration = Duration::from_secs(15);
/// 运行中检查开关/环境/标的的间隔。
const SETTING_CHECK: Duration = Duration::from_secs(10);
/// 空闲（关闭或没有标的）时的轮询间隔。
const IDLE_CHECK: Duration = Duration::from_secs(5);

/// 本地盘口快照。`ts` 取交易所事件时间，单位为毫秒。
#[derive(Debug, Clone, PartialEq)]
pub struct BookState {
    pub bid: f64,
    pub ask: f64,
    // 盘口量暂未进入 MarketSnapshot，保留给后续执行层使用。
    pub bid_qty: f64,
    pub ask_qty: f64,
    pub ts: i64,
}

/// 解析后的一条盘口推送。
#[derive(Debug, Clone, PartialEq)]
pub struct BookTicker {
    pub symbol: String,
    pub state: BookState,
}

fn book_map() -> &'static RwLock<HashMap<String, BookState>> {
    static BOOKS: OnceLock<RwLock<HashMap<String, BookState>>> = OnceLock::new();
    BOOKS.get_or_init(|| RwLock::new(HashMap::new()))
}

pub fn store(ticker: &BookTicker) {
    if let Ok(mut books) = book_map().write() {
        books.insert(ticker.symbol.clone(), ticker.state.clone());
    }
}

/// 本地盘口读取结果，用于统计 stale / 回退 REST 的比例。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteSource {
    /// 本地盘口新鲜，直接采用。
    Local,
    /// 本地盘口存在但过期或非法，回退 REST。
    Stale,
    /// 未启用本地盘口，直接走 REST。
    Rest,
}

/// 盘口读取累计计数；进程内单调递增，重启归零。
#[derive(Debug, Default)]
pub struct BookCounters {
    pub queries: AtomicU64,
    pub stale: AtomicU64,
    pub fallback_rest: AtomicU64,
}

impl BookCounters {
    pub fn record(&self, source: QuoteSource) {
        self.queries.fetch_add(1, Ordering::Relaxed);
        match source {
            QuoteSource::Local => {}
            QuoteSource::Stale => {
                self.stale.fetch_add(1, Ordering::Relaxed);
                self.fallback_rest.fetch_add(1, Ordering::Relaxed);
            }
            QuoteSource::Rest => {
                self.fallback_rest.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    pub fn snapshot(&self) -> Value {
        let queries = self.queries.load(Ordering::Relaxed);
        let stale = self.stale.load(Ordering::Relaxed);
        let stale_pct = if queries == 0 {
            0.0
        } else {
            (stale as f64 / queries as f64 * 10_000.0).round() / 100.0
        };
        serde_json::json!({
            "queries": queries,
            "stale": stale,
            "fallback_rest": self.fallback_rest.load(Ordering::Relaxed),
            "stale_pct": stale_pct,
        })
    }
}

/// 全局计数器的快照，供 `/engine/stats` 读取。
pub fn stats() -> Value {
    counters().snapshot()
}

/// 进程内共享的盘口健康度计数器。
///
/// 必须是同一个静态实例：之前 `stats()` 与 `resolve_for_symbol()` 各持一份，
/// 导致面板永远读到 0。
fn counters() -> &'static BookCounters {
    static COUNTERS: OnceLock<BookCounters> = OnceLock::new();
    COUNTERS.get_or_init(BookCounters::default)
}

pub fn record_source(source: QuoteSource) {
    counters().record(source);
}

pub fn quote(symbol: &str) -> Option<BookState> {
    let books = book_map().read().ok()?;
    books.get(&symbol.to_uppercase()).cloned()
}

/// 新鲜且合法的本地买一/卖一。
///
/// 返回 `Some((bid, ask, spread_bps))` 时调用方不必再打 REST `bookTicker`；
/// 命中会按 `Local` 计入盘口健康度。
pub fn fresh_quote(
    enabled: bool,
    stale_ms: i64,
    symbol: &str,
    now_ms: i64,
) -> Option<(f64, f64, f64)> {
    if !enabled {
        return None;
    }
    let book = quote(symbol)?;
    let age = now_ms.saturating_sub(book.ts);
    let fresh = stale_ms > 0 && book.ts > 0 && age <= stale_ms;
    let valid =
        book.bid.is_finite() && book.ask.is_finite() && book.bid > 0.0 && book.ask >= book.bid;
    if !(fresh && valid) {
        return None;
    }
    record_source(QuoteSource::Local);
    Some((book.bid, book.ask, spread_bps(book.bid, book.ask)))
}

/// 交易环境归一化：只有 live 走主网，其余一律当模拟盘。
pub fn normalize_env(mode: &str) -> &'static str {
    if mode == "live" {
        "live"
    } else {
        "demo"
    }
}

pub fn stream_url(env: &str) -> &'static str {
    STREAM_HOSTS
        .iter()
        .find(|(name, _)| *name == env)
        .map(|(_, url)| *url)
        .unwrap_or(STREAM_HOSTS[0].1)
}

pub fn stream_name(symbol: &str) -> String {
    format!("{}@bookTicker", symbol.to_lowercase())
}

/// 合并流地址：`/stream?streams=btcusdt@bookTicker/ethusdt@bookTicker`。
pub fn combined_url(env: &str, symbols: &[String]) -> String {
    let streams: Vec<String> = symbols.iter().map(|symbol| stream_name(symbol)).collect();
    format!("{}?streams={}", stream_url(env), streams.join("/"))
}

fn next_backoff(current: Duration) -> Duration {
    (current * 2).min(MAX_BACKOFF)
}

/// 解析币安 `bookTicker` 推送，兼容原始流与组合流（包在 `data` 里）。
pub fn parse_book_ticker(raw: &str) -> Option<BookTicker> {
    let message: Value = serde_json::from_str(raw).ok()?;
    let data = match message.get("data") {
        Some(inner) if inner.is_object() => inner,
        _ => &message,
    };
    let symbol = data.get("s")?.as_str()?.to_uppercase();
    let number = |key: &str| {
        data.get(key).and_then(|value| {
            value
                .as_f64()
                .or_else(|| value.as_str().and_then(|text| text.parse::<f64>().ok()))
        })
    };
    let bid = number("b")?;
    let ask = number("a")?;
    if !bid.is_finite() || !ask.is_finite() || bid <= 0.0 || ask <= 0.0 || ask < bid {
        return None;
    }
    let bid_qty = number("B").unwrap_or(0.0);
    let ask_qty = number("A").unwrap_or(0.0);
    let ts = data
        .get("E")
        .and_then(Value::as_i64)
        .filter(|time| *time > 0)
        .unwrap_or_else(crate::now_millis);
    Some(BookTicker {
        symbol,
        state: BookState {
            bid,
            ask,
            bid_qty,
            ask_qty,
            ts,
        },
    })
}

/// 本地盘口在 `stale_ms` 内且价格合法时优先，否则回退 REST。
/// 返回 `(bid, ask, spread_bps)`。生产路径走 `resolve_quote_source`，此函数仅供测试与旧调用。
#[cfg(test)]
pub fn resolve_quote(
    enabled: bool,
    book: Option<&BookState>,
    rest_bid: f64,
    rest_ask: f64,
    now_ms: i64,
    stale_ms: i64,
) -> (f64, f64, f64) {
    resolve_quote_source(enabled, book, rest_bid, rest_ask, now_ms, stale_ms).0
}

/// 同 `resolve_quote`，但额外返回本次读数来源，便于盘口健康度计数。
pub fn resolve_quote_source(
    enabled: bool,
    book: Option<&BookState>,
    rest_bid: f64,
    rest_ask: f64,
    now_ms: i64,
    stale_ms: i64,
) -> ((f64, f64, f64), QuoteSource) {
    let usable = book.filter(|_| enabled).filter(|book| {
        let age = now_ms.saturating_sub(book.ts);
        let fresh = stale_ms > 0 && book.ts > 0 && age <= stale_ms;
        let valid =
            book.bid.is_finite() && book.ask.is_finite() && book.bid > 0.0 && book.ask >= book.bid;
        fresh && valid
    });
    match usable {
        Some(book) => (
            (book.bid, book.ask, spread_bps(book.bid, book.ask)),
            QuoteSource::Local,
        ),
        None => {
            // 本地盘口存在却没用上，说明它过期/非法，记一次 stale。
            let source = if book.is_some() && enabled {
                QuoteSource::Stale
            } else {
                QuoteSource::Rest
            };
            ((rest_bid, rest_ask, spread_bps(rest_bid, rest_ask)), source)
        }
    }
}

/// `snapshot()` 用入口：读取全局盘口后判定新鲜度，并累加盘口健康度计数。
pub fn resolve_for_symbol(
    enabled: bool,
    stale_ms: i64,
    symbol: &str,
    rest_bid: f64,
    rest_ask: f64,
    now_ms: i64,
) -> (f64, f64, f64) {
    let book = quote(symbol);
    let (quote, source) =
        resolve_quote_source(enabled, book.as_ref(), rest_bid, rest_ask, now_ms, stale_ms);
    record_source(source);
    quote
}

fn spread_bps(bid: f64, ask: f64) -> f64 {
    let mid = (bid + ask) / 2.0;
    if mid > 0.0 {
        ((ask - bid) / mid * 10_000.0).max(0.0)
    } else {
        0.0
    }
}

/// 启动时是否值得拉起后台任务：开关打开且交易环境为 live/demo。
pub fn startup_enabled(db: &Db) -> bool {
    let data = read_data(db);
    let enabled = data
        .get("book_ws_enabled")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    enabled && matches!(current_mode(db).as_str(), "live" | "demo")
}

/// 后台任务：连接合并流、落盘口、断线退避重连。
pub async fn run_task(db: Db, mut stop: watch::Receiver<bool>) {
    let mut backoff = Duration::from_secs(1);
    loop {
        if *stop.borrow() {
            break;
        }
        let data = read_data(&db);
        let enabled = data
            .get("book_ws_enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let mode = current_mode(&db);
        let symbols = enabled_symbols(&db);
        if !enabled || !matches!(mode.as_str(), "live" | "demo") || symbols.is_empty() {
            if wait_backoff(&mut stop, IDLE_CHECK).await {
                break;
            }
            continue;
        }
        prewarm(&db, &mode, &symbols).await;
        let env = normalize_env(&mode);
        let url = combined_url(env, &symbols);
        let connected = tokio::select! {
            biased;
            _ = stop.changed() => return,
            result = tokio_tungstenite::connect_async(url.clone()) => result,
        };
        let (mut ws, _response) = match connected {
            Ok(pair) => pair,
            Err(error) => {
                tracing::warn!(%error, url, "盘口流连接失败");
                if wait_backoff(&mut stop, backoff).await {
                    break;
                }
                backoff = next_backoff(backoff);
                continue;
            }
        };
        backoff = Duration::from_secs(1);
        let wanted: HashSet<String> = symbols.iter().map(|symbol| symbol.to_uppercase()).collect();
        tracing::info!(url, symbols = wanted.len(), "盘口流已连接");
        let mut check = tokio::time::interval(SETTING_CHECK);
        check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = stop.changed() => return,
                message = ws.next() => match message {
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                        if let Some(ticker) = parse_book_ticker(&text) {
                            store(&ticker);
                        }
                    }
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Binary(data))) => {
                        if let Ok(text) = String::from_utf8(data.to_vec()) {
                            if let Some(ticker) = parse_book_ticker(&text) {
                                store(&ticker);
                            }
                        }
                    }
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(error)) => {
                        tracing::warn!(%error, "盘口流断开");
                        break;
                    }
                },
                _ = check.tick() => {
                    let next = read_data(&db);
                    let enabled = next
                        .get("book_ws_enabled")
                        .and_then(Value::as_bool)
                        .unwrap_or(true);
                    let next_mode = current_mode(&db);
                    let next_symbols: HashSet<String> = enabled_symbols(&db)
                        .iter()
                        .map(|symbol| symbol.to_uppercase())
                        .collect();
                    if !enabled || normalize_env(&next_mode) != env || next_symbols != wanted {
                        break;
                    }
                }
            }
        }
        if wait_backoff(&mut stop, backoff).await {
            break;
        }
        backoff = next_backoff(backoff);
    }
    tracing::info!("盘口流任务已停止");
}

async fn prewarm(db: &Db, mode: &str, symbols: &[String]) {
    let data = read_data(db);
    let base = data
        .get(match normalize_env(mode) {
            "live" => "live_base_url",
            _ => "demo_base_url",
        })
        .and_then(Value::as_str)
        .unwrap_or(match normalize_env(mode) {
            "live" => "https://fapi.binance.com",
            _ => "https://testnet.binancefuture.com",
        });
    let Ok(provider) = BinanceProvider::public(base) else {
        return;
    };
    let Ok(tops) = provider.book_tops().await else {
        tracing::warn!("盘口 REST 预热失败，等待 WebSocket 首条推送");
        return;
    };
    let wanted: HashSet<&str> = symbols.iter().map(|symbol| symbol.as_str()).collect();
    let mut stored = 0;
    for top in tops {
        if !wanted.contains(top.symbol.as_str()) {
            continue;
        }
        stored += 1;
        store(&BookTicker {
            symbol: top.symbol,
            state: BookState {
                bid: top.bid,
                ask: top.ask,
                bid_qty: 0.0,
                ask_qty: 0.0,
                ts: crate::now_millis(),
            },
        });
    }
    tracing::info!(stored, "盘口 REST 预热完成");
}

fn read_data(db: &Db) -> Value {
    db.setting("data").ok().flatten().unwrap_or(Value::Null)
}

fn enabled_symbols(db: &Db) -> Vec<String> {
    let Ok(conn) = db.conn.lock() else {
        return Vec::new();
    };
    let Ok(mut stmt) = conn.prepare(
        "SELECT symbol FROM trading_symbols WHERE enabled != 0 ORDER BY sort_order, symbol",
    ) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(0)) else {
        return Vec::new();
    };
    rows.filter_map(Result::ok).collect()
}

fn current_mode(db: &Db) -> String {
    let stored = db
        .conn
        .lock()
        .ok()
        .and_then(|conn| {
            conn.query_row(
                "SELECT value FROM engine_state WHERE key='mode'",
                [],
                |row| row.get::<_, String>(0),
            )
            .ok()
        })
        .filter(|mode| !mode.is_empty());
    if let Some(mode) = stored {
        return mode;
    }
    db.setting("engine")
        .ok()
        .flatten()
        .and_then(|value| value.get("mode").and_then(Value::as_str).map(String::from))
        .unwrap_or_else(|| "demo".to_string())
}

async fn wait_backoff(stop: &mut watch::Receiver<bool>, backoff: Duration) -> bool {
    tokio::select! {
        _ = stop.changed() => true,
        _ = tokio::time::sleep(backoff) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn book(ts: i64) -> BookState {
        BookState {
            bid: 99.5,
            ask: 100.5,
            bid_qty: 2.0,
            ask_qty: 3.0,
            ts,
        }
    }

    #[test]
    fn book_state_fresh_uses_local_quote() {
        let state = book(1_000);
        let (bid, ask, spread) = resolve_quote(true, Some(&state), 90.0, 110.0, 2_500, 3_000);
        assert_eq!(bid, 99.5, "新鲜本地盘口应覆盖 REST 买一");
        assert_eq!(ask, 100.5, "新鲜本地盘口应覆盖 REST 卖一");
        assert!(
            (spread - 100.0).abs() < 1e-6,
            "spread 应按本地盘口重算: {spread}"
        );
    }

    #[test]
    fn book_state_stale_falls_back_to_rest() {
        let state = book(1_000);
        let (bid, ask, spread) = resolve_quote(true, Some(&state), 90.0, 110.0, 10_000, 3_000);
        assert_eq!(bid, 90.0);
        assert_eq!(ask, 110.0);
        assert!(
            (spread - 2_000.0).abs() < 1e-6,
            "回退 REST 后按 REST 重算: {spread}"
        );
    }

    #[test]
    fn book_state_disabled_uses_rest() {
        let state = book(1_000);
        let (bid, ask, _) = resolve_quote(false, Some(&state), 90.0, 110.0, 1_100, 3_000);
        assert_eq!(bid, 90.0, "开关关闭时禁止读本地盘口");
        assert_eq!(ask, 110.0);
    }

    #[test]
    fn book_counters_track_stale_and_fallback() {
        let counters = BookCounters::default();
        let state = book(1_000);
        let (_, fresh) = resolve_quote_source(true, Some(&state), 90.0, 110.0, 2_500, 3_000);
        counters.record(fresh);
        let (_, stale) = resolve_quote_source(true, Some(&state), 90.0, 110.0, 10_000, 3_000);
        counters.record(stale);
        let (_, rest) = resolve_quote_source(false, Some(&state), 90.0, 110.0, 1_100, 3_000);
        counters.record(rest);

        let snapshot = counters.snapshot();
        assert_eq!(snapshot["queries"], 3, "每次读数都要计一次查询");
        assert_eq!(snapshot["stale"], 1, "仅过期盘口算 stale");
        assert_eq!(snapshot["fallback_rest"], 2, "过期与未启用都回退 REST");
        assert!(
            (snapshot["stale_pct"].as_f64().unwrap() - 33.33).abs() < 1e-6,
            "stale 占比 = stale / queries: {snapshot}"
        );
        assert_eq!(
            BookCounters::default().snapshot()["queries"],
            0,
            "无数据时计数保持 0"
        );
    }

    #[test]
    fn fresh_quote_skips_rest_only_within_the_stale_window() {
        store(&BookTicker {
            symbol: "NQFRESHUSDT".into(),
            state: book(1_000),
        });

        assert!(
            fresh_quote(false, 3_000, "NQFRESHUSDT", 1_500).is_none(),
            "开关关闭时不得用本地盘口"
        );
        let (bid, ask, _) =
            fresh_quote(true, 3_000, "NQFRESHUSDT", 1_500).expect("窗口内的本地盘口应直接命中");
        assert_eq!((bid, ask), (99.5, 100.5));
        assert!(
            fresh_quote(true, 3_000, "NQFRESHUSDT", 10_000).is_none(),
            "过期盘口必须回退 REST"
        );
    }

    #[test]
    fn stale_stats_reads_the_same_counters_as_resolution() {
        let before = stats()["queries"].as_u64().unwrap_or(0);
        record_source(QuoteSource::Rest);
        assert_eq!(
            stats()["queries"].as_u64().unwrap_or(0),
            before + 1,
            "stats 与 resolve 必须共享同一份计数器"
        );
    }

    #[test]
    fn book_ticker_message_parses() {
        let raw = json!({
            "e": "bookTicker",
            "u": 4_000_000_000_i64,
            "s": "btcusdt",
            "b": "100.5",
            "B": "2.5",
            "a": "100.7",
            "A": "3.5",
            "T": 1_700_000_000_000_i64,
            "E": 1_700_000_000_123_i64
        })
        .to_string();
        let ticker = parse_book_ticker(&raw).expect("应解析出盘口");
        assert_eq!(ticker.symbol, "BTCUSDT");
        assert_eq!(ticker.state.bid, 100.5);
        assert_eq!(ticker.state.ask, 100.7);
        assert_eq!(ticker.state.bid_qty, 2.5);
        assert_eq!(ticker.state.ask_qty, 3.5);
        assert_eq!(ticker.state.ts, 1_700_000_000_123);
    }

    #[test]
    fn combined_stream_and_malformed_messages_are_handled() {
        let wrapped = json!({
            "stream": "btcusdt@bookTicker",
            "data": {
                "e": "bookTicker",
                "s": "BTCUSDT",
                "b": "1",
                "B": "1",
                "a": "2",
                "A": "1",
                "E": 1_700_000_000_000_i64
            }
        })
        .to_string();
        assert!(parse_book_ticker(&wrapped).is_some(), "组合流也要认");
        assert!(parse_book_ticker("not json").is_none());
        assert!(
            parse_book_ticker(r#"{"s":"BTCUSDT","b":"2","a":"1"}"#).is_none(),
            "卖一低于买一必须丢弃"
        );
        assert!(parse_book_ticker(r#"{"s":"BTCUSDT","b":"0","a":"1"}"#).is_none());
    }

    #[test]
    fn combined_url_follows_environment() {
        let symbols = vec!["BTCUSDT".to_string(), "ETHUSDT".to_string()];
        assert_eq!(
            combined_url("live", &symbols),
            "wss://fstream.binance.com/stream?streams=btcusdt@bookTicker/ethusdt@bookTicker"
        );
        assert_eq!(
            combined_url("demo", &symbols),
            "wss://stream.binancefuture.com/stream?streams=btcusdt@bookTicker/ethusdt@bookTicker"
        );
    }
}
