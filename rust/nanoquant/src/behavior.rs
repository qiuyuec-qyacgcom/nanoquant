//! 回测行为模型：撮合、费用、保证金、延迟四族可插拔。
//!
//! 默认实现与改造前的写死逻辑**逐行等价** —— 任何数值差异都视为回归。

/// 撮合模型：决定成交价与成交比例。
#[allow(dead_code)] // K2 回测引擎接入
pub trait FillModel: Send + Sync {
    /// 滑点 + 半价差（bps）：由撮合模型自己持有成本假设。
    fn slip_bps(&self) -> f64 {
        0.0
    }
    /// 开平仓方向：1.0 = 买，-1.0 = 卖。
    fn fill_price(&self, price: f64, bps: f64, direction: f64, opening: bool) -> f64;
    /// 成交比例，供部分成交模型使用。
    fn fill_ratio(&self) -> f64 {
        1.0
    }
}

/// 费用模型。
#[allow(dead_code)] // K2 回测引擎接入
pub trait FeeModel: Send + Sync {
    fn taker_rate(&self) -> f64;
    fn maker_rate(&self) -> f64;
}

/// 保证金模型。
#[allow(dead_code)] // K2 回测引擎接入
pub trait MarginModel: Send + Sync {
    fn maintenance_margin_rate(&self) -> f64;
    /// 强平价；返回 None 表示不判定强平。
    fn liquidation_price(&self, entry: f64, direction: f64, leverage: f64) -> Option<f64>;
}

/// 延迟模型：以 K 线根数为单位。
#[allow(dead_code)] // K2 回测引擎接入
pub trait LatencyModel: Send + Sync {
    fn bars(&self) -> usize {
        0
    }
}

/// 默认撮合：滑点 + 半价差，方向符号与改造前一致。
#[allow(dead_code)] // K2 回测引擎接入
pub struct DefaultFill {
    pub slippage_bps: f64,
    pub spread_bps: f64,
}

impl FillModel for DefaultFill {
    fn slip_bps(&self) -> f64 {
        self.slippage_bps + self.spread_bps / 2.0
    }

    fn fill_price(&self, price: f64, bps: f64, direction: f64, opening: bool) -> f64 {
        let rate = bps / 10_000.0;
        let sign = if (opening && direction > 0.0) || (!opening && direction < 0.0) {
            1.0
        } else {
            -1.0
        };
        price * (1.0 + sign * rate)
    }
}

/// 默认费用：只区分 taker。
#[allow(dead_code)] // K2 回测引擎接入
pub struct DefaultFee {
    pub taker_bps: f64,
}

impl FeeModel for DefaultFee {
    fn taker_rate(&self) -> f64 {
        self.taker_bps / 10_000.0
    }

    fn maker_rate(&self) -> f64 {
        self.taker_rate()
    }
}

/// 默认保证金：维持保证金率固定，强平价沿用改造前的算法。
#[allow(dead_code)] // K2 回测引擎接入
pub struct DefaultMargin {
    pub maintenance_margin_rate: f64,
}

impl MarginModel for DefaultMargin {
    fn maintenance_margin_rate(&self) -> f64 {
        self.maintenance_margin_rate
    }

    fn liquidation_price(&self, entry: f64, direction: f64, leverage: f64) -> Option<f64> {
        Some(entry * (1.0 - direction * (1.0 / leverage - self.maintenance_margin_rate)))
    }
}

/// 无延迟。
#[allow(dead_code)] // K2 回测引擎接入
pub struct NoLatency;

impl LatencyModel for NoLatency {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_fill_reproduces_the_current_price_math() {
        // 现状：买入方向成交价上浮，卖出方向下压。
        let model = DefaultFill {
            slippage_bps: 10.0,
            spread_bps: 2.0,
        };
        let bps = model.slip_bps();
        assert!((bps - 11.0).abs() < 1e-9, "滑点 + 半价差");
        let opened = model.fill_price(100.0, bps, 1.0, true);
        assert!((opened - 100.11).abs() < 1e-9, "买入开仓价 {opened}");
        let closed = model.fill_price(100.0, bps, 1.0, false);
        assert!((closed - 99.89).abs() < 1e-9, "卖出平仓价 {closed}");
        let covered = model.fill_price(100.0, bps, -1.0, false);
        assert!((covered - 100.11).abs() < 1e-9, "买入平空价 {covered}");
    }

    #[test]
    fn default_fee_matches_the_configured_taker_rate() {
        let model = DefaultFee { taker_bps: 5.0 };
        assert!((model.taker_rate() - 0.0005).abs() < 1e-12);
        assert!((model.maker_rate() - 0.0005).abs() < 1e-12);
    }

    #[test]
    fn default_margin_keeps_the_previous_liquidation_formula() {
        let model = DefaultMargin {
            maintenance_margin_rate: 0.005,
        };
        assert_eq!(model.maintenance_margin_rate(), 0.005);
        let long = model.liquidation_price(100.0, 1.0, 20.0).unwrap();
        assert!((long - 100.0 * (1.0 - (1.0 / 20.0 - 0.005))).abs() < 1e-9);
        let short = model.liquidation_price(100.0, -1.0, 20.0).unwrap();
        assert!(short > 100.0, "空头强平价必须在开仓价上方");
    }

    #[test]
    fn no_latency_means_zero_bars() {
        assert_eq!(NoLatency.bars(), 0);
    }
}
