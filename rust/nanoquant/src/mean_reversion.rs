//! 单品种均值回归信号：只在 `regime=range` 启用。
//!
//! 入场需要 RSI 超买/超卖与布林带越界同向共振，并要求量能收敛；
//! 仓位止损止盈使用独立的保证金口径阈值，不复用趋势仓的全局阈值。

use serde_json::Value;

use crate::providers::CandleSnapshot;

const RSI_PERIOD: usize = 14;
const BAND_PERIOD: usize = 20;

pub(crate) struct MeanReversionSignal {
    pub side: &'static str,
    pub score: f64,
    pub reason: String,
}

/// 当前 regime 是否应走均值回归。缺省关闭，避免空配置的回测误启用。
pub(crate) fn mean_reversion_active(regime: Option<&str>, cfg: &Value) -> bool {
    cfg["mr_enabled"].as_bool().unwrap_or(false) && regime == Some("range")
}

pub(crate) fn mean_reversion_signal(
    candles: &[CandleSnapshot],
    cfg: &Value,
) -> Option<MeanReversionSignal> {
    if candles.len() <= BAND_PERIOD {
        return None;
    }
    let closes: Vec<f64> = candles.iter().map(|candle| candle.close).collect();
    if !closes.iter().all(|value| value.is_finite() && *value > 0.0) {
        return None;
    }
    let last = *closes.last()?;
    if !last.is_finite() || last <= 0.0 {
        return None;
    }
    let rsi_low = cfg["mr_rsi_low"].as_f64().unwrap_or(30.0);
    let rsi_high = cfg["mr_rsi_high"].as_f64().unwrap_or(70.0);
    let band_k = cfg["mr_band_k"].as_f64().unwrap_or(2.0);
    if !(rsi_low.is_finite() && rsi_high.is_finite() && band_k.is_finite())
        || rsi_low <= 0.0
        || rsi_high >= 100.0
        || rsi_low >= rsi_high
        || band_k <= 0.0
    {
        return None;
    }
    let rsi = rsi(&closes, RSI_PERIOD)?;
    let window = &closes[closes.len() - BAND_PERIOD..];
    let mean = window.iter().sum::<f64>() / BAND_PERIOD as f64;
    let variance = window
        .iter()
        .map(|value| (value - mean).powi(2))
        .sum::<f64>()
        / BAND_PERIOD as f64;
    let sd = variance.sqrt();
    if !sd.is_finite() || sd <= 0.0 {
        return None;
    }
    let lower = mean - band_k * sd;
    let upper = mean + band_k * sd;
    let width = upper - lower;
    if !width.is_finite() || width <= 0.0 {
        return None;
    }
    let vol_ratio = volume_ratio(candles)?;
    if vol_ratio >= 1.0 {
        return None;
    }
    let (side, rsi_deviation, band_deviation) = if rsi < rsi_low && last < lower {
        (
            "long",
            ((rsi_low - rsi) / rsi_low).clamp(0.0, 1.0),
            ((lower - last) / width).clamp(0.0, 1.0),
        )
    } else if rsi > rsi_high && last > upper {
        (
            "short",
            ((rsi - rsi_high) / (100.0 - rsi_high)).clamp(0.0, 1.0),
            ((last - upper) / width).clamp(0.0, 1.0),
        )
    } else {
        return None;
    };
    let strength = ((rsi_deviation + band_deviation) / 2.0).clamp(0.0, 1.0);
    let score = (0.5 + 0.5 * strength).clamp(0.5, 1.0);
    Some(MeanReversionSignal {
        side,
        score,
        reason: format!(
            "mean_reversion:side={side}:rsi={rsi:.2}:band={band_k:.1}:lower={lower:.4}:upper={upper:.4}:vol={vol_ratio:.2}"
        ),
    })
}

/// 保证金口径 → 价格止损/止盈。
pub(crate) fn mean_reversion_stops(
    side: &str,
    price: f64,
    leverage: f64,
    cfg: &Value,
) -> (f64, f64) {
    let sl_pct = cfg["mr_sl_pct"].as_f64().unwrap_or(20.0);
    let tp_pct = cfg["mr_tp_pct"].as_f64().unwrap_or(30.0);
    if !price.is_finite() || price <= 0.0 || !leverage.is_finite() || leverage <= 0.0 {
        return (0.0, 0.0);
    }
    let direction = if side == "long" { 1.0 } else { -1.0 };
    let stop = if sl_pct > 0.0 {
        price * (1.0 - direction * sl_pct / leverage / 100.0)
    } else {
        0.0
    };
    let take_profit = if tp_pct > 0.0 {
        price * (1.0 + direction * tp_pct / leverage / 100.0)
    } else {
        0.0
    };
    (stop, take_profit)
}

/// 从 positions.features 读取均值回归仓的独立保证金阈值。
pub(crate) fn position_margin_limits(features: Option<&str>) -> Option<(f64, f64)> {
    let features: Value = serde_json::from_str(features?).ok()?;
    if !features["mean_reversion"].as_bool().unwrap_or(false) {
        return None;
    }
    let sl = features["mr_sl_pct"].as_f64()?;
    let tp = features["mr_tp_pct"].as_f64()?;
    (sl.is_finite() && tp.is_finite() && sl > 0.0 && tp > 0.0).then_some((sl, tp))
}

fn rsi(values: &[f64], period: usize) -> Option<f64> {
    if period == 0 || values.len() <= period {
        return None;
    }
    let slice = &values[values.len() - period - 1..];
    let mut gains = 0.0;
    let mut losses = 0.0;
    for pair in slice.windows(2) {
        let change = pair[1] - pair[0];
        if change >= 0.0 {
            gains += change;
        } else {
            losses -= change;
        }
    }
    let average_gain = gains / period as f64;
    let average_loss = losses / period as f64;
    if average_loss == 0.0 {
        return Some(100.0);
    }
    Some(100.0 - 100.0 / (1.0 + average_gain / average_loss))
}

fn volume_ratio(candles: &[CandleSnapshot]) -> Option<f64> {
    let recent = &candles[candles.len() - BAND_PERIOD..];
    if recent
        .iter()
        .any(|candle| !candle.volume.is_finite() || candle.volume < 0.0)
    {
        return None;
    }
    let average = recent.iter().map(|candle| candle.volume).sum::<f64>() / BAND_PERIOD as f64;
    (average > 0.0).then(|| recent.last().map_or(0.0, |candle| candle.volume) / average)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candle(close: f64, volume: f64) -> CandleSnapshot {
        CandleSnapshot {
            close_time: 0,
            open: close,
            high: close + 0.1,
            low: close - 0.1,
            close,
            volume,
        }
    }

    /// 25 根平盘 + 5 根等速下跌：RSI 触底，收盘跌破布林下轨。
    fn oversold_series(last_volume: f64) -> Vec<CandleSnapshot> {
        let mut values = vec![100.0; 25];
        values.extend([96.0, 92.0, 88.0, 84.0, 80.0]);
        let last = values.len() - 1;
        values
            .into_iter()
            .enumerate()
            .map(|(index, close)| candle(close, if index == last { last_volume } else { 100.0 }))
            .collect()
    }

    /// 25 根平盘 + 5 根等速上涨：RSI 触顶，收盘突破布林上轨。
    fn overbought_series(last_volume: f64) -> Vec<CandleSnapshot> {
        let mut values = vec![100.0; 25];
        values.extend([104.0, 108.0, 112.0, 116.0, 120.0]);
        let last = values.len() - 1;
        values
            .into_iter()
            .enumerate()
            .map(|(index, close)| candle(close, if index == last { last_volume } else { 100.0 }))
            .collect()
    }

    fn cfg() -> Value {
        serde_json::json!({
            "mr_enabled": true,
            "mr_rsi_low": 30,
            "mr_rsi_high": 70,
            "mr_band_k": 2.0,
            "mr_sl_pct": 20,
            "mr_tp_pct": 30,
        })
    }

    #[test]
    fn mean_reversion_buys_oversold_in_range() {
        assert!(mean_reversion_active(Some("range"), &cfg()));
        let signal = mean_reversion_signal(&oversold_series(50.0), &cfg())
            .expect("range 内超卖 + 跌破下轨 + 缩量应给出做多信号");
        assert_eq!(signal.side, "long");
        assert!((0.5..=1.0).contains(&signal.score));
        assert!(signal.reason.contains("mean_reversion"));
    }

    #[test]
    fn mean_reversion_sells_overbought_in_range() {
        assert!(mean_reversion_active(Some("range"), &cfg()));
        let signal = mean_reversion_signal(&overbought_series(50.0), &cfg())
            .expect("range 内超买 + 突破上轨 + 缩量应给出做空信号");
        assert_eq!(signal.side, "short");
        assert!((0.5..=1.0).contains(&signal.score));
    }

    #[test]
    fn mean_reversion_disabled_outside_range() {
        assert!(!mean_reversion_active(Some("trend"), &cfg()));
        assert!(!mean_reversion_active(Some("chaos"), &cfg()));
        assert!(!mean_reversion_active(None, &cfg()));
        let mut disabled = cfg();
        disabled["mr_enabled"] = serde_json::json!(false);
        assert!(!mean_reversion_active(Some("range"), &disabled));
        assert!(
            !mean_reversion_active(Some("range"), &Value::Null),
            "空配置必须默认关闭，避免回测误启用"
        );
    }

    #[test]
    fn mean_reversion_requires_volume_contraction() {
        assert!(
            mean_reversion_signal(&oversold_series(300.0), &cfg()).is_none(),
            "放量下跌是趋势/恐慌，不是缩量均值回归"
        );
        assert!(
            mean_reversion_signal(&overbought_series(300.0), &cfg()).is_none(),
            "放量上涨不构成均值回归做空"
        );
    }

    #[test]
    fn mean_reversion_uses_independent_stop_take_profit() {
        let (stop, take_profit) = mean_reversion_stops("long", 100.0, 10.0, &cfg());
        assert!((stop - 98.0).abs() < 1e-9, "20% 保证金止损 / 10x = 2% 价格距离");
        assert!((take_profit - 103.0).abs() < 1e-9);
        let (short_stop, short_tp) = mean_reversion_stops("short", 100.0, 10.0, &cfg());
        assert!((short_stop - 102.0).abs() < 1e-9);
        assert!((short_tp - 97.0).abs() < 1e-9);
        // 全局 10%/20% 在 10x 下是 99.0 / 102.0，必须与独立阈值区分开。
        assert_ne!(
            (stop, take_profit),
            (99.0, 102.0),
            "不能退回全局 10%/20% 阈值"
        );
    }

    #[test]
    fn position_margin_limits_only_reads_mean_reversion_positions() {
        let features = serde_json::json!({
            "mean_reversion": true,
            "mr_sl_pct": 20,
            "mr_tp_pct": 30,
            "injected_mantra_ids": [1],
        });
        assert_eq!(
            position_margin_limits(Some(&features.to_string())),
            Some((20.0, 30.0))
        );
        let trend = serde_json::json!({"injected_mantra_ids": [1]});
        assert_eq!(position_margin_limits(Some(&trend.to_string())), None);
        assert_eq!(position_margin_limits(None), None);
    }
}