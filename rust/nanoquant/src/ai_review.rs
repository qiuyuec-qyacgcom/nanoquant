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

fn strip_secrets(value: &mut Value, group: &str) {
    if let Some(object) = value.as_object_mut() {
        object.retain(|key, _| !crate::param_registry::is_sensitive(group, key));
        for child in object.values_mut() {
            strip_secrets(child, group);
        }
    } else if let Some(items) = value.as_array_mut() {
        for child in items {
            strip_secrets(child, group);
        }
    }
}

fn review_parameters(conn: &rusqlite::Connection, cfg: &ai_client::AiConfig) -> Value {
    let mut out = serde_json::Map::new();
    for group in [
        "engine",
        "strategy",
        "risk",
        "news",
        "journal",
        "evolution",
        "system",
        "data",
        "backup",
        "public",
        "signal_follow",
    ] {
        let mut value = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key = ?1",
                [group],
                |row| row.get::<_, String>(0),
            )
            .ok()
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .unwrap_or(Value::Null);
        strip_secrets(&mut value, group);
        out.insert(group.to_string(), value);
    }
    let tuning_interval = conn
        .query_row("SELECT value FROM app_settings WHERE key='ai'", [], |row| {
            row.get::<_, String>(0)
        })
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|value| value.get("param_tuning_min_interval_sec").cloned())
        .unwrap_or_else(|| json!(0));
    out.insert(
        "ai".to_string(),
        json!({
            "enabled": cfg.enabled,
            "mode": cfg.mode,
            "min_confidence": cfg.min_confidence,
            "max_calls_per_hour": cfg.max_calls_per_hour,
            "monthly_budget_usd": cfg.monthly_budget_usd,
            "timeout_sec": cfg.timeout_sec,
            "temperature": cfg.temperature,
            "decision_schema_version": cfg.decision_schema_version,
            "param_tuning_min_interval_sec": tuning_interval,
        }),
    );
    out.insert(
        "parameter_changes".to_string(),
        parameter_change_context(conn),
    );
    out.insert(
        "writable_parameters".to_string(),
        crate::param_registry::writable_catalog()["groups"].clone(),
    );
    Value::Object(out)
}

fn parameter_change_context(conn: &rusqlite::Connection) -> Value {
    let Ok(mut statement) = conn.prepare(
        "SELECT id, ts, group_name, key, old_value, new_value, reason, mode, source \
         FROM parameter_changes ORDER BY ts DESC, id DESC LIMIT 10",
    ) else {
        return json!([]);
    };
    let Ok(rows) = statement.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
            row.get::<_, String>(7)?,
            row.get::<_, String>(8)?,
        ))
    }) else {
        return json!([]);
    };
    let changes = rows.filter_map(Result::ok).collect::<Vec<_>>();
    let windows = changes
        .iter()
        .map(|row| (row.0, row.1, row.7.clone()))
        .collect::<Vec<_>>();
    let effects = crate::settings_api::effects_for_changes(conn, &windows);
    let items = changes
        .into_iter()
        .map(
            |(id, ts, group, key, old_value, new_value, reason, mode, source)| {
                json!({
                    "id": id,
                    "ts": ts,
                    "group": group,
                    "key": key,
                    "old": old_value.and_then(|text| serde_json::from_str::<Value>(&text).ok()),
                    "new": serde_json::from_str::<Value>(&new_value).ok(),
                    "reason": reason,
                    "mode": mode,
                    "source": source,
                    "effect": effects.get(&id).cloned().unwrap_or_else(|| json!({"trades": 0, "pnl": 0.0})),
                })
            },
        )
        .collect::<Vec<_>>();
    json!(items)
}

async fn review_entry(state: AiState, proposal: EntryProposal) -> Result<ReviewDecision, String> {
    let strategy_cfg = state
        .db
        .setting("strategy")
        .ok()
        .flatten()
        .unwrap_or(Value::Null);
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
    let features = crate::ai_analysis::feature_snapshot(market, &interval);
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
    let (mantra_text, mantra_ids, experiences, parameters) = {
        let conn = state
            .db
            .conn
            .lock()
            .map_err(|_| "数据库锁不可用".to_string())?;
        let evolution = crate::evolution_api::evolution_cfg(&conn);
        let (mantra_text, mantra_ids) = crate::evolution_api::injection_payload(&conn, &evolution);
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
        let parameters = review_parameters(&conn, &cfg);
        (mantra_text, mantra_ids, experiences, parameters)
    };
    let news = crate::ai_analysis::news_digest(&state, 8);
    let quant_signal = proposal.quant_side.as_ref().map(|side| {
        json!({
            "side": side,
            "score": proposal.quant_score,
            "stop_price": proposal.stop_price,
            "take_profit": proposal.take_profit,
            "mark_price": proposal.mark_price,
        })
    });
    let context = json!({
        "analysis_order": [
            "global_strategy",
            "self_evolving_mantras",
            "decision_experiences",
            "parameters",
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
        "parameters": parameters,
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
    let messages = crate::ai_analysis::decision_messages(&context);
    let mut call = ChatCall::new(&messages, "engine_review");
    call.temperature = Some(cfg.temperature);
    let outcome =
        ai_client::chat_with_fallback(state.transport.as_ref(), &state.db, &chain, call, &cfg)
            .await;
    if !outcome.ok {
        return Err(format!("AI 复核失败: {}", outcome.error));
    }
    let mut decision = ai_client::parse_decision(&outcome.text, Some(&context))
        .map_err(|error| format!("AI 复核结果无效: {error}"))?;
    let outcome_log_id = outcome.log_id;
    apply_decision_params(&state, &mut decision, outcome_log_id);
    let side_lock = profile
        .as_ref()
        .and_then(|profile| profile.get("filters"))
        .and_then(|filters| filters.get("side"))
        .and_then(Value::as_str)
        .unwrap_or("any");
    decision_to_review(&proposal, side_lock, &decision, cfg.min_confidence)
}

fn tuning_enabled(value: Option<&Value>) -> bool {
    match value {
        None => true,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_i64().map(|n| n != 0).unwrap_or(false),
        Some(Value::String(text)) => matches!(text.as_str(), "true" | "1"),
        _ => false,
    }
}

/// 冷却窗口只拦截窗口内的重复调参；间隔为 0 时保持原有行为。
fn tuning_cooldown_active(last_ts: Option<i64>, now: i64, interval_sec: i64) -> bool {
    interval_sec > 0 && last_ts.is_some_and(|last| now - last < interval_sec)
}

fn apply_decision_params(state: &AiState, decision: &mut ai_client::Decision, decision_id: i64) {
    if decision.params.is_empty() {
        return;
    }
    let tuning = state
        .db
        .setting("ai")
        .ok()
        .flatten()
        .and_then(|value| value.get("allow_param_tuning").cloned());
    if !tuning_enabled(tuning.as_ref()) {
        let note = format!(
            "ai.allow_param_tuning=false，已忽略 {} 项参数更新",
            decision.params.len()
        );
        decision.risk_flags.push(note.clone());
        if let Ok(conn) = state.db.conn.lock() {
            let _ = conn.execute(
                "INSERT INTO engine_events(ts, level, category, message, payload) \
                 VALUES(?1, 'warn', 'param_change', ?2, ?3)",
                (
                    crate::now(),
                    note,
                    json!({ "count": decision.params.len() }).to_string(),
                ),
            );
        }
        return;
    }
    let mode = state
        .db
        .setting("engine")
        .ok()
        .flatten()
        .and_then(|value| {
            value
                .get("mode")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| "demo".to_string());
    // setting() 会再锁一次库，必须在下方的 conn 守卫之前读取
    let interval_sec = state
        .db
        .setting("ai")
        .ok()
        .flatten()
        .and_then(|value| value.get("param_tuning_min_interval_sec").cloned())
        .and_then(|value| {
            value
                .as_i64()
                .or_else(|| value.as_f64().map(|number| number as i64))
        })
        .unwrap_or(0)
        .max(0);
    let Ok(conn) = state.db.conn.lock() else {
        decision
            .risk_flags
            .push("参数写回失败：数据库锁不可用".to_string());
        return;
    };
    if interval_sec > 0 {
        // 表不存在或查询失败都按“没有历史调参”处理，不能中断复核
        let last_ts = conn
            .query_row("SELECT MAX(ts) FROM parameter_changes", [], |row| {
                row.get::<_, Option<i64>>(0)
            })
            .ok()
            .flatten();
        if tuning_cooldown_active(last_ts, crate::now(), interval_sec) {
            let note = format!("调参冷却中，已忽略 {} 项更新", decision.params.len());
            decision.risk_flags.push(note.clone());
            let _ = conn.execute(
                "INSERT INTO engine_events(ts, level, category, message, payload) \
                 VALUES(?1, 'warn', 'param_change', ?2, ?3)",
                (
                    crate::now(),
                    note,
                    json!({ "count": decision.params.len(), "interval_sec": interval_sec })
                        .to_string(),
                ),
            );
            return;
        }
    }

    match crate::settings_api::apply_param_updates(&conn, &decision.params, decision_id, &mode) {
        Ok(report) => {
            if let Some(skipped) = report.get("skipped").and_then(Value::as_array) {
                for item in skipped {
                    let note = format!("AI 调参被拒绝: {item}");
                    decision.risk_flags.push(note.clone());
                    let _ = conn.execute(
                        "INSERT INTO engine_events(ts, level, category, message, payload) \
                         VALUES(?1, 'warn', 'param_change', ?2, ?3)",
                        (crate::now(), note, item.to_string()),
                    );
                }
            }
        }
        Err(error) => {
            let note = format!("参数写回失败: {error}");
            decision.risk_flags.push(note.clone());
            let _ = conn.execute(
                "INSERT INTO engine_events(ts, level, category, message, payload) \
                 VALUES(?1, 'warn', 'param_change', ?2, ?3)",
                (
                    crate::now(),
                    note,
                    json!({ "count": decision.params.len() }).to_string(),
                ),
            );
        }
    }
}

fn reject(reason: impl Into<String>) -> ReviewDecision {
    ReviewDecision {
        action: ReviewAction::Reject,
        side: None,
        leverage: None,
        stop_price: None,
        take_profit: None,
        reason: reason.into(),
    }
}

fn decision_to_review(
    proposal: &EntryProposal,
    side_lock: &str,
    decision: &ai_client::Decision,
    min_confidence: f64,
) -> Result<ReviewDecision, String> {
    if decision.confidence < min_confidence {
        return Ok(reject(format!(
            "AI 置信度 {:.2} 低于 {:.2}",
            decision.confidence, min_confidence
        )));
    }
    let action = decision.action.as_str();
    let side = match action {
        "open_long" => "long",
        "open_short" => "short",
        _ => {
            return Ok(reject(if decision.reason.is_empty() {
                "AI 未批准开仓".to_string()
            } else {
                decision.reason.clone()
            }))
        }
    };
    if side_lock == "long" && side == "short" {
        return Ok(reject("策略只允许做多，AI 方向相反"));
    }
    if side_lock == "short" && side == "long" {
        return Ok(reject("策略只允许做空，AI 方向相反"));
    }
    let mark = proposal.mark_price;
    let stop_price = if decision.sl_pct > 0.0 {
        if side == "long" {
            mark * (1.0 - decision.sl_pct / 100.0)
        } else {
            mark * (1.0 + decision.sl_pct / 100.0)
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
        if side == "long" {
            mark * (1.0 + decision.tp_pct / 100.0)
        } else {
            mark * (1.0 - decision.tp_pct / 100.0)
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
        return Ok(reject("AI 返回的止损/止盈价无效"));
    }
    let leverage = (decision.leverage > 0.0 && decision.leverage < proposal.leverage)
        .then_some(decision.leverage);
    let changed = side != proposal.side || leverage.is_some();
    Ok(ReviewDecision {
        action: if changed {
            ReviewAction::Tighten
        } else {
            ReviewAction::Agree
        },
        side: Some(side.to_string()),
        leverage,
        stop_price: Some(stop_price),
        take_profit: Some(take_profit),
        reason: decision.reason.clone(),
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
            leverage: 10.0,
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
            sl_pct: 0.0,
            tp_pct: 0.0,
            reason: "test".to_string(),
            risk_flags: Vec::new(),
            invalidation: String::new(),
            params: Vec::new(),
        }
    }

    #[test]
    fn long_side_lock_rejects_opposite_ai_entry() {
        let result = decision_to_review(
            &proposal("short"),
            "long",
            &decision("open_short", 0.8),
            0.45,
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
        )
        .unwrap();
        assert_eq!(result.action, ReviewAction::Reject);
        assert!(result.reason.contains("只允许做空"));
    }

    #[test]
    fn matching_ai_entry_is_approved() {
        let result =
            decision_to_review(&proposal("long"), "long", &decision("open_long", 0.8), 0.45)
                .unwrap();
        assert_eq!(result.action, ReviewAction::Agree);
    }

    #[test]
    fn low_confidence_and_hold_are_rejected() {
        let low = decision_to_review(&proposal("long"), "any", &decision("open_long", 0.2), 0.45)
            .unwrap();
        assert_eq!(low.action, ReviewAction::Reject);
        let hold =
            decision_to_review(&proposal("long"), "any", &decision("hold", 0.9), 0.45).unwrap();
        assert_eq!(hold.action, ReviewAction::Reject);
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

    struct ParamTransport;

    impl crate::ai_client::AiTransport for ParamTransport {
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
                    body: r#"{"choices":[{"message":{"content":"{\"a\":\"L\",\"c\":0.8,\"l\":0,\"s\":0,\"t\":0,\"r\":[1],\"k\":[9],\"x\":2,\"p\":[{\"g\":\"strategy\",\"k\":\"entry_threshold\",\"v\":0.35,\"w\":\"test\"}]}"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#.to_string(),
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

    fn param_reviewer(db: &crate::db::Db) -> EngineAiReviewer {
        let state = AiState::with_transport(
            db.clone(),
            MASTER_KEY.as_bytes().to_vec(),
            std::sync::Arc::new(ParamTransport),
        )
        .with_market(std::sync::Arc::new(StaticMarket));
        EngineAiReviewer::new(state)
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn review_applies_param_updates_when_enabled() {
        let path = std::env::temp_dir().join("nq-ai-reviewer-params.db");
        let db = fixture(&path);
        let reviewer = param_reviewer(&db);
        let _ = reviewer.review(&proposal("long")).unwrap();
        let strategy = stored_strategy(&db);
        assert_eq!(strategy["entry_threshold"], 0.35, "{strategy}");
        let changes: i64 = db
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM parameter_changes", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(changes, 1);
    }

    #[test]
    fn param_tuning_flag_is_fail_closed() {
        assert!(tuning_enabled(None));
        assert!(tuning_enabled(Some(&json!(true))));
        assert!(tuning_enabled(Some(&json!(1))));
        assert!(tuning_enabled(Some(&json!("1"))));
        assert!(!tuning_enabled(Some(&json!(false))));
        assert!(!tuning_enabled(Some(&json!(0))));
        assert!(!tuning_enabled(Some(&json!("false"))));
        assert!(!tuning_enabled(Some(&json!(null))));
        assert!(!tuning_enabled(Some(&json!({ "enabled": true }))));
    }

    #[test]
    fn cooldown_only_blocks_inside_the_window() {
        assert!(!tuning_cooldown_active(None, 1_000, 300));
        assert!(!tuning_cooldown_active(Some(800), 1_000, 0));
        assert!(tuning_cooldown_active(Some(800), 1_000, 300));
        assert!(!tuning_cooldown_active(Some(600), 1_000, 300));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn review_skips_param_updates_when_disabled() {
        let path = std::env::temp_dir().join("nq-ai-reviewer-params-off.db");
        let db = fixture(&path);
        db.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE app_settings SET value=json_set(value,'$.allow_param_tuning',0) WHERE key='ai'",
                [],
            )
            .unwrap();
        let reviewer = param_reviewer(&db);
        let _ = reviewer.review(&proposal("long")).unwrap();
        let strategy = stored_strategy(&db);
        assert!(
            strategy.get("entry_threshold").is_none(),
            "关闭后不得写入参数: {strategy}"
        );
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
            sl_pct: 2.0,
            tp_pct: 0.0,
            reason: "tighten".to_string(),
            risk_flags: Vec::new(),
            invalidation: String::new(),
            params: Vec::new(),
        };
        let reviewed = decision_to_review(&proposal("long"), "any", &long, 0.45).unwrap();
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
            sl_pct: 2.0,
            tp_pct: 0.0,
            reason: "tighten".to_string(),
            risk_flags: Vec::new(),
            invalidation: String::new(),
            params: Vec::new(),
        };
        let reviewed = decision_to_review(&proposal("short"), "any", &short, 0.45).unwrap();
        assert_eq!(reviewed.leverage, Some(3.0));
        let short_stop = reviewed.stop_price.expect("空单应收紧止损");
        assert!(
            (short_stop - 102.0).abs() < 1e-9,
            "空单止损必须下移到 102，实际 {short_stop}"
        );
        assert!(short_stop < 105.0, "收紧后的空单止损不能高于原止损");
    }

    #[test]
    fn wider_ai_stop_is_not_used_for_an_agree_decision() {
        let wider = ai_client::Decision {
            action: "open_long".to_string(),
            confidence: 0.9,
            leverage: 50.0,
            sl_pct: 10.0,
            tp_pct: 0.0,
            reason: "wider".to_string(),
            risk_flags: Vec::new(),
            invalidation: String::new(),
            params: Vec::new(),
        };
        let reviewed = decision_to_review(&proposal("long"), "any", &wider, 0.45).unwrap();
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
        assert_eq!(clamped.leverage, 10.0);
        assert_eq!(clamped.stop_price, 95.0, "更宽的 AI 止损不能被主机采纳");
    }

    #[test]
    fn ai_side_can_override_the_quant_signal() {
        let conflict =
            decision_to_review(&proposal("long"), "any", &decision("open_short", 0.9), 0.45)
                .unwrap();
        assert_eq!(conflict.action, ReviewAction::Tighten);
        assert_eq!(conflict.side.as_deref(), Some("short"));
    }

    #[test]
    fn confidence_equal_to_threshold_is_accepted() {
        let result =
            decision_to_review(&proposal("long"), "any", &decision("open_long", 0.45), 0.45)
                .unwrap();
        assert_eq!(
            result.action,
            ReviewAction::Agree,
            "置信度等于门槛时应放行，而不是被 < 误杀"
        );
    }
}
