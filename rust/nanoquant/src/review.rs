#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReviewAction {
    Agree,
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
}

pub(crate) trait AiReviewer: Send + Sync {
    fn review(&self, proposal: &EntryProposal) -> Result<ReviewDecision, String>;
}

pub(crate) fn apply_review(
    proposal: &EntryProposal,
    reviewer: Option<&dyn AiReviewer>,
    required: bool,
) -> Result<ReviewedEntry, String> {
    if !proposal.leverage.is_finite() || proposal.leverage <= 0.0 {
        return Err("原始杠杆无效".to_string());
    }
    if !proposal.stop_price.is_finite() || proposal.stop_price <= 0.0 {
        return Err("原始止损价无效".to_string());
    }
    let Some(reviewer) = reviewer else {
        if required {
            return Err("AI 复核不可用，按 fail-closed 跳过".to_string());
        }
        return Ok(ReviewedEntry {
            side: proposal.side.clone(),
            leverage: proposal.leverage,
            stop_price: proposal.stop_price,
            take_profit: proposal.take_profit,
            reason: "AI 复核未启用".to_string(),
        });
    };
    let decision = reviewer
        .review(proposal)
        .map_err(|error| format!("AI 复核失败，按 fail-closed 跳过: {error}"))?;
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
) -> Result<ReviewedEntry, String> {
    let side = decision
        .side
        .clone()
        .unwrap_or_else(|| proposal.side.clone());
    let take_profit = decision.take_profit.unwrap_or(proposal.take_profit);
    if !take_profit.is_finite() || take_profit <= 0.0 {
        return Err("AI 返回的止盈价无效".to_string());
    }
    match decision.action {
        ReviewAction::Agree => Ok(ReviewedEntry {
            side,
            leverage: clamp_leverage(
                proposal,
                decision.leverage.filter(|v| v.is_finite() && *v > 0.0),
            )?,
            stop_price: proposal.stop_price,
            take_profit,
            reason: decision.reason,
        }),
        ReviewAction::Reject => Err(if decision.reason.is_empty() {
            "AI 拒绝".to_string()
        } else {
            decision.reason
        }),
        ReviewAction::Tighten => {
            let leverage = clamp_leverage(proposal, decision.leverage)?;
            let stop_price = match decision.stop_price {
                Some(stop) if !stop.is_finite() || stop <= 0.0 => {
                    return Err("AI 返回的止损价无效".to_string())
                }
                Some(stop) if side != proposal.side => stop,
                Some(stop) if side == "long" => stop.max(proposal.stop_price),
                Some(stop) => stop.min(proposal.stop_price),
                None if side == proposal.side => proposal.stop_price,
                None => {
                    let distance = (proposal.mark_price - proposal.stop_price).abs();
                    if side == "long" {
                        proposal.mark_price - distance
                    } else {
                        proposal.mark_price + distance
                    }
                }
            };
            Ok(ReviewedEntry {
                side,
                leverage,
                stop_price,
                take_profit,
                reason: decision.reason,
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
        }));
        let reviewed = apply_review(&proposal(), Some(&too_high), true).unwrap();
        assert_eq!(reviewed.leverage, 20.0, "超过全局上限必须压回上限");
        assert_eq!(reviewed.stop_price, 95.0, "AI 只能收紧多单止损");

        let raised = FakeReviewer(Ok(ReviewDecision {
            action: ReviewAction::Tighten,
            side: None,
            leverage: Some(18.0),
            stop_price: Some(97.0),
            take_profit: None,
            reason: "raise within bounds".to_string(),
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
        }));
        assert!(apply_review(&proposal(), Some(&reject), false).is_err());
        let timeout = FakeReviewer(Err("timeout".to_string()));
        assert!(apply_review(&proposal(), Some(&timeout), false).is_err());
        assert!(apply_review(&proposal(), None, true).is_err());
        assert!(apply_review(&proposal(), None, false).is_ok());
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
        }));
        assert!(apply_review(&proposal(), Some(&invalid), true).is_err());
    }
}
