use serde_json::{json, Value};

use crate::ai_client::{self, ChatCall};
use crate::ai_providers::{provider_chain, AiState};
use crate::review::{AiLatency, AiReviewer, EntryProposal, ReviewAction, ReviewDecision};

pub(crate) struct EngineAiReviewer {
    state: AiState,
    /// 执行复核的 Tokio 句柄。并发复核跑在普通线程上时，线程内拿不到
    /// `Handle::try_current()`，必须预先保存句柄才能驱动异步复核。
    handle: Option<tokio::runtime::Handle>,
}

impl EngineAiReviewer {
    pub(crate) fn new(state: AiState, handle: Option<tokio::runtime::Handle>) -> Self {
        Self { state, handle }
    }
}

impl AiReviewer for EngineAiReviewer {
    fn review(&self, proposal: &EntryProposal) -> Result<ReviewDecision, String> {
        let handle = self
            .handle
            .clone()
            .or_else(|| tokio::runtime::Handle::try_current().ok())
            .ok_or_else(|| "AI 复核需要 Tokio 运行时".to_string())?;
        let state = self.state.clone();
        let conn = state.db.conn.clone();
        let proposal = proposal.clone();
        let run = move || {
            wait_for_lock(&conn, std::time::Duration::from_millis(2000))?;
            let timeout = review_timeout(&state);
            handle.block_on(review_entry_with_timeout(state, proposal, timeout))
        };
        // 当前线程已在运行时里时必须 block_in_place，否则 block_on 会 panic；
        // 普通线程（并发复核的子线程）直接 block_on 即可。
        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::task::block_in_place(run)
        } else {
            run()
        }
    }
}

/// 复核超时独立于 HTTP 请求超时（`ai.timeout_sec`）：`ai.review_timeout_sec`
/// 控制整段复核（取行情 + 模型调用）的截止时间，缺省 30 秒。
fn review_timeout(state: &AiState) -> std::time::Duration {
    let value = match state.db.setting("ai") {
        Ok(value) => value.unwrap_or(Value::Null),
        Err(error) => {
            tracing::debug!(error = ?error, "读取 ai 配置失败，复核超时回退默认值");
            Value::Null
        }
    };
    // UI 建议范围是 5–120 秒；读取侧放宽到 1 秒下限，便于老配置与测试快速收敛。
    let secs = value
        .get("review_timeout_sec")
        .and_then(Value::as_u64)
        .unwrap_or(30)
        .clamp(1, 120);
    std::time::Duration::from_secs(secs)
}

async fn review_entry_with_timeout(
    state: AiState,
    proposal: EntryProposal,
    timeout: std::time::Duration,
) -> Result<ReviewDecision, String> {
    tokio::time::timeout(timeout, review_entry(state, proposal))
        .await
        .map_err(|_| "AI 复核超时".to_string())?
}

/// 等待数据库连接锁释放，避免后台任务短暂持锁被误判为事务内调用。
/// 仅在超时后返回 Err，调用方据此按 fail-closed 跳过本次复核。
fn wait_for_lock<T>(
    lock: &std::sync::Arc<std::sync::Mutex<T>>,
    timeout: std::time::Duration,
) -> Result<(), String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match lock.try_lock() {
            Ok(guard) => {
                drop(guard);
                return Ok(());
            }
            Err(_) => {
                if std::time::Instant::now() >= deadline {
                    return Err("数据库连接繁忙，AI 复核暂时跳过".to_string());
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
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
    // 瘦身档位只压缩注入上下文，不改 prompt 文本与输出上限。
    let slim = cfg.prompt_profile == "slim";
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
        let mut evolution = crate::evolution_api::evolution_cfg(&conn);
        if slim {
            evolution.max_injected = evolution.max_injected.min(3);
        }
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
        let mut experiences = crate::journal::inject_lessons(
            &conn,
            &proposal.symbol,
            &journal_snapshot,
            &proposal.side,
        );
        if slim {
            experiences.truncate(3);
        }
        (mantra_text, mantra_ids, experiences)
    };
    let news = if cfg.inject_news {
        crate::ai_analysis::news_digest(&state, if slim { 4 } else { 8 })
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
        "klines": crate::ai_analysis::recent_bars(&market.candles, if slim { 8 } else { 14 }),
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
    let messages = crate::ai_analysis::decision_messages_for_mode(&context, &cfg.personas);
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
    let parse_started = std::time::Instant::now();
    let decision = ai_client::parse_decision(&outcome.text, Some(&context))
        .map_err(|error| format!("AI 复核结果无效: {error}"))?;
    let parse_ms = parse_started.elapsed().as_millis().min(i64::MAX as u128) as i64;
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
    let mut reviewed = decision_to_review(
        &proposal,
        side_lock,
        &decision,
        cfg.min_confidence,
        min_entry_rr,
        cfg.scale_on_confidence,
    )?;
    reviewed.latency = AiLatency {
        provider_ms: Some(outcome.latency_ms),
        parse_ms: Some(parse_ms),
        wall_ms: None,
    };
    reviewed.prompt_profile = Some(cfg.prompt_profile.clone());
    Ok(fill_default_valid_bars(reviewed, &strategy_cfg))
}

/// AI 未给有效期时回退 strategy.valid_bars，未配置则默认 6 根；越界夹到 1..=24。
fn fill_default_valid_bars(mut reviewed: ReviewDecision, strategy_cfg: &Value) -> ReviewDecision {
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

        reason: reason.into(),
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
    min_entry_rr: f64,
    scale_on_confidence: bool,
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
    let scale = if scale_on_confidence {
        Some((0.5 + 0.5 * decision.confidence).clamp(0.5, 1.0))
    } else {
        None
    };
    let side = match action {
        "open_long" => "long",
        "open_short" => "short",
        _ => {
            let (invalidation_text, risk_text) = text_fields(decision);
            return Ok(ReviewDecision {
                action: ReviewAction::Hold,

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
                scale: None,
                latency: AiLatency::default(),
                prompt_profile: None,
            });
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
    // AI 固定为否决模式：只能同意、否决或降仓，方向必须与量化一致。
    if side != proposal.side {
        return Ok(reject_with("AI 方向与量化相反", decision));
    }
    let (invalidation_text, risk_text) = text_fields(decision);
    Ok(ReviewDecision {
        action: ReviewAction::Agree,
        reason: if decision.reason.is_empty() {
            "AI 同意".to_string()
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
        scale,
        latency: AiLatency::default(),
        prompt_profile: None,
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
            margin_pct: 20.0,
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
        let hold = decision_to_review(&proposal("long"), "any", &item, 0.45, 0.8, false).unwrap();
        assert_eq!(hold.action, ReviewAction::Hold);
        assert_eq!(hold.invalidation_text, None, "空白失效文本不应落库");
        assert_eq!(hold.risk_text, None, "空白风险文本不应落库");

        let mut rejected = decision("open_long", 0.2);
        rejected.invalidation = "\t ".to_string();
        rejected.risk_flags = vec![" ".to_string(), String::new()];
        let rejected =
            decision_to_review(&proposal("long"), "any", &rejected, 0.45, 0.8, false).unwrap();
        assert_eq!(rejected.action, ReviewAction::Reject);
        assert_eq!(
            rejected.invalidation_text, None,
            "拒单路径的空白失效文本不应落库"
        );
        assert_eq!(rejected.risk_text, None, "拒单路径的空白风险文本不应落库");

        let mut mixed = decision("hold", 0.9);
        mixed.invalidation = " 跌破 EMA ".to_string();
        mixed.risk_flags = vec![" ".to_string(), " 上方套牢 ".to_string(), String::new()];
        let mixed = decision_to_review(&proposal("long"), "any", &mixed, 0.45, 0.8, false).unwrap();
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
            0.8,
            false,
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
            0.8,
            false,
        )
        .unwrap();
        assert_eq!(result.action, ReviewAction::Reject);
        assert!(result.reason.contains("只允许做空"));
    }

    #[test]
    fn matching_ai_entry_is_approved() {
        let result = decision_to_review(
            &proposal("long"),
            "long",
            &decision("open_long", 0.8),
            0.45,
            0.8,
            false,
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
            0.8,
            false,
        )
        .unwrap();
        assert_eq!(low.action, ReviewAction::Reject);
        assert!((low.confidence - 0.2).abs() < 1e-9);

        let hold = decision_to_review(
            &proposal("long"),
            "any",
            &decision("hold", 0.9),
            0.45,
            0.8,
            false,
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
        let result =
            decision_to_review(&proposal("long"), "long", &item, 0.45, 0.8, false).unwrap();
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
        let result =
            decision_to_review(&proposal("long"), "long", &item, 0.45, 0.8, false).unwrap();
        assert_eq!(result.action, ReviewAction::Reject);
        assert!(result.reason.contains("数据质量"));
    }

    #[test]
    fn low_rr_target_is_rejected() {
        let mut item = decision("open_long", 0.9);
        item.rr_target = Some(0.4);
        let result =
            decision_to_review(&proposal("long"), "long", &item, 0.45, 0.8, false).unwrap();
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
        let result =
            decision_to_review(&proposal("long"), "long", &dq_equal, 0.45, 0.8, false).unwrap();
        assert_eq!(result.action, ReviewAction::Agree, "dq 等于门槛应放行");

        let mut rr_equal = decision("open_long", 0.9);
        rr_equal.rr_target = Some(0.8);
        let result =
            decision_to_review(&proposal("long"), "long", &rr_equal, 0.45, 0.8, false).unwrap();
        assert_eq!(result.action, ReviewAction::Agree, "rr 等于门槛应放行");
    }

    #[test]
    fn missing_quality_and_rr_do_not_reject() {
        let result = decision_to_review(
            &proposal("long"),
            "long",
            &decision("open_long", 0.9),
            0.45,
            0.8,
            false,
        )
        .unwrap();
        assert_eq!(result.action, ReviewAction::Agree, "dq/rr 缺省时不做门槛");
    }

    #[test]
    fn data_quality_slightly_below_threshold_is_rejected() {
        let mut item = decision("open_long", 0.9);
        item.data_quality = Some(0.449);
        let result =
            decision_to_review(&proposal("long"), "long", &item, 0.45, 0.8, false).unwrap();
        assert_eq!(result.action, ReviewAction::Reject);
        assert!(result.reason.contains("数据质量"));
    }

    #[test]
    fn low_data_quality_does_not_turn_hold_or_close_into_reject() {
        for action in ["hold", "close"] {
            let mut item = decision(action, 0.9);
            item.data_quality = Some(0.1);
            item.rr_target = Some(0.1);
            let result =
                decision_to_review(&proposal("long"), "long", &item, 0.45, 0.8, false).unwrap();
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
               CREATE TABLE news_items(id INTEGER PRIMARY KEY, title TEXT, source TEXT, sentiment REAL, ts INTEGER);
               INSERT INTO app_settings VALUES('ai','{"enabled":true,"mode":"single","provider_id":1,"fallback_provider_ids":[],"min_confidence":0.45,"max_calls_per_hour":30,"monthly_budget_usd":3,"temperature":0.2,"timeout_sec":5,"decision_schema_version":2}',0);
               INSERT INTO app_settings VALUES('risk','{"max_leverage":20}',0);
               INSERT INTO app_settings VALUES('strategy','{"timeframe":"15m"}',0);
               INSERT INTO strategy_profiles VALUES(1,'突破追多','','放量突破后等回踩再进',1,10,'{"symbols":["BTCUSDT"],"side":"long"}',1,1);
               INSERT INTO news_items(title, source, sentiment, ts) VALUES
                 ('a','s',0.1,1),('b','s',0.1,2),('c','s',0.1,3),('d','s',0.1,4),
                 ('e','s',0.1,5),('f','s',0.1,6),('g','s',0.1,7),('h','s',0.1,8);
               INSERT INTO experiences(symbol,tags,scenario,lesson,outcome,source,hits,wins,losses,enabled,created_at,updated_at) VALUES
                 ('BTCUSDT','["trend","dir:long"]','{"rsi":55,"adx":30,"atr_pct":1.2}','exp1','correct','review',1,1,0,1,1,1),
                 ('BTCUSDT','["trend","dir:long"]','{"rsi":55,"adx":30,"atr_pct":1.2}','exp2','correct','review',1,1,0,1,2,2),
                 ('BTCUSDT','["trend","dir:long"]','{"rsi":55,"adx":30,"atr_pct":1.2}','exp3','correct','review',1,1,0,1,3,3),
                 ('BTCUSDT','["trend","dir:long"]','{"rsi":55,"adx":30,"atr_pct":1.2}','exp4','correct','review',1,1,0,1,4,4);
               INSERT INTO mantras(id,title,rule,tags,category,source,confidence,health,hits,wins,losses,enabled,visible,version,superseded_by,evidence,created_at,updated_at,expires_at) VALUES
                 (1,'m1','r1','[]','c','review',0.9,0.9,0,0,0,1,1,1,0,'{}',1,1,0),
                 (2,'m2','r2','[]','c','review',0.9,0.9,0,0,0,1,1,1,0,'{}',2,2,0),
                 (3,'m3','r3','[]','c','review',0.9,0.9,0,0,0,1,1,1,0,'{}',3,3,0),
                 (4,'m4','r4','[]','c','review',0.9,0.9,0,0,0,1,1,1,0,'{}',4,4,0);
               INSERT INTO ai_providers VALUES(1,'alpha','openai_chat','https://api.example.com/v1','','deepseek-v4',0.2,1024,'auto',5,1,1,10,'primary','{}',1,1);"#,
        )
        .unwrap();
        drop(conn);
        let db = crate::db::Db::open(path.to_str().unwrap()).unwrap();
        let cipher = crate::secret::encrypt_secret(MASTER_KEY, "sk-test").unwrap();
        db.lock()
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

    /// 慢速模型：让复核链路在超时窗口内收不到回复。
    struct SlowTransport {
        delay: std::time::Duration,
    }

    impl crate::ai_client::AiTransport for SlowTransport {
        fn post_json<'a>(
            &'a self,
            _url: &'a str,
            _headers: &'a [(String, String)],
            _body: &'a Value,
            _timeout_secs: u64,
        ) -> crate::ai_client::BoxFuture<'a, Result<crate::ai_client::HttpReply, String>> {
            Box::pin(async move {
                tokio::time::sleep(self.delay).await;
                Ok(crate::ai_client::HttpReply {
                    status: 200,
                    body: r#"{"choices":[{"message":{"content":"{\"a\":\"L\",\"c\":0.8}"},"finish_reason":"stop"}]}"#.to_string(),
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
            .lock()
            .query_row(
                "SELECT value FROM app_settings WHERE key='strategy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    fn parameter_change_count(db: &crate::db::Db) -> i64 {
        db.lock()
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
            let conn = db.lock();
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
        let reviewed = EngineAiReviewer::new(state, None)
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
        let reviewer = EngineAiReviewer::new(state, None);
        let reviewed = reviewer.review(&proposal("long")).unwrap();
        assert_eq!(reviewed.action, ReviewAction::Agree);
        let applied: String = db
            .lock()
            .query_row(
                "SELECT value FROM engine_state WHERE key='strategy_applied'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(applied, "突破追多");
        let _ = std::fs::remove_file(&path);
    }

    /// 覆盖 ai 配置中的单个键，验证运行时读取路径。
    fn set_ai_setting(db: &crate::db::Db, key: &str, value: Value) {
        let mut ai: Value = {
            let conn = db.lock();
            let raw: String = conn
                .query_row("SELECT value FROM app_settings WHERE key='ai'", [], |row| {
                    row.get(0)
                })
                .unwrap();
            serde_json::from_str(&raw).unwrap()
        };
        ai[key] = value;
        db.lock()
            .execute(
                "UPDATE app_settings SET value=?1 WHERE key='ai'",
                [serde_json::to_string(&ai).unwrap()],
            )
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn review_timeout_uses_config() {
        let path = std::env::temp_dir().join("nq-ai-reviewer-timeout-config.db");
        let db = fixture(&path);
        set_ai_setting(&db, "review_timeout_sec", json!(1));
        let state = AiState::with_transport(
            db.clone(),
            MASTER_KEY.as_bytes().to_vec(),
            std::sync::Arc::new(SlowTransport {
                delay: std::time::Duration::from_millis(1500),
            }),
        )
        .with_market(std::sync::Arc::new(StaticMarket));
        let started = std::time::Instant::now();
        let error = EngineAiReviewer::new(state, None)
            .review(&proposal("long"))
            .expect_err("超过配置的复核超时必须失败");
        assert!(error.contains("超时"), "错误要说明超时: {error}");
        assert!(
            started.elapsed() < std::time::Duration::from_millis(1400),
            "必须按 ai.review_timeout_sec=1 返回，而不是等模型回复，实际 {:?}",
            started.elapsed()
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn ai_cannot_change_side_leverage_margin_stop_or_take_profit() {
        let mut ai = decision("open_long", 0.9);
        ai.leverage = 80.0;
        ai.margin_pct = Some(90.0);
        ai.sl_pct = 20.0;
        ai.tp_pct = 50.0;
        let reviewed = decision_to_review(&proposal("long"), "any", &ai, 0.45, 0.8, false).unwrap();
        assert_eq!(reviewed.action, ReviewAction::Agree);
        assert_eq!(reviewed.confidence, 0.9);

        struct Fixed(ReviewDecision);
        impl crate::review::AiReviewer for Fixed {
            fn review(
                &self,
                _proposal: &crate::review::EntryProposal,
            ) -> Result<ReviewDecision, String> {
                Ok(self.0.clone())
            }
        }
        let applied =
            crate::review::apply_review(&proposal("long"), Some(&Fixed(reviewed)), true).unwrap();
        assert_eq!(applied.side, "long", "AI 不得改变方向");
        assert_eq!(applied.leverage, 2.0, "AI 杠杆不得生效");

        assert_eq!(applied.stop_price, 95.0, "AI 止损不得生效");
        assert_eq!(applied.take_profit, 110.0, "AI 止盈不得生效");
    }

    #[test]
    fn ai_opposite_direction_is_rejected_regardless_of_confidence() {
        let result = decision_to_review(
            &proposal("long"),
            "any",
            &decision("open_short", 0.9),
            0.45,
            0.8,
            false,
        )
        .unwrap();
        assert_eq!(result.action, ReviewAction::Reject);
        assert!(result.reason.contains("AI 方向与量化相反"));
    }

    #[test]
    fn hold_and_close_are_held_without_entry_fields() {
        for action in ["hold", "close"] {
            let result = decision_to_review(
                &proposal("long"),
                "any",
                &decision(action, 0.9),
                0.45,
                0.8,
                false,
            )
            .unwrap();
            assert_eq!(result.action, ReviewAction::Hold, "{action} 必须转为观望");
            assert_eq!(result.scale, None);
        }
    }

    #[test]
    fn confidence_equal_to_threshold_is_accepted() {
        let result = decision_to_review(
            &proposal("long"),
            "any",
            &decision("open_long", 0.45),
            0.45,
            0.8,
            false,
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
        review_with_profile(inject, None, tag).await.1
    }

    async fn review_with_profile(
        inject: bool,
        profile: Option<&str>,
        tag: &str,
    ) -> (ReviewDecision, Value) {
        let path = std::env::temp_dir().join(format!("nq-ai-review-inject-{tag}.db"));
        let db = fixture(&path);
        let mut ai_value = json!({
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
        if let Some(profile) = profile {
            ai_value["prompt_profile"] = json!(profile);
        }
        db.lock()
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
        let reviewed = EngineAiReviewer::new(state, None)
            .review(&proposal("long"))
            .unwrap();
        assert_eq!(reviewed.action, ReviewAction::Agree);
        let _ = std::fs::remove_file(&path);
        let body = bodies.lock().unwrap().pop().expect("已捕获复核请求");
        let user = body["messages"]
            .as_array()
            .and_then(|items| items.iter().find(|item| item["role"] == "user"))
            .expect("复核请求缺少 user 消息");
        let context =
            serde_json::from_str(user["content"].as_str().expect("user content 不是字符串"))
                .unwrap();
        (reviewed, context)
    }

    fn mantra_lines(context: &Value) -> usize {
        context["mantras"]["text"]
            .as_str()
            .unwrap_or_default()
            .lines()
            .filter(|line| {
                line.chars()
                    .next()
                    .is_some_and(|first| first.is_ascii_digit())
            })
            .count()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slim_profile_limits_klines_news_experiences() {
        let (reviewed, context) = review_with_profile(true, Some("slim"), "slim").await;
        assert_eq!(reviewed.prompt_profile.as_deref(), Some("slim"));
        assert_eq!(
            context["klines"].as_array().unwrap().len(),
            8,
            "瘦身 K 线 8 根"
        );
        assert_eq!(
            context["news"].as_array().unwrap().len(),
            4,
            "瘦身资讯 4 条"
        );
        assert_eq!(
            context["experiences"].as_array().unwrap().len(),
            3,
            "瘦身经验只取前 3 条"
        );
        assert_eq!(mantra_lines(&context), 3, "瘦身心法夹到 3 条");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn full_profile_keeps_default_context_sizes() {
        let (reviewed, context) = review_with_profile(true, Some("full"), "full").await;
        assert_eq!(reviewed.prompt_profile.as_deref(), Some("full"));
        assert_eq!(
            context["klines"].as_array().unwrap().len(),
            14,
            "完整 K 线 14 根"
        );
        assert_eq!(
            context["news"].as_array().unwrap().len(),
            8,
            "完整资讯 8 条"
        );
        assert_eq!(
            context["experiences"].as_array().unwrap().len(),
            4,
            "完整经验全量注入"
        );
        assert_eq!(mantra_lines(&context), 4, "完整心法全量注入");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prompt_profile_is_recorded_in_audit() {
        let (reviewed, _) = review_with_profile(true, Some("weird"), "fallback").await;
        assert_eq!(
            reviewed.prompt_profile.as_deref(),
            Some("full"),
            "非法档位回退 full 并写入审计"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn inject_news_and_inject_mantras_gate_review_prompt() {
        let off = review_context_with(false, "off").await;
        assert!(
            off.get("news").is_none(),
            "关闭时复核 prompt 不含 news: {off}"
        );
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
        let reviewed =
            fill_default_valid_bars(reviewed_with_new_fields(), &json!({"valid_bars": 10}));
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
    fn wait_for_lock_acquires_after_release() {
        let lock = std::sync::Arc::new(std::sync::Mutex::new(0u8));
        let holder = lock.clone();
        let thread = std::thread::spawn(move || {
            let guard = holder.lock().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(40));
            drop(guard);
        });
        let result = wait_for_lock(&lock, std::time::Duration::from_millis(500));
        thread.join().unwrap();
        assert!(result.is_ok(), "锁释放后应等待成功: {result:?}");
    }

    #[test]
    fn wait_for_lock_times_out_when_held() {
        let lock = std::sync::Arc::new(std::sync::Mutex::new(0u8));
        let guard = lock.lock().unwrap();
        let started = std::time::Instant::now();
        let result = wait_for_lock(&lock, std::time::Duration::from_millis(50));
        drop(guard);
        let error = result.expect_err("持锁期间必须超时失败");
        assert!(error.contains("繁忙"), "错误应说明连接繁忙: {error}");
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(50),
            "必须等待满超时才失败，实际 {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn switches_do_not_clear_audit_meta() {
        // 开关只在消费点生效，复核出口必须保留 AI 失效价/入场价供审计落库。
        let reviewed = fill_default_valid_bars(reviewed_with_new_fields(), &json!({}));
        assert_eq!(reviewed.invalidation_price, Some(97.0));
        assert_eq!(reviewed.entry_price, Some(101.0));
    }
}
