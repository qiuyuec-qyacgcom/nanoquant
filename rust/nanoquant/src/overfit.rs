//! 回测过拟合体检：前视偏差与递归指标不稳定。

/// 前视偏差检测结论。
#[allow(dead_code)] // J3 接入证据门后由 evidence_task 读取全部字段
pub(crate) struct LookaheadReport {
    pub baseline_expectancy: f64,
    pub shifted_expectancy: f64,
    pub delta_pct: f64,
    pub suspicious: bool,
    pub reason: String,
}

/// 默认阈值：延后一根信号后，期望相对提升超过该比例即判为可疑。
pub(crate) const DEFAULT_LOOKAHEAD_DELTA_PCT: f64 = 0.20;

/// 判定：把信号延后若干根后表现明显更好 → 原回测用到了当时拿不到的信息。
pub(crate) fn evaluate_lookahead(baseline: f64, shifted: f64) -> LookaheadReport {
    let delta_pct = if baseline.abs() > 1e-9 {
        (shifted - baseline) / baseline.abs()
    } else {
        0.0
    };
    let suspicious = baseline.abs() > 1e-9 && delta_pct > DEFAULT_LOOKAHEAD_DELTA_PCT;
    LookaheadReport {
        baseline_expectancy: baseline,
        shifted_expectancy: shifted,
        delta_pct,
        suspicious,
        reason: if suspicious {
            format!(
                "lookahead_suspect: 延后信号后期望提升 {:.1}%",
                delta_pct * 100.0
            )
        } else {
            String::new()
        },
    }
}

/// 递归指标稳定性结论。
#[allow(dead_code)] // J3 接入证据门后由 evidence_task 读取全部字段
pub(crate) struct RecursiveReport {
    pub max_divergence_pct: f64,
    pub suspicious: bool,
    pub reason: String,
}

/// 默认阈值：两种 warmup 窗口的 ATR% 相对差异超过该比例即判为不稳定。
#[allow(dead_code)] // J3 接入证据门后由 evidence_task 使用
pub(crate) const DEFAULT_RECURSIVE_DIVERGENCE_PCT: f64 = 0.05;

/// 判定：同一根 bar 用两种 warmup 长度算出的指标不收敛 → 递归指标失稳。
#[allow(dead_code)] // J3 接入证据门后由 evidence_task 使用
pub(crate) fn evaluate_recursive(short_atr_pct: f64, long_atr_pct: f64) -> RecursiveReport {
    let max_divergence_pct = if short_atr_pct.abs() > 1e-9 {
        (long_atr_pct - short_atr_pct).abs() / short_atr_pct.abs()
    } else {
        0.0
    };
    let suspicious = max_divergence_pct > DEFAULT_RECURSIVE_DIVERGENCE_PCT;
    RecursiveReport {
        max_divergence_pct,
        suspicious,
        reason: if suspicious {
            format!(
                "recursive_unstable: warmup 差异导致 ATR 漂移 {:.1}%",
                max_divergence_pct * 100.0
            )
        } else {
            String::new()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookahead_is_flagged_when_shifting_the_signal_helps() {
        // 基线 0.4 期望，延后后 0.9 期望 → 明显变好，判定为可疑。
        let report = evaluate_lookahead(0.4, 0.9);
        assert!(report.suspicious, "延后信号后显著变好必须判为可疑");
        assert!(report.delta_pct > 0.0);
    }

    #[test]
    fn lookahead_is_not_flagged_when_shifting_does_not_help() {
        let report = evaluate_lookahead(0.4, 0.38);
        assert!(!report.suspicious, "延后没有变好不算可疑");
    }

    #[test]
    fn lookahead_needs_both_legs_to_have_trades() {
        assert!(
            !evaluate_lookahead(0.0, 0.0).suspicious,
            "没有样本时不得判为可疑"
        );
    }

    #[test]
    fn recursive_divergence_is_flagged_when_warmup_changes_the_indicator() {
        // 同一根 bar，warmup 30 与 120 算出的 ATR% 差 40% → 不收敛，判为不稳定。
        let report = evaluate_recursive(0.40, 0.56);
        assert!(report.suspicious, "指标随 warmup 大幅漂移必须判为不稳定");
    }

    #[test]
    fn recursive_divergence_is_small_for_a_converged_series() {
        let report = evaluate_recursive(0.40, 0.402);
        assert!(!report.suspicious);
    }
}
