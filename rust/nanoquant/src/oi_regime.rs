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
    let threshold = threshold_pct.abs();
    if price_change_pct.abs() < threshold || oi_change_pct.abs() < threshold {
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
}