//! 独立调参任务：按 `ai.param_tuning_min_interval_sec` 周期性让 AI 生成参数增量，
//! 校验后只落一条 `candidate` 参数集。绝不写 `app_settings.strategy`、绝不激活参数集；
//! 证据门与启用由 B/C 期负责。

use std::time::Duration;

use rusqlite::{params, Connection};
use serde_json::{json, Map, Value};
use tokio::sync::watch;

use crate::ai_client::{self, ChatCall};
use crate::ai_providers::{provider_chain, AiState};
use crate::parameter_sets::{
    active_for_regime, insert_candidate, validate_candidate, ParameterSet,
};

const STARTUP_DELAY_SEC: u64 = 300;
const DEFAULT_INTERVAL_SEC: i64 = 86_400;
const MIN_INTERVAL_SEC: i64 = 60;
const MAX_INTERVAL_SEC: i64 = 604_800;
const TRADES_WINDOW_SEC: i64 = 30 * 24 * 3600;
const SIGNALS_WINDOW_SEC: i64 = 7 * 24 * 3600;

// 保持显式 Future 签名：调用方直接把返回值交给 tokio::spawn，行为等价于 async fn。
#[allow(clippy::manual_async_fn)]
pub(crate) fn spawn_tuning_task(
    state: AiState,
    mut stop: watch::Receiver<bool>,
) -> impl std::future::Future<Output = ()> {
    async move {
        // 首次检查延迟 5 分钟：服务刚启动时行情、regime 与信号审计都还没稳定。
        tokio::time::sleep(Duration::from_secs(STARTUP_DELAY_SEC)).await;
        loop {
            if *stop.borrow() {
                break;
            }
            if let Err(error) = run_once(&state).await {
                record_failure(&state, &error);
            }
            let wait = interval_seconds(&state).max(MIN_INTERVAL_SEC) as u64;
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(wait)) => {}
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        break;
                    }
                }
            }
        }
    }
}

/// 单轮任务：读取配置与父集，调 AI，校验并落候选。跳过条件返回 `Ok(())`。
pub(crate) async fn run_once(state: &AiState) -> Result<(), String> {
    let Some(prepared) = prepare(state)? else {
        return Ok(());
    };
    let (cfg, chain) = provider_chain(state);
    if chain.is_empty() {
        return Err("未配置可用的 AI 模型, 请先在「AI 模型」中接入并启用".to_string());
    }
    let messages = crate::ai_analysis::tuning_messages(&prepared.context);
    let call = ChatCall::new(&messages, "parameter_tuning");
    let outcome =
        ai_client::chat_with_fallback(state.transport.as_ref(), &state.db, &chain, call, &cfg)
            .await;
    if !outcome.ok {
        return Err(format!("AI 调参调用失败: {}", outcome.error));
    }
    let decision = ai_client::parse_decision(&outcome.text, None)
        .map_err(|error| format!("AI 调参输出无法解析: {error}"))?;

    let conn = state
        .db
        .conn
        .lock()
        .map_err(|_| "数据库连接锁不可用".to_string())?;
    let now = crate::now();
    if decision.params.is_empty() {
        write_last_tuning_ts(&conn, now)?;
        record_event(
            &conn,
            "info",
            "调参未产生候选",
            json!({
                "event": "parameter_set.no_candidate",
                "regime": prepared.regime,
                "timeframe": prepared.active.timeframe,
            }),
        )?;
        return Ok(());
    }
    let next = validate_candidate(&prepared.active.params, &decision.params)?;
    let name = format!("{}-ai-{}", prepared.regime, now);
    let set_id = insert_candidate(
        &conn,
        &name,
        &prepared.regime,
        prepared.active.timeframe.as_str(),
        &next,
        Some(prepared.active.id),
    )?;
    write_last_tuning_ts(&conn, now)?;
    record_event(
        &conn,
        "info",
        "生成参数集候选",
        json!({
            "set_id": set_id,
            "regime": prepared.regime,
            "timeframe": prepared.active.timeframe,
        }),
    )?;
    Ok(())
}

struct Prepared {
    regime: String,
    active: ParameterSet,
    context: Value,
}

/// 同步准备阶段：全部数据库读取在返回前完成，避免锁跨 await。
fn prepare(state: &AiState) -> Result<Option<Prepared>, String> {
    let conn = state
        .db
        .conn
        .lock()
        .map_err(|_| "数据库连接锁不可用".to_string())?;
    let ai = group_json(&conn, "ai");
    if !flag(&ai, "allow_param_tuning", true) {
        return Ok(None);
    }
    let parameter_set = group_json(&conn, "parameter_set");
    if !flag(&parameter_set, "enabled", true) {
        return Ok(None);
    }
    let mode = current_mode(&conn);
    let requested = read_market_regime(&conn, &mode).unwrap_or_else(|| "range".to_string());
    let mut active = active_for_regime(&conn, &requested)?;
    if active.is_none() && requested == "trend" {
        // trend 没有独立参数集时按规格回退到 range 父集。
        active = active_for_regime(&conn, "range")?;
    }
    let Some(active) = active else {
        record_event(
            &conn,
            "warn",
            "参数集父集缺失",
            json!({
                "event": "parameter_set.parent_missing",
                "mode": mode,
                "regime": requested,
            }),
        )?;
        return Ok(None);
    };
    // 回退后一律以父集 regime 生候选：候选 regime 必须与父集一致。
    let regime = active.regime.clone();
    let now = crate::now();
    let interval = tuning_interval_sec(&ai);
    if let Some(last) = state_i64(&conn, "parameter_set.last_tuning_ts") {
        if now - last < interval {
            return Ok(None);
        }
    }
    let context = tuning_context(&conn, &mode, &requested, &active, now)?;
    Ok(Some(Prepared {
        regime,
        active,
        context,
    }))
}

fn tuning_context(
    conn: &Connection,
    mode: &str,
    requested_regime: &str,
    active: &ParameterSet,
    now: i64,
) -> Result<Value, String> {
    let (closed, wins, total_pnl): (i64, i64, f64) = conn
        .query_row(
            "SELECT COUNT(*), \
             COALESCE(SUM(CASE WHEN pnl > 0 THEN 1 ELSE 0 END), 0), \
             COALESCE(SUM(pnl), 0) \
             FROM trades WHERE mode=?1 AND closed_at >= ?2",
            params![mode, now - TRADES_WINDOW_SEC],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|error| format!("统计最近 30 天成交失败: {error}"))?;
    let win_rate = if closed > 0 {
        wins as f64 / closed as f64
    } else {
        0.0
    };
    let avg_pnl = if closed > 0 {
        total_pnl / closed as f64
    } else {
        0.0
    };
    let signals_total: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM signals WHERE mode=?1 AND ts >= ?2",
            params![mode, now - SIGNALS_WINDOW_SEC],
            |row| row.get(0),
        )
        .map_err(|error| format!("统计最近 7 天信号失败: {error}"))?;
    let gate_rejected: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM signals WHERE mode=?1 AND ts >= ?2 \
             AND json_extract(CASE WHEN json_valid(features) THEN features ELSE '{}' END, '$.stage')='gate'",
            params![mode, now - SIGNALS_WINDOW_SEC],
            |row| row.get(0),
        )
        .map_err(|error| format!("统计 gate 拒绝失败: {error}"))?;
    let ai_hold: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM signals WHERE mode=?1 AND ts >= ?2 AND ai_action='hold'",
            params![mode, now - SIGNALS_WINDOW_SEC],
            |row| row.get(0),
        )
        .map_err(|error| format!("统计 AI hold 失败: {error}"))?;
    Ok(json!({
        "mode": mode,
        "requested_regime": requested_regime,
        "regime": active.regime,
        "timeframe": active.timeframe,
        "active_set": { "id": active.id, "name": active.name },
        "active_params": active.params,
        "trades_30d": {
            "closed": closed,
            "wins": wins,
            "win_rate": win_rate,
            "total_pnl": total_pnl,
            "avg_pnl": avg_pnl,
        },
        "signals_7d": {
            "total": signals_total,
            "gate_rejected": gate_rejected,
            "ai_hold": ai_hold,
        },
        "writable_params": crate::param_registry::writable_catalog(),
        "ai_bounds": ai_bounds_of(&active.params),
    }))
}

/// 只暴露 active 快照里真的存在、且 AI 可写的数字键的专用边界。
fn ai_bounds_of(params: &Value) -> Value {
    let mut bounds = Map::new();
    if let Some(object) = params.as_object() {
        for key in object.keys() {
            let Some(meta) = crate::param_registry::find("strategy", key) else {
                continue;
            };
            if meta.kind != "number" || !meta.ai_writable {
                continue;
            }
            if let Some((low, high)) = crate::param_registry::ai_bounds("strategy", key) {
                bounds.insert(key.clone(), json!([low, high]));
            }
        }
    }
    Value::Object(bounds)
}

fn record_failure(state: &AiState, error: &str) {
    tracing::warn!(%error, "参数集调参任务失败");
    let Ok(conn) = state.db.conn.lock() else {
        return;
    };
    let _ = record_event(
        &conn,
        "warn",
        "参数集调参失败",
        json!({ "event": "parameter_set.tuning_failed", "error": error }),
    );
}

fn record_event(
    conn: &Connection,
    level: &str,
    message: &str,
    payload: Value,
) -> Result<(), String> {
    conn.execute(
        "INSERT INTO engine_events(ts,level,category,message,payload) \
         VALUES(?1,?2,'parameter_set',?3,?4)",
        params![crate::now(), level, message, payload.to_string()],
    )
    .map_err(|error| format!("写入参数集事件失败: {error}"))?;
    Ok(())
}

fn write_last_tuning_ts(conn: &Connection, now: i64) -> Result<(), String> {
    conn.execute(
        "INSERT INTO engine_state(key,value,updated_at) VALUES('parameter_set.last_tuning_ts',?1,?2) \
         ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
        params![now.to_string(), now],
    )
    .map_err(|error| format!("写入调参时间失败: {error}"))?;
    Ok(())
}

fn interval_seconds(state: &AiState) -> i64 {
    let Ok(conn) = state.db.conn.lock() else {
        return DEFAULT_INTERVAL_SEC;
    };
    tuning_interval_sec(&group_json(&conn, "ai"))
}

fn tuning_interval_sec(ai: &Value) -> i64 {
    let value = ai
        .get("param_tuning_min_interval_sec")
        .and_then(|item| item.as_i64().or_else(|| item.as_f64().map(|n| n as i64)))
        .unwrap_or(DEFAULT_INTERVAL_SEC);
    value.clamp(MIN_INTERVAL_SEC, MAX_INTERVAL_SEC)
}

fn flag(group: &Value, key: &str, default: bool) -> bool {
    group.get(key).and_then(Value::as_bool).unwrap_or(default)
}

fn group_json(conn: &Connection, key: &str) -> Value {
    conn.query_row(
        "SELECT value FROM app_settings WHERE key=?1",
        [key],
        |row| row.get::<_, String>(0),
    )
    .ok()
    .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
    .unwrap_or(Value::Null)
}

/// `engine_state.mode` 优先，其次 `engine.mode`，最后 demo。
fn current_mode(conn: &Connection) -> String {
    let stored = conn
        .query_row(
            "SELECT value FROM engine_state WHERE key='mode'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .filter(|mode| !mode.is_empty());
    if let Some(mode) = stored {
        return mode;
    }
    group_json(conn, "engine")
        .get("mode")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| "demo".to_string())
}

/// A4 按交易环境隔离写入 `market_regime.{mode}`；无值或非法值都回退 range。
fn read_market_regime(conn: &Connection, mode: &str) -> Option<String> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM engine_state WHERE key=?1",
            [format!("market_regime.{mode}")],
            |row| row.get(0),
        )
        .ok();
    raw.and_then(|value| {
        crate::regime::Regime::from_str(value.trim()).map(|regime| regime.as_str().to_string())
    })
}

fn state_i64(conn: &Connection, key: &str) -> Option<i64> {
    conn.query_row(
        "SELECT value FROM engine_state WHERE key=?1",
        [key],
        |row| row.get::<_, String>(0),
    )
    .ok()
    .and_then(|raw| raw.parse::<i64>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai_client::{AiTransport, BoxFuture, HttpReply};
    use crate::db::Db;
    use std::sync::{Arc, Mutex};

    const MASTER_KEY: &str = "MDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDA=";

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
            let reply = self.replies.lock().unwrap().pop().unwrap_or(HttpReply {
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

    fn memory_db() -> Db {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::schema::migrate(&conn).unwrap();
        Db {
            path: String::new(),
            conn: Arc::new(Mutex::new(conn)),
        }
    }

    fn strategy_snapshot() -> Value {
        json!({
            "timeframe": "1h",
            "atr_pct_min": 1.5,
            "atr_pct_max": 8.0,
            "volume_ratio_min": 1.1,
            "entry_threshold": 0.55
        })
    }

    fn ai_config(allow: bool) -> Value {
        json!({
            "enabled": true,
            "mode": "single",
            "provider_id": 1,
            "fallback_provider_ids": [],
            "min_confidence": 0.45,
            "max_calls_per_hour": 30,
            "monthly_budget_usd": 3.0,
            "temperature": 0.2,
            "timeout_sec": 5,
            "decision_schema_version": 2,
            "allow_param_tuning": allow,
            "param_tuning_min_interval_sec": 86400
        })
    }

    fn seed_settings(db: &Db, allow: bool) {
        let conn = db.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO app_settings VALUES('ai',?1,0)",
            [ai_config(allow).to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO app_settings VALUES('engine',?1,0)",
            [json!({"enabled": false, "mode": "demo", "cycle_interval_sec": 15}).to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO app_settings VALUES('parameter_set',?1,0)",
            [
                json!({"enabled": true, "auto_apply": false, "auto_apply_timeframes": "1h"})
                    .to_string(),
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO app_settings VALUES('strategy',?1,0)",
            [strategy_snapshot().to_string()],
        )
        .unwrap();
    }

    fn seed_provider(db: &Db) {
        let conn = db.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO ai_providers(id,name,protocol,base_url,api_key_enc,model,temperature, \
             max_tokens,reasoning_effort,timeout_sec,json_mode,enabled,priority,role,extra, \
             created_at,updated_at) \
             VALUES(1,'alpha','openai_chat','https://api.example.com/v1','x','deepseek-v4', \
             0.2,1024,'auto',5,1,1,10,'primary','{}',1,1)",
            [],
        )
        .unwrap();
        let cipher = crate::secret::encrypt_secret(MASTER_KEY, "sk-test").unwrap();
        conn.execute("UPDATE ai_providers SET api_key_enc=?1", [cipher])
            .unwrap();
    }

    fn seed_regime(db: &Db, regime: &str) {
        db.conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO engine_state(key,value,updated_at) VALUES('market_regime.demo',?1,0)",
                [regime],
            )
            .unwrap();
    }

    fn chat_reply(content: &str) -> HttpReply {
        HttpReply {
            status: 200,
            body: json!({
                "choices": [{"message": {"content": content}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5}
            })
            .to_string(),
        }
    }

    fn state(db: Db, replies: Vec<HttpReply>) -> AiState {
        let transport = Arc::new(FakeTransport {
            replies: Mutex::new(replies),
        });
        AiState::with_transport(db, MASTER_KEY.as_bytes().to_vec(), transport)
    }

    fn strategy_value(db: &Db) -> String {
        db.conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT value FROM app_settings WHERE key='strategy'",
                [],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn candidate_count(db: &Db) -> i64 {
        db.conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM parameter_sets WHERE source='ai'",
                [],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn event_payloads(db: &Db) -> Vec<Value> {
        let conn = db.conn.lock().unwrap();
        let mut statement = conn
            .prepare("SELECT payload FROM engine_events WHERE category='parameter_set' ORDER BY id")
            .unwrap();
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap();
        rows.filter_map(Result::ok)
            .map(|raw| serde_json::from_str(&raw).unwrap_or(Value::Null))
            .collect()
    }

    #[tokio::test]
    async fn allow_param_tuning_false_skips_generation() {
        let db = memory_db();
        seed_settings(&db, false);
        seed_regime(&db, "range");
        let state = state(
            db.clone(),
            vec![chat_reply("{\"a\":\"H\",\"c\":0.5,\"p\":[]}")],
        );

        run_once(&state).await.unwrap();

        assert_eq!(candidate_count(&db), 0, "关闭总开关不应生成候选");
        assert!(event_payloads(&db).is_empty(), "关闭总开关不应写参数集事件");
    }

    #[tokio::test]
    async fn missing_active_parent_records_event_and_keeps_task_alive() {
        let db = memory_db();
        seed_settings(&db, true);
        seed_regime(&db, "range");
        let state = state(db.clone(), Vec::new());

        run_once(&state).await.unwrap();

        assert_eq!(candidate_count(&db), 0);
        let payloads = event_payloads(&db);
        assert_eq!(payloads.len(), 1, "只应写一条父集缺失事件");
        assert_eq!(payloads[0]["event"], "parameter_set.parent_missing");
        assert_eq!(payloads[0]["regime"], "range");
    }

    #[tokio::test]
    async fn run_once_creates_candidate_without_touching_strategy() {
        let db = memory_db();
        seed_settings(&db, true);
        seed_provider(&db);
        seed_regime(&db, "range");
        {
            let conn = db.conn.lock().unwrap();
            crate::parameter_sets::seed_bootstrap(&conn).unwrap();
        }
        let before = strategy_value(&db);
        let state = state(
            db.clone(),
            vec![chat_reply(
                "{\"a\":\"H\",\"c\":0.5,\"p\":[{\"g\":\"strategy\",\"k\":\"atr_pct_min\",\"v\":0.7,\"w\":\"test\"}]}",
            )],
        );

        run_once(&state).await.unwrap();

        let conn = db.conn.lock().unwrap();
        let parent_id: i64 = conn
            .query_row(
                "SELECT id FROM parameter_sets WHERE regime='range' AND status='active'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let (status, source, timeframe, parent, params): (String, String, String, Option<i64>, String) =
            conn.query_row(
                "SELECT status,source,timeframe,parent_id,params FROM parameter_sets WHERE source='ai'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .unwrap();
        assert_eq!(status, "candidate");
        assert_eq!(source, "ai");
        assert_eq!(timeframe, "1h");
        assert_eq!(parent, Some(parent_id));
        let params: Value = serde_json::from_str(&params).unwrap();
        assert_eq!(params["atr_pct_min"], 0.7);
        assert_eq!(params["timeframe"], "1h");

        let last_ts: String = conn
            .query_row(
                "SELECT value FROM engine_state WHERE key='parameter_set.last_tuning_ts'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(last_ts.parse::<i64>().unwrap() > 0);

        let (message, payload): (String, String) = conn
            .query_row(
                "SELECT message,payload FROM engine_events \
                 WHERE category='parameter_set' AND message='生成参数集候选' ORDER BY id DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(message, "生成参数集候选");
        let payload: Value = serde_json::from_str(&payload).unwrap();
        assert!(payload["set_id"].as_i64().unwrap() > 0);
        assert_eq!(payload["regime"], "range");
        assert_eq!(payload["timeframe"], "1h");
        drop(conn);

        assert_eq!(
            strategy_value(&db),
            before,
            "调参任务绝不能改写 app_settings.strategy"
        );
    }

    #[tokio::test]
    async fn interval_not_elapsed_skips_generation() {
        let db = memory_db();
        seed_settings(&db, true);
        seed_provider(&db);
        seed_regime(&db, "range");
        {
            let conn = db.conn.lock().unwrap();
            crate::parameter_sets::seed_bootstrap(&conn).unwrap();
            conn.execute(
                "INSERT INTO engine_state(key,value,updated_at) \
                 VALUES('parameter_set.last_tuning_ts',?1,0)",
                [(crate::now() - 10).to_string()],
            )
            .unwrap();
        }
        let state = state(db.clone(), Vec::new());

        run_once(&state).await.unwrap();

        assert_eq!(
            candidate_count(&db),
            0,
            "间隔未到不应重复调用 AI 或生成候选"
        );
        assert!(event_payloads(&db).is_empty());
    }

    #[test]
    fn tuning_prompt_requires_bounded_strategy_only_patch() {
        let messages = crate::ai_analysis::tuning_messages(&json!({"regime": "range"}));
        let system = &messages[0].content;
        assert!(system.contains("\"p\":"));
        assert!(system.contains("strategy"));
        assert!(system.contains("atr_pct_min"));
        assert!(system.contains("atr_pct_max"));
        assert!(system.contains("volume_ratio_min"));
        assert!(system.contains("entry_threshold"));
        assert!(system.contains("25%"));
        assert!(system.contains("最多 3 个键"));
        assert!(!system.contains("```"));
        assert_eq!(messages[1].role, "user");
        assert!(messages[1].content.contains("\"regime\":\"range\""));
    }
}
