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
    let context = json!({
        "symbol": proposal.symbol,
        "price": market.mark_price,
        "score": features["ema_gap_pct"].as_f64().map(|value| value / 0.22).unwrap_or(0.0),
        "side_hint": proposal.side,
        "quant_reasons": ["ema_gap_pct", "rsi", "vol_ratio", "funding", "oi_change_pct", "oi_regime", "atr_pct"],
        "features": features,
        "oi_regime": features["oi_regime"].clone(),
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
        "strategy": profile,
        "st": st,
        "proposal": {
            "side": proposal.side,
            "leverage": proposal.leverage,
            "stop_price": proposal.stop_price,
            "mark_price": proposal.mark_price,
        },
        "news": [],
        "mantras": [],
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
    let decision = ai_client::parse_decision(&outcome.text, Some(&context))
        .map_err(|error| format!("AI 复核结果无效: {error}"))?;
    let side_lock = profile
        .as_ref()
        .and_then(|profile| profile.get("filters"))
        .and_then(|filters| filters.get("side"))
        .and_then(Value::as_str)
        .unwrap_or("any");
    decision_to_review(&proposal, side_lock, &decision, cfg.min_confidence)
}

fn decision_to_review(
    proposal: &EntryProposal,
    side_lock: &str,
    decision: &ai_client::Decision,
    min_confidence: f64,
) -> Result<ReviewDecision, String> {
    if decision.confidence < min_confidence {
        return Ok(ReviewDecision {
            action: ReviewAction::Reject,
            leverage: None,
            stop_price: None,
            reason: format!(
                "AI 置信度 {:.2} 低于 {:.2}",
                decision.confidence, min_confidence
            ),
        });
    }
    let action = decision.action.as_str();
    if action != "open_long" && action != "open_short" {
        return Ok(ReviewDecision {
            action: ReviewAction::Reject,
            leverage: None,
            stop_price: None,
            reason: if decision.reason.is_empty() {
                "AI 未批准开仓".to_string()
            } else {
                decision.reason.clone()
            },
        });
    }
    if side_lock == "long" && action == "open_short" {
        return Ok(ReviewDecision {
            action: ReviewAction::Reject,
            leverage: None,
            stop_price: None,
            reason: "策略只允许做多，AI 提案方向相反".to_string(),
        });
    }
    if side_lock == "short" && action == "open_long" {
        return Ok(ReviewDecision {
            action: ReviewAction::Reject,
            leverage: None,
            stop_price: None,
            reason: "策略只允许做空，AI 提案方向相反".to_string(),
        });
    }
    let wanted = if proposal.side == "long" {
        "open_long"
    } else {
        "open_short"
    };
    if action != wanted {
        return Ok(ReviewDecision {
            action: ReviewAction::Reject,
            leverage: None,
            stop_price: None,
            reason: "AI 方向与量化信号冲突".to_string(),
        });
    }
    let leverage = (decision.leverage > 0.0 && decision.leverage < proposal.leverage)
        .then_some(decision.leverage);
    let stop_price = if decision.sl_pct > 0.0 {
        let ai_stop = if proposal.side == "long" {
            proposal.mark_price * (1.0 - decision.sl_pct / 100.0)
        } else {
            proposal.mark_price * (1.0 + decision.sl_pct / 100.0)
        };
        let tighter = (proposal.side == "long" && ai_stop > proposal.stop_price)
            || (proposal.side == "short" && ai_stop < proposal.stop_price);
        tighter.then_some(ai_stop)
    } else {
        None
    };
    Ok(ReviewDecision {
        action: if leverage.is_some() || stop_price.is_some() {
            ReviewAction::Tighten
        } else {
            ReviewAction::Agree
        },
        leverage,
        stop_price,
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
            leverage: 10.0,
            stop_price: if side == "long" { 95.0 } else { 105.0 },
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
               CREATE TABLE ai_providers(id INTEGER PRIMARY KEY, name TEXT, protocol TEXT, base_url TEXT,
                 api_key_enc TEXT, model TEXT, temperature REAL, max_tokens INTEGER, reasoning_effort TEXT,
                 timeout_sec INTEGER, json_mode INTEGER, enabled INTEGER, priority INTEGER, role TEXT,
                 extra TEXT, created_at INTEGER, updated_at INTEGER);
               CREATE TABLE strategy_profiles(id INTEGER PRIMARY KEY, name TEXT UNIQUE, description TEXT, rule TEXT,
                 enabled INTEGER, priority INTEGER, filters TEXT, created_at INTEGER, updated_at INTEGER);
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
    fn wider_ai_stop_and_higher_leverage_are_downgraded_to_no_change() {
        let wider = ai_client::Decision {
            action: "open_long".to_string(),
            confidence: 0.9,
            leverage: 50.0,
            sl_pct: 10.0,
            tp_pct: 0.0,
            reason: "wider".to_string(),
            risk_flags: Vec::new(),
            invalidation: String::new(),
        };
        let reviewed = decision_to_review(&proposal("long"), "any", &wider, 0.45).unwrap();
        assert_eq!(
            reviewed.action,
            ReviewAction::Agree,
            "更宽的止损和更高的杠杆都不该被采纳"
        );
        assert_eq!(reviewed.leverage, None);
        assert_eq!(reviewed.stop_price, None);
    }

    #[test]
    fn ai_side_must_match_the_quant_signal() {
        let conflict =
            decision_to_review(&proposal("long"), "any", &decision("open_short", 0.9), 0.45)
                .unwrap();
        assert_eq!(conflict.action, ReviewAction::Reject);
        assert!(
            conflict.reason.contains("冲突"),
            "方向冲突必须说明原因，实际：{}",
            conflict.reason
        );
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
