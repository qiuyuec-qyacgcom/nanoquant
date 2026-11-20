use crate::providers::CandleSnapshot;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Signal {
    pub side: String,
    pub score: f64,
    pub confidence: f64,
    pub stop_price: f64,
    pub take_profit: f64,
    pub reason: String,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Config {
    pub mode: &'static str,
    pub entry_threshold: f64,
    pub sl_atr_mult: f64,
    pub tp_atr_mult: f64,
    pub mr_z_entry: f64,
    pub mr_z_exit: f64,
    pub min_risk_reward: f64,
}

pub(crate) fn evaluate(
    candles: &[CandleSnapshot],
    mark: f64,
    atr: f64,
    cfg: Config,
) -> Option<Signal> {
    if candles.len() < 3 || !mark.is_finite() || mark <= 0.0 || !atr.is_finite() || atr <= 0.0 {
        return None;
    }
    let closes: Vec<f64> = candles.iter().map(|c| c.close).collect();
    if closes
        .iter()
        .any(|value| !value.is_finite() || *value <= 0.0)
    {
        return None;
    }
    let (side, score) = match cfg.mode {
        "trend" => {
            let fast = ema(&closes, 9)?;
            let slow = ema(&closes, 21)?;
            let momentum =
                (closes[closes.len() - 1] - closes[closes.len() - 4]) / closes[closes.len() - 4];
            let score = ((fast - slow) / mark * 8.0 + momentum * 4.0).clamp(-1.0, 1.0);
            (if score > 0.0 { "long" } else { "short" }, score)
        }
        "meanrev" => {
            let window = &closes[closes.len() - 20.min(closes.len())..];
            let mean = window.iter().sum::<f64>() / window.len() as f64;
            let variance = window
                .iter()
                .map(|value| (value - mean).powi(2))
                .sum::<f64>()
                / window.len() as f64;
            let z = (mark - mean) / variance.sqrt().max(mark * 1e-9);
            if z.abs() < cfg.mr_z_entry {
                return None;
            }
            let side = if z < 0.0 { "long" } else { "short" };
            let score = (-z / cfg.mr_z_entry).clamp(-1.0, 1.0);
            (side, score)
        }
        _ => return None,
    };
    if score.abs() < cfg.entry_threshold {
        return None;
    }
    let stop_distance = atr * cfg.sl_atr_mult.max(0.2);
    let reward_distance =
        (atr * cfg.tp_atr_mult.max(0.2)).max(stop_distance * cfg.min_risk_reward.max(0.5));
    let direction = if side == "long" { 1.0 } else { -1.0 };
    let stop_price = mark - direction * stop_distance;
    let take_profit = mark + direction * reward_distance;
    if stop_price <= 0.0 || take_profit <= 0.0 {
        return None;
    }
    Some(Signal {
        side: side.into(),
        score,
        confidence: score.abs(),
        stop_price,
        take_profit,
        reason: format!("{}:score={score:.4}", cfg.mode),
    })
}

fn ema(values: &[f64], period: usize) -> Option<f64> {
    if values.len() < period {
        return None;
    }
    let k = 2.0 / (period as f64 + 1.0);
    Some(
        values
            .iter()
            .skip(1)
            .fold(values[0], |ema, value| value * k + ema * (1.0 - k)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn candles(values: &[f64]) -> Vec<CandleSnapshot> {
        values
            .iter()
            .enumerate()
            .map(|(i, close)| CandleSnapshot {
                open: *close,
                high: *close + 1.0,
                low: *close - 1.0,
                close: *close,
                volume: 100.0 + i as f64,
                close_time: i as i64 * 60_000,
            })
            .collect()
    }
    #[test]
    fn trend_signal_has_protective_prices_in_direction() {
        let values: Vec<f64> = (1..=40).map(|i| 100.0 + i as f64).collect();
        let signal = evaluate(
            &candles(&values),
            140.0,
            2.0,
            Config {
                mode: "trend",
                entry_threshold: 0.3,
                sl_atr_mult: 1.2,
                tp_atr_mult: 2.0,
                mr_z_entry: 2.0,
                mr_z_exit: 0.0,
                min_risk_reward: 2.0,
            },
        )
        .unwrap();
        assert_eq!(signal.side, "long");
        assert!(signal.stop_price < 140.0);
        assert!(signal.take_profit > 140.0);
    }
    #[test]
    fn flat_market_has_no_directional_signal() {
        let signal = evaluate(
            &candles(&vec![100.0; 40]),
            100.0,
            2.0,
            Config {
                mode: "trend",
                entry_threshold: 0.3,
                sl_atr_mult: 1.2,
                tp_atr_mult: 2.0,
                mr_z_entry: 2.0,
                mr_z_exit: 0.0,
                min_risk_reward: 2.0,
            },
        );
        assert!(signal.is_none());
    }
}
