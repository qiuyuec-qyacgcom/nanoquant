#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReviewAction {
    Agree,
    Hold,
    Reject,
}

/// 一次 AI 复核的分段耗时（毫秒）。
/// `wall_ms` 由 runtime 在复核返回后补齐；`provider_ms` 来自供应商 HTTP 耗时，
/// `parse_ms` 来自解析响应文本的耗时。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct AiLatency {
    pub provider_ms: Option<i64>,
    pub parse_ms: Option<i64>,
    pub wall_ms: Option<i64>,
}

impl AiLatency {
    /// 排队耗时 = 总耗时 - 供应商耗时 - 解析耗时，下限 0。
    pub fn queue_ms(&self) -> Option<i64> {
        let wall = self.wall_ms?;
        Some((wall - self.provider_ms.unwrap_or(0) - self.parse_ms.unwrap_or(0)).max(0))
    }
}

/// AI 复核只能给出同意/否决/降仓与审计信息；方向、杠杆、保证金、止损、止盈
/// 全部来自量化提案，不存在可被 AI 改写的字段。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ReviewDecision {
    pub action: ReviewAction,
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
    /// 复核分段耗时；wall_ms 由 runtime 补齐。
    pub latency: AiLatency,
    /// 本次复核使用的提示词档位：full / slim；未知时为空。
    pub prompt_profile: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EntryProposal {
    pub symbol: String,
    pub side: String,
    pub quant_side: Option<String>,
    pub quant_score: Option<f64>,
    /// 量化杠杆；AI 不可改写。
    pub leverage: f64,
    /// 全局建议保证金比例（量化中枢）；AI 不可改写。
    pub margin_pct: f64,
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
    /// 本次开仓是否真的经过 AI 复核（fail-open 未复核时为 false）。
    pub reviewed: bool,
    /// 归一化后的动作：open_long / open_short / hold。
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
    /// 复核分段耗时；wall_ms 由 runtime 补齐。
    pub latency: AiLatency,
    /// 本次复核使用的提示词档位。
    pub prompt_profile: Option<String>,
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
    /// 复核分段耗时；wall_ms 由 runtime 补齐。
    pub latency: AiLatency,
    /// 本次复核使用的提示词档位。
    pub prompt_profile: Option<String>,
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
            latency: value.latency,
            prompt_profile: value.prompt_profile.clone(),
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
            latency: value.latency,
            prompt_profile: value.prompt_profile.clone(),
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
            latency: value.latency,
            prompt_profile: value.prompt_profile.clone(),
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
    /// 复核分段耗时；wall_ms 由 runtime 补齐。
    pub latency: AiLatency,
    /// 本次复核使用的提示词档位。
    pub prompt_profile: Option<String>,
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
            latency: AiLatency::default(),
            prompt_profile: None,
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
        self.latency = decision.latency;
        self.prompt_profile = meta.prompt_profile;
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
        if proposal.stop_price <= 0.0 {
            return Err(ReviewError::new("缺少有效止损价，无法开仓", "reject"));
        }
        return Ok(ReviewedEntry {
            side: proposal.side.clone(),
            leverage: proposal.leverage,

            stop_price: proposal.stop_price,
            take_profit: proposal.take_profit,
            reason: "AI 复核未启用".to_string(),
            reviewed: false,
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
            latency: AiLatency::default(),
            prompt_profile: None,
        });
    };
    let decision = reviewer.review(proposal).map_err(|error| {
        ReviewError::new(format!("AI 复核失败，按 fail-open 降仓: {error}"), "error")
    })?;
    apply_decision(proposal, decision)
}

/// AI 只能同意/否决/降仓：方向、杠杆、保证金、止损、止盈始终取自量化提案。
#[allow(clippy::result_large_err)]
fn apply_decision(
    proposal: &EntryProposal,
    decision: ReviewDecision,
) -> Result<ReviewedEntry, ReviewError> {
    let confidence = decision.confidence;
    let meta = AiMeta::from(&decision);
    let latency = decision.latency;
    let scale = decision
        .scale
        .filter(|value| value.is_finite() && *value > 0.0)
        .map(|value| value.clamp(0.0, 1.0));
    if proposal.stop_price <= 0.0 {
        return Err(ReviewError::new("缺少有效止损价，无法开仓", "reject").with_meta(&decision));
    }
    match decision.action {
        ReviewAction::Agree => Ok(ReviewedEntry {
            side: proposal.side.clone(),
            leverage: proposal.leverage,

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
            reviewed: true,
            action: action_for(&proposal.side),
            confidence,
            scale,
            latency,
            prompt_profile: meta.prompt_profile.clone(),
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
            latency,
            prompt_profile: meta.prompt_profile.clone(),
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
            latency,
            prompt_profile: meta.prompt_profile.clone(),
        }),
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
            margin_pct: 20.0,
            stop_price: 95.0,
            take_profit: 110.0,
            mark_price: 100.0,
        }
    }

    fn decision(action: ReviewAction) -> ReviewDecision {
        ReviewDecision {
            action,
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
            latency: AiLatency::default(),
            prompt_profile: None,
        }
    }

    #[test]
    fn agree_keeps_the_original_plan() {
        let reviewer = FakeReviewer(Ok(decision(ReviewAction::Agree)));
        let reviewed = apply_review(&proposal(), Some(&reviewer), true).unwrap();
        assert_eq!(reviewed.leverage, 10.0);
        assert_eq!(reviewed.stop_price, 95.0);
        assert!(reviewed.reviewed, "真实复核结果必须标记 reviewed");
    }

    #[test]
    fn ai_can_only_approve_reject_or_scale() {
        let mut approved = decision(ReviewAction::Agree);
        approved.confidence = 0.8;
        approved.scale = Some(0.7);
        approved.reason = "AI 同意并降仓".to_string();
        let reviewed = apply_review(&proposal(), Some(&FakeReviewer(Ok(approved))), true).unwrap();
        assert_eq!(reviewed.side, "long");
        assert_eq!(reviewed.leverage, 10.0);

        assert_eq!(reviewed.stop_price, 95.0);
        assert_eq!(reviewed.take_profit, 110.0);
        assert_eq!(
            reviewed.scale,
            Some(0.7),
            "scale 是 AI 唯一能调整的仓位口径"
        );
        assert_eq!(reviewed.action, "open_long");
        assert_eq!(reviewed.confidence, 0.8);
        assert_eq!(reviewed.reason, "AI 同意并降仓");
    }

    #[test]
    fn ai_cannot_change_side_leverage_margin_stop_or_take_profit() {
        let reviewed = apply_review(
            &proposal(),
            Some(&FakeReviewer(Ok(decision(ReviewAction::Agree)))),
            true,
        )
        .unwrap();
        assert_eq!(reviewed.side, "long", "AI 不得改变方向");
        assert_eq!(reviewed.leverage, 10.0, "AI 不得改变杠杆");

        assert_eq!(reviewed.stop_price, 95.0, "AI 不得改变止损");
        assert_eq!(reviewed.take_profit, 110.0, "AI 不得改变止盈");
        assert_eq!(reviewed.scale, None, "AI 未给 scale 时不缩放");
    }

    #[test]
    fn reject_timeout_and_missing_reviewer_are_fail_closed() {
        let reject = FakeReviewer(Ok(ReviewDecision {
            action: ReviewAction::Reject,
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
            latency: AiLatency::default(),
            prompt_profile: None,
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
            latency: AiLatency::default(),
            prompt_profile: None,
        }));
        let error = apply_review(&proposal(), Some(&hold), true).unwrap_err();
        assert_eq!(error.action, "hold");
        assert!((error.confidence - 0.62).abs() < 1e-9);
        assert_eq!(error.reason, "观望");
    }

    #[test]
    fn missing_stop_is_rejected() {
        let mut bad = proposal();
        bad.stop_price = 0.0;
        let error = apply_review(
            &bad,
            Some(&FakeReviewer(Ok(decision(ReviewAction::Agree)))),
            true,
        )
        .unwrap_err();
        assert_eq!(error.action, "reject");
    }

    #[test]
    fn trend_entry_allowed_without_take_profit() {
        // 趋势仓止盈交给 ROI 阶梯与快平任务，开仓只要求有效止损价。
        let mut no_tp = proposal();
        no_tp.take_profit = 0.0;
        let reviewed = apply_review(
            &no_tp,
            Some(&FakeReviewer(Ok(decision(ReviewAction::Agree)))),
            true,
        )
        .expect("take_profit=0 的趋势仓必须允许开仓");
        assert_eq!(reviewed.stop_price, 95.0);
        assert_eq!(reviewed.take_profit, 0.0);
    }

    #[test]
    fn fail_open_branch_allows_missing_take_profit() {
        // AI 不可用时的 fail-open 分支同样只查止损价，
        // 否则表现为「AI 一超时就不开仓」。
        let mut no_tp = proposal();
        no_tp.take_profit = 0.0;
        let reviewed =
            apply_review(&no_tp, None, false).expect("fail-open 分支不得因 take_profit=0 拒单");
        assert_eq!(reviewed.take_profit, 0.0);
    }

    #[test]
    fn unreviewed_entry_keeps_quant_plan() {
        let unreviewed = apply_review(&proposal(), None, false).unwrap();
        assert_eq!(unreviewed.side, "long");
        assert_eq!(unreviewed.leverage, 10.0);
        assert_eq!(unreviewed.stop_price, 95.0);
        assert_eq!(unreviewed.take_profit, 110.0);
        assert!(
            !unreviewed.reviewed,
            "fail-open 未复核必须标记 reviewed=false"
        );
    }

    #[test]
    fn queue_ms_never_negative() {
        let latency = AiLatency {
            provider_ms: Some(120),
            parse_ms: Some(10),
            wall_ms: Some(100),
        };
        assert_eq!(latency.queue_ms(), Some(0), "扣减超过总耗时时必须夹到 0");

        let normal = AiLatency {
            provider_ms: Some(60),
            parse_ms: Some(10),
            wall_ms: Some(100),
        };
        assert_eq!(normal.queue_ms(), Some(30), "排队 = 总耗时 - 供应商 - 解析");

        assert_eq!(
            AiLatency::default().queue_ms(),
            None,
            "无总耗时不产出排队值"
        );
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
        let error = apply_review(&proposal(), Some(&FakeReviewer(Ok(rejected))), true)
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
        let error = apply_review(&proposal(), Some(&FakeReviewer(Ok(rejected))), true)
            .expect_err("Reject 必须转成错误");
        assert_eq!(error.scale, Some(1.0), "scale 必须夹到 0~1");
        assert_eq!(AiMeta::from(&error).scale, Some(1.0));
    }

    #[test]
    fn rejected_signal_persists_ai_meta_in_features() {
        let mut rejected = decision(ReviewAction::Reject);
        rejected.data_quality = Some(0.3);
        rejected.rr_target = Some(0.5);
        let error = apply_review(&proposal(), Some(&FakeReviewer(Ok(rejected))), true).unwrap_err();
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
