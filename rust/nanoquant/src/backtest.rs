//! 回测接口：参数校验、历史行情拉取、撮合执行、结果持久化、资源上限与取消。
//!
//! 对齐 `api/routes/backtest.py` 的契约，同时补齐 legacy 内核（已退役） 版本没有的持久化与取消能力。
//! 周期不在白名单、网格越界、组合数/并发数超限都在拉 K 线或执行前拒绝。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::{Json, Router};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::backtest_engine::{self, EngineConfig};
use crate::db::Db;
use crate::history::{self, HistorySeries, HistorySource};
use crate::settings_api;

const INTERVALS: [&str; 6] = ["5m", "15m", "30m", "1h", "4h", "1d"];
const MAX_CONCURRENT_RUNS: usize = 2;
const MAX_PERSISTED_RUNS: usize = 200;

const BOUNDS: [(&str, f64, f64); 4] = [
    ("entry_threshold", 0.05, 0.95),
    ("sl_atr_mult", 0.2, 10.0),
    ("tp_atr_mult", 0.2, 20.0),
    ("trailing_pct", 0.0, 200.0),
];

pub fn check_interval(interval: &str) -> Result<(), String> {
    if INTERVALS.contains(&interval) {
        Ok(())
    } else {
        Err(format!("周期只支持 {INTERVALS:?}"))
    }
}

/// 每个参数的取值必须落在区间内，且至少要给一个参数一组取值。
pub fn check_grid(grid: &[(&str, &[f64])]) -> Result<(), String> {
    if grid.is_empty() {
        return Err("至少需要给一个参数一组取值".to_string());
    }
    for (key, values) in grid {
        let Some((_, low, high)) = BOUNDS.iter().find(|(k, _, _)| k == key) else {
            return Err(format!("{key} 不是可扫描的参数"));
        };
        for value in *values {
            if !(low <= value && value <= high) {
                return Err(format!("{key}={value} 超出允许范围 [{low}, {high}]"));
            }
        }
    }
    Ok(())
}

/// 请求里显式给了手续费就用，否则把实盘 taker 费率（比例）换算成 bps。
pub fn fee_bps(explicit: Option<f64>, taker_rate: f64) -> f64 {
    explicit.unwrap_or_else(|| (taker_rate * 10_000.0 * 10_000.0).round() / 10_000.0)
}

pub fn routes() -> Router<Db> {
    Router::new()
        .route("/backtest/run", axum::routing::post(run))
        .route("/backtest/grid", axum::routing::post(grid))
        .route("/backtest/walkforward", axum::routing::post(walkforward))
        .route("/backtest/runs", axum::routing::get(list_runs))
        .route("/backtest/runs/{id}", axum::routing::get(get_run))
        .route(
            "/backtest/runs/{id}/cancel",
            axum::routing::post(cancel_run),
        )
}

#[derive(Deserialize, Serialize, Clone)]
struct BacktestIn {
    symbol: String,
    #[serde(default = "default_interval")]
    interval: String,
    #[serde(default = "default_limit")]
    limit: i64,
    fee_bps: Option<f64>,
    #[serde(default = "default_slippage")]
    slippage_bps: f64,
    #[serde(default)]
    spread_bps: f64,
    #[serde(default = "default_funding")]
    funding_bps_8h: f64,
    #[serde(default = "default_equity")]
    initial_equity: f64,
    #[serde(default)]
    run_id: Option<String>,
}

fn default_interval() -> String {
    "15m".into()
}
fn default_limit() -> i64 {
    1000
}
fn default_slippage() -> f64 {
    2.0
}
fn default_funding() -> f64 {
    1.0
}
fn default_equity() -> f64 {
    100.0
}

#[derive(Deserialize, Clone)]
struct BacktestGridIn {
    #[serde(flatten)]
    base: BacktestIn,
    #[serde(default)]
    entry_threshold: Vec<f64>,
    #[serde(default)]
    sl_atr_mult: Vec<f64>,
    #[serde(default)]
    tp_atr_mult: Vec<f64>,
    #[serde(default)]
    trailing_pct: Vec<f64>,
    #[serde(default = "default_folds")]
    folds: i64,
    #[serde(default = "default_train_bars")]
    train_bars: i64,
}
fn default_folds() -> i64 {
    3
}
fn default_train_bars() -> i64 {
    600
}

struct Prepared {
    symbol: String,
    live: bool,
    strategy: Value,
    risk: Value,
}

fn setting(conn: &Connection, key: &str) -> Value {
    conn.query_row(
        "SELECT value FROM app_settings WHERE key=?1",
        [key],
        |row| row.get::<_, String>(0),
    )
    .ok()
    .and_then(|raw| serde_json::from_str(&raw).ok())
    .unwrap_or_else(|| settings_api::defaults_of(key))
}

fn engine_mode(conn: &Connection) -> String {
    conn.query_row(
        "SELECT value FROM engine_state WHERE key='mode'",
        [],
        |row| row.get::<_, String>(0),
    )
    .unwrap_or_else(|_| "demo".to_string())
}

fn prepare(db: &Db, input: &BacktestIn) -> Result<Prepared, (StatusCode, Json<Value>)> {
    let conn = db.conn.lock().map_err(|_| internal("数据库锁不可用"))?;
    let symbol = validate_base(&conn, input)?;
    Ok(Prepared {
        symbol,
        live: matches!(engine_mode(&conn).as_str(), "live" | "mainnet"),
        strategy: setting(&conn, "strategy"),
        risk: setting(&conn, "risk"),
    })
}

fn validate_base(
    conn: &Connection,
    input: &BacktestIn,
) -> Result<String, (StatusCode, Json<Value>)> {
    let symbol = input.symbol.trim().to_uppercase();
    check_interval(&input.interval).map_err(|detail| bad(StatusCode::BAD_REQUEST, &detail))?;
    if !(200..=1500).contains(&input.limit) {
        return Err(bad(
            StatusCode::UNPROCESSABLE_ENTITY,
            "limit 必须在 200 到 1500 之间",
        ));
    }
    for (value, name, low, high) in [
        (input.fee_bps, "fee_bps", 0.0, 100.0),
        (Some(input.slippage_bps), "slippage_bps", 0.0, 100.0),
        (Some(input.spread_bps), "spread_bps", 0.0, 100.0),
        (Some(input.funding_bps_8h), "funding_bps_8h", -10.0, 50.0),
    ] {
        if let Some(v) = value {
            if !v.is_finite() || v < low || v > high {
                return Err(bad(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    &format!("{name} 超出允许范围 [{low}, {high}]"),
                ));
            }
        }
    }
    if !input.initial_equity.is_finite() || input.initial_equity <= 0.0 {
        return Err(bad(
            StatusCode::UNPROCESSABLE_ENTITY,
            "initial_equity 必须大于 0",
        ));
    }
    let exists: bool = conn
        .query_row(
            "SELECT 1 FROM trading_symbols WHERE symbol=?1",
            [&symbol],
            |_| Ok(()),
        )
        .is_ok();
    if !exists {
        return Err(bad(
            StatusCode::NOT_FOUND,
            &format!("{symbol} 不在标的池中"),
        ));
    }
    Ok(symbol)
}

fn validate_grid(input: &BacktestGridIn) -> Result<(), (StatusCode, Json<Value>)> {
    let entries = [
        ("entry_threshold", &input.entry_threshold),
        ("sl_atr_mult", &input.sl_atr_mult),
        ("tp_atr_mult", &input.tp_atr_mult),
        ("trailing_pct", &input.trailing_pct),
    ];
    let mut any = false;
    for (key, values) in entries {
        if values.len() > 4 {
            return Err(bad(
                StatusCode::UNPROCESSABLE_ENTITY,
                &format!("{key} 最多允许 4 个值"),
            ));
        }
        if !values.is_empty() {
            any = true;
        }
        check_grid(&[(key, values)]).map_err(|detail| bad(StatusCode::BAD_REQUEST, &detail))?;
    }
    if !any {
        return Err(bad(StatusCode::BAD_REQUEST, "至少需要给一个参数一组取值"));
    }
    if !(2..=6).contains(&input.folds) || !(200..=2000).contains(&input.train_bars) {
        return Err(bad(
            StatusCode::UNPROCESSABLE_ENTITY,
            "folds 或 train_bars 超出允许范围",
        ));
    }
    Ok(())
}

fn grid_spec(
    input: &BacktestGridIn,
) -> (
    Vec<(&'static str, Vec<f64>)>,
    Result<(), (StatusCode, Json<Value>)>,
) {
    let grid = vec![
        ("entry_threshold", input.entry_threshold.clone()),
        ("sl_atr_mult", input.sl_atr_mult.clone()),
        ("tp_atr_mult", input.tp_atr_mult.clone()),
        ("trailing_pct", input.trailing_pct.clone()),
    ];
    let checked = backtest_engine::grid_combinations(&grid)
        .map(|_| ())
        .map_err(|detail| bad(StatusCode::BAD_REQUEST, &detail));
    (grid, checked)
}

fn bad(status: StatusCode, detail: &str) -> (StatusCode, Json<Value>) {
    (status, Json(json!({"detail":detail})))
}

fn internal(detail: &str) -> (StatusCode, Json<Value>) {
    bad(StatusCode::INTERNAL_SERVER_ERROR, detail)
}

#[derive(Default)]
struct Registry {
    running: Mutex<HashMap<String, Arc<AtomicBool>>>,
}

impl Registry {
    fn begin(&self, id: &str) -> Result<Arc<AtomicBool>, String> {
        let mut running = self.running.lock().map_err(|_| "回测注册表不可用")?;
        if running.len() >= MAX_CONCURRENT_RUNS {
            return Err(format!("并发回测已达上限 {MAX_CONCURRENT_RUNS}"));
        }
        if running.contains_key(id) {
            return Err(format!("run_id {id} 已存在"));
        }
        let flag = Arc::new(AtomicBool::new(false));
        running.insert(id.to_string(), flag.clone());
        Ok(flag)
    }
    fn finish(&self, id: &str) {
        if let Ok(mut running) = self.running.lock() {
            running.remove(id);
        }
    }
    fn cancel(&self, id: &str) -> bool {
        self.running
            .lock()
            .ok()
            .and_then(|running| running.get(id).cloned())
            .map(|flag| {
                flag.store(true, Ordering::Relaxed);
                true
            })
            .unwrap_or(false)
    }
}

fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(Registry::default)
}

struct RunGuard {
    id: String,
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        registry().finish(&self.id);
    }
}

fn begin_run(id: &str) -> Result<(RunGuard, Arc<AtomicBool>), (StatusCode, Json<Value>)> {
    let flag = registry()
        .begin(id)
        .map_err(|detail| bad(StatusCode::BAD_REQUEST, &detail))?;
    Ok((RunGuard { id: id.to_string() }, flag))
}

fn new_run_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("bt_{}_{counter}", crate::now_millis())
}

fn valid_run_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 96
        && id.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '_' || character == '-'
        })
}

fn store_dir(db: &Db) -> PathBuf {
    Path::new(&db.path)
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("backtests")
}

fn persist(db: &Db, id: &str, body: &Value) -> Result<(), String> {
    let dir = store_dir(db);
    std::fs::create_dir_all(&dir).map_err(|error| format!("创建回测目录失败: {error}"))?;
    let path = dir.join(format!("{id}.json"));
    let temp = dir.join(format!("{id}.json.tmp"));
    let text = serde_json::to_string(body).map_err(|error| error.to_string())?;
    std::fs::write(&temp, text).map_err(|error| format!("写入回测结果失败: {error}"))?;
    std::fs::rename(&temp, &path).map_err(|error| format!("提交回测结果失败: {error}"))?;
    prune_runs(db);
    Ok(())
}

fn prune_runs(db: &Db) {
    let dir = store_dir(db);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut files: Vec<(u128, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                return None;
            }
            let modified = entry
                .metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_millis())
                .unwrap_or_default();
            Some((modified, path))
        })
        .collect();
    if files.len() <= MAX_PERSISTED_RUNS {
        return;
    }
    files.sort_by_key(|entry| entry.0);
    for (_, path) in files.iter().take(files.len() - MAX_PERSISTED_RUNS) {
        let _ = std::fs::remove_file(path);
    }
}

fn cleanup_partial(db: &Db, id: &str) {
    if !valid_run_id(id) {
        return;
    }
    let dir = store_dir(db);
    let _ = std::fs::remove_file(dir.join(format!("{id}.json")));
    let _ = std::fs::remove_file(dir.join(format!("{id}.json.tmp")));
}

async fn load_series(
    prepared: &Prepared,
    input: &BacktestIn,
) -> Result<HistorySeries, (StatusCode, Json<Value>)> {
    let step = history::interval_ms(&input.interval)
        .ok_or_else(|| bad(StatusCode::BAD_REQUEST, "不支持的历史周期"))?;
    let now = crate::now_millis();
    let end = (now / step) * step;
    let start = end - input.limit * step;
    let source = history::BinanceHistory::new(prepared.live);
    let series = source
        .load(&prepared.symbol, &input.interval, start, end)
        .await
        .map_err(|error| {
            bad(
                StatusCode::SERVICE_UNAVAILABLE,
                &format!("历史 K 线不可用: {error}"),
            )
        })?;
    if series.bars.len() < 100 {
        return Err(bad(
            StatusCode::BAD_REQUEST,
            &format!("K 线不足 (仅 {} 根)", series.bars.len()),
        ));
    }
    if series.truncated {
        return Err(bad(
            StatusCode::BAD_REQUEST,
            "历史区间过长，数据被截断，拒绝用不完整数据回测",
        ));
    }
    Ok(series)
}

fn engine_config(prepared: &Prepared, input: &BacktestIn) -> EngineConfig {
    let policy = crate::runtime::EngineRuntime::trade_policy(&prepared.strategy);
    let manage = crate::runtime::EngineRuntime::manage_config(&prepared.strategy, &prepared.risk);
    let taker = prepared.risk["fee_rate_taker"].as_f64().unwrap_or(0.0005);
    EngineConfig {
        interval_ms: history::interval_ms(&input.interval).unwrap_or(900_000),
        fee_bps: fee_bps(input.fee_bps, taker),
        slippage_bps: input.slippage_bps,
        spread_bps: input.spread_bps,
        funding_bps_8h: input.funding_bps_8h,
        initial_equity: input.initial_equity,

        margin_pct: prepared.risk["margin_pct"].as_f64().unwrap_or(50.0),
        // 回测没有 AI 决策，杠杆取全局下限：与实盘「AI 未给杠杆时的回退值」一致
        leverage: prepared.risk["min_leverage"].as_f64().unwrap_or(1.0),
        maintenance_margin_rate: 0.005,
        max_bars_hold: None,
        policy,
        manage,
    }
}

fn attach_meta(
    body: &mut Map<String, Value>,
    prepared: &Prepared,
    input: &BacktestIn,
    series: &HistorySeries,
    run_id: &str,
) {
    let first = series
        .bars
        .first()
        .map(|bar| bar.open_time)
        .unwrap_or_default();
    let last = series
        .bars
        .last()
        .map(|bar| bar.open_time)
        .unwrap_or_default();
    body.insert("id".into(), json!(run_id));
    body.insert("generated_at".into(), json!(crate::now_millis()));
    body.insert("symbol".into(), json!(prepared.symbol.clone()));
    body.insert("interval".into(), json!(input.interval.clone()));
    body.insert("bars".into(), json!(series.bars.len()));
    body.insert("from_ts".into(), json!(first));
    body.insert("to_ts".into(), json!(last));
    body.insert(
        "params".into(),
        json!({
            "scope": "backtest",
            "symbol": prepared.symbol.clone(),
            "interval": input.interval.clone(),
            "limit": input.limit,
            "fee_bps": input.fee_bps,
            "slippage_bps": input.slippage_bps,
            "spread_bps": input.spread_bps,
            "funding_bps_8h": input.funding_bps_8h,
            "initial_equity": input.initial_equity,
        }),
    );
    body.insert(
        "data_window".into(),
        json!({
            "start_ms": first,
            "end_ms": last,
            "bars": series.bars.len(),
            "funding_points": series.funding.len(),
            "funding_error": series.funding_error.clone(),
        }),
    );
    body.insert("gaps".into(), json!(series.gaps));
    body.insert("gap_count".into(), json!(series.gaps.len()));
    body.insert(
        "assumptions".into(),
        json!({
            "fee_bps": input.fee_bps,
            "slippage_bps": input.slippage_bps,
            "spread_bps": input.spread_bps,
            "funding_bps_8h": input.funding_bps_8h,
        }),
    );
}

fn finish_cancelled(db: &Db, id: &str) -> (StatusCode, Json<Value>) {
    cleanup_partial(db, id);
    bad(StatusCode::CONFLICT, backtest_engine::CANCELLED)
}

async fn run(
    State(db): State<Db>,
    Json(input): Json<BacktestIn>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let prepared = prepare(&db, &input)?;
    let run_id = input.run_id.clone().unwrap_or_else(new_run_id);
    if !valid_run_id(&run_id) {
        return Err(bad(StatusCode::BAD_REQUEST, "run_id 含非法字符"));
    }
    let (guard, cancel) = begin_run(&run_id)?;
    let series = load_series(&prepared, &input).await?;
    let config = engine_config(&prepared, &input);
    let bars = series.bars.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        backtest_engine::run_with_cancel(&bars, &config, &cancel)
    })
    .await
    .map_err(|error| internal(&format!("回测任务异常: {error}")))?;
    let result = match outcome {
        Ok(result) => result,
        Err(error) if error == backtest_engine::CANCELLED => {
            drop(guard);
            return Err(finish_cancelled(&db, &run_id));
        }
        Err(error) => {
            drop(guard);
            return Err(bad(StatusCode::UNPROCESSABLE_ENTITY, &error));
        }
    };
    if let Some(message) = result.summary.get("error").and_then(Value::as_str) {
        drop(guard);
        return Err(bad(StatusCode::BAD_REQUEST, message));
    }
    let mut body = result.summary.as_object().cloned().unwrap_or_default();
    if let Some(count) = body.remove("trades") {
        body.insert("trade_count".into(), count);
    }
    attach_meta(&mut body, &prepared, &input, &series, &run_id);
    let skip = result.equity_curve.len().saturating_sub(300);
    body.insert("equity_curve".into(), json!(&result.equity_curve[skip..]));
    let skip = result.trades.len().saturating_sub(100);
    body.insert("trades".into(), json!(&result.trades[skip..]));
    let body = Value::Object(body);
    persist(&db, &run_id, &body).map_err(|error| internal(&error))?;
    drop(guard);
    Ok(Json(body))
}

async fn grid(
    State(db): State<Db>,
    Json(input): Json<BacktestGridIn>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let prepared = prepare(&db, &input.base)?;
    validate_grid(&input)?;
    let (spec, checked) = grid_spec(&input);
    checked?;
    let run_id = input.base.run_id.clone().unwrap_or_else(new_run_id);
    if !valid_run_id(&run_id) {
        return Err(bad(StatusCode::BAD_REQUEST, "run_id 含非法字符"));
    }
    let (guard, cancel) = begin_run(&run_id)?;
    let series = load_series(&prepared, &input.base).await?;
    let config = engine_config(&prepared, &input.base);
    let bars = series.bars.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        backtest_engine::run_grid(&bars, &config, &spec, &cancel)
    })
    .await
    .map_err(|error| internal(&format!("网格任务异常: {error}")))?;
    let result = match outcome {
        Ok(result) => result,
        Err(error) if error == backtest_engine::CANCELLED => {
            drop(guard);
            return Err(finish_cancelled(&db, &run_id));
        }
        Err(error) => {
            drop(guard);
            return Err(bad(StatusCode::BAD_REQUEST, &error));
        }
    };
    let mut body = result.as_object().cloned().unwrap_or_default();
    attach_meta(&mut body, &prepared, &input.base, &series, &run_id);
    drop(guard);
    Ok(Json(Value::Object(body)))
}

fn detail_of(error: (StatusCode, Json<Value>)) -> String {
    error.1 .0["detail"]
        .as_str()
        .unwrap_or("回测失败")
        .to_string()
}

/// `ops final-verdict`：用真实历史行情执行 walk-forward，并给出可判定结论。
///
/// 判定规则固定为：样本外平均收益 > 0、过拟合差距 ≤ 1.0 个百分点、正收益折数过半。
pub async fn final_verdict(
    db: Db,
    interval: &str,
    symbols: &[String],
    bars: i64,
    folds: i64,
    train_bars: i64,
) -> Result<Value, String> {
    if !(200..=1500).contains(&bars) {
        return Err("bars 必须在 200 到 1500 之间".to_string());
    }
    if !(2..=6).contains(&folds) {
        return Err("folds 必须在 2 到 6 之间".to_string());
    }
    let mut results = Vec::new();
    let mut all_pass = true;
    for symbol in symbols {
        let input = BacktestIn {
            symbol: symbol.clone(),
            interval: interval.to_string(),
            limit: bars,
            fee_bps: None,
            slippage_bps: 2.0,
            spread_bps: 0.0,
            funding_bps_8h: 1.0,
            initial_equity: 100.0,
            run_id: None,
        };
        let prepared = prepare(&db, &input).map_err(detail_of)?;
        let series = load_series(&prepared, &input).await.map_err(detail_of)?;
        let config = engine_config(&prepared, &input);
        let grid: Vec<(&'static str, Vec<f64>)> = vec![
            ("entry_threshold", vec![0.45, 0.50, 0.55]),
            ("sl_atr_mult", vec![1.2, 1.5, 2.0]),
            ("tp_atr_mult", vec![2.0, 3.0]),
            ("trailing_pct", vec![0.0]),
        ];
        let bars_vec = series.bars.clone();
        let run_bars = bars_vec.len();
        let outcome = tokio::task::spawn_blocking(move || {
            let cancel = AtomicBool::new(false);
            backtest_engine::run_walkforward(&bars_vec, &config, &grid, folds, train_bars, &cancel)
        })
        .await
        .map_err(|error| format!("样本外任务异常: {error}"))??;
        let aggregate = &outcome["aggregate"];
        let oos_avg = aggregate["out_sample_avg_pct"].as_f64().unwrap_or(0.0);
        let gap = aggregate["overfit_gap_pct"].as_f64().unwrap_or(0.0);
        let positive = aggregate["positive_folds"].as_i64().unwrap_or(0);
        let folds_done = outcome["folds"].as_i64().unwrap_or(0);
        let verdict = oos_avg > 0.0 && gap <= 1.0 && positive * 2 >= folds_done;
        all_pass &= verdict;
        results.push(json!({
            "symbol": prepared.symbol,
            "bars": run_bars,
            "folds": folds_done,
            "out_sample_avg_pct": oos_avg,
            "in_sample_avg_pct": aggregate["in_sample_avg_pct"],
            "overfit_gap_pct": gap,
            "positive_folds": positive,
            "oos_return_pct": aggregate["oos_return_pct"],
            "oos_worst_fold_pct": aggregate["oos_worst_fold_pct"],
            "oos_trades": aggregate["oos_trades"],
            "verdict": if verdict { "pass" } else { "reject" },
        }));
    }
    Ok(json!({
        "ok": all_pass,
        "interval": interval,
        "bars": bars,
        "folds": folds,
        "train_bars": train_bars,
        "verdict_rule": "样本外平均收益 > 0 且过拟合差距 ≤ 1.0 且正收益折过半",
        "results": results,
    }))
}

async fn walkforward(
    State(db): State<Db>,
    Json(input): Json<BacktestGridIn>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let prepared = prepare(&db, &input.base)?;
    validate_grid(&input)?;
    let (spec, checked) = grid_spec(&input);
    checked?;
    let run_id = input.base.run_id.clone().unwrap_or_else(new_run_id);
    if !valid_run_id(&run_id) {
        return Err(bad(StatusCode::BAD_REQUEST, "run_id 含非法字符"));
    }
    let (guard, cancel) = begin_run(&run_id)?;
    let series = load_series(&prepared, &input.base).await?;
    if series.bars.len() <= 240 {
        drop(guard);
        return Err(bad(
            StatusCode::BAD_REQUEST,
            &format!("K 线不足: 至少需要 241 根 (当前 {})", series.bars.len()),
        ));
    }
    let config = engine_config(&prepared, &input.base);
    let bars = series.bars.clone();
    let folds = input.folds;
    let train_bars = input.train_bars;
    let outcome = tokio::task::spawn_blocking(move || {
        backtest_engine::run_walkforward(&bars, &config, &spec, folds, train_bars, &cancel)
    })
    .await
    .map_err(|error| internal(&format!("样本外任务异常: {error}")))?;
    let result = match outcome {
        Ok(result) => result,
        Err(error) if error == backtest_engine::CANCELLED => {
            drop(guard);
            return Err(finish_cancelled(&db, &run_id));
        }
        Err(error) => {
            drop(guard);
            return Err(bad(StatusCode::BAD_REQUEST, &error));
        }
    };
    let mut body = result.as_object().cloned().unwrap_or_default();
    attach_meta(&mut body, &prepared, &input.base, &series, &run_id);
    drop(guard);
    Ok(Json(Value::Object(body)))
}

async fn list_runs(State(db): State<Db>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let dir = store_dir(&db);
    let mut runs = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            runs.push(json!({
                "id": value["id"],
                "symbol": value["symbol"],
                "interval": value["interval"],
                "bars": value["bars"],
                "generated_at": value["generated_at"],
                "from_ts": value["from_ts"],
                "to_ts": value["to_ts"],
                "trade_count": value["trade_count"],
                "total_return_pct": value["total_return_pct"],
            }));
        }
    }
    runs.sort_by(|left, right| {
        right["generated_at"]
            .as_i64()
            .unwrap_or_default()
            .cmp(&left["generated_at"].as_i64().unwrap_or_default())
    });
    Ok(Json(json!({"runs": runs, "count": runs.len()})))
}

async fn get_run(
    State(db): State<Db>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !valid_run_id(&id) {
        return Err(bad(StatusCode::BAD_REQUEST, "run_id 含非法字符"));
    }
    let path = store_dir(&db).join(format!("{id}.json"));
    let text =
        std::fs::read_to_string(&path).map_err(|_| bad(StatusCode::NOT_FOUND, "回测结果不存在"))?;
    let value = serde_json::from_str::<Value>(&text)
        .map_err(|error| internal(&format!("回测结果损坏: {error}")))?;
    Ok(Json(value))
}

async fn cancel_run(
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !valid_run_id(&id) {
        return Err(bad(StatusCode::BAD_REQUEST, "run_id 含非法字符"));
    }
    if !registry().cancel(&id) {
        return Err(bad(StatusCode::NOT_FOUND, "没有正在执行的回测任务"));
    }
    Ok(Json(json!({"id": id, "canceling": true})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_interval_is_rejected() {
        assert!(check_interval("15m").is_ok());
        assert!(check_interval("3m").is_err(), "白名单外的周期不能进回测");
    }

    #[test]
    fn grid_values_outside_the_bounds_are_rejected() {
        let ok = check_grid(&[("sl_atr_mult", &[1.5, 2.0])]);
        assert!(ok.is_ok());

        let bad = check_grid(&[("sl_atr_mult", &[0.1])]);
        assert!(bad.is_err(), "0.1 低于 0.2 的下限");

        let empty = check_grid(&[]);
        assert!(empty.is_err(), "一个参数都不给就没有可扫的组合");
    }

    #[test]
    fn fee_falls_back_to_the_live_taker_rate() {
        assert_eq!(fee_bps(Some(8.0), 0.0005), 8.0, "显式给了就用给的");
        assert_eq!(
            fee_bps(None, 0.0005),
            5.0,
            "没给就按实盘 taker 费率换算成 bps"
        );
    }

    #[test]
    fn run_ids_are_restricted_to_a_filesystem_safe_alphabet() {
        assert!(valid_run_id("bt_123_0"));
        assert!(!valid_run_id("../etc/passwd"));
        assert!(!valid_run_id("a/b"));
        assert!(!valid_run_id(""));
    }

    #[test]
    fn concurrent_runs_are_capped_and_cancellable() {
        let registry = Registry::default();
        let first = registry.begin("a").unwrap();
        let second = registry.begin("b").unwrap();
        assert!(registry.begin("c").is_err(), "并发上限必须拒绝而非排队");
        assert!(registry.cancel("a"));
        assert!(first.load(Ordering::Relaxed));
        assert!(!second.load(Ordering::Relaxed));
        registry.finish("a");
        assert!(registry.begin("c").is_ok());
    }

    #[test]
    fn run_results_survive_and_cancelled_partials_are_removed() {
        let dir = std::env::temp_dir().join(format!("nanoquant_bt_{}", crate::now_millis()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db {
            path: dir.join("nanoquant.db").to_string_lossy().into_owned(),
            conn: Arc::new(Mutex::new(Connection::open_in_memory().unwrap())),
        };
        let body = json!({
            "id": "bt_test_0",
            "generated_at": 1,
            "symbol": "BTCUSDT",
            "interval": "15m",
            "bars": 500,
            "trade_count": 0,
        });
        persist(&db, "bt_test_0", &body).unwrap();
        let path = store_dir(&db).join("bt_test_0.json");
        assert!(path.exists(), "结果必须落盘，重启后才可查询");
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["symbol"], "BTCUSDT");
        cleanup_partial(&db, "bt_test_0");
        assert!(!path.exists(), "取消后不能残留半成品");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grid_combination_cap_is_enforced_before_execution() {
        let values = vec![1.0, 1.5, 2.0, 2.5, 3.0];
        let input = BacktestGridIn {
            base: BacktestIn {
                symbol: "BTCUSDT".into(),
                interval: "15m".into(),
                limit: 500,
                fee_bps: None,
                slippage_bps: 2.0,
                spread_bps: 0.0,
                funding_bps_8h: 1.0,
                initial_equity: 100.0,
                run_id: None,
            },
            entry_threshold: values.clone(),
            sl_atr_mult: values.clone(),
            tp_atr_mult: values,
            trailing_pct: Vec::new(),
            folds: 3,
            train_bars: 600,
        };
        let (_, checked) = grid_spec(&input);
        assert!(checked.is_err(), "组合数超限必须在执行前 400");
    }
}
