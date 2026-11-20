#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OiRegime {
    Neutral,
    PriceUpOiUp,
    PriceUpOiDown,
    PriceDownOiUp,
    PriceDownOiDown,
}

pub const DEFAULT_THRESHOLD_PCT: f64 = 0.05;

impl OiRegime {
    pub fn code(self) -> &'static str {
        match self {
            Self::Neutral => "neutral",
            Self::PriceUpOiUp => "price_up_oi_up",
            Self::PriceUpOiDown => "price_up_oi_down",
            Self::PriceDownOiUp => "price_down_oi_up",
            Self::PriceDownOiDown => "price_down_oi_down",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Neutral => "价格与持仓量变化不明显",
            Self::PriceUpOiUp => "价涨 + OI增 → 上升趋势确认",
            Self::PriceUpOiDown => "价涨 + OI减 → 反弹不牢",
            Self::PriceDownOiUp => "价跌 + OI增 → 下降趋势确认",
            Self::PriceDownOiDown => "价跌 + OI减 → 趋势可能尾声",
        }
    }

    pub fn short_label(self) -> &'static str {
        match self {
            Self::Neutral => "中性",
            Self::PriceUpOiUp => "上升确认",
            Self::PriceUpOiDown => "反弹不牢",
            Self::PriceDownOiUp => "下降确认",
            Self::PriceDownOiDown => "趋势尾声",
        }
    }

    pub fn tone(self) -> &'static str {
        match self {
            Self::Neutral => "dim",
            Self::PriceUpOiUp => "up",
            Self::PriceUpOiDown => "warn",
            Self::PriceDownOiUp => "down",
            Self::PriceDownOiDown => "warn",
        }
    }
}

pub fn classify(price_change_pct: f64, oi_change_pct: f64, threshold_pct: f64) -> OiRegime {
    if !price_change_pct.is_finite() || !oi_change_pct.is_finite() {
        return OiRegime::Neutral;
    }
    let threshold = threshold_pct.abs();
    let price_moved = price_change_pct != 0.0 && price_change_pct.abs() >= threshold;
    let oi_moved = oi_change_pct != 0.0 && oi_change_pct.abs() >= threshold;
    if !price_moved || !oi_moved {
        return OiRegime::Neutral;
    }
    match (price_change_pct > 0.0, oi_change_pct > 0.0) {
        (true, true) => OiRegime::PriceUpOiUp,
        (true, false) => OiRegime::PriceUpOiDown,
        (false, true) => OiRegime::PriceDownOiUp,
        (false, false) => OiRegime::PriceDownOiDown,
    }
}

pub fn payload(price_change_pct: f64, oi_change_pct: f64) -> serde_json::Value {
    let regime = classify(price_change_pct, oi_change_pct, DEFAULT_THRESHOLD_PCT);
    serde_json::json!({
        "code": regime.code(),
        "label": regime.label(),
        "short_label": regime.short_label(),
        "tone": regime.tone(),
        "price_change_pct": price_change_pct,
        "oi_change_pct": oi_change_pct,
        "threshold_pct": DEFAULT_THRESHOLD_PCT,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn four_quadrants_match_price_and_open_interest() {
        assert_eq!(classify(1.2, 0.8, 0.05), OiRegime::PriceUpOiUp);
        assert_eq!(classify(1.2, -0.8, 0.05), OiRegime::PriceUpOiDown);
        assert_eq!(classify(-1.2, 0.8, 0.05), OiRegime::PriceDownOiUp);
        assert_eq!(classify(-1.2, -0.8, 0.05), OiRegime::PriceDownOiDown);
    }

    #[test]
    fn tiny_changes_are_neutral() {
        assert_eq!(classify(0.01, 0.01, 0.05), OiRegime::Neutral);
        assert_eq!(classify(1.0, 0.01, 0.05), OiRegime::Neutral);
    }

    #[test]
    fn payload_contains_ai_friendly_labels() {
        let value = payload(1.2, 0.8);
        assert_eq!(value["code"], "price_up_oi_up");
        assert_eq!(value["short_label"], "上升确认");
        assert_eq!(value["tone"], "up");
    }

    #[test]
    fn negative_threshold_is_normalised_to_absolute_value() {
        assert_eq!(classify(1.2, 0.8, -0.05), OiRegime::PriceUpOiUp);
        assert_eq!(classify(-1.2, -0.8, -0.05), OiRegime::PriceDownOiDown);
    }

    #[test]
    fn exactly_at_threshold_counts_as_a_real_move() {
        assert_eq!(classify(0.05, 0.8, 0.05), OiRegime::PriceUpOiUp);
        assert_eq!(classify(-0.05, -0.8, 0.05), OiRegime::PriceDownOiDown);
    }

    #[test]
    fn zero_threshold_still_treats_zero_change_as_neutral() {
        assert_eq!(classify(0.0, 0.0, 0.0), OiRegime::Neutral);
        assert_eq!(classify(0.0, 1.0, 0.0), OiRegime::Neutral);
        assert_eq!(classify(1.0, 0.0, 0.0), OiRegime::Neutral);
        assert_eq!(classify(1.0, 1.0, 0.0), OiRegime::PriceUpOiUp);
    }

    #[test]
    fn payload_preserves_raw_inputs_and_the_threshold() {
        let value = payload(-1.5, 2.0);
        assert_eq!(value["code"], "price_down_oi_up");
        assert_eq!(value["short_label"], "下降确认");
        assert_eq!(value["tone"], "down");
        assert_eq!(value["price_change_pct"], -1.5);
        assert_eq!(value["oi_change_pct"], 2.0);
        assert_eq!(value["threshold_pct"], DEFAULT_THRESHOLD_PCT);
    }
}
