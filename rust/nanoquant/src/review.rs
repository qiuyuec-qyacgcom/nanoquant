#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReviewAction {
    Agree,
    Hold,
    Reject,
    Tighten,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ReviewDecision {
    pub action: ReviewAction,
    pub side: Option<String>,
    pub leverage: Option<f64>,
    /// AI 按波动率给出的单仓保证金比例；None 表示使用宿主回退。
    pub margin_pct: Option<f64>,
    pub stop_price: Option<f64>,
    pub take_profit: Option<f64>,
    pub reason: String,
    /// AI 自报的置信度（0~1）；没有就 0。
    pub confidence: f64,
    /// AI 给出的失效价（ip）；用于持仓失效平仓。
    pub invalidation_price: Option<f64>,
    /// AI 估计的有效 K 线根数（vb）。
    pub valid_bars: Option<i64>,
    /// AI 自评的数据质量分（dq）。
    pub data_quality: Option<f64>,
    /// AI 估计的盈亏比（rr）。
    pub rr_target: Option<f64>,
    /// AI 建议的入场价（ep）。
    pub entry_price: Option<f64>,
    /// AI 给出的失效条件文本（inv）。
    pub invalidation_text: Option<String>,
    /// AI 给出的风险提示文本（risk）。
    pub risk_text: Option<String>,
    /// 下单数量缩放系数（0~1）；None 表示不缩放。
    pub scale: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EntryProposal {
    pub symbol: String,
    pub side: String,
    pub quant_side: Option<String>,
    pub quant_score: Option<f64>,
    /// 全局杠杆下限，也是 AI 未给杠杆时的回退值。
    pub leverage: f64,
    /// 全局杠杆上限（已并入交易所/单标的限制）。
    pub max_leverage: f64,
    /// 全局建议保证金比例（AI 的参考中枢）。
    pub margin_pct: f64,
    /// AI 可调整的保证金下限/上限（默认 中枢±5%）。
    pub margin_min_pct: f64,
    pub margin_max_pct: f64,
    pub stop_price: f64,
    pub take_profit: f64,
    pub mark_price: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ReviewedEntry {
    pub side: String,
    pub leverage: f64,
    /// AI 给出的保证金比例；None 时由 runtime 走原有 ATR 缩放。
    pub margin_pct: Option<f64>,
    pub stop_price: f64,
    pub take_profit: f64,
    pub reason: String,
    /// 归一化后的动作：open_long / open_short / tighten。
    pub action: String,
    pub confidence: f64,
    /// AI 给出的失效价（ip）；用于持仓失效平仓。
    pub invalidation_price: Option<f64>,
    /// AI 估计的有效 K 线根数（vb）。
    pub valid_bars: Option<i64>,
    /// AI 自评的数据质量分（dq）。
    pub data_quality: Option<f64>,
    /// AI 估计的盈亏比（rr）。
    pub rr_target: Option<f64>,
    /// AI 建议的入场价（ep）。
    pub entry_price: Option<f64>,
    /// AI 给出的失效条件文本（inv）。
    pub invalidation_text: Option<String>,
    /// AI 给出的风险提示文本（risk）。
    pub risk_text: Option<String>,
    /// 下单数量缩放系数（0~1）；None 表示不缩放。
    pub scale: Option<f64>,
}

/// AI 自评字段的载体：拒单/观望也要把 dq、rr 等落进审计。
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct AiMeta {
    pub invalidation_price: Option<f64>,
    pub valid_bars: Option<i64>,
    pub data_quality: Option<f64>,
    pub rr_target: Option<f64>,
    pub entry_price: Option<f64>,
    pub invalidation_text: Option<String>,
    pub risk_text: Option<String>,
    /// 下单数量缩放系数（0~1）；None 表示不缩放。
    pub scale: Option<f64>,
}

impl From<&ReviewedEntry> for AiMeta {
    fn from(value: &ReviewedEntry) -> Self {
        Self {
            invalidation_price: value.invalidation_price,
            valid_bars: value.valid_bars,
            data_quality: value.data_quality,
            rr_target: value.rr_target,
            entry_price: value.entry_price,
            invalidation_text: value.invalidation_text.clone(),
            risk_text: value.risk_text.clone(),
            scale: value.scale,
        }
    }
}

impl From<&ReviewDecision> for AiMeta {
    fn from(value: &ReviewDecision) -> Self {
        Self {
            invalidation_price: value.invalidation_price,
            valid_bars: value.valid_bars,
            data_quality: value.data_quality,
            rr_target: value.rr_target,
            entry_price: value.entry_price,
            invalidation_text: value.invalidation_text.clone(),
            risk_text: value.risk_text.clone(),
            scale: value.scale,
        }
    }
}

impl From<&ReviewError> for AiMeta {
    fn from(value: &ReviewError) -> Self {
        Self {
            invalidation_price: value.invalidation_price,
            valid_bars: value.valid_bars,
            data_quality: value.data_quality,
            rr_target: value.rr_target,
            entry_price: value.entry_price,
            invalidation_text: value.invalidation_text.clone(),
            risk_text: value.risk_text.clone(),
            scale: value.scale,
        }
    }
}

/// AI 复核没给出可执行结论：原因、动作、置信度都要留痕，
/// 信号审计要能回答「为什么这一刀没开」。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ReviewError {
    pub reason: String,
    /// none / reject / error。
    pub action: String,
    pub confidence: f64,
    pub invalidation_price: Option<f64>,
    pub valid_bars: Option<i64>,
    pub data_quality: Option<f64>,
    pub rr_target: Option<f64>,
    pub entry_price: Option<f64>,
    /// AI 给出的失效条件文本（inv）。
    pub invalidation_text: Option<String>,
    /// AI 给出的风险提示文本（risk）。
    pub risk_text: Option<String>,
    /// 下单数量缩放系数（0~1）；拒绝/观望时也留痕。
    pub scale: Option<f64>,
}

impl ReviewError {
    fn new(reason: impl Into<String>, action: &str) -> Self {
        Self {
            reason: reason.into(),
            action: action.to_string(),
            confidence: 0.0,
            invalidation_price: None,
            valid_bars: None,
            data_quality: None,
            rr_target: None,
            entry_price: None,
            invalidation_text: None,
            risk_text: None,
            scale: None,
        }
    }

    /// 把原始 ReviewDecision 的 AI 自评字段带进错误，避免拒单审计丢失 dq/rr。
    fn with_meta(mut self, decision: &ReviewDecision) -> Self {
        let meta = AiMeta::from(decision);
        self.invalidation_price = meta.invalidation_price;
        self.valid_bars = meta.valid_bars;
        self.data_quality = meta.data_quality;
        self.rr_target = meta.rr_target;
        self.entry_price = meta.entry_price;
        self.invalidation_text = meta.invalidation_text;
        self.risk_text = meta.risk_text;
        self.scale = decision
            .scale
            .filter(|value| value.is_finite() && *value > 0.0)
            .map(|value| value.clamp(0.0, 1.0));
        self
    }
}

/// 方向 → AI 动作名。信号审计按这个字段还原「AI 站在哪一边」。
fn action_for(side: &str) -> String {
    match side {
        "long" => "open_long".to_string(),
        "short" => "open_short".to_string(),
        _ => "hold".to_string(),
    }
}

pub(crate) trait AiReviewer: Send + Sync {
    fn review(&self, proposal: &EntryProposal) -> Result<ReviewDecision, String>;
}

#[allow(clippy::result_large_err)]
pub(crate) fn apply_review(
    proposal: &EntryProposal,
    reviewer: Option<&dyn AiReviewer>,
    required: bool,
    veto_only: bool,
) -> Result<ReviewedEntry, ReviewError> {
    if !proposal.leverage.is_finite() || proposal.leverage <= 0.0 {
        return Err(ReviewError::new("原始杠杆无效", "reject"));
    }
    if !proposal.stop_price.is_finite() || proposal.stop_price < 0.0 {
        return Err(ReviewError::new("原始止损价无效", "reject"));
    }
    let Some(reviewer) = reviewer else {
        if required {
            return Err(ReviewError::new(
                "AI 复核不可用，按 fail-closed 跳过",
                "none",
            ));
        }
        if proposal.stop_price <= 0.0 || proposal.take_profit <= 0.0 {
            return Err(ReviewError::new("缺少有效止损/止盈价，无法开仓", "reject"));
        }
        return Ok(ReviewedEntry {
            side: proposal.side.clone(),
            leverage: proposal.leverage,
            margin_pct: veto_only.then_some(proposal.margin_pct),
            stop_price: proposal.stop_price,
            take_profit: proposal.take_profit,
            reason: "AI 复核未启用".to_string(),
            action: action_for(&proposal.side),
            confidence: 0.0,
            invalidation_price: None,
            valid_bars: None,
            data_quality: None,
            rr_target: None,
            entry_price: None,
            invalidation_text: None,
            risk_text: None,
            scale: None,
        });
    };
    let decision = reviewer.review(proposal).map_err(|error| {
        ReviewError::new(format!("AI 复核失败，按 fail-open 降仓: {error}"), "error")
    })?;
    apply_decision(proposal, decision, veto_only)
}

/// 杠杆由 AI 在 [下限, 上限] 内决定；未给值时回退到下限。
fn clamp_leverage(proposal: &EntryProposal, requested: Option<f64>) -> Result<f64, String> {
    let upper = proposal.max_leverage.max(proposal.leverage);
    match requested {
        None => Ok(proposal.leverage),
        Some(value) if !value.is_finite() || value <= 0.0 => Err("AI 返回的杠杆无效".to_string()),
        Some(value) => Ok(value.clamp(proposal.leverage, upper)),
    }
}

fn clamp_margin(proposal: &EntryProposal, requested: Option<f64>) -> Option<f64> {
    requested
        .filter(|value| value.is_finite() && *value > 0.0)
        .map(|value| value.clamp(proposal.margin_min_pct, proposal.margin_max_pct))
}

#[allow(clippy::result_large_err)]
fn apply_decision(
    proposal: &EntryProposal,
    decision: ReviewDecision,
    veto_only: bool,
) -> Result<ReviewedEntry, ReviewError> {
    let confidence = decision.confidence;
    let meta = AiMeta::from(&decision);
    let scale = decision
        .scale
        .filter(|value| value.is_finite() && *value > 0.0)
        .map(|value| value.clamp(0.0, 1.0));
    // 否决模式：AI 只能拦单和降仓，不得改写方向、杠杆、止损、止盈与保证金。
    if veto_only {
        if proposal.stop_price <= 0.0 || proposal.take_profit <= 0.0 {
            return Err(
                ReviewError::new("缺少有效止损/止盈价，无法开仓", "reject").with_meta(&decision)
            );
        }
        match decision.action {
            ReviewAction::Agree | ReviewAction::Tighten => {
                let action = if decision.action == ReviewAction::Tighten {
                    "tighten".to_string()
                } else {
                    action_for(&proposal.side)
                };
                return Ok(ReviewedEntry {
                    side: proposal.side.clone(),
                    leverage: proposal.leverage,
                    margin_pct: Some(proposal.margin_pct),
                    stop_price: proposal.stop_price,
                    take_profit: proposal.take_profit,
                    invalidation_price: decision.invalidation_price,
                    valid_bars: decision.valid_bars,
                    data_quality: decision.data_quality,
                    rr_target: decision.rr_target,
                    entry_price: decision.entry_price,
                    invalidation_text: decision.invalidation_text,
                    risk_text: decision.risk_text,
                    reason: decision.reason,
                    action,
                    confidence,
                    scale,
                });
            }
            ReviewAction::Hold => {
                return Err(ReviewError {
                    reason: if decision.reason.is_empty() {
                        "AI 观望".to_string()
                    } else {
                        decision.reason
                    },
                    action: "hold".to_string(),
                    confidence,
                    invalidation_price: meta.invalidation_price,
                    valid_bars: meta.valid_bars,
                    data_quality: meta.data_quality,
                    rr_target: meta.rr_target,
                    entry_price: meta.entry_price,
                    invalidation_text: meta.invalidation_text,
                    risk_text: meta.risk_text,
                    scale,
                })
            }
            ReviewAction::Reject => {
                return Err(ReviewError {
                    reason: if decision.reason.is_empty() {
                        "AI 拒绝".to_string()
                    } else {
                        decision.reason
                    },
                    action: "reject".to_string(),
                    confidence,
                    invalidation_price: meta.invalidation_price,
                    valid_bars: meta.valid_bars,
                    data_quality: meta.data_quality,
                    rr_target: meta.rr_target,
                    entry_price: meta.entry_price,
                    invalidation_text: meta.invalidation_text,
                    risk_text: meta.risk_text,
                    scale,
                })
            }
        }
    }
    let side = decision
        .side
        .clone()
        .unwrap_or_else(|| proposal.side.clone());
    let take_profit = decision.take_profit.unwrap_or(proposal.take_profit);
    if !take_profit.is_finite() || take_profit <= 0.0 {
        return Err(ReviewError::new("AI 返回的止盈价无效", "reject").with_meta(&decision));
    }
    // AI 可以给出任意有效的止损价；没有给时才回退到仓位止盈止损率计算出的默认价。
    let stop_price = match decision.stop_price {
        Some(stop) if !stop.is_finite() || stop <= 0.0 => {
            return Err(ReviewError::new("AI 返回的止损价无效", "reject").with_meta(&decision))
        }
        Some(stop) => stop,
        None if side == proposal.side && proposal.stop_price > 0.0 => proposal.stop_price,
        None if proposal.stop_price > 0.0 => {
            let distance = (proposal.mark_price - proposal.stop_price).abs();
            if side == "long" {
                proposal.mark_price - distance
            } else {
                proposal.mark_price + distance
            }
        }
        None => {
            return Err(ReviewError::new("缺少有效止损价，无法开仓", "reject").with_meta(&decision))
        }
    };
    if !stop_price.is_finite() || stop_price <= 0.0 {
        return Err(ReviewError::new("缺少有效止损价，无法开仓", "reject").with_meta(&decision));
    }
    match decision.action {
        ReviewAction::Agree => Ok(ReviewedEntry {
            side: side.clone(),
            leverage: clamp_leverage(
                proposal,
                decision.leverage.filter(|v| v.is_finite() && *v > 0.0),
            )
            .map_err(|error| ReviewError::new(error, "reject"))?,
            margin_pct: clamp_margin(proposal, decision.margin_pct),
            stop_price,
            take_profit,
            invalidation_price: decision.invalidation_price,
            valid_bars: decision.valid_bars,
            data_quality: decision.data_quality,
            rr_target: decision.rr_target,
            entry_price: decision.entry_price,
            invalidation_text: decision.invalidation_text,
            risk_text: decision.risk_text,
            reason: decision.reason,
            action: action_for(&side),
            confidence,
            scale,
        }),
        ReviewAction::Hold => Err(ReviewError {
            reason: if decision.reason.is_empty() {
                "AI 观望".to_string()
            } else {
                decision.reason
            },
            action: "hold".to_string(),
            confidence,
            invalidation_price: meta.invalidation_price,
            valid_bars: meta.valid_bars,
            data_quality: meta.data_quality,
            rr_target: meta.rr_target,
            entry_price: meta.entry_price,
            invalidation_text: meta.invalidation_text,
            risk_text: meta.risk_text,
            scale,
        }),
        ReviewAction::Reject => Err(ReviewError {
            reason: if decision.reason.is_empty() {
                "AI 拒绝".to_string()
            } else {
                decision.reason
            },
            action: "reject".to_string(),
            confidence,
            invalidation_price: meta.invalidation_price,
            valid_bars: meta.valid_bars,
            data_quality: meta.data_quality,
            rr_target: meta.rr_target,
            entry_price: meta.entry_price,
            invalidation_text: meta.invalidation_text,
            risk_text: meta.risk_text,
            scale,
        }),
        ReviewAction::Tighten => {
            let leverage = clamp_leverage(proposal, decision.leverage)
                .map_err(|error| ReviewError::new(error, "reject"))?;
            Ok(ReviewedEntry {
                side: side.clone(),
                leverage,
                margin_pct: clamp_margin(proposal, decision.margin_pct),
                stop_price,
                take_profit,
                invalidation_price: decision.invalidation_price,
                valid_bars: decision.valid_bars,
                data_quality: decision.data_quality,
                rr_target: decision.rr_target,
                entry_price: decision.entry_price,
                invalidation_text: decision.invalidation_text,
                risk_text: decision.risk_text,
                reason: decision.reason,
                action: "tighten".to_string(),
                confidence,
                scale,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeReviewer(Result<ReviewDecision, String>);

    impl AiReviewer for FakeReviewer {
        fn review(&self, _proposal: &EntryProposal) -> Result<ReviewDecision, String> {
            self.0.clone()
        }
    }

    fn proposal() -> EntryProposal {
        EntryProposal {
            symbol: "BTCUSDT".to_string(),
            side: "long".to_string(),
            quant_side: Some("long".to_string()),
            quant_score: Some(0.8),
            leverage: 10.0,
            max_leverage: 20.0,
            margin_pct: 20.0,
            margin_min_pct: 15.0,
            margin_max_pct: 25.0,
            stop_price: 95.0,
            take_profit: 110.0,
            mark_price: 100.0,
        }
    }

    fn decision(action: ReviewAction) -> ReviewDecision {
        ReviewDecision {
            action,
            side: None,
            leverage: None,
            margin_pct: None,
            stop_price: None,
            take_profit: None,
            reason: String::new(),
            confidence: 0.0,
            invalidation_price: None,
            valid_bars: None,
            data_quality: None,
            rr_target: None,
            entry_price: None,
            invalidation_text: None,
            risk_text: None,
            scale: None,
        }
    }

    #[test]
    fn agree_keeps_the_original_plan() {
        let reviewer = FakeReviewer(Ok(decision(ReviewAction::Agree)));
        let reviewed = apply_review(&proposal(), Some(&reviewer), true, false).unwrap();
        assert_eq!(reviewed.leverage, 10.0);
        assert_eq!(reviewed.stop_price, 95.0);
    }

    #[test]
    fn veto_only_ignores_ai_leverage_stop_and_side() {
        let reviewer = FakeReviewer(Ok(ReviewDecision {
            action: ReviewAction::Tighten,
            side: Some("short".to_string()),
            leverage: Some(18.0),
            margin_pct: Some(22.0),
            stop_price: Some(80.0),
            take_profit: Some(200.0),
            reason: "ai tighten".to_string(),
            confidence: 0.8,
            invalidation_price: None,
            valid_bars: None,
            data_quality: None,
            rr_target: None,
            entry_price: None,
            invalidation_text: None,
            risk_text: None,
            scale: Some(0.7),
        }));
        let reviewed = apply_review(&proposal(), Some(&reviewer), true, true).unwrap();
        assert_eq!(reviewed.side, "long", "否决模式必须保留量化方向");
        assert_eq!(reviewed.leverage, 10.0, "AI 杠杆不得生效");
        assert_eq!(reviewed.stop_price, 95.0, "AI 止损不得生效");
        assert_eq!(reviewed.take_profit, 110.0, "AI 止盈不得生效");
        assert_eq!(
            reviewed.margin_pct,
            Some(20.0),
            "保证金必须保留量化中枢比例"
        );
        assert_eq!(reviewed.scale, Some(0.7), "scale 仍可用于降仓");
        assert_eq!(reviewed.action, "tighten", "保留 AI 的动作语义");
        assert_eq!(reviewed.confidence, 0.8);
        assert_eq!(reviewed.reason, "ai tighten");
    }

    #[test]
    fn ai_margin_is_clamped_to_the_configured_center_band() {
        let mut low = decision(ReviewAction::Agree);
        low.margin_pct = Some(12.0);
        let reviewed =
            apply_review(&proposal(), Some(&FakeReviewer(Ok(low))), true, false).unwrap();
        assert_eq!(reviewed.margin_pct, Some(15.0), "低于中枢-5% 必须抬回 15%");

        let mut high = decision(ReviewAction::Agree);
        high.margin_pct = Some(28.0);
        let reviewed =
            apply_review(&proposal(), Some(&FakeReviewer(Ok(high))), true, false).unwrap();
        assert_eq!(reviewed.margin_pct, Some(25.0), "高于中枢+5% 必须压回 25%");

        let missing = decision(ReviewAction::Agree);
        let reviewed =
            apply_review(&proposal(), Some(&FakeReviewer(Ok(missing))), true, false).unwrap();
        assert_eq!(
            reviewed.margin_pct, None,
            "未给 margin 时交给 runtime 走 ATR 回退"
        );
    }

    #[test]
    fn leverage_is_clamped_into_the_global_bounds_but_can_rise() {
        let too_high = FakeReviewer(Ok(ReviewDecision {
            action: ReviewAction::Tighten,
            side: None,
            leverage: Some(50.0),
            margin_pct: None,
            stop_price: Some(90.0),
            take_profit: None,
            reason: "tighten".to_string(),
            confidence: 0.0,
            invalidation_price: None,
            valid_bars: None,
            data_quality: None,
            rr_target: None,
            entry_price: None,
            invalidation_text: None,
            risk_text: None,
            scale: None,
        }));
        let reviewed = apply_review(&proposal(), Some(&too_high), true, false).unwrap();
        assert_eq!(reviewed.leverage, 20.0, "超过全局上限必须压回上限");
        assert_eq!(reviewed.stop_price, 90.0, "AI 给出的止损价直接生效");

        let raised = FakeReviewer(Ok(ReviewDecision {
            action: ReviewAction::Tighten,
            side: None,
            leverage: Some(18.0),
            margin_pct: None,
            stop_price: Some(97.0),
            take_profit: None,
            reason: "raise within bounds".to_string(),
            confidence: 0.0,
            invalidation_price: None,
            valid_bars: None,
            data_quality: None,
            rr_target: None,
            entry_price: None,
            invalidation_text: None,
            risk_text: None,
            scale: None,
        }));
        let reviewed = apply_review(&proposal(), Some(&raised), true, false).unwrap();
        assert_eq!(reviewed.leverage, 18.0, "区间内 AI 可以上调杠杆");

        let too_low = FakeReviewer(Ok(ReviewDecision {
            action: ReviewAction::Tighten,
            side: None,
            leverage: Some(5.0),
            margin_pct: None,
            stop_price: Some(97.0),
            take_profit: None,
            reason: "tighten".to_string(),
            confidence: 0.0,
            invalidation_price: None,
            valid_bars: None,
            data_quality: None,
            rr_target: None,
            entry_price: None,
            invalidation_text: None,
            risk_text: None,
            scale: None,
        }));
        let reviewed = apply_review(&proposal(), Some(&too_low), true, false).unwrap();
        assert_eq!(reviewed.leverage, 10.0, "低于下限必须抬回下限");
    }

    #[test]
    fn missing_ai_leverage_falls_back_to_the_lower_bound() {
        let silent = FakeReviewer(Ok(decision(ReviewAction::Tighten)));
        let reviewed = apply_review(&proposal(), Some(&silent), true, false).unwrap();
        assert_eq!(reviewed.leverage, 10.0, "AI 未给杠杆时取下限");
        let unreviewed = apply_review(&proposal(), None, false, false).unwrap();
        assert_eq!(unreviewed.leverage, 10.0, "AI 未启用时同样取下限");
    }

    #[test]
    fn reject_timeout_and_missing_reviewer_are_fail_closed() {
        let reject = FakeReviewer(Ok(ReviewDecision {
            action: ReviewAction::Reject,
            side: None,
            leverage: None,
            margin_pct: None,
            stop_price: None,
            take_profit: None,
            reason: "行情不匹配".to_string(),
            confidence: 0.0,
            invalidation_price: None,
            valid_bars: None,
            data_quality: None,
            rr_target: None,
            entry_price: None,
            invalidation_text: None,
            risk_text: None,
            scale: None,
        }));
        assert!(apply_review(&proposal(), Some(&reject), false, false).is_err());
        let timeout = FakeReviewer(Err("timeout".to_string()));
        assert!(apply_review(&proposal(), Some(&timeout), false, false).is_err());
        assert!(apply_review(&proposal(), None, true, false).is_err());
        assert!(apply_review(&proposal(), None, false, false).is_ok());
    }

    #[test]
    fn hold_returns_ai_confidence_without_an_entry() {
        let hold = FakeReviewer(Ok(ReviewDecision {
            action: ReviewAction::Hold,
            side: None,
            leverage: None,
            margin_pct: None,
            stop_price: None,
            take_profit: None,
            reason: "观望".to_string(),
            confidence: 0.62,
            invalidation_price: None,
            valid_bars: None,
            data_quality: None,
            rr_target: None,
            entry_price: None,
            invalidation_text: None,
            risk_text: None,
            scale: None,
        }));
        let error = apply_review(&proposal(), Some(&hold), true, false).unwrap_err();
        assert_eq!(error.action, "hold");
        assert!((error.confidence - 0.62).abs() < 1e-9);
        assert_eq!(error.reason, "观望");
    }

    #[test]
    fn invalid_tighten_payload_is_rejected() {
        let invalid = FakeReviewer(Ok(ReviewDecision {
            action: ReviewAction::Tighten,
            side: None,
            leverage: Some(f64::NAN),
            margin_pct: None,
            stop_price: Some(0.0),
            take_profit: None,
            reason: "bad".to_string(),
            confidence: 0.0,
            invalidation_price: None,
            valid_bars: None,
            data_quality: None,
            rr_target: None,
            entry_price: None,
            invalidation_text: None,
            risk_text: None,
            scale: None,
        }));
        assert!(apply_review(&proposal(), Some(&invalid), true, false).is_err());
    }

    #[test]
    fn rejected_review_carries_ai_meta() {
        let mut rejected = decision(ReviewAction::Reject);
        rejected.confidence = 0.9;
        rejected.data_quality = Some(0.3);
        rejected.rr_target = Some(0.5);
        rejected.invalidation_price = Some(97.5);
        rejected.valid_bars = Some(6);
        rejected.entry_price = Some(99.8);
        let error = apply_review(&proposal(), Some(&FakeReviewer(Ok(rejected))), true, false)
            .expect_err("Reject 必须转成错误");
        assert_eq!(error.action, "reject");
        assert_eq!(error.data_quality, Some(0.3));
        assert_eq!(error.rr_target, Some(0.5));
        assert_eq!(error.invalidation_price, Some(97.5));
        assert_eq!(error.valid_bars, Some(6));
        assert_eq!(error.entry_price, Some(99.8));
        let meta = AiMeta::from(&error);
        assert_eq!(meta.data_quality, Some(0.3));
        assert_eq!(meta.rr_target, Some(0.5));
        assert_eq!(meta.invalidation_price, Some(97.5));
        assert_eq!(meta.valid_bars, Some(6));
        assert_eq!(meta.entry_price, Some(99.8));
    }

    #[test]
    fn rejected_review_carries_ai_scale() {
        let mut rejected = decision(ReviewAction::Reject);
        rejected.scale = Some(1.5);
        let error = apply_review(&proposal(), Some(&FakeReviewer(Ok(rejected))), true, false)
            .expect_err("Reject 必须转成错误");
        assert_eq!(error.scale, Some(1.0), "scale 必须夹到 0~1");
        assert_eq!(AiMeta::from(&error).scale, Some(1.0));
    }

    #[test]
    fn rejected_signal_persists_ai_meta_in_features() {
        let mut rejected = decision(ReviewAction::Reject);
        rejected.data_quality = Some(0.3);
        rejected.rr_target = Some(0.5);
        let error =
            apply_review(&proposal(), Some(&FakeReviewer(Ok(rejected))), true, false).unwrap_err();
        let mut row = crate::signal_audit::AuditSignal::new("BTCUSDT");
        let meta = AiMeta::from(&error);
        row.data_quality = meta.data_quality;
        row.rr_target = meta.rr_target;
        row.valid_bars = meta.valid_bars;
        row.invalidation_price = meta.invalidation_price;
        row.entry_price = meta.entry_price;
        let features = row.features_with_meta();
        assert_eq!(features["ai_dq"], 0.3);
        assert_eq!(features["ai_rr"], 0.5);
    }
}
