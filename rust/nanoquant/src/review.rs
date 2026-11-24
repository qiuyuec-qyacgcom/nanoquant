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
    pub stop_price: Option<f64>,
    pub take_profit: Option<f64>,
    pub reason: String,
    /// AI 自报的置信度（0~1）；没有就 0。
    pub confidence: f64,
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
    pub stop_price: f64,
    pub take_profit: f64,
    pub mark_price: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ReviewedEntry {
    pub side: String,
    pub leverage: f64,
    pub stop_price: f64,
    pub take_profit: f64,
    pub reason: String,
    /// 归一化后的动作：open_long / open_short / tighten。
    pub action: String,
    pub confidence: f64,
}

/// AI 复核没给出可执行结论：原因、动作、置信度都要留痕，
/// 信号审计要能回答「为什么这一刀没开」。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ReviewError {
    pub reason: String,
    /// none / reject / error。
    pub action: String,
    pub confidence: f64,
}

impl ReviewError {
    fn new(reason: impl Into<String>, action: &str) -> Self {
        Self {
            reason: reason.into(),
            action: action.to_string(),
            confidence: 0.0,
        }
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

pub(crate) fn apply_review(
    proposal: &EntryProposal,
    reviewer: Option<&dyn AiReviewer>,
    required: bool,
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
            return Err(ReviewError::new(
                "缺少有效止损/止盈价，无法开仓",
                "reject",
            ));
        }
        return Ok(ReviewedEntry {
            side: proposal.side.clone(),
            leverage: proposal.leverage,
            stop_price: proposal.stop_price,
            take_profit: proposal.take_profit,
            reason: "AI 复核未启用".to_string(),
            action: action_for(&proposal.side),
            confidence: 0.0,
        });
    };
    let decision = reviewer.review(proposal).map_err(|error| {
        ReviewError::new(
            format!("AI 复核失败，按 fail-closed 跳过: {error}"),
            "error",
        )
    })?;
    apply_decision(proposal, decision)
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

fn apply_decision(
    proposal: &EntryProposal,
    decision: ReviewDecision,
) -> Result<ReviewedEntry, ReviewError> {
    let side = decision
        .side
        .clone()
        .unwrap_or_else(|| proposal.side.clone());
    let take_profit = decision.take_profit.unwrap_or(proposal.take_profit);
    if !take_profit.is_finite() || take_profit <= 0.0 {
        return Err(ReviewError::new("AI 返回的止盈价无效", "reject"));
    }
    // AI 可以给出任意有效的止损价；没有给时才回退到仓位止盈止损率计算出的默认价。
    let stop_price = match decision.stop_price {
        Some(stop) if !stop.is_finite() || stop <= 0.0 => {
            return Err(ReviewError::new("AI 返回的止损价无效", "reject"))
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
        None => return Err(ReviewError::new("缺少有效止损价，无法开仓", "reject")),
    };
    if !stop_price.is_finite() || stop_price <= 0.0 {
        return Err(ReviewError::new("缺少有效止损价，无法开仓", "reject"));
    }
    let confidence = decision.confidence;
    match decision.action {
        ReviewAction::Agree => Ok(ReviewedEntry {
            side: side.clone(),
            leverage: clamp_leverage(
                proposal,
                decision.leverage.filter(|v| v.is_finite() && *v > 0.0),
            )
            .map_err(|error| ReviewError::new(error, "reject"))?,
            stop_price,
            take_profit,
            reason: decision.reason,
            action: action_for(&side),
            confidence,
        }),
        ReviewAction::Hold => Err(ReviewError {
            reason: if decision.reason.is_empty() {
                "AI 观望".to_string()
            } else {
                decision.reason
            },
            action: "hold".to_string(),
            confidence,
        }),
        ReviewAction::Reject => Err(ReviewError {
            reason: if decision.reason.is_empty() {
                "AI 拒绝".to_string()
            } else {
                decision.reason
            },
            action: "reject".to_string(),
            confidence,
        }),
        ReviewAction::Tighten => {
            let leverage = clamp_leverage(proposal, decision.leverage)
                .map_err(|error| ReviewError::new(error, "reject"))?;
            Ok(ReviewedEntry {
                side: side.clone(),
                leverage,
                stop_price,
                take_profit,
                reason: decision.reason,
                action: "tighten".to_string(),
                confidence,
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
            stop_price: None,
            take_profit: None,
            reason: String::new(),
            confidence: 0.0,
        }
    }

    #[test]
    fn agree_keeps_the_original_plan() {
        let reviewer = FakeReviewer(Ok(decision(ReviewAction::Agree)));
        let reviewed = apply_review(&proposal(), Some(&reviewer), true).unwrap();
        assert_eq!(reviewed.leverage, 10.0);
        assert_eq!(reviewed.stop_price, 95.0);
    }

    #[test]
    fn leverage_is_clamped_into_the_global_bounds_but_can_rise() {
        let too_high = FakeReviewer(Ok(ReviewDecision {
            action: ReviewAction::Tighten,
            side: None,
            leverage: Some(50.0),
            stop_price: Some(90.0),
            take_profit: None,
            reason: "tighten".to_string(),
            confidence: 0.0,
        }));
        let reviewed = apply_review(&proposal(), Some(&too_high), true).unwrap();
        assert_eq!(reviewed.leverage, 20.0, "超过全局上限必须压回上限");
        assert_eq!(reviewed.stop_price, 90.0, "AI 给出的止损价直接生效");

        let raised = FakeReviewer(Ok(ReviewDecision {
            action: ReviewAction::Tighten,
            side: None,
            leverage: Some(18.0),
            stop_price: Some(97.0),
            take_profit: None,
            reason: "raise within bounds".to_string(),
            confidence: 0.0,
        }));
        let reviewed = apply_review(&proposal(), Some(&raised), true).unwrap();
        assert_eq!(reviewed.leverage, 18.0, "区间内 AI 可以上调杠杆");

        let too_low = FakeReviewer(Ok(ReviewDecision {
            action: ReviewAction::Tighten,
            side: None,
            leverage: Some(5.0),
            stop_price: Some(97.0),
            take_profit: None,
            reason: "tighten".to_string(),
            confidence: 0.0,
        }));
        let reviewed = apply_review(&proposal(), Some(&too_low), true).unwrap();
        assert_eq!(reviewed.leverage, 10.0, "低于下限必须抬回下限");
    }

    #[test]
    fn missing_ai_leverage_falls_back_to_the_lower_bound() {
        let silent = FakeReviewer(Ok(decision(ReviewAction::Tighten)));
        let reviewed = apply_review(&proposal(), Some(&silent), true).unwrap();
        assert_eq!(reviewed.leverage, 10.0, "AI 未给杠杆时取下限");
        let unreviewed = apply_review(&proposal(), None, false).unwrap();
        assert_eq!(unreviewed.leverage, 10.0, "AI 未启用时同样取下限");
    }

    #[test]
    fn reject_timeout_and_missing_reviewer_are_fail_closed() {
        let reject = FakeReviewer(Ok(ReviewDecision {
            action: ReviewAction::Reject,
            side: None,
            leverage: None,
            stop_price: None,
            take_profit: None,
            reason: "行情不匹配".to_string(),
            confidence: 0.0,
        }));
        assert!(apply_review(&proposal(), Some(&reject), false).is_err());
        let timeout = FakeReviewer(Err("timeout".to_string()));
        assert!(apply_review(&proposal(), Some(&timeout), false).is_err());
        assert!(apply_review(&proposal(), None, true).is_err());
        assert!(apply_review(&proposal(), None, false).is_ok());
    }

    #[test]
    fn hold_returns_ai_confidence_without_an_entry() {
        let hold = FakeReviewer(Ok(ReviewDecision {
            action: ReviewAction::Hold,
            side: None,
            leverage: None,
            stop_price: None,
            take_profit: None,
            reason: "观望".to_string(),
            confidence: 0.62,
        }));
        let error = apply_review(&proposal(), Some(&hold), true).unwrap_err();
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
            stop_price: Some(0.0),
            take_profit: None,
            reason: "bad".to_string(),
            confidence: 0.0,
        }));
        assert!(apply_review(&proposal(), Some(&invalid), true).is_err());
    }
}
