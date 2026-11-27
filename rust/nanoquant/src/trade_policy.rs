use std::sync::atomic::{AtomicI64, Ordering};

/// EMA 快慢周期：信号内核、regime 判定与 AI 特征快照共用同一份配置。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct EmaPeriods {
    pub fast: usize,
    pub slow: usize,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct TradePolicy {
    pub base_threshold: f64,
    pub volume_ratio_min: f64,
    pub atr_pct_min: f64,
    pub atr_pct_max: f64,
    pub ema: EmaPeriods,
    /// ADX 计算周期，来自 strategy.adx_period。
    pub adx_period: usize,
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

    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Preview {
    pub side: &'static str,
    pub score: f64,
}

struct Core {
    volume_ratio: f64,
    atr_pct: f64,
    /// ADX 值：regime 判定的趋势强度输入，周期随配置变化。
    adx: f64,
    /// EMA 快慢差（%）：regime 判定的趋势强度输入，随配置周期变化。
    gap_pct: f64,
    regime: &'static str,
    rs: f64,
    breakout: f64,
    carry: f64,
    flow: f64,
    score: f64,
    agreement: usize,
    coverage: f64,
    confidence: f64,
}

/// 信号内核需要的最小 K 线数：30 根基础窗口、ADX 需要 `period + 1` 个窗口、
/// EMA 需要 `period` 根收盘价。任何一项不满足，`core` 都会整体返回 `None`，
/// 这里把这条隐式耦合显式化，供校验与告警共用。
fn required_candles(policy: &TradePolicy) -> usize {
    30usize
        .max(policy.adx_period.saturating_add(1))
        .max(policy.ema.fast)
        .max(policy.ema.slow)
}

/// 数据窗口不足是配置/行情问题，不是常规空信号；限流告警避免每轮刷屏。
fn warn_insufficient_candles(available: usize, required: usize) {
    static LAST_WARN_TS: AtomicI64 = AtomicI64::new(0);
    let now = crate::now();
    let last = LAST_WARN_TS.load(Ordering::Relaxed);
    if now.saturating_sub(last) < 60 {
        return;
    }
    if LAST_WARN_TS
        .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
    {
        tracing::warn!(
            available,
            required,
            "K 线不足以计算配置的指标周期，本轮不产生信号；请检查 ema_slow/adx_period 与行情数据窗口"
        );
    }
}

fn core(
    candles: &[crate::providers::CandleSnapshot],
    mark: f64,
    atr: f64,
    policy: TradePolicy,
    features: MarketFeatures,
) -> Option<Core> {
    let required = required_candles(&policy);
    if candles.len() < required {
        warn_insufficient_candles(candles.len(), required);
        return None;
    }
    if !mark.is_finite() || mark <= 0.0 || !atr.is_finite() || atr <= 0.0 {
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
    let atr_pct = atr / last * 100.0;
    let adx_value = adx(candles, policy.adx_period)?;
    let fast = ema(&closes, policy.ema.fast)?;
    let slow = ema(&closes, policy.ema.slow)?;
    let gap_pct = (fast - slow) / slow * 100.0;
    let regime = crate::regime::classify(adx_value, gap_pct, atr_pct, volume_ratio);
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
        crate::regime::Regime::Trend => (0.30, 0.35, 0.10, 0.25),
        crate::regime::Regime::Chaos => (0.20, 0.20, 0.25, 0.35),
        _ => (0.15, 0.15, 0.35, 0.35),
    };
    let raw = w_rs * rs + w_break * breakout + w_carry * carry + w_flow * flow;
    let score = (raw * 1.2).tanh();
    if !score.is_finite() {
        return None;
    }
    let direction = score.signum();
    let agreement = [rs, breakout, carry, flow]
        .iter()
        .filter(|factor| factor.abs() > 0.15 && factor.signum() == direction)
        .count();
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
    Some(Core {
        volume_ratio,
        atr_pct,
        adx: adx_value,
        gap_pct,
        regime: regime.as_str(),
        rs,
        breakout,
        carry,
        flow,
        score,
        agreement,
        coverage,
        confidence,
    })
}

pub(crate) fn regime_for(
    candles: &[crate::providers::CandleSnapshot],
    mark: f64,
    atr: f64,
    policy: TradePolicy,
    features: MarketFeatures,
) -> Option<crate::regime::Regime> {
    core(candles, mark, atr, policy, features)
        .and_then(|core| crate::regime::Regime::from_str(core.regime))
}

pub(crate) fn preview(
    candles: &[crate::providers::CandleSnapshot],
    mark: f64,
    atr: f64,
    policy: TradePolicy,
    features: MarketFeatures,
) -> Option<Preview> {
    let core = core(candles, mark, atr, policy, features)?;
    let side = if core.score > 0.0 {
        "long"
    } else if core.score < 0.0 {
        "short"
    } else {
        "flat"
    };
    Some(Preview {
        side,
        score: core.score,
    })
}

pub(crate) fn evaluate(
    candles: &[crate::providers::CandleSnapshot],
    mark: f64,
    atr: f64,
    policy: TradePolicy,
    features: MarketFeatures,
) -> Option<TradeSignal> {
    let core = core(candles, mark, atr, policy, features)?;
    if core.volume_ratio < policy.volume_ratio_min {
        return None;
    }
    if !(policy.atr_pct_min..=policy.atr_pct_max).contains(&core.atr_pct) {
        return None;
    }
    if core.score.abs() < policy.base_threshold {
        return None;
    }
    if core.agreement < 2 {
        return None;
    }
    if core.confidence < 0.55 {
        return None;
    }
    Some(TradeSignal {
        side: if core.score > 0.0 { "long" } else { "short" },
        score: core.score,
        reason: format!(
            "fusion:score={:.4}:vol={:.2}:adx={:.1}:regime={},gap={:.4},rs={:.2},break={:.2},carry={:.2},flow={:.2},conf={:.2},coverage={:.2}",
            core.score,
            core.volume_ratio,
            core.adx,
            core.regime,
            core.gap_pct,
            core.rs,
            core.breakout,
            core.carry,
            core.flow,
            core.confidence,
            core.coverage
        ),
    })
}

/// 全仓唯一 EMA 实现：以第一根收盘价播种，避免信号内核与 AI 特征快照
/// 各用一套播种口径导致长周期下 EMA 差出现可见分叉。
pub(crate) fn ema(values: &[f64], period: usize) -> Option<f64> {
    if period == 0 || values.len() < period {
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
            atr_pct_min: 0.05,
            atr_pct_max: 4.0,
            ema: EmaPeriods { fast: 9, slow: 21 },
            adx_period: 14,
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

    #[test]
    fn preview_keeps_direction_and_score_when_volume_gate_rejects() {
        let mut data = candles(&(1..=50).map(|i| 100.0 + i as f64).collect::<Vec<_>>());
        for candle in data.iter_mut().rev().take(20) {
            candle.volume = 1.0;
        }
        let policy = TradePolicy {
            volume_ratio_min: 1.5,
            ..cfg()
        };
        let mark = data.last().unwrap().close;
        assert!(
            evaluate(&data, mark, 1.0, policy, MarketFeatures::default()).is_none(),
            "正式判定必须尊重量能门槛"
        );
        let preview = preview(&data, mark, 1.0, cfg(), MarketFeatures::default())
            .expect("量能不足时仍应给出预览评分");
        assert_eq!(preview.side, "long");
        assert!(preview.score > 0.0);
    }

    #[test]
    fn evaluate_uses_policy_atr_bounds() {
        let values: Vec<f64> = (0..40).map(|i| 100.0 + i as f64).collect();
        let data = candles(&values);
        let mark = *values.last().unwrap();
        let features = MarketFeatures {
            funding_rate: 0.0,
            open_interest_change_pct: 5.0,
            relative_strength: 0.8,
            relative_strength_available: true,
        };
        let atr_for = |pct: f64| mark * pct / 100.0;
        let policy = TradePolicy {
            atr_pct_min: 1.4,
            atr_pct_max: 1.6,
            ..cfg()
        };
        assert!(
            evaluate(&data, mark, atr_for(1.5), policy, features).is_some(),
            "atr_pct≈1.5 落在 policy 上下界内时应给出信号"
        );
        assert!(
            evaluate(&data, mark, atr_for(0.8), policy, features).is_none(),
            "atr_pct≈0.8 低于 policy 下界时必须拒绝"
        );
        let capped = TradePolicy {
            atr_pct_max: 1.2,
            ..cfg()
        };
        assert!(
            evaluate(&data, mark, atr_for(1.5), capped, features).is_none(),
            "atr_pct≈1.5 高于 policy 上界时必须拒绝"
        );
        let inverted = TradePolicy {
            atr_pct_min: 1.6,
            atr_pct_max: 1.4,
            ..cfg()
        };
        assert!(
            evaluate(&data, mark, atr_for(1.5), inverted, features).is_none(),
            "上下界反转时闭区间恒为空，必须拒绝而不是钳制"
        );
    }

    #[test]
    fn ema_periods_drive_the_signal_kernel() {
        // EMA 周期必须真的进入内核计算：不同周期给出不同的 EMA 差。
        let values: Vec<f64> = (0..60).map(|i| 100.0 + i as f64).collect();
        let data = candles(&values);
        let mark = *values.last().unwrap();
        let atr = mark * 0.01;
        let gap = |fast: usize, slow: usize| {
            core(
                &data,
                mark,
                atr,
                TradePolicy {
                    ema: EmaPeriods { fast, slow },
                    ..cfg()
                },
                MarketFeatures::default(),
            )
            .expect("K 线足够时应给出内核结果")
            .gap_pct
        };
        let wide = gap(9, 21);
        let narrow = gap(10, 11);
        assert!(
            (wide - narrow).abs() > 1e-9,
            "EMA 周期必须真的影响内核的 gap（{wide} vs {narrow}）"
        );
    }

    #[test]
    fn adx_period_drives_the_signal_kernel() {
        // 最近 6 根走平、此前单边上涨：短周期 ADX 被走平段稀释得更多，
        // 长周期仍保留更多趋势段，两个周期必须给出不同的 ADX。
        let mut values: Vec<f64> = (0..40).map(|i| 100.0 + i as f64).collect();
        values.extend(std::iter::repeat_n(139.0, 6));
        let data = candles(&values);
        let mark = *values.last().unwrap();
        let atr = mark * 0.01;
        let adx_for = |period: usize| {
            core(
                &data,
                mark,
                atr,
                TradePolicy {
                    adx_period: period,
                    ..cfg()
                },
                MarketFeatures::default(),
            )
            .expect("K 线足够时应给出内核结果")
            .adx
        };
        let short = adx_for(7);
        let long = adx_for(14);
        assert!(
            (short - long).abs() > 1.0,
            "ADX 周期必须真的进入内核计算（{short} vs {long}）"
        );
    }

    #[test]
    fn required_candles_cover_the_widest_indicator_window() {
        assert_eq!(required_candles(&cfg()), 30, "默认 14/9/21 的基础窗口是 30");
        assert_eq!(
            required_candles(&TradePolicy {
                adx_period: 40,
                ..cfg()
            }),
            41,
            "ADX 需要 period + 1 个窗口"
        );
        assert_eq!(
            required_candles(&TradePolicy {
                ema: EmaPeriods { fast: 9, slow: 60 },
                ..cfg()
            }),
            60,
            "EMA 慢线需要 period 根收盘价"
        );
    }

    #[test]
    fn core_returns_none_when_configured_period_exceeds_candles() {
        let values: Vec<f64> = (0..40).map(|i| 100.0 + i as f64).collect();
        let data = candles(&values);
        let mark = *values.last().unwrap();
        let atr = mark * 0.01;
        assert!(
            core(
                &data,
                mark,
                atr,
                TradePolicy {
                    ema: EmaPeriods { fast: 9, slow: 50 },
                    ..cfg()
                },
                MarketFeatures::default(),
            )
            .is_none(),
            "ema_slow 超过可用 K 线时内核必须拒绝，且由告警暴露配置问题"
        );
        assert!(
            core(
                &data,
                mark,
                atr,
                TradePolicy {
                    adx_period: 40,
                    ..cfg()
                },
                MarketFeatures::default(),
            )
            .is_none(),
            "adx_period 用尽 K 线时内核必须拒绝"
        );
    }

    #[test]
    fn ema_uses_first_close_seed() {
        // 全仓唯一 EMA 口径：第一根收盘价播种。alpha=0.5 时 100 → 101 → 102.5。
        let values = [100.0, 102.0, 104.0];
        assert!((ema(&values, 3).unwrap() - 102.5).abs() < 1e-9);
        assert!(ema(&values, 0).is_none(), "0 周期必须拒绝");
        assert!(ema(&values, 4).is_none(), "K 线不足必须拒绝");
    }

    #[test]
    fn preview_returns_none_without_enough_candles() {
        let data = candles(&(0..20).map(|i| 100.0 + i as f64).collect::<Vec<_>>());
        assert!(preview(&data, 120.0, 1.0, cfg(), MarketFeatures::default()).is_none());
    }
}
