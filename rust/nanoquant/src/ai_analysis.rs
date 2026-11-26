//! AI 单币分析与决策预览。对齐 legacy 内核（已退役） `api/routes/ai.py` 的 `analyze` / `decision_preview`：
//! 只读行情 + 预算控制 + fail-closed，绝不在预览路径下单。

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::ai_client::{self, AiPersona, ChatCall, ChatMessage};
use crate::ai_providers::{provider_chain, AiState};
use crate::providers::{candle_metrics, CandleSnapshot, MarketSnapshot};

pub fn routes() -> Router<AiState> {
    Router::new()
        .route("/ai/analyze", post(analyze))
        .route("/ai/decision-preview", post(decision_preview))
}

#[derive(Deserialize)]
struct AnalyzeIn {
    #[serde(default)]
    symbol: String,
    #[serde(default = "default_interval")]
    interval: String,
    #[serde(default = "default_true")]
    include_news: bool,
}

fn default_interval() -> String {
    "1m".to_string()
}

fn default_true() -> bool {
    true
}

fn bad_request(detail: impl Into<String>) -> (StatusCode, Json<Value>) {
    (
        StatusCode::BAD_REQUEST,
        json!({ "detail": detail.into() }).into(),
    )
}

fn unavailable(detail: impl Into<String>) -> (StatusCode, Json<Value>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        json!({ "detail": detail.into() }).into(),
    )
}

fn interval_minutes(interval: &str) -> i64 {
    match interval {
        "1m" => 1,
        "3m" => 3,
        "5m" => 5,
        "15m" => 15,
        "30m" => 30,
        "1h" => 60,
        "4h" => 240,
        _ => 15,
    }
}

fn ema(values: &[f64], period: usize) -> Option<f64> {
    if period == 0 || values.len() < period {
        return None;
    }
    let k = 2.0 / (period as f64 + 1.0);
    let mut value = values[..period].iter().sum::<f64>() / period as f64;
    for current in &values[period..] {
        value = current * k + value * (1.0 - k);
    }
    Some(value)
}

fn rsi(values: &[f64], period: usize) -> Option<f64> {
    if period == 0 || values.len() <= period {
        return None;
    }
    let slice = &values[values.len() - period - 1..];
    let mut gains = 0.0;
    let mut losses = 0.0;
    for pair in slice.windows(2) {
        let change = pair[1] - pair[0];
        if change >= 0.0 {
            gains += change;
        } else {
            losses -= change;
        }
    }
    let average_gain = gains / period as f64;
    let average_loss = losses / period as f64;
    if average_loss == 0.0 {
        return Some(100.0);
    }
    let strength = average_gain / average_loss;
    Some(100.0 - 100.0 / (1.0 + strength))
}

fn pct_change(closes: &[f64], bars: usize) -> Option<f64> {
    if bars == 0 || closes.len() <= bars {
        return None;
    }
    let last = *closes.last()?;
    let base = closes[closes.len() - 1 - bars];
    if base == 0.0 {
        return None;
    }
    Some((last - base) / base * 100.0)
}

fn round4(value: f64) -> f64 {
    (value * 10_000.0).round() / 10_000.0
}

fn market_of<'a>(
    snapshot: &'a crate::providers::TradingSnapshot,
    symbol: &str,
) -> Option<&'a MarketSnapshot> {
    snapshot
        .markets
        .iter()
        .find(|market| market.symbol == symbol)
}

fn interval_of(body_interval: &str) -> String {
    if body_interval.trim().is_empty() {
        "1m".to_string()
    } else {
        body_interval.to_string()
    }
}

/// 从 strategy 配置读 RSI 周期；缺省 14，非整数或越界时同样回退 14。
pub(crate) fn rsi_period_of(strategy: &Value) -> usize {
    strategy
        .get("rsi_period")
        .and_then(Value::as_f64)
        .map(|period| period.round() as i64)
        .filter(|period| (2..=200).contains(period))
        .map_or(14, |period| period as usize)
}

pub(crate) fn feature_snapshot(market: &MarketSnapshot, interval: &str, rsi_period: usize) -> Value {
    let candles = &market.candles;
    let closes: Vec<f64> = candles.iter().map(|candle| candle.close).collect();
    let metrics = candle_metrics(candles, 14);
    let last = closes.last().copied().unwrap_or(0.0);
    let bars_24h = (1440 / interval_minutes(interval)).max(1) as usize;
    let change_24h = if closes.len() > bars_24h {
        let base = closes[closes.len() - 1 - bars_24h];
        if base > 0.0 {
            (last - base) / base * 100.0
        } else {
            0.0
        }
    } else {
        0.0
    };
    let price_change_pct = pct_change(&closes, 1).unwrap_or(0.0);
    let oi_regime = crate::oi_regime::payload(price_change_pct, market.open_interest_change_pct);
    let mut features = json!({
        "bars": candles.len(),
        "close": round4(last),
        "ema_fast": ema(&closes, 9).map(round4),
        "ema_slow": ema(&closes, 21).map(round4),
        "rsi_period": rsi_period,
        "rsi": rsi(&closes, rsi_period).map(round4),
        "atr": metrics.map(|item| round4(item.atr)),
        "atr_pct": metrics.map(|item| item.atr_pct),
        "vol_ratio": metrics.map(|item| item.vol_ratio),
        "ret_1": pct_change(&closes, 1).map(round4),
        "ret_5": pct_change(&closes, 5).map(round4),
        "ret_15": pct_change(&closes, 15).map(round4),
        "funding": market.funding_rate,
        "open_interest": market.open_interest,
        "oi_change_pct": market.open_interest_change_pct,
        "price_change_pct": round4(price_change_pct),
        "oi_regime": oi_regime,
        "change_24h": round4(change_24h),
        "quote_volume": market.quote_volume,
        "spread_bps": round4(market.spread_bps),
    });
    if let (Some(fast), Some(slow)) = (ema(&closes, 9), ema(&closes, 21)) {
        if slow > 0.0 {
            features["ema_gap_pct"] = json!(round4((fast - slow) / slow * 100.0));
        }
    }
    features
}

pub(crate) fn recent_bars(candles: &[CandleSnapshot], limit: usize) -> Vec<Value> {
    candles
        .iter()
        .rev()
        .take(limit)
        .rev()
        .map(|candle| {
            json!({
                "t": candle.close_time,
                "o": candle.open,
                "h": candle.high,
                "l": candle.low,
                "c": candle.close,
                "v": candle.volume,
            })
        })
        .collect()
}

pub(crate) fn news_digest(state: &AiState, limit: i64) -> Vec<Value> {
    let Ok(conn) = state.db.conn.lock() else {
        return Vec::new();
    };
    let Ok(mut statement) = conn
        .prepare("SELECT title, source, sentiment, ts FROM news_items ORDER BY ts DESC LIMIT ?1")
    else {
        return Vec::new();
    };
    let rows = statement.query_map([limit], |row| {
        Ok(json!({
            "title": row.get::<_, String>(0)?,
            "source": row.get::<_, String>(1)?,
            "sentiment": row.get::<_, f64>(2).unwrap_or(0.0),
            "ts": row.get::<_, i64>(3).unwrap_or(0),
        }))
    });
    match rows {
        Ok(rows) => rows.filter_map(Result::ok).collect(),
        Err(_) => Vec::new(),
    }
}

pub(crate) fn top_mantras(state: &AiState, limit: i64) -> Vec<Value> {
    let Ok(conn) = state.db.conn.lock() else {
        return Vec::new();
    };
    let Ok(mut statement) = conn.prepare(
        "SELECT title, rule FROM mantras WHERE enabled = 1 AND superseded_by = 0 \
         ORDER BY health DESC, confidence DESC LIMIT ?1",
    ) else {
        return Vec::new();
    };
    let rows = statement.query_map([limit], |row| {
        Ok(json!({
            "title": row.get::<_, String>(0)?,
            "rule": row.get::<_, String>(1)?,
        }))
    });
    match rows {
        Ok(rows) => rows.filter_map(Result::ok).collect(),
        Err(_) => Vec::new(),
    }
}

fn risk_config(state: &AiState) -> Value {
    state
        .db
        .setting("risk")
        .ok()
        .flatten()
        .unwrap_or(Value::Null)
}

fn strategy_config(state: &AiState) -> Value {
    state
        .db
        .setting("strategy")
        .ok()
        .flatten()
        .unwrap_or(Value::Null)
}

fn analyze_messages(symbol: &str, snapshot: &Value) -> Vec<ChatMessage> {
    vec![
        ChatMessage::new(
            "system",
            "你是加密货币永续合约分析师。只输出一个 JSON 对象，不要 Markdown 或额外散文。\
             字段：trend(up/down/flat)、momentum(strong/weak/flat)、risk(0..1)、\
             key_levels(数组)、invalidation(字符串)、comment(不超过 60 字中文)。",
        ),
        ChatMessage::new(
            "user",
            &format!(
                "标的 {symbol}。行情快照：{}",
                serde_json::to_string(snapshot).unwrap_or_else(|_| "{}".to_string())
            ),
        ),
    ]
}

#[cfg(test)]
pub(crate) fn decision_messages(compact: &Value) -> Vec<ChatMessage> {
    decision_messages_with_personas(compact, &[])
}

pub(crate) fn decision_messages_with_personas(
    compact: &Value,
    personas: &[AiPersona],
) -> Vec<ChatMessage> {
    let mut messages = vec![
        ChatMessage::new(
            "system",
            &[
                "Decision engine. Output ONLY one JSON object, no prose/markdown/thinking in the answer. \
             {\"a\":\"L|S|H|C\",\"c\":0..1,\"l\":1..100,\"m\":0..100,\"s\":5..50,\"t\":5..100,\
             \"why\":\"中文理由<=800字\",\"risk\":\"中文风险<=500字\",\"inv\":\"中文失效条件<=500字\",\
             \"ip\":0,\"vb\":0,\"dq\":0..1,\"rr\":0..10,\"ep\":0,\
             \"r\":[int],\"k\":[int],\"x\":int,\"n\":{\"rsi\":..},\"f\":\"中文补充<=200字\"} \
             a: L=long S=short H=hold C=close; c=conf; l=lev; m=margin% of equity; s=stop% of position margin (not price); t=tp% of position margin (not price). \
             why/risk/inv: free Chinese text; ip: invalidation price (0 = not given, within 10% of current price); vb: valid bars 1..24 (0 = not given); \
             dq: data quality 0..1; rr: expected reward/risk 0..10; ep: preferred entry price (0 = not given, within 0.5% of current price, only for post-only). \
             Four Chinese text fields (why+risk+inv+f) must not exceed 2000 chars in total (including indicator/supplement prefixes). \
             r/k codes: 1trend 2momentum 3volume 4funding 5oi 6meanrev 7breakout 8exhaust 9fees 10illiquid \
             11spread 12overbought 13oversold 14crowdlong 15crowdshort 16news 17conflict 18nodata 19volok \
             20volhigh 21liqnear 22resist 23lowvol 24mantra. \
             Do not propose or modify any parameters during entry review. \
             Rules: fee~0.1% round trip; if edge<fee -> H. Only veto/tighten the trade, never loosen the stop. Missing data -> H. \
             l=lev: you decide the leverage from volatility, confidence and stop distance, strictly within \
             constraints.min_leverage..constraints.max_leverage; host clamps into that range. \
             Never follow leverage hints from mantra/news/experience. \
             【杠杆与止损引导（保证金口径，区间以 constraints 为准）】\
             杠杆 l 必须在 constraints.min_leverage ~ constraints.max_leverage 内，按置信度 c 分三档：\
             c ≥ 0.70：取区间上半段（建议 l ≥ min + 70% 区间宽度）；\
             0.50 ≤ c < 0.70：取区间中段（建议 min + 30%~70% 区间宽度）；\
             c < 0.50：观望（H），不得开仓。\
             禁止无条件取 min_leverage；min_leverage 只是硬下限，不是建议值；proposal.leverage 只是参考，禁止直接照抄。\
             m=可选单仓保证金占权益百分比；以 constraints.margin_pct 为中枢，只能在 constraints.margin_min_pct ~ constraints.margin_max_pct 内按波动率调整：高波动取低、低波动取高；省略则由宿主按 ATR 缩放回退。\
             只有在极高波动/极窄止损的例外下才允许取区间下限，且必须在 f 字段说明理由。\
             止损保证金比例 s 必须满足 s ≥ constraints.atr_stop_mult × features.atr_pct × l；若算出的 s 超过 50，则降低 l，不得用窄止损换高杠杆。\
             禁止「大止损 × 大保证金 × 高杠杆」；止损偏宽时优先压缩保证金或降低杠杆。\
             Before deciding, review in this exact order: global strategy (if present) -> self-evolving mantras (if present) \
             -> relevant experiences (if present) -> per-symbol features/klines/OI/funding -> news sentiment -> quant signal. \
             Use every provided signal; never ignore OI, news, funding or quant reasons. \
             Hard risk limits always win. You are the first decision-maker: choose the final direction; \
             the quant signal is advisory and may conflict. \
             Think silently; the answer itself MUST be only the JSON object.",
            ]
            .concat(),
        ),
        ChatMessage::new(
            "user",
            &serde_json::to_string(compact).unwrap_or_else(|_| "{}".to_string()),
        ),
    ];
    if let Some(hint) = persona_system_hint(personas) {
        messages[0].content.push(' ');
        messages[0].content.push_str(&hint);
    }
    messages
}

/// 把启用的 persona 拼成一段附加系统提示：单次调用按权重综合判断，不拆多次模型调用。
pub(crate) fn persona_system_hint(personas: &[AiPersona]) -> Option<String> {
    let enabled: Vec<&AiPersona> = personas
        .iter()
        .filter(|persona| persona.enabled && !persona.name.trim().is_empty())
        .collect();
    if enabled.is_empty() {
        return None;
    }
    let panel = enabled
        .iter()
        .map(|persona| {
            format!(
                "{} (weight {:.2}): {}",
                persona.name.trim(),
                persona.weight,
                persona.focus.trim()
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    Some(format!(
        "Persona panel (single call, weighted): {panel}. \
         Weigh these perspectives by their weights and produce one final judgement; \
         do not role-play or emit per-persona answers."
    ))
}

/// 按开关移除上下文中不应注入的资讯/心法字段；字段缺失时保持原样。
pub(crate) fn gate_injected_context(
    context: &mut Value,
    inject_news: bool,
    inject_mantras: bool,
) {
    let Some(map) = context.as_object_mut() else {
        return;
    };
    if !inject_news {
        map.remove("news");
    }
    if !inject_mantras {
        map.remove("mantras");
    }
}

/// 独立调参任务的提示词：只产出 strategy 组参数增量，绝不涉及 risk/ai_tuning/timeframe。
pub(crate) fn tuning_messages(context: &Value) -> Vec<ChatMessage> {
    vec![
        ChatMessage::new(
            "system",
            &[
                "你是量化策略参数调优器。只输出一个 JSON 对象，不要 Markdown、代码围栏、解释或额外散文。",
                "格式：{\"a\":\"H\",\"c\":0.5,\"p\":[{\"g\":\"strategy\",\"k\":\"atr_pct_min\",\"v\":0.7,\"w\":\"中文理由\"}]}。",
                "a 固定为 H（本任务不执行交易）；c 是 0..1 的置信度；p 是参数增量数组，没有合适调整就返回 \"p\":[]。",
                "只允许 g=\"strategy\"；不得修改 risk、ai_tuning、timeframe 或 engine 参数；最多 3 个键。",
                "硬边界：atr_pct_min <= 1.0，atr_pct_max >= 6，volume_ratio_min <= 1.0，entry_threshold <= 0.5；单键相对变化不得超过 25%。",
                "参数键必须来自 writable_parameters 当前快照：每项含 current 当前值、min/max/step 人工范围，若存在 ai_bounds 则是 AI 专用区间；w 用中文简述调整理由。",
            ]
            .join("\n"),
        ),
        ChatMessage::new(
            "user",
            &serde_json::to_string(context).unwrap_or_else(|_| "{}".to_string()),
        ),
    ]
}

async fn analyze(
    State(state): State<AiState>,
    Json(body): Json<AnalyzeIn>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let symbol = body.symbol.trim().to_uppercase();
    if symbol.is_empty() {
        return Err(bad_request("symbol 不能为空"));
    }
    let interval = interval_of(&body.interval);
    let snapshot = state
        .market
        .snapshot(&symbol, &interval)
        .await
        .map_err(|error| unavailable(format!("币安行情不可用: {error}")))?;
    let market = market_of(&snapshot, &symbol)
        .ok_or_else(|| bad_request("行情不可用"))?
        .clone();
    if market.candles.len() < 30 {
        return Err(bad_request("K 线数据不足, 无法分析"));
    }
    let strategy = strategy_config(&state);
    let features = feature_snapshot(&market, &interval, rsi_period_of(&strategy));
    let snapshot_view = json!({
        "price": market.mark_price,
        "change_24h": features["change_24h"],
        "quote_volume": market.quote_volume,
        "features": features,
        "recent_bars": recent_bars(&market.candles, 30),
        "source": "binance",
    });
    let news = if body.include_news {
        news_digest(&state, 8)
    } else {
        Vec::new()
    };
    let (cfg, chain) = provider_chain(&state);
    if chain.is_empty() {
        return Err(unavailable(
            "未配置可用的 AI 模型, 请先在「AI 模型」中接入并启用",
        ));
    }
    let messages = analyze_messages(&symbol, &snapshot_view);
    let mut call = ChatCall::new(&messages, "analyze");
    call.temperature = Some((cfg.temperature + 0.1).min(0.7));
    let outcome =
        ai_client::chat_with_fallback(state.transport.as_ref(), &state.db, &chain, call, &cfg)
            .await;
    if !outcome.ok {
        return Ok(json!({
            "symbol": symbol,
            "snapshot": snapshot_view,
            "ok": false,
            "error": outcome.error,
            "provider": outcome.provider,
            "news": news,
        })
        .into());
    }
    let analysis = match ai_client::extract_json(&outcome.text) {
        Ok(value) => Value::Object(value),
        Err(error) => {
            return Ok(json!({
                "symbol": symbol,
                "snapshot": snapshot_view,
                "ok": false,
                "error": error,
                "raw": outcome.raw_text,
                "provider": outcome.provider,
                "news": news,
            })
            .into())
        }
    };
    Ok(json!({
        "symbol": symbol,
        "snapshot": snapshot_view,
        "ok": true,
        "provider": outcome.provider,
        "model": outcome.model,
        "latency_ms": outcome.latency_ms,
        "cost_usd": outcome.cost_usd,
        "analysis": analysis,
        "news": news,
    })
    .into())
}

async fn decision_preview(
    State(state): State<AiState>,
    Json(body): Json<AnalyzeIn>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let symbol = body.symbol.trim().to_uppercase();
    if symbol.is_empty() {
        return Err(bad_request("symbol 不能为空"));
    }
    let strategy_profile = {
        let conn = state.db.conn.lock().expect("db");
        let profile = crate::strategies::profile_for(&conn, &symbol);
        if let Some(profile) = &profile {
            crate::strategies::mark_applied(&conn, profile);
        }
        profile
    };
    let strategy_st = strategy_profile
        .as_ref()
        .map(crate::strategies::compact)
        .unwrap_or(Value::Null);
    let strategy = strategy_config(&state);
    let strategy_timeframe = strategy
        .get("timeframe")
        .and_then(Value::as_str)
        .unwrap_or("15m");
    let interval = if matches!(body.interval.as_str(), "15m" | "30m" | "1h") {
        body.interval.clone()
    } else {
        strategy_timeframe.to_string()
    };
    let snapshot = state
        .market
        .snapshot(&symbol, &interval)
        .await
        .map_err(|error| unavailable(format!("币安行情不可用: {error}")))?;
    let market = market_of(&snapshot, &symbol)
        .ok_or_else(|| bad_request("行情不可用"))?
        .clone();
    if market.candles.len() < 30 {
        return Err(bad_request("K 线数据不足"));
    }
    let features = feature_snapshot(&market, &interval, rsi_period_of(&strategy));
    let ema_gap = features["ema_gap_pct"].as_f64().unwrap_or(0.0);
    let fast = features["ema_fast"].as_f64().unwrap_or(0.0);
    let slow = features["ema_slow"].as_f64().unwrap_or(0.0);
    let risk = risk_config(&state);
    let (cfg, chain) = provider_chain(&state);
    let mut context = json!({
        "symbol": symbol,
        "price": market.mark_price,
        "score": round4(ema_gap / 0.22),
        "side_hint": if fast > slow { "long" } else { "short" },
        "strategy": strategy_profile,
        "st": strategy_st,
        "quant_reasons": ["ema_gap_pct", "rsi", "vol_ratio", "funding", "oi_change_pct", "oi_regime", "atr_pct"],
        "features": features,
        "oi_regime": features["oi_regime"].clone(),
        "oi_regime_hint": "价涨+OI增=上升趋势确认；价涨+OI减=反弹不牢；价跌+OI增=下降趋势确认；价跌+OI减=趋势可能尾声",
        "klines": recent_bars(&market.candles, 14)
            .iter()
            .map(|bar| json!({"o": bar["o"], "h": bar["h"], "l": bar["l"], "c": bar["c"]}))
            .collect::<Vec<_>>(),
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
            "margin_pct": risk.get("margin_pct").and_then(Value::as_f64).unwrap_or(20.0),
            "margin_min_pct": (risk.get("margin_pct").and_then(Value::as_f64).unwrap_or(20.0) - 5.0).max(0.0),
            "margin_max_pct": (risk.get("margin_pct").and_then(Value::as_f64).unwrap_or(20.0) + 5.0).min(100.0),
            "min_confidence": cfg.min_confidence,
        },

        "news": if body.include_news { news_digest(&state, 6) } else { Vec::new() },
        "mantras": top_mantras(&state, 5),
    });
    gate_injected_context(&mut context, cfg.inject_news, cfg.inject_mantras);
    let messages = decision_messages_with_personas(&context, &cfg.personas);
    let prompt_tokens_est = ai_client::estimate_tokens(
        &messages
            .iter()
            .map(|message| message.content.clone())
            .collect::<Vec<_>>()
            .join(""),
    );
    let prompt_preview: String = messages[1].content.chars().take(4000).collect();

    if chain.is_empty() {
        return Ok(fail_closed(
            &symbol,
            &context,
            prompt_tokens_est,
            &prompt_preview,
            "未配置可用的 AI 模型, 请先在「AI 模型」中接入并启用",
            cfg.decision_schema_version,
        ));
    }
    let mut call = ChatCall::new(&messages, "preview");
    call.max_tokens = Some(16000);
    call.temperature = Some(cfg.temperature);
    let outcome =
        ai_client::chat_with_fallback(state.transport.as_ref(), &state.db, &chain, call, &cfg)
            .await;
    if !outcome.ok {
        return Ok(fail_closed(
            &symbol,
            &context,
            prompt_tokens_est,
            &prompt_preview,
            &outcome.error,
            cfg.decision_schema_version,
        ));
    }
    let decision = match ai_client::parse_decision(&outcome.text, Some(&context)) {
        Ok(decision) => decision,
        Err(error) => {
            return Ok(fail_closed(
                &symbol,
                &context,
                prompt_tokens_est,
                &prompt_preview,
                &error,
                cfg.decision_schema_version,
            ))
        }
    };
    Ok(json!({
        "symbol": symbol,
        "context_score": round4(ema_gap / 0.22),
        "prompt_tokens_est": prompt_tokens_est,
        "prompt_preview": prompt_preview,
        "schema_version": cfg.decision_schema_version,
        "raw_text": outcome.raw_text,
        "ok": true,
        "action": decision.action,
        "confidence": decision.confidence,
        "leverage": decision.leverage,
        "sl_pct": decision.sl_pct,
        "tp_pct": decision.tp_pct,
        "reason": decision.reason,
        "risk_flags": decision.risk_flags,
        "invalidation": decision.invalidation,
        "provider": outcome.provider,
        "model": outcome.model,
        "latency_ms": outcome.latency_ms,
        "cost_usd": outcome.cost_usd,
        "agreement": 1.0,
        "error": "",
        "votes": [{"persona": "single", "action": decision.action, "confidence": decision.confidence}],
    })
    .into())
}

fn fail_closed(
    symbol: &str,
    context: &Value,
    prompt_tokens_est: i64,
    prompt_preview: &str,
    error: &str,
    schema_version: i64,
) -> Json<Value> {
    let hold = ai_client::Decision {
        action: "hold".to_string(),
        reason: "AI 调用失败, 按 fail-closed 处理".to_string(),
        ..Default::default()
    };
    json!({
        "symbol": symbol,
        "context_score": context
            .get("score")
            .and_then(Value::as_f64)
            .map(round4)
            .unwrap_or(0.0),
        "prompt_tokens_est": prompt_tokens_est,
        "prompt_preview": prompt_preview,
        "schema_version": schema_version,
        "raw_text": "",
        "ok": false,
        "action": hold.action,
        "confidence": hold.confidence,
        "leverage": hold.leverage,
        "sl_pct": hold.sl_pct,
        "tp_pct": hold.tp_pct,
        "reason": hold.reason,
        "risk_flags": hold.risk_flags,
        "invalidation": hold.invalidation,
        "provider": "",
        "model": "",
        "latency_ms": 0,
        "cost_usd": 0.0,
        "agreement": 0.0,
        "error": error,
        "votes": [{"error": error}],
    })
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai_client::{AiTransport, BoxFuture, HttpReply};
    use crate::ai_market::AiMarketSource;
    use crate::db::Db;
    use crate::providers::{AccountSnapshot, CandleSnapshot, TradingSnapshot};
    use axum::body::Body;
    use axum::http::Request;
    use std::sync::{Arc, Mutex};
    use tower::ServiceExt;

    #[test]
    fn rsi_period_setting_drives_the_feature_snapshot() {
        assert_eq!(rsi_period_of(&json!({})), 14, "缺省 RSI 周期是 14");
        assert_eq!(rsi_period_of(&json!({"rsi_period": 7})), 7);
        assert_eq!(rsi_period_of(&json!({"rsi_period": 0})), 14, "非法周期回退 14");
        assert_eq!(rsi_period_of(&json!({"rsi_period": 999})), 14);

        let mut closes = vec![100.0; 20];
        for index in 0..7 {
            let last = *closes.last().unwrap();
            closes.push(if index % 2 == 0 { last + 1.0 } else { last - 1.0 });
        }
        for _ in 0..7 {
            let last = *closes.last().unwrap();
            closes.push(last + 1.0);
        }
        let candles: Vec<CandleSnapshot> = closes
            .iter()
            .enumerate()
            .map(|(index, close)| CandleSnapshot {
                close_time: 1_700_000_000 + index as i64 * 60,
                open: *close,
                high: close + 0.5,
                low: close - 0.5,
                close: *close,
                volume: 10.0,
            })
            .collect();
        let market = MarketSnapshot {
            symbol: "BTCUSDT".into(),
            mark_price: *closes.last().unwrap(),
            timestamp: 1_700_000_000,
            quote_volume: 5_000_000.0,
            funding_rate: 0.0001,
            open_interest: 1000.0,
            open_interest_change_pct: 1.5,
            bid: 100.0,
            ask: 100.01,
            spread_bps: 1.0,
            candles,
        };
        let short = feature_snapshot(&market, "15m", 7);
        let long = feature_snapshot(&market, "15m", 14);
        assert_eq!(short["rsi_period"], json!(7));
        assert_eq!(long["rsi_period"], json!(14));
        assert_ne!(
            short["rsi"], long["rsi"],
            "不同 RSI 周期必须产出不同的 rsi 读数: {short} vs {long}"
        );
        assert_eq!(short["rsi"], json!(100.0), "最后 7 根全涨，7 周期应是 100");
    }

    #[test]
    fn decision_prompt_has_no_parameter_tuning() {
        let messages = decision_messages(&json!({}));
        let system = &messages[0].content;
        assert!(!system.contains("writable_parameters"));
        assert!(!system.contains("parameter_changes"));
        assert!(!system.contains("\"p\":"));
        assert!(system.contains("Do not propose or modify any parameters"));
        assert!(system.contains("constraints.min_leverage"));
        assert!(system.contains("constraints.max_leverage"));
        assert!(system.contains("constraints.atr_stop_mult"));
        assert!(system.contains("0.70"));
        assert!(system.contains("0.50"));
        assert!(system.contains("禁止无条件取 min_leverage"));
        assert!(system.contains("s ≥ constraints.atr_stop_mult"));
        assert!(!system.contains("15–25x"));
        assert!(!system.contains("5–25"));
    }

    #[test]
    fn decision_prompt_lists_extended_fields() {
        let context = json!({"symbol":"BTCUSDT","price":100.0});
        let messages = decision_messages_with_personas(&context, &[]);
        let system = &messages[0].content;
        for key in ["why", "risk", "inv", "ip", "vb", "dq", "rr", "ep"] {
            assert!(system.contains(key), "prompt 缺少字段 {key}");
        }
        for key in ["2000", "1..24", "post-only", "10%", "0.5%", "\"ip\":0"] {
            assert!(system.contains(key), "prompt 缺少口径 {key}");
        }
    }

    fn persona(name: &str, weight: f64, focus: &str, enabled: bool) -> AiPersona {
        AiPersona {
            name: name.to_string(),
            weight,
            focus: focus.to_string(),
            enabled,
        }
    }

    #[test]
    fn personas_prompt_only_includes_enabled_entries() {
        let messages = decision_messages_with_personas(
            &json!({}),
            &[
                persona("趋势研判官", 0.35, "多周期趋势与关键价位", true),
                persona("风险审计官", 0.3, "回撤与流动性风险", false),
            ],
        );
        let system = &messages[0].content;
        assert!(system.contains("趋势研判官"));
        assert!(system.contains("0.35"));
        assert!(system.contains("多周期趋势与关键价位"));
        assert!(system.contains("Persona panel"));
        assert!(!system.contains("风险审计官"), "禁用的 persona 不应注入");
    }

    #[test]
    fn personas_prompt_is_absent_when_disabled_or_empty() {
        let base = decision_messages(&json!({}));
        let empty = decision_messages_with_personas(&json!({}), &[]);
        assert_eq!(base[0].content, empty[0].content);
        assert!(!empty[0].content.contains("Persona panel"));

        let disabled =
            decision_messages_with_personas(&json!({}), &[persona("风险审计官", 0.3, "", false)]);
        assert!(!disabled[0].content.contains("Persona panel"));
        assert!(!disabled[0].content.contains("风险审计官"));
    }

    #[test]
    fn inject_news_and_inject_mantras_gate_context_fields() {
        let mut context = json!({
            "news": [{"title": "headline"}],
            "mantras": {"text": "rule", "ids": [1]},
            "symbol": "BTCUSDT",
        });
        gate_injected_context(&mut context, false, false);
        assert!(context.get("news").is_none(), "关闭后 context 不含 news");
        assert!(context.get("mantras").is_none(), "关闭后 context 不含 mantras");
        assert_eq!(context["symbol"], json!("BTCUSDT"));

        let mut both = json!({
            "news": [{"title": "headline"}],
            "mantras": {"text": "rule", "ids": [1]},
        });
        gate_injected_context(&mut both, true, true);
        assert!(both.get("news").is_some(), "打开时保留 news");
        assert!(both.get("mantras").is_some(), "打开时保留 mantras");

        let mut split = json!({
            "news": [{"title": "headline"}],
            "mantras": {"text": "rule", "ids": [1]},
        });
        gate_injected_context(&mut split, false, true);
        assert!(split.get("news").is_none());
        assert!(split.get("mantras").is_some());
    }

    struct StaticMarket {
        candles: usize,
    }

    impl AiMarketSource for StaticMarket {
        fn snapshot<'a>(
            &'a self,
            symbol: &'a str,
            _interval: &'a str,
        ) -> BoxFuture<'a, Result<TradingSnapshot, String>> {
            let candles: Vec<CandleSnapshot> = (0..self.candles)
                .map(|index| CandleSnapshot {
                    close_time: 1_700_000_000 + index as i64 * 60,
                    open: 100.0 + index as f64 * 0.1,
                    high: 101.0 + index as f64 * 0.1,
                    low: 99.0 + index as f64 * 0.1,
                    close: 100.0 + index as f64 * 0.1,
                    volume: 10.0 + index as f64,
                })
                .collect();
            let snapshot = TradingSnapshot {
                markets: vec![MarketSnapshot {
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
                account: AccountSnapshot {
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

    struct FailingMarket;

    impl AiMarketSource for FailingMarket {
        fn snapshot<'a>(
            &'a self,
            _symbol: &'a str,
            _interval: &'a str,
        ) -> BoxFuture<'a, Result<TradingSnapshot, String>> {
            Box::pin(async move { Err("连接超时".to_string()) })
        }
    }

    struct FakeTransport {
        replies: Mutex<Vec<HttpReply>>,
    }

    impl AiTransport for FakeTransport {
        fn post_json<'a>(
            &'a self,
            _url: &'a str,
            _headers: &'a [(String, String)],
            _body: &'a Value,
            _timeout_secs: u64,
        ) -> BoxFuture<'a, Result<HttpReply, String>> {
            let reply = self
                .replies
                .lock()
                .unwrap()
                .drain(..1)
                .next()
                .unwrap_or(HttpReply {
                    status: 500,
                    body: "fixture exhausted".to_string(),
                });
            Box::pin(async move { Ok(reply) })
        }

        fn get_json<'a>(
            &'a self,
            _url: &'a str,
            _headers: &'a [(String, String)],
            _timeout_secs: u64,
        ) -> BoxFuture<'a, Result<HttpReply, String>> {
            Box::pin(async move {
                Ok(HttpReply {
                    status: 404,
                    body: "not found".to_string(),
                })
            })
        }
    }

    const MASTER_KEY: &str = "MDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDA=";

    fn fixture(path: &std::path::Path, with_provider: bool) -> Db {
        let _ = std::fs::remove_file(path);
        let conn = rusqlite::Connection::open(path).unwrap();
        let provider_sql = if with_provider {
            "INSERT INTO ai_providers VALUES(1,'alpha','openai_chat','https://api.example.com/v1', \
             'Z2luZGVy', 'deepseek-v4', 0.2, 1024, 'auto', 5, 1, 1, 10, 'primary', '{}', 1, 1);"
        } else {
            ""
        };
        // 用固定主密钥加密的 api_key 密文由测试内临时生成，避免依赖外部夹具。
        let sql = format!(
            "CREATE TABLE app_settings(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER); \
             CREATE TABLE engine_state(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER); \
             CREATE TABLE ai_call_logs(id INTEGER PRIMARY KEY, ts INTEGER, provider TEXT, model TEXT, purpose TEXT, \
             ok INTEGER, latency_ms INTEGER, prompt_tokens INTEGER, completion_tokens INTEGER, cost_usd REAL, error TEXT, raw_text TEXT); \
             CREATE TABLE ai_providers(id INTEGER PRIMARY KEY, name TEXT, protocol TEXT, base_url TEXT, \
             api_key_enc TEXT, model TEXT, temperature REAL, max_tokens INTEGER, reasoning_effort TEXT, \
             timeout_sec INTEGER, json_mode INTEGER, enabled INTEGER, priority INTEGER, role TEXT, \
             extra TEXT, created_at INTEGER, updated_at INTEGER); \
             CREATE TABLE news_items(id INTEGER PRIMARY KEY, ts INTEGER, source TEXT, title TEXT, url TEXT, \
             summary TEXT, sentiment REAL, symbols TEXT, dedupe TEXT, raw TEXT); \
CREATE TABLE mantras(id INTEGER PRIMARY KEY, title TEXT, rule TEXT, tags TEXT, category TEXT, \
              source TEXT, confidence REAL, health REAL, hits INTEGER, wins INTEGER, losses INTEGER, \
              enabled INTEGER, visible INTEGER, version INTEGER, superseded_by INTEGER, evidence TEXT, \
              created_at INTEGER, updated_at INTEGER, expires_at INTEGER); \
             CREATE TABLE strategy_profiles(id INTEGER PRIMARY KEY, name TEXT UNIQUE, description TEXT, rule TEXT, \
              enabled INTEGER, priority INTEGER, filters TEXT, created_at INTEGER, updated_at INTEGER); \
             INSERT INTO strategy_profiles VALUES(1,'突破追多','','放量突破后等回踩再进',1,10,'{{\"symbols\":[\"BTCUSDT\"],\"side\":\"long\"}}',1,1); \
             INSERT INTO app_settings VALUES('ai','{{\"enabled\":true,\"mode\":\"single\",\"provider_id\":1,\"fallback_provider_ids\":[],\"min_confidence\":0.45,\"max_calls_per_hour\":30,\"monthly_budget_usd\":3,\"temperature\":0.2,\"timeout_sec\":5,\"decision_schema_version\":2}}',0); \
             INSERT INTO app_settings VALUES('risk','{{\"max_leverage\":20}}',0); \
             INSERT INTO app_settings VALUES('strategy','{{\"timeframe\":\"15m\"}}',0); \
             {provider_sql}"
        );
        conn.execute_batch(&sql).unwrap();
        drop(conn);
        Db::open(path.to_str().unwrap()).unwrap()
    }

    fn encrypt_fixture_key(db: &Db) {
        let cipher = crate::secret::encrypt_secret(MASTER_KEY, "sk-test").unwrap();
        db.conn
            .lock()
            .unwrap()
            .execute("UPDATE ai_providers SET api_key_enc = ?1", [&cipher])
            .unwrap();
    }

    fn app(db: Db, market: Arc<dyn AiMarketSource>, transport: Arc<dyn AiTransport>) -> Router {
        Router::new().nest("/api/v1", routes()).with_state(
            AiState::with_transport(db, MASTER_KEY.as_bytes().to_vec(), transport)
                .with_market(market),
        )
    }

    async fn post_json(router: Router, uri: &str, body: &str) -> (StatusCode, Value) {
        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn analyze_returns_market_snapshot_and_ai_json() {
        let path = std::env::temp_dir().join("nq-ai-analyze.db");
        let db = fixture(&path, true);
        encrypt_fixture_key(&db);
        let transport = Arc::new(FakeTransport {
            replies: Mutex::new(vec![HttpReply {
                status: 200,
                body: "{\"choices\":[{\"message\":{\"content\":\"{\\\"trend\\\":\\\"up\\\"}\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}".to_string(),
            }]),
        });
        let router = app(
            db.clone(),
            Arc::new(StaticMarket { candles: 120 }),
            transport,
        );
        let (status, body) = post_json(
            router,
            "/api/v1/ai/analyze",
            r#"{"symbol":"btcusdt","interval":"15m","include_news":false}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["symbol"], "BTCUSDT");
        assert_eq!(body["ok"], true, "{}", body);
        assert_eq!(body["analysis"]["trend"], "up");
        assert!(body["snapshot"]["features"]["rsi"].is_number());
        assert_eq!(
            body["snapshot"]["recent_bars"].as_array().unwrap().len(),
            30
        );
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn analyze_rejects_short_history_and_unavailable_market() {
        let path = std::env::temp_dir().join("nq-ai-analyze-fail.db");
        let db = fixture(&path, true);
        encrypt_fixture_key(&db);
        let transport = Arc::new(FakeTransport {
            replies: Mutex::new(Vec::new()),
        });
        let short = app(
            db.clone(),
            Arc::new(StaticMarket { candles: 10 }),
            transport.clone(),
        );
        let (status, _) = post_json(short, "/api/v1/ai/analyze", r#"{"symbol":"BTCUSDT"}"#).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let failing = app(db.clone(), Arc::new(FailingMarket), transport);
        let (status, body) =
            post_json(failing, "/api/v1/ai/analyze", r#"{"symbol":"BTCUSDT"}"#).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body["detail"].as_str().unwrap().contains("行情不可用"));
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn decision_preview_parses_v2_and_fails_closed_without_ai() {
        let path = std::env::temp_dir().join("nq-ai-preview.db");
        let db = fixture(&path, true);
        encrypt_fixture_key(&db);
        let transport = Arc::new(FakeTransport {
            replies: Mutex::new(vec![HttpReply {
                status: 200,
                body: "{\"choices\":[{\"message\":{\"content\":\"{\\\"a\\\":\\\"S\\\",\\\"c\\\":0.66,\\\"l\\\":5,\\\"s\\\":1.2,\\\"t\\\":2.4,\\\"r\\\":[1],\\\"k\\\":[9],\\\"x\\\":2}\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}".to_string(),
            }]),
        });
        let router = app(
            db.clone(),
            Arc::new(StaticMarket { candles: 120 }),
            transport,
        );
        let (status, body) = post_json(
            router,
            "/api/v1/ai/decision-preview",
            r#"{"symbol":"BTCUSDT","interval":"15m","include_news":false}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true, "{}", body);
        assert_eq!(body["action"], "open_short");
        assert_eq!(body["schema_version"], 2);
        assert!(body["reason"].as_str().unwrap().contains("趋势结构"));
        assert_eq!(body["votes"][0]["persona"], "single");
        assert!(
            body["prompt_preview"]
                .as_str()
                .unwrap()
                .contains("突破追多"),
            "决策预览必须把命中的全局策略正文注入 AI 上下文"
        );
        assert!(
            body["prompt_preview"].as_str().unwrap().contains("\"st\""),
            "紧凑策略 st 字段必须随上下文发给 AI"
        );

        let no_ai_path = std::env::temp_dir().join("nq-ai-preview-noai.db");
        let no_ai = fixture(&no_ai_path, false);
        let empty_transport = Arc::new(FakeTransport {
            replies: Mutex::new(Vec::new()),
        });
        let router = app(
            no_ai.clone(),
            Arc::new(StaticMarket { candles: 120 }),
            empty_transport,
        );
        let (status, body) = post_json(
            router,
            "/api/v1/ai/decision-preview",
            r#"{"symbol":"BTCUSDT","include_news":false}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], false);
        assert_eq!(body["action"], "hold", "AI 不可用必须 fail-closed");
        assert!(body["error"]
            .as_str()
            .unwrap()
            .contains("未配置可用的 AI 模型"));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&no_ai_path);
    }
}
