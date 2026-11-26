use serde_json::{json, Value};

use crate::ai_client::{self, ChatCall};
use crate::ai_providers::{provider_chain, AiState};
use crate::review::{AiReviewer, EntryProposal, ReviewAction, ReviewDecision};

pub(crate) struct EngineAiReviewer {
    state: AiState,
}

impl EngineAiReviewer {
    pub(crate) fn new(state: AiState) -> Self {
        Self { state }
    }
}

impl AiReviewer for EngineAiReviewer {
    fn review(&self, proposal: &EntryProposal) -> Result<ReviewDecision, String> {
        self.state
            .db
            .conn
            .try_lock()
            .map_err(|_| "AI 复核不能在数据库事务内执行".to_string())
            .map(drop)?;
        let state = self.state.clone();
        let proposal = proposal.clone();
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|_| "AI 复核需要 Tokio 运行时".to_string())?;
        tokio::task::block_in_place(|| handle.block_on(review_entry(state, proposal)))
    }
}

async fn review_entry(state: AiState, proposal: EntryProposal) -> Result<ReviewDecision, String> {
    let strategy_cfg = match state.db.setting("strategy") {
        Ok(value) => value.unwrap_or(Value::Null),
        Err(error) => {
            tracing::debug!(error = ?error, "读取 strategy 配置失败，复核门槛回退默认值");
            Value::Null
        }
    };
    let interval = strategy_cfg
        .get("timeframe")
        .and_then(Value::as_str)
        .unwrap_or("15m")
        .to_string();
    let profile = {
        let conn = state
            .db
            .conn
            .lock()
            .map_err(|_| "数据库锁不可用".to_string())?;
        let profile = crate::strategies::profile_for(&conn, &proposal.symbol);
        if let Some(profile) = &profile {
            crate::strategies::mark_applied(&conn, profile);
        }
        profile
    };
    let snapshot = state
        .market
        .snapshot(&proposal.symbol, &interval)
        .await
        .map_err(|error| format!("AI 复核行情不可用: {error}"))?;
    let market = snapshot
        .markets
        .iter()
        .find(|market| market.symbol.eq_ignore_ascii_case(&proposal.symbol))
        .ok_or_else(|| format!("{} 行情不可用", proposal.symbol))?;
    if market.candles.len() < 30 {
        return Err("K 线不足，无法复核".to_string());
    }
    let features = crate::ai_analysis::feature_snapshot(
        market,
        &interval,
        crate::ai_analysis::rsi_period_of(&strategy_cfg),
    );
    let risk = state
        .db
        .setting("risk")
        .ok()
        .flatten()
        .unwrap_or(Value::Null);
    let (cfg, chain) = provider_chain(&state);
    if !cfg.enabled {
        return Err("AI 已禁用".to_string());
    }
    if chain.is_empty() {
        return Err("未配置可用的 AI 模型".to_string());
    }
    let st = profile
        .as_ref()
        .map(crate::strategies::compact)
        .unwrap_or(Value::Null);
    let (mantra_text, mantra_ids, experiences) = {
        let conn = state
            .db
            .conn
            .lock()
            .map_err(|_| "数据库锁不可用".to_string())?;
        let evolution = crate::evolution_api::evolution_cfg(&conn);
        let (mantra_text, mantra_ids) = if cfg.inject_mantras {
            crate::evolution_api::injection_payload(&conn, &evolution)
        } else {
            (String::new(), Vec::new())
        };
        let journal_snapshot = json!({
            "atr_pct": features["atr_pct"],
            "funding": market.funding_rate,
            "oi_change_pct": features["oi_change_pct"],
            "oi_regime": features["oi_regime"],
        });
        let experiences = crate::journal::inject_lessons(
            &conn,
            &proposal.symbol,
            &journal_snapshot,
            &proposal.side,
        );
        (mantra_text, mantra_ids, experiences)
    };
    let news = if cfg.inject_news {
        crate::ai_analysis::news_digest(&state, 8)
    } else {
        Vec::new()
    };
    let quant_signal = proposal.quant_side.as_ref().map(|side| {
        json!({
            "side": side,
            "score": proposal.quant_score,
            "stop_price": proposal.stop_price,
            "take_profit": proposal.take_profit,
            "mark_price": proposal.mark_price,
        })
    });
    let margin_base = risk
        .get("margin_pct")
        .and_then(Value::as_f64)
        .unwrap_or(20.0);
    let margin_min = (margin_base - 5.0).max(0.0);
    let margin_max = (margin_base + 5.0).min(100.0);
    let mut context = json!({
        "analysis_order": [
            "global_strategy",
            "self_evolving_mantras",
            "decision_experiences",
            "symbol_analysis",
            "oi",
            "news_sentiment",
            "quant_signal"
        ],
        "strategy": profile,
        "st": st,
        "mantras": {
            "text": mantra_text,
            "ids": mantra_ids,
        },
        "experiences": experiences,
        "symbol": proposal.symbol,
        "price": market.mark_price,
        "score": features["ema_gap_pct"].as_f64().map(|value| value / 0.22).unwrap_or(0.0),
        "side_hint": proposal.quant_side.clone().unwrap_or_else(|| "flat".to_string()),
        "quant_reasons": ["ema_gap_pct", "rsi", "vol_ratio", "funding", "oi_change_pct", "oi_regime", "atr_pct"],
        "features": features,
        "oi": {
            "change_pct": features["oi_change_pct"],
            "regime": features["oi_regime"],
        },
        "oi_regime_hint": "价涨+OI增=上升趋势确认；价涨+OI减=反弹不牢；价跌+OI增=下降趋势确认；价跌+OI减=趋势可能尾声",
        "klines": crate::ai_analysis::recent_bars(&market.candles, 14),
        "funding": market.funding_rate,
        "change_24h": features["change_24h"],
        "quote_volume": market.quote_volume,
        "account": {
            "equity": snapshot.account.equity,
            "available": snapshot.account.available_balance,
            "open_positions": snapshot.positions.len(),
        },
        "constraints": {
            "max_leverage": risk.get("max_leverage").and_then(Value::as_f64).unwrap_or(50.0),
            "min_leverage": risk.get("min_leverage").and_then(Value::as_f64).unwrap_or(1.0),
            "atr_stop_mult": risk.get("atr_stop_mult").and_then(Value::as_f64).unwrap_or(1.3),
            "margin_pct": margin_base,
            "margin_min_pct": margin_min,
            "margin_max_pct": margin_max,
            "min_confidence": cfg.min_confidence,
        },
        "news": news,
        "quant_signal": quant_signal,
        "proposal": {
            "side": proposal.side,
            "leverage": proposal.leverage,
            "stop_price": proposal.stop_price,
            "mark_price": proposal.mark_price,
        },
    });
    crate::ai_analysis::gate_injected_context(&mut context, cfg.inject_news, cfg.inject_mantras);
    let messages = crate::ai_analysis::decision_messages_with_personas(&context, &cfg.personas);
    let mut call = ChatCall::new(&messages, "engine_review");
    call.max_tokens = Some(16000);
    call.min_max_tokens = true;
    call.temperature = Some(cfg.temperature);
    let outcome =
        ai_client::chat_with_fallback(state.transport.as_ref(), &state.db, &chain, call, &cfg)
            .await;
    if !outcome.ok {
        return Err(format!("AI 复核失败: {}", outcome.error));
    }
    let decision = ai_client::parse_decision(&outcome.text, Some(&context))
        .map_err(|error| format!("AI 复核结果无效: {error}"))?;
    let side_lock = profile
        .as_ref()
        .and_then(|profile| profile.get("filters"))
        .and_then(|filters| filters.get("side"))
        .and_then(Value::as_str)
        .unwrap_or("any");
    // 开仓盈亏比门槛来自策略配置，未配置时回退默认值。
    let min_entry_rr = strategy_cfg
        .get("min_entry_rr")
        .and_then(Value::as_f64)
        .unwrap_or(DEFAULT_MIN_ENTRY_RR);
    // AI 新协议字段照常落在审计里；`use_invalidation_price` / `use_entry_price`
    // 只在消费点（快速风控、限价开仓）决定是否采用，复核出口不得清空。
    let reviewed = decision_to_review(
        &proposal,
        side_lock,
        &decision,
        cfg.min_confidence,
        cfg.require_agreement,
        cfg.allow_ai_override,
        min_entry_rr,
    )?;
    Ok(fill_default_valid_bars(reviewed, &strategy_cfg))
}

/// AI 未给有效期时回退 strategy.valid_bars，未配置则默认 6 根；越界夹到 1..=24。
fn fill_default_valid_bars(
    mut reviewed: ReviewDecision,
    strategy_cfg: &Value,
) -> ReviewDecision {
    if reviewed.valid_bars.is_none() {
        reviewed.valid_bars = strategy_cfg
            .get("valid_bars")
            .and_then(Value::as_i64)
            .map(|v| v.clamp(1, 24))
            .or(Some(6));
    }
    reviewed
}

const DEFAULT_MIN_ENTRY_RR: f64 = 0.8;

fn reject(reason: impl Into<String>) -> ReviewDecision {
    ReviewDecision {
        action: ReviewAction::Reject,
        side: None,
        leverage: None,
        margin_pct: None,
        stop_price: None,
        take_profit: None,
        reason: reason.into(),
        confidence: 0.0,
        invalidation_price: None,
        valid_bars: None,
        data_quality: None,
        rr_target: None,
        entry_price: None,
        invalidation_text: None,
        risk_text: None,
    }
}

/// AI 文本字段规范化：去首尾空白，空内容不落库。
fn text_fields(decision: &ai_client::Decision) -> (Option<String>, Option<String>) {
    let invalidation = decision.invalidation.trim();
    let risk = decision
        .risk_flags
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("；");
    (
        (!invalidation.is_empty()).then(|| invalidation.to_string()),
        (!risk.is_empty()).then_some(risk),
    )
}

/// 拒单时必须把 AI 自评字段一并带出，否则审计层拿不到 dq/rr。
fn reject_with(reason: impl Into<String>, decision: &ai_client::Decision) -> ReviewDecision {
    let (invalidation_text, risk_text) = text_fields(decision);
    ReviewDecision {
        invalidation_price: decision.invalidation_price,
        valid_bars: decision.valid_bars,
        data_quality: decision.data_quality,
        rr_target: decision.rr_target,
        entry_price: decision.entry_price,
        invalidation_text,
        risk_text,
        ..reject(reason)
    }
}

fn decision_to_review(
    proposal: &EntryProposal,
    side_lock: &str,
    decision: &ai_client::Decision,
    min_confidence: f64,
    require_agreement: bool,
    allow_ai_override: bool,
    min_entry_rr: f64,
) -> Result<ReviewDecision, String> {
    if decision.confidence < min_confidence {
        let mut rejected = reject_with(
            format!(
                "AI 置信度 {:.2} 低于 {:.2}",
                decision.confidence, min_confidence
            ),
            decision,
        );
        rejected.confidence = decision.confidence;
        return Ok(rejected);
    }
    let action = decision.action.as_str();
    let side = match action {
        "open_long" => "long",
        "open_short" => "short",
        _ => {
            let (invalidation_text, risk_text) = text_fields(decision);
            return Ok(ReviewDecision {
                action: ReviewAction::Hold,
                side: None,
                leverage: None,
                margin_pct: None,
                stop_price: None,
                take_profit: None,
                reason: if decision.reason.is_empty() {
                    "AI 未批准开仓".to_string()
                } else {
                    decision.reason.clone()
                },
                confidence: decision.confidence,
                invalidation_price: decision.invalidation_price,
                valid_bars: decision.valid_bars,
                data_quality: decision.data_quality,
                rr_target: decision.rr_target,
                entry_price: decision.entry_price,
                invalidation_text,
                risk_text,
            })
        }
    };
    // 复核前置拒单：仅对开仓决策生效，hold/close 不因门槛被改写为 Reject。
    if let Some(dq) = decision.data_quality {
        if dq.is_finite() && dq < min_confidence {
            let mut rejected = reject_with(
                format!("AI 数据质量分 {dq:.2} 低于 {min_confidence:.2}"),
                decision,
            );
            rejected.confidence = decision.confidence;
            return Ok(rejected);
        }
    }
    if let Some(rr) = decision.rr_target {
        if rr.is_finite() && rr < min_entry_rr {
            let mut rejected = reject_with(
                format!("AI 盈亏比 {rr:.2} 低于 {min_entry_rr:.2}"),
                decision,
            );
            rejected.confidence = decision.confidence;
            return Ok(rejected);
        }
    }
    if side_lock == "long" && side == "short" {
        return Ok(reject_with("策略只允许做多，AI 方向相反", decision));
    }
    if side_lock == "short" && side == "long" {
        return Ok(reject_with("策略只允许做空，AI 方向相反", decision));
    }
    if let Some(quant_side) = proposal.quant_side.as_deref() {
        if quant_side != side {
            if require_agreement {
                return Ok(reject_with("AI 与量化方向不一致", decision));
            }
            if !allow_ai_override {
                return Ok(reject_with("AI 与量化方向不一致，且未允许 AI 覆盖", decision));
            }
        }
    } else if require_agreement {
        return Ok(reject_with("缺少量化方向，无法与量化方向一致", decision));
    }
    let mark = proposal.mark_price;
    // 杠杆由 AI 在全局区间内决定：低于下限抬回下限，高于上限压回上限。
    // 止损/止盈百分比按保证金收益率解释，换算价格距离前必须先确定最终杠杆。
    let upper = proposal.max_leverage.max(proposal.leverage);
    let leverage = (decision.leverage.is_finite() && decision.leverage > 0.0)
        .then(|| decision.leverage.clamp(proposal.leverage, upper));
    let effective_leverage = leverage.unwrap_or(proposal.leverage).max(1.0);
    let stop_price = if decision.sl_pct > 0.0 {
        let price_pct = decision.sl_pct / effective_leverage;
        if side == "long" {
            mark * (1.0 - price_pct / 100.0)
        } else {
            mark * (1.0 + price_pct / 100.0)
        }
    } else if side == proposal.side {
        proposal.stop_price
    } else {
        let distance = (mark - proposal.stop_price).abs();
        if side == "long" {
            mark - distance
        } else {
            mark + distance
        }
    };
    let take_profit = if decision.tp_pct > 0.0 {
        let price_pct = decision.tp_pct / effective_leverage;
        if side == "long" {
            mark * (1.0 + price_pct / 100.0)
        } else {
            mark * (1.0 - price_pct / 100.0)
        }
    } else if side == proposal.side {
        proposal.take_profit
    } else {
        let distance = (proposal.take_profit - mark).abs();
        if side == "long" {
            mark + distance
        } else {
            mark - distance
        }
    };
    if !stop_price.is_finite()
        || stop_price <= 0.0
        || !take_profit.is_finite()
        || take_profit <= 0.0
    {
        return Ok(reject_with("AI 返回的止损/止盈价无效", decision));
    }
    let leverage_changed =
        leverage.is_some_and(|value| (value - proposal.leverage).abs() > f64::EPSILON);
    let stop_tightened = match (side, side == proposal.side) {
        ("long", true) => stop_price > proposal.stop_price,
        ("short", true) => stop_price < proposal.stop_price,
        _ => false,
    };
    let changed = side != proposal.side || leverage_changed || stop_tightened;
    let (invalidation_text, risk_text) = text_fields(decision);
    Ok(ReviewDecision {
        action: if changed {
            ReviewAction::Tighten
        } else {
            ReviewAction::Agree
        },
        side: Some(side.to_string()),
        leverage,
        margin_pct: decision.margin_pct,
        stop_price: Some(stop_price),
        take_profit: Some(take_profit),
        reason: decision.reason.clone(),
        confidence: decision.confidence,
        invalidation_price: decision.invalidation_price,
        valid_bars: decision.valid_bars,
        data_quality: decision.data_quality,
        rr_target: decision.rr_target,
        entry_price: decision.entry_price,
        invalidation_text,
        risk_text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proposal(side: &str) -> EntryProposal {
        EntryProposal {
            symbol: "BTCUSDT".to_string(),
            side: side.to_string(),
            quant_side: Some(side.to_string()),
            quant_score: Some(0.8),
            leverage: 2.0,
            max_leverage: 20.0,
            margin_pct: 20.0,
            margin_min_pct: 15.0,
            margin_max_pct: 25.0,
            stop_price: if side == "long" { 95.0 } else { 105.0 },
            take_profit: if side == "long" { 110.0 } else { 90.0 },
            mark_price: 100.0,
        }
    }

    fn decision(action: &str, confidence: f64) -> ai_client::Decision {
        ai_client::Decision {
            action: action.to_string(),
            confidence,
            leverage: 0.0,
            margin_pct: None,
            sl_pct: 0.0,
            tp_pct: 0.0,
            reason: "test".to_string(),
            risk_flags: Vec::new(),
            invalidation: String::new(),
            params: Vec::new(),
            invalidation_price: None,
            valid_bars: None,
            data_quality: None,
            rr_target: None,
            entry_price: None,
        }
    }

    #[test]
    fn blank_invalidation_and_risk_are_ignored() {
        let mut item = decision("hold", 0.9);
        item.invalidation = "   ".to_string();
        item.risk_flags = vec!["  ".to_string()];
        let hold = decision_to_review(
            &proposal("long"),
            "any",
            &item,
            0.45,
            true,
            false,
            0.8,
        )
        .unwrap();
        assert_eq!(hold.action, ReviewAction::Hold);
        assert_eq!(hold.invalidation_text, None, "空白失效文本不应落库");
        assert_eq!(hold.risk_text, None, "空白风险文本不应落库");

        let mut rejected = decision("open_long", 0.2);
        rejected.invalidation = "\t ".to_string();
        rejected.risk_flags = vec![" ".to_string(), String::new()];
        let rejected = decision_to_review(
            &proposal("long"),
            "any",
            &rejected,
            0.45,
            true,
            false,
            0.8,
        )
        .unwrap();
        assert_eq!(rejected.action, ReviewAction::Reject);
        assert_eq!(
            rejected.invalidation_text, None,
            "拒单路径的空白失效文本不应落库"
        );
        assert_eq!(rejected.risk_text, None, "拒单路径的空白风险文本不应落库");

        let mut mixed = decision("hold", 0.9);
        mixed.invalidation = " 跌破 EMA ".to_string();
        mixed.risk_flags = vec![" ".to_string(), " 上方套牢 ".to_string(), String::new()];
        let mixed = decision_to_review(
            &proposal("long"),
            "any",
            &mixed,
            0.45,
            true,
            false,
            0.8,
        )
        .unwrap();
        assert_eq!(
            mixed.invalidation_text.as_deref(),
            Some("跌破 EMA"),
            "有效文本应去掉首尾空白"
        );
        assert_eq!(
            mixed.risk_text.as_deref(),
            Some("上方套牢"),
            "风险项应过滤空白后拼接"
        );
    }

    #[test]
    fn long_side_lock_rejects_opposite_ai_entry() {
        let result = decision_to_review(
            &proposal("short"),
            "long",
            &decision("open_short", 0.8),
            0.45,
            true,
            false,
            0.8,
        )
        .unwrap();
        assert_eq!(result.action, ReviewAction::Reject);
        assert!(result.reason.contains("只允许做多"));
    }

    #[test]
    fn short_side_lock_rejects_opposite_ai_entry() {
        let result = decision_to_review(
            &proposal("long"),
            "short",
            &decision("open_long", 0.8),
            0.45,
            true,
            false,
            0.8,
        )
        .unwrap();
        assert_eq!(result.action, ReviewAction::Reject);
        assert!(result.reason.contains("只允许做空"));
    }

    #[test]
    fn matching_ai_entry_is_approved() {
        let result =
            decision_to_review(
                &proposal("long"),
                "long",
                &decision("open_long", 0.8),
                0.45,
                true,
                false,
                0.8,
            )
            .unwrap();
        assert_eq!(result.action, ReviewAction::Agree);
    }

    #[test]
    fn low_confidence_is_rejected_and_hold_keeps_ai_confidence() {
        let low = decision_to_review(
            &proposal("long"),
            "any",
            &decision("open_long", 0.2),
            0.45,
            true,
            false,
            0.8,
        )
        .unwrap();
        assert_eq!(low.action, ReviewAction::Reject);
        assert!((low.confidence - 0.2).abs() < 1e-9);

        let hold = decision_to_review(
            &proposal("long"),
            "any",
            &decision("hold", 0.9),
            0.45,
            true,
            false,
            0.8,
        )
        .unwrap();
        assert_eq!(hold.action, ReviewAction::Hold);
        assert!((hold.confidence - 0.9).abs() < 1e-9);
        assert_eq!(hold.reason, "test");
    }

    #[test]
    fn low_data_quality_is_rejected() {
        let mut item = decision("open_long", 0.9);
        item.data_quality = Some(0.2);
        let result = decision_to_review(&proposal("long"), "long", &item, 0.45, true, true, 0.8).unwrap();
        assert_eq!(result.action, ReviewAction::Reject);
        assert!(result.reason.contains("数据质量"));
        assert_eq!(
            result.data_quality,
            Some(0.2),
            "门槛拒单必须保留 AI 自评数据质量，供审计落 ai_dq"
        );
        assert!(
            (result.confidence - 0.9).abs() < 1e-9,
            "门槛拒单必须保留 AI 置信度"
        );
    }

    #[test]
    fn tighten_gate_blocks_low_quality_open() {
        // 数据质量门槛必须在校验 tighten 之前生效，否则低质量开仓会被当作可执行计划。
        let mut item = decision("open_long", 0.9);
        item.data_quality = Some(0.2);
        let result = decision_to_review(&proposal("long"), "long", &item, 0.45, true, true, 0.8)
            .unwrap();
        assert_eq!(result.action, ReviewAction::Reject);
        assert!(result.reason.contains("数据质量"));
    }

    #[test]
    fn low_rr_target_is_rejected() {
        let mut item = decision("open_long", 0.9);
        item.rr_target = Some(0.4);
        let result = decision_to_review(&proposal("long"), "long", &item, 0.45, true, true, 0.8).unwrap();
        assert_eq!(result.action, ReviewAction::Reject);
        assert!(result.reason.contains("盈亏比"));
        assert_eq!(
            result.rr_target,
            Some(0.4),
            "门槛拒单必须保留 AI 自估盈亏比，供审计落 ai_rr"
        );
        assert!(
            (result.confidence - 0.9).abs() < 1e-9,
            "门槛拒单必须保留 AI 置信度"
        );
    }

    #[test]
    fn thresholds_at_the_boundary_are_accepted() {
        let mut dq_equal = decision("open_long", 0.9);
        dq_equal.data_quality = Some(0.45);
        let result = decision_to_review(
            &proposal("long"),
            "long",
            &dq_equal,
            0.45,
            true,
            true,
            0.8,
        )
        .unwrap();
        assert_eq!(result.action, ReviewAction::Agree, "dq 等于门槛应放行");

        let mut rr_equal = decision("open_long", 0.9);
        rr_equal.rr_target = Some(0.8);
        let result = decision_to_review(
            &proposal("long"),
            "long",
            &rr_equal,
            0.45,
            true,
            true,
            0.8,
        )
        .unwrap();
        assert_eq!(result.action, ReviewAction::Agree, "rr 等于门槛应放行");
    }

    #[test]
    fn missing_quality_and_rr_do_not_reject() {
        let result = decision_to_review(
            &proposal("long"),
            "long",
            &decision("open_long", 0.9),
            0.45,
            true,
            true,
            0.8,
        )
        .unwrap();
        assert_eq!(result.action, ReviewAction::Agree, "dq/rr 缺省时不做门槛");
    }

    #[test]
    fn data_quality_slightly_below_threshold_is_rejected() {
        let mut item = decision("open_long", 0.9);
        item.data_quality = Some(0.449);
        let result = decision_to_review(
            &proposal("long"),
            "long",
            &item,
            0.45,
            true,
            true,
            0.8,
        )
        .unwrap();
        assert_eq!(result.action, ReviewAction::Reject);
        assert!(result.reason.contains("数据质量"));
    }

    #[test]
    fn low_data_quality_does_not_turn_hold_or_close_into_reject() {
        for action in ["hold", "close"] {
            let mut item = decision(action, 0.9);
            item.data_quality = Some(0.1);
            item.rr_target = Some(0.1);
            let result = decision_to_review(
                &proposal("long"),
                "long",
                &item,
                0.45,
                true,
                true,
                0.8,
            )
            .unwrap();
            assert_eq!(
                result.action,
                ReviewAction::Hold,
                "{action} 不应因门槛被改为 Reject"
            );
        }
    }

    const MASTER_KEY: &str = "MDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDA=";

    fn fixture(path: &std::path::Path) -> crate::db::Db {
        let _ = std::fs::remove_file(path);
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(
            r#"CREATE TABLE app_settings(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER);
               CREATE TABLE engine_state(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER);
CREATE TABLE ai_call_logs(id INTEGER PRIMARY KEY, ts INTEGER, provider TEXT, model TEXT, purpose TEXT,
                  ok INTEGER, latency_ms INTEGER, prompt_tokens INTEGER, completion_tokens INTEGER, cost_usd REAL, error TEXT, raw_text TEXT);
               CREATE TABLE engine_events(id INTEGER PRIMARY KEY, ts INTEGER, level TEXT, category TEXT, message TEXT, payload TEXT);
               CREATE TABLE ai_providers(id INTEGER PRIMARY KEY, name TEXT, protocol TEXT, base_url TEXT,
                 api_key_enc TEXT, model TEXT, temperature REAL, max_tokens INTEGER, reasoning_effort TEXT,
                 timeout_sec INTEGER, json_mode INTEGER, enabled INTEGER, priority INTEGER, role TEXT,
                 extra TEXT, created_at INTEGER, updated_at INTEGER);
               CREATE TABLE strategy_profiles(id INTEGER PRIMARY KEY, name TEXT UNIQUE, description TEXT, rule TEXT,
                 enabled INTEGER, priority INTEGER, filters TEXT, created_at INTEGER, updated_at INTEGER);
               CREATE TABLE mantras(id INTEGER PRIMARY KEY, title TEXT, rule TEXT, tags TEXT, category TEXT,
                 source TEXT, confidence REAL, health REAL, hits INTEGER, wins INTEGER, losses INTEGER,
                 enabled INTEGER, visible INTEGER, version INTEGER, superseded_by INTEGER, evidence TEXT,
                 created_at INTEGER, updated_at INTEGER, expires_at INTEGER);
               CREATE TABLE experiences(id INTEGER PRIMARY KEY, symbol TEXT, tags TEXT, scenario TEXT, lesson TEXT,
                 outcome TEXT, source_decision_id INTEGER, source TEXT, hits INTEGER, wins INTEGER, losses INTEGER,
                 enabled INTEGER, created_at INTEGER, updated_at INTEGER);
               INSERT INTO app_settings VALUES('ai','{"enabled":true,"mode":"single","provider_id":1,"fallback_provider_ids":[],"min_confidence":0.45,"max_calls_per_hour":30,"monthly_budget_usd":3,"temperature":0.2,"timeout_sec":5,"decision_schema_version":2}',0);
               INSERT INTO app_settings VALUES('risk','{"max_leverage":20}',0);
               INSERT INTO app_settings VALUES('strategy','{"timeframe":"15m"}',0);
               INSERT INTO strategy_profiles VALUES(1,'突破追多','','放量突破后等回踩再进',1,10,'{"symbols":["BTCUSDT"],"side":"long"}',1,1);
               INSERT INTO ai_providers VALUES(1,'alpha','openai_chat','https://api.example.com/v1','','deepseek-v4',0.2,1024,'auto',5,1,1,10,'primary','{}',1,1);"#,
        )
        .unwrap();
        drop(conn);
        let db = crate::db::Db::open(path.to_str().unwrap()).unwrap();
        let cipher = crate::secret::encrypt_secret(MASTER_KEY, "sk-test").unwrap();
        db.conn
            .lock()
            .unwrap()
            .execute("UPDATE ai_providers SET api_key_enc=?1", [&cipher])
            .unwrap();
        db
    }

    struct StaticMarket;

    impl crate::ai_market::AiMarketSource for StaticMarket {
        fn snapshot<'a>(
            &'a self,
            symbol: &'a str,
            _interval: &'a str,
        ) -> crate::ai_client::BoxFuture<'a, Result<crate::providers::TradingSnapshot, String>>
        {
            let candles: Vec<crate::providers::CandleSnapshot> = (0..120)
                .map(|index| crate::providers::CandleSnapshot {
                    close_time: 1_700_000_000 + index as i64 * 60,
                    open: 100.0 + index as f64 * 0.1,
                    high: 101.0 + index as f64 * 0.1,
                    low: 99.0 + index as f64 * 0.1,
                    close: 100.0 + index as f64 * 0.1,
                    volume: 10.0 + index as f64,
                })
                .collect();
            let snapshot = crate::providers::TradingSnapshot {
                markets: vec![crate::providers::MarketSnapshot {
                    symbol: symbol.to_uppercase(),
                    mark_price: candles.last().unwrap().close,
                    timestamp: 1_700_000_000,
                    quote_volume: 5_000_000.0,
                    funding_rate: 0.0001,
                    open_interest: 1000.0,
                    open_interest_change_pct: 1.5,
                    bid: 100.0,
                    ask: 100.01,
                    spread_bps: 1.0,
                    candles,
                }],
                account: crate::providers::AccountSnapshot {
                    equity: 1000.0,
                    wallet: 1000.0,
                    available_balance: 900.0,
                },
                positions: Vec::new(),
                open_orders: Vec::new(),
            };
            Box::pin(async move { Ok(snapshot) })
        }
    }

    struct FakeTransport;

    impl crate::ai_client::AiTransport for FakeTransport {
        fn post_json<'a>(
            &'a self,
            _url: &'a str,
            _headers: &'a [(String, String)],
            _body: &'a Value,
            _timeout_secs: u64,
        ) -> crate::ai_client::BoxFuture<'a, Result<crate::ai_client::HttpReply, String>> {
            Box::pin(async move {
                Ok(crate::ai_client::HttpReply {
                    status: 200,
                    body: r#"{"choices":[{"message":{"content":"{\"a\":\"L\",\"c\":0.8,\"l\":0,\"s\":0,\"t\":0,\"r\":[1],\"k\":[9],\"x\":2}"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#.to_string(),
                })
            })
        }

        fn get_json<'a>(
            &'a self,
            _url: &'a str,
            _headers: &'a [(String, String)],
            _timeout_secs: u64,
        ) -> crate::ai_client::BoxFuture<'a, Result<crate::ai_client::HttpReply, String>> {
            Box::pin(async move {
                Ok(crate::ai_client::HttpReply {
                    status: 404,
                    body: "not found".to_string(),
                })
            })
        }
    }

    struct HoldParamTransport;

    impl crate::ai_client::AiTransport for HoldParamTransport {
        fn post_json<'a>(
            &'a self,
            _url: &'a str,
            _headers: &'a [(String, String)],
            _body: &'a Value,
            _timeout_secs: u64,
        ) -> crate::ai_client::BoxFuture<'a, Result<crate::ai_client::HttpReply, String>> {
            Box::pin(async move {
                Ok(crate::ai_client::HttpReply {
                    status: 200,
                    body: r#"{"choices":[{"message":{"content":"{\"a\":\"H\",\"c\":0.9,\"p\":[{\"g\":\"strategy\",\"k\":\"atr_pct_min\",\"v\":0.7,\"w\":\"x\"}]}"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#.to_string(),
                })
            })
        }

        fn get_json<'a>(
            &'a self,
            _url: &'a str,
            _headers: &'a [(String, String)],
            _timeout_secs: u64,
        ) -> crate::ai_client::BoxFuture<'a, Result<crate::ai_client::HttpReply, String>> {
            Box::pin(async move {
                Ok(crate::ai_client::HttpReply {
                    status: 404,
                    body: "not found".to_string(),
                })
            })
        }
    }

    fn stored_strategy(db: &crate::db::Db) -> Value {
        let raw: String = db
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT value FROM app_settings WHERE key='strategy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    fn parameter_change_count(db: &crate::db::Db) -> i64 {
        db.conn
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM parameter_changes", [], |row| {
                row.get(0)
            })
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn review_entry_ignores_ai_param_updates() {
        let path = std::env::temp_dir().join("nq-ai-reviewer-ignores-params.db");
        let db = fixture(&path);
        {
            let conn = db.conn.lock().unwrap();
            crate::schema::ensure_parameter_changes(&conn).unwrap();
            conn.execute(
                "INSERT INTO parameter_changes(ts, group_name, key, old_value, new_value, reason, decision_id, mode, source) \
                 VALUES(1,'strategy','entry_threshold','null','0.3','seed',0,'demo','manual')",
                [],
            )
            .unwrap();
        }
        let before = parameter_change_count(&db);
        let state = AiState::with_transport(
            db.clone(),
            MASTER_KEY.as_bytes().to_vec(),
            std::sync::Arc::new(HoldParamTransport),
        )
        .with_market(std::sync::Arc::new(StaticMarket));
        let reviewed = EngineAiReviewer::new(state)
            .review(&proposal("long"))
            .unwrap();
        assert_eq!(reviewed.action, ReviewAction::Hold);
        assert!((reviewed.confidence - 0.9).abs() < 1e-9);
        assert_eq!(
            parameter_change_count(&db),
            before,
            "开仓复核不得写入 parameter_changes"
        );
        let strategy = stored_strategy(&db);
        assert!(
            strategy.get("atr_pct_min").is_none(),
            "开仓复核不得改写 strategy 参数: {strategy}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn production_reviewer_runs_outside_a_transaction() {
        let path = std::env::temp_dir().join("nq-ai-reviewer.db");
        let db = fixture(&path);
        let state = AiState::with_transport(
            db.clone(),
            MASTER_KEY.as_bytes().to_vec(),
            std::sync::Arc::new(FakeTransport),
        )
        .with_market(std::sync::Arc::new(StaticMarket));
        let reviewer = EngineAiReviewer::new(state);
        let reviewed = reviewer.review(&proposal("long")).unwrap();
        assert_eq!(reviewed.action, ReviewAction::Agree);
        let applied: String = db
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT value FROM engine_state WHERE key='strategy_applied'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(applied, "突破追多");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn ai_tightening_keeps_side_semantics_for_long_and_short() {
        let long = ai_client::Decision {
            action: "open_long".to_string(),
            confidence: 0.9,
            leverage: 4.0,
            margin_pct: None,
            sl_pct: 8.0,
            tp_pct: 0.0,
            reason: "tighten".to_string(),
            risk_flags: Vec::new(),
            invalidation: String::new(),
            params: Vec::new(),
            invalidation_price: None,
            valid_bars: None,
            data_quality: None,
            rr_target: None,
            entry_price: None,
        };
        let reviewed =
            decision_to_review(&proposal("long"), "any", &long, 0.45, true, false, 0.8).unwrap();
        assert_eq!(reviewed.action, ReviewAction::Tighten);
        assert_eq!(reviewed.leverage, Some(4.0));
        let long_stop = reviewed.stop_price.expect("多单应收紧止损");
        assert!(
            (long_stop - 98.0).abs() < 1e-9,
            "多单止损必须上移到 98，实际 {long_stop}"
        );
        assert!(long_stop > 95.0, "收紧后的多单止损不能低于原止损");

        let short = ai_client::Decision {
            action: "open_short".to_string(),
            confidence: 0.9,
            leverage: 3.0,
            margin_pct: None,
            sl_pct: 6.0,
            tp_pct: 0.0,
            reason: "tighten".to_string(),
            risk_flags: Vec::new(),
            invalidation: String::new(),
            params: Vec::new(),
            invalidation_price: None,
            valid_bars: None,
            data_quality: None,
            rr_target: None,
            entry_price: None,
        };
        let reviewed =
            decision_to_review(&proposal("short"), "any", &short, 0.45, true, false, 0.8).unwrap();
        assert_eq!(reviewed.leverage, Some(3.0));
        let short_stop = reviewed.stop_price.expect("空单应收紧止损");
        assert!(
            (short_stop - 102.0).abs() < 1e-9,
            "空单止损必须下移到 102，实际 {short_stop}"
        );
        assert!(short_stop < 105.0, "收紧后的空单止损不能高于原止损");
    }

    #[test]
    fn ai_leverage_is_clamped_to_the_global_bounds() {
        let raised = ai_client::Decision {
            action: "open_long".to_string(),
            confidence: 0.9,
            leverage: 12.0,
            margin_pct: None,
            sl_pct: 0.0,
            tp_pct: 0.0,
            reason: "波动收敛，区间内加杠杆".to_string(),
            risk_flags: Vec::new(),
            invalidation: String::new(),
            params: Vec::new(),
            invalidation_price: None,
            valid_bars: None,
            data_quality: None,
            rr_target: None,
            entry_price: None,
        };
        let reviewed =
            decision_to_review(&proposal("long"), "any", &raised, 0.45, true, false, 0.8).unwrap();
        assert_eq!(reviewed.leverage, Some(12.0), "区间内允许上调杠杆");

        let too_high = ai_client::Decision {
            leverage: 80.0,
            ..raised.clone()
        };
        let reviewed =
            decision_to_review(&proposal("long"), "any", &too_high, 0.45, true, false, 0.8).unwrap();
        assert_eq!(reviewed.leverage, Some(20.0), "超过上限压回上限");

        let too_low = ai_client::Decision {
            leverage: 1.0,
            ..raised
        };
        let reviewed =
            decision_to_review(&proposal("long"), "any", &too_low, 0.45, true, false, 0.8).unwrap();
        assert_eq!(reviewed.leverage, Some(2.0), "低于下限抬回下限");
    }

    #[test]
    fn ai_stop_is_used_for_an_agree_decision() {
        let wider = ai_client::Decision {
            action: "open_long".to_string(),
            confidence: 0.9,
            leverage: 0.0,
            margin_pct: None,
            sl_pct: 20.0,
            tp_pct: 0.0,
            reason: "wider".to_string(),
            risk_flags: Vec::new(),
            invalidation: String::new(),
            params: Vec::new(),
            invalidation_price: None,
            valid_bars: None,
            data_quality: None,
            rr_target: None,
            entry_price: None,
        };
        let reviewed =
            decision_to_review(&proposal("long"), "any", &wider, 0.45, true, false, 0.8).unwrap();
        assert_eq!(reviewed.action, ReviewAction::Agree);
        assert_eq!(reviewed.leverage, None);
        assert_eq!(reviewed.stop_price, Some(90.0));

        struct Fixed(crate::review::ReviewDecision);
        impl crate::review::AiReviewer for Fixed {
            fn review(
                &self,
                _proposal: &crate::review::EntryProposal,
            ) -> Result<crate::review::ReviewDecision, String> {
                Ok(self.0.clone())
            }
        }
        let clamped =
            crate::review::apply_review(&proposal("long"), Some(&Fixed(reviewed)), true).unwrap();
        assert_eq!(clamped.leverage, 2.0, "AI 未给杠杆时取全局下限");
        assert_eq!(clamped.stop_price, 90.0, "AI 给出的止损价直接生效");
    }

    #[test]
    fn ai_side_can_override_the_quant_signal() {
        let blocked = decision_to_review(
            &proposal("long"),
            "any",
            &decision("open_short", 0.9),
            0.45,
            true,
            false,
            0.8,
        )
        .unwrap();
        assert_eq!(blocked.action, ReviewAction::Reject, "默认必须与量化方向一致");

        let conflict = decision_to_review(
            &proposal("long"),
            "any",
            &decision("open_short", 0.9),
            0.45,
            false,
            true,
            0.8,
        )
        .unwrap();
        assert_eq!(conflict.action, ReviewAction::Tighten);
        assert_eq!(conflict.side.as_deref(), Some("short"));
    }

    fn proposal_without_quant(side: &str) -> EntryProposal {
        let mut item = proposal(side);
        item.quant_side = None;
        item.quant_score = None;
        item
    }

    #[test]
    fn require_agreement_blocks_ai_against_quant_side() {
        let result = decision_to_review(
            &proposal("long"),
            "any",
            &decision("open_short", 0.9),
            0.45,
            true,
            true,
            0.8,
        )
        .unwrap();
        assert_eq!(result.action, ReviewAction::Reject);
        assert!(result.reason.contains("AI 与量化方向不一致"));

        let agree = decision_to_review(
            &proposal("long"),
            "any",
            &decision("open_long", 0.9),
            0.45,
            true,
            false,
            0.8,
        )
        .unwrap();
        assert_eq!(agree.action, ReviewAction::Agree);
    }

    #[test]
    fn allow_ai_override_false_blocks_ai_against_quant_side() {
        let result = decision_to_review(
            &proposal("long"),
            "any",
            &decision("open_short", 0.9),
            0.45,
            false,
            false,
            0.8,
        )
        .unwrap();
        assert_eq!(result.action, ReviewAction::Reject);
        assert!(result.reason.contains("未允许 AI 覆盖"));
    }

    #[test]
    fn allow_ai_override_true_permits_a_side_flip() {
        let result = decision_to_review(
            &proposal("short"),
            "any",
            &decision("open_long", 0.9),
            0.45,
            false,
            true,
            0.8,
        )
        .unwrap();
        assert_eq!(result.action, ReviewAction::Tighten);
        assert_eq!(result.side.as_deref(), Some("long"));
    }

    #[test]
    fn require_agreement_without_quant_side_rejects_preview_entry() {
        let result = decision_to_review(
            &proposal_without_quant("long"),
            "any",
            &decision("open_long", 0.9),
            0.45,
            true,
            false,
            0.8,
        )
        .unwrap();
        assert_eq!(result.action, ReviewAction::Reject);
        assert!(result.reason.contains("缺少量化方向"));

        let ai_first = decision_to_review(
            &proposal_without_quant("long"),
            "any",
            &decision("open_long", 0.9),
            0.45,
            false,
            false,
            0.8,
        )
        .unwrap();
        assert_eq!(ai_first.action, ReviewAction::Agree, "无量化方向时 AI 仍是第一决策者");
    }

    #[test]
    fn hold_is_held_regardless_of_agreement_flags() {
        for (require_agreement, allow_ai_override) in [(true, false), (false, true)] {
            let result = decision_to_review(
                &proposal("long"),
                "any",
                &decision("hold", 0.9),
                0.45,
                require_agreement,
                allow_ai_override,
                0.8,
            )
            .unwrap();
            assert_eq!(result.action, ReviewAction::Hold);
            assert_eq!(result.side, None);
        }
    }

    #[test]
    fn confidence_equal_to_threshold_is_accepted() {
        let result = decision_to_review(
            &proposal("long"),
            "any",
            &decision("open_long", 0.45),
            0.45,
            true,
            false,
            0.8,
        )
        .unwrap();
        assert_eq!(
            result.action,
            ReviewAction::Agree,
            "置信度等于门槛时应放行，而不是被 < 误杀"
        );
    }

    struct CapturingTransport {
        bodies: std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
    }

    impl crate::ai_client::AiTransport for CapturingTransport {
        fn post_json<'a>(
            &'a self,
            _url: &'a str,
            _headers: &'a [(String, String)],
            body: &'a Value,
            _timeout_secs: u64,
        ) -> crate::ai_client::BoxFuture<'a, Result<crate::ai_client::HttpReply, String>> {
            let bodies = self.bodies.clone();
            let body = body.clone();
            Box::pin(async move {
                bodies.lock().unwrap().push(body);
                Ok(crate::ai_client::HttpReply {
                    status: 200,
                    body: r#"{"choices":[{"message":{"content":"{\"a\":\"L\",\"c\":0.8,\"l\":0,\"s\":0,\"t\":0,\"r\":[1],\"k\":[9],\"x\":2}"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#.to_string(),
                })
            })
        }

        fn get_json<'a>(
            &'a self,
            _url: &'a str,
            _headers: &'a [(String, String)],
            _timeout_secs: u64,
        ) -> crate::ai_client::BoxFuture<'a, Result<crate::ai_client::HttpReply, String>> {
            Box::pin(async move {
                Ok(crate::ai_client::HttpReply {
                    status: 404,
                    body: "not found".to_string(),
                })
            })
        }
    }

    async fn review_context_with(inject: bool, tag: &str) -> Value {
        let path = std::env::temp_dir().join(format!("nq-ai-review-inject-{tag}.db"));
        let db = fixture(&path);
        let ai_value = json!({
            "enabled": true,
            "mode": "single",
            "provider_id": 1,
            "fallback_provider_ids": [],
            "min_confidence": 0.45,
            "max_calls_per_hour": 30,
            "monthly_budget_usd": 3,
            "temperature": 0.2,
            "timeout_sec": 5,
            "decision_schema_version": 2,
            "inject_news": inject,
            "inject_mantras": inject,
        });
        db.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE app_settings SET value=?1 WHERE key='ai'",
                [serde_json::to_string(&ai_value).unwrap()],
            )
            .unwrap();
        let bodies = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let state = AiState::with_transport(
            db.clone(),
            MASTER_KEY.as_bytes().to_vec(),
            std::sync::Arc::new(CapturingTransport {
                bodies: bodies.clone(),
            }),
        )
        .with_market(std::sync::Arc::new(StaticMarket));
        let reviewed = EngineAiReviewer::new(state)
            .review(&proposal("long"))
            .unwrap();
        assert_eq!(reviewed.action, ReviewAction::Agree);
        let _ = std::fs::remove_file(&path);
        let body = bodies.lock().unwrap().pop().expect("已捕获复核请求");
        let user = body["messages"]
            .as_array()
            .and_then(|items| items.iter().find(|item| item["role"] == "user"))
            .expect("复核请求缺少 user 消息");
        serde_json::from_str(user["content"].as_str().expect("user content 不是字符串")).unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn inject_news_and_inject_mantras_gate_review_prompt() {
        let off = review_context_with(false, "off").await;
        assert!(off.get("news").is_none(), "关闭时复核 prompt 不含 news: {off}");
        assert!(
            off.get("mantras").is_none(),
            "关闭时复核 prompt 不含 mantras: {off}"
        );

        let on = review_context_with(true, "on").await;
        assert!(on.get("news").is_some(), "打开时复核 prompt 含 news");
        assert!(on.get("mantras").is_some(), "打开时复核 prompt 含 mantras");
    }

    fn reviewed_with_new_fields() -> ReviewDecision {
        let mut reviewed = reject("测试用复核结果");
        reviewed.invalidation_price = Some(97.0);
        reviewed.entry_price = Some(101.0);
        reviewed
    }

    #[test]
    fn valid_bars_falls_back_to_strategy_config() {
        let reviewed = fill_default_valid_bars(reviewed_with_new_fields(), &json!({"valid_bars": 10}));
        assert_eq!(reviewed.valid_bars, Some(10));
    }

    #[test]
    fn valid_bars_keeps_ai_value_when_present() {
        let mut reviewed = reviewed_with_new_fields();
        reviewed.valid_bars = Some(4);
        let reviewed = fill_default_valid_bars(reviewed, &json!({"valid_bars": 10}));
        assert_eq!(reviewed.valid_bars, Some(4));
    }

    #[test]
    fn valid_bars_defaults_when_config_missing() {
        let reviewed = fill_default_valid_bars(reviewed_with_new_fields(), &json!({}));
        assert_eq!(reviewed.valid_bars, Some(6));
        // 配置越界值夹到 1..=24。
        for (input, expected) in [(0, 1), (-3, 1), (99, 24)] {
            let reviewed =
                fill_default_valid_bars(reviewed_with_new_fields(), &json!({"valid_bars": input}));
            assert_eq!(reviewed.valid_bars, Some(expected), "valid_bars={input}");
        }
    }

    #[test]
    fn switches_do_not_clear_audit_meta() {
        // 开关只在消费点生效，复核出口必须保留 AI 失效价/入场价供审计落库。
        let reviewed = fill_default_valid_bars(reviewed_with_new_fields(), &json!({}));
        assert_eq!(reviewed.invalidation_price, Some(97.0));
        assert_eq!(reviewed.entry_price, Some(101.0));
    }
}
