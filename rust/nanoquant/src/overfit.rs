//! 回测过拟合体检：前视偏差与递归指标不稳定。

/// 前视偏差检测结论。
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
pub(crate) struct RecursiveReport {
    pub max_divergence_pct: f64,
    pub suspicious: bool,
    pub reason: String,
}

/// 默认阈值：两种 warmup 窗口的 ATR% 相对差异超过该比例即判为不稳定。
pub(crate) const DEFAULT_RECURSIVE_DIVERGENCE_PCT: f64 = 0.05;

/// 判定：同一根 bar 用两种 warmup 长度算出的指标不收敛 → 递归指标失稳。
/// 多标的场景由调用方先取最大漂移，再用 `evaluate_recursive_divergence` 判定。
#[allow(dead_code)] // 单标的入口保留给调用方与测试
pub(crate) fn evaluate_recursive(short_atr_pct: f64, long_atr_pct: f64) -> RecursiveReport {
    let max_divergence_pct = if short_atr_pct.abs() > 1e-9 {
        (long_atr_pct - short_atr_pct).abs() / short_atr_pct.abs()
    } else {
        0.0
    };
    evaluate_recursive_divergence(max_divergence_pct)
}

/// 直接按相对漂移判定，供「多标的取最大漂移」的调用方使用。
pub(crate) fn evaluate_recursive_divergence(max_divergence_pct: f64) -> RecursiveReport {
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

/// 多档前视结论：任一档可疑即整体可疑，并返回最大 delta 供证据记录。
pub(crate) fn summarize_lookahead(reports: &[LookaheadReport]) -> (bool, f64) {
    let suspicious = reports.iter().any(|report| report.suspicious);
    let max_delta = reports
        .iter()
        .map(|report| report.delta_pct)
        .fold(f64::NEG_INFINITY, f64::max);
    (
        suspicious,
        if max_delta.is_finite() {
            max_delta
        } else {
            0.0
        },
    )
}

/// 蒙特卡洛贸易自助法：对 OOS 成交收益有放回重采样。
///
/// 返回（重采样后累计收益为正的概率, 重采样序列最大回撤的 P95，百分比）。
/// 少于 10 笔不构建分布，避免小样本给出伪精确的置信度。
pub(crate) fn monte_carlo_trade_bootstrap(returns: &[f64]) -> (f64, f64) {
    use rand::prelude::IndexedRandom;
    use rand::SeedableRng;

    const ROUNDS: usize = 1000;
    if returns.len() < 10 {
        return (0.0, 0.0);
    }
    let mut rng = rand::rngs::StdRng::seed_from_u64(7);
    let mut positive = 0usize;
    let mut drawdowns = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let mut equity = 1.0_f64;
        let mut peak = 1.0_f64;
        let mut worst = 0.0_f64;
        for _ in 0..returns.len() {
            let value = *returns.choose(&mut rng).unwrap_or(&0.0);
            equity *= 1.0 + value / 100.0;
            peak = peak.max(equity);
            if peak > 0.0 {
                worst = worst.max((peak - equity) / peak * 100.0);
            }
        }
        if equity > 1.0 {
            positive += 1;
        }
        drawdowns.push(worst);
    }
    drawdowns.sort_by(f64::total_cmp);
    let index = ((ROUNDS as f64) * 0.95) as usize;
    (
        round4(positive as f64 / ROUNDS as f64),
        round4(drawdowns[index.min(ROUNDS - 1)]),
    )
}

/// Deflated Sharpe Ratio（Bailey & López de Prado, 2014）。
///
/// 给定 OOS 成交收益与「总共试过多少次参数」`trials`，返回扣掉多重试验后
/// 真实 Sharpe 大于 0 的概率。样本不足或方差为 0 时返回 0。
/// `trials` 至少按 1 处理；试验越多，门槛越高。
pub(crate) fn deflated_sharpe(returns: &[f64], trials: usize) -> f64 {
    const EULER_GAMMA: f64 = 0.577_215_664_901_532_9;
    let n = returns.len();
    if n < 3 {
        return 0.0;
    }
    let mean = returns.iter().sum::<f64>() / n as f64;
    let m2 = returns
        .iter()
        .map(|value| (value - mean).powi(2))
        .sum::<f64>()
        / n as f64;
    if m2 <= 0.0 {
        return 0.0;
    }
    let sd = (m2 * n as f64 / (n - 1) as f64).sqrt();
    if sd <= 0.0 {
        return 0.0;
    }
    let sr = mean / sd; // 非年化
    let m3 = returns
        .iter()
        .map(|value| (value - mean).powi(3))
        .sum::<f64>()
        / n as f64;
    let m4 = returns
        .iter()
        .map(|value| (value - mean).powi(4))
        .sum::<f64>()
        / n as f64;
    let skew = m3 / m2.powf(1.5);
    let kurtosis = m4 / m2.powi(2);
    let trials = trials.max(1) as f64;
    // 零假设下 Sharpe 估计量的方差近似 1/T；对 N 次独立试验取期望最大值作为门槛。
    let sr0 = if trials <= 1.0 {
        0.0
    } else {
        (1.0 / n as f64).sqrt()
            * ((1.0 - EULER_GAMMA) * normal_quantile(1.0 - 1.0 / trials)
                + EULER_GAMMA * normal_quantile(1.0 - 1.0 / (trials * std::f64::consts::E)))
    };
    let denominator = 1.0 - skew * sr + (kurtosis - 1.0) / 4.0 * sr * sr;
    if denominator <= 1e-12 {
        return 0.0;
    }
    let z = (sr - sr0) * ((n - 1) as f64).sqrt() / denominator.sqrt();
    round4(normal_cdf(z).clamp(0.0, 1.0))
}

fn round4(value: f64) -> f64 {
    (value * 10_000.0).round() / 10_000.0
}

fn normal_cdf(value: f64) -> f64 {
    0.5 * (1.0 + erf(value / std::f64::consts::SQRT_2))
}

/// Abramowitz & Stegun 7.1.26，最大误差约 1.5e-7，足够做显著性判定。
fn erf(value: f64) -> f64 {
    let sign = if value < 0.0 { -1.0 } else { 1.0 };
    let x = value.abs();
    let t = 1.0 / (1.0 + 0.327_591_1 * x);
    let y = 1.0
        - (((((1.061_405_429 * t - 1.453_152_027) * t) + 1.421_413_741) * t - 0.284_496_736) * t
            + 0.254_829_592)
            * t
            * (-x * x).exp();
    sign * y
}

/// 标准正态分位点：二分求解，避免引入额外依赖。
fn normal_quantile(probability: f64) -> f64 {
    let p = probability.clamp(1e-12, 1.0 - 1e-12);
    let mut low = -10.0_f64;
    let mut high = 10.0_f64;
    for _ in 0..200 {
        let middle = (low + high) / 2.0;
        if normal_cdf(middle) < p {
            low = middle;
        } else {
            high = middle;
        }
    }
    (low + high) / 2.0
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

    #[test]
    fn multi_shift_lookahead_flags_any_suspicious_shift_and_reports_max_delta() {
        let reports = vec![evaluate_lookahead(0.4, 0.41), evaluate_lookahead(0.4, 0.9)];
        let (suspicious, max_delta) = summarize_lookahead(&reports);
        assert!(suspicious, "任一档可疑即整体可疑");
        assert!((max_delta - reports[1].delta_pct).abs() < 1e-12);
        assert_eq!(summarize_lookahead(&[]), (false, 0.0));
    }

    #[test]
    fn deflated_sharpe_drops_as_more_trials_are_attempted() {
        let returns: Vec<f64> = (0..60)
            .map(|index| if index % 3 == 0 { -0.4 } else { 0.6 })
            .collect();
        let single = deflated_sharpe(&returns, 1);
        let many = deflated_sharpe(&returns, 500);
        assert!(single > 0.5, "单次试验应显著: {single}");
        assert!(
            many < single,
            "试了 500 次后必须折减 Sharpe: {many} vs {single}"
        );
    }

    #[test]
    fn deflated_sharpe_needs_samples_and_variance() {
        assert_eq!(deflated_sharpe(&[], 1), 0.0);
        assert_eq!(deflated_sharpe(&[1.0, 1.0, 1.0], 1), 0.0);
    }

    #[test]
    fn monte_carlo_bootstrap_is_certain_for_steady_wins() {
        let (probability, drawdown) = monte_carlo_trade_bootstrap(&[1.0; 20]);
        assert!((probability - 1.0).abs() < 1e-9);
        assert_eq!(drawdown, 0.0);
    }

    #[test]
    fn monte_carlo_bootstrap_reports_mixed_outcomes_and_drawdown() {
        let returns: Vec<f64> = (0..40)
            .map(|index| if index % 2 == 0 { 1.0 } else { -1.5 })
            .collect();
        let (probability, drawdown) = monte_carlo_trade_bootstrap(&returns);
        assert!((0.0..=1.0).contains(&probability));
        assert!(drawdown > 0.0, "混合盈亏必须给出非零回撤分位");
        assert_eq!(
            monte_carlo_trade_bootstrap(&[0.1; 5]).0,
            0.0,
            "少于 10 笔不构建分布"
        );
    }
}
