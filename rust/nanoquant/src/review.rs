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
    pub leverage: f64,
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
            leverage: proposal.leverage,
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
            let leverage = decision.leverage.unwrap_or(proposal.leverage);
            if !leverage.is_finite() || leverage <= 0.0 {
                return Err("AI 返回的杠杆无效".to_string());
            }
            let leverage = leverage.min(proposal.leverage);
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
    fn tighten_never_increases_leverage_or_widens_the_stop() {
        let reviewer = FakeReviewer(Ok(ReviewDecision {
            action: ReviewAction::Tighten,
            side: None,
            leverage: Some(50.0),
            stop_price: Some(90.0),
            take_profit: None,
            reason: "tighten".to_string(),
        }));
        let reviewed = apply_review(&proposal(), Some(&reviewer), true).unwrap();
        assert_eq!(reviewed.leverage, 10.0, "AI 只能降杠杆");
        assert_eq!(reviewed.stop_price, 95.0, "AI 只能收紧多单止损");

        let tightened = FakeReviewer(Ok(ReviewDecision {
            action: ReviewAction::Tighten,
            side: None,
            leverage: Some(5.0),
            stop_price: Some(97.0),
            take_profit: None,
            reason: "tighten".to_string(),
        }));
        let reviewed = apply_review(&proposal(), Some(&tightened), true).unwrap();
        assert_eq!(reviewed.leverage, 5.0);
        assert_eq!(reviewed.stop_price, 97.0);
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
