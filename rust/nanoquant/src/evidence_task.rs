//! 候选参数集证据门：按 B 期规则拉真实历史、跑固定 walk-forward 并落 approved/rejected。
//!
//! 只写参数集状态与审计事件：不激活参数集、不写 `app_settings.strategy`。
//! 历史数据不可用或回测本身出错时返回 Err 并记 `parameter_set.evidence_failed`，
//! 候选保持 `candidate`，绝不把「没数据」误判成「策略不达标」。

use std::sync::atomic::AtomicBool;

use rusqlite::{params, Connection};
use serde_json::{json, Value};

use crate::ai_providers::AiState;
use crate::backtest_engine::{self, BasketMetrics, EngineConfig};
use crate::evidence::{self, EvidenceReport};
use crate::history::{self, BinanceHistory, HistoryBar, HistorySource};
use crate::parameter_sets::{self, ParameterSet};
use crate::regime::Regime;
use crate::runtime::EngineRuntime;

const FOLDS: i64 = 3;
const TRAIN_BARS: i64 = 800;
const BARS_LIMIT: i64 = 2_000;
const MIN_SYMBOLS: usize = 6;
const DEFAULT_MIN_OOS_TRADES: f64 = 30.0;
const DEFAULT_TIMEFRAME: &str = "1h";
const EVENT_CATEGORY: &str = "parameter_set";

/// 候选证据门入口：真实币安历史 + 真实回测。
pub(crate) async fn evaluate_candidate(state: AiState, candidate_id: i64) -> Result<(), String> {
    let store = BinanceHistory::new(live_mode(&state));
    evaluate_with_history(&state, candidate_id, &store).await
}

/// 注入数据源的实现，测试用 fake history 避免触网。
async fn evaluate_with_history(
    state: &AiState,
    candidate_id: i64,
    history_source: &dyn HistorySource,
) -> Result<(), String> {
    if let Err(error) = run_gate(state, candidate_id, history_source).await {
        record_failed(state, candidate_id, &error);
        return Err(error);
    }
    Ok(())
}

struct Prepared {
    candidate: ParameterSet,
    baseline: Option<ParameterSet>,
    symbols: Vec<String>,
    risk: Value,
    live_timeframe: String,
    min_oos_trades: usize,
    auto_apply_timeframes: Vec<String>,
}

async fn run_gate(
    state: &AiState,
    candidate_id: i64,
    history_source: &dyn HistorySource,
) -> Result<(), String> {
    let Some(prepared) = prepare(state, candidate_id)? else {
        return Ok(());
    };
    // 时间框架不合格时直接失败：不拉数据、不跑回测，避免为注定失败的门浪费上游配额。
    if prepared.candidate.timeframe != prepared.live_timeframe
        || !prepared
            .auto_apply_timeframes
            .iter()
            .any(|value| value == &prepared.candidate.timeframe)
    {
        let metrics = BasketMetrics {
            timeframe: prepared.candidate.timeframe.clone(),
            target_regime: prepared.candidate.regime.clone(),
            ..BasketMetrics::default()
        };
        return finish(state, prepared, metrics, None);
    }

    let interval_ms = history::interval_ms(&prepared.candidate.timeframe)
        .ok_or_else(|| format!("不支持的回测周期: {}", prepared.candidate.timeframe))?;
    let end_ms = crate::now_millis();
    let start_ms = end_ms - BARS_LIMIT * interval_ms;
    let (basket, data_skipped) = load_basket(
        history_source,
        &prepared.symbols,
        &prepared.candidate.timeframe,
        start_ms,
        end_ms,
    )
    .await;
    if basket.len() < MIN_SYMBOLS {
        return Err(format!(
            "可用历史标的仅 {} 个，少于 {MIN_SYMBOLS} 个，证据门无法评估",
            basket.len()
        ));
    }
    let regime = Regime::from_str(&prepared.candidate.regime)
        .ok_or_else(|| format!("未知 regime: {}", prepared.candidate.regime))?;
    let candidate_config = build_config(
        &prepared.candidate.params,
        &prepared.risk,
        &prepared.candidate.timeframe,
    )?;
    let mut metrics = run_metrics(
        basket.clone(),
        candidate_config,
        regime,
        prepared.candidate.timeframe.clone(),
    )
    .await?;
    merge_skipped(&mut metrics, &data_skipped);

    let baseline = match &prepared.baseline {
        Some(set) => {
            let config = build_config(&set.params, &prepared.risk, &prepared.candidate.timeframe)?;
            let mut baseline_metrics =
                run_metrics(basket, config, regime, prepared.candidate.timeframe.clone()).await?;
            merge_skipped(&mut baseline_metrics, &data_skipped);
            Some(baseline_metrics)
        }
        None => None,
    };
    finish(state, prepared, metrics, baseline)
}

fn finish(
    state: &AiState,
    prepared: Prepared,
    metrics: BasketMetrics,
    baseline: Option<BasketMetrics>,
) -> Result<(), String> {
    let whitelist: Vec<&str> = prepared
        .auto_apply_timeframes
        .iter()
        .map(String::as_str)
        .collect();
    let baseline_oos_avg = baseline.as_ref().map(|value| value.oos_avg_return_pct);
    let report = evidence::evaluate_gate(
        &metrics,
        prepared.min_oos_trades,
        &prepared.live_timeframe,
        &whitelist,
        baseline_oos_avg,
    );
    let baseline_set_id = prepared.baseline.as_ref().map(|set| set.id);
    let evidence = evidence::evidence_json(&metrics, &report, baseline_set_id, baseline.as_ref());
    write_outcome(state, prepared.candidate.id, &report, &evidence)
}

/// 同步准备阶段：全部数据库读取在返回前完成，避免锁跨 await。
fn prepare(state: &AiState, candidate_id: i64) -> Result<Option<Prepared>, String> {
    let conn = state
        .db
        .conn
        .lock()
        .map_err(|_| "数据库连接锁不可用".to_string())?;
    let candidate = parameter_sets::load_set(&conn, candidate_id)?;
    if candidate.source != "ai" || candidate.status != "candidate" {
        return Ok(None);
    }
    let strategy = group_json(&conn, "strategy");
    let risk = group_json(&conn, "risk");
    let parameter_set = group_json(&conn, "parameter_set");
    let live_timeframe = strategy
        .get("timeframe")
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_TIMEFRAME)
        .to_string();
    let min_oos_trades = parameter_set
        .get("min_oos_trades")
        .and_then(Value::as_f64)
        .unwrap_or(DEFAULT_MIN_OOS_TRADES)
        .max(1.0) as usize;
    let auto_apply_timeframes = parse_timeframes(&parameter_set);
    // 同 regime 的 active 作为基线；该 regime 没有 active 时回退 range-bootstrap。
    let mut baseline = parameter_sets::active_for_regime(&conn, &candidate.regime)?;
    if baseline.is_none() {
        baseline = parameter_sets::active_for_regime(&conn, "range")?;
    }
    let symbols = pool_symbols(&conn)?;
    Ok(Some(Prepared {
        candidate,
        baseline,
        symbols,
        risk,
        live_timeframe,
        min_oos_trades,
        auto_apply_timeframes,
    }))
}

fn parse_timeframes(parameter_set: &Value) -> Vec<String> {
    parameter_set
        .get("auto_apply_timeframes")
        .and_then(Value::as_str)
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn pool_symbols(conn: &Connection) -> Result<Vec<String>, String> {
    let mut statement = conn
        .prepare(
            "SELECT symbol FROM trading_symbols WHERE enabled=1 \
             ORDER BY sort_order ASC, symbol ASC",
        )
        .map_err(|error| format!("读取启用标的失败: {error}"))?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| format!("读取启用标的失败: {error}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("读取启用标的失败: {error}"))
}

async fn load_basket(
    source: &dyn HistorySource,
    symbols: &[String],
    timeframe: &str,
    start_ms: i64,
    end_ms: i64,
) -> (Vec<(String, Vec<HistoryBar>)>, Vec<String>) {
    let mut basket = Vec::new();
    let mut skipped = Vec::new();
    for symbol in symbols {
        match source.load(symbol, timeframe, start_ms, end_ms).await {
            Ok(series) if series.bars.len() >= BARS_LIMIT as usize => {
                basket.push((symbol.clone(), series.bars));
            }
            Ok(series) => {
                tracing::warn!(
                    symbol = %symbol,
                    bars = series.bars.len(),
                    limit = BARS_LIMIT,
                    "证据门历史 K 线不足，跳过标的"
                );
                skipped.push(symbol.clone());
            }
            Err(error) => {
                tracing::warn!(
                    symbol = %symbol,
                    error = %error,
                    "证据门历史 K 线不可用，跳过标的"
                );
                skipped.push(symbol.clone());
            }
        }
    }
    (basket, skipped)
}

fn merge_skipped(metrics: &mut BasketMetrics, data_skipped: &[String]) {
    for symbol in data_skipped {
        if !metrics.symbols_skipped.contains(symbol) {
            metrics.symbols_skipped.push(symbol.clone());
        }
    }
}

async fn run_metrics(
    basket: Vec<(String, Vec<HistoryBar>)>,
    config: EngineConfig,
    regime: Regime,
    timeframe: String,
) -> Result<BasketMetrics, String> {
    tokio::task::spawn_blocking(move || {
        let cancel = AtomicBool::new(false);
        backtest_engine::run_walkforward_basket_fixed(
            &basket, &config, FOLDS, TRAIN_BARS, regime, &timeframe, &cancel,
        )
    })
    .await
    .map_err(|error| format!("证据门回测任务异常: {error}"))?
}

/// 回测参数口径与实盘成交一致：费率取 risk，其余按 B 期固定成本压力设定。
pub(crate) fn build_config(
    params: &Value,
    risk: &Value,
    timeframe: &str,
) -> Result<EngineConfig, String> {
    let interval_ms =
        history::interval_ms(timeframe).ok_or_else(|| format!("不支持的回测周期: {timeframe}"))?;
    let taker = risk
        .get("fee_rate_taker")
        .and_then(Value::as_f64)
        .unwrap_or(0.0005);
    let policy = EngineRuntime::trade_policy(params);
    let manage = EngineRuntime::manage_config(params, risk);
    Ok(EngineConfig {
        interval_ms,
        fee_bps: taker * 10_000.0,
        slippage_bps: 2.0,
        spread_bps: 0.0,
        funding_bps_8h: 1.0,
        initial_equity: 100.0,
        risk_per_trade_pct: risk
            .get("risk_per_trade_pct")
            .and_then(Value::as_f64)
            .unwrap_or(1.0),
        margin_pct: risk
            .get("margin_pct")
            .and_then(Value::as_f64)
            .unwrap_or(50.0),
        leverage: risk
            .get("min_leverage")
            .and_then(Value::as_f64)
            .unwrap_or(1.0),
        maintenance_margin_rate: 0.005,
        max_bars_hold: None,
        policy,
        manage,
    })
}

fn write_outcome(
    state: &AiState,
    set_id: i64,
    report: &EvidenceReport,
    evidence: &Value,
) -> Result<(), String> {
    let conn = state
        .db
        .conn
        .lock()
        .map_err(|_| "数据库连接锁不可用".to_string())?;
    parameter_sets::mark_evidence(&conn, set_id, report.pass, evidence)?;
    let (level, message, event) = if report.pass {
        ("info", "参数集证据门通过", "parameter_set.evidence_pass")
    } else {
        ("warn", "参数集证据门未通过", "parameter_set.evidence_fail")
    };
    record_event(
        &conn,
        level,
        message,
        json!({
            "event": event,
            "set_id": set_id,
            "metrics": evidence["candidate"],
            "baseline_set_id": evidence["baseline_set_id"],
            "reasons": report.reasons,
        }),
    )
}

fn record_failed(state: &AiState, set_id: i64, error: &str) {
    let detail = crate::tuning::sanitize_error(error);
    tracing::warn!(set_id, error = %detail, "候选参数集证据门执行失败");
    let Ok(conn) = state.db.conn.lock() else {
        return;
    };
    let _ = record_event(
        &conn,
        "warn",
        "参数集证据门执行失败",
        json!({
            "event": "parameter_set.evidence_failed",
            "set_id": set_id,
            "error": detail,
        }),
    );
}

fn record_event(
    conn: &Connection,
    level: &str,
    message: &str,
    payload: Value,
) -> Result<(), String> {
    conn.execute(
        "INSERT INTO engine_events(ts,level,category,message,payload) \
         VALUES(?1,?2,?3,?4,?5)",
        params![
            crate::now(),
            level,
            EVENT_CATEGORY,
            message,
            payload.to_string()
        ],
    )
    .map_err(|error| format!("写入参数集证据事件失败: {error}"))?;
    Ok(())
}

fn group_json(conn: &Connection, key: &str) -> Value {
    conn.query_row(
        "SELECT value FROM app_settings WHERE key=?1",
        [key],
        |row| row.get::<_, String>(0),
    )
    .ok()
    .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
    .unwrap_or(Value::Null)
}

fn live_mode(state: &AiState) -> bool {
    let Ok(conn) = state.db.conn.lock() else {
        return false;
    };
    current_mode(&conn) == "live"
}

/// `engine_state.mode` 优先，其次 `engine.mode`，最后 demo。
fn current_mode(conn: &Connection) -> String {
    let stored = conn
        .query_row(
            "SELECT value FROM engine_state WHERE key='mode'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .filter(|mode| !mode.is_empty());
    if let Some(mode) = stored {
        return mode;
    }
    group_json(conn, "engine")
        .get("mode")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| "demo".to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use rusqlite::Connection as SqlConnection;

    use super::*;
    use crate::kline_stream::BoxFuture;

    const MASTER_KEY: &[u8] = b"test-master-key";

    struct FakeHistory {
        reply: Result<Vec<HistoryBar>, String>,
        calls: AtomicUsize,
    }

    impl FakeHistory {
        fn unavailable() -> Self {
            Self {
                reply: Err("历史行情不可用".to_string()),
                calls: AtomicUsize::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::Relaxed)
        }
    }

    impl HistorySource for FakeHistory {
        fn load(
            &self,
            _symbol: &str,
            _interval: &str,
            _start_ms: i64,
            _end_ms: i64,
        ) -> BoxFuture<'_, Result<crate::history::HistorySeries, String>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let reply = self.reply.clone();
            Box::pin(async move {
                reply.map(|bars| crate::history::HistorySeries {
                    bars,
                    ..crate::history::HistorySeries::default()
                })
            })
        }
    }

    fn memory_state() -> AiState {
        let conn = SqlConnection::open_in_memory().unwrap();
        crate::schema::migrate(&conn).unwrap();
        conn.execute(
            "INSERT INTO app_settings VALUES('strategy',?1,0)",
            [json!({
                "timeframe": "1h",
                "atr_pct_min": 1.5,
                "atr_pct_max": 8.0,
                "volume_ratio_min": 1.1,
                "entry_threshold": 0.55
            })
            .to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO app_settings VALUES('risk',?1,0)",
            [json!({
                "fee_rate_taker": 0.0005,
                "risk_per_trade_pct": 10.0,
                "margin_pct": 50.0,
                "min_leverage": 1.0
            })
            .to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO app_settings VALUES('parameter_set',?1,0)",
            [json!({
                "enabled": true,
                "auto_apply": false,
                "auto_apply_timeframes": "1h",
                "min_oos_trades": 30
            })
            .to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO app_settings VALUES('engine',?1,0)",
            [json!({"enabled": false, "mode": "demo"}).to_string()],
        )
        .unwrap();
        for (index, symbol) in [
            "AAAUSDT", "BBBUSDT", "CCCUSDT", "DDDUSDT", "EEEUSDT", "FFFUSDT",
        ]
        .iter()
        .enumerate()
        {
            conn.execute(
                "INSERT INTO trading_symbols(symbol,name,tier,sl_atr_mult,enabled, \
                 max_leverage,precision,tick_sz,min_sz,min_notional,ct_val,min_score,note, \
                 sort_order,created_at,updated_at) \
                 VALUES(?1,?1,'tier_1',1.2,1,10,2,'0.01','0.001',5,1,0,'',?2,0,0)",
                params![symbol, index as i64],
            )
            .unwrap();
        }
        let db = crate::db::Db {
            path: String::new(),
            conn: Arc::new(Mutex::new(conn)),
        };
        AiState::new(db, MASTER_KEY.to_vec())
    }

    fn setup(timeframe: &str) -> (AiState, i64) {
        let state = memory_state();
        let candidate_id = {
            let conn = state.db.conn.lock().unwrap();
            crate::parameter_sets::seed_bootstrap(&conn).unwrap();
            let parent = crate::parameter_sets::active_for_regime(&conn, "range")
                .unwrap()
                .unwrap();
            let mut snapshot = parent.params.clone();
            snapshot["atr_pct_min"] = json!(0.7);
            snapshot["timeframe"] = json!(timeframe);
            crate::parameter_sets::insert_candidate(
                &conn,
                "range-ai-evidence",
                "range",
                timeframe,
                &snapshot,
                Some(parent.id),
            )
            .unwrap()
        };
        (state, candidate_id)
    }

    fn events(state: &AiState) -> Vec<Value> {
        let conn = state.db.conn.lock().unwrap();
        let mut statement = conn
            .prepare("SELECT payload FROM engine_events WHERE category='parameter_set' ORDER BY id")
            .unwrap();
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap();
        rows.filter_map(Result::ok)
            .map(|raw| serde_json::from_str(&raw).unwrap_or(Value::Null))
            .collect()
    }

    #[test]
    fn build_config_maps_strategy_and_risk_fields() {
        let params = json!({
            "timeframe": "1h",
            "atr_pct_min": 1.5,
            "atr_pct_max": 8.0,
            "volume_ratio_min": 1.1,
            "entry_threshold": 0.55,
            "sl_atr_mult": 1.2,
            "tp_atr_mult": 2.0,
            "min_risk_reward": 2.0
        });
        let risk = json!({
            "fee_rate_taker": 0.0005,
            "risk_per_trade_pct": 10.0,
            "margin_pct": 50.0,
            "min_leverage": 1.0
        });

        let config = build_config(&params, &risk, "1h").unwrap();

        assert_eq!(config.interval_ms, 3_600_000);
        assert!(
            (config.fee_bps - 5.0).abs() < 1e-9,
            "费率必须按 risk 折算 bps"
        );
        assert_eq!(config.policy.atr_pct_min, 1.5);
        assert_eq!(config.policy.atr_pct_max, 8.0);
        assert_eq!(config.policy.volume_ratio_min, 1.1);
        assert_eq!(config.slippage_bps, 2.0);
        assert_eq!(config.spread_bps, 0.0);
        assert_eq!(config.funding_bps_8h, 1.0);
        assert_eq!(config.initial_equity, 100.0);
        assert_eq!(config.risk_per_trade_pct, 10.0);
        assert_eq!(config.margin_pct, 50.0);
        assert_eq!(config.leverage, 1.0);
        assert_eq!(config.maintenance_margin_rate, 0.005);
        assert!(config.max_bars_hold.is_none());
        assert!(
            build_config(&params, &risk, "3d").is_err(),
            "不支持的周期必须报错，不能猜默认值"
        );
    }

    #[tokio::test]
    async fn timeframe_mismatch_is_rejected_without_fetching_history() {
        let (state, candidate_id) = setup("4h");
        let history = FakeHistory::unavailable();

        evaluate_with_history(&state, candidate_id, &history)
            .await
            .unwrap();

        assert_eq!(history.calls(), 0, "注定失败的门不应触网");
        let conn = state.db.conn.lock().unwrap();
        let set = crate::parameter_sets::load_set(&conn, candidate_id).unwrap();
        assert_eq!(set.status, "rejected");
        assert_eq!(set.evidence["gate"]["pass"], false);
        let reasons = set.evidence["gate"]["reasons"].as_array().unwrap();
        assert!(
            reasons.iter().any(|reason| reason
                .as_str()
                .unwrap_or_default()
                .starts_with("timeframe_mismatch")),
            "timeframe 不一致必须写入失败原因"
        );
        drop(conn);
        let payloads = events(&state);
        assert_eq!(payloads.len(), 1, "应写一条 evidence_fail 事件");
        assert_eq!(payloads[0]["event"], "parameter_set.evidence_fail");
        assert_eq!(payloads[0]["set_id"], candidate_id);
        assert!(payloads[0]["reasons"].is_array());
    }

    #[tokio::test]
    async fn history_unavailable_keeps_candidate_and_records_failed_event() {
        let (state, candidate_id) = setup("1h");
        let history = FakeHistory::unavailable();

        let result = evaluate_with_history(&state, candidate_id, &history).await;

        assert!(result.is_err(), "历史不可用必须报错，不能判 rejected");
        assert!(history.calls() > 0);
        let conn = state.db.conn.lock().unwrap();
        let set = crate::parameter_sets::load_set(&conn, candidate_id).unwrap();
        assert_eq!(set.status, "candidate", "数据失败时候选必须保持待评估");
        assert_eq!(set.evidence, json!({}), "数据失败不得写证据");
        drop(conn);
        let payloads = events(&state);
        assert!(
            payloads
                .iter()
                .any(|payload| payload["event"] == "parameter_set.evidence_failed"),
            "数据失败必须留审计事件"
        );
        assert!(!payloads
            .iter()
            .any(|payload| payload["event"] == "parameter_set.evidence_pass"
                || payload["event"] == "parameter_set.evidence_fail"));
    }

    #[tokio::test]
    async fn non_candidate_is_skipped_without_fetching_history() {
        let (state, candidate_id) = setup("1h");
        state
            .db
            .conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE parameter_sets SET status='approved' WHERE id=?1",
                [candidate_id],
            )
            .unwrap();
        let history = FakeHistory::unavailable();

        evaluate_with_history(&state, candidate_id, &history)
            .await
            .unwrap();

        assert_eq!(history.calls(), 0);
        assert!(events(&state).is_empty(), "非候选不应写证据事件");
    }
}
