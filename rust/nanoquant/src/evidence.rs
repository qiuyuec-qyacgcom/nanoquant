use serde_json::{json, Value};

use crate::backtest_engine::BasketMetrics;

const MIN_SYMBOLS: usize = 6;
const MIN_FOLDS: usize = 2;
const MIN_OVERFIT_GAP_PP: f64 = 1.0;
const MIN_BOOTSTRAP_P_POSITIVE: f64 = 0.90;
const MAX_DRAWDOWN_PCT: f64 = 15.0;
const MIN_RELATIVE_ADVANTAGE_PP: f64 = 0.10;

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(crate) struct EvidenceReport {
    pub pass: bool,
    pub reasons: Vec<String>,
}

/// 证据门判定：所有阈值均为代码常量，AI 与页面不可改（规格 §7.3/§10）。
/// 任一条件失败即 `pass=false`，且全部失败原因都会收集到 `reasons`。
#[allow(dead_code)]
pub(crate) fn evaluate_gate(
    metrics: &BasketMetrics,
    min_oos_trades: usize,
    live_timeframe: &str,
    auto_apply_timeframes: &[&str],
    baseline_oos_avg: Option<f64>,
) -> EvidenceReport {
    let mut reasons = Vec::new();
    if metrics.timeframe != live_timeframe {
        reasons.push(format!(
            "timeframe_mismatch: {} != {}",
            metrics.timeframe, live_timeframe
        ));
    }
    if !auto_apply_timeframes.contains(&metrics.timeframe.as_str()) {
        reasons.push(format!("timeframe_not_whitelisted: {}", metrics.timeframe));
    }
    if metrics.oos_trades < min_oos_trades {
        reasons.push(format!(
            "insufficient_oos_trades: {} < {}",
            metrics.oos_trades, min_oos_trades
        ));
    }
    if metrics.folds < MIN_FOLDS {
        reasons.push(format!(
            "insufficient_folds: {} < {}",
            metrics.folds, MIN_FOLDS
        ));
    }
    if metrics.positive_folds * 3 < metrics.folds * 2 {
        reasons.push(format!(
            "insufficient_positive_folds: {}/{} below 2/3",
            metrics.positive_folds, metrics.folds
        ));
    }
    if metrics.oos_avg_return_pct <= 0.0 {
        reasons.push(format!(
            "non_positive_oos_avg: {}",
            metrics.oos_avg_return_pct
        ));
    }
    if metrics.overfit_gap_pp > MIN_OVERFIT_GAP_PP {
        reasons.push(format!(
            "overfit_gap_exceeded: {} > {}",
            metrics.overfit_gap_pp, MIN_OVERFIT_GAP_PP
        ));
    }
    if metrics.bootstrap_ci_low <= 0.0 {
        reasons.push(format!(
            "bootstrap_ci_low_not_positive: {}",
            metrics.bootstrap_ci_low
        ));
    }
    if metrics.bootstrap_p_positive < MIN_BOOTSTRAP_P_POSITIVE {
        reasons.push(format!(
            "bootstrap_p_positive_low: {} < {}",
            metrics.bootstrap_p_positive, MIN_BOOTSTRAP_P_POSITIVE
        ));
    }
    if metrics.stressed_avg_return_pct < 0.0 {
        reasons.push(format!(
            "stressed_avg_negative: {}",
            metrics.stressed_avg_return_pct
        ));
    }
    if metrics.max_drawdown_pct > MAX_DRAWDOWN_PCT {
        reasons.push(format!(
            "max_drawdown_exceeded: {} > {}",
            metrics.max_drawdown_pct, MAX_DRAWDOWN_PCT
        ));
    }
    if let Some(baseline) = baseline_oos_avg {
        let advantage = metrics.oos_avg_return_pct - baseline;
        if advantage < MIN_RELATIVE_ADVANTAGE_PP {
            reasons.push(format!(
                "relative_advantage_low: {advantage} < {MIN_RELATIVE_ADVANTAGE_PP}"
            ));
        }
    }
    if metrics.symbols_used.len() < MIN_SYMBOLS {
        reasons.push(format!(
            "insufficient_symbols: {} < {}",
            metrics.symbols_used.len(),
            MIN_SYMBOLS
        ));
    }
    EvidenceReport {
        pass: reasons.is_empty(),
        reasons,
    }
}

#[allow(dead_code)]
pub(crate) fn evidence_json(
    metrics: &BasketMetrics,
    report: &EvidenceReport,
    baseline_set_id: Option<i64>,
    baseline_oos_avg: Option<f64>,
) -> Value {
    let relative_advantage_pp =
        baseline_oos_avg.map(|baseline| round(metrics.oos_avg_return_pct - baseline, 4));
    json!({
        "version": 1,
        "generated_at": crate::now(),
        "timeframe": metrics.timeframe,
        "target_regime": metrics.target_regime,
        "symbols_used": metrics.symbols_used,
        "symbols_skipped": metrics.symbols_skipped,
        "folds": metrics.folds,
        "candidate": {
            "oos_trades": metrics.oos_trades,
            "positive_folds": metrics.positive_folds,
            "oos_avg_return_pct": metrics.oos_avg_return_pct,
            "overfit_gap_pp": metrics.overfit_gap_pp,
            "stressed_avg_return_pct": metrics.stressed_avg_return_pct,
            "max_drawdown_pct": metrics.max_drawdown_pct,
            "bootstrap_ci_low": metrics.bootstrap_ci_low,
            "bootstrap_p_positive": metrics.bootstrap_p_positive,
        },
        "baseline_set_id": baseline_set_id,
        "baseline_oos_avg": baseline_oos_avg,
        "relative_advantage_pp": relative_advantage_pp,
        "gate": {
            "pass": report.pass,
            "reasons": report.reasons,
        },
    })
}

fn round(value: f64, digits: u32) -> f64 {
    let factor = 10_f64.powi(digits as i32);
    (value * factor).round() / factor
}

#[cfg(test)]
mod tests {
    use super::*;

    fn symbols(count: usize) -> Vec<String> {
        (0..count).map(|index| format!("SYM{index}USDT")).collect()
    }

    fn passing_metrics() -> BasketMetrics {
        BasketMetrics {
            folds: 6,
            oos_trades: 30,
            positive_folds: 4,
            oos_avg_return_pct: 0.4,
            overfit_gap_pp: 1.0,
            stressed_avg_return_pct: 0.0,
            max_drawdown_pct: 15.0,
            bootstrap_ci_low: 0.01,
            bootstrap_p_positive: 0.90,
            symbols_used: symbols(6),
            symbols_skipped: Vec::new(),
            target_regime: "range".to_string(),
            timeframe: "1h".to_string(),
        }
    }

    fn gate(metrics: &BasketMetrics, baseline: Option<f64>) -> EvidenceReport {
        evaluate_gate(metrics, 30, "1h", &["1h"], baseline)
    }

    #[test]
    fn evidence_gate_passes_with_full_evidence() {
        let report = gate(&passing_metrics(), Some(0.2));
        assert!(report.pass, "reasons: {:?}", report.reasons);
        assert!(report.reasons.is_empty());
    }

    #[test]
    fn evidence_gate_fails_at_each_threshold_boundary() {
        let mut metrics = passing_metrics();
        metrics.oos_trades = 29;
        assert!(!gate(&metrics, Some(0.2)).pass);

        let mut metrics = passing_metrics();
        metrics.positive_folds = 3;
        assert!(!gate(&metrics, Some(0.2)).pass);

        let mut metrics = passing_metrics();
        metrics.overfit_gap_pp = 1.01;
        assert!(!gate(&metrics, Some(0.2)).pass);

        let mut metrics = passing_metrics();
        metrics.bootstrap_ci_low = 0.0;
        assert!(!gate(&metrics, Some(0.2)).pass);

        let mut metrics = passing_metrics();
        metrics.bootstrap_p_positive = 0.89;
        assert!(!gate(&metrics, Some(0.2)).pass);

        let mut metrics = passing_metrics();
        metrics.stressed_avg_return_pct = -0.01;
        assert!(!gate(&metrics, Some(0.2)).pass);

        let mut metrics = passing_metrics();
        metrics.max_drawdown_pct = 15.01;
        assert!(!gate(&metrics, Some(0.2)).pass);

        let metrics = passing_metrics();
        assert!(!gate(&metrics, Some(0.301)).pass);

        let mut metrics = passing_metrics();
        metrics.symbols_used = symbols(5);
        assert!(!gate(&metrics, Some(0.2)).pass);

        let mut metrics = passing_metrics();
        metrics.timeframe = "4h".to_string();
        assert!(!gate(&metrics, Some(0.2)).pass);

        let metrics = passing_metrics();
        let report = evaluate_gate(&metrics, 30, "1h", &[], Some(0.2));
        assert!(!report.pass);
    }

    #[test]
    fn evidence_gate_collects_all_failed_reasons() {
        let mut metrics = passing_metrics();
        metrics.oos_trades = 29;
        metrics.symbols_used = symbols(5);
        metrics.timeframe = "4h".to_string();
        let report = gate(&metrics, Some(0.301));
        assert!(!report.pass);
        assert_eq!(report.reasons.len(), 5, "reasons: {:?}", report.reasons);
        assert!(report
            .reasons
            .iter()
            .any(|reason| reason.starts_with("timeframe_mismatch")));
        assert!(report
            .reasons
            .iter()
            .any(|reason| reason.starts_with("insufficient_oos_trades")));
        assert!(report
            .reasons
            .iter()
            .any(|reason| reason.starts_with("relative_advantage_low")));
        assert!(report
            .reasons
            .iter()
            .any(|reason| reason.starts_with("insufficient_symbols")));
    }

    #[test]
    fn evidence_json_reports_candidate_baseline_and_gate() {
        let metrics = passing_metrics();
        let report = gate(&metrics, Some(0.2));
        let value = evidence_json(&metrics, &report, Some(7), Some(0.2));
        assert_eq!(value["version"], 1);
        assert_eq!(value["timeframe"], "1h");
        assert_eq!(value["target_regime"], "range");
        assert_eq!(value["folds"], 6);
        assert_eq!(value["symbols_used"].as_array().unwrap().len(), 6);
        assert_eq!(value["candidate"]["oos_trades"], 30);
        assert_eq!(value["candidate"]["positive_folds"], 4);
        assert_eq!(value["candidate"]["bootstrap_p_positive"], 0.9);
        assert_eq!(value["baseline_set_id"], 7);
        assert_eq!(value["baseline_oos_avg"], 0.2);
        assert_eq!(value["relative_advantage_pp"], 0.2);
        assert_eq!(value["gate"]["pass"], true);
        assert!(value["gate"]["reasons"].as_array().unwrap().is_empty());
    }

    #[test]
    fn evidence_json_without_baseline_has_null_relative_advantage() {
        let mut metrics = passing_metrics();
        metrics.oos_trades = 1;
        let report = gate(&metrics, None);
        assert!(!report.pass);
        let value = evidence_json(&metrics, &report, None, None);
        assert!(value["baseline_set_id"].is_null());
        assert!(value["baseline_oos_avg"].is_null());
        assert!(value["relative_advantage_pp"].is_null());
        assert_eq!(value["gate"]["pass"], false);
        assert!(!value["gate"]["reasons"].as_array().unwrap().is_empty());
    }
}
