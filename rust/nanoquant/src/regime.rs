use crate::providers::{candle_metrics, CandleSnapshot};

#[cfg_attr(not(test), allow(dead_code))]
const MIN_BARS_FOR_REGIME: usize = 30;
pub(crate) const MARKET_QUORUM: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Regime {
    Trend,
    Range,
    Chaos,
}

impl Regime {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Regime::Trend => "trend",
            Regime::Range => "range",
            Regime::Chaos => "chaos",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub(crate) fn from_str(value: &str) -> Option<Self> {
        match value {
            "trend" => Some(Regime::Trend),
            "range" => Some(Regime::Range),
            "chaos" => Some(Regime::Chaos),
            _ => None,
        }
    }
}

pub(crate) fn classify(adx: f64, gap_pct: f64, atr_pct: f64, volume_ratio: f64) -> Regime {
    if adx >= 25.0 && gap_pct.abs() >= 0.15 {
        Regime::Trend
    } else if atr_pct >= 2.0 || volume_ratio >= 2.5 {
        Regime::Chaos
    } else {
        Regime::Range
    }
}

/// 启用标的池的等权多数 regime；少于 quorum 无结果，平票取 range。
pub(crate) fn market_regime(regimes: &[Regime]) -> Option<Regime> {
    if regimes.len() < MARKET_QUORUM {
        return None;
    }
    let trend = regimes
        .iter()
        .filter(|regime| **regime == Regime::Trend)
        .count();
    let range = regimes
        .iter()
        .filter(|regime| **regime == Regime::Range)
        .count();
    let chaos = regimes
        .iter()
        .filter(|regime| **regime == Regime::Chaos)
        .count();
    let highest = trend.max(range).max(chaos);
    let winners = [trend, range, chaos]
        .iter()
        .filter(|count| **count == highest)
        .count();
    if winners != 1 {
        return Some(Regime::Range);
    }
    Some(if trend == highest {
        Regime::Trend
    } else if chaos == highest {
        Regime::Chaos
    } else {
        Regime::Range
    })
}

/// 逐根 bar 计算前缀的 regime；不足 30 根或指标不可用时为 None。
///
/// A 期只读判定与记录，B 期回测复用；当前仅由测试直接调用。
#[allow(dead_code)]
pub(crate) fn regime_series(candles: &[CandleSnapshot], atr_period: usize) -> Vec<Option<Regime>> {
    (0..candles.len())
        .map(|index| {
            let end = index + 1;
            if end < MIN_BARS_FOR_REGIME {
                return None;
            }
            let prefix = &candles[..end];
            // regime 只用该前缀的 last close 作为 mark；资金费率/相对强弱等 MarketFeatures 不参与判定。
            let mark = candles[index].close;
            let metrics = candle_metrics(prefix, atr_period)?;
            crate::trade_policy::regime_for(
                prefix,
                mark,
                metrics.atr,
                crate::trade_policy::MarketFeatures::default(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthesized_candles(count: usize) -> Vec<CandleSnapshot> {
        (0..count)
            .map(|index| {
                let price = 100.0 + index as f64 * 0.2;
                CandleSnapshot {
                    open: price,
                    high: price + 0.5,
                    low: price - 0.5,
                    close: price,
                    volume: 100.0 + index as f64,
                    close_time: index as i64 * 3_600_000,
                }
            })
            .collect()
    }

    #[test]
    fn classify_boundaries_are_stable() {
        assert_eq!(classify(25.0, 0.15, 1.0, 1.0), Regime::Trend);
        assert_eq!(classify(24.9, 0.15, 1.0, 1.0), Regime::Range);
        assert_eq!(classify(25.0, 0.149, 1.0, 1.0), Regime::Range);
        assert_eq!(classify(25.0, -0.15, 1.0, 1.0), Regime::Trend);
        assert_eq!(classify(24.0, 0.0, 2.0, 1.0), Regime::Chaos);
        assert_eq!(classify(24.0, 0.0, 1.99, 1.0), Regime::Range);
        assert_eq!(classify(24.0, 0.0, 1.0, 2.5), Regime::Chaos);
        assert_eq!(classify(24.0, 0.0, 1.0, 2.49), Regime::Range);
        assert_eq!(classify(24.0, 0.0, 1.0, 1.0), Regime::Range);
        assert_eq!(
            classify(f64::NAN, 0.0, 1.0, 1.0),
            Regime::Range,
            "NaN 的 ADX 不满足趋势条件，也不触发 chaos，应落到 range"
        );
    }

    #[test]
    fn majority_requires_six_and_ties_to_range() {
        assert_eq!(market_regime(&[Regime::Range; 5]), None);
        assert_eq!(
            market_regime(&[
                Regime::Trend,
                Regime::Trend,
                Regime::Range,
                Regime::Range,
                Regime::Chaos,
                Regime::Chaos,
            ]),
            Some(Regime::Range),
            "三向平票应取 range"
        );
        assert_eq!(
            market_regime(&[
                Regime::Trend,
                Regime::Trend,
                Regime::Trend,
                Regime::Trend,
                Regime::Range,
                Regime::Range,
            ]),
            Some(Regime::Trend)
        );
        assert_eq!(
            market_regime(&[
                Regime::Trend,
                Regime::Trend,
                Regime::Trend,
                Regime::Range,
                Regime::Range,
                Regime::Range,
            ]),
            Some(Regime::Range),
            "trend 与 range 平票应取 range"
        );
        assert_eq!(
            market_regime(&[
                Regime::Chaos,
                Regime::Chaos,
                Regime::Chaos,
                Regime::Chaos,
                Regime::Range,
                Regime::Trend,
            ]),
            Some(Regime::Chaos)
        );
    }

    #[test]
    fn regime_series_aligns_to_candles() {
        let candles = synthesized_candles(80);
        let series = regime_series(&candles, 14);
        assert_eq!(series.len(), candles.len());
        assert!(
            series[..MIN_BARS_FOR_REGIME - 1]
                .iter()
                .all(Option::is_none),
            "不足 30 根 K 线的前缀不应给出 regime"
        );
        assert!(
            series[MIN_BARS_FOR_REGIME - 2].is_none(),
            "第 29 根 K 线仍不足 30 根，应为 None"
        );
        assert!(
            series[MIN_BARS_FOR_REGIME - 1].is_some(),
            "第 30 根 K 线起应能给出 regime"
        );
        assert!(
            series[MIN_BARS_FOR_REGIME - 1..]
                .iter()
                .any(Option::is_some),
            "稳定合成序列在 30 根后至少应有一个 regime"
        );
    }

    #[test]
    fn regime_strings_round_trip() {
        for regime in [Regime::Trend, Regime::Range, Regime::Chaos] {
            assert_eq!(Regime::from_str(regime.as_str()), Some(regime));
        }
        assert_eq!(Regime::from_str("unknown"), None);
    }
}
