//! 候选AI 调参证据门：按 B 期规则拉真实历史、跑固定 walk-forward 并落 approved/rejected。
//!
//! 证据门通过后自动激活并物化 strategy；未通过或执行失败只落状态与审计事件。
//! 历史数据不可用或回测本身出错时返回 Err 并记 `ai_tuning.evidence_failed`，
//! 候选保持 `candidate`，绝不把「没数据」误判成「策略不达标」。

use std::sync::atomic::AtomicBool;

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::ai_providers::AiState;
use crate::ai_tuning::{self, AiTuningSet};
use crate::backtest_engine::{self, BasketMetrics, EngineConfig};
use crate::evidence::{self, EvidenceReport};
use crate::history::{self, BinanceHistory, HistoryBar, HistorySource};
use crate::regime::Regime;
use crate::runtime::EngineRuntime;

const FOLDS: i64 = 3;
const TRAIN_BARS: i64 = 800;
const BARS_LIMIT: i64 = 2_000;
const MIN_SYMBOLS: usize = 6;
const DEFAULT_MIN_OOS_TRADES: f64 = 30.0;
pub(crate) const DEFAULT_TIMEFRAME: &str = "1h";
const EVENT_CATEGORY: &str = "ai_tuning";
const PENDING_WINDOW_SEC: i64 = 7 * 24 * 3600;
const ATTEMPT_COOLDOWN_SEC: i64 = 6 * 3600;
const SWEEP_INTERVAL_SEC: i64 = 6 * 3600;

/// 候选证据门入口：真实币安历史 + 真实回测。
pub(crate) async fn evaluate_candidate(state: AiState, candidate_id: i64) -> Result<(), String> {
    let live = live_mode(&state)?;
    let store = BinanceHistory::new(live);
    evaluate_with_history(&state, candidate_id, &store).await
}

/// 注入数据源的实现，测试用 fake history 避免触网。
///
/// 所有直接/扫描评估统一在这里先记 attempt：失败候选同样进入 6h 冷却，
/// 同轮 sweep 不会重复挑中刚 spawn 的 id；写失败直接返回 Err，不进入回测。
async fn evaluate_with_history(
    state: &AiState,
    candidate_id: i64,
    history_source: &dyn HistorySource,
) -> Result<(), String> {
    {
        let conn = state
            .db
            .conn
            .lock()
            .map_err(|_| "数据库连接锁不可用".to_string())?;
        write_attempt(&conn, candidate_id, crate::now())?;
    }
    if let Err(error) = run_gate(state, candidate_id, history_source).await {
        record_failed(state, candidate_id, &error);
        return Err(error);
    }
    Ok(())
}

struct Prepared {
    candidate: AiTuningSet,
    baseline: Option<AiTuningSet>,
    symbols: Vec<String>,
    risk: Value,
    live_timeframe: String,
    min_oos_trades: usize,
    /// 同一 regime×timeframe 已尝试过的参数集数量：Deflated Sharpe 的试验次数。
    trials: usize,
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
    if prepared.candidate.timeframe != prepared.live_timeframe {
        let metrics = BasketMetrics {
            timeframe: prepared.candidate.timeframe.clone(),
            target_regime: prepared.candidate.regime.clone(),
            ..BasketMetrics::default()
        };
        return finish(state, prepared, metrics, None);
    }

    let interval_ms = history::interval_ms(&prepared.candidate.timeframe)
        .ok_or_else(|| format!("不支持的回测周期: {}", prepared.candidate.timeframe))?;
    // 排除当前未收盘 bar，并按 warmup 扩窗后再截取最近 BARS_LIMIT 根。
    let (start_ms, end_ms) = aligned_window(crate::now_millis(), interval_ms);
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
    // 体检必须针对候选参数本身：活动基线可能长期通过体检，却掩盖候选的过拟合风险。
    let checks = overfit_checks(&basket, &candidate_config);
    let mut metrics = run_metrics(
        basket.clone(),
        candidate_config,
        regime,
        prepared.candidate.timeframe.clone(),
        prepared.trials,
    )
    .await?;
    metrics.lookahead_suspicious = checks.lookahead_suspicious;
    metrics.lookahead_max_delta_pct = checks.lookahead_max_delta_pct;
    metrics.recursive_suspicious = checks.recursive_suspicious;
    merge_skipped(&mut metrics, &data_skipped);

    let baseline = match &prepared.baseline {
        Some(set) => {
            let config = build_config(&set.params, &prepared.risk, &prepared.candidate.timeframe)?;
            let mut baseline_metrics = run_metrics(
                basket,
                config,
                regime,
                prepared.candidate.timeframe.clone(),
                prepared.trials,
            )
            .await?;
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
    let baseline_oos_avg = baseline.as_ref().map(|value| value.oos_avg_return_pct);
    let report = evidence::evaluate_gate(
        &metrics,
        prepared.min_oos_trades,
        &prepared.live_timeframe,
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
    let candidate = ai_tuning::load_set(&conn, candidate_id)?;
    if !matches!(candidate.source.as_str(), "ai" | "ga") || candidate.status != "candidate" {
        return Ok(None);
    }
    let strategy = group_json(&conn, "strategy")?;
    let risk = group_json(&conn, "risk")?;
    let ai_tuning = group_json(&conn, "ai_tuning")?;
    let live_timeframe = strategy
        .get("timeframe")
        .and_then(Value::as_str)
        .ok_or_else(|| "strategy.timeframe 缺失或非法".to_string())?
        .to_string();
    let min_oos_trades = ai_tuning
        .get("min_oos_trades")
        .and_then(Value::as_f64)
        .filter(|value| *value >= 1.0)
        .map(|value| value as usize)
        .unwrap_or(DEFAULT_MIN_OOS_TRADES as usize);
    // 同一 regime×timeframe 的候选总数即多重试验次数：试得越多，DSR 门槛越高。
    let trials = conn
        .query_row(
            "SELECT COUNT(*) FROM ai_tuning_sets WHERE regime=?1 AND timeframe=?2",
            (&candidate.regime, &candidate.timeframe),
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| format!("统计调参试验次数失败: {error}"))?
        .max(1) as usize;

    // 同 regime 的 active 作为基线；该 regime 没有 active 时回退 range-bootstrap。
    let mut baseline = ai_tuning::active_for_regime(&conn, &candidate.regime)?;
    if baseline.is_none() {
        baseline = ai_tuning::active_for_regime(&conn, "range")?;
    }
    let symbols = pool_symbols(&conn)?;
    Ok(Some(Prepared {
        candidate,
        baseline,
        symbols,
        risk,
        live_timeframe,
        min_oos_trades,
        trials,
    }))
}

struct OverfitChecks {
    lookahead_suspicious: bool,
    /// 多档前视的最大 delta，百分比（12.5 表示 12.5%）。
    lookahead_max_delta_pct: f64,
    recursive_suspicious: bool,
}

/// 过拟合体检：对每个标的跑多档前视偏移（1/2/3 根）与递归 warmup 检测。
fn overfit_checks(basket: &[(String, Vec<HistoryBar>)], config: &EngineConfig) -> OverfitChecks {
    const LOOKAHEAD_SHIFTS: [usize; 3] = [1, 2, 3];
    let mut lookahead_reports = Vec::new();
    let mut max_divergence = 0.0_f64;
    for (_, bars) in basket {
        for shift in LOOKAHEAD_SHIFTS {
            if let Ok(report) = backtest_engine::run_lookahead(bars, config, shift) {
                if report.suspicious {
                    tracing::warn!(
                        shift,
                        baseline = report.baseline_expectancy,
                        shifted = report.shifted_expectancy,
                        delta_pct = report.delta_pct,
                        reason = %report.reason,
                        "前视偏差体检可疑"
                    );
                }
                lookahead_reports.push(report);
            }
        }
        let candles = backtest_engine::candles_from_history(bars, config.interval_ms);
        let divergence = backtest_engine::measure_recursive_divergence(&candles, config.atr_period);
        if divergence > max_divergence {
            max_divergence = divergence;
        }
    }
    let (lookahead_suspicious, lookahead_max_delta) =
        crate::overfit::summarize_lookahead(&lookahead_reports);
    let recursive = crate::overfit::evaluate_recursive_divergence(max_divergence);
    if recursive.suspicious {
        tracing::warn!(
            divergence = recursive.max_divergence_pct,
            reason = %recursive.reason,
            "递归指标稳定性体检可疑"
        );
    }
    OverfitChecks {
        lookahead_suspicious,
        lookahead_max_delta_pct: lookahead_max_delta * 100.0,
        recursive_suspicious: recursive.suspicious,
    }
}

pub(crate) fn pool_symbols(conn: &Connection) -> Result<Vec<String>, String> {
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

/// 请求窗口：结束时间对齐到上一根已收盘 bar，起点额外覆盖 2 段 warmup。
pub(crate) fn aligned_window(now_ms: i64, step_ms: i64) -> (i64, i64) {
    let step = step_ms.max(1);
    let end = (now_ms / step) * step - step;
    let requested = BARS_LIMIT + 2 * backtest_engine::WARMUP as i64;
    (end - requested * step, end)
}

/// 只保留最近 `BARS_LIMIT` 根；升序时间序列直接截尾部。
fn trim_bars(bars: Vec<HistoryBar>) -> Vec<HistoryBar> {
    let keep = BARS_LIMIT as usize;
    if bars.len() <= keep {
        return bars;
    }
    bars[bars.len() - keep..].to_vec()
}

pub(crate) async fn load_basket(
    source: &dyn HistorySource,
    symbols: &[String],
    timeframe: &str,
    start_ms: i64,
    end_ms: i64,
) -> (Vec<(String, Vec<HistoryBar>)>, Vec<(String, String)>) {
    let mut basket = Vec::new();
    let mut skipped = Vec::new();
    for symbol in symbols {
        match source.load(symbol, timeframe, start_ms, end_ms).await {
            Ok(series) if series.truncated => {
                skipped.push((symbol.clone(), "历史区间被截断".to_string()));
            }
            Ok(series) if series.bars.len() < BARS_LIMIT as usize => {
                skipped.push((
                    symbol.clone(),
                    format!("历史 K 线不足: {} < {BARS_LIMIT}", series.bars.len()),
                ));
            }
            Ok(series) => {
                if !series.gaps.is_empty() {
                    // 缺口只记录不插值，也不因此排除标的；回测内核按缺口原样跳过。
                    tracing::info!(
                        symbol = %symbol,
                        gaps = series.gaps.len(),
                        "证据门历史存在缺口，按原样回测"
                    );
                }
                basket.push((symbol.clone(), trim_bars(series.bars)));
            }
            Err(error) => {
                skipped.push((symbol.clone(), format!("历史不可用: {error}")));
            }
        }
    }
    (basket, skipped)
}

fn merge_skipped(metrics: &mut BasketMetrics, data_skipped: &[(String, String)]) {
    for (symbol, reason) in data_skipped {
        tracing::info!(symbol = %symbol, reason = %reason, "证据门跳过标的");
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
    trials: usize,
) -> Result<BasketMetrics, String> {
    tokio::task::spawn_blocking(move || {
        let cancel = AtomicBool::new(false);
        backtest_engine::run_walkforward_basket_fixed(
            &basket, &config, FOLDS, TRAIN_BARS, regime, &timeframe, trials, &cancel,
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
        fill: std::sync::Arc::new(crate::behavior::DefaultFill {
            slippage_bps: 2.0,
            spread_bps: 0.0,
        }),
        fee: std::sync::Arc::new(crate::behavior::DefaultFee {
            taker_bps: taker * 10_000.0,
        }),
        margin: std::sync::Arc::new(crate::behavior::DefaultMargin {
            maintenance_margin_rate: 0.005,
        }),
        latency: std::sync::Arc::new(crate::behavior::NoLatency),
        funding_bps_8h: 1.0,
        funding_points: Vec::new(),
        initial_equity: 100.0,

        margin_pct: risk
            .get("margin_pct")
            .and_then(Value::as_f64)
            .unwrap_or(50.0),
        leverage: risk
            .get("min_leverage")
            .and_then(Value::as_f64)
            .unwrap_or(1.0),
        max_bars_hold: None,
        policy,
        manage,
        stop_distance_atr_mult: risk
            .get("stop_distance_atr_mult")
            .and_then(Value::as_f64)
            .unwrap_or(1.3),
        atr_period: crate::ai_analysis::atr_period_of(params),
        mr: params.clone(),
        signal_shift_bars: 0,
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
    // 状态与事件必须同进同退：事件写失败时回滚，candidate 保持待评估。
    let tx = conn
        .unchecked_transaction()
        .map_err(|error| format!("开启证据门事务失败: {error}"))?;
    ai_tuning::mark_evidence(&tx, set_id, report.pass, evidence)?;
    let mut auto_activated = false;
    if report.pass {
        // 证据门通过后直接激活，不再等待人工点击。
        // 只有「当前市场有效 regime」的候选才能立即物化 strategy：
        // trend 无独立集时按 range 回退，因此侯选 regime 为 range 也可直接生效。
        let set = ai_tuning::load_set(&tx, set_id)?;
        let mode = crate::journal::current_mode(&tx);
        let current_regime: String = tx
            .query_row(
                "SELECT value FROM engine_state WHERE key=?1",
                [format!("regime.current.{mode}")],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())?
            .unwrap_or_default();
        let effective_for_current = current_regime.is_empty()
            || set.regime == current_regime
            || (current_regime == "trend" && set.regime == "range");
        if effective_for_current {
            match ai_tuning::activate(&tx, set_id, "evidence", &mode) {
                Ok(()) => auto_activated = true,
                Err(error) => {
                    // 激活失败保留 approved，不阻断证据结果落库，之后仍可手动启用。
                    record_event(
                        &tx,
                        "warn",
                        "AI 调参自动激活失败",
                        json!({
                            "event": "ai_tuning.auto_activate_failed",
                            "set_id": set_id,
                            "error": error,
                        }),
                    )?;
                }
            }
        } else {
            record_event(
                &tx,
                "info",
                "AI 调参候选跨 regime，等待切换后生效",
                json!({
                    "event": "ai_tuning.auto_activate_deferred",
                    "set_id": set_id,
                    "candidate_regime": set.regime,
                    "current_regime": current_regime,
                }),
            )?;
        }
    }
    let (level, message, event) = if report.pass {
        ("info", "AI 调参证据门通过", "ai_tuning.evidence_pass")
    } else {
        ("warn", "AI 调参证据门未通过", "ai_tuning.evidence_fail")
    };
    record_event(
        &tx,
        level,
        message,
        json!({
            "event": event,
            "set_id": set_id,
            "metrics": evidence["candidate"],
            "baseline_set_id": evidence["baseline_set_id"],
            "reasons": report.reasons,
            "auto_activated": auto_activated,
        }),
    )?;
    tx.commit()
        .map_err(|error| format!("提交证据门事务失败: {error}"))
}

fn record_failed(state: &AiState, set_id: i64, error: &str) {
    let detail = crate::tuning::sanitize_error(error);
    tracing::warn!(set_id, error = %detail, "候选AI 调参证据门执行失败");
    let Ok(conn) = state.db.conn.lock() else {
        return;
    };
    // 只有回滚后仍是 candidate 才写失败事件；已 approved/rejected 的不能再被覆盖语义。
    let status: Option<String> = conn
        .query_row(
            "SELECT status FROM ai_tuning_sets WHERE id=?1",
            [set_id],
            |row| row.get(0),
        )
        .optional()
        .ok()
        .flatten();
    if status.as_deref() != Some("candidate") {
        return;
    }
    let _ = record_event(
        &conn,
        "warn",
        "AI 调参证据门执行失败",
        json!({
            "event": "ai_tuning.evidence_failed",
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
    .map_err(|error| format!("写入AI 调参证据事件失败: {error}"))?;
    Ok(())
}

/// 配置组读取：缺行回退默认值，损坏 JSON 直接 Err（走 evidence_failed，不判 rejected）。
fn group_json(conn: &Connection, key: &str) -> Result<Value, String> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM app_settings WHERE key=?1",
            [key],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|error| format!("读取配置组 {key} 失败: {error}"))?;
    match raw {
        None => Ok(crate::settings_api::defaults_of(key)),
        Some(text) => serde_json::from_str::<Value>(&text)
            .map_err(|error| format!("配置组 {key} 不是合法 JSON: {error}")),
    }
}

fn is_live_mode(mode: &str) -> bool {
    let value = mode.trim();
    value.eq_ignore_ascii_case("live") || value.eq_ignore_ascii_case("mainnet")
}

fn live_mode(state: &AiState) -> Result<bool, String> {
    let conn = state
        .db
        .conn
        .lock()
        .map_err(|_| "数据库连接锁不可用".to_string())?;
    Ok(is_live_mode(&current_mode(&conn)?))
}

/// `engine_state.mode` 优先，其次 `engine.mode`，最后 demo。
fn current_mode(conn: &Connection) -> Result<String, String> {
    let stored = conn
        .query_row(
            "SELECT value FROM engine_state WHERE key='mode'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|error| format!("读取交易模式失败: {error}"))?
        .filter(|mode| !mode.trim().is_empty());
    if let Some(mode) = stored {
        return Ok(mode);
    }
    Ok(group_json(conn, "engine")?
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("demo")
        .to_string())
}

/// 证据门扫描节流：`ai_tuning.evidence_sweep_ts` 每 6h 最多一次。
pub(crate) fn sweep_due(state: &AiState) -> bool {
    let Ok(conn) = state.db.conn.lock() else {
        return false;
    };
    sweep_due_at(&conn, crate::now()).unwrap_or(false)
}

fn sweep_due_at(conn: &Connection, now: i64) -> Result<bool, String> {
    let last: Option<i64> = conn
        .query_row(
            "SELECT value FROM engine_state WHERE key='ai_tuning.evidence_sweep_ts'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|error| format!("读取证据门扫描时间失败: {error}"))?
        .and_then(|raw| raw.parse::<i64>().ok());
    if last
        .map(|ts| now - ts < SWEEP_INTERVAL_SEC)
        .unwrap_or(false)
    {
        return Ok(false);
    }
    conn.execute(
        "INSERT INTO engine_state(key,value,updated_at) \
         VALUES('ai_tuning.evidence_sweep_ts',?1,?1) \
         ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
        [now.to_string()],
    )
    .map_err(|error| format!("写入证据门扫描时间失败: {error}"))?;
    Ok(true)
}

/// 选取最近 7 天内、冷却已过的最早待评估候选（`limit` 由调用方控制）。
fn select_pending(conn: &Connection, limit: usize, now: i64) -> Result<Vec<i64>, String> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let window_start = now - PENDING_WINDOW_SEC;
    let cooldown_before = now - ATTEMPT_COOLDOWN_SEC;
    let mut statement = conn
        .prepare(
            "SELECT p.id FROM ai_tuning_sets p \
             LEFT JOIN engine_state e \
               ON e.key = 'ai_tuning.evidence_attempt.' || p.id \
             WHERE p.source IN ('ai','ga') AND p.status='candidate' AND p.created_at > ?1 \
               AND (e.value IS NULL OR CAST(e.value AS INTEGER) <= ?2) \
             ORDER BY p.id ASC LIMIT ?3",
        )
        .map_err(|error| format!("查询待评估候选失败: {error}"))?;
    let rows = statement
        .query_map(
            params![window_start, cooldown_before, limit as i64],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| format!("查询待评估候选失败: {error}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("查询待评估候选失败: {error}"))
}

fn write_attempt(conn: &Connection, id: i64, now: i64) -> Result<(), String> {
    conn.execute(
        "INSERT INTO engine_state(key,value,updated_at) VALUES(?1,?2,?2) \
         ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
        params![format!("ai_tuning.evidence_attempt.{id}"), now.to_string()],
    )
    .map_err(|error| format!("写入证据门尝试时间失败: {error}"))?;
    Ok(())
}

/// 扫描重试入口：失败候选保持 candidate，由 6h 冷却后的下一轮扫描继续尝试。
pub(crate) async fn sweep_pending(state: &AiState, limit: usize) -> Result<Option<i64>, String> {
    let now = crate::now();
    let ids = {
        let conn = state
            .db
            .conn
            .lock()
            .map_err(|_| "数据库连接锁不可用".to_string())?;
        select_pending(&conn, limit, now)?
    };
    let mut first = None;
    for id in ids {
        // attempt 由 evaluate 入口统一写入，扫描侧不再重复写。
        evaluate_candidate(state.clone(), id).await?;
        if first.is_none() {
            first = Some(id);
        }
    }
    Ok(first)
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

                "margin_pct": 50.0,
                "min_leverage": 1.0
            })
            .to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO app_settings VALUES('ai_tuning',?1,0)",
            [json!({
                "enabled": true,

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
            let conn = state.db.lock();
            crate::ai_tuning::seed_bootstrap(&conn).unwrap();
            let parent = crate::ai_tuning::active_for_regime(&conn, "range")
                .unwrap()
                .unwrap();
            let mut snapshot = parent.params.clone();
            snapshot["atr_pct_min"] = json!(0.7);
            snapshot["timeframe"] = json!(timeframe);
            crate::ai_tuning::insert_candidate(
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
        let conn = state.db.lock();
        let mut statement = conn
            .prepare("SELECT payload FROM engine_events WHERE category='ai_tuning' ORDER BY id")
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
            "entry_threshold": 0.55
        });
        let risk = json!({
            "fee_rate_taker": 0.0005,

            "margin_pct": 50.0,
            "min_leverage": 1.0
        });

        let config = build_config(&params, &risk, "1h").unwrap();

        assert_eq!(config.interval_ms, 3_600_000);
        assert!(
            (config.fee.taker_rate() - 0.0005).abs() < 1e-9,
            "费率必须按 risk 折算"
        );
        assert_eq!(config.policy.atr_pct_min, 1.5);
        assert_eq!(config.policy.atr_pct_max, 8.0);
        assert_eq!(config.policy.volume_ratio_min, 1.1);
        assert!((config.fill.slip_bps() - 2.0).abs() < 1e-9);
        assert_eq!(config.funding_bps_8h, 1.0);
        assert_eq!(config.initial_equity, 100.0);

        assert_eq!(config.margin_pct, 50.0);
        assert_eq!(config.leverage, 1.0);
        assert_eq!(config.margin.maintenance_margin_rate(), 0.005);
        assert!(config.max_bars_hold.is_none());
        assert!(
            build_config(&params, &risk, "3d").is_err(),
            "不支持的周期必须报错，不能猜默认值"
        );
    }

    #[test]
    fn passing_evidence_auto_activates_candidate() {
        let (state, candidate_id) = setup("1h");
        let evidence = json!({
            "gate": {"pass": true},
            "candidate": {},
            "baseline_set_id": null,
        });
        let report = EvidenceReport {
            pass: true,
            reasons: Vec::new(),
        };

        write_outcome(&state, candidate_id, &report, &evidence).unwrap();

        let conn = state.db.lock();
        let set = crate::ai_tuning::load_set(&conn, candidate_id).unwrap();
        assert_eq!(set.status, "active");
        assert!(set.activated_at.is_some(), "证据门通过后应自动激活");
        let active = crate::ai_tuning::active_for_regime(&conn, "range")
            .unwrap()
            .unwrap();
        assert_eq!(active.id, candidate_id, "候选应替换为当前 active 集");
        let trigger: String = conn
            .query_row(
                "SELECT trigger FROM ai_tuning_activations WHERE set_id=?1 \
                 ORDER BY id DESC LIMIT 1",
                [candidate_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(trigger, "evidence");
        let strategy: String = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key='strategy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let strategy: Value = serde_json::from_str(&strategy).unwrap();
        assert_eq!(strategy["atr_pct_min"], set.params["atr_pct_min"]);
    }

    #[test]
    fn cross_regime_evidence_pass_defers_activation() {
        let (state, candidate_id) = setup("1h");
        {
            let conn = state.db.lock();
            conn.execute(
                "INSERT OR REPLACE INTO engine_state(key,value,updated_at) \
                 VALUES('regime.current.demo','chaos',0)",
                [],
            )
            .unwrap();
        }
        let evidence = json!({
            "gate": {"pass": true},
            "candidate": {},
            "baseline_set_id": null,
        });
        let report = EvidenceReport {
            pass: true,
            reasons: Vec::new(),
        };

        write_outcome(&state, candidate_id, &report, &evidence).unwrap();

        let conn = state.db.lock();
        let set = crate::ai_tuning::load_set(&conn, candidate_id).unwrap();
        assert_eq!(set.status, "approved", "跨 regime 候选只落证据，不物化");
        assert!(set.activated_at.is_none());
        let strategy: String = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key='strategy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let strategy: Value = serde_json::from_str(&strategy).unwrap();
        assert_eq!(strategy["atr_pct_min"], 1.5, "当前 regime 的策略不得被改写");
        drop(conn);
        let payloads = events(&state);
        assert!(
            payloads
                .iter()
                .any(|payload| payload["event"] == "ai_tuning.auto_activate_deferred"),
            "跨 regime 候选应记录等待切换事件"
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
        let conn = state.db.lock();
        let set = crate::ai_tuning::load_set(&conn, candidate_id).unwrap();
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
        assert_eq!(payloads[0]["event"], "ai_tuning.evidence_fail");
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
        let conn = state.db.lock();
        let set = crate::ai_tuning::load_set(&conn, candidate_id).unwrap();
        assert_eq!(set.status, "candidate", "数据失败时候选必须保持待评估");
        assert_eq!(set.evidence, json!({}), "数据失败不得写证据");
        drop(conn);
        let payloads = events(&state);
        assert!(
            payloads
                .iter()
                .any(|payload| payload["event"] == "ai_tuning.evidence_failed"),
            "数据失败必须留审计事件"
        );
        assert!(!payloads
            .iter()
            .any(|payload| payload["event"] == "ai_tuning.evidence_pass"
                || payload["event"] == "ai_tuning.evidence_fail"));
    }

    #[tokio::test]
    async fn non_candidate_is_skipped_without_fetching_history() {
        let (state, candidate_id) = setup("1h");
        state
            .db
            .lock()
            .execute(
                "UPDATE ai_tuning_sets SET status='approved' WHERE id=?1",
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

    fn bar(open_time: i64) -> HistoryBar {
        HistoryBar {
            open_time,
            close_time: open_time + 3_599_999,
            open: 1.0,
            high: 1.0,
            low: 1.0,
            close: 1.0,
            volume: 1.0,
        }
    }

    #[test]
    fn aligned_window_excludes_current_bar_and_covers_warmup() {
        let step = 3_600_000;
        let now = 10 * step + 1_234;
        let (start, end) = aligned_window(now, step);

        assert_eq!(end, 9 * step, "end 必须是上一根已收盘 bar");
        assert!(end < now);
        let requested = BARS_LIMIT + 2 * crate::backtest_engine::WARMUP as i64;
        assert_eq!(end - start, requested * step, "窗口要额外覆盖 warmup");
    }

    #[test]
    fn trim_bars_keeps_newest_limit() {
        let bars: Vec<HistoryBar> = (0..BARS_LIMIT + 7).map(bar).collect();
        let trimmed = trim_bars(bars);

        assert_eq!(trimmed.len(), BARS_LIMIT as usize);
        assert_eq!(trimmed.first().unwrap().open_time, 7);
        assert_eq!(trimmed.last().unwrap().open_time, BARS_LIMIT + 6);

        let short = vec![bar(1), bar(2)];
        assert_eq!(trim_bars(short).len(), 2, "不足上限时原样保留");
    }

    #[test]
    fn live_mode_recognizes_aliases_case_insensitively() {
        assert!(is_live_mode("live"));
        assert!(is_live_mode("MAINNET"));
        assert!(is_live_mode(" Mainnet "));
        assert!(!is_live_mode("demo"));
        assert!(!is_live_mode(""));
    }

    #[test]

    fn select_pending_respects_window_and_cooldown() {
        let (state, candidate_id) = setup("1h");
        let conn = state.db.lock();
        let now = crate::now();

        assert_eq!(select_pending(&conn, 1, now).unwrap(), vec![candidate_id]);
        write_attempt(&conn, candidate_id, now).unwrap();
        assert!(
            select_pending(&conn, 1, now).unwrap().is_empty(),
            "6h 冷却期内不应重试"
        );
        assert_eq!(
            select_pending(&conn, 1, now + ATTEMPT_COOLDOWN_SEC).unwrap(),
            vec![candidate_id],
            "冷却结束后应重新入选"
        );

        conn.execute(
            "DELETE FROM engine_state WHERE key=?1",
            [format!("ai_tuning.evidence_attempt.{candidate_id}")],
        )
        .unwrap();
        conn.execute(
            "UPDATE ai_tuning_sets SET created_at=?1 WHERE id=?2",
            params![now - PENDING_WINDOW_SEC - 1, candidate_id],
        )
        .unwrap();
        assert!(
            select_pending(&conn, 1, now).unwrap().is_empty(),
            "7 天窗口外的候选不参与扫描"
        );
    }

    #[test]
    fn sweep_due_throttles_to_six_hours() {
        let (state, _) = setup("1h");
        let conn = state.db.lock();
        let now = crate::now();

        assert!(sweep_due_at(&conn, now).unwrap());
        assert!(!sweep_due_at(&conn, now + 60).unwrap(), "6h 内不应重复扫描");
        assert!(sweep_due_at(&conn, now + SWEEP_INTERVAL_SEC).unwrap());
    }

    #[test]
    fn missing_ai_tuning_row_falls_back_to_defaults() {
        let (state, candidate_id) = setup("1h");
        {
            let conn = state.db.lock();
            conn.execute("DELETE FROM app_settings WHERE key='ai_tuning'", [])
                .unwrap();
        }

        let prepared = prepare(&state, candidate_id).unwrap().unwrap();

        assert_eq!(prepared.min_oos_trades, 30, "缺配置时默认 30 笔");
        assert_eq!(prepared.live_timeframe, "1h", "实盘周期照旧读取");
    }

    #[test]
    fn candidate_health_config_follows_the_candidate_params() {
        let (state, candidate_id) = setup("1h");

        let prepared = prepare(&state, candidate_id).unwrap().unwrap();
        let config = build_config(&prepared.candidate.params, &prepared.risk, "1h").unwrap();

        assert_eq!(
            config.policy.atr_pct_min, 0.7,
            "过拟合体检必须跟随候选参数，而不是线上/活动基线"
        );
    }

    #[test]
    fn prepare_counts_trials_for_the_same_regime_and_timeframe() {
        let (state, candidate_id) = setup("1h");

        let prepared = prepare(&state, candidate_id).unwrap().unwrap();
        let expected = {
            let conn = state.db.lock();
            conn.query_row(
                "SELECT COUNT(*) FROM ai_tuning_sets WHERE regime='range' AND timeframe='1h'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
        };

        assert_eq!(prepared.trials, expected.max(1) as usize);
    }

    #[tokio::test]
    async fn corrupt_ai_tuning_returns_error_and_keeps_candidate() {
        let (state, candidate_id) = setup("1h");
        {
            let conn = state.db.lock();
            conn.execute(
                "UPDATE app_settings SET value='{invalid' WHERE key='ai_tuning'",
                [],
            )
            .unwrap();
        }
        let history = FakeHistory::unavailable();

        let result = evaluate_with_history(&state, candidate_id, &history).await;

        assert!(
            result.is_err(),
            "损坏配置必须走 evidence_failed 而非 gate 失败"
        );
        assert_eq!(history.calls(), 0);
        let conn = state.db.lock();
        let set = crate::ai_tuning::load_set(&conn, candidate_id).unwrap();
        assert_eq!(set.status, "candidate", "配置错误不得把候选永久拒绝");
        assert_eq!(set.evidence, json!({}));
    }

    #[tokio::test]
    async fn event_failure_rolls_back_status_and_keeps_candidate() {
        let (state, candidate_id) = setup("4h");
        {
            let conn = state.db.lock();
            conn.execute_batch(
                "CREATE TRIGGER block_ai_tuning_events BEFORE INSERT ON engine_events \
                 BEGIN SELECT RAISE(ABORT, 'blocked'); END;",
            )
            .unwrap();
        }
        let history = FakeHistory::unavailable();

        let result = evaluate_with_history(&state, candidate_id, &history).await;

        assert!(result.is_err(), "事件写失败必须整体失败");
        assert_eq!(history.calls(), 0);
        let conn = state.db.lock();
        let set = crate::ai_tuning::load_set(&conn, candidate_id).unwrap();
        assert_eq!(set.status, "candidate", "事务回滚后候选必须保持待评估");
        assert_eq!(set.evidence, json!({}), "回滚不得留下证据");
        drop(conn);
        assert!(events(&state).is_empty(), "被触发器拦截的事件不应落库");
    }

    #[tokio::test]
    async fn evaluate_records_attempt_and_sweep_skips_it() {
        // 4h 候选走静态 gate 失败，不触网。
        let (state, candidate_id) = setup("4h");
        let history = FakeHistory::unavailable();

        evaluate_with_history(&state, candidate_id, &history)
            .await
            .unwrap();

        let conn = state.db.lock();
        let now = crate::now();
        let attempt: String = conn
            .query_row(
                "SELECT value FROM engine_state WHERE key=?1",
                [format!("ai_tuning.evidence_attempt.{candidate_id}")],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            (attempt.parse::<i64>().unwrap() - now).abs() <= 2,
            "评估入口必须记录 attempt"
        );
        assert!(
            select_pending(&conn, 1, now).unwrap().is_empty(),
            "同轮 sweep 不得重复选中刚评估的 id"
        );
    }

    #[tokio::test]
    async fn failed_evaluation_still_records_attempt() {
        let (state, candidate_id) = setup("1h");
        let history = FakeHistory::unavailable();

        assert!(evaluate_with_history(&state, candidate_id, &history)
            .await
            .is_err());

        let conn = state.db.lock();
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM engine_state WHERE key=?1",
                [format!("ai_tuning.evidence_attempt.{candidate_id}")],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(exists, 1, "失败候选同样进入 6h 冷却");
    }
}
