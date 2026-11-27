//! 风险调整后收益指标：Sharpe / Sortino / Calmar。
//!
//! 输入是按时间升序的权益采样点，输出是年化后的风险调整指标。
//! 样本不足或无波动时返回 0.0，不返回 NaN / 无穷。

/// 权益序列 → 周期收益率。点数少于 2 时返回空序列。
pub(crate) fn returns_from_equity(points: &[(i64, f64)]) -> Vec<f64> {
    points
        .windows(2)
        .filter_map(|pair| {
            let (prev_ts, prev) = pair[0];
            let (ts, cur) = pair[1];
            if prev <= 0.0 || ts <= prev_ts {
                return None;
            }
            Some(cur / prev - 1.0)
        })
        .collect()
}

/// 年化周期数：由采样间隔的中位数推算。
pub(crate) fn periods_per_year(points: &[(i64, f64)]) -> f64 {
    let mut gaps: Vec<i64> = points.windows(2).map(|p| p[1].0 - p[0].0).collect();
    if gaps.is_empty() {
        return 365.0;
    }
    gaps.sort_unstable();
    let median = gaps[gaps.len() / 2].max(1);
    365.0 * 24.0 * 3600.0 / median as f64
}

fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.iter().sum::<f64>() / values.len() as f64
}

/// 样本标准差（ddof=1）。样本少于 2 时返回 0.0。
fn std_dev(values: &[f64]) -> f64 {
    if values.len() < 2 {
        return 0.0;
    }
    let m = mean(values);
    let var = values.iter().map(|v| (v - m).powi(2)).sum::<f64>() / (values.len() - 1) as f64;
    var.sqrt()
}

/// 权益序列的最大回撤百分比（正数）：以历史峰值为基准。
pub(crate) fn max_drawdown_pct(points: &[(i64, f64)]) -> f64 {
    let mut peak = 0.0_f64;
    let mut worst = 0.0_f64;
    for (_, equity) in points {
        if *equity > peak {
            peak = *equity;
        }
        if peak > 0.0 {
            let drawdown = (peak - equity) / peak * 100.0;
            if drawdown > worst {
                worst = drawdown;
            }
        }
    }
    worst
}

/// 年化夏普比率。无波动或样本不足时返回 0.0。
pub(crate) fn sharpe(returns: &[f64], periods_per_year: f64) -> f64 {
    let sd = std_dev(returns);
    if returns.len() < 2 || sd <= 0.0 {
        return 0.0;
    }
    mean(returns) / sd * periods_per_year.sqrt()
}

/// 年化索提诺比率：分母只算下行波动。
pub(crate) fn sortino(returns: &[f64], periods_per_year: f64) -> f64 {
    if returns.len() < 2 {
        return 0.0;
    }
    let downside = returns
        .iter()
        .map(|v| if *v < 0.0 { *v } else { 0.0 })
        .map(|v| v * v)
        .sum::<f64>()
        / returns.len() as f64;
    let dd = downside.sqrt();
    if dd <= 0.0 {
        return 0.0;
    }
    mean(returns) / dd * periods_per_year.sqrt()
}

/// Calmar：年化收益率 / 最大回撤百分比。回撤为 0 时返回 0.0。
///
/// `periods_per_year` 与 Sharpe/Sortino 同源：权益采样点按分钟采样时为 525600，
/// 回测按单笔成交采样时用「年成交笔数」，避免把不同频率的收益混成同一口径。
pub(crate) fn calmar(returns: &[f64], periods_per_year: f64, max_drawdown_pct: f64) -> f64 {
    if returns.is_empty() || max_drawdown_pct <= 0.0 {
        return 0.0;
    }
    (mean(returns) * periods_per_year) / max_drawdown_pct
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_are_computed_from_consecutive_equity_points() {
        let points = vec![(0i64, 100.0f64), (60, 101.0), (120, 99.98)];
        let returns = returns_from_equity(&points);
        assert_eq!(returns.len(), 2);
        assert!((returns[0] - 0.01).abs() < 1e-12);
        assert!((returns[1] + 0.010_099_009_900_99).abs() < 1e-9);
    }

    #[test]
    fn periods_per_year_follow_the_sampling_interval() {
        let points = vec![(0i64, 100.0f64), (60, 101.0), (120, 102.0)];
        assert!((periods_per_year(&points) - 525_600.0).abs() < 1.0);
    }

    #[test]
    fn sharpe_is_positive_for_a_steady_uptrend() {
        let returns = vec![0.01, 0.012, 0.008, 0.011, 0.009, 0.010];
        assert!(sharpe(&returns, 365.0) > 0.0);
    }

    #[test]
    fn sharpe_is_zero_when_there_is_no_variance() {
        assert_eq!(sharpe(&[0.01, 0.01, 0.01], 365.0), 0.0);
    }

    #[test]
    fn sortino_ignores_upside_volatility() {
        // 上行大、下行小：Sortino 必须显著高于同均值的 Sharpe。
        let returns = vec![0.10, 0.08, 0.12, 0.09, 0.11, -0.005];
        assert!(sortino(&returns, 365.0) > sharpe(&returns, 365.0));
    }

    #[test]
    fn max_drawdown_pct_uses_the_peak_before_the_drop() {
        let points = vec![(0i64, 100.0f64), (60, 120.0), (120, 90.0)];
        assert!((max_drawdown_pct(&points) - 25.0).abs() < 1e-9);
        assert_eq!(max_drawdown_pct(&[(0i64, 100.0f64)]), 0.0);
    }

    #[test]
    fn calmar_divides_annualized_return_by_drawdown() {
        let returns = vec![0.01, 0.01, 0.01, 0.01];
        assert!(calmar(&returns, 525_600.0, 20.0) > 0.0);
        assert_eq!(
            calmar(&returns, 525_600.0, 0.0),
            0.0,
            "回撤为 0 时不返回无穷"
        );
    }
}
