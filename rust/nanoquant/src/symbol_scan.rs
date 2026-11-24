//! 自动换池：纯函数替换计划 + 行情扫描执行。
//!
//! `plan` 根据当前池、持仓、扫描特征与闸门，输出「保留 / 删除 / 新增 / 持仓 / 候选」，
//! 不访问外部资源；`run` 负责拉取合约表与行情，并在单事务内落库。


use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Json;
use rusqlite::{Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::scan::{risk_gate, Candidate, ScanGates};

/// 全进程扫描防重入。手动 API 与后台调度可能并发触发同一轮扫描。
static SCAN_BUSY: AtomicBool = AtomicBool::new(false);

/// RAII 防重入 guard。获取失败表示已有扫描在跑；离开作用域自动释放，早退也不会卡死。
struct BusyGuard;

impl BusyGuard {
    fn acquire() -> Option<Self> {
        SCAN_BUSY
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| Self)
    }
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        SCAN_BUSY.store(false, Ordering::Release);
    }
}

/// 一个标的的本轮扫描特征，只保留换池要看的字段。
#[derive(Debug, Clone)]
pub struct ScannedSymbol {
    pub symbol: String,
    pub data_ok: bool,
    pub vol_ratio: f64,
    pub atr_pct: f64,
    pub quote_volume: f64,
    pub spread_bps: f64,
}

/// 一轮换池的计划。
#[derive(Debug, Clone, Default)]
pub struct ScanPlan {
    pub keep: Vec<String>,
    pub delete: Vec<String>,
    pub add: Vec<String>,
    /// 本轮有持仓的标的。恒为 `keep` 的子集（`held ⊆ keep`），下游不要重复计数。
    pub held: Vec<String>,
    /// 原池中的休市标的（股票/商品类永续 `BREAK`）。与 `held` 分开，避免把休市误当持仓。
    pub suspended: Vec<String>,
    pub candidates: Vec<String>,
    /// 与 `delete` 一一对应的删除原因（闸门失败原因，或缺少行情数据）。
    pub delete_reasons: Vec<(String, String)>,
}

/// 把扫描特征转成闸门输入。强度用 `vol_ratio`，方向留空（换池只看能不能做）。
fn as_candidate(row: &ScannedSymbol) -> Candidate {
    Candidate {
        symbol: row.symbol.clone(),
        data_ok: row.data_ok,
        atr_pct: row.atr_pct,
        vol_ratio: row.vol_ratio,
        quote_volume: row.quote_volume,
        spread_bps: row.spread_bps,
        strength: row.vol_ratio,
        preview_side: None,
    }
}

/// 计算一轮换池计划。纯函数，不产生副作用。
pub fn plan(
    current: &[String],
    held: &HashSet<String>,
    suspended: &HashSet<String>,
    scanned: &[ScannedSymbol],
    gates: &ScanGates,
    target: usize,
) -> ScanPlan {
    // `HashMap::collect` 对重复 symbol 保留最后一条扫描记录；原池判定与候选生成共用它，
    // 避免同一标的在两处采用不同行。如需首条优先，应在扫描层先去重。
    let by_symbol: HashMap<&str, &ScannedSymbol> =
        scanned.iter().map(|s| (s.symbol.as_str(), s)).collect();

    // 原池输入可能重复（例如上游拼接了多个来源），按首次出现顺序去重，
    // 否则同一标的会重复进入 keep / delete / delete_reasons。
    let mut seen: HashSet<&str> = HashSet::new();
    let current_unique: Vec<&String> = current
        .iter()
        .filter(|symbol| seen.insert(symbol.as_str()))
        .collect();
    let current_set: HashSet<&str> = current_unique.iter().map(|s| s.as_str()).collect();

    let mut plan = ScanPlan::default();
    let mut to_replace: Vec<(String, String)> = Vec::new();

    for symbol in current_unique.iter().copied() {
        // 休市优先于持仓：没有行情是预期状态，保留且只记一次。
        if suspended.contains(symbol) {
            plan.keep.push(symbol.clone());
            plan.suspended.push(symbol.clone());
            continue;
        }
        if held.contains(symbol) {
            plan.keep.push(symbol.clone());
            plan.held.push(symbol.clone());
            continue;
        }
        match by_symbol.get(symbol.as_str()) {
            Some(row) => {
                let reason = risk_gate(&as_candidate(row), gates);
                if reason.is_empty() {
                    plan.keep.push(symbol.clone());
                } else {
                    to_replace.push((symbol.clone(), reason));
                }
            }
            None => to_replace.push((symbol.clone(), "缺少行情数据".to_string())),
        }
    }

    // 从 `by_symbol` 生成候选：同一 symbol 天然只出现一次，避免 `scanned` 重复导致重复入池。
    let mut candidates: Vec<&ScannedSymbol> = by_symbol
        .values()
        .copied()
        .filter(|s| !current_set.contains(s.symbol.as_str()))
        .filter(|s| risk_gate(&as_candidate(s), gates).is_empty())
        .collect();
    candidates.sort_by(|a, b| b.vol_ratio.total_cmp(&a.vol_ratio));
    plan.candidates = candidates.iter().map(|s| s.symbol.clone()).collect();

    let replace_count = to_replace.len().min(candidates.len());
    for (symbol, reason) in to_replace.iter().take(replace_count) {
        plan.delete.push(symbol.clone());
        plan.delete_reasons.push((symbol.clone(), reason.clone()));
    }
    plan.add
        .extend(candidates.iter().take(replace_count).map(|s| s.symbol.clone()));

    // 先按「合格 keep + 已替换数量」算空位，再把未被替换的标的并入 keep，
    // 否则剩余标的会先撑大 keep，导致 target 补足算少。
    // leftover（未替换的原池标的）与剩余候选互斥，所以 leftover 必须在 slots 计算之后并入 keep。
    let slots = target.saturating_sub(plan.keep.len() + replace_count);
    let fill = candidates.len().saturating_sub(replace_count).min(slots);
    plan.add.extend(
        candidates
            .iter()
            .skip(replace_count)
            .take(fill)
            .map(|s| s.symbol.clone()),
    );

    // 候选不足时未替换成功的不达标标的实际留在池里，必须计入 keep，避免下游误删。
    plan.keep.extend(
        to_replace
            .iter()
            .skip(replace_count)
            .map(|(symbol, _)| symbol.clone()),
    );

    plan
}

/// 当前可交易（含休市）的 USDT 永续合约。规则与标的池写入共用 `symbols::valid_contract`，
/// 避免扫描层与写入层各有一套口径。
pub(crate) fn tradable_symbols(contracts: &[Value]) -> HashSet<String> {
    contracts
        .iter()
        .filter_map(|row| {
            let symbol = row["symbol"].as_str()?;
            crate::symbols::valid_contract(row, symbol).then_some(symbol.to_string())
        })
        .collect()
}

/// 休市中的股票/商品类永续。行情缺失是预期状态，本轮必须保留在池中。
pub(crate) fn suspended_symbols(contracts: &[Value]) -> HashSet<String> {
    contracts
        .iter()
        .filter(|row| row["status"].as_str() == Some("BREAK"))
        .filter_map(|row| {
            let symbol = row["symbol"].as_str()?;
            crate::symbols::valid_contract(row, symbol).then_some(symbol.to_string())
        })
        .collect()
}

/// 全市场休市集合与当前池求交，结果排序以便稳定输出。
fn suspended_in_pool(contracts: &[Value], current: &HashSet<String>) -> Vec<String> {
    let mut suspended: Vec<String> = suspended_symbols(contracts)
        .into_iter()
        .filter(|symbol| current.contains(symbol))
        .collect();
    suspended.sort();
    suspended
}

/// 剔除用户手动禁用（enabled=0）的标的。它们仍在池表里，若进入候选会在新增时主键冲突。
pub(crate) fn exclude_blocked(
    scanned: Vec<ScannedSymbol>,
    blocked: &HashSet<String>,
) -> Vec<ScannedSymbol> {
    scanned
        .into_iter()
        .filter(|row| !blocked.contains(&row.symbol))
        .collect()
}

/// 扫描配置。每轮从 app_settings 现读，缺项按内置默认值补齐。
struct Config {
    enabled: bool,
    interval_hours: i64,
    market_limit: usize,
    target: usize,
    concurrency: usize,
    kline_limit: usize,
    atr_period: usize,
    gates: ScanGates,
    base_url: String,
}

// `ScanGates` 位于 scan 模块且未实现 Clone/Debug，本任务不改动它，这里按字段复制。
impl Clone for Config {
    fn clone(&self) -> Self {
        Self {
            enabled: self.enabled,
            interval_hours: self.interval_hours,
            market_limit: self.market_limit,
            target: self.target,
            concurrency: self.concurrency,
            kline_limit: self.kline_limit,
            atr_period: self.atr_period,
            gates: ScanGates {
                atr_pct_min: self.gates.atr_pct_min,
                atr_pct_max: self.gates.atr_pct_max,
                volume_ratio_min: self.gates.volume_ratio_min,
                min_24h_quote_volume: self.gates.min_24h_quote_volume,
                spread_bps_max: self.gates.spread_bps_max,
                entry_threshold: self.gates.entry_threshold,
            },
            base_url: self.base_url.clone(),
        }
    }
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("enabled", &self.enabled)
            .field("interval_hours", &self.interval_hours)
            .field("market_limit", &self.market_limit)
            .field("target", &self.target)
            .field("concurrency", &self.concurrency)
            .field("kline_limit", &self.kline_limit)
            .field("atr_period", &self.atr_period)
            .field("gates.atr_pct_min", &self.gates.atr_pct_min)
            .field("gates.atr_pct_max", &self.gates.atr_pct_max)
            .field("gates.volume_ratio_min", &self.gates.volume_ratio_min)
            .field(
                "gates.min_24h_quote_volume",
                &self.gates.min_24h_quote_volume,
            )
            .field("gates.spread_bps_max", &self.gates.spread_bps_max)
            .field("gates.entry_threshold", &self.gates.entry_threshold)
            .field("base_url", &self.base_url)
            .finish()
    }
}

/// 读取一个配置组；组缺失或不是对象时按空对象处理，让默认值接管。
fn setting_group(db: &crate::db::Db, name: &str) -> Result<Value, String> {
    let value = db
        .setting(name)
        .map_err(|error| format!("读取配置 {name} 失败: {error}"))?;
    Ok(match value {
        Some(value) if value.is_object() => value,
        _ => json!({}),
    })
}

fn load_config(db: &crate::db::Db) -> Result<Config, String> {
    let scan = setting_group(db, "symbol_scan")?;
    let engine = setting_group(db, "engine")?;
    let strategy = setting_group(db, "strategy")?;
    let data = setting_group(db, "data")?;
    let mode = crate::symbols::current_mode(db);

    // 点差上限以 `max_spread_bps` 为准，兼容历史上使用的 `spread_bps_max`。
    let spread_bps_max = strategy["max_spread_bps"]
        .as_f64()
        .or_else(|| strategy["spread_bps_max"].as_f64())
        .unwrap_or(8.0);
    let gates = ScanGates {
        atr_pct_min: strategy["atr_pct_min"].as_f64().unwrap_or(0.05),
        atr_pct_max: strategy["atr_pct_max"].as_f64().unwrap_or(4.0),
        volume_ratio_min: strategy["volume_ratio_min"].as_f64().unwrap_or(0.8),
        min_24h_quote_volume: strategy["min_24h_quote_volume"]
            .as_f64()
            .unwrap_or(3_000_000.0),
        spread_bps_max,
        entry_threshold: strategy["entry_threshold"].as_f64().unwrap_or(0.3),
    };
    let (endpoint, fallback) = if mode == "live" {
        ("live_base_url", "https://fapi.binance.com")
    } else {
        ("demo_base_url", "https://testnet.binancefuture.com")
    };

    Ok(Config {
        enabled: scan["enabled"].as_bool().unwrap_or(true),
        interval_hours: scan["interval_hours"].as_i64().unwrap_or(3).clamp(1, 24),
        market_limit: scan["market_limit"].as_i64().unwrap_or(300).clamp(50, 500) as usize,
        target: engine["max_symbols_per_cycle"]
            .as_i64()
            .unwrap_or(12)
            .clamp(1, 100) as usize,
        concurrency: engine["scan_concurrency"]
            .as_i64()
            .unwrap_or(3)
            .clamp(1, 20) as usize,
        kline_limit: data["kline_limit"]
            .as_i64()
            .unwrap_or(200)
            .clamp(20, 500) as usize,
        atr_period: strategy["atr_period"].as_i64().unwrap_or(14).max(1) as usize,
        gates,
        base_url: data[endpoint]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| fallback.to_string()),
    })
}

fn current_pool(db: &crate::db::Db) -> Result<Vec<String>, String> {
    let conn = db.conn.lock().map_err(|_| "数据库锁不可用".to_string())?;
    let mut stmt = conn
        .prepare("SELECT symbol FROM trading_symbols WHERE enabled=1 ORDER BY sort_order, symbol")
        .map_err(|error| format!("读取标的池失败: {error}"))?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| format!("读取标的池失败: {error}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("读取标的池失败: {error}"))
}

fn all_pool_symbols(db: &crate::db::Db) -> Result<HashSet<String>, String> {
    let conn = db.conn.lock().map_err(|_| "数据库锁不可用".to_string())?;
    let mut stmt = conn
        .prepare("SELECT symbol FROM trading_symbols")
        .map_err(|error| format!("读取全部标的失败: {error}"))?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| format!("读取全部标的失败: {error}"))?;
    rows.collect::<Result<HashSet<_>, _>>()
        .map_err(|error| format!("读取全部标的失败: {error}"))
}

fn held_symbols(db: &crate::db::Db, mode: &str) -> Result<HashSet<String>, String> {
    let conn = db.conn.lock().map_err(|_| "数据库锁不可用".to_string())?;
    let mut stmt = conn
        .prepare("SELECT DISTINCT symbol FROM positions WHERE mode=?1 AND status='open'")
        .map_err(|error| format!("读取持仓失败: {error}"))?;
    let rows = stmt
        .query_map([mode], |row| row.get::<_, String>(0))
        .map_err(|error| format!("读取持仓失败: {error}"))?;
    rows.collect::<Result<HashSet<_>, _>>()
        .map_err(|error| format!("读取持仓失败: {error}"))
}

fn last_run_ts(db: &crate::db::Db) -> Result<i64, String> {
    let conn = db.conn.lock().map_err(|_| "数据库锁不可用".to_string())?;
    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM engine_state WHERE key=?1",
            ["symbol_scan.last_run_ts"],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| format!("读取上次换池时间失败: {error}"))?;
    Ok(raw
        .and_then(|value| value.trim().parse::<i64>().ok())
        .unwrap_or(0))
}

fn set_last_run_ts(db: &crate::db::Db, ts: i64) -> Result<(), String> {
    let conn = db.conn.lock().map_err(|_| "数据库锁不可用".to_string())?;
    upsert_state(&conn, "symbol_scan.last_run_ts", &ts.to_string())
}

/// 是否到期换池。`last_run_ts == 0` 表示尚未建立基线，不能算到期。
pub fn due(last_run_ts: i64, now: i64, interval_hours: i64) -> bool {
    last_run_ts > 0
        && now.saturating_sub(last_run_ts) >= interval_hours.max(1) * 3600
}

fn record_event(db: &crate::db::Db, level: &str, message: &str, payload: Value) {
    let Ok(conn) = db.conn.lock() else {
        return;
    };
    let _ = conn.execute(
        "INSERT INTO engine_events(ts,level,category,message,payload) VALUES(?1,?2,'symbol_scan',?3,?4)",
        (crate::now(), level, message, payload.to_string()),
    );
}

/// 独立后台换池循环：与交易引擎解耦，仅共享 SQLite 锁。
///
/// 首次启用不立即扫描，只落基线时间；满一个间隔后才自动换池。
pub fn spawn_loop(
    db: crate::db::Db,
    mut stop: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if *stop.borrow() {
                break;
            }
            match load_config(&db) {
                Ok(cfg) if cfg.enabled => {
                    let last = last_run_ts(&db).unwrap_or(0);
                    let now = crate::now();
                    if last == 0 {
                        if let Err(error) = set_last_run_ts(&db, now) {
                            record_event(
                                &db,
                                "warn",
                                "自动换池失败",
                                json!({ "error": error }),
                            );
                        }
                    } else if due(last, now, cfg.interval_hours) {
                        if let Err(error) = run(&db, true).await {
                            record_event(
                                &db,
                                "warn",
                                "自动换池失败",
                                json!({ "error": error }),
                            );
                        }
                    }
                }
                Ok(_) => {}
                Err(error) => {
                    record_event(&db, "warn", "自动换池失败", json!({ "error": error }));
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(tokio::time::Duration::from_secs(60)) => {}
                _ = stop.changed() => break,
            }
        }
    })
}

fn upsert_state(conn: &Connection, key: &str, value: &str) -> Result<(), String> {
    conn.execute(
        "INSERT INTO engine_state(key,value,updated_at) VALUES(?1,?2,?3) \
         ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
        (key, value, crate::now()),
    )
    .map(|_| ())
    .map_err(|error| format!("写入 engine_state.{key} 失败: {error}"))
}

fn plan_result(plan: &ScanPlan, scanned: usize, applied: bool) -> Value {
    json!({
        "scanned": scanned,
        "candidates": plan.candidates.clone(),
        "keep": plan.keep.clone(),
        "held": plan.held.clone(),
        "suspended": plan.suspended.clone(),
        "delete": plan.delete.clone(),
        // dry-run 无法预知二次持仓检查；null 表示不适用，apply 时由 merge_applied_result 写入。
        "delete_skipped": Value::Null,
        "add_skipped": Value::Null,
        "add": plan.add.clone(),
        "delete_reasons": plan
            .delete_reasons
            .iter()
            .map(|(symbol, reason)| json!({"symbol": symbol, "reason": reason}))
            .collect::<Vec<_>>(),
        "applied": applied,
    })
}

/// 用实际执行结果校正对外结果：删除原因只保留真正删除的标的，
/// 被二次持仓检查跳过的标的并入 keep，保证 keep ∪ delete ∪ add 可重建最终池。
fn merge_applied_result(mut result: Value, plan: &ScanPlan, applied: &AppliedPlan) -> Value {
    let deleted: HashSet<&str> = applied.deleted.iter().map(String::as_str).collect();
    let reasons: Vec<Value> = plan
        .delete_reasons
        .iter()
        .filter(|(symbol, _)| deleted.contains(symbol.as_str()))
        .map(|(symbol, reason)| json!({"symbol": symbol, "reason": reason}))
        .collect();
    let mut keep = plan.keep.clone();
    for symbol in &applied.skipped {
        if !keep.contains(symbol) {
            keep.push(symbol.clone());
        }
    }
    result["keep"] = json!(keep);
    result["delete"] = json!(applied.deleted);
    result["delete_reasons"] = json!(reasons);
    result["delete_skipped"] = json!(applied.skipped);
    result["add_skipped"] = json!(applied.add_skipped);
    result["add"] = json!(applied.added);
    result
}

/// `apply_plan` 的实际执行结果。删除前的二次持仓检查可能让计划落空，
/// 调用方应以本结果覆盖对外结果，避免宣称已删除。
#[derive(Debug, Clone, Default)]
struct AppliedPlan {
    deleted: Vec<String>,
    added: Vec<String>,
    /// 删除时因持仓被二次检查跳过的标的。
    skipped: Vec<String>,
    /// 新增时因已在池中（含手动禁用）而跳过的标的，与删除跳过分开。
    add_skipped: Vec<String>,
}

/// 把计划落到库里。删除前再查一次持仓，持仓标的跳过；写入新增标的。
/// 状态与事件由调用方在同一事务内负责，本函数不写。
fn apply_plan(
    conn: &Connection,
    mode: &str,
    plan: &ScanPlan,
    contracts: &[Value],
) -> Result<AppliedPlan, String> {
    let mut applied = AppliedPlan::default();
    for symbol in &plan.delete {
        let open: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM positions WHERE symbol=?1 AND mode=?2 AND status='open'",
                (symbol.as_str(), mode),
                |row| row.get(0),
            )
            .map_err(|error| format!("检查 {symbol} 持仓失败: {error}"))?;
        if open > 0 {
            applied.skipped.push(symbol.clone());
            continue;
        }
        conn.execute(
            "DELETE FROM trading_symbols WHERE symbol=?1",
            [symbol.as_str()],
        )
        .map_err(|error| format!("删除标的 {symbol} 失败: {error}"))?;
        applied.deleted.push(symbol.clone());
    }
    for symbol in &plan.add {
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM trading_symbols WHERE symbol=?1",
                [symbol.as_str()],
                |row| row.get(0),
            )
            .map_err(|error| format!("检查 {symbol} 是否已在池中失败: {error}"))?;
        if exists > 0 {
            applied.add_skipped.push(symbol.clone());
            continue;
        }
        match crate::symbols::upsert_contract_row_conn(conn, contracts, symbol) {
            Ok(_) => applied.added.push(symbol.clone()),
            // 并发窗口内被别的写入抢先时，同样按已存在跳过，不整轮回滚。
            Err(error) if error.contains("已在交易池中") => {
                applied.add_skipped.push(symbol.clone());
            }
            Err(error) => return Err(error),
        }
    }
    Ok(applied)
}

/// 执行一轮换池扫描。`apply=false` 只返回计划，不写库、不更新上次运行时间。
/// 行情或合约表读取失败直接返回错误，不修改标的池。
pub async fn run(db: &crate::db::Db, apply: bool) -> Result<Value, String> {
    let _busy = BusyGuard::acquire().ok_or_else(|| "自动换池正在执行".to_string())?;
    let cfg = load_config(db)?;
    let mode = crate::symbols::current_mode(db);

    let contracts = crate::symbols::exchange_contracts(db)
        .await
        .map_err(|(_, Json(body))| {
            body.get("detail")
                .and_then(Value::as_str)
                .unwrap_or("无法读取币安合约表")
                .to_string()
        })?;

    let provider = crate::providers::BinanceProvider::public(cfg.base_url.clone())
        .map_err(|error| format!("初始化行情客户端失败: {error:?}"))?;

    let current = current_pool(db)?;
    let current_set: HashSet<String> = current.iter().cloned().collect();
    let all_pool = all_pool_symbols(db)?;
    // 用户手动禁用的标的仍在池表里，不能进入候选，否则新增会主键冲突。
    let blocked: HashSet<String> = all_pool.difference(&current_set).cloned().collect();
    // 休市保护只针对当前池：全市场休市标的与池求交后才是本轮要保留的。
    let suspended: HashSet<String> = suspended_in_pool(&contracts, &current_set)
        .into_iter()
        .collect();

    let tickers = provider
        .tickers_24h()
        .await
        .map_err(|error| format!("读取 24h 成交额失败: {error:?}"))?;
    let books = provider
        .book_tops()
        .await
        .map_err(|error| format!("读取盘口失败: {error:?}"))?;

    // 与标的池写入共用同一套可交易规则。
    let tradable = tradable_symbols(&contracts);

    let mut ranked: Vec<&crate::providers::ExchangeTicker> = tickers
        .iter()
        .filter(|ticker| tradable.contains(&ticker.symbol) && ticker.quote_volume > 0.0)
        .collect();
    ranked.sort_by(|a, b| b.quote_volume.total_cmp(&a.quote_volume));
    ranked.truncate(cfg.market_limit);

    // 原池标的即使跌出 Top N 也要扫描，否则会被当作缺行情误删。
    let mut scan_symbols: Vec<String> = Vec::with_capacity(ranked.len() + current.len());
    let mut seen: HashSet<String> = HashSet::new();
    for ticker in &ranked {
        if seen.insert(ticker.symbol.clone()) {
            scan_symbols.push(ticker.symbol.clone());
        }
    }
    for symbol in &current {
        if seen.insert(symbol.clone()) {
            scan_symbols.push(symbol.clone());
        }
    }

    let ticker_map: HashMap<String, f64> = tickers
        .iter()
        .map(|ticker| (ticker.symbol.clone(), ticker.quote_volume))
        .collect();
    let book_map: HashMap<String, f64> = books
        .iter()
        .map(|book| (book.symbol.clone(), book.spread_bps))
        .collect();

    let mut scanned: Vec<ScannedSymbol> = Vec::with_capacity(scan_symbols.len());
    let mut queue = scan_symbols.into_iter();
    let mut set = tokio::task::JoinSet::new();
    loop {
        while set.len() < cfg.concurrency {
            let Some(symbol) = queue.next() else { break };
            let provider = provider.clone();
            let kline_limit = cfg.kline_limit;
            let atr_period = cfg.atr_period;
            let quote_volume = ticker_map.get(&symbol).copied().unwrap_or(0.0);
            let spread_bps = book_map.get(&symbol).copied().unwrap_or(f64::INFINITY);
            set.spawn(async move {
                let klines = provider.klines(&symbol, kline_limit).await;
                (symbol, klines, quote_volume, spread_bps, atr_period)
            });
        }
        let Some(joined) = set.join_next().await else {
            break;
        };
        let (symbol, klines, quote_volume, spread_bps, atr_period) = match joined {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(%error, "扫描任务异常退出");
                continue;
            }
        };
        // 单只标的失败不中断整轮，按缺数据处理后交给闸门。
        let metrics = klines
            .ok()
            .and_then(|rows| crate::providers::candle_metrics(&rows, atr_period));
        scanned.push(match metrics {
            Some(metrics) => ScannedSymbol {
                symbol,
                data_ok: true,
                vol_ratio: metrics.vol_ratio,
                atr_pct: metrics.atr_pct,
                quote_volume,
                spread_bps,
            },
            None => ScannedSymbol {
                symbol,
                data_ok: false,
                vol_ratio: 0.0,
                atr_pct: 0.0,
                quote_volume,
                spread_bps,
            },
        });
    }

    let scanned = exclude_blocked(scanned, &blocked);

    let held = held_symbols(db, &mode)?;
    let plan = plan(&current, &held, &suspended, &scanned, &cfg.gates, cfg.target);

    if !apply {
        return Ok(plan_result(&plan, scanned.len(), false));
    }

    let now = crate::now();
    let result = {
        // 锁只覆盖同步落库段，不跨 await，避免 future 失去 Send。
        let conn = db.conn.lock().map_err(|_| "数据库锁不可用".to_string())?;
        let tx = conn
            .unchecked_transaction()
            .map_err(|error| format!("开启换池事务失败: {error}"))?;
        // 二次持仓检查可能跳过删除，对外结果以实际执行列表为准。
        let applied = apply_plan(&tx, &mode, &plan, &contracts)?;
        let replaced = applied.deleted.len();
        let added = applied.added.len();
        let result =
            merge_applied_result(plan_result(&plan, scanned.len(), true), &plan, &applied);
        let message = format!("自动换池：替换 {replaced} 个，新增 {added} 个");
        let payload = result.to_string();
        upsert_state(&tx, "symbol_scan.last_run_ts", &now.to_string())?;
        upsert_state(&tx, "symbol_scan.last_result", &payload)?;
        tx.execute(
            "INSERT INTO engine_events(ts,level,category,message,payload) VALUES(?1,'info','symbol_scan',?2,?3)",
            (now, message, payload),
        )
        .map_err(|error| format!("写入换池事件失败: {error}"))?;
        tx.commit()
            .map_err(|error| format!("提交换池事务失败: {error}"))?;
        result
    };
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use rusqlite::Connection;
    use serde_json::json;

    fn memory_db(settings: &[(&str, serde_json::Value)]) -> crate::db::Db {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE app_settings(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER);
             CREATE TABLE engine_state(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER);",
        )
        .unwrap();
        for (key, value) in settings {
            conn.execute(
                "INSERT INTO app_settings(key,value,updated_at) VALUES(?1,?2,0)",
                (key, value.to_string()),
            )
            .unwrap();
        }
        crate::db::Db {
            path: ":memory:".to_string(),
            conn: Arc::new(Mutex::new(conn)),
        }
    }

    #[test]
    fn first_run_is_delayed_until_interval_elapsed() {
        let now = 1_000_000;
        assert!(!due(0, now, 3));
        assert!(due(now - 3 * 3600, now, 3));
        assert!(!due(now - 2 * 3600, now, 3));
        assert!(due(now - 3600, now, 1));
        assert!(!due(now - 3600, now, 2));
    }

    #[test]
    fn record_event_writes_symbol_scan_warning() {
        let db = memory_db(&[]);
        {
            let conn = db.conn.lock().unwrap();
            conn.execute_batch(
                "CREATE TABLE engine_events(ts INTEGER, level TEXT, category TEXT, message TEXT, payload TEXT);",
            )
            .unwrap();
        }
        record_event(
            &db,
            "warn",
            "自动换池失败",
            json!({"error": "network down"}),
        );
        let conn = db.conn.lock().unwrap();
        let row: (String, String, String, String) = conn
            .query_row(
                "SELECT level, category, message, payload FROM engine_events",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(row.0, "warn");
        assert_eq!(row.1, "symbol_scan");
        assert_eq!(row.2, "自动换池失败");
        let payload: Value = serde_json::from_str(&row.3).unwrap();
        assert_eq!(payload["error"], "network down");
    }

    fn contract_with(symbol: &str, contract_type: &str, status: &str, quote: &str) -> Value {
        json!({
            "symbol": symbol,
            "baseAsset": symbol.strip_suffix("USDT").unwrap_or(symbol),
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

    fn contract(symbol: &str) -> Value {
        contract_with(symbol, "PERPETUAL", "TRADING", "USDT")
    }

    #[test]
    fn apply_plan_keeps_held_symbols_and_replaces_the_rest() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE app_settings(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER);
             CREATE TABLE trading_symbols(symbol TEXT PRIMARY KEY, name TEXT, tier TEXT, sl_atr_mult REAL,
               enabled INTEGER, max_leverage INTEGER, precision INTEGER, tick_sz TEXT, min_sz TEXT,
               min_notional REAL, ct_val REAL, min_score REAL, note TEXT, sort_order INTEGER,
               created_at INTEGER, updated_at INTEGER);
             CREATE TABLE positions(symbol TEXT, status TEXT, mode TEXT);
             INSERT INTO app_settings VALUES('risk','{\"max_leverage\":50}',0);
             INSERT INTO trading_symbols(symbol,name,tier,sl_atr_mult,enabled,max_leverage,precision,tick_sz,min_sz,min_notional,ct_val,min_score,note,sort_order,created_at,updated_at)
               VALUES('AAAUSDT','AAA','tier_2_momentum',2.2,1,20,3,'0.001','0.001',5.0,1.0,0.0,'',1,1,1);
             INSERT INTO trading_symbols(symbol,name,tier,sl_atr_mult,enabled,max_leverage,precision,tick_sz,min_sz,min_notional,ct_val,min_score,note,sort_order,created_at,updated_at)
               VALUES('BBBUSDT','BBB','tier_2_momentum',2.2,1,20,3,'0.001','0.001',5.0,1.0,0.0,'',2,1,1);
             INSERT INTO positions VALUES('BBBUSDT','open','live');",
        )
        .unwrap();
        let plan = ScanPlan {
            keep: vec!["BBBUSDT".into()],
            delete: vec!["AAAUSDT".into()],
            add: vec!["CCCUSDT".into()],
            held: vec!["BBBUSDT".into()],
            suspended: vec![],
            candidates: vec!["CCCUSDT".into()],
            delete_reasons: vec![("AAAUSDT".into(), "量能不足".into())],
        };
        let contracts = vec![contract("CCCUSDT")];
        let applied = apply_plan(&conn, "live", &plan, &contracts).unwrap();
        assert_eq!(applied.deleted, vec!["AAAUSDT"]);
        assert_eq!(applied.added, vec!["CCCUSDT"]);
        assert!(applied.skipped.is_empty());
        assert!(applied.add_skipped.is_empty());
        let symbols: Vec<String> = conn
            .prepare("SELECT symbol FROM trading_symbols ORDER BY sort_order, symbol")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(symbols, vec!["BBBUSDT", "CCCUSDT"]);
        let spec: (String, i64, String, String, f64, i64) = conn
            .query_row(
                "SELECT tier, max_leverage, tick_sz, min_sz, min_notional, precision \
                 FROM trading_symbols WHERE symbol='CCCUSDT'",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            spec,
            (
                "tier_2_momentum".to_string(),
                50,
                "0.010".to_string(),
                "0.0010".to_string(),
                5.0,
                3
            ),
            "新增标的的规格必须按合约表写入"
        );
    }

    #[test]
    fn apply_plan_skips_existing_symbols_without_overwrite() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE app_settings(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER);
             CREATE TABLE trading_symbols(symbol TEXT PRIMARY KEY, name TEXT, tier TEXT, sl_atr_mult REAL,
               enabled INTEGER, max_leverage INTEGER, precision INTEGER, tick_sz TEXT, min_sz TEXT,
               min_notional REAL, ct_val REAL, min_score REAL, note TEXT, sort_order INTEGER,
               created_at INTEGER, updated_at INTEGER);
             CREATE TABLE positions(symbol TEXT, status TEXT, mode TEXT);
             INSERT INTO app_settings VALUES('risk','{\"max_leverage\":50}',0);
             INSERT INTO trading_symbols(symbol,name,tier,sl_atr_mult,enabled,max_leverage,precision,tick_sz,min_sz,min_notional,ct_val,min_score,note,sort_order,created_at,updated_at)
               VALUES('CCCUSDT','CCC','tier_1_bluechip',1.8,0,20,2,'0.1','0.01',10.0,1.0,0.0,'手动禁用',5,1,1);",
        )
        .unwrap();
        let plan = ScanPlan {
            keep: vec![],
            delete: vec![],
            add: vec!["CCCUSDT".into()],
            held: vec![],
            suspended: vec![],
            candidates: vec!["CCCUSDT".into()],
            delete_reasons: vec![],
        };
        let applied = apply_plan(&conn, "live", &plan, &[contract("CCCUSDT")]).unwrap();
        assert!(applied.added.is_empty(), "已存在的标的不能重复写入");
        assert_eq!(applied.add_skipped, vec!["CCCUSDT"]);
        assert!(applied.deleted.is_empty(), "删除跳过与新增跳过不能混用");
        assert!(applied.skipped.is_empty(), "删除跳过与新增跳过不能混用");

        let (enabled, tier, note, count): (i64, String, String, i64) = conn
            .query_row(
                "SELECT enabled, tier, note, (SELECT COUNT(*) FROM trading_symbols) \
                 FROM trading_symbols WHERE symbol='CCCUSDT'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(enabled, 0, "不能自动重新启用用户手动禁用的标的");
        assert_eq!(tier, "tier_1_bluechip");
        assert_eq!(note, "手动禁用");
        assert_eq!(count, 1, "最终池不变");
    }

    #[test]
    fn apply_plan_skips_delete_when_position_is_open() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE trading_symbols(symbol TEXT PRIMARY KEY, enabled INTEGER NOT NULL);
             CREATE TABLE positions(symbol TEXT, status TEXT, mode TEXT);
             INSERT INTO trading_symbols VALUES('AAAUSDT',1);
             INSERT INTO trading_symbols VALUES('BBBUSDT',1);
             INSERT INTO positions VALUES('AAAUSDT','open','live');",
        )
        .unwrap();
        let plan = ScanPlan {
            keep: vec![],
            delete: vec!["AAAUSDT".into()],
            add: vec![],
            held: vec!["AAAUSDT".into()],
            suspended: vec![],
            candidates: vec![],
            delete_reasons: vec![("AAAUSDT".into(), "量能不足".into())],
        };
        let applied = apply_plan(&conn, "live", &plan, &[]).unwrap();
        assert!(applied.deleted.is_empty(), "有持仓的标的实际未删除");
        assert_eq!(applied.skipped, vec!["AAAUSDT"]);
        assert!(
            applied.add_skipped.is_empty(),
            "删除跳过与新增跳过不能混用"
        );
        let symbols: Vec<String> = conn
            .prepare("SELECT symbol FROM trading_symbols ORDER BY symbol")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            symbols,
            vec!["AAAUSDT", "BBBUSDT"],
            "有持仓的标的不能被二次检查删除"
        );
    }

    #[test]
    fn merge_applied_result_aligns_reasons_and_keeps_skipped() {
        let plan = ScanPlan {
            keep: vec!["BBBUSDT".into()],
            delete: vec!["AAAUSDT".into(), "DDDUSDT".into()],
            add: vec!["CCCUSDT".into()],
            held: vec!["BBBUSDT".into()],
            suspended: vec![],
            candidates: vec!["CCCUSDT".into()],
            delete_reasons: vec![
                ("AAAUSDT".into(), "量能不足".into()),
                ("DDDUSDT".into(), "缺少行情数据".into()),
            ],
        };
        let applied = AppliedPlan {
            deleted: vec!["AAAUSDT".into()],
            added: vec!["CCCUSDT".into()],
            skipped: vec!["DDDUSDT".into()],
            add_skipped: vec!["EEEUSDT".into()],
        };
        let result = merge_applied_result(plan_result(&plan, 4, true), &plan, &applied);
        assert_eq!(result["delete"], json!(["AAAUSDT"]));
        assert_eq!(result["delete_skipped"], json!(["DDDUSDT"]));
        assert_eq!(
            result["add_skipped"],
            json!(["EEEUSDT"]),
            "新增跳过与删除跳过必须分开呈现"
        );
        assert_eq!(
            result["delete_reasons"],
            json!([{"symbol": "AAAUSDT", "reason": "量能不足"}]),
            "删除原因必须与实际的 delete 一一对应"
        );
        let keep: Vec<String> = serde_json::from_value(result["keep"].clone()).unwrap();
        assert_eq!(keep, vec!["BBBUSDT", "DDDUSDT"], "跳过的标的不再丢失");
        assert_eq!(result["add"], json!(["CCCUSDT"]));
        assert_eq!(result["applied"], json!(true));
    }

    #[test]
    fn busy_guard_rejects_concurrent_acquisition_and_releases_on_drop() {
        let first = BusyGuard::acquire().expect("首次应获取");
        assert!(BusyGuard::acquire().is_none(), "执行中不能重复获取");
        drop(first);
        assert!(BusyGuard::acquire().is_some(), "释放后应可再次获取");
    }

    #[test]
    fn tradable_and_suspended_contracts_align_with_pool_rules() {
        let contracts = vec![
            contract_with("AAAUSDT", "PERPETUAL", "TRADING", "USDT"),
            contract_with("NVDAUSDT", "TRADIFI_PERPETUAL", "BREAK", "USDT"),
            contract_with("ETHUSDT", "PERPETUAL", "BREAK", "USDT"),
            contract_with("BADUSDT", "PERPETUAL", "TRADING", "BUSD"),
            contract_with("NVDLUSDT", "TRADIFI_PERPETUAL", "TRADING", "USDT"),
        ];

        let tradable = tradable_symbols(&contracts);
        assert!(tradable.contains("AAAUSDT"));
        assert!(
            tradable.contains("NVDAUSDT"),
            "休市的股票类永续仍属可交易（本轮保留）"
        );
        assert!(tradable.contains("NVDLUSDT"));
        assert!(
            !tradable.contains("ETHUSDT"),
            "非 TRADIFI 的 BREAK 合约不可交易"
        );
        assert!(!tradable.contains("BADUSDT"), "非 USDT 报价不可交易");

        let suspended = suspended_symbols(&contracts);
        let mut suspended_list: Vec<&str> = suspended.iter().map(String::as_str).collect();
        suspended_list.sort();
        assert_eq!(
            suspended_list,
            vec!["NVDAUSDT"],
            "只有 TRADIFI_PERPETUAL + BREAK 算休市"
        );

        // 休市标的没有 K 线，但 suspended 优先保留，不依赖 held。
        let current = vec!["NVDAUSDT".to_string()];
        let suspended_set: HashSet<String> = suspended.clone();
        let no_data = plan(
            &current,
            &HashSet::new(),
            &suspended_set,
            &[],
            &gates(),
            12,
        );
        assert_eq!(no_data.keep, vec!["NVDAUSDT"]);
        assert_eq!(no_data.suspended, vec!["NVDAUSDT"]);
        assert!(no_data.held.is_empty(), "休市不等于持仓");
        assert!(no_data.delete.is_empty(), "休市标的不能被替换");
    }

    #[test]
    fn suspended_is_limited_to_current_pool() {
        let contracts = vec![
            contract_with("NVDAUSDT", "TRADIFI_PERPETUAL", "BREAK", "USDT"),
            contract_with("NVDLUSDT", "TRADIFI_PERPETUAL", "BREAK", "USDT"),
            contract_with("ETHUSDT", "PERPETUAL", "TRADING", "USDT"),
        ];
        let current: HashSet<String> = ["ETHUSDT".to_string(), "NVDAUSDT".to_string()]
            .into_iter()
            .collect();
        assert_eq!(
            suspended_in_pool(&contracts, &current),
            vec!["NVDAUSDT"],
            "全市场休市集合只保留当前池内的标的"
        );
    }

    #[test]
    fn blocked_symbols_are_excluded_from_candidates() {
        let blocked: HashSet<String> = ["CCCUSDT".to_string()].into_iter().collect();
        let rows = vec![scanned("AAAUSDT", 1.0), scanned("CCCUSDT", 3.0)];
        let kept = exclude_blocked(rows, &blocked);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].symbol, "AAAUSDT");

        let current = owned(&["AAAUSDT"]);
        let plan = plan(
            &current,
            &HashSet::new(),
            &HashSet::new(),
            &kept,
            &gates(),
            2,
        );
        assert!(plan.candidates.is_empty(), "禁用标的不能成为候选");
        assert!(plan.add.is_empty(), "禁用标的不能被新增");
    }

    #[test]
    fn load_config_uses_defaults_when_groups_are_missing() {
        let db = memory_db(&[]);
        let cfg = load_config(&db).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.interval_hours, 3);
        assert_eq!(cfg.market_limit, 300);
        assert_eq!(cfg.target, 12);
        assert_eq!(cfg.concurrency, 3);
        assert_eq!(cfg.kline_limit, 200);
        assert_eq!(cfg.atr_period, 14);
        assert_eq!(cfg.gates.spread_bps_max, 8.0);
        assert_eq!(cfg.base_url, "https://testnet.binancefuture.com");
    }

    #[test]
    fn load_config_clamps_and_follows_mode() {
        let db = memory_db(&[
            (
                "symbol_scan",
                json!({"enabled": false, "interval_hours": 99, "market_limit": 10}),
            ),
            (
                "engine",
                json!({"mode": "live", "max_symbols_per_cycle": 200, "scan_concurrency": 0}),
            ),
            (
                "strategy",
                json!({"atr_period": 20, "max_spread_bps": 5.0, "entry_threshold": 0.5}),
            ),
            (
                "data",
                json!({"kline_limit": 1000, "live_base_url": "https://example.test"}),
            ),
        ]);
        let cfg = load_config(&db).unwrap();
        assert!(!cfg.enabled);
        assert_eq!(cfg.interval_hours, 24);
        assert_eq!(cfg.market_limit, 50);
        assert_eq!(cfg.target, 100);
        assert_eq!(cfg.concurrency, 1);
        assert_eq!(cfg.kline_limit, 500);
        assert_eq!(cfg.atr_period, 20);
        assert_eq!(cfg.gates.spread_bps_max, 5.0);
        assert_eq!(cfg.gates.entry_threshold, 0.5);
        assert_eq!(cfg.base_url, "https://example.test");
    }

    #[test]
    fn load_config_accepts_legacy_spread_key() {
        let db = memory_db(&[("strategy", json!({"spread_bps_max": 6.0}))]);
        assert_eq!(load_config(&db).unwrap().gates.spread_bps_max, 6.0);
    }

    fn gates() -> ScanGates {
        ScanGates {
            atr_pct_min: 0.05,
            atr_pct_max: 4.0,
            volume_ratio_min: 0.8,
            min_24h_quote_volume: 3_000_000.0,
            spread_bps_max: 8.0,
            entry_threshold: 0.3,
        }
    }

    fn scanned(symbol: &str, vol: f64) -> ScannedSymbol {
        ScannedSymbol {
            symbol: symbol.to_string(),
            data_ok: true,
            vol_ratio: vol,
            atr_pct: 1.0,
            quote_volume: 1e8,
            spread_bps: 2.0,
        }
    }

    fn owned(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn all_current_pass_keeps_everything_without_replacements() {
        let current = owned(&["AAAUSDT", "BBBUSDT"]);
        let rows = vec![scanned("AAAUSDT", 1.5), scanned("BBBUSDT", 2.0)];
        let plan = plan(&current, &HashSet::new(), &HashSet::new(), &rows, &gates(), 2);

        assert_eq!(plan.keep, owned(&["AAAUSDT", "BBBUSDT"]));
        assert!(plan.delete.is_empty(), "全部通过不应删除任何标的");
        assert!(plan.add.is_empty(), "全部通过不应新增任何标的");
        assert!(plan.held.is_empty());
        assert!(plan.candidates.is_empty(), "原池标的不能算候选");
    }

    #[test]
    fn failing_current_symbols_are_replaced_by_top_candidates() {
        let current = owned(&["AAAUSDT", "BBBUSDT"]);
        let rows = vec![
            scanned("AAAUSDT", 1.5),
            scanned("BBBUSDT", 0.5),
            scanned("CCCUSDT", 1.0),
            scanned("DDDUSDT", 2.0),
        ];
        let plan = plan(&current, &HashSet::new(), &HashSet::new(), &rows, &gates(), 2);

        assert_eq!(plan.keep, owned(&["AAAUSDT"]));
        assert_eq!(plan.delete, owned(&["BBBUSDT"]));
        assert_eq!(plan.add, owned(&["DDDUSDT"]), "应补入量能最强的候选");
        assert_eq!(plan.candidates, owned(&["DDDUSDT", "CCCUSDT"]));
        assert!(plan.held.is_empty());
    }

    #[test]
    fn held_symbols_are_kept_even_when_they_fail_the_gate() {
        let current = owned(&["AAAUSDT", "BBBUSDT"]);
        let rows = vec![scanned("AAAUSDT", 1.5), scanned("BBBUSDT", 0.5)];
        let held: HashSet<String> = ["BBBUSDT".to_string()].into_iter().collect();
        let plan = plan(&current, &held, &HashSet::new(), &rows, &gates(), 2);

        assert_eq!(plan.keep, owned(&["AAAUSDT", "BBBUSDT"]));
        assert_eq!(plan.held, owned(&["BBBUSDT"]));
        assert!(plan.delete.is_empty(), "持仓标的永不删除");
        assert!(plan.add.is_empty());
    }

    #[test]
    fn suspended_symbols_are_kept_without_counting_as_held() {
        let current = owned(&["AAAUSDT", "NVDAUSDT"]);
        let held: HashSet<String> = ["AAAUSDT".to_string(), "NVDAUSDT".to_string()]
            .into_iter()
            .collect();
        let suspended: HashSet<String> = ["NVDAUSDT".to_string()].into_iter().collect();
        let plan = plan(&current, &held, &suspended, &[], &gates(), 2);

        assert_eq!(plan.keep, owned(&["AAAUSDT", "NVDAUSDT"]));
        assert_eq!(plan.held, owned(&["AAAUSDT"]), "休市标的不能混入 held");
        assert_eq!(plan.suspended, owned(&["NVDAUSDT"]));
        assert!(plan.delete.is_empty());
    }

    #[test]
    fn short_pool_is_filled_to_target_from_candidates() {
        let current = owned(&["AAAUSDT"]);
        let rows = vec![
            scanned("AAAUSDT", 1.5),
            scanned("BBBUSDT", 2.0),
            scanned("CCCUSDT", 1.0),
        ];
        let plan = plan(&current, &HashSet::new(), &HashSet::new(), &rows, &gates(), 3);

        assert_eq!(plan.keep, owned(&["AAAUSDT"]));
        assert!(plan.delete.is_empty());
        assert_eq!(plan.add, owned(&["BBBUSDT", "CCCUSDT"]));
        assert_eq!(plan.candidates, owned(&["BBBUSDT", "CCCUSDT"]));
    }

    #[test]
    fn insufficient_candidates_only_replace_what_is_available() {
        let current = owned(&["AAAUSDT", "BBBUSDT", "CCCUSDT"]);
        let rows = vec![
            scanned("AAAUSDT", 0.5),
            scanned("BBBUSDT", 0.5),
            scanned("CCCUSDT", 0.5),
            scanned("DDDUSDT", 1.0),
        ];
        let plan = plan(&current, &HashSet::new(), &HashSet::new(), &rows, &gates(), 3);

        assert_eq!(plan.keep, owned(&["BBBUSDT", "CCCUSDT"]));
        assert_eq!(plan.delete, owned(&["AAAUSDT"]));
        assert_eq!(plan.add, owned(&["DDDUSDT"]));
        assert_eq!(plan.candidates, owned(&["DDDUSDT"]));
        assert!(
            !plan.delete.contains(&"BBBUSDT".to_string())
                && !plan.delete.contains(&"CCCUSDT".to_string()),
            "候选不足时剩余不达标标的不应进入删除"
        );
    }

    #[test]
    fn oversized_passing_pool_is_not_trimmed() {
        let current = owned(&["AAAUSDT", "BBBUSDT", "CCCUSDT"]);
        let rows = vec![
            scanned("AAAUSDT", 1.0),
            scanned("BBBUSDT", 1.0),
            scanned("CCCUSDT", 1.0),
        ];
        let plan = plan(&current, &HashSet::new(), &HashSet::new(), &rows, &gates(), 2);

        assert_eq!(plan.keep, owned(&["AAAUSDT", "BBBUSDT", "CCCUSDT"]));
        assert!(plan.delete.is_empty());
        assert!(plan.add.is_empty());
    }

    #[test]
    fn full_gate_applies_to_pool_and_candidates() {
        let current = owned(&["AAAUSDT", "BBBUSDT", "CCCUSDT"]);
        let mut high_atr = scanned("AAAUSDT", 1.0);
        high_atr.atr_pct = 5.0;
        let mut thin = scanned("BBBUSDT", 1.0);
        thin.quote_volume = 2_000_000.0;
        let mut wide = scanned("CCCUSDT", 1.0);
        wide.spread_bps = 9.0;
        let mut bad_candidate = scanned("EEEUSDT", 5.0);
        bad_candidate.spread_bps = 9.0;
        let rows = vec![high_atr, thin, wide, scanned("DDDUSDT", 1.0), bad_candidate];
        let plan = plan(&current, &HashSet::new(), &HashSet::new(), &rows, &gates(), 3);

        assert_eq!(plan.keep, owned(&["BBBUSDT", "CCCUSDT"]));
        assert_eq!(plan.delete, owned(&["AAAUSDT"]));
        assert_eq!(plan.add, owned(&["DDDUSDT"]));
        assert_eq!(plan.candidates, owned(&["DDDUSDT"]), "点差过大的候选必须被剔除");
    }

    #[test]
    fn missing_scanned_symbol_is_deleted_with_reason() {
        let current = owned(&["AAAUSDT"]);
        let rows = vec![scanned("DDDUSDT", 1.0)];
        let plan = plan(&current, &HashSet::new(), &HashSet::new(), &rows, &gates(), 1);

        assert!(plan.keep.is_empty());
        assert_eq!(plan.delete, owned(&["AAAUSDT"]));
        assert_eq!(
            plan.delete_reasons,
            vec![("AAAUSDT".to_string(), "缺少行情数据".to_string())]
        );
        assert_eq!(plan.add, owned(&["DDDUSDT"]));
    }

    #[test]
    fn delete_reasons_capture_all_gate_failures() {
        let current = owned(&["AAAUSDT", "BBBUSDT", "CCCUSDT"]);
        let mut high_atr = scanned("AAAUSDT", 1.0);
        high_atr.atr_pct = 5.0;
        let mut thin = scanned("BBBUSDT", 1.0);
        thin.quote_volume = 2_000_000.0;
        let mut wide = scanned("CCCUSDT", 1.0);
        wide.spread_bps = 9.0;
        let rows = vec![
            high_atr,
            thin,
            wide,
            scanned("DDDUSDT", 3.0),
            scanned("EEEUSDT", 2.0),
            scanned("FFFUSDT", 1.5),
        ];
        let plan = plan(&current, &HashSet::new(), &HashSet::new(), &rows, &gates(), 3);

        assert_eq!(plan.delete, owned(&["AAAUSDT", "BBBUSDT", "CCCUSDT"]));
        assert_eq!(plan.delete_reasons.len(), 3);
        assert!(
            plan.delete_reasons[0].1.contains("波动率过高"),
            "实际原因: {:?}",
            plan.delete_reasons[0].1
        );
        assert_eq!(plan.delete_reasons[1].1, "24h 成交额偏低");
        assert!(
            plan.delete_reasons[2].1.contains("盘口点差过大"),
            "实际原因: {:?}",
            plan.delete_reasons[2].1
        );
        assert_eq!(plan.add, owned(&["DDDUSDT", "EEEUSDT", "FFFUSDT"]));
    }

    #[test]
    fn duplicate_scanned_symbols_are_deduplicated() {
        let current = owned(&["AAAUSDT"]);
        let rows = vec![
            scanned("BBBUSDT", 1.0),
            scanned("BBBUSDT", 2.0),
            scanned("CCCUSDT", 3.0),
        ];
        let plan = plan(&current, &HashSet::new(), &HashSet::new(), &rows, &gates(), 3);

        assert_eq!(plan.candidates, owned(&["CCCUSDT", "BBBUSDT"]));
        assert_eq!(plan.add, owned(&["CCCUSDT", "BBBUSDT"]));
        let unique: HashSet<&String> = plan.candidates.iter().collect();
        assert_eq!(unique.len(), plan.candidates.len(), "候选不得重复");
    }

    #[test]
    fn duplicate_current_symbols_are_deduplicated() {
        let current = owned(&["AAAUSDT", "AAAUSDT", "BBBUSDT"]);
        let rows = vec![
            scanned("AAAUSDT", 0.5),
            scanned("BBBUSDT", 1.5),
            scanned("CCCUSDT", 1.8),
            scanned("DDDUSDT", 2.0),
        ];
        let deduped = plan(&current, &HashSet::new(), &HashSet::new(), &rows, &gates(), 2);

        assert_eq!(deduped.keep, owned(&["BBBUSDT"]));
        assert_eq!(deduped.delete, owned(&["AAAUSDT"]), "AAA 只能删除一次");
        assert_eq!(deduped.delete_reasons.len(), 1);
        assert_eq!(deduped.delete_reasons[0].0, "AAAUSDT");
        assert_eq!(deduped.add, owned(&["DDDUSDT"]));

        // 全部通过时，重复的原池标的在 keep 中同样只出现一次。
        let current = owned(&["AAAUSDT", "AAAUSDT"]);
        let rows = vec![scanned("AAAUSDT", 1.0)];
        let all_pass = plan(&current, &HashSet::new(), &HashSet::new(), &rows, &gates(), 1);
        assert_eq!(all_pass.keep, owned(&["AAAUSDT"]));
    }

    #[test]
    fn candidates_are_sorted_by_volume_ratio_desc() {
        let current = owned(&["AAAUSDT"]);
        let rows = vec![
            scanned("AAAUSDT", 1.0),
            scanned("BBBUSDT", 1.2),
            scanned("CCCUSDT", 3.0),
            scanned("DDDUSDT", 2.0),
        ];
        let plan = plan(&current, &HashSet::new(), &HashSet::new(), &rows, &gates(), 5);

        assert_eq!(plan.keep, owned(&["AAAUSDT"]));
        assert_eq!(plan.candidates, owned(&["CCCUSDT", "DDDUSDT", "BBBUSDT"]));
        assert_eq!(plan.add, owned(&["CCCUSDT", "DDDUSDT", "BBBUSDT"]));
    }

    #[test]
    fn empty_input_and_zero_target_are_handled() {
        let first = plan(&[], &HashSet::new(), &HashSet::new(), &[], &gates(), 0);
        assert!(first.keep.is_empty());
        assert!(first.delete.is_empty());
        assert!(first.add.is_empty());
        assert!(first.held.is_empty());
        assert!(first.candidates.is_empty());
        assert!(first.delete_reasons.is_empty());

        // target = 0 且全部通过时不裁减合格标的。
        let current = owned(&["AAAUSDT"]);
        let rows = vec![scanned("AAAUSDT", 1.0)];
        let plan = plan(&current, &HashSet::new(), &HashSet::new(), &rows, &gates(), 0);
        assert_eq!(plan.keep, owned(&["AAAUSDT"]));
        assert!(plan.delete.is_empty());
        assert!(plan.add.is_empty());
    }
}
