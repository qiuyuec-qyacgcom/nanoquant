#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct TradePolicy {
    pub base_threshold: f64,
    pub volume_ratio_min: f64,
    pub sl_atr_mult: f64,
    pub tp_atr_mult: f64,
    pub min_risk_reward: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) struct MarketFeatures {
    pub funding_rate: f64,
    pub open_interest_change_pct: f64,
    pub relative_strength: f64,
    pub relative_strength_available: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TradeSignal {
    pub side: &'static str,
    pub score: f64,
    pub stop_price: f64,
    pub take_profit: f64,
    pub reason: String,
}

pub(crate) fn evaluate(
    candles: &[crate::providers::CandleSnapshot],
    mark: f64,
    atr: f64,
    policy: TradePolicy,
    features: MarketFeatures,
) -> Option<TradeSignal> {
    if candles.len() < 30 || !mark.is_finite() || mark <= 0.0 || !atr.is_finite() || atr <= 0.0 {
        return None;
    }
    let closes: Vec<f64> = candles.iter().map(|candle| candle.close).collect();
    let last = *closes.last()?;
    if !closes.iter().all(|value| value.is_finite() && *value > 0.0) {
        return None;
    }
    let average_volume = candles[candles.len() - 20..]
        .iter()
        .map(|candle| candle.volume)
        .sum::<f64>()
        / 20.0;
    let volume_ratio = if average_volume > 0.0 {
        candles.last()?.volume / average_volume
    } else {
        0.0
    };
    if volume_ratio < policy.volume_ratio_min {
        return None;
    }
    let atr_pct = atr / last * 100.0;
    if !(0.05..=4.0).contains(&atr_pct) {
        return None;
    }
    let adx_value = adx(candles, 14)?;
    let fast = ema(&closes, 9)?;
    let slow = ema(&closes, 21)?;
    let gap_pct = (fast - slow) / slow * 100.0;
    let regime = if adx_value >= 25.0 && gap_pct.abs() >= 0.15 {
        "trend"
    } else if atr_pct >= 2.0 || volume_ratio >= 2.5 {
        "chaos"
    } else {
        "range"
    };
    let prior = &candles[candles.len() - 21..candles.len() - 1];
    let prior_high = prior
        .iter()
        .map(|candle| candle.high)
        .fold(f64::NEG_INFINITY, f64::max);
    let prior_low = prior
        .iter()
        .map(|candle| candle.low)
        .fold(f64::INFINITY, f64::min);
    let breakout_dir = if last > prior_high {
        1.0
    } else if last < prior_low {
        -1.0
    } else {
        0.0
    };
    let breakout = breakout_dir * (0.4 + 0.6 * ((volume_ratio - 1.0) / 1.5).clamp(0.0, 1.0));
    let rs = if features.relative_strength_available {
        features.relative_strength.clamp(-1.0, 1.0)
    } else {
        0.0
    };
    let carry = if features.funding_rate.is_finite() {
        (-features.funding_rate * 2000.0).clamp(-1.0, 1.0)
    } else {
        0.0
    };
    let price_direction = if last > closes[closes.len() - 2] {
        1.0
    } else if last < closes[closes.len() - 2] {
        -1.0
    } else {
        0.0
    };
    let flow = if features.open_interest_change_pct.is_finite() {
        price_direction * (features.open_interest_change_pct / 5.0).clamp(-1.0, 1.0)
    } else {
        0.0
    };
    let (w_rs, w_break, w_carry, w_flow) = match regime {
        "trend" => (0.30, 0.35, 0.10, 0.25),
        "chaos" => (0.20, 0.20, 0.25, 0.35),
        _ => (0.15, 0.15, 0.35, 0.35),
    };
    let raw = w_rs * rs + w_break * breakout + w_carry * carry + w_flow * flow;
    let score = (raw * 1.2).tanh();
    if score.abs() < policy.base_threshold {
        return None;
    }
    let direction = score.signum();
    let agreement = [rs, breakout, carry, flow]
        .iter()
        .filter(|factor| factor.abs() > 0.15 && factor.signum() == direction)
        .count();
    if agreement < 2 {
        return None;
    }
    let mut coverage: f64 = 0.60;
    if features.relative_strength_available {
        coverage += 0.15;
    }
    if features.open_interest_change_pct.is_finite() {
        coverage += 0.075;
    }
    if features.funding_rate.is_finite() {
        coverage += 0.075;
    }
    let coverage = coverage.min(1.0);
    let confidence = (0.55 + 0.10 * agreement as f64) * coverage;
    if confidence < 0.55 {
        return None;
    }
    let sl_distance = (atr * policy.sl_atr_mult.max(0.2)).max(mark * 0.0018);
    let tp_distance =
        (atr * policy.tp_atr_mult.max(0.2)).max(sl_distance * policy.min_risk_reward.max(0.5));
    let stop_price = mark - direction * sl_distance;
    let take_profit = mark + direction * tp_distance;
    if stop_price <= 0.0 || take_profit <= 0.0 {
        return None;
    }
    Some(TradeSignal {
        side: if score > 0.0 { "long" } else { "short" },
        score,
        stop_price,
        take_profit,
        reason: format!(
            "fusion:score={score:.4}:vol={volume_ratio:.2}:regime={regime},rs={rs:.2},break={breakout:.2},carry={carry:.2},flow={flow:.2},conf={confidence:.2},coverage={coverage:.2}"
        ),
    })
}

fn ema(values: &[f64], period: usize) -> Option<f64> {
    if values.len() < period {
        return None;
    }
    let alpha = 2.0 / (period as f64 + 1.0);
    Some(values.iter().skip(1).fold(values[0], |current, value| {
        alpha * value + (1.0 - alpha) * current
    }))
}

fn adx(candles: &[crate::providers::CandleSnapshot], period: usize) -> Option<f64> {
    if candles.len() <= period {
        return None;
    }
    let mut dx = Vec::new();
    for pair in candles.windows(2).rev().take(period) {
        let up = pair[1].high - pair[0].high;
        let down = pair[0].low - pair[1].low;
        let plus = if up > down && up > 0.0 { up } else { 0.0 };
        let minus = if down > up && down > 0.0 { down } else { 0.0 };
        dx.push((plus - minus).abs() / (plus + minus).max(1e-12) * 100.0);
    }
    (!dx.is_empty()).then(|| dx.iter().sum::<f64>() / dx.len() as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candles(values: &[f64]) -> Vec<crate::providers::CandleSnapshot> {
        values
            .iter()
            .enumerate()
            .map(|(i, v)| crate::providers::CandleSnapshot {
                open: *v,
                high: *v + 0.5,
                low: *v - 0.5,
                close: *v,
                volume: 100.0 + i as f64,
                close_time: i as i64 * 60_000,
            })
            .collect()
    }

    fn cfg() -> TradePolicy {
        TradePolicy {
            base_threshold: 0.1,
            volume_ratio_min: 0.5,
            sl_atr_mult: 1.2,
            tp_atr_mult: 2.0,
            min_risk_reward: 2.0,
        }
    }

    #[test]
    fn flat_series_has_no_directional_entry() {
        assert!(evaluate(
            &candles(&vec![100.0; 40]),
            100.0,
            1.0,
            cfg(),
            MarketFeatures::default()
        )
        .is_none());
    }

    #[test]
    fn volume_gate_rejects_entry() {
        let mut data = candles(&(1..=50).map(|i| 100.0 + i as f64).collect::<Vec<_>>());
        for candle in data.iter_mut().rev().take(20) {
            candle.volume = 1.0;
        }
        assert!(evaluate(
            &data,
            data.last().unwrap().close,
            1.0,
            TradePolicy {
                volume_ratio_min: 1.5,
                ..cfg()
            },
            MarketFeatures::default()
        )
        .is_none());
    }

    #[test]
    fn falling_price_with_rising_oi_confirms_short() {
        let values: Vec<f64> = (0..50).map(|i| 100.0 - i as f64 * 0.6).collect();
        let signal = evaluate(
            &candles(&values),
            *values.last().unwrap(),
            1.0,
            cfg(),
            MarketFeatures {
                funding_rate: 0.001,
                open_interest_change_pct: 10.0,
                ..MarketFeatures::default()
            },
        )
        .unwrap();
        assert_eq!(signal.side, "short", "价跌 + OI 增是空头确认，不是做多信号");
    }

    #[test]
    fn insufficient_or_invalid_inputs_never_produce_a_signal() {
        let short_series = candles(&(0..21).map(|i| 100.0 + i as f64).collect::<Vec<_>>());
        assert!(
            evaluate(&short_series, 120.0, 1.0, cfg(), MarketFeatures::default()).is_none(),
            "少于 30 根 K 线必须直接放弃"
        );
        let data = candles(&(0..40).map(|i| 100.0 + i as f64 * 0.1).collect::<Vec<_>>());
        assert!(
            evaluate(&data, f64::NAN, 1.0, cfg(), MarketFeatures::default()).is_none(),
            "mark 为 NaN 不能给出信号"
        );
        assert!(
            evaluate(&data, 0.0, 1.0, cfg(), MarketFeatures::default()).is_none(),
            "mark 为 0 不能给出信号"
        );
        assert!(
            evaluate(&data, 101.0, 0.0, cfg(), MarketFeatures::default()).is_none(),
            "atr 为 0 不能给出信号"
        );
    }

    #[test]
    fn fusion_trend_breakout_with_relative_strength_goes_long() {
        let values: Vec<f64> = (0..40).map(|i| 100.0 + i as f64).collect();
        let signal = evaluate(
            &candles(&values),
            139.0,
            1.0,
            cfg(),
            MarketFeatures {
                funding_rate: 0.0,
                open_interest_change_pct: 5.0,
                relative_strength: 0.8,
                relative_strength_available: true,
            },
        )
        .expect("趋势 + 突破 + 相对强弱同向应出多单");
        assert_eq!(signal.side, "long");
        assert!(signal.score.is_finite() && signal.score > 0.0);
        assert!(signal.reason.contains("regime=trend"));
    }

    #[test]
    fn fusion_requires_at_least_two_agreeing_factors() {
        let values = vec![100.0; 40];
        let signal = evaluate(
            &candles(&values),
            100.0,
            1.0,
            cfg(),
            MarketFeatures::default(),
        );
        assert!(signal.is_none(), "无方向因子时不应出信号");
    }

    #[test]
    fn fusion_low_coverage_is_gated_to_flat() {
        let values: Vec<f64> = (0..40).map(|i| 100.0 + i as f64).collect();
        let signal = evaluate(
            &candles(&values),
            139.0,
            1.0,
            cfg(),
            MarketFeatures {
                funding_rate: -0.001,
                open_interest_change_pct: f64::NAN,
                relative_strength: 0.0,
                relative_strength_available: false,
            },
        );
        assert!(signal.is_none(), "缺失 RS 与 OI 时覆盖率不足应保持空仓");
    }
}
