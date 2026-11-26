//! 资讯：列表、刷新、源诊断与 AI 摘要。列表对齐 `news/service.py` 的 `list_items`。

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::future::join_all;
#[cfg(test)]
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::ai_client::{self, ChatCall, ChatMessage};
use crate::ai_providers::{provider_chain, AiState};
use crate::db::Db;

const SOURCE_LABELS: &[(&str, &str)] = &[
    ("binance_square", "币安广场"),
    ("binance_announcement", "币安公告"),
    ("binance_announcement_rsshub", "币安公告(RSSHub)"),
    ("coindesk", "CoinDesk"),
    ("cointelegraph", "Cointelegraph"),
    ("theblock", "The Block"),
    ("decrypt", "Decrypt"),
    ("bitcoinmagazine", "Bitcoin Magazine"),
    ("newsbtc", "NewsBTC"),
    ("beincrypto", "BeInCrypto"),
    ("utoday", "U.Today"),
    ("demo", "离线示例"),
];

const DEFAULT_SOURCES: &[&str] = &[
    "binance_square",
    "binance_announcement",
    "cointelegraph",
    "coindesk",
    "theblock",
    "decrypt",
    "bitcoinmagazine",
    "newsbtc",
    "beincrypto",
    "utoday",
];

fn source_label(source: &str) -> String {
    SOURCE_LABELS
        .iter()
        .find(|(k, _)| *k == source)
        .map(|(_, v)| v.to_string())
        .unwrap_or_else(|| source.to_string())
}

/// 最近一次刷新的进程内状态：面板用它区分「库内旧闻」与「本轮抓取」。
#[derive(Clone, Default)]
struct LastRefresh {
    ts: i64,
    status: Map<String, Value>,
    inserted: i64,
    failed: Vec<String>,
    stale: bool,
}

/// 资讯抓取依赖，测试注入假源，生产默认走 HTTP。
#[derive(Clone)]
pub struct NewsState {
    pub db: Db,
    pub ai: Option<AiState>,
    sources: Arc<Vec<Arc<dyn NewsSource>>>,
    last: Arc<Mutex<LastRefresh>>,
    lock: Arc<tokio::sync::Mutex<()>>,
}

impl NewsState {
    pub fn new(db: Db) -> Self {
        let rsshub_base = news_cfg(&db).rsshub_base;
        Self::with_sources(db, http_sources(&rsshub_base))
    }

    pub fn with_sources(db: Db, sources: Vec<Arc<dyn NewsSource>>) -> Self {
        Self {
            db,
            ai: None,
            sources: Arc::new(sources),
            last: Arc::new(Mutex::new(LastRefresh::default())),
            lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    pub fn with_ai(mut self, ai: AiState) -> Self {
        self.ai = Some(ai);
        self
    }

    fn last_snapshot(&self) -> LastRefresh {
        self.last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

#[derive(Debug, Clone)]
pub struct FetchError {
    pub message: String,
    pub url: String,
}

/// 一条资讯源。`fetch` 返回已解析的条目；失败必须保留可读原因。
pub trait NewsSource: Send + Sync {
    fn name(&self) -> &'static str;
    fn fetch<'a>(&'a self, limit: usize) -> BoxFuture<'a, Result<Vec<Incoming>, FetchError>>;
}

pub fn routes() -> Router<NewsState> {
    Router::new()
        .route("/news", get(list_news))
        .route("/news/overview", get(overview))
        .route("/news/refresh", post(refresh_handler))
        .route("/news/diagnostics", get(diagnostics_handler))
        .route("/news/ai-summary", post(ai_summary_handler))
}

#[derive(Deserialize)]
struct NewsQuery {
    #[serde(default = "default_limit")]
    limit: i64,
    #[serde(default)]
    source: String,
    #[serde(default)]
    symbol: String,
}

fn default_limit() -> i64 {
    60
}

async fn list_news(State(state): State<NewsState>, Query(q): Query<NewsQuery>) -> Json<Value> {
    Json(json!({
        "items": items(&state.db, q.limit.clamp(1, 300), &q.source, &q.symbol),
        "sources": source_catalog(&state),
    }))
}

pub(crate) fn items(db: &Db, limit: i64, source: &str, symbol: &str) -> Vec<Value> {
    let symbol = symbol.to_uppercase();
    let conn = db.conn.lock().expect("db");
    let fetch = (limit * 4).min(800);
    let (sql, source_filter) = if source.is_empty() {
        ("SELECT id, ts, source, title, url, summary, sentiment, symbols FROM news_items ORDER BY ts DESC LIMIT ?1".to_string(), None)
    } else {
        ("SELECT id, ts, source, title, url, summary, sentiment, symbols FROM news_items WHERE source = ?2 ORDER BY ts DESC LIMIT ?1".to_string(), Some(source.to_string()))
    };
    let mut stmt = conn.prepare(&sql).expect("prepare news");
    let map = |r: &rusqlite::Row<'_>| -> rusqlite::Result<Value> {
        let symbols_raw: String = r
            .get::<_, Option<String>>(7)?
            .unwrap_or_else(|| "[]".into());
        let symbols: Value = serde_json::from_str(&symbols_raw).unwrap_or(json!([]));
        Ok(json!({
            "id": r.get::<_, i64>(0)?,
            "ts": r.get::<_, i64>(1)?,
            "source": r.get::<_, String>(2)?,
            "title": r.get::<_, String>(3)?,
            "url": r.get::<_, String>(4)?,
            "summary": r.get::<_, String>(5)?,
            "sentiment": (r.get::<_, f64>(6)? * 1000.0).round() / 1000.0,
            "symbols": symbols,
        }))
    };
    let rows: Vec<Value> = match &source_filter {
        None => stmt
            .query_map([fetch], map)
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows"),
        Some(src) => stmt
            .query_map(rusqlite::params![fetch, src], map)
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows"),
    };
    let items: Vec<Value> = rows
        .into_iter()
        .filter(|row| {
            symbol.is_empty()
                || row["symbols"]
                    .as_array()
                    .map(|a| a.iter().any(|s| s.as_str() == Some(&symbol)))
                    .unwrap_or(false)
        })
        .take(limit as usize)
        .map(|mut row| {
            let label = source_label(row["source"].as_str().unwrap_or(""));
            row["source_label"] = json!(label);
            row
        })
        .collect();
    items
}

async fn overview(State(state): State<NewsState>) -> Json<Value> {
    let last = state.last_snapshot();
    let mut value = sentiment_overview(&state.db);
    value["last_refresh"] = json!(last.ts);
    value["source_status"] = Value::Object(last.status);
    value["failed"] = json!(last.failed);
    value["stale"] = json!(last.stale);
    Json(value)
}

/// 近 24 小时的情绪分布。阈值读 `news.sentiment_threshold`，缺省 0.35，
/// 超过 ±阈值才算显著多空，否则中性。
pub(crate) fn sentiment_overview(db: &Db) -> Value {
    let conn = db.conn.lock().expect("db");
    let raw = conn
        .query_row(
            "SELECT value FROM app_settings WHERE key = 'news'",
            [],
            |r| r.get::<_, String>(0),
        )
        .unwrap_or_default();
    let threshold = serde_json::from_str::<Value>(&raw)
        .ok()
        .and_then(|v| v.get("sentiment_threshold").and_then(|t| t.as_f64()))
        .unwrap_or(0.35)
        .clamp(0.0, 1.0);
    let since = crate::now() - 86_400;
    let mut stmt = conn
        .prepare("SELECT source, sentiment FROM news_items WHERE ts >= ?1")
        .expect("prepare overview");
    let rows: Vec<(String, f64)> = stmt
        .query_map([since], |r| Ok((r.get(0)?, r.get(1)?)))
        .expect("query overview")
        .collect::<Result<_, _>>()
        .expect("overview rows");
    let positive = rows.iter().filter(|(_, s)| *s > threshold).count();
    let negative = rows.iter().filter(|(_, s)| *s < -threshold).count();
    let avg = if rows.is_empty() {
        0.0
    } else {
        rows.iter().map(|(_, s)| s).sum::<f64>() / rows.len() as f64
    };
    let mut by_source = serde_json::Map::new();
    for (source, _) in &rows {
        let n = by_source.get(source).and_then(|v| v.as_i64()).unwrap_or(0) + 1;
        by_source.insert(source.clone(), json!(n));
    }
    json!({
        "total_24h": rows.len(),
        "positive": positive,
        "negative": negative,
        "neutral": rows.len() - positive - negative,
        "sentiment_index": (avg * 10_000.0).round() / 10_000.0,
        "sentiment_threshold": (threshold * 1000.0).round() / 1000.0,
        "by_source": by_source,
    })
}

/// 负面事件关键词。命中任一即视为与标的相关的利空。
const NEGATIVE_EVENT_KEYWORDS: &[&str] = &[
    "下架", "暂停", "黑客", "清算", "监管", "诉讼", "退市", "delist", "suspend", "hack", "liquidat",
];

/// 事件否决参数：只关心开关、回看窗口与最多返回几条命中。
#[derive(Debug, Clone, Copy)]
pub(crate) struct NewsVetoCfg {
    pub enabled: bool,
    pub window_min: i64,
    pub max_items: i64,
}

pub(crate) fn veto_cfg(news: &Value) -> NewsVetoCfg {
    NewsVetoCfg {
        enabled: news
            .get("veto_enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        window_min: news
            .get("veto_window_min")
            .and_then(Value::as_i64)
            .unwrap_or(30)
            .clamp(5, 240),
        max_items: news
            .get("veto_max_items")
            .and_then(Value::as_i64)
            .unwrap_or(3)
            .clamp(1, 10),
    }
}

/// 查询最近 `window_min` 分钟内与 `symbol` 相关的负面事件。
///
/// 匹配规则：`symbols` 字段包含该标的，或标题/摘要包含去掉 `USDT` 后的币种词；
/// 命中关键词后按时间倒序取最多 `max_items` 条，返回「标题 摘要」的拼接文本。
pub(crate) fn negative_event_hit(
    conn: &rusqlite::Connection,
    symbol: &str,
    window_min: i64,
    max_items: i64,
) -> Option<String> {
    let upper = symbol.trim().to_uppercase();
    let base = upper.strip_suffix("USDT").unwrap_or(upper.as_str());
    if base.is_empty() {
        return None;
    }
    let since = crate::now() - window_min.clamp(1, 10_080) * 60;
    let limit = max_items.clamp(1, 10) as usize;
    let mut stmt = conn
        .prepare(
            "SELECT title, summary, symbols FROM news_items WHERE ts >= ?1 ORDER BY ts DESC \
             LIMIT 500",
        )
        .ok()?;
    let rows = stmt
        .query_map([since], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                row.get::<_, Option<String>>(2)?.unwrap_or_default(),
            ))
        })
        .ok()?;
    let mut hits = Vec::with_capacity(limit);
    for (title, summary, symbols_raw) in rows.flatten() {
        let related = news_row_matches(&upper, base, &symbols_raw, &title, &summary);
        if !related {
            continue;
        }
        let text = format!("{title} {summary}").to_lowercase();
        if !NEGATIVE_EVENT_KEYWORDS
            .iter()
            .any(|keyword| text.contains(keyword))
        {
            continue;
        }
        let title = title.trim();
        let summary = summary.trim();
        hits.push(if summary.is_empty() {
            title.to_string()
        } else {
            format!("{title} {summary}")
        });
        if hits.len() >= limit {
            break;
        }
    }
    (!hits.is_empty()).then(|| hits.join("；"))
}

/// 一条资讯是否与标的相关：`symbols` 里有完整标的，或文本里出现币种词。
fn news_row_matches(
    symbol: &str,
    base: &str,
    symbols_raw: &str,
    title: &str,
    summary: &str,
) -> bool {
    let symbols: Value = serde_json::from_str(symbols_raw).unwrap_or(json!([]));
    if symbols.as_array().is_some_and(|items| {
        items.iter().any(|item| {
            item.as_str()
                .is_some_and(|value| value.eq_ignore_ascii_case(symbol))
        })
    }) {
        return true;
    }
    format!("{title} {summary}").to_uppercase().contains(base)
}

/// 事件否决理由：只在候选方向存在时判断。
///
/// 负面事件方向向下，做多与事件冲击同向、最容易吃到急跌，因此否决多头；
/// 做空属于反向，不受影响。
pub(crate) fn veto_reason(
    conn: &rusqlite::Connection,
    news: &Value,
    symbol: &str,
    side: &str,
) -> Option<String> {
    let cfg = veto_cfg(news);
    if !cfg.enabled || side != "long" {
        return None;
    }
    negative_event_hit(conn, symbol, cfg.window_min, cfg.max_items)
        .map(|hit| format!("负面事件否决: {hit}"))
}

/// 一条待入库的资讯。
#[derive(Debug, Clone)]
pub struct Incoming {
    pub ts: i64,
    pub source: String,
    pub title: String,
    pub url: String,
    pub summary: String,
}

/// 入库。标题短于 8 个字的丢掉；去重键是 `sha1(小写去空白的 url)`，
/// 没有 url 时用 `sha1(source:title)`。返回新插入的条数。
#[cfg(test)]
pub fn ingest(db: &Db, items: &[Incoming]) -> Result<usize, String> {
    let conn = Connection::open(&db.path).map_err(|e| e.to_string())?;
    let mut inserted = 0;
    conn.execute_batch("PRAGMA busy_timeout=3000;")
        .map_err(|e| e.to_string())?;
    for item in items {
        let title = item.title.trim().to_string();
        if title.chars().count() < 8 {
            continue;
        }
        let url = item.url.trim().to_lowercase();
        let basis = if url.is_empty() {
            format!("{}:{title}", item.source)
        } else {
            url
        };
        let key = sha1_hex(&basis);
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM news_items WHERE dedupe = ?1",
                [&key],
                |r| r.get(0),
            )
            .map_err(|e| format!("count: {e}"))?;
        if exists > 0 {
            continue;
        }
        let n = conn
            .execute(
                "INSERT INTO news_items(ts, source, title, url, summary, sentiment, symbols, dedupe) VALUES(?1,?2,?3,?4,?5,0,'[]',?6)",
                rusqlite::params![item.ts, item.source, title, item.url, item.summary, key],
            )
            .map_err(|e| format!("insert: {e}"))?;
        inserted += n;
    }
    Ok(inserted)
}

fn sha1_hex(text: &str) -> String {
    use sha1::Digest;
    hex::encode(sha1::Sha1::digest(text.as_bytes()))
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
         CREATE TABLE news_items(id INTEGER PRIMARY KEY, ts INTEGER, source TEXT, title TEXT, url TEXT, \
         summary TEXT, sentiment REAL, symbols TEXT, dedupe TEXT); \
         INSERT INTO news_items VALUES(1,100,'coindesk','旧','','',0.1234,'[\"ETHUSDT\"]','d1'); \
         INSERT INTO news_items VALUES(2,300,'coindesk','新','','',-0.5,'[\"BTCUSDT\"]','d2'); \
         INSERT INTO news_items VALUES(3,200,'demo','别的源','','',0.0,'[\"BTCUSDT\"]','d3');",
    )
    .unwrap();
    drop(conn);
    Db::open(path.to_str().unwrap()).unwrap()
}

#[derive(Debug, Clone)]
struct NewsCfg {
    enabled: bool,
    refresh_minutes: i64,
    max_items_per_source: usize,
    keep_days: i64,
    request_timeout_sec: u64,
    rsshub_base: String,
    sources: Vec<String>,
}

fn news_cfg(db: &Db) -> NewsCfg {
    let raw = db.setting("news").ok().flatten().unwrap_or(Value::Null);
    let num = |key: &str, default: i64| raw.get(key).and_then(Value::as_i64).unwrap_or(default);
    let sources = raw
        .get("sources")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .filter(|items: &Vec<String>| !items.is_empty())
        .unwrap_or_else(|| DEFAULT_SOURCES.iter().map(|s| (*s).to_string()).collect());
    NewsCfg {
        enabled: raw.get("enabled").and_then(Value::as_bool).unwrap_or(true),
        refresh_minutes: num("refresh_minutes", 30).clamp(1, 1440),
        max_items_per_source: num("max_items_per_source", 20).clamp(1, 100) as usize,
        keep_days: num("keep_days", 7).clamp(1, 365),
        request_timeout_sec: num("request_timeout_sec", 8).clamp(3, 60) as u64,
        rsshub_base: raw
            .get("rsshub_base")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string(),
        sources,
    }
}

fn source_catalog(state: &NewsState) -> Vec<Value> {
    let cfg = news_cfg(&state.db);
    let last = state.last_snapshot();
    state
        .sources
        .iter()
        .map(|source| {
            let name = source.name();
            let status = last.status.get(name).cloned().unwrap_or(Value::Null);
            json!({
                "id": name,
                "label": source_label(name),
                "enabled": cfg.sources.iter().any(|item| item == name),
                "ok": status["ok"].as_bool().unwrap_or(false),
                "count": status["count"].as_i64().unwrap_or(0),
                "error": status["error"].as_str().unwrap_or(""),
                "latency_ms": status["latency_ms"].as_i64().unwrap_or(0),
                "needs_proxy": matches!(name, "binance_square" | "binance_announcement" | "coindesk"),
            })
        })
        .collect()
}

async fn refresh_handler(State(state): State<NewsState>) -> Json<Value> {
    Json(refresh(&state).await)
}

async fn diagnostics_handler(State(state): State<NewsState>) -> Json<Value> {
    Json(diagnostics(&state).await)
}

async fn ai_summary_handler(State(state): State<NewsState>) -> Json<Value> {
    Json(ai_summary(&state).await)
}

/// 手动刷新仍遵守冷却，避免面板连点把外部源打爆；返回 next_in 供倒计时。
async fn refresh(state: &NewsState) -> Value {
    let cfg = news_cfg(&state.db);
    if !cfg.enabled {
        return json!({"ok": false, "error": "资讯模块已关闭"});
    }
    let now = crate::now();
    let cooldown = cfg.refresh_minutes * 60;
    let last_ts = state.last_snapshot().ts;
    if last_ts > 0 && now - last_ts < cooldown {
        return json!({"ok": true, "skipped": true, "next_in": cooldown - (now - last_ts)});
    }
    let _guard = state.lock.lock().await;
    let last_ts = state.last_snapshot().ts;
    if last_ts > 0 && now - last_ts < cooldown {
        return json!({"ok": true, "skipped": true, "next_in": cooldown - (now - last_ts)});
    }
    let active: Vec<Arc<dyn NewsSource>> = state
        .sources
        .iter()
        .filter(|source| cfg.sources.iter().any(|name| name == source.name()))
        .cloned()
        .collect();
    let results = join_all(active.iter().map(|source| {
        let source = source.clone();
        let limit = cfg.max_items_per_source;
        let timeout = cfg.request_timeout_sec;
        async move { run_source(source.as_ref(), limit, timeout).await }
    }))
    .await;
    let mut status = Map::new();
    let mut failed = Vec::new();
    let mut inserted = 0_i64;
    for (name, fetched, error, latency_ms, url) in results {
        let ok = error.is_empty();
        if !ok {
            failed.push(name.to_string());
        }
        status.insert(
            name.to_string(),
            json!({
                "ok": ok,
                "count": fetched.len(),
                "error": error,
                "latency_ms": latency_ms,
                "url": url,
            }),
        );
        inserted += store_items(&state.db, &fetched, now) as i64;
    }
    prune_old(&state.db, now - cfg.keep_days * 86_400);
    if inserted == 0 && count_items(&state.db) == 0 {
        inserted += seed_demo(&state.db) as i64;
    }
    let stale = !status.is_empty() && failed.len() == status.len();
    {
        let mut last = state
            .last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        last.ts = now;
        last.status = status.clone();
        last.inserted = inserted;
        last.failed = failed.clone();
        last.stale = stale;
    }
    let level = if stale && inserted == 0 {
        "warn"
    } else {
        "info"
    };
    record_event(
        &state.db,
        level,
        inserted,
        status.len() - failed.len(),
        status.len(),
        &failed,
    );
    json!({
        "ok": true,
        "inserted": inserted,
        "sources": status,
        "failed": failed,
        "stale": stale,
    })
}

async fn run_source(
    source: &dyn NewsSource,
    limit: usize,
    timeout_sec: u64,
) -> (&'static str, Vec<Incoming>, String, i64, String) {
    let name = source.name();
    let started = Instant::now();
    let budget = Duration::from_secs(timeout_sec + 5);
    match tokio::time::timeout(budget, source.fetch(limit)).await {
        Ok(Ok(items)) => {
            let latency = started.elapsed().as_millis() as i64;
            if items.is_empty() {
                (
                    name,
                    items,
                    "接口可达但未解析出条目（结构可能已变化）".to_string(),
                    latency,
                    String::new(),
                )
            } else {
                (name, items, String::new(), latency, String::new())
            }
        }
        Ok(Err(error)) => (
            name,
            Vec::new(),
            error.message,
            started.elapsed().as_millis() as i64,
            error.url,
        ),
        Err(_) => (
            name,
            Vec::new(),
            format!("超时 (> {}s)", timeout_sec + 5),
            started.elapsed().as_millis() as i64,
            String::new(),
        ),
    }
}

async fn diagnostics(state: &NewsState) -> Value {
    let cfg = news_cfg(&state.db);
    let active: Vec<Arc<dyn NewsSource>> = state
        .sources
        .iter()
        .filter(|source| cfg.sources.iter().any(|name| name == source.name()))
        .cloned()
        .collect();
    let results = join_all(active.iter().map(|source| {
        let source = source.clone();
        let limit = cfg.max_items_per_source;
        let timeout = cfg.request_timeout_sec;
        async move { run_source(source.as_ref(), limit, timeout).await }
    }))
    .await;
    let mut rows = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut ok_count = 0;
    for (name, fetched, error, latency_ms, url) in results {
        seen.insert(name.to_string());
        let ok = error.is_empty();
        if ok {
            ok_count += 1;
        }
        rows.push(json!({
            "source": name,
            "label": source_label(name),
            "enabled": true,
            "ok": ok,
            "count": fetched.len(),
            "latency_ms": latency_ms,
            "error": error,
            "url": url,
            "hint": source_hint(name, &error),
        }));
    }
    for source in state.sources.iter() {
        let name = source.name();
        if seen.contains(name) || name == "binance_announcement_rsshub" {
            continue;
        }
        rows.push(json!({
            "source": name,
            "label": source_label(name),
            "enabled": false,
            "ok": false,
            "count": 0,
            "latency_ms": 0,
            "error": "未启用",
            "url": "",
            "hint": "在「参数配置 → 资讯 → sources」中加入该源名即可启用",
        }));
    }
    let total = rows.len();
    json!({
        "ok": ok_count > 0,
        "checked_at": crate::now(),
        "timeout_sec": cfg.request_timeout_sec,
        "ok_count": ok_count,
        "total": total,
        "rows": rows,
        "suggestion": if ok_count > 0 { "" } else { "所有源均不可达: 请检查服务器出网/DNS, 或在「参数配置 → 资讯」填写 HTTP 代理" },
    })
}

fn source_hint(name: &str, error: &str) -> &'static str {
    if error.is_empty() {
        return "";
    }
    if error.contains("Connect") || error.contains("Network") || error.contains("dns") {
        return match name {
            "binance_square" | "binance_announcement" => {
                "该源需要访问 www.binance.com; 可填写代理后重试, 或改用其它源"
            }
            "coindesk" => "该源需要访问 www.coindesk.com; 可填写代理后重试, 或改用其它源",
            _ => "该域名在当前网络不可达; 可填写代理后重试",
        };
    }
    if error.contains("HTTP 403") {
        return "被站点风控(Cloudflare)拦截, 换网络/代理或换源";
    }
    if error.contains("HTTP 404") || error.contains("HTTP 410") {
        return "接口已下线或地址变更, 建议更换源";
    }
    if error.contains("超时") {
        return "响应超时: 检查网络质量, 或调大 request_timeout_sec";
    }
    if error.contains("未解析出条目") {
        return "接口可达但结构变化, 需要更新解析规则";
    }
    ""
}

async fn ai_summary(state: &NewsState) -> Value {
    let items = items(&state.db, 20, "", "");
    if items.is_empty() {
        return json!({"ok": false, "error": "暂无资讯"});
    }
    let Some(ai) = &state.ai else {
        return json!({"ok": false, "error": "AI 未启用"});
    };
    let (cfg, chain) = provider_chain(ai);
    if !cfg.enabled || chain.is_empty() {
        return json!({"ok": false, "error": "AI 未启用"});
    }
    let payload: Vec<Value> = items
        .iter()
        .take(20)
        .map(|item| {
            json!({
                "title": item["title"],
                "summary": item["summary"].as_str().unwrap_or("").chars().take(200).collect::<String>(),
                "source": item["source"],
            })
        })
        .collect();
    let messages = vec![
        ChatMessage::new(
            "system",
            "你是加密市场资讯分析员. 输入是一批新闻标题与摘要, 请输出:\
             {\"summary\":\"120 字以内的市场情绪综述\",\"sentiment\":-1.0 到 1.0 的情绪分,\
             \"hot\":[\"最多3个最需关注的主题\"],\"risk\":\"一句话风险提示\"} 只输出 JSON.",
        ),
        ChatMessage::new(
            "user",
            &serde_json::to_string(&payload).unwrap_or_else(|_| "[]".to_string()),
        ),
    ];
    let mut call = ChatCall::new(&messages, "news_summary");
    call.temperature = Some(0.3);
    let outcome =
        ai_client::chat_with_fallback(ai.transport.as_ref(), &ai.db, &chain, call, &cfg).await;
    if !outcome.ok {
        return json!({"ok": false, "error": outcome.error});
    }
    match ai_client::extract_json(&outcome.text) {
        Ok(data) => json!({
            "ok": true,
            "provider": outcome.provider,
            "model": outcome.model,
            "summary": Value::Object(data),
            "items": items.len(),
        }),
        Err(error) => {
            json!({"ok": false, "error": error, "raw": outcome.text.chars().take(800).collect::<String>()})
        }
    }
}

fn clean_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_tag = false;
    for ch in text.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if in_tag => {}
            _ if ch.is_whitespace() => {
                if !out.ends_with(' ') {
                    out.push(' ');
                }
            }
            _ => out.push(ch),
        }
    }
    out.trim().to_string()
}

fn score_text(text: &str) -> f64 {
    const POSITIVE: &[(&str, f64)] = &[
        ("上涨", 1.0),
        ("突破", 1.2),
        ("新高", 1.4),
        ("利好", 1.3),
        ("增持", 1.1),
        ("回购", 1.0),
        ("批准", 1.2),
        ("通过", 0.8),
        ("上线", 1.0),
        ("合作", 0.9),
        ("融资", 0.8),
        ("流入", 0.9),
        ("rally", 1.0),
        ("surge", 1.0),
        ("soar", 1.2),
        ("bullish", 1.1),
        ("record high", 1.3),
        ("approval", 1.0),
        ("partnership", 0.8),
        ("inflow", 0.9),
        ("upgrade", 0.9),
        ("gain", 0.8),
    ];
    const NEGATIVE: &[(&str, f64)] = &[
        ("下跌", -1.0),
        ("暴跌", -1.5),
        ("跌破", -1.2),
        ("利空", -1.3),
        ("减持", -1.0),
        ("抛售", -1.2),
        ("黑客", -1.4),
        ("被盗", -1.4),
        ("调查", -1.0),
        ("起诉", -1.2),
        ("罚款", -1.1),
        ("清算", -1.2),
        ("流出", -0.9),
        ("暂停", -1.0),
        ("下架", -1.4),
        ("延期", -0.7),
        ("风险", -0.6),
        ("crash", -1.4),
        ("plunge", -1.2),
        ("dump", -1.1),
        ("bearish", -1.0),
        ("hack", -1.4),
        ("lawsuit", -1.2),
        ("outflow", -0.9),
        ("delist", -1.4),
        ("ban", -1.2),
        ("liquidation", -1.2),
    ];
    let lower = text.to_lowercase();
    if lower.is_empty() {
        return 0.0;
    }
    let mut score = 0.0;
    let mut hits = 0;
    for (word, weight) in POSITIVE.iter().chain(NEGATIVE.iter()) {
        if lower.contains(word) {
            score += weight;
            hits += 1;
        }
    }
    if hits == 0 {
        return 0.0;
    }
    (score / (2.0_f64).max(hits as f64 * 0.9)).clamp(-1.0, 1.0)
}

fn extract_symbols(text: &str) -> Vec<String> {
    const ALIASES: &[(&str, &str)] = &[
        ("BTC", "BTCUSDT"),
        ("BITCOIN", "BTCUSDT"),
        ("比特币", "BTCUSDT"),
        ("ETH", "ETHUSDT"),
        ("ETHEREUM", "ETHUSDT"),
        ("以太坊", "ETHUSDT"),
        ("SOL", "SOLUSDT"),
        ("SOLANA", "SOLUSDT"),
        ("BNB", "BNBUSDT"),
        ("币安币", "BNBUSDT"),
        ("DOGE", "DOGEUSDT"),
        ("狗狗币", "DOGEUSDT"),
        ("XRP", "XRPUSDT"),
        ("瑞波", "XRPUSDT"),
        ("ADA", "ADAUSDT"),
        ("LINK", "LINKUSDT"),
        ("AVAX", "AVAXUSDT"),
        ("ARB", "ARBUSDT"),
        ("OP", "OPUSDT"),
        ("TON", "TONUSDT"),
    ];
    let upper = text.to_uppercase();
    let mut found = std::collections::BTreeSet::new();
    for (alias, symbol) in ALIASES {
        let hit = if alias.chars().count() > 3 {
            upper.contains(alias) || text.contains(alias)
        } else {
            ascii_word_hit(&upper, alias)
        };
        if hit {
            found.insert((*symbol).to_string());
        }
    }
    found.into_iter().take(5).collect()
}

fn ascii_word_hit(text: &str, word: &str) -> bool {
    let mut start = 0;
    while let Some(index) = text[start..].find(word) {
        let begin = start + index;
        let end = begin + word.len();
        let left_ok = begin == 0
            || !text[..begin]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric());
        let right_ok = end == text.len()
            || !text[end..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric());
        if left_ok && right_ok {
            return true;
        }
        start = end;
    }
    false
}

const TEXT_KEYS: &[&str] = &[
    "title",
    "content",
    "body",
    "text",
    "summary",
    "description",
    "articleTitle",
];
const URL_KEYS: &[&str] = &["url", "link", "shareUrl", "webLink", "sourceUrl"];
const TIME_KEYS: &[&str] = &[
    "publishTime",
    "releaseDate",
    "createTime",
    "date",
    "published_at",
    "publishDate",
];

fn pick(node: &Map<String, Value>, keys: &[&str]) -> String {
    for key in keys {
        if let Some(value) = node.get(*key).and_then(Value::as_str) {
            if !value.trim().is_empty() {
                return value.trim().to_string();
            }
        }
    }
    String::new()
}

fn parse_ts(value: &Value) -> i64 {
    let raw = value
        .as_i64()
        .or_else(|| value.as_str().and_then(|s| s.parse::<i64>().ok()));
    match raw {
        Some(ts) if ts > 100_000_000_000 => ts / 1000,
        Some(ts) => ts,
        None => 0,
    }
}

fn extract_json_items(value: &Value, source: &str, limit: usize) -> Vec<Incoming> {
    let mut nodes = Vec::new();
    walk_json(value, &mut nodes, 0);
    let mut items = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for node in nodes {
        let title = pick(&node, TEXT_KEYS);
        if title.chars().count() < 8 {
            continue;
        }
        let title: String = title.chars().take(380).collect();
        if !seen.insert(title.clone()) {
            continue;
        }
        let ts = TIME_KEYS
            .iter()
            .find_map(|key| node.get(*key).map(parse_ts))
            .filter(|ts| *ts > 0)
            .unwrap_or(0);
        items.push(Incoming {
            ts,
            source: source.to_string(),
            title,
            url: pick(&node, URL_KEYS).chars().take(700).collect(),
            summary: pick(&node, &["summary", "description", "content", "body"])
                .chars()
                .take(400)
                .collect(),
        });
        if items.len() >= limit {
            break;
        }
    }
    items
}

fn walk_json(node: &Value, out: &mut Vec<Map<String, Value>>, depth: usize) {
    if depth > 8 || out.len() >= 80 {
        return;
    }
    match node {
        Value::Object(map) => {
            if TEXT_KEYS.iter().any(|key| {
                map.get(*key)
                    .and_then(Value::as_str)
                    .is_some_and(|s| s.chars().count() > 12)
            }) {
                out.push(map.clone());
                return;
            }
            for value in map.values() {
                walk_json(value, out, depth + 1);
            }
        }
        Value::Array(items) => {
            for value in items.iter().take(80) {
                walk_json(value, out, depth + 1);
            }
        }
        _ => {}
    }
}

fn parse_rss(text: &str, source: &str, limit: usize) -> Vec<Incoming> {
    let mut items = Vec::new();
    for block in rss_blocks(text) {
        let title = tag_text(block, "title");
        if title.chars().count() < 8 {
            continue;
        }
        let link = tag_text(block, "link");
        let link = if link.is_empty() {
            attr(block, "link", "href")
        } else {
            link
        };
        let summary = {
            let raw = tag_text(block, "description");
            if raw.is_empty() {
                tag_text(block, "summary")
            } else {
                raw
            }
        };
        let published = {
            let raw = tag_text(block, "pubDate");
            if raw.is_empty() {
                tag_text(block, "updated")
            } else {
                raw
            }
        };
        items.push(Incoming {
            ts: parse_rfc2822(&published),
            source: source.to_string(),
            title: title.chars().take(380).collect(),
            url: link.chars().take(700).collect(),
            summary: clean_text(&summary).chars().take(400).collect(),
        });
        if items.len() >= limit {
            break;
        }
    }
    items
}

fn rss_blocks(text: &str) -> Vec<&str> {
    let mut blocks = Vec::new();
    let mut cursor = 0;
    while cursor < text.len() {
        let item = text[cursor..].find("<item").map(|i| (i + cursor, "item"));
        let entry = text[cursor..].find("<entry").map(|i| (i + cursor, "entry"));
        let Some((start, tag)) = item.into_iter().chain(entry).min_by_key(|(i, _)| *i) else {
            break;
        };
        let close = format!("</{tag}>");
        let Some(end) = text[start..].find(&close).map(|i| i + start + close.len()) else {
            break;
        };
        blocks.push(&text[start..end]);
        cursor = end;
        if blocks.len() >= 40 {
            break;
        }
    }
    blocks
}

fn tag_text(block: &str, name: &str) -> String {
    let open = format!("<{name}");
    let Some(start) = block.find(&open) else {
        return String::new();
    };
    let Some(gt) = block[start..].find('>').map(|i| i + start) else {
        return String::new();
    };
    let close = format!("</{name}>");
    let Some(end) = block[gt..].find(&close).map(|i| i + gt) else {
        return String::new();
    };
    let raw = &block[gt + 1..end];
    let raw = raw
        .trim()
        .strip_prefix("<![CDATA[")
        .and_then(|s| s.strip_suffix("]]>"))
        .unwrap_or(raw);
    clean_text(&decode_entities(raw))
}

fn attr(block: &str, name: &str, attr: &str) -> String {
    let open = format!("<{name}");
    let Some(start) = block.find(&open) else {
        return String::new();
    };
    let Some(end) = block[start..].find('>').map(|i| i + start) else {
        return String::new();
    };
    let tag = &block[start..=end];
    let key = format!("{attr}=");
    let Some(pos) = tag.find(&key) else {
        return String::new();
    };
    let rest = tag[pos + key.len()..].trim_start();
    let quote = rest.chars().next().unwrap_or('"');
    if quote != '"' && quote != '\'' {
        return String::new();
    }
    rest[1..]
        .find(quote)
        .map(|end| rest[1..1 + end].to_string())
        .unwrap_or_default()
}

fn decode_entities(text: &str) -> String {
    text.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
}

fn parse_rfc2822(raw: &str) -> i64 {
    let text = raw.trim();
    if text.is_empty() {
        return 0;
    }
    let parts: Vec<&str> = text.split_whitespace().collect();
    let Some(day_index) = parts
        .iter()
        .position(|part| part.trim_end_matches(',').parse::<i64>().is_ok())
    else {
        return 0;
    };
    if parts.len() < day_index + 5 {
        return 0;
    }
    let day = parts[day_index]
        .trim_end_matches(',')
        .parse::<i64>()
        .unwrap_or(0);
    let months = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let month = months
        .iter()
        .position(|m| parts[day_index + 1].to_lowercase().starts_with(m))
        .map(|i| i as i64 + 1)
        .unwrap_or(0);
    let year = parts[day_index + 2].parse::<i64>().unwrap_or(0);
    let time: Vec<i64> = parts[day_index + 3]
        .split(':')
        .map(|item| item.parse::<i64>().unwrap_or(0))
        .collect();
    if day == 0 || month == 0 || year < 2000 || time.len() < 3 {
        return 0;
    }
    let days = days_from_civil(year, month, day);
    days * 86_400 + time[0] * 3600 + time[1] * 60 + time[2]
}

/// Howard Hinnant 的 civil-from-days 逆算法，只处理 2000 年后的公历日期。
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn store_items(db: &Db, items: &[Incoming], now: i64) -> usize {
    if items.is_empty() {
        return 0;
    }
    let Ok(conn) = db.conn.lock() else {
        return 0;
    };
    let _ = conn.execute_batch("PRAGMA busy_timeout=3000;");
    let mut inserted = 0;
    for item in items {
        let title = clean_text(&item.title);
        if title.chars().count() < 8 {
            continue;
        }
        let title: String = title.chars().take(380).collect();
        let url = item.url.trim().to_lowercase();
        let basis = if url.is_empty() {
            format!("{}:{title}", item.source)
        } else {
            url
        };
        let key = sha1_hex(&basis);
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM news_items WHERE dedupe = ?1)",
                [&key],
                |row| row.get(0),
            )
            .unwrap_or(false);
        if exists {
            continue;
        }
        let text = format!("{title} {}", item.summary);
        let sentiment = score_text(&text);
        let symbols = extract_symbols(&text);
        let ts = if item.ts > 0 { item.ts } else { now };
        if conn
            .execute(
                "INSERT INTO news_items(ts, source, title, url, summary, sentiment, symbols, dedupe) \
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                (
                    ts,
                    &item.source,
                    &title,
                    &item.url,
                    &item.summary,
                    sentiment,
                    serde_json::to_string(&symbols).unwrap_or_else(|_| "[]".to_string()),
                    &key,
                ),
            )
            .is_ok()
        {
            inserted += 1;
        }
    }
    inserted
}

fn prune_old(db: &Db, before: i64) {
    if let Ok(conn) = db.conn.lock() {
        let _ = conn.execute("DELETE FROM news_items WHERE ts < ?1", [before]);
    }
}

fn count_items(db: &Db) -> i64 {
    db.conn
        .lock()
        .ok()
        .and_then(|conn| {
            conn.query_row("SELECT COUNT(*) FROM news_items", [], |row| row.get(0))
                .ok()
        })
        .unwrap_or(0)
}

fn seed_demo(db: &Db) -> usize {
    let samples = [
        (
            "BTC 现货 ETF 单日净流入 2.4 亿美元, 连续三日为正",
            0.7,
            vec!["BTCUSDT"],
        ),
        (
            "以太坊 Layer2 活跃地址创季度新高, 手续费降至低位",
            0.45,
            vec!["ETHUSDT"],
        ),
        ("某交易所宣布将上线新的永续合约品种", 0.3, Vec::new()),
        (
            "市场杠杆率快速上升, 分析师提示短期回调风险",
            -0.4,
            vec!["BTCUSDT", "ETHUSDT"],
        ),
        ("SOL 生态 DEX 交易量环比下滑 18%", -0.35, vec!["SOLUSDT"]),
        ("监管机构就稳定币储备披露征求意见", -0.2, Vec::new()),
    ];
    let now = crate::now();
    let Ok(conn) = db.conn.lock() else {
        return 0;
    };
    let mut inserted = 0;
    for (index, (title, sentiment, symbols)) in samples.into_iter().enumerate() {
        let key = sha1_hex(&format!("demo:{title}"));
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM news_items WHERE dedupe = ?1)",
                [&key],
                |row| row.get(0),
            )
            .unwrap_or(false);
        if exists {
            continue;
        }
        if conn
            .execute(
                "INSERT INTO news_items(ts, source, title, url, summary, sentiment, symbols, dedupe) \
                 VALUES(?1,'demo',?2,'',?3,?4,?5,?6)",
                (
                    now - index as i64 * 900,
                    title,
                    "离线演示数据: 当前网络无法访问外部资讯源, 此条目用于展示面板结构。",
                    sentiment,
                    serde_json::to_string(&symbols).unwrap_or_else(|_| "[]".to_string()),
                    key,
                ),
            )
            .is_ok()
        {
            inserted += 1;
        }
    }
    inserted
}

fn record_event(
    db: &Db,
    level: &str,
    inserted: i64,
    ok_sources: usize,
    total: usize,
    failed: &[String],
) {
    let message = if failed.is_empty() {
        format!("资讯刷新: 新增 {inserted} 条, 成功 {ok_sources}/{total} 个源")
    } else {
        format!(
            "资讯刷新: 新增 {inserted} 条, 成功 {ok_sources}/{total} 个源, 失败: {}",
            failed
                .iter()
                .take(4)
                .map(|name| source_label(name))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    if let Ok(conn) = db.conn.lock() {
        let _ = conn.execute(
            "INSERT INTO engine_events(ts, level, category, message, payload) VALUES(?1,?2,'news',?3,?4)",
            (
                crate::now(),
                level,
                message,
                serde_json::to_string(&json!({"failed": failed, "inserted": inserted})).unwrap_or_else(|_| "{}".to_string()),
            ),
        );
    }
}

enum SourceKind {
    Json,
    Rss,
}

struct HttpSource {
    name: &'static str,
    url: String,
    params: Vec<(&'static str, &'static str)>,
    kind: SourceKind,
}

impl NewsSource for HttpSource {
    fn name(&self) -> &'static str {
        self.name
    }

    fn fetch<'a>(&'a self, limit: usize) -> BoxFuture<'a, Result<Vec<Incoming>, FetchError>> {
        Box::pin(async move {
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(12))
                .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36")
                .build()
                .map_err(|error| FetchError {
                    message: error.to_string(),
                    url: self.url.to_string(),
                })?;
            let mut request = client.get(self.url.as_str()).header(
                "Accept",
                "application/rss+xml, application/xml;q=0.9, application/json;q=0.8, text/html;q=0.7, */*;q=0.5",
            );
            if !self.params.is_empty() {
                let params: Vec<(&str, String)> = self
                    .params
                    .iter()
                    .map(|(key, value)| (*key, (*value).to_string()))
                    .chain(std::iter::once(("pageSize", limit.to_string())))
                    .collect();
                request = request.query(&params);
            }
            let response = request.send().await.map_err(|error| FetchError {
                message: error.to_string(),
                url: self.url.to_string(),
            })?;
            let status = response.status();
            if !status.is_success() {
                return Err(FetchError {
                    message: format!("HTTP {}", status.as_u16()),
                    url: self.url.to_string(),
                });
            }
            let body = response.text().await.map_err(|error| FetchError {
                message: error.to_string(),
                url: self.url.to_string(),
            })?;
            Ok(match &self.kind {
                SourceKind::Json => match serde_json::from_str::<Value>(&body) {
                    Ok(value) => extract_json_items(&value, self.name, limit),
                    Err(error) => {
                        return Err(FetchError {
                            message: format!("JSON 解析失败: {error}"),
                            url: self.url.to_string(),
                        })
                    }
                },
                SourceKind::Rss => parse_rss(&body, self.name, limit),
            })
        })
    }
}

fn http_sources(rsshub_base: &str) -> Vec<Arc<dyn NewsSource>> {
    let rss = |name: &'static str, url: &'static str| -> Arc<dyn NewsSource> {
        Arc::new(HttpSource {
            name,
            url: url.to_string(),
            params: Vec::new(),
            kind: SourceKind::Rss,
        })
    };
    let mut sources = vec![
        Arc::new(HttpSource {
            name: "binance_square",
            url: "https://www.binance.com/bapi/composite/v1/public/content/square/feed".to_string(),
            params: vec![("page", "1")],
            kind: SourceKind::Json,
        }),
        Arc::new(HttpSource {
            name: "binance_announcement",
            url: "https://www.binance.com/bapi/apex/v1/public/apex/cms/article/list/query"
                .to_string(),
            params: vec![("type", "1"), ("pageNo", "1")],
            kind: SourceKind::Json,
        }),
        rss("cointelegraph", "https://cointelegraph.com/rss"),
        rss("theblock", "https://www.theblock.co/rss.xml"),
        rss("decrypt", "https://decrypt.co/feed"),
        rss("bitcoinmagazine", "https://bitcoinmagazine.com/feed"),
        rss("newsbtc", "https://www.newsbtc.com/feed/"),
        rss("beincrypto", "https://beincrypto.com/feed/"),
        rss("utoday", "https://u.today/rss"),
        rss(
            "coindesk",
            "https://www.coindesk.com/arc/outboundfeeds/rss/",
        ),
    ];
    let base = rsshub_base.trim().trim_end_matches('/');
    if !base.is_empty() {
        sources.push(Arc::new(HttpSource {
            name: "binance_announcement_rsshub",
            url: format!("{base}/binance/announcement"),
            params: Vec::new(),
            kind: SourceKind::Rss,
        }));
    }
    sources
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn filters_news_by_symbol_newest_first() {
        let path = std::env::temp_dir().join("nq-news.db");
        let db = seed(&path);
        let app = Router::new()
            .nest("/api/v1", routes())
            .with_state(NewsState::with_sources(db, Vec::new()));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/news?symbol=btcusdt&source=coindesk")
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
        assert_eq!(items.len(), 1, "应只剩 coindesk 且含 BTCUSDT 的一条");
        assert_eq!(items[0]["title"], "新");
        assert_eq!(items[0]["source_label"], "CoinDesk");
        assert_eq!(items[0]["sentiment"], -0.5);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn ingest_skips_short_titles_and_duplicate_urls() {
        let path = std::env::temp_dir().join("nq-news-ingest.db");
        let db = seed(&path);
        let incoming = vec![
            Incoming {
                ts: 400,
                source: "coindesk".into(),
                title: "太短".into(),
                url: "https://x/a".into(),
                summary: "".into(),
            },
            Incoming {
                ts: 500,
                source: "coindesk".into(),
                title: "比特币突破十万新高".into(),
                url: "https://x/b".into(),
                summary: "".into(),
            },
            Incoming {
                ts: 600,
                source: "demo".into(),
                title: "同一条新闻再来一次".into(),
                url: "HTTPS://X/B".into(),
                summary: "".into(),
            },
        ];
        let inserted = ingest(&db, &incoming).unwrap_or_else(|e| panic!("ingest: {e}"));
        assert_eq!(inserted, 1, "短标题丢掉，大小写不同的同一 URL 只入一次");

        let app = Router::new()
            .nest("/api/v1", routes())
            .with_state(NewsState::with_sources(db, Vec::new()));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/news?limit=10")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        let titles: Vec<&str> = body["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|i| i["title"].as_str())
            .collect();
        assert_eq!(
            titles
                .iter()
                .filter(|t| **t == "比特币突破十万新高")
                .count(),
            1
        );
        let _ = std::fs::remove_file(&path);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn overview_buckets_the_last_day_by_the_configured_threshold() {
        let path = std::env::temp_dir().join("nq-news-overview.db");
        let db = seed(&path);
        let now = crate::now();
        let conn = db.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO app_settings VALUES('news','{\"sentiment_threshold\":0.3}',0)",
            [],
        )
        .unwrap();
        conn.execute("DELETE FROM news_items", []).unwrap();
        conn.execute(
            "INSERT INTO news_items VALUES(1,?1,'coindesk','利好','','',0.6,'[]','a')",
            [now - 1000],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO news_items VALUES(2,?1,'coindesk','利空','','',-0.5,'[]','b')",
            [now - 2000],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO news_items VALUES(3,?1,'demo','平淡','','',0.1,'[]','c')",
            [now - 3000],
        )
        .unwrap();
        // 超过 24 小时的不进统计。
        conn.execute(
            "INSERT INTO news_items VALUES(4,?1,'demo','旧闻','','',0.9,'[]','d')",
            [now - 90_000],
        )
        .unwrap();
        drop(conn);
        let app = Router::new()
            .nest("/api/v1", routes())
            .with_state(NewsState::with_sources(db, Vec::new()));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/news/overview")
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
        assert_eq!(body["total_24h"], 3, "一天前的旧闻不应计入");
        assert_eq!(body["positive"], 1);
        assert_eq!(body["negative"], 1);
        assert_eq!(body["neutral"], 1, "0.1 低于 0.3 的阈值，算中性");
        assert_eq!(body["sentiment_threshold"], 0.3);
        assert_eq!(body["by_source"]["coindesk"], 2);
        let _ = std::fs::remove_file(&path);
    }

    struct FakeSource {
        name: &'static str,
        items: Vec<Incoming>,
        error: Option<String>,
    }

    impl NewsSource for FakeSource {
        fn name(&self) -> &'static str {
            self.name
        }

        fn fetch<'a>(&'a self, _limit: usize) -> BoxFuture<'a, Result<Vec<Incoming>, FetchError>> {
            Box::pin(async move {
                match &self.error {
                    Some(message) => Err(FetchError {
                        message: message.clone(),
                        url: format!("https://{}.test/feed", self.name),
                    }),
                    None => Ok(self.items.clone()),
                }
            })
        }
    }

    fn news_fixture(path: &std::path::Path, sources: &[&str], refresh_minutes: i64) -> Db {
        let db = seed(path);
        let raw = json!({
            "enabled": true,
            "refresh_minutes": refresh_minutes,
            "sources": sources,
            "request_timeout_sec": 3,
            "keep_days": 7,
        });
        db.conn
            .lock()
            .unwrap()
            .execute_batch(&format!(
                "DELETE FROM news_items; INSERT INTO app_settings VALUES('news','{}',0);",
                serde_json::to_string(&raw).unwrap()
            ))
            .unwrap();
        db
    }

    fn incoming(title: &str, url: &str) -> Incoming {
        Incoming {
            ts: 0,
            source: "ok".to_string(),
            title: title.to_string(),
            url: url.to_string(),
            summary: "ETF 净流入 2.4 亿美元".to_string(),
        }
    }

    #[tokio::test]
    async fn refresh_enforces_cooldown_and_marks_stale_when_every_source_fails() {
        let path = std::env::temp_dir().join("nq-news-refresh.db");
        let db = news_fixture(&path, &["ok", "bad"], 30);
        let ok = Arc::new(FakeSource {
            name: "ok",
            items: vec![incoming("比特币突破十万大关并创下新高", "https://ok/1")],
            error: None,
        });
        let bad = Arc::new(FakeSource {
            name: "bad",
            items: Vec::new(),
            error: Some("HTTP 503".to_string()),
        });
        let state = NewsState::with_sources(db.clone(), vec![ok.clone(), bad.clone()]);
        let first = refresh(&state).await;
        assert_eq!(first["inserted"], 1, "{first}");
        assert_eq!(first["failed"], json!(["bad"]));
        assert_eq!(first["stale"], false);
        let second = refresh(&state).await;
        assert_eq!(second["skipped"], true, "冷却期内不得重复抓取");
        assert!(second["next_in"].as_i64().unwrap() > 0);

        let failing = NewsState::with_sources(db.clone(), vec![bad]);
        let third = refresh(&failing).await;
        assert_eq!(third["stale"], true, "全部源失败要标注过期");
        assert_eq!(count_items(&db), 1, "全部源失败时保留上次结果");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn diagnostics_reports_each_source_with_hint() {
        let path = std::env::temp_dir().join("nq-news-diagnostics.db");
        let db = news_fixture(&path, &["ok", "bad"], 30);
        let state = NewsState::with_sources(
            db.clone(),
            vec![
                Arc::new(FakeSource {
                    name: "ok",
                    items: vec![incoming("以太坊升级后手续费降至低位", "https://ok/2")],
                    error: None,
                }),
                Arc::new(FakeSource {
                    name: "bad",
                    items: Vec::new(),
                    error: Some("HTTP 403".to_string()),
                }),
                Arc::new(FakeSource {
                    name: "idle",
                    items: Vec::new(),
                    error: None,
                }),
            ],
        );
        let body = diagnostics(&state).await;
        assert_eq!(body["ok_count"], 1);
        assert_eq!(body["total"], 3, "未启用的源也要列出来: {body}");
        let rows = body["rows"].as_array().unwrap();
        let bad = rows.iter().find(|row| row["source"] == "bad").unwrap();
        assert!(
            !bad["hint"].as_str().unwrap().is_empty(),
            "失败源要给出修复建议"
        );
        assert_eq!(bad["error"], "HTTP 403");
        let idle = rows.iter().find(|row| row["source"] == "idle").unwrap();
        assert_eq!(idle["enabled"], false);
        assert_eq!(idle["error"], "未启用");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn ai_summary_fails_clearly_without_news_or_ai() {
        let path = std::env::temp_dir().join("nq-news-ai-summary.db");
        let db = news_fixture(&path, &["ok"], 30);
        let state = NewsState::with_sources(db.clone(), Vec::new());
        let empty = ai_summary(&state).await;
        assert_eq!(empty["ok"], false);
        assert_eq!(empty["error"], "暂无资讯");
        store_items(
            &db,
            &[incoming("市场杠杆率上升带来回调风险", "https://ok/3")],
            crate::now(),
        );
        let no_ai = ai_summary(&state).await;
        assert_eq!(no_ai["ok"], false);
        assert_eq!(no_ai["error"], "AI 未启用");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn sentiment_and_symbols_are_extracted_offline() {
        assert!(score_text("比特币突破新高, 机构大额流入") > 0.4);
        assert!(score_text("交易所被盗, 市场暴跌清算") < -0.5);
        assert_eq!(score_text("一条没有情绪词的公告"), 0.0);
        assert_eq!(
            extract_symbols("BTC 与 ETH 上涨, $SOL 跟涨, BTC 突破"),
            vec!["BTCUSDT", "ETHUSDT", "SOLUSDT"]
        );
        assert_eq!(clean_text("<p>hello   world</p>"), "hello world");
    }

    fn veto_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE news_items(id INTEGER PRIMARY KEY, ts INTEGER, source TEXT, title TEXT, \
             url TEXT, summary TEXT, sentiment REAL, symbols TEXT, dedupe TEXT);",
        )
        .unwrap();
        conn
    }

    fn insert_veto_news(conn: &Connection, ts: i64, title: &str, symbols: &str) {
        conn.execute(
            "INSERT INTO news_items(ts, source, title, url, summary, sentiment, symbols, dedupe) \
             VALUES(?1,'test',?2,'','',0,?3,'veto')",
            rusqlite::params![ts, title, symbols],
        )
        .unwrap();
    }

    fn veto_setting(raw: &str) -> Value {
        serde_json::from_str(raw).unwrap()
    }

    #[test]
    fn negative_news_blocks_same_direction() {
        let conn = veto_conn();
        insert_veto_news(&conn, crate::now() - 60, "BTC 永续被交易所下架", "[]");
        let cfg = veto_setting(r#"{"veto_enabled":true,"veto_window_min":30,"veto_max_items":3}"#);
        let long = veto_reason(&conn, &cfg, "BTCUSDT", "long").expect("利空消息应否决多头");
        assert!(long.starts_with("负面事件否决: "), "{long}");
        assert!(long.contains("下架"), "{long}");
        assert!(
            veto_reason(&conn, &cfg, "BTCUSDT", "short").is_none(),
            "反向做空不受影响"
        );
    }

    #[test]
    fn negative_news_ignores_unrelated_symbol() {
        let conn = veto_conn();
        insert_veto_news(
            &conn,
            crate::now() - 60,
            "ETH 交易对暂停充值",
            "[\"ETHUSDT\"]",
        );
        let cfg = veto_setting(r#"{"veto_enabled":true,"veto_window_min":30,"veto_max_items":3}"#);
        assert!(
            veto_reason(&conn, &cfg, "BTCUSDT", "long").is_none(),
            "其他标的的负面事件不能误伤"
        );
        assert!(veto_reason(&conn, &cfg, "ETHUSDT", "long").is_some());
    }

    #[test]
    fn news_veto_disabled_allows_entry() {
        let conn = veto_conn();
        insert_veto_news(
            &conn,
            crate::now() - 60,
            "BTC 永续被交易所下架",
            "[\"BTCUSDT\"]",
        );
        let cfg = veto_setting(r#"{"veto_enabled":false,"veto_window_min":30,"veto_max_items":3}"#);
        assert!(veto_reason(&conn, &cfg, "BTCUSDT", "long").is_none());
    }

    #[test]
    fn negative_news_window_excludes_old_items() {
        let conn = veto_conn();
        let cfg = veto_setting(r#"{"veto_enabled":true,"veto_window_min":30,"veto_max_items":3}"#);
        insert_veto_news(&conn, crate::now() - 31 * 60, "BTC 永续被下架", "[]");
        assert!(
            veto_reason(&conn, &cfg, "BTCUSDT", "long").is_none(),
            "超出窗口的旧闻不参与否决"
        );
        insert_veto_news(&conn, crate::now() - 5 * 60, "BTC 交易对暂停", "[]");
        assert!(veto_reason(&conn, &cfg, "BTCUSDT", "long").is_some());
    }

    #[test]
    fn negative_news_caps_returned_items() {
        let conn = veto_conn();
        for i in 0..5 {
            insert_veto_news(
                &conn,
                crate::now() - 60 * (i + 1),
                &format!("BTC 下架公告 {i}"),
                "[]",
            );
        }
        let cfg = veto_setting(r#"{"veto_enabled":true,"veto_window_min":30,"veto_max_items":2}"#);
        let hit = veto_reason(&conn, &cfg, "BTCUSDT", "long").expect("应有命中");
        assert_eq!(hit.matches("下架公告").count(), 2, "{hit}");
    }

    #[test]
    fn rsshub_base_adds_announcement_source_only_when_configured() {
        let path = std::env::temp_dir().join("nq-news-rsshub.db");
        let db = seed(&path);
        let names = |state: &NewsState| {
            state
                .sources
                .iter()
                .map(|source| source.name())
                .collect::<Vec<_>>()
        };
        assert!(
            !names(&NewsState::new(db.clone())).contains(&"binance_announcement_rsshub"),
            "rsshub_base 为空时不启用 RSSHub 源"
        );
        {
            let conn = db.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO app_settings(key,value,updated_at) VALUES('news','{\"rsshub_base\":\"https://rss.example.com/\"}',0)",
                [],
            )
            .unwrap();
        }
        assert!(
            names(&NewsState::new(db.clone())).contains(&"binance_announcement_rsshub"),
            "配置 rsshub_base 后应启用 RSSHub 公告源"
        );
        let _ = std::fs::remove_file(&path);
    }
}
