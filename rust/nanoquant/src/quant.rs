//! 量化内核：只做纯计算，不碰数据库、网络、时钟与全局状态。
//!
//! 依赖方向固定为 `runtime → quant`：内核只接收已经加载好的行情与参数，
//! 输出候选开仓信号与落库所需的特征文本。这样实盘、回测与单测可以共用
//! 同一套规则，不会因为取数路径不同而算出两种结论。
//!
//! 半 AI 边界：内核只产出「量化结论」，不判断 AI 是否同意；
//! AI 复核、fail-open 与最终下单由 runtime 在上层处理。

use serde_json::{json, Value};

use crate::providers::CandleSnapshot;
use crate::regime::{HtfGate, Regime};
use crate::trade_policy::{MarketFeatures, TradePolicy};

/// 信号计算的输入：全部由调用方预先取好。
pub(crate) struct EntrySignalInput<'a> {
    pub(crate) candles: &'a [CandleSnapshot],
    pub(crate) mark_price: f64,
    pub(crate) atr: f64,
    pub(crate) atr_pct: f64,
    pub(crate) funding_rate: f64,
    pub(crate) open_interest_change_pct: f64,
    /// 相对强弱与「是否可用」分开传：缺失时不能当成 0 分参与评分。
    pub(crate) relative_strength: f64,
    pub(crate) relative_strength_available: bool,
    pub(crate) strategy: &'a Value,
    pub(crate) policy: TradePolicy,
    pub(crate) mean_reversion_active: bool,
    /// 风控参数（risk_settings 的 JSON）。
    pub(crate) risk: &'a Value,
    /// 交易所给出的最大杠杆；None 表示该字段缺失，按 100 兜底。
    pub(crate) exchange_max_leverage: Option<i64>,
}

/// 一次候选开仓的量化结论。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EntrySignal {
    pub(crate) side: String,
    pub(crate) score: f64,
    pub(crate) quant_side: Option<String>,
    pub(crate) quant_score: Option<f64>,
    pub(crate) preview_side: Option<String>,
    pub(crate) preview_score: Option<f64>,
    pub(crate) quant_passed: bool,
    pub(crate) mean_reversion: bool,
    /// 量化信号给出的理由；仅审计用，空表示没有信号。
    pub(crate) reason: Option<String>,
    pub(crate) leverage: f64,
    pub(crate) margin_pct: f64,
    pub(crate) stop_price: f64,
    pub(crate) take_profit: f64,
    pub(crate) scale: f64,
    pub(crate) journal_snapshot: Value,
}

/// 特征与标识构建的输入：注入 ID 需要先拿到信号方向，故分两步。
pub(crate) struct EntryFeatureInput<'a> {
    pub(crate) mode: &'a str,
    pub(crate) symbol: &'a str,
    pub(crate) close_time: i64,
    pub(crate) atr_pct: f64,
    pub(crate) vol_ratio: f64,
    pub(crate) quote_volume: f64,
    pub(crate) spread_bps: f64,
    pub(crate) higher_timeframe: &'a str,
    pub(crate) higher_regime: Option<Regime>,
    pub(crate) htf_gate: HtfGate,
    pub(crate) strategy: &'a Value,
    pub(crate) signal: &'a EntrySignal,
    pub(crate) injected_mantra_ids: Vec<i64>,
    pub(crate) injected_experience_ids: Vec<i64>,
}

/// 落库所需的特征文本与确定性标识。
pub(crate) struct EntryFeatures {
    pub(crate) features_text: String,
    pub(crate) client_id: String,
    pub(crate) decision_id: String,
}

/// 决策 ID：mode + symbol + 信号 K 线 close_time + 方向。
///
/// 同一根 K 线在同一环境下重复扫描必须得到同一个 ID，用来把
/// 信号审计、订单、持仓、成交串成一条链；方向不同视为两次不同决策。
pub(crate) fn decision_id_of(mode: &str, symbol: &str, close_time: i64, side: &str) -> String {
    format!("{mode}:{symbol}:{close_time}:{side}")
}

/// 半 AI 模式下由本地规则决定杠杆：ATR% 相对波动目标给出 0.5–2 倍缩放，
/// 评分相对中性值 0.5 再做加减，最后夹到全局上下限并取整。
pub(crate) fn local_leverage(
    atr_pct: f64,
    score: f64,
    base: f64,
    vol_target: f64,
    score_weight: f64,
    min_lev: f64,
    max_lev: f64,
) -> f64 {
    // 评分带方向：负数是空头，杠杆按强度计算；只有真正的 0 分
    // （如非 fusion 预览信号）才按中性 0.5 处理，避免空头被当成无评分。
    let score = if !score.is_finite() || score == 0.0 {
        0.5
    } else {
        score.abs().clamp(0.0, 1.0)
    };
    let vol = if atr_pct > 0.0 {
        (vol_target / atr_pct).clamp(0.5, 2.0)
    } else {
        1.0
    };
    (base * vol + (score - 0.5) * score_weight)
        .clamp(min_lev, max_lev)
        .round()
}

/// 给定行情与参数，计算本次候选开仓的全部量化字段。
///
/// 这块逻辑此前内联在 `runtime::collect_entries` 的扫描循环里：
/// 判断趋势/均值回归信号、决定杠杆与保证金中枢、算止损止盈与波动缩放。
/// 抽出来之后，runtime 只负责取行情、查配置与落库。
pub(crate) fn evaluate_entry(input: &EntrySignalInput<'_>) -> EntrySignal {
    let features = MarketFeatures {
        funding_rate: input.funding_rate,
        open_interest_change_pct: input.open_interest_change_pct,
        relative_strength: input.relative_strength,
        relative_strength_available: input.relative_strength_available,
    };
    // range 市启用均值回归后，趋势信号不再作为开仓来源：
    // 有 MR 信号就用它，没有就只留审计草稿，避免震荡市被趋势假突破带偏。
    let mr_signal = if input.mean_reversion_active {
        crate::mean_reversion::mean_reversion_signal(input.candles, input.strategy)
    } else {
        None
    };
    let trend_signal = if input.mean_reversion_active {
        None
    } else {
        crate::trade_policy::evaluate(
            input.candles,
            input.mark_price,
            input.atr,
            input.policy,
            features,
        )
    };
    let (
        side,
        score,
        quant_side,
        quant_score,
        preview_side,
        preview_score,
        quant_passed,
        mean_reversion,
        reason,
    ) = if let Some(signal) = mr_signal {
        (
            signal.side.to_string(),
            signal.score,
            Some(signal.side.to_string()),
            Some(signal.score),
            Some(signal.side.to_string()),
            Some(signal.score),
            true,
            true,
            Some(signal.reason),
        )
    } else if input.mean_reversion_active {
        let preview = crate::trade_policy::preview(
            input.candles,
            input.mark_price,
            input.atr,
            input.policy,
            features,
        );
        let preview_side = preview
            .filter(|item| item.side != "flat")
            .map(|item| item.side.to_string());
        let preview_score = preview.map(|item| item.score);
        (
            "long".to_string(),
            0.0,
            None,
            None,
            preview_side,
            preview_score,
            false,
            true,
            None,
        )
    } else {
        match trend_signal {
            Some(signal) => (
                signal.side.to_string(),
                signal.score,
                Some(signal.side.to_string()),
                Some(signal.score),
                Some(signal.side.to_string()),
                Some(signal.score),
                true,
                false,
                Some(signal.reason),
            ),
            None => {
                let preview = crate::trade_policy::preview(
                    input.candles,
                    input.mark_price,
                    input.atr,
                    input.policy,
                    features,
                );
                let preview_side = preview
                    .filter(|item| item.side != "flat")
                    .map(|item| item.side.to_string());
                let preview_score = preview.map(|item| item.score);
                (
                    "long".to_string(),
                    0.0,
                    None,
                    None,
                    preview_side,
                    preview_score,
                    false,
                    false,
                    None,
                )
            }
        }
    };
    let upper = input.risk["max_leverage"]
        .as_i64()
        .unwrap_or(100)
        .min(
            input
                .exchange_max_leverage
                .filter(|v| *v > 0)
                .unwrap_or(100),
        )
        .clamp(1, 100);
    let lower = input.risk["min_leverage"]
        .as_i64()
        .unwrap_or(1)
        .clamp(1, upper);
    // 杠杆由本地规则按 ATR 与评分计算，AI 复核不再决定杠杆。
    let leverage = local_leverage(
        input.atr_pct,
        score,
        input.risk["local_leverage_base"].as_f64().unwrap_or(10.0),
        input.risk["local_leverage_vol_target"]
            .as_f64()
            .unwrap_or(2.0),
        input.risk["local_leverage_score_weight"]
            .as_f64()
            .unwrap_or(10.0),
        lower as f64,
        upper as f64,
    );
    // 保证金以参数值为量化中枢，AI 不再调整该比例。
    let margin_pct = input.risk["margin_pct"].as_f64().unwrap_or(20.0);
    // 趋势仓止损距离只由 ATR 决定，与杠杆解耦；止盈交给 ROI 阶梯与快平任务。
    let atr_stop_mult = input.risk["stop_distance_atr_mult"].as_f64().unwrap_or(1.3);
    // 均值回归仓使用独立的保证金止损止盈率，不继承趋势仓的全局阈值。
    let (stop_price, take_profit) = if mean_reversion {
        crate::mean_reversion::mean_reversion_stops(
            &side,
            input.mark_price,
            leverage,
            input.strategy,
        )
    } else {
        let stop_price =
            crate::risk::atr_stop_price(&side, input.mark_price, input.atr_pct, atr_stop_mult);
        (stop_price, 0.0)
    };
    let target = input.risk["vol_target_atr_pct"].as_f64().unwrap_or(0.0);
    let scale = if target > 0.0 && input.atr_pct > 0.0 {
        (target / input.atr_pct).clamp(0.25, 2.0)
    } else {
        1.0
    };
    EntrySignal {
        side,
        score,
        quant_side,
        quant_score,
        preview_side,
        preview_score,
        quant_passed,
        mean_reversion,
        reason,
        leverage,
        margin_pct,
        stop_price,
        take_profit,
        scale,
        journal_snapshot: json!({
            "atr_pct": input.atr_pct,
            "funding": input.funding_rate,
            "oi_change_pct": input.open_interest_change_pct,
        }),
    }
}

/// 组装落库特征与确定性标识。AI 注入 ID 由调用方查好后传入，
/// 这里只做纯序列化，保证同一输入产生完全一致的特征文本。
pub(crate) fn entry_features(input: EntryFeatureInput<'_>) -> EntryFeatures {
    let signal = input.signal;
    let features_text = serde_json::to_string(&json!({
        "decision_id": decision_id_of(input.mode, input.symbol, input.close_time, &signal.side),
        "injected_mantra_ids": &input.injected_mantra_ids,
        "injected_experience_ids": &input.injected_experience_ids,
        "atr_pct": input.atr_pct,
        "vol_ratio": input.vol_ratio,
        "quote_volume": input.quote_volume,
        "spread_bps": input.spread_bps,
        "journal_snapshot": &signal.journal_snapshot,
        "quant_side": &signal.quant_side,
        "quant_score": signal.quant_score,
        "preview_side": &signal.preview_side,
        "preview_score": signal.preview_score,
        "quant_passed": signal.quant_passed,
        "mean_reversion": signal.mean_reversion,
        "higher_timeframe": input.higher_timeframe,
        "higher_regime": input.higher_regime.map(|regime| regime.as_str()),
        "htf_status": input.htf_gate.status(),
        "mr_sl_pct": if signal.mean_reversion { input.strategy["mr_sl_pct"].as_f64().unwrap_or(20.0) } else { 0.0 },
        "mr_tp_pct": if signal.mean_reversion { input.strategy["mr_tp_pct"].as_f64().unwrap_or(30.0) } else { 0.0 },
        "signal_reason": signal.reason.as_deref(),
    }))
    .unwrap_or_else(|_| "{}".to_string());
    EntryFeatures {
        features_text,
        client_id: format!("demo-entry-{}-{}", input.close_time, input.symbol),
        decision_id: decision_id_of(input.mode, input.symbol, input.close_time, &signal.side),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candles(closes: &[f64]) -> Vec<CandleSnapshot> {
        closes
            .iter()
            .enumerate()
            .map(|(index, close)| CandleSnapshot {
                open: *close,
                high: *close + 0.5,
                low: *close - 0.5,
                close: *close,
                volume: 100.0,
                close_time: index as i64 * 60_000,
            })
            .collect()
    }

    fn policy() -> TradePolicy {
        TradePolicy {
            base_threshold: 0.1,
            volume_ratio_min: 0.5,
            atr_pct_min: 0.05,
            atr_pct_max: 4.0,
            ema: crate::trade_policy::EmaPeriods { fast: 9, slow: 21 },
            adx_period: 14,
        }
    }

    fn risk() -> Value {
        json!({
            "max_leverage": 20,
            "min_leverage": 2,
            "local_leverage_base": 10.0,
            "local_leverage_vol_target": 2.0,
            "local_leverage_score_weight": 10.0,
            "margin_pct": 15.0,
            "stop_distance_atr_mult": 1.3,
            "vol_target_atr_pct": 0.0,
        })
    }

    #[test]
    fn decision_id_is_stable_for_the_same_bar() {
        let first = decision_id_of("demo", "BTCUSDT", 1_700_000_000_000, "long");
        assert_eq!(
            first,
            decision_id_of("demo", "BTCUSDT", 1_700_000_000_000, "long"),
            "同一根 K 线重复扫描必须得到同一个决策 ID"
        );
        assert_ne!(
            first,
            decision_id_of("live", "BTCUSDT", 1_700_000_000_000, "long"),
            "环境不同不能共用决策 ID"
        );
        assert_ne!(
            first,
            decision_id_of("demo", "BTCUSDT", 1_700_000_000_001, "long"),
            "下一根 K 线是新决策"
        );
        assert_ne!(
            first,
            decision_id_of("demo", "BTCUSDT", 1_700_000_000_000, "short"),
            "方向不同是两次决策"
        );
    }

    #[test]
    fn local_leverage_rises_for_low_atr_high_score() {
        let leverage = local_leverage(1.0, 0.9, 10.0, 2.0, 10.0, 1.0, 100.0);
        assert_eq!(leverage, 24.0, "低波动 + 高评分应放大到基准之上");
    }

    #[test]
    fn local_leverage_falls_for_high_atr_low_score() {
        let leverage = local_leverage(4.0, 0.1, 10.0, 2.0, 10.0, 1.0, 100.0);
        assert_eq!(leverage, 1.0, "高波动 + 低评分应压到基准之下");
    }

    #[test]
    fn local_leverage_treats_short_score_by_strength() {
        let long = local_leverage(2.0, 0.8, 6.0, 2.0, 4.0, 5.0, 16.0);
        let short = local_leverage(2.0, -0.8, 6.0, 2.0, 4.0, 5.0, 16.0);
        assert_eq!(short, long, "同样强度的空头评分不能因为负号被当成中性 0.5");
        assert_eq!(short, 7.0, "强度 0.8 应在基准 6x 之上");
    }

    #[test]
    fn local_leverage_clamps_to_bounds() {
        assert_eq!(
            local_leverage(0.5, 1.0, 10.0, 2.0, 10.0, 1.0, 5.0),
            5.0,
            "超过上限时夹到全局杠杆上限"
        );
        assert_eq!(
            local_leverage(8.0, 0.0, 10.0, 2.0, 10.0, 8.0, 100.0),
            8.0,
            "低于下限时夹到全局杠杆下限"
        );
    }

    #[test]
    fn local_leverage_treats_zero_score_as_neutral() {
        assert_eq!(
            local_leverage(1.0, 0.0, 10.0, 2.0, 10.0, 1.0, 100.0),
            local_leverage(1.0, 0.5, 10.0, 2.0, 10.0, 1.0, 100.0),
            "无评分应与中性评分 0.5 等价"
        );
    }

    #[test]
    fn local_leverage_handles_zero_atr() {
        assert_eq!(
            local_leverage(0.0, 0.5, 10.0, 2.0, 10.0, 1.0, 100.0),
            10.0,
            "ATR 缺失时波动倍率回落到 1，按评分中性值给基准杠杆"
        );
    }

    #[test]
    fn flat_market_yields_no_quant_pass_and_neutral_defaults() {
        let data = candles(&vec![100.0; 40]);
        let strategy = json!({});
        let signal = evaluate_entry(&EntrySignalInput {
            candles: &data,
            mark_price: 100.0,
            atr: 1.0,
            atr_pct: 0.0,
            funding_rate: 0.0,
            open_interest_change_pct: 0.0,
            relative_strength: 0.0,
            relative_strength_available: false,
            strategy: &strategy,
            policy: policy(),
            mean_reversion_active: false,
            risk: &risk(),
            exchange_max_leverage: Some(20),
        });
        assert_eq!(signal.side, "long", "无信号时保持审计用的默认方向");
        assert_eq!(signal.score, 0.0);
        assert!(!signal.quant_passed);
        assert!(!signal.mean_reversion);
        assert_eq!(signal.quant_side, None);
        assert_eq!(signal.leverage, 10.0, "ATR 缺失时按评分中性值给基准杠杆");
        assert_eq!(signal.margin_pct, 15.0);
        assert_eq!(signal.scale, 1.0, "未配置波动目标时不缩放");
        assert_eq!(signal.stop_price, 0.0, "ATR% 为 0 时不能硬算出止损价");
        assert_eq!(signal.take_profit, 0.0, "趋势仓不预置固定止盈");
    }

    #[test]
    fn leverage_is_clamped_by_exchange_cap_and_risk_range() {
        let data = candles(&vec![100.0; 40]);
        let strategy = json!({});
        let signal = evaluate_entry(&EntrySignalInput {
            candles: &data,
            mark_price: 100.0,
            atr: 1.0,
            atr_pct: 1.0,
            funding_rate: 0.0,
            open_interest_change_pct: 0.0,
            relative_strength: 0.0,
            relative_strength_available: false,
            strategy: &strategy,
            policy: policy(),
            mean_reversion_active: false,
            risk: &risk(),
            exchange_max_leverage: Some(4),
        });
        assert!(
            (2.0..=4.0).contains(&signal.leverage),
            "杠杆必须落在风控上下限与交易所上限之内: {}",
            signal.leverage
        );
    }

    #[test]
    fn entry_features_carry_decision_id_and_injections() {
        let data = candles(&vec![100.0; 40]);
        let strategy = json!({"mr_sl_pct": 12.0, "mr_tp_pct": 18.0});
        let signal = evaluate_entry(&EntrySignalInput {
            candles: &data,
            mark_price: 100.0,
            atr: 1.0,
            atr_pct: 0.5,
            funding_rate: 0.0001,
            open_interest_change_pct: 1.5,
            relative_strength: 0.2,
            relative_strength_available: true,
            strategy: &strategy,
            policy: policy(),
            mean_reversion_active: false,
            risk: &risk(),
            exchange_max_leverage: Some(20),
        });
        let features = entry_features(EntryFeatureInput {
            mode: "demo",
            symbol: "BTCUSDT",
            close_time: 1_700_000_000_000,
            atr_pct: 0.5,
            vol_ratio: 1.2,
            quote_volume: 1000.0,
            spread_bps: 2.0,
            higher_timeframe: "4h",
            higher_regime: Some(Regime::Trend),
            htf_gate: HtfGate::Ready(Regime::Trend),
            strategy: &strategy,
            signal: &signal,
            injected_mantra_ids: vec![1, 2],
            injected_experience_ids: vec![7],
        });
        let value: Value = serde_json::from_str(&features.features_text).unwrap();
        assert_eq!(value["decision_id"], "demo:BTCUSDT:1700000000000:long");
        assert_eq!(value["injected_mantra_ids"], json!([1, 2]));
        assert_eq!(value["injected_experience_ids"], json!([7]));
        assert_eq!(value["higher_regime"], "trend");
        assert_eq!(value["htf_status"], "ok");
        assert_eq!(value["mr_sl_pct"], 0.0, "趋势仓不携带 MR 阈值");
        assert_eq!(
            features.client_id, "demo-entry-1700000000000-BTCUSDT",
            "客户端订单号保持既有格式"
        );
        assert_eq!(features.decision_id, value["decision_id"]);
    }
}
