use crate::providers::{candle_metrics, CandleSnapshot};

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

/// regime 确认与冷却状态机。纯内存，调用方负责读写 `engine_state`。
///
/// 首个有 quorum 的 regime 不做确认窗口；之后必须连续保持 `confirm_sec`
/// 且距上次切换不短于 `cooldown_sec` 才生效。
#[derive(Debug, Clone)]
pub(crate) struct RegimeTracker {
    current: Option<Regime>,
    pending: Option<Regime>,
    pending_since: Option<i64>,
    changed_at: i64,
    confirm_sec: i64,
    cooldown_sec: i64,
}

impl RegimeTracker {
    #[cfg(test)]
    pub(crate) fn new(confirm_sec: i64, cooldown_sec: i64) -> Self {
        Self {
            current: None,
            pending: None,
            pending_since: None,
            changed_at: 0,
            confirm_sec,
            cooldown_sec,
        }
    }

    /// 从 `engine_state` 恢复状态；`current` 为空等价于首次启动。
    pub(crate) fn restore(
        current: Option<Regime>,
        pending: Option<Regime>,
        pending_since: Option<i64>,
        changed_at: i64,
        confirm_sec: i64,
        cooldown_sec: i64,
    ) -> Self {
        Self {
            current,
            pending,
            pending_since,
            changed_at,
            confirm_sec,
            cooldown_sec,
        }
    }

    pub(crate) fn current(&self) -> Option<Regime> {
        self.current
    }

    pub(crate) fn pending(&self) -> Option<Regime> {
        self.pending
    }

    pub(crate) fn pending_since(&self) -> Option<i64> {
        self.pending_since
    }

    pub(crate) fn changed_at(&self) -> i64 {
        self.changed_at
    }

    /// 观察本轮的 `market_regime`；返回 `Some` 表示本轮完成一次切换。
    pub(crate) fn observe(&mut self, next: Regime, now: i64) -> Option<Regime> {
        let Some(current) = self.current else {
            self.current = Some(next);
            self.pending = None;
            self.pending_since = None;
            self.changed_at = now;
            return Some(next);
        };
        if next == current {
            self.pending = None;
            self.pending_since = None;
            return None;
        }
        if self.pending != Some(next) {
            self.pending = Some(next);
            self.pending_since = Some(now);
        }
        let since = self.pending_since.unwrap_or(now);
        if now - since >= self.confirm_sec && now - self.changed_at >= self.cooldown_sec {
            self.current = Some(next);
            self.changed_at = now;
            self.pending = None;
            self.pending_since = None;
            return Some(next);
        }
        None
    }
}

/// 启用标的池的等权多数 regime；少于 quorum 无结果，平票取 range。
///
/// 允许调用方按标的池规模下调 quorum（例如回测 basket 存在单标的缺失）。
pub(crate) fn market_regime_with_quorum(regimes: &[Regime], quorum: usize) -> Option<Regime> {
    if quorum == 0 || regimes.len() < quorum {
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

/// 实盘默认判定：固定要求 `MARKET_QUORUM` 个标的同时投票。
pub(crate) fn market_regime(regimes: &[Regime]) -> Option<Regime> {
    market_regime_with_quorum(regimes, MARKET_QUORUM)
}

/// 逐根 bar 计算前缀的 regime；不足 30 根或指标不可用时为 None。
///
/// A 期只读判定与记录，B 期回测复用。
pub(crate) fn regime_series(
    candles: &[CandleSnapshot],
    atr_period: usize,
    policy: crate::trade_policy::TradePolicy,
) -> Vec<Option<Regime>> {
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
                policy,
                crate::trade_policy::MarketFeatures::default(),
            )
        })
        .collect()
}

/// 用本周期 K 线聚合出高周期 K 线，并给出其 regime。
///
/// 高周期非法、不能整除本周期、聚合后不足 30 根或指标不可用时返回 None；
/// 调用方按「不可判定即放行」处理，避免把系统整体锁死。
pub(crate) fn higher_timeframe_regime(
    candles: &[CandleSnapshot],
    higher: &str,
    base: &str,
    atr_period: usize,
    policy: crate::trade_policy::TradePolicy,
) -> Option<Regime> {
    let higher_ms = crate::history::interval_ms(higher)?;
    let base_ms = crate::history::interval_ms(base)?;
    if higher_ms <= base_ms || higher_ms % base_ms != 0 {
        return None;
    }
    let aggregated = aggregate_candles(candles, higher_ms);
    if aggregated.len() < MIN_BARS_FOR_REGIME {
        return None;
    }
    let mark = aggregated.last()?.close;
    let metrics = candle_metrics(&aggregated, atr_period)?;
    crate::trade_policy::regime_for(
        &aggregated,
        mark,
        metrics.atr,
        policy,
        crate::trade_policy::MarketFeatures::default(),
    )
}

/// 高周期过滤结果。
///
/// `Off`：未启用（空字符串或与本周期相同）；`Ready`：已判定；
/// `InsufficientData`：已配置但聚合后 K 线不足或周期不可用，调用方必须显式展示后放行。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HtfGate {
    Off,
    Ready(Regime),
    InsufficientData,
}

impl HtfGate {
    pub(crate) fn regime(self) -> Option<Regime> {
        match self {
            HtfGate::Ready(regime) => Some(regime),
            HtfGate::Off | HtfGate::InsufficientData => None,
        }
    }

    /// 审计/特征里的稳定状态串。
    pub(crate) fn status(self) -> &'static str {
        match self {
            HtfGate::Off => "off",
            HtfGate::Ready(_) => "ok",
            HtfGate::InsufficientData => "insufficient_htf_data",
        }
    }
}

/// 高周期过滤入口：把「未启用 / 已判定 / 行情不足」三态显式区分开。
///
/// 行情不足时仍返回可放行结果，但状态必须是 `InsufficientData`，
/// 由调用方写进特征并告警，避免静默 fail-open。
pub(crate) fn higher_timeframe_gate(
    candles: &[CandleSnapshot],
    higher: &str,
    base: &str,
    atr_period: usize,
    policy: crate::trade_policy::TradePolicy,
) -> HtfGate {
    if higher.trim().is_empty() || higher == base {
        return HtfGate::Off;
    }
    match higher_timeframe_regime(candles, higher, base, atr_period, policy) {
        Some(regime) => HtfGate::Ready(regime),
        None => HtfGate::InsufficientData,
    }
}

/// 按 close_time 所在的高周期桶聚合 K 线；输入需按时间升序。
fn aggregate_candles(candles: &[CandleSnapshot], bucket_ms: i64) -> Vec<CandleSnapshot> {
    let mut out: Vec<CandleSnapshot> = Vec::new();
    let mut bucket = i64::MIN;
    for candle in candles {
        let key = candle.close_time / bucket_ms.max(1);
        match out.last_mut() {
            Some(last) if key == bucket => {
                last.high = last.high.max(candle.high);
                last.low = last.low.min(candle.low);
                last.close = candle.close;
                last.close_time = candle.close_time;
                last.volume += candle.volume;
            }
            _ => {
                bucket = key;
                out.push(candle.clone());
            }
        }
    }
    out
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
    fn market_regime_with_lower_quorum_tolerates_missing_symbol() {
        let votes = [
            Regime::Trend,
            Regime::Trend,
            Regime::Trend,
            Regime::Trend,
            Regime::Trend,
            Regime::Range,
        ];
        assert_eq!(market_regime(&votes), Some(Regime::Trend));
        assert_eq!(market_regime_with_quorum(&votes, 5), Some(Regime::Trend));
        assert_eq!(
            market_regime_with_quorum(&votes[..5], 5),
            Some(Regime::Trend),
            "6 票中 5 票同向时，低 quorum 应仍给出结果"
        );
        assert_eq!(
            market_regime_with_quorum(&votes[..2], 3),
            None,
            "票数少于 quorum 时应无结果"
        );
        assert_eq!(
            market_regime_with_quorum(&votes, 0),
            None,
            "quorum 为 0 属于无效配置，应无结果"
        );
    }

    #[test]
    fn regime_series_aligns_to_candles() {
        let candles = synthesized_candles(80);
        let series = regime_series(
            &candles,
            14,
            crate::trade_policy::TradePolicy {
                base_threshold: 0.3,
                volume_ratio_min: 0.8,
                atr_pct_min: 0.05,
                atr_pct_max: 4.0,
                ema: crate::trade_policy::EmaPeriods { fast: 9, slow: 21 },
                adx_period: 14,
            },
        );
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
    fn higher_timeframe_regime_aggregates_and_rejects_bad_inputs() {
        let candles = synthesized_candles(240);
        let policy = crate::trade_policy::TradePolicy {
            base_threshold: 0.3,
            volume_ratio_min: 0.8,
            atr_pct_min: 0.05,
            atr_pct_max: 4.0,
            ema: crate::trade_policy::EmaPeriods { fast: 9, slow: 21 },
            adx_period: 14,
        };
        assert_eq!(
            higher_timeframe_regime(&candles, "4h", "1h", 14, policy),
            Some(Regime::Trend),
            "单边上涨聚合后应判为趋势"
        );
        assert!(
            higher_timeframe_regime(&candles, "1h", "4h", 14, policy).is_none(),
            "高周期必须大于本周期"
        );
        assert!(
            higher_timeframe_regime(&candles, "7m", "1h", 14, policy).is_none(),
            "非法周期必须放行"
        );
        assert!(
            higher_timeframe_regime(&candles[..60], "4h", "1h", 14, policy).is_none(),
            "聚合后不足 30 根必须放行"
        );
    }

    #[test]
    fn higher_timeframe_gate_distinguishes_off_ready_and_insufficient_data() {
        let candles = synthesized_candles(240);
        let policy = crate::trade_policy::TradePolicy {
            base_threshold: 0.3,
            volume_ratio_min: 0.8,
            atr_pct_min: 0.05,
            atr_pct_max: 4.0,
            ema: crate::trade_policy::EmaPeriods { fast: 9, slow: 21 },
            adx_period: 14,
        };
        assert_eq!(
            higher_timeframe_gate(&candles, "", "1h", 14, policy),
            HtfGate::Off,
            "空字符串表示关闭"
        );
        assert_eq!(
            higher_timeframe_gate(&candles, "1h", "1h", 14, policy),
            HtfGate::Off,
            "与本周期相同等于关闭"
        );
        assert_eq!(
            higher_timeframe_gate(&candles, "4h", "1h", 14, policy),
            HtfGate::Ready(Regime::Trend)
        );
        assert_eq!(
            higher_timeframe_gate(&candles[..60], "4h", "1h", 14, policy),
            HtfGate::InsufficientData,
            "聚合后不足 30 根必须显式标记，而不是静默放行"
        );
        assert_eq!(
            higher_timeframe_gate(&candles, "7m", "1h", 14, policy),
            HtfGate::InsufficientData,
            "非法周期同样不可判定"
        );
        assert_eq!(HtfGate::Off.status(), "off");
        assert_eq!(HtfGate::Ready(Regime::Trend).status(), "ok");
        assert_eq!(HtfGate::InsufficientData.status(), "insufficient_htf_data");
    }

    #[test]
    fn regime_strings_round_trip() {
        for regime in [Regime::Trend, Regime::Range, Regime::Chaos] {
            assert_eq!(Regime::from_str(regime.as_str()), Some(regime));
        }
        assert_eq!(Regime::from_str("unknown"), None);
    }

    #[test]
    fn regime_tracker_first_observation_is_immediate() {
        let mut tracker = RegimeTracker::new(1_800, 1_800);
        assert_eq!(tracker.current(), None);
        assert_eq!(tracker.observe(Regime::Range, 1_000), Some(Regime::Range));
        assert_eq!(tracker.current(), Some(Regime::Range));
        assert_eq!(tracker.changed_at(), 1_000);
        assert_eq!(tracker.pending(), None, "首次生效不应留下 pending");
    }

    #[test]
    fn regime_tracker_requires_confirmation_window() {
        let mut tracker = RegimeTracker::new(1_800, 0);
        assert_eq!(tracker.observe(Regime::Range, 0), Some(Regime::Range));

        assert_eq!(
            tracker.observe(Regime::Trend, 60),
            None,
            "刚出现的新 regime 不能立即切换"
        );
        assert_eq!(tracker.current(), Some(Regime::Range));
        assert_eq!(tracker.pending(), Some(Regime::Trend));
        assert_eq!(tracker.pending_since(), Some(60));

        assert_eq!(
            tracker.observe(Regime::Trend, 1_859),
            None,
            "确认窗口未满 1800 秒"
        );
        assert_eq!(
            tracker.observe(Regime::Trend, 1_860),
            Some(Regime::Trend),
            "连续保持达到 confirm_sec 应切换"
        );
        assert_eq!(tracker.changed_at(), 1_860);
        assert_eq!(tracker.pending(), None);
    }

    #[test]
    fn regime_tracker_resets_pending_when_regime_returns() {
        let mut tracker = RegimeTracker::new(1_800, 0);
        tracker.observe(Regime::Range, 0);
        tracker.observe(Regime::Chaos, 60);
        assert_eq!(tracker.pending(), Some(Regime::Chaos));

        assert_eq!(tracker.observe(Regime::Range, 120), None);
        assert_eq!(tracker.pending(), None, "回到当前 regime 应清空 pending");
        assert_eq!(tracker.pending_since(), None);
    }

    #[test]
    fn regime_tracker_respects_cooldown_after_switch() {
        let mut tracker = RegimeTracker::new(1_800, 3_600);
        tracker.observe(Regime::Range, 0);
        tracker.observe(Regime::Trend, 60);
        assert_eq!(
            tracker.observe(Regime::Trend, 1_860),
            None,
            "确认窗口已满但冷却未到，仍保持当前"
        );
        assert_eq!(tracker.current(), Some(Regime::Range));
        assert_eq!(
            tracker.observe(Regime::Trend, 3_600),
            Some(Regime::Trend),
            "冷却期满后允许切换"
        );
    }

    #[test]
    fn regime_tracker_keeps_state_when_quorum_missing() {
        let mut tracker = RegimeTracker::new(0, 0);
        tracker.observe(Regime::Range, 0);
        // 本轮 market_regime 无结果时调用方不调用 observe，状态必须原样保留。
        assert_eq!(tracker.current(), Some(Regime::Range));
        assert_eq!(tracker.pending(), None);
        assert_eq!(tracker.changed_at(), 0);
    }

    #[test]
    fn regime_tracker_restore_continues_pending_window() {
        let mut tracker = RegimeTracker::restore(
            Some(Regime::Range),
            Some(Regime::Trend),
            Some(60),
            0,
            1_800,
            0,
        );
        assert_eq!(
            tracker.observe(Regime::Trend, 1_860),
            Some(Regime::Trend),
            "跨周期恢复后仍按原 pending_since 计算确认窗口"
        );
        assert_eq!(tracker.changed_at(), 1_860);
    }
}
