//! 全仓唯一的技术指标实现。
//!
//! 信号内核（trade_policy）、AI 特征快照（ai_analysis）、regime 判定都从这里取指标，
//! 保证「AI 看到的」与「引擎算的」是同一份数字。
use crate::providers::CandleSnapshot;

/// EMA 序列：前 `period` 根的 SMA 作种子，输出长度 = 输入长度 - period + 1。
pub(crate) fn ema_series(values: &[f64], period: usize) -> Option<Vec<f64>> {
    if period == 0 || values.len() < period {
        return None;
    }
    let k = 2.0 / (period as f64 + 1.0);
    let mut out = Vec::with_capacity(values.len() - period + 1);
    let mut seed = values[..period].iter().sum::<f64>() / period as f64;
    out.push(seed);
    for value in &values[period..] {
        seed = value * k + seed * (1.0 - k);
        out.push(seed);
    }
    Some(out)
}

#[allow(dead_code)] // I2 特征快照接入
pub(crate) struct Macd {
    pub dif: f64,
    pub dea: f64,
    pub histogram: f64,
}

/// MACD(12,26,9)。输入不足 `slow` 根时返回 None。
#[allow(dead_code)] // I2 特征快照接入
pub(crate) fn macd(values: &[f64], fast: usize, slow: usize, signal: usize) -> Option<Macd> {
    let fast_ema = ema_series(values, fast)?;
    let slow_ema = ema_series(values, slow)?;
    let dif: Vec<f64> = fast_ema
        .iter()
        .rev()
        .zip(slow_ema.iter().rev())
        .map(|(fast, slow)| fast - slow)
        .collect();
    let dea = ema_series(&dif, signal)?.last().copied()?;
    let dif_last = *dif.last()?;
    Some(Macd {
        dif: dif_last,
        dea,
        histogram: (dif_last - dea) * 2.0,
    })
}

#[allow(dead_code)] // I2 特征快照接入
pub(crate) struct Bollinger {
    pub middle: f64,
    pub upper: f64,
    pub lower: f64,
    /// 收盘价在带内的相对位置：0 = 下轨，1 = 上轨。
    pub percent_b: f64,
}

#[allow(dead_code)] // I2 特征快照接入
pub(crate) fn bollinger(values: &[f64], period: usize, k: f64) -> Option<Bollinger> {
    if period == 0 || values.len() < period {
        return None;
    }
    let window = &values[values.len() - period..];
    let middle = window.iter().sum::<f64>() / period as f64;
    let variance = window.iter().map(|v| (v - middle).powi(2)).sum::<f64>() / period as f64;
    let sd = variance.sqrt();
    let upper = middle + k * sd;
    let lower = middle - k * sd;
    let last = *values.last()?;
    let span = upper - lower;
    let percent_b = if span.abs() > 0.0 {
        (last - lower) / span
    } else {
        0.5
    };
    Some(Bollinger {
        middle,
        upper,
        lower,
        percent_b,
    })
}

/// 成交量加权均价。
#[allow(dead_code)] // I2 特征快照接入
pub(crate) fn vwap(candles: &[CandleSnapshot]) -> Option<f64> {
    if candles.is_empty() {
        return None;
    }
    let mut pv = 0.0;
    let mut volume = 0.0;
    for candle in candles {
        let typical = (candle.high + candle.low + candle.close) / 3.0;
        pv += typical * candle.volume;
        volume += candle.volume;
    }
    if volume <= 0.0 {
        return None;
    }
    Some(pv / volume)
}

#[allow(dead_code)] // I2 特征快照接入
pub(crate) struct AtrChannel {
    pub atr: f64,
    pub upper: f64,
    pub lower: f64,
}

/// 以最新收盘价为中轴的 ATR 通道。
#[allow(dead_code)] // I2 特征快照接入
pub(crate) fn atr_channel(
    candles: &[CandleSnapshot],
    atr_period: usize,
    k: f64,
) -> Option<AtrChannel> {
    let metrics = crate::providers::candle_metrics(candles, atr_period)?;
    let last = candles.last()?.close;
    Some(AtrChannel {
        atr: metrics.atr,
        upper: last + k * metrics.atr,
        lower: last - k * metrics.atr,
    })
}

/// ADX（平均趋向指数）。从 `trade_policy` 原样搬入，算法未改。
pub(crate) fn adx(candles: &[CandleSnapshot], period: usize) -> Option<f64> {
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
    use crate::providers::CandleSnapshot;

    fn closes(values: &[f64]) -> Vec<CandleSnapshot> {
        values
            .iter()
            .enumerate()
            .map(|(index, close)| CandleSnapshot {
                close_time: 1_700_000_000 + index as i64 * 60_000,
                open: *close,
                high: close + 1.0,
                low: close - 1.0,
                close: *close,
                volume: 10.0,
            })
            .collect()
    }

    #[test]
    fn ema_series_seeds_with_sma_and_converges() {
        let series = ema_series(&[1.0, 2.0, 3.0, 4.0, 5.0], 3).unwrap();
        assert_eq!(series.len(), 3, "N 根输入产出 N-2 个 EMA 点");
        assert!((series[0] - 2.0).abs() < 1e-12, "种子是前 3 根的 SMA");
    }

    #[test]
    fn macd_histogram_is_positive_in_an_uptrend() {
        let values: Vec<f64> = (0..60).map(|i| 100.0 + i as f64).collect();
        let macd = macd(&values, 12, 26, 9).unwrap();
        assert!(macd.dif > 0.0, "上涨趋势 DIF 必须为正");
        assert!(macd.histogram.is_finite());
    }

    #[test]
    fn bollinger_percent_b_is_above_half_in_an_uptrend() {
        let values: Vec<f64> = (0..60).map(|i| 100.0 + i as f64).collect();
        let bands = bollinger(&values, 20, 2.0).unwrap();
        assert!(bands.percent_b > 0.5, "收盘在带上方时 %B 应大于 0.5");
        assert!(bands.upper > bands.middle && bands.middle > bands.lower);
    }

    #[test]
    fn vwap_is_volume_weighted() {
        let candles = closes(&[10.0, 20.0]);
        let value = vwap(&candles).unwrap();
        assert!(value > 10.0 && value < 20.0, "VWAP 必须落在价格区间内");
    }

    #[test]
    fn adx_is_shared_and_rejects_short_series() {
        let candles = closes(&(0..40).map(|i| 100.0 + i as f64).collect::<Vec<_>>());
        assert!(adx(&candles, 14).is_some());
        assert!(adx(&candles, 100).is_none(), "窗口不够必须返回 None");
    }
}
