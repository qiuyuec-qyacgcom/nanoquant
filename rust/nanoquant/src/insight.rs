//! 决策洞察对象：一次决策的「方向、幅度、置信度、有效期、被谁否决」。

#[derive(Debug, Clone, serde::Serialize)]
pub struct Insight {
    /// long / short / flat。
    pub direction: String,
    /// 信号强度绝对值（0–1）。
    pub magnitude: f64,
    /// AI 置信度；未经过复核时为 0。
    pub confidence: f64,
    /// 决策有效根数。
    pub valid_bars: Option<u64>,
    /// 否决方：`ai_veto` / `news_veto` / `funding_veto` / `risk_gate` / `score_split_low` / `ai_fail_open`。
    pub vetoed_by: Option<String>,
    /// 归一化动作：open_long / open_short / hold / reject。
    pub action: String,
    pub reason: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insight_serializes_with_all_five_facets() {
        let insight = Insight {
            direction: "long".to_string(),
            magnitude: 0.42,
            confidence: 0.77,
            valid_bars: Some(6),
            vetoed_by: None,
            action: "open_long".to_string(),
            reason: "趋势确认".to_string(),
        };
        let value = serde_json::to_value(&insight).unwrap();
        for key in [
            "direction",
            "magnitude",
            "confidence",
            "valid_bars",
            "vetoed_by",
            "action",
            "reason",
        ] {
            assert!(value.get(key).is_some(), "缺少字段 {key}: {value}");
        }
    }
}
