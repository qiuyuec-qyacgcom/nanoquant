use crate::db::Db;
use futures_util::FutureExt;
use rusqlite::OptionalExtension;
use serde_json::{json, Value};
use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{watch, Notify};
use tokio::time::{sleep, Duration};

const MIN_INTERVAL_SEC: i64 = 15;
const MAX_INTERVAL_SEC: i64 = 3600;

/// 全局冷却在 `cooldowns` 里的哨兵 symbol。
/// 真实标的不会叫这个名字，用同一张表避免额外的 schema 迁移。
const GLOBAL_COOLDOWN_SYMBOL: &str = "__global__";

/// LIMIT 开仓挂单的最短超时。周期更长时按 `cycle_interval_sec * 2` 放宽。
const LIMIT_ORDER_TIMEOUT_SEC: i64 = 120;

/// 引擎判活阈值（秒）。
///
/// 必须跟着扫描周期走：周期 10 分钟时，两轮之间天然有近 10 分钟没有新的扫描心跳，
/// 阈值写死 180 秒的话，面板每 10 分钟有 7 分钟在喊「引擎未运行」——那是假的。
/// 取两倍周期再加一分钟余量：慢一轮不算死，连续两轮没有心跳才算。
pub(crate) fn liveness_threshold_sec(interval_sec: u64) -> i64 {
    const MIN_THRESHOLD_SEC: i64 = 180;
    (interval_sec as i64 * 2 + 60).max(MIN_THRESHOLD_SEC)
}

/// Instant 到现在的毫秒数（f64，用于七段延迟样本）。
fn elapsed_ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

/// 当前扫描周期（秒）。读不到配置就用最小值。
pub(crate) fn cycle_interval_sec(conn: &rusqlite::Connection) -> u64 {
    conn.query_row(
        "SELECT value FROM app_settings WHERE key='engine'",
        [],
        |row| row.get::<_, String>(0),
    )
    .ok()
    .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
    .and_then(|value| value.get("cycle_interval_sec").and_then(Value::as_i64))
    .unwrap_or(MIN_INTERVAL_SEC)
    .clamp(MIN_INTERVAL_SEC, MAX_INTERVAL_SEC) as u64
}

/// LIMIT 开仓挂单的超时秒数。
/// 最短 120 秒；周期拉长到 1 分钟以上时按两轮周期放宽，避免刚挂上就被撤掉。
fn limit_order_timeout_sec(conn: &rusqlite::Connection) -> i64 {
    LIMIT_ORDER_TIMEOUT_SEC.max(cycle_interval_sec(conn) as i64 * 2)
}

/// panic 载荷里能取到字符串就取，取不到给个兜底文案。
fn panic_detail(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "未知 panic".to_string()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RuntimeConfig {
    pub(crate) enabled: bool,
    pub(crate) interval_sec: u64,
}

/// 实盘开仓的下单方式。平仓与保护单始终是市价，避免挂不出去。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LiveOrderType {
    Market,
    LimitPostOnly,
}

impl LiveOrderType {
    fn from_setting(raw: Option<&str>) -> Self {
        match raw.map(str::trim) {
            Some(value) if value.eq_ignore_ascii_case("limit_post_only") => Self::LimitPostOnly,
            _ => Self::Market,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Market => "MARKET",
            Self::LimitPostOnly => "LIMIT",
        }
    }

    fn is_post_only(self) -> bool {
        self == Self::LimitPostOnly
    }
}

/// 实盘准备发送的一笔管仓单。网络请求必须在数据库事务之外发。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LivePlan {
    pub(crate) symbol: String,
    pub(crate) position_side: String,
    /// 交易所方向：平多 SELL，平空 BUY。
    pub(crate) side: String,
    pub(crate) qty: f64,
    pub(crate) purpose: String,
    pub(crate) mark_price: f64,
    /// 开仓下单方式；平仓计划固定为市价。
    pub(crate) order_type: LiveOrderType,
    /// 限价开仓的委托价（未按 tick 格式化，发单前统一格式化）。
    pub(crate) price: Option<f64>,
    entry: Option<EntryTerms>,
    /// 开仓决策时的注入 ID，随成交回执写入持仓 features。
    features: Option<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct EntryTerms {
    leverage: f64,
    stop: f64,
    take_profit: f64,
    score: f64,
}

struct EntryDraft {
    symbol: String,
    side: String,
    quant_side: Option<String>,
    quant_score: Option<f64>,
    preview_side: Option<String>,
    preview_score: Option<f64>,
    quant_passed: bool,
    /// 全局杠杆下限（AI 未给杠杆时的回退值）。
    leverage: f64,
    /// 全局杠杆上限，已并入交易所与单标的限制。
    max_leverage: f64,
    stop_price: f64,
    take_profit: f64,
    score: f64,
    mark_price: f64,
    filter: crate::live::SymbolFilter,
    scale: f64,
    features_text: String,
    journal_snapshot: Value,
    client_id: String,
}

impl EntryDraft {
    fn signal_side(&self) -> Option<&str> {
        self.quant_side
            .as_deref()
            .or(self.preview_side.as_deref())
    }

    fn signal_score(&self) -> f64 {
        self.quant_score
            .or(self.preview_score)
            .unwrap_or(self.score)
    }

    fn proposal(&self) -> crate::review::EntryProposal {
        crate::review::EntryProposal {
            symbol: self.symbol.clone(),
            side: self.side.clone(),
            quant_side: self.quant_side.clone(),
            quant_score: self.quant_score,
            leverage: self.leverage,
            max_leverage: self.max_leverage,
            stop_price: self.stop_price,
            take_profit: self.take_profit,
            mark_price: self.mark_price,
        }
    }

    /// 拼一条信号审计记录。评分、方向、特征、AI 结论、拒绝原因一次带齐，
    /// 前端的「信号审计」页直接读这张表回答「为什么没开仓」。
    fn audit_row(
        &self,
        decision: &str,
        direction: &str,
        reject_reason: &str,
        ai_action: &str,
        ai_confidence: f64,
        ai_reason: &str,
    ) -> crate::signal_audit::AuditSignal {
        let mut features: Value =
            serde_json::from_str(&self.features_text).unwrap_or_else(|_| serde_json::json!({}));
        if let Some(object) = features.as_object_mut() {
            object.insert(
                "quant_side".to_string(),
                self.quant_side
                    .clone()
                    .map(Value::from)
                    .unwrap_or(Value::Null),
            );
            object.insert(
                "quant_score".to_string(),
                self.quant_score.map(Value::from).unwrap_or(Value::Null),
            );
            object.insert("mark_price".to_string(), json!(self.mark_price));
        }
        let row = crate::signal_audit::AuditSignal::new(&self.symbol)
            .with_direction(direction)
            .with_score(self.signal_score())
            .with_features(features)
            .with_ai(ai_action, ai_confidence, ai_reason)
            .with_decision(decision)
            .with_reject_reason(reject_reason);
        if !self.quant_passed
            && self.preview_side.as_deref() == Some(direction)
            && decision == "rejected"
        {
            row.with_stage("evaluate")
        } else {
            row
        }
    }
}

struct ReviewedDraft {
    draft: EntryDraft,
    result: Result<crate::review::ReviewedEntry, crate::review::ReviewError>,
}

#[derive(Clone)]
pub(crate) struct LiveCreds {
    base: String,
    api_key: String,
    secret: String,
    recv_window_ms: i64,
}

impl RuntimeConfig {
    pub(crate) fn from_db(db: &Db) -> Self {
        let value = db.setting("engine").ok().flatten().unwrap_or(Value::Null);
        let interval = value["cycle_interval_sec"]
            .as_i64()
            .unwrap_or(MIN_INTERVAL_SEC)
            .clamp(MIN_INTERVAL_SEC, MAX_INTERVAL_SEC) as u64;
        let enabled = value["enabled"].as_bool().unwrap_or(false);
        Self {
            enabled,
            interval_sec: interval,
        }
    }
}

pub(crate) struct EngineRuntime {
    db: Db,
    _lock: File,
    master_key: Vec<u8>,
    /// 事件总线：账户、持仓、订单的变化从这里推给前端。
    events: crate::events::EventBus,
    cycle_signal: Option<Arc<Notify>>,
    cycle_busy: Arc<AtomicBool>,
    runtime_active: Arc<AtomicBool>,
    reviewer: Option<Arc<dyn crate::review::AiReviewer>>,
    /// 平仓后自进化复盘用；与 HTTP 层共用同一套供应商选择逻辑。
    ai: Option<crate::ai_providers::AiState>,
    /// 同一笔成交的复盘可能被平仓与补扫同时命中，进程内去重。
    reviewing: Arc<std::sync::Mutex<std::collections::HashSet<i64>>>,
    /// 七段延迟样本的异步上报通道；None = 执行内核未连接。
    exec_client: Option<Arc<crate::exec_client::ExecClient>>,
}

#[derive(Clone)]
pub(crate) struct EngineControl {
    db: Db,
    master_key: Vec<u8>,
    cycle_signal: Option<Arc<Notify>>,
    cycle_busy: Arc<AtomicBool>,
    runtime_active: Arc<AtomicBool>,
    gateway: Arc<dyn crate::live::Gateway>,
}

#[derive(Debug)]
pub(crate) enum TradeModeError {
    Credentials(String),
    Positions(Vec<String>),
    Exchange(String),
}

#[derive(Debug)]
pub(crate) enum ProtectError {
    NotFound,
    Credentials(String),
    Exchange(String),
    Failed(Value),
}

#[derive(Debug)]
pub(crate) enum CloseError {
    NotFound,
    Credentials(String),
    Exchange(String),
    Rejected(String),
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProtectionResult {
    pub status: u16,
    pub body: String,
    pub degraded: bool,
}

impl EngineControl {
    pub(crate) fn new(db: Db, master_key: Vec<u8>) -> Self {
        Self::with_cycle(
            db,
            master_key,
            None,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        )
    }

    pub(crate) fn with_cycle(
        db: Db,
        master_key: Vec<u8>,
        cycle_signal: Option<Arc<Notify>>,
        cycle_busy: Arc<AtomicBool>,
        runtime_active: Arc<AtomicBool>,
    ) -> Self {
        Self {
            db,
            master_key,
            cycle_signal,
            cycle_busy,
            runtime_active,
            gateway: Arc::new(crate::live::HttpGateway::new()),
        }
    }

    /// 注入自定义网关。只在测试里用假网关，生产路径固定用内置 HTTP 网关。
    #[cfg(test)]
    pub(crate) fn with_gateway(mut self, gateway: Arc<dyn crate::live::Gateway>) -> Self {
        self.gateway = gateway;
        self
    }

    pub(crate) fn is_running(&self) -> bool {
        self.runtime_active.load(Ordering::SeqCst)
    }

    pub(crate) fn trigger_cycle(&self) -> Result<i64, String> {
        if !self.runtime_active.load(Ordering::SeqCst) {
            return Err("引擎未运行".to_string());
        }
        if self.cycle_busy.load(Ordering::SeqCst) {
            return Err("周期正在进行".to_string());
        }
        let Some(signal) = &self.cycle_signal else {
            return Err("引擎未运行".to_string());
        };
        if self.mode() == "live" {
            self.live_credentials()
                .map_err(|error| format!("凭证不可用: {error}"))?;
        }
        signal.notify_one();
        Ok(crate::now())
    }

    async fn exchange_request(
        &self,
        creds: &LiveCreds,
        method: &str,
        path: &str,
        params: &[(String, String)],
    ) -> Result<(u16, String), String> {
        let gate = self.ban_gate();
        let now = crate::now_millis();
        let timestamp = crate::exchange_time::signed_timestamp(&creds.base).await;
        let request = crate::submit::prepare(
            &gate,
            now,
            method,
            &creds.base,
            path,
            params,
            &creds.api_key,
            &creds.secret,
            timestamp,
            creds.recv_window_ms,
        )?;
        let reply = self.gateway.send(&request).await;
        if reply.transport_failed {
            return Err("交易所连接失败".to_string());
        }
        crate::exchange_time::note_response(&creds.base, reply.status, &reply.body).await;
        Ok((reply.status, reply.body))
    }

    pub(crate) async fn trade_mode_status(&self) -> Result<Value, TradeModeError> {
        let creds = self
            .live_credentials()
            .map_err(TradeModeError::Credentials)?;
        let (status, body) = self
            .exchange_request(&creds, "GET", "/fapi/v1/positionSide/dual", &[])
            .await
            .map_err(TradeModeError::Exchange)?;
        if !(200..300).contains(&status) {
            return Err(TradeModeError::Exchange(format!(
                "交易所返回 HTTP {status}: {}",
                truncate_body(&body)
            )));
        }
        let dual = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|value| value["dualSidePosition"].as_bool())
            .ok_or_else(|| {
                TradeModeError::Exchange("交易所持仓模式响应缺少 dualSidePosition".to_string())
            })?;
        let (status, body) = self
            .exchange_request(&creds, "GET", "/fapi/v3/positionRisk", &[])
            .await
            .map_err(TradeModeError::Exchange)?;
        if !(200..300).contains(&status) {
            return Err(TradeModeError::Exchange(format!(
                "交易所持仓查询失败 HTTP {status}: {}",
                truncate_body(&body)
            )));
        }
        let rows = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default();
        let mut margin: std::collections::BTreeMap<String, String> =
            std::collections::BTreeMap::new();
        let mut with_positions = Vec::new();
        for row in rows {
            let Some(symbol) = row["symbol"].as_str() else {
                continue;
            };
            margin.insert(
                symbol.to_string(),
                row["marginType"]
                    .as_str()
                    .unwrap_or_default()
                    .to_lowercase(),
            );
            let amount = row["positionAmt"]
                .as_str()
                .and_then(|value| value.parse::<f64>().ok())
                .or_else(|| row["positionAmt"].as_f64())
                .unwrap_or(0.0);
            if amount.abs() > 1e-12 {
                with_positions.push(symbol.to_string());
            }
        }
        with_positions.sort();
        with_positions.dedup();
        let pool = self.pool_symbols();
        let pool = if pool.is_empty() {
            with_positions.clone()
        } else {
            pool
        };
        let need_isolated: Vec<String> = pool
            .iter()
            .filter(|symbol| {
                margin
                    .get(*symbol)
                    .map(|kind| kind == "cross")
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        let margin_known = pool
            .iter()
            .filter(|symbol| {
                matches!(
                    margin.get(*symbol).map(String::as_str),
                    Some("cross") | Some("isolated")
                )
            })
            .count();
        let can_switch = with_positions.is_empty();
        let hint = if dual && need_isolated.is_empty() {
            "已是双向逐仓".to_string()
        } else if !dual {
            "账户为单向持仓：请先开启双向持仓（需无持仓、无挂单）".to_string()
        } else if margin_known == 0 {
            "无持仓；开仓时会自动把该标的切为逐仓".to_string()
        } else {
            format!("{} 个标的仍是全仓，建议切为逐仓", need_isolated.len())
        };
        let margin_json: serde_json::Map<String, Value> = pool
            .iter()
            .map(|symbol| {
                (
                    symbol.clone(),
                    json!(margin
                        .get(symbol)
                        .cloned()
                        .unwrap_or_else(|| "unknown".to_string())),
                )
            })
            .collect();
        Ok(json!({
            "hedge_mode": dual,
            "need_hedge": !dual,
            "need_isolated": need_isolated,
            "margin_known": margin_known,
            "with_positions": with_positions,
            "can_switch": can_switch,
            "margin": margin_json,
            "hint": hint,
        }))
    }

    pub(crate) async fn switch_trade_mode(
        &self,
        hedge: bool,
        isolated: bool,
    ) -> Result<Value, TradeModeError> {
        let open = self.open_position_symbols();
        if !open.is_empty() {
            return Err(TradeModeError::Positions(open));
        }
        let status = self.trade_mode_status().await?;
        if let Some(positions) = status["with_positions"].as_array() {
            if !positions.is_empty() {
                let symbols = positions
                    .iter()
                    .filter_map(|value| value.as_str().map(str::to_string))
                    .collect();
                return Err(TradeModeError::Positions(symbols));
            }
        }
        let dual = status["hedge_mode"].as_bool().unwrap_or(false);
        let creds = self
            .live_credentials()
            .map_err(TradeModeError::Credentials)?;
        let mut results = Vec::new();
        let mut ok = true;
        if hedge {
            if dual {
                results.push(json!({"target": "hedge", "ok": true, "detail": "已是双向持仓"}));
            } else {
                let params = vec![("dualSidePosition".to_string(), "true".to_string())];
                match self
                    .exchange_request(&creds, "POST", "/fapi/v1/positionSide/dual", &params)
                    .await
                {
                    Ok((status, _)) if (200..300).contains(&status) => {
                        results.push(
                            json!({"target": "hedge", "ok": true, "detail": "已切换为双向持仓"}),
                        );
                    }
                    Ok((status, body)) => {
                        ok = false;
                        results.push(json!({"target": "hedge", "ok": false, "detail": format!("切换失败 HTTP {status}: {}", truncate_body(&body))}));
                    }
                    Err(error) => {
                        ok = false;
                        results.push(json!({"target": "hedge", "ok": false, "detail": error}));
                    }
                }
            }
        }
        if isolated {
            if hedge && !ok {
                results.push(json!({"target": "isolated", "ok": false, "detail": "上一步连接币安失败，已跳过逐仓切换以避免重复超时；稍后重试", "failed": []}));
            } else {
                let symbols = self.pool_symbols();
                let mut failed = Vec::new();
                for symbol in &symbols {
                    let params = vec![
                        ("symbol".to_string(), symbol.clone()),
                        ("marginType".to_string(), "ISOLATED".to_string()),
                    ];
                    match self
                        .exchange_request(&creds, "POST", "/fapi/v1/marginType", &params)
                        .await
                    {
                        Ok((status, _)) if (200..300).contains(&status) => {}
                        Ok((_, body)) if body.contains("-4046") => {}
                        Ok((status, body)) => {
                            failed.push(json!({"symbol": symbol, "detail": format!("HTTP {status}: {}", truncate_body(&body))}));
                        }
                        Err(error) => {
                            failed.push(json!({"symbol": symbol, "detail": error}));
                        }
                    }
                }
                if !failed.is_empty() {
                    ok = false;
                }
                results.push(json!({
                    "target": "isolated",
                    "ok": failed.is_empty(),
                    "detail": if failed.is_empty() { format!("{} 个标的已切/已是逐仓", symbols.len()) } else { format!("{} 个标的逐仓切换失败", failed.len()) },
                    "failed": failed,
                }));
            }
        }

        Ok(json!({"ok": ok, "results": results}))
    }

    pub(crate) async fn close_position(
        &self,
        position_id: i64,
        reason: &str,
    ) -> Result<Value, CloseError> {
        let position: Option<(String, String, String, f64, f64)> = {
            let conn = self
                .db
                .conn
                .lock()
                .map_err(|_| CloseError::Exchange("数据库锁不可用".to_string()))?;
            conn.query_row(
                "SELECT mode, symbol, side, qty, mark_price FROM positions WHERE id=?1 AND status='open'",
                [position_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, f64>(3)?,
                        row.get::<_, f64>(4)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| CloseError::Exchange(format!("读取仓位失败: {error}")))?
        };
        let Some((mode, symbol, side, qty, mark_price)) = position else {
            return Err(CloseError::NotFound);
        };
        let creds = self
            .credentials_for_mode(&mode)
            .map_err(CloseError::Credentials)?;
        let (cancelled, cancel_failures) = self
            .cancel_all_orders(&mode, &symbol)
            .await
            .map_err(CloseError::Exchange)?;
        if mode == "demo" {
            crate::execution::close_position(&self.db, position_id, mark_price, reason, false)
                .map_err(CloseError::Exchange)?;
            return Ok(json!({
                "ok": true,
                "id": position_id,
                "symbol": symbol,
                "mode": mode,
                "cancelled": cancelled,
                "cancel_failures": cancel_failures,
            }));
        }
        let dual = self
            .query_dual_side(&creds)
            .await
            .map_err(CloseError::Exchange)?;
        let filter = self.symbol_filter(&symbol);
        let close_side = if side == "long" { "SELL" } else { "BUY" };
        let mut params = vec![
            ("symbol".to_string(), symbol.clone()),
            ("side".to_string(), close_side.to_string()),
            ("type".to_string(), "MARKET".to_string()),
            ("quantity".to_string(), filter.format_qty(qty)),
            (
                "newClientOrderId".to_string(),
                crate::live::client_order_id("nx", "close", position_id as usize),
            ),
        ];
        if dual {
            // 双向持仓模式只认 positionSide；同时带 reduceOnly 会被交易所拒绝。
            params.push((
                "positionSide".to_string(),
                if side == "long" { "LONG" } else { "SHORT" }.to_string(),
            ));
        } else {
            params.push(("reduceOnly".to_string(), "true".to_string()));
        }
        params.push(("recvWindow".to_string(), creds.recv_window_ms.to_string()));
        let (status, body) = self
            .exchange_request(&creds, "POST", crate::live::ORDER_PATH, &params)
            .await
            .map_err(CloseError::Exchange)?;
        if !(200..300).contains(&status) {
            if (400..500).contains(&status) {
                return Err(CloseError::Rejected(format!(
                    "平仓拒单 HTTP {status}: {}",
                    truncate_body(&body)
                )));
            }
            return Err(CloseError::Unknown(format!(
                "平仓结果未知 HTTP {status}: {}",
                truncate_body(&body)
            )));
        }
        let avg = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|value| {
                value["avgPrice"]
                    .as_str()
                    .and_then(|text| text.parse::<f64>().ok())
                    .or_else(|| value["avgPrice"].as_f64())
            })
            .filter(|value| value.is_finite() && *value > 0.0)
            .unwrap_or(mark_price);
        crate::execution::close_position(&self.db, position_id, avg, reason, true)
            .map_err(CloseError::Exchange)?;
        Ok(json!({
            "ok": true,
            "id": position_id,
            "symbol": symbol,
            "mode": mode,
            "exit_price": avg,
            "cancelled": cancelled,
            "cancel_failures": cancel_failures,
        }))
    }

    pub(crate) async fn flatten(&self, stop_engine: bool) -> Result<Value, String> {
        let mode = self.mode();
        let rows: Vec<(i64, String, f64)> = {
            let conn = self
                .db
                .conn
                .lock()
                .map_err(|_| "数据库锁不可用".to_string())?;
            let mut stmt = conn
                .prepare("SELECT id, symbol, mark_price FROM positions WHERE status='open' AND mode=?1 ORDER BY id")
                .map_err(|error| error.to_string())?;
            let mapped = stmt
                .query_map([&mode], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                .map_err(|error| error.to_string())?;
            mapped
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())?
        };
        if stop_engine {
            if let Ok(conn) = self.db.conn.lock() {
                let _ = conn.execute(
                    "INSERT INTO engine_state(key,value,updated_at) VALUES('enabled','false',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
                    [crate::now()],
                );
            }
        }
        let mut closed = Vec::new();
        let mut failures = Vec::new();
        for (id, symbol, _mark_price) in rows {
            match self.close_position(id, "flatten").await {
                Ok(value) => closed.push(value),
                Err(error) => failures.push(json!({
                    "id": id,
                    "symbol": symbol,
                    "error": format!("{error:?}"),
                })),
            }
        }
        Ok(json!({
            "ok": failures.is_empty(),
            "mode": mode,
            "engine_stopped": stop_engine,
            "closed": closed,
            "failures": failures,
        }))
    }

    async fn query_dual_side(&self, creds: &LiveCreds) -> Result<bool, String> {
        let (status, body) = self
            .exchange_request(creds, "GET", "/fapi/v1/positionSide/dual", &[])
            .await?;
        if !(200..300).contains(&status) {
            return Err(format!(
                "无法读取交易所持仓模式 HTTP {status}: {}",
                truncate_body(&body)
            ));
        }
        serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|value| value["dualSidePosition"].as_bool())
            .ok_or_else(|| "交易所持仓模式响应缺少 dualSidePosition".to_string())
    }

    fn symbol_filter(&self, symbol: &str) -> crate::live::SymbolFilter {
        self.db
            .conn
            .lock()
            .ok()
            .and_then(|conn| {
                let symbols = vec![symbol.to_string()];
                crate::live::load_filters(&conn, &symbols)
                    .get(symbol)
                    .copied()
            })
            .unwrap_or_default()
    }

    pub(crate) async fn cancel_all_orders(
        &self,
        mode: &str,
        symbol: &str,
    ) -> Result<(usize, Vec<String>), String> {
        let creds = self.credentials_for_mode(mode)?;
        let mut cancelled = 0usize;
        let mut failures = Vec::new();
        let params = vec![("symbol".to_string(), symbol.to_string())];
        match self
            .exchange_request(&creds, "GET", "/fapi/v1/openOrders", &params)
            .await
        {
            Ok((status, body)) if (200..300).contains(&status) => {
                let rows = serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|value| value.as_array().cloned())
                    .unwrap_or_default();
                for row in rows {
                    let Some(order_id) = row["orderId"]
                        .as_i64()
                        .map(|value| value.to_string())
                        .or_else(|| row["orderId"].as_str().map(str::to_string))
                    else {
                        continue;
                    };
                    let params = vec![
                        ("symbol".to_string(), symbol.to_string()),
                        ("orderId".to_string(), order_id.clone()),
                    ];
                    match self
                        .exchange_request(&creds, "DELETE", "/fapi/v1/order", &params)
                        .await
                    {
                        Ok((status, _)) if (200..300).contains(&status) => cancelled += 1,
                        Ok((status, body)) => failures.push(format!(
                            "撤单 {order_id} 失败 HTTP {status}: {}",
                            truncate_body(&body)
                        )),
                        Err(error) => failures.push(format!("撤单 {order_id} 失败: {error}")),
                    }
                }
            }
            Ok((status, body)) => failures.push(format!(
                "读取普通挂单失败 HTTP {status}: {}",
                truncate_body(&body)
            )),
            Err(error) => failures.push(format!("读取普通挂单失败: {error}")),
        }
        match self
            .exchange_request(&creds, "GET", "/fapi/v1/openAlgoOrders", &params)
            .await
        {
            Ok((status, body)) if (200..300).contains(&status) => {
                let rows = serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|value| {
                        value
                            .as_array()
                            .cloned()
                            .or_else(|| value["orders"].as_array().cloned())
                            .or_else(|| value["data"].as_array().cloned())
                    })
                    .unwrap_or_default();
                for row in rows {
                    let Some(algo_id) = row["algoId"]
                        .as_i64()
                        .map(|value| value.to_string())
                        .or_else(|| row["algoId"].as_str().map(str::to_string))
                    else {
                        continue;
                    };
                    let params = vec![("algoId".to_string(), algo_id.clone())];
                    match self
                        .exchange_request(&creds, "DELETE", "/fapi/v1/algoOrder", &params)
                        .await
                    {
                        Ok((status, _)) if (200..300).contains(&status) => cancelled += 1,
                        Ok((status, body)) => failures.push(format!(
                            "撤保护单 {algo_id} 失败 HTTP {status}: {}",
                            truncate_body(&body)
                        )),
                        Err(error) => failures.push(format!("撤保护单 {algo_id} 失败: {error}")),
                    }
                }
            }
            Ok((status, body)) => failures.push(format!(
                "读取保护单失败 HTTP {status}: {}",
                truncate_body(&body)
            )),
            Err(error) => failures.push(format!("读取保护单失败: {error}")),
        }
        Ok((cancelled, failures))
    }

    pub(crate) async fn protect_position(&self, position_id: i64) -> Result<Value, ProtectError> {
        let position: Option<(String, String, String, f64, f64)> = {
            let conn = self
                .db
                .conn
                .lock()
                .map_err(|_| ProtectError::Exchange("数据库锁不可用".to_string()))?;
            conn.query_row(
                "SELECT symbol, side, mode, sl_price, tp_price FROM positions WHERE id=?1 AND status='open'",
                [position_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, f64>(3)?,
                        row.get::<_, f64>(4)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| ProtectError::Exchange(format!("读取仓位失败: {error}")))?
        };
        let Some((symbol, side, mode, sl_price, tp_price)) = position else {
            return Err(ProtectError::NotFound);
        };
        let creds = self
            .credentials_for_mode(&mode)
            .map_err(ProtectError::Credentials)?;
        let (status, body) = self
            .exchange_request(&creds, "GET", "/fapi/v1/positionSide/dual", &[])
            .await
            .map_err(ProtectError::Exchange)?;
        if !(200..300).contains(&status) {
            return Err(ProtectError::Exchange(format!(
                "无法读取交易所持仓模式 HTTP {status}: {}",
                truncate_body(&body)
            )));
        }
        let dual = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|value| value["dualSidePosition"].as_bool())
            .ok_or_else(|| {
                ProtectError::Exchange("交易所持仓模式响应缺少 dualSidePosition".to_string())
            })?;
        let filter = self
            .db
            .conn
            .lock()
            .ok()
            .and_then(|conn| {
                crate::live::load_filters(&conn, std::slice::from_ref(&symbol))
                    .get(&symbol)
                    .copied()
            })
            .unwrap_or_default();
        let legs = crate::live::protection_legs_mode(
            &symbol,
            &side,
            sl_price,
            tp_price,
            &filter,
            creds.recv_window_ms,
            position_id as usize,
            dual,
        );
        if legs.is_empty() {
            return Err(ProtectError::Failed(
                json!({"ok": false, "reason": "仓位缺少有效止损/止盈价"}),
            ));
        }
        let mut results = Vec::new();
        let mut all_ok = true;
        for leg in legs {
            let mut gate = self.ban_gate();
            let outcome = crate::live::place_protection_leg(
                &mut gate,
                self.gateway.as_ref(),
                &creds.base,
                &creds.api_key,
                &creds.secret,
                &leg,
            )
            .await;
            let ok = outcome.failure.is_none();
            if !ok {
                all_ok = false;
            }
            results.push(json!({
                "kind": leg.kind,
                "ok": ok,
                "unknown": outcome.unknown,
                "detail": outcome.failure,
            }));
        }
        if !all_ok {
            let reason = results
                .iter()
                .find_map(|item| item["detail"].as_str())
                .unwrap_or("补挂失败")
                .to_string();
            return Err(ProtectError::Failed(json!({
                "ok": false,
                "position_id": position_id,
                "symbol": symbol,
                "reason": reason,
                "results": results,
            })));
        }
        Ok(json!({
            "ok": true,
            "position_id": position_id,
            "symbol": symbol,
            "dual_side": dual,
            "results": results,
        }))
    }

    fn pool_symbols(&self) -> Vec<String> {
        self.db
            .conn
            .lock()
            .ok()
            .and_then(|conn| {
                let mut stmt = conn
                    .prepare("SELECT symbol FROM trading_symbols WHERE enabled=1 ORDER BY sort_order ASC, symbol ASC")
                    .ok()?;
                let rows = stmt.query_map([], |row| row.get::<_, String>(0)).ok()?;
                Some(rows.filter_map(Result::ok).collect())
            })
            .unwrap_or_default()
    }

    fn open_position_symbols(&self) -> Vec<String> {
        self.db
            .conn
            .lock()
            .ok()
            .and_then(|conn| {
                let mut stmt = conn
                    .prepare(
                        "SELECT DISTINCT symbol FROM positions WHERE status='open' ORDER BY symbol",
                    )
                    .ok()?;
                let rows = stmt.query_map([], |row| row.get::<_, String>(0)).ok()?;
                Some(rows.filter_map(Result::ok).collect())
            })
            .unwrap_or_default()
    }

    fn live_credentials(&self) -> Result<LiveCreds, String> {
        self.credentials_for_mode(&self.mode())
    }

    fn credentials_for_mode(&self, mode: &str) -> Result<LiveCreds, String> {
        let (api_key, secret) = self.exchange_credentials(mode)?;
        let engine = self
            .db
            .setting("engine")
            .map_err(|error| error.to_string())?
            .unwrap_or(Value::Null);
        Ok(LiveCreds {
            base: match mode {
                "demo" => "https://testnet.binancefuture.com".to_string(),
                _ => "https://fapi.binance.com".to_string(),
            },
            api_key,
            secret,
            recv_window_ms: engine["recv_window_ms"]
                .as_i64()
                .unwrap_or(5_000)
                .clamp(1_000, 60_000),
        })
    }

    fn mode(&self) -> String {
        self.db
            .conn
            .lock()
            .ok()
            .and_then(|conn| {
                conn.query_row(
                    "SELECT value FROM engine_state WHERE key='mode'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .ok()
            })
            .or_else(|| {
                self.db.setting("engine").ok().flatten().and_then(|engine| {
                    engine
                        .get("mode")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
            })
            .unwrap_or_else(|| "demo".to_string())
    }

    fn exchange_credentials(&self, mode: &str) -> Result<(String, String), String> {
        let data = self
            .db
            .setting("data")
            .map_err(|error| error.to_string())?
            .unwrap_or(Value::Null);
        let key =
            std::str::from_utf8(&self.master_key).map_err(|_| "主密钥格式无效".to_string())?;
        let decrypt = |name: &str| {
            let field = format!("{mode}_api_{name}_enc");
            let legacy = format!("api_{name}_enc");
            let encrypted = data.get(&field).or_else(|| {
                if mode == "live" {
                    data.get(&legacy)
                } else {
                    None
                }
            });
            crate::secret::decrypt_secret(key, encrypted.and_then(Value::as_str).unwrap_or(""))
        };
        let credentials =
            crate::sign::require_credentials(mode, &decrypt("key")?, &decrypt("secret")?)?;
        Ok((credentials.api_key, credentials.secret))
    }

    fn ban_gate(&self) -> crate::submit::BanGate {
        let until = self
            .db
            .conn
            .lock()
            .ok()
            .and_then(|conn| {
                conn.query_row(
                    "SELECT value FROM engine_state WHERE key='execution_ban_until'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .ok()
            })
            .and_then(|raw| raw.parse::<i64>().ok())
            .unwrap_or(0);
        crate::submit::BanGate::with_until(until)
    }

    fn save_ban_gate(&self, gate: &crate::submit::BanGate) {
        if let Ok(conn) = self.db.conn.lock() {
            let _ = upsert_state(&conn, "execution_ban_until", &gate.until_ms().to_string());
        }
    }

    pub(crate) async fn protection_request(
        &self,
        method: &str,
        path: &str,
        params: &[(String, String)],
    ) -> Result<ProtectionResult, String> {
        let mut gate = self.ban_gate();
        let credentials = self.live_credentials()?;
        let now = crate::now_millis();
        let timestamp = crate::exchange_time::signed_timestamp(&credentials.base).await;
        let request = crate::submit::prepare(
            &gate,
            now,
            method,
            &credentials.base,
            path,
            params,
            &credentials.api_key,
            &credentials.secret,
            timestamp,
            credentials.recv_window_ms,
        )?;
        let reply = self.gateway.send(&request).await;
        if reply.transport_failed {
            return Err("交易所请求传输失败".to_string());
        }
        let (status, body) = (reply.status, reply.body);
        crate::exchange_time::note_response(&credentials.base, status, &body).await;
        let before = gate.until_ms();
        gate.observe(&body, crate::now_millis());
        if (200..300).contains(&status) {
            gate.observe_success();
        }
        if gate.until_ms() != before {
            self.save_ban_gate(&gate);
        }
        let degraded = status == 429 || body.contains("\"code\":-1003");
        Ok(ProtectionResult {
            status,
            body,
            degraded,
        })
    }
}

impl EngineRuntime {
    pub(crate) fn manage_config(strategy: &Value, risk: &Value) -> crate::manage::ManageConfig {
        crate::manage::ManageConfig {
            partial_tp_at_r: strategy["partial_tp_at_r"].as_f64().unwrap_or(1.0),
            partial_tp_ratio: strategy["partial_tp_ratio"].as_f64().unwrap_or(0.5),
            trailing_pct: strategy["trailing_pct"].as_f64().unwrap_or(0.0),
            trailing_arm_r: strategy["trailing_arm_r"].as_f64().unwrap_or(1.5),
            exit_threshold: strategy["exit_threshold"].as_f64().unwrap_or(0.6),
            breakeven_at_r: strategy["breakeven_at_r"].as_f64().unwrap_or(1.0),
            global_stop_loss_pct: risk["global_stop_loss_pct"].as_f64().unwrap_or(10.0),
            global_take_profit_pct: risk["global_take_profit_pct"].as_f64().unwrap_or(20.0),
            qty_step: strategy["qty_step"].as_f64().unwrap_or(0.001),
            min_qty: strategy["min_qty"].as_f64().unwrap_or(0.001),
            roi_min_r: strategy["roi_min_r"].as_f64().unwrap_or(1.0),
            roi_table: Self::roi_table(&strategy["roi_table"]),
        }
    }

    fn roi_table(value: &Value) -> Vec<(f64, f64)> {
        let mut rows: Vec<(f64, f64)> = value
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|row| {
                        let minutes = row.get(0)?.as_f64()?;
                        let pct = row.get(1)?.as_f64()?;
                        (minutes.is_finite() && pct.is_finite()).then_some((minutes, pct))
                    })
                    .collect()
            })
            .unwrap_or_default();
        if rows.is_empty() {
            rows = vec![
                (0.0, 0.8),
                (15.0, 0.6),
                (30.0, 0.45),
                (60.0, 0.3),
                (120.0, 0.2),
            ];
        }
        rows.sort_by(|left, right| left.0.total_cmp(&right.0));
        rows
    }

    pub(crate) fn trade_policy(strategy: &Value) -> crate::trade_policy::TradePolicy {
        let num = |value: &Value, key: &str, default: f64| value[key].as_f64().unwrap_or(default);
        crate::trade_policy::TradePolicy {
            base_threshold: num(strategy, "entry_threshold", 0.3),
            volume_ratio_min: num(strategy, "volume_ratio_min", 0.8),
            atr_pct_min: num(strategy, "atr_pct_min", 0.05),
            atr_pct_max: num(strategy, "atr_pct_max", 4.0),

        }
    }
    fn engine_enabled(tx: &rusqlite::Transaction<'_>, config: &Value) -> Result<bool, String> {
        let configured = config.get("enabled").and_then(Value::as_bool);
        let raw: Option<String> = tx
            .query_row(
                "SELECT value FROM engine_state WHERE key='enabled'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        Ok(configured.unwrap_or(false) || raw.as_deref() == Some("true"))
    }

    fn engine_config(tx: &rusqlite::Transaction<'_>) -> Result<Value, String> {
        let raw: Option<String> = tx
            .query_row(
                "SELECT value FROM app_settings WHERE key='engine'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        raw.map(|text| serde_json::from_str(&text).map_err(|e| e.to_string()))
            .transpose()
            .map(|value| value.unwrap_or(Value::Null))
    }

    pub(crate) fn new(
        db: Db,
        lock: File,
        master_key: Vec<u8>,
        events: crate::events::EventBus,
    ) -> Self {
        let ai = Some(
            crate::ai_providers::AiState::new(db.clone(), master_key.clone()).with_market(
                Arc::new(crate::ai_market::BinanceMarketSource::new(db.clone())),
            ),
        );
        let reviewer = ai.as_ref().map(|state| {
            Arc::new(crate::ai_review::EngineAiReviewer::new(state.clone()))
                as Arc<dyn crate::review::AiReviewer>
        });
        Self {
            db,
            _lock: lock,
            master_key,
            events,
            cycle_signal: None,
            cycle_busy: Arc::new(AtomicBool::new(false)),
            runtime_active: Arc::new(AtomicBool::new(false)),
            reviewer,
            ai,
            reviewing: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            exec_client: None,
        }
    }

    /// 平仓事务提交后再触发复盘：失败只告警，不能把整轮周期带崩。
    fn spawn_reviews(&self, trade_ids: &[i64]) {
        let Some(ai) = self.ai.clone() else {
            return;
        };
        for trade_id in trade_ids {
            let ai = ai.clone();
            let reviewing = self.reviewing.clone();
            let id = *trade_id;
            tokio::spawn(async move {
                crate::evolution_review::review_trade_and_record(ai, reviewing, id).await;
            });
        }
    }

    /// 周期结束后按当前环境的标记价回访到期决策；5 分钟节流，缺价的保持 pending。
    fn maybe_label_journal(&self, mode: &str, snapshot: &crate::providers::TradingSnapshot) {
        let now = crate::now();
        {
            let Ok(conn) = self.db.conn.lock() else {
                return;
            };
            let last: Option<i64> = conn
                .query_row(
                    "SELECT value FROM engine_state WHERE key='journal_label_ts'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .ok()
                .and_then(|value| value.parse().ok());
            if last.is_some_and(|ts| now - ts < 300) {
                return;
            }
            let _ = conn.execute(
                "INSERT INTO engine_state(key,value,updated_at) VALUES('journal_label_ts',?1,?2) \
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
                (now.to_string(), now),
            );
        }
        let mut prices = std::collections::BTreeMap::new();
        for market in &snapshot.markets {
            if market.mark_price.is_finite() && market.mark_price > 0.0 {
                prices.insert(market.symbol.clone(), market.mark_price);
            }
        }
        if prices.is_empty() {
            return;
        }
        let result = crate::journal::label_due(&self.db, &prices, mode, now);
        let labeled = result["labeled"].as_i64().unwrap_or(0);
        if labeled > 0 {
            tracing::info!(mode, labeled, "决策回访打标完成");
        }
    }

    pub(crate) fn with_reviewer(mut self, reviewer: Arc<dyn crate::review::AiReviewer>) -> Self {
        self.reviewer = Some(reviewer);
        self
    }

    pub(crate) fn with_cycle(
        mut self,
        cycle_signal: Arc<Notify>,
        cycle_busy: Arc<AtomicBool>,
        runtime_active: Arc<AtomicBool>,
    ) -> Self {
        self.cycle_signal = Some(cycle_signal);
        self.cycle_busy = cycle_busy;
        self.runtime_active = runtime_active;
        self
    }

    pub(crate) fn with_exec_client(
        mut self,
        client: Arc<crate::exec_client::ExecClient>,
    ) -> Self {
        self.exec_client = Some(client);
        self
    }

    /// 把一段下单延迟样本投进有界上报队列；队列满由客户端计数丢弃，绝不阻塞下单。
    fn report_segments(&self, samples: &[(&'static str, f64)]) {
        if let Some(client) = &self.exec_client {
            for (segment, ms) in samples {
                client.record(segment, *ms);
            }
        }
    }

    fn record_equity_point(
        tx: &rusqlite::Transaction<'_>,
        mode: &str,
        snapshot: &crate::providers::TradingSnapshot,
    ) -> Result<(), String> {
        let now = crate::now();
        let last: Option<i64> = tx
            .query_row(
                "SELECT value FROM engine_state WHERE key='equity_point_ts'",
                [],
                |row| row.get::<_, String>(0),
            )
            .ok()
            .and_then(|value| value.parse().ok());
        if last.is_some_and(|ts| now - ts < 60) {
            return Ok(());
        }
        if !snapshot.account.equity.is_finite() || snapshot.account.equity <= 0.0 {
            return Ok(());
        }
        let margin_used: f64 = tx
            .query_row(
                "SELECT COALESCE(SUM(margin),0) FROM positions WHERE mode=?1 AND status='open'",
                [mode],
                |row| row.get(0),
            )
            .unwrap_or(0.0);
        let open_positions: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM positions WHERE mode=?1 AND status='open'",
                [mode],
                |row| row.get(0),
            )
            .unwrap_or(0);
        tx.execute(
            "INSERT INTO equity_points(ts,mode,wallet,available,unrealized,margin_used,positions) VALUES(?1,?2,?3,?4,0,?5,?6)",
            (now, mode, snapshot.account.equity, snapshot.account.available_balance, margin_used, open_positions),
        )
        .map_err(|error| error.to_string())?;
        upsert_state(tx, "equity_point_ts", &now.to_string())
    }

    fn persist_snapshot(
        &self,
        mode: &str,
        snapshot: &crate::providers::TradingSnapshot,
    ) -> Result<Vec<LivePlan>, String> {
        Self::validate_snapshot(mode, snapshot)?;
        let payload = serde_json::to_string(snapshot).map_err(|error| error.to_string())?;
        let strategy = self
            .db
            .setting("strategy")
            .map_err(|error| error.to_string())?
            .unwrap_or(Value::Null);
        let atr_period = match strategy.get("atr_period") {
            None => 14,
            Some(value) => value
                .as_u64()
                .filter(|period| (2..=100).contains(period))
                .ok_or_else(|| "策略 ATR 周期必须是 2 到 100 的整数".to_string())?
                as usize,
        };
        let metrics: std::collections::BTreeMap<_, _> = snapshot
            .markets
            .iter()
            .map(|market| {
                let metric = crate::providers::candle_metrics(&market.candles, atr_period)
                    .ok_or_else(|| format!("{} K 线不足或无法计算 ATR/量比", market.symbol))?;
                Ok::<_, String>((market.symbol.as_str(), metric))
            })
            .collect::<Result<_, _>>()?;
        let mut conn = self
            .db
            .conn
            .lock()
            .map_err(|_| "数据库锁不可用".to_string())?;
        let tx = conn.transaction().map_err(|error| error.to_string())?;
        upsert_state(&tx, &format!("snapshot.{mode}"), &payload)?;
        upsert_state(
            &tx,
            &format!("candle_metrics.{mode}"),
            &serde_json::to_string(&metrics).map_err(|error| error.to_string())?,
        )?;
        record_regimes(&tx, mode, &snapshot.markets, &metrics)?;
        upsert_state(
            &tx,
            &format!("snapshot_ts.{mode}"),
            &crate::now().to_string(),
        )?;
        if let Err(error) = Self::record_equity_point(&tx, mode, snapshot) {
            tracing::warn!(%error, "权益采样写入失败，不影响本轮周期");
        }
        upsert_state(
            &tx,
            &format!("open_orders.{mode}"),
            &serde_json::to_string(&snapshot.open_orders).map_err(|error| error.to_string())?,
        )?;
        let order_differences = Self::record_order_differences(&tx, mode, snapshot)?;
        Self::record_position_differences(&tx, mode, snapshot)?;
        // 交易所侧的止损/止盈被触发后，本地不能继续把这笔仓当成还在。
        let venue_closed = Self::close_missing_positions(&tx, mode, snapshot)?;
        let config = Self::engine_config(&tx)?;
        let engine_is_enabled = Self::engine_enabled(&tx, &config)?;
        let entry_cfg = Self::risk_settings(&tx)?;
        let cooldown_seconds = entry_cfg["symbol_cooldown_minutes"]
            .as_i64()
            .unwrap_or(5)
            .clamp(0, 1440)
            * 60;
        let global_cooldown_seconds = entry_cfg["cooldown_minutes"]
            .as_i64()
            .unwrap_or(10)
            .clamp(0, 1440)
            * 60;
        let _ = tx.execute("DELETE FROM cooldowns WHERE until_ts<=?1", [crate::now()]);
        let (
            mut preview_value,
            report,
            symbols,
            positions,
            rejected_rows,
            risk_halt,
        ) = Self::run_cycle_state(
            &tx,
            mode,
            &strategy,
            snapshot,
            &metrics,
            &order_differences,
            engine_is_enabled,
        )?;
        // 行情闸门就把标的挡掉的，本轮已经有结论了，先落审计：
        // 「为什么没开仓」最常见的一种答案就是它。
        if !rejected_rows.is_empty() {
            if let Err(error) = crate::signal_audit::record(&tx, mode, crate::now(), &rejected_rows)
            {
                tracing::warn!(%error, "信号审计写入失败，不影响本轮周期");
            }
        }
        let mut executed = Self::execute_demo_cycle(
            &tx,
            mode,
            &symbols,
            &positions,
            &report,
            snapshot,
            engine_is_enabled,
        )?;
        // 风控熔断且开了 close_on_halt：本轮只平一次，熔断解除前不重复全平。
        // 手动熔断与普通 skipped 不在此列（risk_halt 只由风控闸门置位）。
        let halt_key = format!("close_on_halt.{mode}");
        let halt_seen: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM engine_state WHERE key=?1)",
                [&halt_key],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        let close_on_halt = config["close_on_halt"].as_bool().unwrap_or(false);
        let halt_flatten = risk_halt && close_on_halt && !halt_seen;
        if halt_flatten {
            upsert_state(&tx, &halt_key, &crate::now().to_string())?;
            if mode == "demo" {
                let fee_rate = entry_cfg["fee_rate_taker"].as_f64().unwrap_or(0.0005);
                Self::flatten_demo_positions(&tx, mode, snapshot, fee_rate, &mut executed)?;
            } else {
                // 实盘平仓要撤单 + 发单，不能在事务里做；tick 提交后按这个标记执行。
                upsert_state(&tx, &format!("close_on_halt.pending.{mode}"), "1")?;
            }
        } else if !risk_halt && halt_seen {
            tx.execute("DELETE FROM engine_state WHERE key=?1", [&halt_key])
                .map_err(|error| error.to_string())?;
        }
        let closed_symbols: Vec<(i64, String, f64, i64)> = {
            let mut stmt = tx.prepare("SELECT id,symbol,realized_pnl,trade_id FROM positions WHERE mode=?1 AND closed_at>=?2 AND status='closed'").map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map((mode, crate::now() - 2), |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, i64>(3)?))
                })
                .map_err(|e| e.to_string())?;
            rows.collect::<Result<_, _>>().map_err(|e| e.to_string())?
        };
        // 平仓事务提交后再触发复盘；这里只收集本轮的成交 id。
        let review_ids: Vec<i64> = closed_symbols
            .iter()
            .filter_map(|(_, _, _, trade_id)| (*trade_id > 0).then_some(*trade_id))
            .collect();
        for (id, symbol, pnl, _) in closed_symbols {
            let seen: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM engine_state WHERE key=?1)",
                    [format!("cooldown_close.{mode}.{id}")],
                    |r| r.get(0),
                )
                .unwrap_or(false);
            if seen {
                continue;
            }
            if pnl < 0.0 && cooldown_seconds > 0 {
                tx.execute("INSERT INTO cooldowns(symbol,until_ts,reason,kind) VALUES(?1,?2,?3,'symbol') ON CONFLICT(symbol) DO UPDATE SET until_ts=excluded.until_ts,reason=excluded.reason,kind=excluded.kind WHERE cooldowns.until_ts<excluded.until_ts", (&symbol, crate::now() + cooldown_seconds, format!("本笔亏损 {pnl:.4} USDT"))).map_err(|e| e.to_string())?;
            } else if pnl >= 0.0 {
                tx.execute(
                    "DELETE FROM cooldowns WHERE symbol=?1 AND kind='symbol'",
                    [&symbol],
                )
                .map_err(|e| e.to_string())?;
            }
            // 全局冷却：亏损平仓后除标的自身冷却外，再对全部新开仓关门一段时间。
            // 盈利不写全局行，也不清掉既有的全局冷却（那段时间本来就该空仓）。
            if pnl < 0.0 && global_cooldown_seconds > 0 {
                tx.execute(
                    "INSERT INTO cooldowns(symbol,until_ts,reason,kind) VALUES(?1,?2,?3,'global') \
                     ON CONFLICT(symbol) DO UPDATE SET until_ts=excluded.until_ts,reason=excluded.reason,kind=excluded.kind \
                     WHERE cooldowns.until_ts<excluded.until_ts",
                    (
                        GLOBAL_COOLDOWN_SYMBOL,
                        crate::now() + global_cooldown_seconds,
                        format!("全局冷却: 本笔亏损 {pnl:.4} USDT"),
                    ),
                )
                .map_err(|e| e.to_string())?;
            }
            upsert_state(
                &tx,
                &format!("cooldown_close.{mode}.{id}"),
                &crate::now().to_string(),
            )?;
        }
        let drafts = if engine_is_enabled && !report.ai_symbols.is_empty() {
            Self::collect_entries(
                &tx,
                mode,
                &config,
                &strategy,
                snapshot,
                &metrics,
                &report.ai_symbols,
                &order_differences,
            )?
        } else {
            Vec::new()
        };
        // 实盘订单要离线发送：这里只给出计划，网络请求留给调用方在事务外做。
        let mut plans = if mode == "live" && engine_is_enabled {
            // 上一轮没拿到结论的平仓单还挂着：再发一张就是重复平仓。
            let inflight: std::collections::BTreeSet<String> = {
                let mut stmt = tx
                    .prepare(
                        // 只拦市价/限价管仓单：条件单是保护腿，不该阻止平仓。
                        "SELECT DISTINCT symbol FROM orders WHERE mode='live' \
                         AND status IN ('SUBMITTING','UNKNOWN','NEW','PARTIALLY_FILLED') \
                         AND order_type IN ('MARKET','LIMIT')",
                    )
                    .map_err(|error| error.to_string())?;
                let rows = stmt
                    .query_map([], |row| row.get::<_, String>(0))
                    .map_err(|error| error.to_string())?;
                rows.collect::<Result<_, _>>()
                    .map_err(|error| error.to_string())?
            };
            Self::live_plans(&symbols, &positions, &report, &inflight)
        } else {
            Vec::new()
        };
        tx.commit().map_err(|error| error.to_string())?;
        drop(conn);
        let reviewed = Self::review_drafts(self.reviewer.as_deref(), drafts, true);

        let mut conn = self
            .db
            .conn
            .lock()
            .map_err(|_| "数据库锁不可用".to_string())?;
        let tx = conn.transaction().map_err(|error| error.to_string())?;
        plans.extend(Self::apply_entries(
            &tx,
            mode,
            &config,
            snapshot,
            reviewed,
            &mut executed,
        )?);
        preview_value["engine_enabled"] = serde_json::json!(engine_is_enabled);
        preview_value["executed"] = serde_json::json!(executed);
        preview_value["venue_closed"] = serde_json::json!(venue_closed);
        preview_value["halt_flatten"] = serde_json::json!(halt_flatten);
        // 周期尾推进 regime 状态机并切换AI 调参；失败只告警，不阻断本轮。
        if let Err(error) = apply_regime_transition(&tx, mode) {
            tracing::warn!(%error, %mode, "regime 切换处理失败，不影响本轮周期");
        }
        // 自动启用 approved 候选并推进无候选守卫；失败只告警，不阻断本轮。
        let live_timeframe = strategy
            .get("timeframe")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if let Err(error) = apply_ai_tuning_governance(
            &tx,
            mode,
            &live_timeframe,
            !report.ai_symbols.is_empty(),
            !report.skipped.is_empty(),
            crate::now(),
        ) {
            tracing::warn!(%error, %mode, "AI 调参自动启用/无候选守卫失败，不影响本轮周期");
        }
        upsert_state(
            &tx,
            &format!("cycle_preview.{mode}"),
            &serde_json::to_string(&preview_value).map_err(|e| e.to_string())?,
        )?;
        tx.commit().map_err(|error| error.to_string())?;
        drop(conn);
        // 事务提交后再推事件：没落库的账不能出现在前端。
        self.publish_cycle_events(mode, snapshot, &executed, &order_differences);
        if halt_flatten {
            self.events.record(
                "warn",
                "risk",
                if mode == "demo" {
                    "风控熔断，已按 close_on_halt 平掉全部仓位"
                } else {
                    "风控熔断，已按 close_on_halt 请求平掉全部仓位"
                },
                json!({"mode": mode, "reason": preview_value["skipped"].clone()}),
            );
        }
        self.spawn_reviews(&review_ids);
        self.maybe_label_journal(mode, snapshot);
        Ok(plans)
    }

    /// 把本轮的账户、持仓、订单变化推给前端。
    fn publish_cycle_events(
        &self,
        mode: &str,
        snapshot: &crate::providers::TradingSnapshot,
        executed: &[Value],
        order_differences: &Value,
    ) {
        for item in executed {
            let symbol = item.get("symbol").and_then(Value::as_str).unwrap_or("");
            if symbol.is_empty() {
                continue;
            }
            let side = item.get("side").and_then(Value::as_str).unwrap_or("");
            let reason = item.get("reason").and_then(Value::as_str).unwrap_or("fill");
            self.events.record(
                "info",
                "trade",
                &format!("{symbol} {reason} {side}"),
                serde_json::json!({"mode": mode, "symbol": symbol, "side": side, "detail": item}),
            );
        }
        let local_only = order_differences
            .get("local_only")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0);
        let venue_only = order_differences
            .get("venue_only")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0);
        if local_only > 0 || venue_only > 0 {
            self.events.record(
                "warn",
                "engine",
                &format!("订单对账不一致：本地 {local_only} 条，交易所 {venue_only} 条"),
                order_differences.clone(),
            );
        }
        self.events.publish(
            "account",
            serde_json::json!({
                "mode": mode,
                "equity": snapshot.account.equity,
                "available_balance": snapshot.account.available_balance,
                "positions": snapshot.positions.len(),
                "open_orders": snapshot.open_orders.len(),
                "ts": crate::now(),
            }),
        );
        self.events.publish(
            "positions",
            serde_json::json!({"mode": mode, "positions": snapshot.positions}),
        );
        self.events.publish(
            "orders",
            serde_json::json!({"mode": mode, "orders": snapshot.open_orders}),
        );
    }

/// 有界并发映射，结果顺序与输入严格一致。
///
/// 只用本地线程池：扫描阶段已经拿到快照，指标计算是纯 CPU，
/// 不能在这里额外打交易所接口。并发度上限 20，避免池子被扫描线程占满。
fn map_bounded<T, R, F>(items: &[T], concurrency: usize, map: F) -> Vec<R>
where
    T: Sync,
    R: Send,
    F: Fn(&T) -> R + Sync,
{
    let workers = concurrency.clamp(1, 20).min(items.len());
    if workers <= 1 {
        return items.iter().map(&map).collect();
    }
    let chunk = items.len().div_ceil(workers);
    let mut slots: Vec<Option<R>> = (0..items.len()).map(|_| None).collect();
    std::thread::scope(|scope| {
        for (chunk_items, chunk_slots) in items.chunks(chunk).zip(slots.chunks_mut(chunk)) {
            let map = &map;
            scope.spawn(move || {
                for (item, slot) in chunk_items.iter().zip(chunk_slots.iter_mut()) {
                    *slot = Some(map(item));
                }
            });
        }
    });
    slots.into_iter().flatten().collect()
}

/// 限价开仓优先挂对手盘最优价：买单挂买一，卖单挂卖一。
/// 没有可信盘口（<=0 或非有限）时退回标记价，宁可挂不上也不追价。
fn limit_entry_price(side: &str, bid: f64, ask: f64, mark: f64) -> f64 {
    let book = if side == "BUY" { bid } else { ask };
    if book.is_finite() && book > 0.0 {
        book
    } else {
        mark
    }
}

/// 开仓成交费率：只做 Maker 的限价开仓用 maker 费率，其余（市价开仓、
/// 平仓、保护单）一律 taker，保证 positions/trades 的手续费口径一致。
fn plan_fee_rate(plan: &LivePlan, taker_rate: f64, maker_rate: f64) -> f64 {
    if plan.purpose == "entry" && plan.order_type.is_post_only() {
        maker_rate
    } else {
        taker_rate
    }
}

/// 实盘开仓单是否还挂着。未成交的限价单不会写 positions，
/// 只看仓位会重复挂单，所以必须回查订单表。
fn live_entry_inflight(
    tx: &rusqlite::Transaction<'_>,
    symbol: &str,
) -> Result<bool, String> {
    tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM orders WHERE mode='live' AND purpose='entry' AND symbol=?1 \
         AND status IN ('SUBMITTING','UNKNOWN','NEW','PARTIALLY_FILLED'))",
        [symbol],
        |row| row.get(0),
    )
    .map_err(|error| error.to_string())
}

/// 实盘要发的订单：只把管仓动作翻译成反向 reduce-only 市价单。
fn live_plans(
        symbols: &[String],
        positions: &[crate::manage::ManagedPosition],
        report: &crate::cycle::CycleReport,
        inflight: &std::collections::BTreeSet<String>,
    ) -> Vec<LivePlan> {
        symbols
            .iter()
            .zip(positions.iter())
            .zip(report.manage.iter())
            .filter(|((symbol, _), _)| !inflight.contains(*symbol))
            .filter_map(|((symbol, position), action)| {
                let (qty, purpose) = match action {
                    crate::manage::ManageAction::Close { reason } => (position.qty, reason.clone()),
                    crate::manage::ManageAction::PartialTakeProfit { qty } => {
                        (*qty, "partial_take_profit".to_string())
                    }
                    crate::manage::ManageAction::Hold
                    | crate::manage::ManageAction::MoveToBreakeven { .. } => return None,
                };
                if !position.qty.is_finite() || position.qty <= 0.0 {
                    return None;
                }
                Some(LivePlan {
                    symbol: symbol.clone(),
                    position_side: position.side.clone(),
                    side: if position.side == "long" {
                        "SELL".to_string()
                    } else {
                        "BUY".to_string()
                    },
                    qty,
                    purpose,
                    mark_price: position.mark_price,
                    order_type: LiveOrderType::Market,
                    price: None,
                    entry: None,
                    features: None,
                })
            })
            .collect()
    }

/// 扫描并发度：界面配置 1..20，0 或非法值回退 1。
    /// 并发只作用于本地指标/候选计算，不放大交易所请求。
    fn scan_concurrency_of(engine: &Value) -> usize {
        let parsed = engine
            .get("scan_concurrency")
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite() && *value >= 1.0)
            .map(|value| value.floor() as usize);
        parsed.unwrap_or(1).clamp(1, 20)
    }

    /// 扫描闸门配置。点差上限优先读界面字段 `max_spread_bps`，
    /// 兼容历史键 `spread_bps_max`，都为缺时按 8bp 兜底。
fn scan_gates_of(strategy: &Value) -> crate::scan::ScanGates {
    let spread_bps_max = strategy["max_spread_bps"]
        .as_f64()
        .or_else(|| strategy["spread_bps_max"].as_f64())
        .unwrap_or(8.0);
    crate::scan::ScanGates {
        atr_pct_min: strategy["atr_pct_min"].as_f64().unwrap_or(0.05),
        atr_pct_max: strategy["atr_pct_max"].as_f64().unwrap_or(4.0),
        volume_ratio_min: strategy["volume_ratio_min"].as_f64().unwrap_or(0.8),
        min_24h_quote_volume: strategy["min_24h_quote_volume"]
            .as_f64()
            .unwrap_or(3_000_000.0),
        spread_bps_max,
        entry_threshold: strategy["entry_threshold"].as_f64().unwrap_or(0.3),
    }
}

fn timeframe_ms(strategy: &Value) -> Result<i64, String> {
        let minutes: i64 = match strategy["timeframe"].as_str().unwrap_or("4h") {
            "1m" => 1,
            "3m" => 3,
            "5m" => 5,
            "15m" => 15,
            "30m" => 30,
            "1h" => 60,
            "2h" => 120,
            "4h" => 240,
            "6h" => 360,
            "8h" => 480,
            "12h" => 720,
            "1d" => 1_440,
            "3d" => 4_320,
            "1w" => 10_080,
            _ => return Err("策略 K 线周期无效".into()),
        };
        Ok(minutes * 60_000)
    }

    fn risk_settings(tx: &rusqlite::Transaction<'_>) -> Result<Value, String> {
        Ok(tx
            .query_row(
                "SELECT value FROM app_settings WHERE key='risk'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| error.to_string())?
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_else(|| crate::settings_api::defaults_of("risk")))
    }

    /// 装载可撮合的持仓。
    ///
    /// 三类别跳过：订单状态对不上的标的、没有行情快照的标的、K 线停更的标的。
    /// 停更行情上的标记价不是当前价，拿它触发止损等于赌运气。
    /// 顺带把本轮的标记价和最有利价格写回库，移动止损才有峰值可用。
    fn managed_positions(
        tx: &rusqlite::Transaction<'_>,
        mode: &str,
        strategy: &Value,
        snapshot: &crate::providers::TradingSnapshot,
        metrics: &std::collections::BTreeMap<&str, crate::providers::CandleMetrics>,

        blocked: &std::collections::BTreeSet<&str>,
        now_ms: i64,
        max_age_ms: f64,
    ) -> Result<(Vec<String>, Vec<crate::manage::ManagedPosition>), String> {
        let policy = Self::trade_policy(strategy);
        let rows = {
            let mut stmt = tx
                .prepare(
                    "SELECT id,symbol,side,qty,entry_price,sl_price,tp_price,peak_price,\
                     initial_sl_price,signal_score,partial_taken_at,opened_at,leverage FROM positions \
                     WHERE mode=?1 AND status='open' ORDER BY opened_at,id",
                )
                .map_err(|error| error.to_string())?;
            let rows = stmt
                .query_map([mode], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<f64>>(3)?.unwrap_or(0.0),
                        row.get::<_, Option<f64>>(4)?.unwrap_or(0.0),
                        row.get::<_, Option<f64>>(5)?.unwrap_or(0.0),
                        row.get::<_, Option<f64>>(6)?.unwrap_or(0.0),
                        row.get::<_, Option<f64>>(7)?.unwrap_or(0.0),
                        row.get::<_, Option<f64>>(8)?.unwrap_or(0.0),
                        row.get::<_, Option<f64>>(9)?.unwrap_or(0.0),
                        row.get::<_, Option<i64>>(10)?,
                        row.get::<_, Option<i64>>(11)?.unwrap_or(0),
                        row.get::<_, Option<f64>>(12)?.unwrap_or(1.0),
                    ))
                })
                .map_err(|error| error.to_string())?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())?
        };
        let mut symbols = Vec::new();
        let mut positions = Vec::new();
        for (
            id,
            symbol,
            side,
            qty,
            entry,
            sl,
            tp,
            peak,
            initial_sl,
            signal_score,
            partial,
            opened_at,
            leverage,
        ) in rows
        {
            if blocked.contains(symbol.as_str()) {
                continue;
            }
            let Some(market) = snapshot
                .markets
                .iter()
                .find(|market| market.symbol == symbol)
            else {
                continue;
            };
            let Some(last) = market.candles.last() else {
                continue;
            };
            let age_ms = now_ms.saturating_sub(last.close_time);
            if age_ms < 0 || age_ms as f64 > max_age_ms {
                continue;
            }
            let mark = market.mark_price;
            if !mark.is_finite() || mark <= 0.0 {
                continue;
            }
            let peak = if peak > 0.0 {
                if side == "long" {
                    peak.max(mark)
                } else {
                    peak.min(mark)
                }
            } else if side == "long" {
                entry.max(mark)
            } else {
                entry.min(mark)
            };
            tx.execute(
                "UPDATE positions SET mark_price=?1, peak_price=?2 WHERE id=?3",
                (mark, peak, id),
            )
            .map_err(|error| error.to_string())?;
            // 信号反转要看当轮方向分：开仓时的分数不会随行情翻转。
            let score = metrics
                .get(market.symbol.as_str())
                .and_then(|metric| {
                    crate::trade_policy::evaluate(
                        &market.candles,
                        mark,
                        metric.atr,
                        policy,
                        crate::trade_policy::MarketFeatures {
                            funding_rate: market.funding_rate,
                            open_interest_change_pct: market.open_interest_change_pct,
                            relative_strength: 0.0,
                            relative_strength_available: false,
                        },
                    )
                })
                .map_or(signal_score, |signal| signal.score);
            let held_minutes = (crate::now() - opened_at).max(0) as f64 / 60.0;
            symbols.push(symbol);
            positions.push(crate::manage::ManagedPosition {
                side,
                entry_price: entry,
                mark_price: mark,
                peak_price: peak,
                sl_price: sl,
                initial_sl: if initial_sl > 0.0 { initial_sl } else { sl },
                tp_price: tp,
                qty,
                leverage: leverage.max(1.0),
                partial_taken: partial.is_some(),
                score,
                held_minutes,
            });
        }
        Ok((symbols, positions))
    }

    /// 交易所已经不在的仓位：按标记价补记平仓。
    ///
    /// 保护单在交易所被触发时，引擎没走平仓流程；只把本地状态改成 closed
    /// 会让成交表和复盘漏掉这笔，所以走同一条结算路径补记。
    fn close_missing_positions(
        tx: &rusqlite::Transaction<'_>,
        mode: &str,
        snapshot: &crate::providers::TradingSnapshot,
    ) -> Result<Vec<Value>, String> {
        if mode != "live" {
            return Ok(Vec::new());
        }
        let venue: std::collections::BTreeSet<(&str, &str)> = snapshot
            .positions
            .iter()
            .filter(|position| position.qty > 0.0)
            .map(|position| (position.symbol.as_str(), position.side.as_str()))
            .collect();
        let locals: Vec<(String, String, f64)> = {
            let mut stmt = tx
                .prepare(
                    "SELECT symbol,side,COALESCE(mark_price,0) FROM positions \
                     WHERE mode='live' AND status='open'",
                )
                .map_err(|error| error.to_string())?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<f64>>(2)?.unwrap_or(0.0),
                    ))
                })
                .map_err(|error| error.to_string())?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())?
        };
        if locals.is_empty() {
            return Ok(Vec::new());
        }
        let fee_rate = Self::risk_settings(tx)?["fee_rate_taker"]
            .as_f64()
            .unwrap_or(0.0005);
        let mut closed = Vec::new();
        for (symbol, side, stored_mark) in locals {
            if venue.contains(&(symbol.as_str(), side.as_str())) {
                continue;
            }
            let mark = snapshot
                .markets
                .iter()
                .find(|market| market.symbol == symbol)
                .map(|market| market.mark_price)
                .filter(|price| price.is_finite() && *price > 0.0)
                .unwrap_or(stored_mark);
            // 没有可信价格就不动账本：宁可下一轮再对，也不能写一个假出场价。
            if !(mark.is_finite() && mark > 0.0) {
                continue;
            }
            if crate::execution::close_venue_position_tx(
                tx,
                "live",
                crate::now(),
                fee_rate,
                &symbol,
                &side,
                mark,
            )? {
                closed.push(serde_json::json!({
                    "symbol": symbol,
                    "side": side,
                    "exit_price": mark,
                    "reason": "venue_sync",
                }));
            }
        }
        Ok(closed)
    }

    /// 快速对账：只拉交易所持仓，关闭本地已不存在的实盘仓位。
    ///
    /// 用户可能在币安 App 手动平仓，不经过引擎；主周期默认 600 秒，
    /// 这里每 15 秒对一次，避免总览长时间显示幽灵仓位。
    async fn sync_venue_positions(&self) -> Result<Vec<Value>, String> {
        let mode = self
            .db
            .conn
            .lock()
            .ok()
            .and_then(|conn| {
                conn.query_row(
                    "SELECT value FROM engine_state WHERE key='mode'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .ok()
                .filter(|mode| !mode.is_empty())
            })
            .or_else(|| {
                self.db
                    .setting("engine")
                    .ok()
                    .flatten()
                    .and_then(|engine| engine["mode"].as_str().map(str::to_string))
            })
            .unwrap_or_else(|| "demo".to_string());
        if mode != "live" {
            return Ok(Vec::new());
        }
        let creds = self.live_credentials()?;
        let locals: Vec<(String, String, f64)> = {
            let conn = self
                .db
                .conn
                .lock()
                .map_err(|_| "数据库锁不可用".to_string())?;
            let mut stmt = conn
                .prepare(
                    "SELECT symbol, side, COALESCE(mark_price,0) FROM positions \
                     WHERE mode='live' AND status='open'",
                )
                .map_err(|error| error.to_string())?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<f64>>(2)?.unwrap_or(0.0),
                    ))
                })
                .map_err(|error| error.to_string())?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())?
        };
        if locals.is_empty() {
            return Ok(Vec::new());
        }
        let symbols: Vec<String> = locals
            .iter()
            .map(|(symbol, _, _)| symbol.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let provider = crate::providers::BinanceProvider::with_symbols(
            &creds.base,
            symbols,
            creds.api_key.clone(),
            creds.secret.clone(),
        )
        .map_err(|error| match error {
            crate::providers::ProviderError::Timeout => "交易所持仓快照超时".to_string(),
            crate::providers::ProviderError::Unavailable(message)
            | crate::providers::ProviderError::InvalidData(message) => message,
        })?;
        let venue = provider
            .positions_only()
            .await
            .map_err(|error| match error {
                crate::providers::ProviderError::Timeout => "交易所持仓快照超时".to_string(),
                crate::providers::ProviderError::Unavailable(message)
                | crate::providers::ProviderError::InvalidData(message) => message,
            })?;
        let venue_keys: std::collections::BTreeSet<(&str, &str)> = venue
            .iter()
            .filter(|position| position.qty > 0.0)
            .map(|position| (position.symbol.as_str(), position.side.as_str()))
            .collect();
        let fee_rate = self
            .db
            .setting("risk")
            .ok()
            .flatten()
            .and_then(|risk| risk["fee_rate_taker"].as_f64())
            .unwrap_or(0.0005);
        let mut closed = Vec::new();
        for (symbol, side, stored_mark) in locals {
            if venue_keys.contains(&(symbol.as_str(), side.as_str())) {
                continue;
            }
            let mark = provider
                .mark_price(&symbol)
                .await
                .ok()
                .filter(|price| price.is_finite() && *price > 0.0)
                .unwrap_or(stored_mark);
            if !(mark.is_finite() && mark > 0.0) {
                continue;
            }
            let mut conn = self
                .db
                .conn
                .lock()
                .map_err(|_| "数据库锁不可用".to_string())?;
            let tx = conn.transaction().map_err(|error| error.to_string())?;
            if crate::execution::close_venue_position_tx(
                &tx,
                "live",
                crate::now(),
                fee_rate,
                &symbol,
                &side,
                mark,
            )? {
                tx.commit().map_err(|error| error.to_string())?;
                closed.push(serde_json::json!({
                    "symbol": symbol,
                    "side": side,
                    "exit_price": mark,
                    "reason": "venue_sync",
                }));
            }
        }
        if !closed.is_empty() {
            self.events.record(
                "warn",
                "engine",
                "交易所已平仓，已同步关闭本地仓位",
                serde_json::json!({ "closed": closed }),
            );
        }
        Ok(closed)
    }

    /// 一轮调度：对账、管仓、时段与扫描全部走 `cycle::run_cycle`。
    ///
    /// 这里只产出审计结果，不落订单。返回的预览已经是可直接序列化的 JSON，
    /// 避免调用方再解析一次字符串。
    fn run_cycle_state(
        tx: &rusqlite::Transaction<'_>,
        mode: &str,
        strategy: &Value,
        snapshot: &crate::providers::TradingSnapshot,
        metrics: &std::collections::BTreeMap<&str, crate::providers::CandleMetrics>,
        order_differences: &Value,
        engine_enabled: bool,
    ) -> Result<
        (
            Value,
            crate::cycle::CycleReport,
            Vec<String>,
            Vec<crate::manage::ManagedPosition>,
            Vec<crate::signal_audit::AuditSignal>,
            bool,
        ),
        String,
    > {
        let read = |sql: &str, key: &str| -> Result<Option<String>, String> {
            tx.query_row(sql, [key], |row| row.get(0))
                .optional()
                .map_err(|error| error.to_string())
        };
        let setting = |key| -> Result<Value, String> {
            read("SELECT value FROM app_settings WHERE key=?1", key)?
                .map(|raw| serde_json::from_str(&raw).map_err(|error| error.to_string()))
                .transpose()
                .map(|value| value.unwrap_or(Value::Null))
        };
        let state = |key| read("SELECT value FROM engine_state WHERE key=?1", key);
        let engine = setting("engine")?;
        let risk_cfg = Self::risk_settings(tx)?;
        let now_ms = crate::now_millis();
        let now = now_ms / 1_000;
        let local_now = now
            + engine["timezone_offset"]
                .as_i64()
                .unwrap_or(8)
                .clamp(-12, 14)
                * 3_600;
        let today: String = tx
            .query_row("SELECT date(?1, 'unixepoch')", [local_now], |row| {
                row.get(0)
            })
            .map_err(|error| error.to_string())?;
        let equity_state = |key| -> Result<f64, String> {
            Ok(state(key)?
                .and_then(|raw| raw.parse::<f64>().ok())
                .filter(|v| v.is_finite() && *v > 0.0)
                .unwrap_or(0.0))
        };
        let int_state = |key| -> Result<i64, String> {
            Ok(state(key)?
                .and_then(|raw| raw.parse::<i64>().ok())
                .unwrap_or(0))
        };
        let risk = crate::risk::RiskInput {
            equity: snapshot.account.equity,
            day_start_equity: if state("day_anchor")?.as_deref() == Some(today.as_str()) {
                equity_state("day_start_equity")?
            } else {
                0.0
            },
            peak_equity: equity_state("equity_peak")?,
            min_equity: risk_cfg["min_equity_usdt"].as_f64().unwrap_or(0.5),
            daily_loss_limit_pct: risk_cfg["daily_loss_limit_pct"].as_f64().unwrap_or(25.0),
            max_drawdown_pct: risk_cfg["max_drawdown_pct"].as_f64().unwrap_or(50.0),
            consecutive_losses: int_state("consecutive_losses")?,
            consecutive_loss_limit: risk_cfg["consecutive_loss_limit"].as_i64().unwrap_or(4),
            cooldown_until: int_state("loss_halt_until")?,
            now,
            halted: state("halted")?.unwrap_or_default(),
        };
        let gates = Self::scan_gates_of(strategy);
        let interval_ms = Self::timeframe_ms(strategy)?;
        let max_age_ms = interval_ms as f64
            * strategy["data_stale_mult"]
                .as_f64()
                .unwrap_or(2.0)
                .clamp(1.0, 20.0);
        let max_gaps = strategy["max_data_gaps"].as_u64().unwrap_or(1).min(20) as usize;
        let relative_strength: std::collections::BTreeMap<String, f64> = {
            let mut returns = Vec::new();
            for market in &snapshot.markets {
                if market.candles.len() < 21 {
                    continue;
                }
                let last = market.candles.last().map(|candle| candle.close);
                let previous = market.candles[market.candles.len() - 21].close;
                if let Some(last) = last.filter(|value| value.is_finite() && previous > 0.0) {
                    returns.push((market.symbol.clone(), last / previous - 1.0));
                }
            }
            let mean = if returns.is_empty() {
                0.0
            } else {
                returns.iter().map(|(_, value)| value).sum::<f64>() / returns.len() as f64
            };
            let sd = if returns.is_empty() {
                0.0
            } else {
                (returns
                    .iter()
                    .map(|(_, value)| (value - mean).powi(2))
                    .sum::<f64>()
                    / returns.len() as f64)
                    .sqrt()
            };
            returns
                .into_iter()
                .map(|(symbol, value)| {
                    (
                        symbol,
                        if sd > 0.0 {
                            ((value - mean) / sd).clamp(-1.0, 1.0)
                        } else {
                            0.0
                        },
                    )
                })
                .collect()
        };
        let concurrency = Self::scan_concurrency_of(&engine);
        let candidates: Vec<_> = Self::map_bounded(&snapshot.markets, concurrency, |market| {
            let metric = &metrics[market.symbol.as_str()];
            let recent = &market.candles[market.candles.len() - 20..];
            let fresh = recent.last().is_some_and(|candle| {
                let age = now_ms.saturating_sub(candle.close_time);
                age > 0 && age as f64 <= max_age_ms
            });
            let gaps = recent
                .windows(2)
                .filter(|pair| {
                    pair[1].close_time.saturating_sub(pair[0].close_time) != interval_ms
                })
                .count();
            let preview = crate::trade_policy::preview(
                &market.candles,
                market.mark_price,
                metric.atr,
                crate::trade_policy::MarketFeatures {
                    funding_rate: market.funding_rate,
                    open_interest_change_pct: market.open_interest_change_pct,
                    relative_strength: relative_strength
                        .get(&market.symbol)
                        .copied()
                        .unwrap_or(0.0),
                    relative_strength_available: relative_strength.contains_key(&market.symbol),
                },
            );
            let (strength, preview_side) = match preview {
                Some(preview) => (preview.score.abs(), Some(preview.side)),
                None => (0.0, None),
            };
            crate::scan::Candidate {
                symbol: market.symbol.clone(),
                data_ok: fresh && gaps <= max_gaps,
                atr_pct: metric.atr_pct,
                vol_ratio: metric.vol_ratio,
                quote_volume: market.quote_volume,
                spread_bps: market.spread_bps,
                strength,
                preview_side,
            }
        });
        let windows: Vec<String> = match engine.get("trading_windows") {
            None => vec!["00:00-24:00".into()],
            Some(value) => {
                serde_json::from_value(value.clone()).map_err(|_| "交易时段配置无效".to_string())?
            }
        };
        let blocked: std::collections::BTreeSet<&str> = ["local_only", "venue_only"]
            .iter()
            .flat_map(|key| order_differences[*key].as_array().into_iter().flatten())
            .filter_map(|order| order["symbol"].as_str())
            .collect();
        let (symbols, positions) = Self::managed_positions(
            tx, mode, strategy, snapshot, metrics, &blocked, now_ms, max_age_ms,
        )?;
        let local: Vec<crate::reconcile::LocalPosition> = {
            let mut stmt = tx
                .prepare("SELECT symbol,mode,side,qty,entry_price FROM positions WHERE mode=?1 AND status='open'")
                .map_err(|error| error.to_string())?;
            let rows = stmt
                .query_map([mode], |row| {
                    Ok(crate::reconcile::LocalPosition {
                        symbol: row.get(0)?,
                        mode: row.get(1)?,
                        side: row.get(2)?,
                        qty: row.get::<_, Option<f64>>(3)?.unwrap_or(0.0),
                        entry_price: row.get::<_, Option<f64>>(4)?.unwrap_or(0.0),
                    })
                })
                .map_err(|error| error.to_string())?;
            // 缺开仓价就算不出盈亏，硬凑一个 0 会在对账里造出假数字。
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())?
                .into_iter()
                .filter(|position| position.qty > 0.0 && position.entry_price > 0.0)
                .collect()
        };
        let venue: Vec<_> = snapshot
            .positions
            .iter()
            .filter(|position| position.qty > 0.0)
            .map(|position| crate::reconcile::VenuePosition {
                symbol: position.symbol.clone(),
                side: position.side.clone(),
                qty: position.qty,
                mark_price: snapshot
                    .markets
                    .iter()
                    .find(|market| market.symbol == position.symbol)
                    .map_or(position.entry_price, |market| market.mark_price),
                sl_price: 0.0,
                tp_price: 0.0,
            })
            .collect();
        let exit_prices: Vec<(String, f64)> = snapshot
            .markets
            .iter()
            .map(|market| (market.symbol.clone(), market.mark_price))
            .collect();
        let mut report = crate::cycle::run_cycle(&crate::cycle::CycleInput {
            enabled: engine_enabled,
            mode,
            now_minute: local_now.rem_euclid(86_400) / 60,
            windows: &windows,
            risk: &risk,
            venue: &venue,
            local: &local,
            fee_rate: risk_cfg["fee_rate_taker"].as_f64().unwrap_or(0.0005),
            exit_prices: &exit_prices,
            closing: &[],
            positions: &positions,
            manage_cfg: &Self::manage_config(strategy, &risk_cfg),
            candidates: &candidates,
            scan_gates: &gates,
            budget: engine["max_symbols_per_cycle"].as_u64().unwrap_or(12) as usize,
        });
        // 止损保护是独立状态，不属于 risk 输入；熔断期间的亏损仓位仍要管，
        // 所以只拦扫描，不拦管仓。
        let risk_gate = crate::risk::global_gates(&risk);
        let sl_guard_active = int_state("sl_guard_until")? > now;
        if report.skipped.is_empty() && sl_guard_active {
            report.skipped = "止损熔断中".into();
            report.scanned.clear();
            report.ai_symbols.clear();
        }
        // close_on_halt 只认风控熔断（日亏/回撤/连亏/止损），手动熔断与普通 skipped 不算。
        let risk_halt =
            engine_enabled && (sl_guard_active || (risk_gate.risk_halt && !risk_gate.ok));
        let rejected: std::collections::BTreeMap<_, _> = candidates
            .iter()
            .filter_map(|candidate| {
                let reason = crate::scan::risk_gate(candidate, &gates);
                (!reason.is_empty()).then_some((&candidate.symbol, reason))
            })
            .collect();
        // 没过行情闸门的标的也要进审计：这是「为什么没开仓」最常见的一种答案。
        let rejected_rows: Vec<crate::signal_audit::AuditSignal> = candidates
            .iter()
            .filter_map(|candidate| {
                let reason = crate::scan::risk_gate(candidate, &gates);
                if reason.is_empty() {
                    return None;
                }
                Some(
                    crate::signal_audit::AuditSignal::new(&candidate.symbol)
                        .with_direction(candidate.preview_side.unwrap_or("flat"))
                        .with_score(candidate.strength)
                        .with_stage("gate")
                        .with_features(serde_json::json!({
                            "atr_pct": candidate.atr_pct,
                            "vol_ratio": candidate.vol_ratio,
                            "quote_volume": candidate.quote_volume,
                            "spread_bps": candidate.spread_bps,
                        }))
                        .with_decision("rejected")
                        .with_reject_reason(&reason),
                )
            })
            .collect();
        let preview = serde_json::json!({
            "skipped": &report.skipped,
            "scan_candidates": &report.scanned,
            "ai_symbols": &report.ai_symbols,
            "rejected": rejected,
            "reconcile": &report.reconcile,
        });
        Ok((
            preview,
            report,
            symbols,
            positions,
            rejected_rows,
            risk_halt,
        ))
    }

    fn collect_entries(
        tx: &rusqlite::Transaction<'_>,
        mode: &str,
        _engine: &Value,
        strategy: &Value,
        snapshot: &crate::providers::TradingSnapshot,
        metrics: &std::collections::BTreeMap<&str, crate::providers::CandleMetrics>,
        scanned: &[String],
        differences: &Value,
    ) -> Result<Vec<EntryDraft>, String> {
        if mode == "live" {
            let pending: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM orders WHERE mode='live' AND purpose='entry' AND status NOT IN ('FILLED','REJECTED','CANCELED','EXPIRED'))",
                [], |row| row.get(0),
            ).map_err(|e| e.to_string())?;
            if pending
                || ["local_only", "venue_only", "quantity_mismatches"]
                    .iter()
                    .any(|key| {
                        differences[*key]
                            .as_array()
                            .is_some_and(|rows| !rows.is_empty())
                    })
            {
                return Ok(Vec::new());
            }
            for position in &snapshot.positions {
                let matched: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM positions WHERE mode='live' AND status='open' AND symbol=?1 AND side=?2 AND abs(qty-?3)<1e-9)",
                    (&position.symbol, &position.side, position.qty), |row| row.get(0),
                ).map_err(|e| e.to_string())?;
                if !matched {
                    return Ok(Vec::new());
                }
            }
        }
        let cfg = Self::risk_settings(tx)?;
        let policy = Self::trade_policy(strategy);
        let filters = crate::live::load_filters(tx, scanned);
        let mut returns = Vec::new();
        for symbol in scanned {
            let Some(market) = snapshot
                .markets
                .iter()
                .find(|market| market.symbol == *symbol)
            else {
                continue;
            };
            if market.candles.len() < 21 {
                continue;
            }
            let last = market.candles.last().map(|candle| candle.close);
            let previous = market.candles[market.candles.len() - 21].close;
            if let Some(last) = last.filter(|value| value.is_finite() && previous > 0.0) {
                returns.push((symbol.clone(), last / previous - 1.0));
            }
        }
        let mean = if returns.is_empty() {
            0.0
        } else {
            returns.iter().map(|(_, value)| value).sum::<f64>() / returns.len() as f64
        };
        let sd = if returns.is_empty() {
            0.0
        } else {
            (returns
                .iter()
                .map(|(_, value)| (value - mean).powi(2))
                .sum::<f64>()
                / returns.len() as f64)
                .sqrt()
        };
        let relative_strength: std::collections::BTreeMap<String, f64> = returns
            .iter()
            .map(|(symbol, value)| {
                (
                    symbol.clone(),
                    if sd > 0.0 {
                        ((value - mean) / sd).clamp(-1.0, 1.0)
                    } else {
                        0.0
                    },
                )
            })
            .collect();

        let mut drafts = Vec::new();
        for symbol in scanned {
            let Some(filter) = filters.get(symbol) else {
                continue;
            };
            if ["local_only", "venue_only"].iter().any(|key| {
                differences[*key]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|order| order["symbol"].as_str() == Some(symbol))
            }) {
                continue;
            }
            let Some(market) = snapshot
                .markets
                .iter()
                .find(|market| market.symbol == *symbol)
            else {
                continue;
            };
            let Some(metric) = metrics.get(symbol.as_str()) else {
                continue;
            };
            let price = market.mark_price;
            let features = crate::trade_policy::MarketFeatures {
                funding_rate: market.funding_rate,
                open_interest_change_pct: market.open_interest_change_pct,
                relative_strength: relative_strength.get(symbol).copied().unwrap_or(0.0),
                relative_strength_available: relative_strength.contains_key(symbol),
            };
            let signal = crate::trade_policy::evaluate(
                &market.candles,
                price,
                metric.atr,
                policy,
                features,
            );
            let (
                side,
                score,
                quant_side,
                quant_score,
                preview_side,
                preview_score,
                quant_passed,
            ) = match signal {
                Some(signal) => (
                    signal.side.to_string(),
                    signal.score,
                    Some(signal.side.to_string()),
                    Some(signal.score),
                    Some(signal.side.to_string()),
                    Some(signal.score),
                    true,
                ),
                None => {
                    let preview = crate::trade_policy::preview(
                        &market.candles,
                        price,
                        metric.atr,
                        features,
                    );
                    let preview_side = preview
                        .filter(|item| item.side != "flat")
                        .map(|item| item.side.to_string());
                    let preview_score = preview.map(|item| item.score);
                    (
                        "long".to_string(),
                        0.0,
                        None,
                        None,
                        preview_side,
                        preview_score,
                        false,
                    )
                }
            };
            let Some(exchange_max): Option<Option<i64>> = tx
                .query_row(
                    "SELECT max_leverage FROM trading_symbols WHERE symbol=?1 AND enabled!=0",
                    [symbol],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|e| e.to_string())?
            else {
                continue;
            };
            let upper = cfg["max_leverage"]
                .as_i64()
                .unwrap_or(100)
                .min(exchange_max.filter(|v| *v > 0).unwrap_or(100))
                .clamp(1, 100);
            let lower = cfg["min_leverage"].as_i64().unwrap_or(1).clamp(1, upper);
            // 杠杆由 AI 在 [lower, upper] 内决定；这里只给出回退值（下限）。
            let leverage = lower as f64;
            // 仓位止盈止损率是用户的心理预期（保证金收益率）：
            // 作为 AI 未给出止损/止盈价时的默认价，价格距离 = 保证金% ÷ 杠杆。
            let direction = if side == "long" { 1.0 } else { -1.0 };
            let global_stop = cfg["global_stop_loss_pct"].as_f64().unwrap_or(10.0);
            let global_take_profit = cfg["global_take_profit_pct"].as_f64().unwrap_or(20.0);
            let stop_price = if global_stop > 0.0 {
                price * (1.0 - direction * global_stop / leverage / 100.0)
            } else {
                0.0
            };
            let take_profit = if global_take_profit > 0.0 {
                price * (1.0 + direction * global_take_profit / leverage / 100.0)
            } else {
                0.0
            };
            let target = cfg["vol_target_atr_pct"].as_f64().unwrap_or(0.0);
            let scale = if target > 0.0 && metric.atr_pct > 0.0 {
                (target / metric.atr_pct).clamp(0.25, 2.0)
            } else {
                1.0
            };
            let journal_snapshot = serde_json::json!({
                "atr_pct": metric.atr_pct,
                "funding": market.funding_rate,
                "oi_change_pct": market.open_interest_change_pct,
            });
            let evolution = crate::evolution_api::evolution_cfg(tx);
            let (_mantra_text, injected_mantra_ids) =
                crate::evolution_api::injection_payload(tx, &evolution);
            let injected_experience_ids =
                crate::journal::inject_ids(tx, symbol, &journal_snapshot, &side);
            let features_text = serde_json::to_string(&serde_json::json!({
                "injected_mantra_ids": injected_mantra_ids,
                "injected_experience_ids": injected_experience_ids,
                "atr_pct": metric.atr_pct,
                "vol_ratio": metric.vol_ratio,
                "quote_volume": market.quote_volume,
                "spread_bps": market.spread_bps,
                "journal_snapshot": journal_snapshot,
                "quant_side": quant_side,
                "quant_score": quant_score,
                "preview_side": preview_side,
                "preview_score": preview_score,
                "quant_passed": quant_passed,
            }))
            .unwrap_or_else(|_| "{}".to_string());
            let client_id = format!(
                "demo-entry-{}-{symbol}",
                market.candles.last().map_or(0, |c| c.close_time)
            );
            drafts.push(EntryDraft {
                symbol: symbol.clone(),
                side,
                quant_side,
                quant_score,
                preview_side,
                preview_score,
                quant_passed,
                leverage,
                max_leverage: upper as f64,
                stop_price,
                take_profit,
                score,
                mark_price: price,
                filter: *filter,
                scale,
                features_text,
                journal_snapshot,
                client_id,
            });
        }
        Ok(drafts)
    }

    fn review_drafts(
        reviewer: Option<&dyn crate::review::AiReviewer>,
        drafts: Vec<EntryDraft>,
        required: bool,
    ) -> Vec<ReviewedDraft> {
        drafts
            .into_iter()
            .map(|draft| {
                let proposal = draft.proposal();
                let result = if required {
                    crate::review::apply_review(&proposal, reviewer, true)
                } else {
                    crate::review::apply_review(&proposal, None, false)
                };
                ReviewedDraft { draft, result }
            })
            .collect()
    }

    fn apply_entries(
        tx: &rusqlite::Transaction<'_>,
        mode: &str,
        engine: &Value,
        snapshot: &crate::providers::TradingSnapshot,
        reviewed: Vec<ReviewedDraft>,
        executed: &mut Vec<Value>,
    ) -> Result<Vec<LivePlan>, String> {
        let cfg = Self::risk_settings(tx)?;
        let entry_order_type = LiveOrderType::from_setting(engine["order_type"].as_str());
        let mut plans = Vec::new();
        let mut available = snapshot.account.available_balance;
        let now = crate::now();
        let mut audit: Vec<crate::signal_audit::AuditSignal> = Vec::new();
        for ReviewedDraft { draft, result } in reviewed {
            let reviewed = match result {
                Ok(reviewed) => reviewed,
                Err(error) => {
                    let _ = tx.execute(
                        "INSERT INTO engine_events(ts,level,category,message,payload) VALUES(?1,'warn','ai',?2,?3)",
                        (
                            now,
                            "AI 复核跳过开仓",
                            serde_json::json!({"symbol": draft.symbol, "reason": error.reason}).to_string(),
                        ),
                    );
                    audit.push(draft.audit_row(
                        "rejected",
                        draft.signal_side().unwrap_or("flat"),
                        &error.reason,
                        &error.action,
                        error.confidence,
                        &error.reason,
                    ));
                    continue;
                }
            };
            let mut features_value: Value = serde_json::from_str(&draft.features_text)
                .unwrap_or_else(|_| serde_json::json!({}));
            let ai_reason = reviewed.reason.clone();
            if let Some(object) = features_value.as_object_mut() {
                object.insert(
                    "ai_review".to_string(),
                    serde_json::json!({
                        "reason": ai_reason,
                        "leverage": reviewed.leverage,
                        "stop_price": reviewed.stop_price,
                    }),
                );
            }
            let features_text = serde_json::to_string(&features_value)
                .unwrap_or_else(|_| draft.features_text.clone());
            let final_side = if reviewed.side.is_empty() {
                draft.side.clone()
            } else {
                reviewed.side.clone()
            };
            let take_profit = if reviewed.take_profit > 0.0 {
                reviewed.take_profit
            } else {
                draft.take_profit
            };
            let leverage = reviewed.leverage;
            let stop_price = reviewed.stop_price;
            let (qty, _) = crate::risk::estimate_qty(
                snapshot.account.equity,
                draft.mark_price,
                leverage,
                &crate::risk::SizeParams {
                    // 波动率目标仍作用于保证金比例：ATR 高于目标时缩仓，低于时放大（0.25–2x）。
                    margin_pct: (cfg["margin_pct"].as_f64().unwrap_or(50.0)
                        * draft.scale.clamp(0.25, 2.0))
                    .clamp(0.0, 100.0),
                    min_notional_buffer: cfg["min_notional_buffer"].as_f64().unwrap_or(1.05),
                },
                &draft.filter,
                Some(available),
            );
            let blocked = if qty <= 0.0 {
                Some("可用余额或风险预算不足，下单量为 0".to_string())
            } else {
                Self::entry_allowed(
                    tx,
                    mode,
                    engine,
                    &cfg,
                    snapshot,
                    &draft.symbol,
                    &final_side,
                    qty * draft.mark_price,
                    now,
                )?
            };
            if let Some(reason) = blocked {
                audit.push(draft.audit_row(
                    "rejected",
                    draft.quant_side.as_deref().unwrap_or(final_side.as_str()),
                    &reason,
                    &reviewed.action,
                    reviewed.confidence,
                    &reviewed.reason,
                ));
                continue;
            }
            let side = if final_side == "long" { "BUY" } else { "SELL" };
            let entry = EntryTerms {
                leverage,
                stop: stop_price,
                take_profit,
                score: draft.score,
            };
            if mode == "live" {
                // 上一轮挂出去的开仓单还没结论时，绝不能再发第二张；
                // 本地没有持仓不代表交易所没有挂单。
                if Self::live_entry_inflight(tx, &draft.symbol)? {
                    audit.push(draft.audit_row(
                        "rejected",
                        draft.quant_side.as_deref().unwrap_or(final_side.as_str()),
                        "已有未完成的实盘开仓挂单，等待成交或对账",
                        &reviewed.action,
                        reviewed.confidence,
                        &reviewed.reason,
                    ));
                    break;
                }
                let price = match entry_order_type {
                    LiveOrderType::Market => None,
                    LiveOrderType::LimitPostOnly => {
                        let book = snapshot
                            .markets
                            .iter()
                            .find(|market| market.symbol == draft.symbol);
                        let (bid, ask) = book
                            .map(|market| (market.bid, market.ask))
                            .unwrap_or((draft.mark_price, draft.mark_price));
                        Some(Self::limit_entry_price(
                            side,
                            bid,
                            ask,
                            draft.mark_price,
                        ))
                    }
                };
                plans.push(LivePlan {
                    symbol: draft.symbol.clone(),
                    position_side: final_side.clone(),
                    side: side.into(),
                    qty,
                    purpose: "entry".into(),
                    mark_price: draft.mark_price,
                    order_type: entry_order_type,
                    price,
                    entry: Some(entry),
                    features: Some(features_text.clone()),
                });
                // 实盘单还没发出去，只能记「挂单中」，成交与否由后面的结算补账。
                audit.push(
                    draft
                        .audit_row(
                            "pending",
                            draft.quant_side.as_deref().unwrap_or(final_side.as_str()),
                            "",
                            &reviewed.action,
                            reviewed.confidence,
                            &reviewed.reason,
                        )
                        .with_feature("client_order_id", serde_json::json!(&draft.client_id)),
                );
                break;
            }
            let exists: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM orders WHERE mode='demo' AND client_order_id=?1)",
                    [&draft.client_id],
                    |r| r.get(0),
                )
                .map_err(|e| e.to_string())?;
            if exists {
                audit.push(draft.audit_row(
                    "rejected",
                    draft.quant_side.as_deref().unwrap_or(final_side.as_str()),
                    "同一根 K 线已开过单，忽略重复开仓",
                    &reviewed.action,
                    reviewed.confidence,
                    &reviewed.reason,
                ));
                continue;
            }
            let margin = draft.mark_price * qty / leverage;
            let fee_rate = cfg["fee_rate_taker"].as_f64().unwrap_or(0.0005);
            tx.execute("INSERT INTO orders(ts,symbol,side,order_type,qty,price,status,purpose,client_order_id,exchange_order_id,reduce_only,mode,latency_ms) VALUES(?1,?2,?3,'MARKET',?4,?5,'FILLED','entry',?6,'',0,'demo',0)", (now, &draft.symbol, side, qty, draft.mark_price, &draft.client_id)).map_err(|e| e.to_string())?;
            Self::insert_entry_position(
                tx,
                mode,
                &draft.symbol,
                &final_side,
                qty,
                draft.mark_price,
                &entry,
                fee_rate,
                now,
                Some(&features_text),
            )?;
            let target_pct = if draft.mark_price > 0.0 {
                let pct = (take_profit - draft.mark_price).abs() / draft.mark_price * 100.0;
                (pct * 10_000.0).round() / 10_000.0
            } else {
                0.0
            };
            crate::journal::record_decision(
                tx,
                mode,
                &draft.symbol,
                &final_side,
                draft.score,
                draft.mark_price,
                target_pct,
                if reviewed.reason.trim().is_empty() {
                    "量化信号"
                } else {
                    reviewed.reason.as_str()
                },
                &draft.journal_snapshot,
            );
            available -= margin + draft.mark_price * qty * fee_rate;
            executed.push(serde_json::json!({"symbol": draft.symbol, "side": side, "qty": qty, "price": draft.mark_price, "reason": "entry", "client_order_id": draft.client_id}));
            audit.push(draft.audit_row(
                "opened",
                final_side.as_str(),
                "",
                &reviewed.action,
                reviewed.confidence,
                &reviewed.reason,
            ));
        }
        // 审计写入失败不能拖垮下单：日志里留痕，下一轮继续。
        if !audit.is_empty() {
            if let Err(error) = crate::signal_audit::record(tx, mode, now, &audit) {
                tracing::warn!(%error, "信号审计写入失败，不影响本轮下单");
            }
        }
        Ok(plans)
    }

    fn insert_entry_position(
        tx: &rusqlite::Transaction<'_>,
        mode: &str,
        symbol: &str,
        side: &str,
        qty: f64,
        price: f64,
        entry: &EntryTerms,
        fee_rate: f64,
        now: i64,
        features: Option<&str>,
    ) -> Result<(), String> {
        tx.execute("INSERT INTO positions(symbol,side,qty,entry_price,mark_price,leverage,margin,sl_price,tp_price,peak_price,initial_sl_price,signal_score,opened_at,closed_at,exit_price,exit_reason,realized_pnl,fees,status,mode,venue_unrealized,venue_ts,partial_taken_at,ai_confidence,ai_reason,entry_slippage_bps,trade_id) VALUES(?1,?2,?3,?4,?4,?5,?6,?7,?8,?4,?7,?9,?10,0,0,'',0,?11,'open',?12,0,0,0,0,'',0,0)", (symbol, side, qty, price, entry.leverage, qty * price / entry.leverage, entry.stop, entry.take_profit, entry.score, now, qty * price * fee_rate, mode)).map_err(|e| e.to_string())?;
        if let Some(features) = features {
            let id = tx.last_insert_rowid();
            tx.execute(
                "UPDATE positions SET features=?1 WHERE id=?2",
                (features, id),
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// 开仓前的最后一道闸门。返回 Ok(None) 表示放行，
    /// Ok(Some(原因)) 表示被挡下——原因要写进信号审计，
    /// 「为什么没开仓」不能只说「不允许」。
    fn entry_allowed(
        tx: &rusqlite::Transaction<'_>,
        mode: &str,
        engine: &Value,
        cfg: &Value,
        snapshot: &crate::providers::TradingSnapshot,
        symbol: &str,
        side: &str,
        notional: f64,
        now: i64,
    ) -> Result<Option<String>, String> {
        let global_cooldown: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM cooldowns WHERE kind='global' AND until_ts>?1)",
                [now],
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())?;
        if global_cooldown {
            return Ok(Some("全局冷却中，本轮不新开仓".to_string()));
        }
        let cooldown: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM cooldowns WHERE symbol=?1 AND until_ts>?2)",
                (symbol, now),
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())?;
        if cooldown {
            return Ok(Some(format!("{symbol} 处于冷却期")));
        }
        let category = |symbol: &str| {
            if symbol.contains("BTC") {
                0
            } else if symbol.contains("ETH") {
                1
            } else if [
                "SOL", "BNB", "XRP", "ADA", "DOGE", "LTC", "AVAX", "LINK", "DOT", "TRX", "TON",
                "SUI", "MATIC", "POL",
            ]
            .iter()
            .any(|base| symbol.contains(base))
            {
                2
            } else {
                3
            }
        };
        let mut stmt = tx.prepare("SELECT symbol,side,qty,COALESCE(NULLIF(mark_price,0),entry_price) FROM positions WHERE mode=?1 AND status='open'").map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([mode], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, f64>(2)?,
                    r.get::<_, f64>(3)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        let (mut count, mut same, mut total, mut group) = (0, 0, notional, notional);
        for row in rows {
            let (held, direction, qty, fallback) = row.map_err(|e| e.to_string())?;
            if held == symbol {
                return Ok(Some(format!("{symbol} 已有持仓")));
            }
            let price = snapshot
                .markets
                .iter()
                .find(|m| m.symbol == held)
                .map_or(fallback, |m| m.mark_price);
            let value = qty * price;
            if !value.is_finite() || value <= 0.0 {
                return Ok(Some(format!("{held} 仓位价值异常")));
            }
            count += 1;
            same += i64::from(direction == side);
            total += value;
            if category(&held) == category(symbol) {
                group += value;
            }
        }
        let side_cap = cfg["max_same_direction"].as_i64().unwrap_or(0);
        let category_cap = cfg["category_exposure_limit_pct"].as_f64().unwrap_or(500.0);
        let max_positions = engine["max_positions"].as_i64().unwrap_or(2);
        let equity = snapshot.account.equity;
        if !equity.is_finite() || equity <= 0.0 {
            return Ok(Some("账户权益为 0，无法开仓".to_string()));
        }
        if count >= max_positions {
            return Ok(Some(format!("持仓数 {count} 已达上限 {max_positions}")));
        }
        if side_cap > 0 && same >= side_cap {
            return Ok(Some(format!("同向持仓 {same} 达到上限 {side_cap}")));
        }
        let exposure_pct = total / equity * 100.0;
        let max_exposure_pct = cfg["max_exposure_pct"].as_f64().unwrap_or(2000.0);
        if exposure_pct > max_exposure_pct {
            return Ok(Some(format!(
                "总敞口 {exposure_pct:.1}% 超过上限 {max_exposure_pct:.1}%"
            )));
        }
        let group_pct = group / equity * 100.0;
        if category_cap > 0.0 && group_pct > category_cap {
            return Ok(Some(format!(
                "同类敞口 {group_pct:.1}% 超过上限 {category_cap:.1}%"
            )));
        }
        Ok(None)
    }

    /// close_on_halt 触发的 demo 全平：复用管仓平仓的成交结算路径，
/// 只处理本地账本，不发网络请求。标记价缺失时用当前行情快照兜底。
    fn flatten_demo_positions(
        tx: &rusqlite::Transaction<'_>,
        mode: &str,
        snapshot: &crate::providers::TradingSnapshot,
        fee_rate: f64,
        executed: &mut Vec<Value>,
    ) -> Result<(), String> {
        if mode != "demo" {
            return Ok(());
        }
        let cycle_id = format!("halt-{}", crate::now());
        let rows: Vec<(String, String, f64, f64)> = {
            let mut stmt = tx
                .prepare(
                    "SELECT symbol,side,qty,mark_price FROM positions \
                     WHERE mode=?1 AND status='open' AND qty>0 ORDER BY id",
                )
                .map_err(|error| error.to_string())?;
            let mapped = stmt
                .query_map([mode], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, f64>(2)?,
                        row.get::<_, f64>(3)?,
                    ))
                })
                .map_err(|error| error.to_string())?;
            mapped
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())?
        };
        for (symbol, side, qty, mark_price) in rows {
            let mark = if mark_price.is_finite() && mark_price > 0.0 {
                mark_price
            } else {
                snapshot
                    .markets
                    .iter()
                    .find(|market| market.symbol == symbol)
                    .map(|market| market.mark_price)
                    .unwrap_or(0.0)
            };
            let action = crate::manage::ManageAction::Close {
                reason: "halt_flatten".to_string(),
            };
            let Some(fill) = crate::execution::demo_fill_for_manage(
                &cycle_id, &symbol, &side, qty, mark, &action,
            ) else {
                continue;
            };
            let (applied, trade_id) =
                crate::execution::apply_demo_fill_tx(tx, mode, crate::now(), fee_rate, &fill)?;
            if applied {
                executed.push(json!({
                    "symbol": fill.symbol,
                    "side": fill.side,
                    "qty": fill.qty,
                    "price": fill.price,
                    "reason": fill.reason,
                    "client_order_id": fill.client_order_id,
                    "trade_id": trade_id,
                }));
            }
        }
        Ok(())
    }

    fn execute_demo_cycle(
        tx: &rusqlite::Transaction<'_>,
        mode: &str,
        symbols: &[String],
        positions: &[crate::manage::ManagedPosition],
        report: &crate::cycle::CycleReport,
        snapshot: &crate::providers::TradingSnapshot,
        enabled: bool,
    ) -> Result<Vec<Value>, String> {
        if !enabled || mode != "demo" {
            return Ok(Vec::new());
        }
        let fee_rate = Self::risk_settings(tx)?["fee_rate_taker"]
            .as_f64()
            .unwrap_or(0.0005);
        let cycle_id = format!(
            "{}-{}",
            crate::now(),
            snapshot
                .markets
                .iter()
                .filter_map(|market| market.candles.last().map(|candle| candle.close_time))
                .max()
                .unwrap_or(0)
        );
        let mut executed = Vec::new();
        for ((symbol, position), action) in symbols
            .iter()
            .zip(positions.iter())
            .zip(report.manage.iter())
        {
            let id: i64 = tx
                .query_row(
                    "SELECT id FROM positions WHERE symbol=?1 AND side=?2 AND mode=?3 AND status='open' ORDER BY id LIMIT 1",
                    (symbol, &position.side, mode),
                    |row| row.get(0),
                )
                .map_err(|error| error.to_string())?;
            match action {
                crate::manage::ManageAction::Hold => {}
                crate::manage::ManageAction::MoveToBreakeven { new_sl } => {
                    tx.execute("UPDATE positions SET sl_price=?1 WHERE id=?2", (new_sl, id))
                        .map_err(|error| error.to_string())?;
                    executed.push(serde_json::json!({
                        "symbol": symbol,
                        "action": "move_to_breakeven",
                        "sl_price": new_sl,
                    }));
                }
                action => {
                    let Some(fill) = crate::execution::demo_fill_for_manage(
                        &cycle_id,
                        symbol,
                        &position.side,
                        position.qty,
                        position.mark_price,
                        action,
                    ) else {
                        continue;
                    };
                    let (applied, trade_id) = crate::execution::apply_demo_fill_tx(
                        tx,
                        mode,
                        crate::now(),
                        fee_rate,
                        &fill,
                    )?;
                    if applied {
                        executed.push(serde_json::json!({
                            "symbol": fill.symbol,
                            "side": fill.side,
                            "qty": fill.qty,
                            "price": fill.price,
                            "reason": fill.reason,
                            "client_order_id": fill.client_order_id,
                            "trade_id": trade_id,
                        }));
                    }
                }
            }
        }
        Ok(executed)
    }

    pub(crate) fn exchange_credentials(&self, mode: &str) -> Result<(String, String), String> {
        let data = self
            .db
            .setting("data")
            .map_err(|error| error.to_string())?
            .unwrap_or(Value::Null);
        let key =
            std::str::from_utf8(&self.master_key).map_err(|_| "主密钥格式无效".to_string())?;
        let decrypt = |name: &str| {
            let field = format!("{mode}_api_{name}_enc");
            let legacy = format!("api_{name}_enc");
            let encrypted = data.get(&field).or_else(|| {
                if mode == "live" {
                    data.get(&legacy)
                } else {
                    None
                }
            });
            crate::secret::decrypt_secret(key, encrypted.and_then(Value::as_str).unwrap_or(""))
        };
        let credentials =
            crate::sign::require_credentials(mode, &decrypt("key")?, &decrypt("secret")?)?;
        Ok((credentials.api_key, credentials.secret))
    }

    pub(crate) fn live_credentials(&self) -> Result<LiveCreds, String> {
        let (api_key, secret) = self.exchange_credentials("live")?;
        let engine = self
            .db
            .setting("engine")
            .map_err(|error| error.to_string())?
            .unwrap_or(Value::Null);
        Ok(LiveCreds {
            base: "https://fapi.binance.com".to_string(),
            api_key,
            secret,
            recv_window_ms: engine["recv_window_ms"]
                .as_i64()
                .unwrap_or(5_000)
                .clamp(1_000, 60_000),
        })
    }

    pub(crate) fn ban_gate(&self) -> crate::submit::BanGate {
        let until = self
            .db
            .conn
            .lock()
            .ok()
            .and_then(|conn| {
                conn.query_row(
                    "SELECT value FROM engine_state WHERE key='execution_ban_until'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .ok()
            })
            .and_then(|raw| raw.parse::<i64>().ok())
            .unwrap_or(0);
        crate::submit::BanGate::with_until(until)
    }

    pub(crate) fn save_ban_gate(&self, gate: &crate::submit::BanGate) {
        if let Ok(conn) = self.db.conn.lock() {
            let _ = upsert_state(&conn, "execution_ban_until", &gate.until_ms().to_string());
        }
    }

    pub(crate) async fn build_live(
        &self,
        gate: &mut crate::submit::BanGate,
        creds: &LiveCreds,
        method: &str,
        path: &str,
        params: &[(String, String)],
    ) -> Result<crate::submit::PreparedRequest, String> {
        let now = crate::now_millis();
        let timestamp = crate::exchange_time::signed_timestamp(&creds.base).await;
        crate::submit::prepare(
            gate,
            now,
            method,
            &creds.base,
            path,
            params,
            &creds.api_key,
            &creds.secret,
            timestamp,
            creds.recv_window_ms,
        )
    }

    /// 发单前先落一条 `SUBMITTING`：进程崩在请求中途，也能在对账里看到这笔悬案。
    fn reserve_order(
        &self,
        plan: &LivePlan,
        client_order_id: &str,
        qty: f64,
    ) -> Result<(), String> {
        let mut conn = self
            .db
            .conn
            .lock()
            .map_err(|_| "数据库锁不可用".to_string())?;
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        if let Some(entry) = &plan.entry {
            upsert_state(
                &tx,
                &format!("entry_terms.{client_order_id}"),
                &serde_json::to_string(entry).map_err(|e| e.to_string())?,
            )?;
        }
        if let Some(features) = &plan.features {
            upsert_state(&tx, &format!("entry_features.{client_order_id}"), features)?;
        }
        tx.execute(
            "INSERT INTO orders(ts,symbol,side,order_type,qty,price,status,purpose,\
             client_order_id,exchange_order_id,reduce_only,mode,latency_ms) \
             VALUES(?1,?2,?3,?4,?5,?6,'SUBMITTING',?7,?8,'',?9,'live',0)",
            (
                crate::now(),
                &plan.symbol,
                &plan.side,
                plan.order_type.as_str(),
                qty,
                plan.price.unwrap_or(0.0),
                &plan.purpose,
                client_order_id,
                plan.entry.is_none(),
            ),
        )
        .map_err(|error| error.to_string())?;
        tx.commit().map_err(|e| e.to_string())
    }

    fn update_order_status(
        &self,
        client_order_id: &str,
        status: &str,
        exchange_order_id: &str,
    ) -> Result<(), String> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|_| "数据库锁不可用".to_string())?;
        conn.execute(
            "UPDATE orders SET status=?1, exchange_order_id=?2 WHERE client_order_id=?3 AND mode='live'",
            (status, exchange_order_id, client_order_id),
        )
        .map(|_| ())
        .map_err(|error| error.to_string())
    }

    fn settle_live(
        &self,
        plan: &LivePlan,
        client_order_id: &str,
        qty: f64,
        fee_rate: f64,
        reply: &crate::live::Reply,
    ) -> Result<Option<String>, String> {
        use crate::live::Reply;
        let mut conn = self
            .db
            .conn
            .lock()
            .map_err(|_| "数据库锁不可用".to_string())?;
        let tx = conn.transaction().map_err(|error| error.to_string())?;
        let mut review_ids: Vec<i64> = Vec::new();
        if plan.purpose == "entry" {
            if let Reply::Filled {
                order_id,
                executed_qty,
                avg_price,
            } = reply
            {
                let key = format!("entry_terms.{client_order_id}");
                let raw: Option<String> = tx
                    .query_row(
                        "SELECT value FROM engine_state WHERE key=?1",
                        [&key],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(|e| e.to_string())?;
                let Some(raw) = raw else { return Ok(None) };
                let entry: EntryTerms = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
                let features_key = format!("entry_features.{client_order_id}");
                let features: Option<String> = tx
                    .query_row(
                        "SELECT value FROM engine_state WHERE key=?1",
                        [&features_key],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(|e| e.to_string())?;
                if !avg_price.is_finite()
                    || *avg_price <= 0.0
                    || !executed_qty.is_finite()
                    || *executed_qty <= 0.0
                {
                    return Ok(Some(format!(
                        "{} 开仓回执缺成交价或数量，等待对账",
                        plan.symbol
                    )));
                }
                Self::insert_entry_position(
                    &tx,
                    "live",
                    &plan.symbol,
                    &plan.position_side,
                    *executed_qty,
                    *avg_price,
                    &entry,
                    fee_rate,
                    crate::now(),
                    features.as_deref(),
                )?;
                let target_pct = if *avg_price > 0.0 {
                    let pct = (entry.take_profit - *avg_price).abs() / *avg_price * 100.0;
                    (pct * 10_000.0).round() / 10_000.0
                } else {
                    0.0
                };
                crate::journal::record_decision(
                    &tx,
                    "live",
                    &plan.symbol,
                    &plan.position_side,
                    entry.score,
                    *avg_price,
                    target_pct,
                    "实盘成交",
                    &serde_json::json!({}),
                );
                tx.execute("UPDATE orders SET status='FILLED',price=?1,qty=?2,exchange_order_id=?3 WHERE client_order_id=?4 AND mode='live'", (avg_price, executed_qty, order_id, client_order_id)).map_err(|e| e.to_string())?;
                // 成交后把当时记的「挂单中」审计行改成「已开仓」，否则信号审计的已开仓统计一直是 0。
                tx.execute(
                    "UPDATE signals SET decision='opened' \
                     WHERE mode='live' AND decision='pending' AND symbol=?1 \
                       AND json_extract(CASE WHEN json_valid(features) THEN features ELSE '{}' END, '$.client_order_id')=?2",
                    (&plan.symbol, client_order_id),
                )
                .map_err(|error| error.to_string())?;
                tx.execute("DELETE FROM engine_state WHERE key=?1", [&key])
                    .map_err(|e| e.to_string())?;
                tx.commit().map_err(|e| e.to_string())?;
                return Ok(None);
            }
        }
        let failure = match reply {
            Reply::Filled {
                order_id,
                executed_qty,
                avg_price,
            } => {
                let price = if avg_price.is_finite() && *avg_price > 0.0 {
                    *avg_price
                } else {
                    plan.mark_price
                };
                let filled = if executed_qty.is_finite() && *executed_qty > 0.0 {
                    *executed_qty
                } else {
                    qty
                };
                let position_side = if plan.side == "SELL" { "long" } else { "short" };
                let open: bool = tx
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM positions WHERE symbol=?1 AND side=?2 \
                         AND mode='live' AND status='open')",
                        (&plan.symbol, position_side),
                        |row| row.get(0),
                    )
                    .map_err(|error| error.to_string())?;
                if open {
                    let fill = crate::execution::DemoFill {
                        client_order_id: client_order_id.to_string(),
                        exchange_order_id: order_id.clone(),
                        symbol: plan.symbol.clone(),
                        side: plan.side.clone(),
                        qty: filled,
                        price,
                        reduce_only: true,
                        reason: plan.purpose.clone(),
                    };
                    let (applied, trade_id) = crate::execution::apply_live_fill_tx(
                        &tx,
                        "live",
                        crate::now(),
                        fee_rate,
                        &fill,
                    )?;
                    if applied {
                        if let Some(trade_id) = trade_id {
                            review_ids.push(trade_id);
                        }
                    }
                } else {
                    // 仓位已被 venue_sync 关掉：只补订单状态，不能再减一次。
                    tx.execute(
                        "UPDATE orders SET status='FILLED', price=?1, qty=?2, exchange_order_id=?3 \
                         WHERE client_order_id=?4 AND mode='live'",
                        (price, filled, order_id, client_order_id),
                    )
                    .map_err(|error| error.to_string())?;
                }
                None
            }
            Reply::Accepted { order_id, status } => {
                tx.execute(
                    "UPDATE orders SET status=?1, exchange_order_id=?2 WHERE client_order_id=?3 AND mode='live'",
                    (if status.is_empty() { "NEW" } else { status.as_str() }, order_id, client_order_id),
                )
                .map_err(|error| error.to_string())?;
                // 交易所受理（NEW/PARTIALLY_FILLED）不是失败：开仓和平仓都等待下一轮对账，
                // 只有明确拒绝或结果未知才上报，避免把正常回执写成错误事件。
                tracing::debug!(
                    symbol = %plan.symbol,
                    status = %reply.status_text(),
                    "订单已受理，等待对账"
                );
                None
            }
            Reply::Rejected { code, message } => {
                tx.execute(
                    "UPDATE orders SET status='REJECTED' WHERE client_order_id=?1 AND mode='live'",
                    [client_order_id],
                )
                .map_err(|error| error.to_string())?;
                Some(format!("{} 下单被拒 {code} {message}", plan.symbol))
            }
            Reply::NotFound | Reply::Unknown => {
                tx.execute(
                    "UPDATE orders SET status='UNKNOWN' WHERE client_order_id=?1 AND mode='live'",
                    [client_order_id],
                )
                .map_err(|error| error.to_string())?;
                Some(format!(
                    "{} 下单结果未知，已阻止重发，交由对账确认",
                    plan.symbol
                ))
            }
        };
        tx.commit().map_err(|error| error.to_string())?;
        self.spawn_reviews(&review_ids);
        Ok(failure)
    }

    /// 未知结果只查不发。返回查询到的结论，查不到就保持未知。
    async fn query_dual_side(
        &self,
        creds: &LiveCreds,
        gateway: &dyn crate::live::Gateway,
    ) -> Result<bool, String> {
        let mut gate = self.ban_gate();
        let request = self
            .build_live(&mut gate, creds, "GET", "/fapi/v1/positionSide/dual", &[])
            .await?;
        let response = gateway.send(&request).await;
        if response.transport_failed || !(200..300).contains(&response.status) {
            return Err(format!(
                "无法读取交易所持仓模式 (HTTP {})，已停止实盘订单",
                response.status
            ));
        }
        let body: Value = serde_json::from_str(&response.body)
            .map_err(|_| "交易所持仓模式响应不是有效 JSON，已停止实盘订单".to_string())?;
        body["dualSidePosition"]
            .as_bool()
            .ok_or_else(|| "交易所持仓模式缺少 dualSidePosition，已停止实盘订单".to_string())
    }

    async fn resolve_live(
        &self,
        creds: &LiveCreds,
        gateway: &dyn crate::live::Gateway,
        symbol: &str,
        client_order_id: &str,
    ) -> crate::live::Reply {
        let mut gate = self.ban_gate();
        for attempt in 0..4 {
            let params = crate::live::query_params(symbol, client_order_id, creds.recv_window_ms);
            let request = match self
                .build_live(&mut gate, creds, "GET", crate::live::ORDER_PATH, &params)
                .await
            {
                Ok(request) => request,
                Err(error) => {
                    tracing::warn!(%error, %symbol, "实盘未知结果查询无法签名");
                    return crate::live::Reply::Unknown;
                }
            };
            let reply = crate::live::submit(gateway, &request).await;
            match reply {
                crate::live::Reply::NotFound if attempt < 3 => {
                    // 交易所接受后落账有延迟，立刻重查必然落空。
                    sleep(Duration::from_millis(1_200)).await;
                }
                other => return other,
            }
        }
        crate::live::Reply::Unknown
    }

    async fn run_live_with(
        &self,
        creds: &LiveCreds,
        gateway: &dyn crate::live::Gateway,
        plans: Vec<LivePlan>,
    ) -> Result<Vec<String>, String> {
        let dual_side = self.query_dual_side(creds, gateway).await?;
        let risk = self
            .db
            .setting("risk")
            .map_err(|error| error.to_string())?
            .unwrap_or_else(|| crate::settings_api::defaults_of("risk"));
        let taker_rate = risk["fee_rate_taker"].as_f64().unwrap_or(0.0005);
        let maker_rate = risk["fee_rate_maker"].as_f64().unwrap_or(0.0002);
        let buffer = risk["min_notional_buffer"].as_f64().unwrap_or(1.05);
        let symbols: Vec<String> = plans.iter().map(|plan| plan.symbol.clone()).collect();
        let filters = {
            let conn = self
                .db
                .conn
                .lock()
                .map_err(|_| "数据库锁不可用".to_string())?;
            crate::live::load_filters(&conn, &symbols)
        };
        let mut gate = self.ban_gate();
        let mut failures = Vec::new();
        for (index, plan) in plans.into_iter().enumerate() {
            let t_total = Instant::now();
            let t_plan = Instant::now();
            let filter = filters.get(&plan.symbol).copied().unwrap_or_default();
            let qty = match crate::live::admit_qty(plan.qty, plan.mark_price, &filter, buffer) {
                Ok(qty) => qty,
                Err(error) => {
                    failures.push(format!("{} {error}", plan.symbol));
                    self.report_segments(&[("total", elapsed_ms(t_total))]);
                    continue;
                }
            };
            if let Some(entry) = &plan.entry {
                let leverage = entry.leverage as i64;
                let params = [
                    ("symbol".into(), plan.symbol.clone()),
                    ("leverage".into(), leverage.to_string()),
                ];
                let request = match self
                    .build_live(&mut gate, creds, "POST", "/fapi/v1/leverage", &params)
                    .await
                {
                    Ok(request) => request,
                    Err(error) => {
                        failures.push(format!("{} 杠杆设置被拦截：{error}", plan.symbol));
                        self.report_segments(&[
                            ("total", elapsed_ms(t_total)),
                            ("plan", elapsed_ms(t_plan)),
                        ]);
                        continue;
                    }
                };
                let response = gateway.send(&request).await;
                let body = serde_json::from_str::<Value>(&response.body).unwrap_or(Value::Null);
                if body["code"].as_i64() == Some(-1003) {
                    gate.observe(body["msg"].as_str().unwrap_or(""), crate::now_millis());
                }
                if response.transport_failed
                    || !(200..300).contains(&response.status)
                    || body["symbol"].as_str() != Some(plan.symbol.as_str())
                    || body["leverage"].as_i64() != Some(leverage)
                {
                    failures.push(format!("{} 无法确认交易所杠杆，跳过开仓", plan.symbol));
                    self.report_segments(&[
                        ("total", elapsed_ms(t_total)),
                        ("plan", elapsed_ms(t_plan)),
                    ]);
                    continue;
                }
            }
            let client_order_id = crate::live::client_order_id("nq", &plan.purpose, index);
            let t_persist = Instant::now();
            if let Err(error) = self.reserve_order(&plan, &client_order_id, qty) {
                self.report_segments(&[
                    ("total", elapsed_ms(t_total)),
                    ("plan", elapsed_ms(t_plan)),
                    ("persist", elapsed_ms(t_persist)),
                ]);
                return Err(error);
            }
            let reserve_ms = elapsed_ms(t_persist);
            let spec = crate::live::OrderSpec {
                symbol: plan.symbol.clone(),
                side: plan.side.clone(),
                qty,
                reduce_only: plan.entry.is_none(),
                position_side: plan.position_side.clone(),
                dual_side,
                purpose: plan.purpose.clone(),
                client_order_id: client_order_id.clone(),
                order_type: plan.order_type.as_str().to_string(),
                price: plan.price,
                post_only: plan.order_type.is_post_only(),
            };
            let request = match self
                .build_live(
                    &mut gate,
                    creds,
                    "POST",
                    crate::live::ORDER_PATH,
                    &spec.params(&filter, creds.recv_window_ms),
                )
                .await
            {
                Ok(request) => request,
                Err(error) => {
                    let update = self.update_order_status(&client_order_id, "REJECTED", "");
                    failures.push(format!("{} 发单前被拦截：{error}", plan.symbol));
                    self.report_segments(&[
                        ("total", elapsed_ms(t_total)),
                        ("plan", elapsed_ms(t_plan)),
                        ("persist", reserve_ms),
                    ]);
                    update?;
                    continue;
                }
            };
            let plan_at_build = elapsed_ms(t_plan);
            let sign_ms = request.sign_ms;
            let plan_ms = (plan_at_build - reserve_ms - sign_ms).max(0.0);
            let t_http = Instant::now();
            let mut reply = crate::live::submit(gateway, &request).await;
            let http_ms = elapsed_ms(t_http);
            if let crate::live::Reply::Rejected {
                code: -1003,
                message,
            } = &reply
            {
                // 限频封禁：记进熔断，后面的请求在本地就拦下，避免续期。
                gate.observe(message, crate::now_millis());
            }
            let t_wait = Instant::now();
            if reply == crate::live::Reply::Unknown {
                reply = self
                    .resolve_live(creds, gateway, &plan.symbol, &client_order_id)
                    .await;
            }
            let wait_ms = elapsed_ms(t_wait);
            let t_settle = Instant::now();
            let fee_rate = Self::plan_fee_rate(&plan, taker_rate, maker_rate);
            match self.settle_live(&plan, &client_order_id, qty, fee_rate, &reply) {
                Ok(Some(failure)) => failures.push(failure),
                Ok(None) => {}
                Err(error) => {
                    self.report_segments(&[
                        ("total", elapsed_ms(t_total)),
                        ("plan", plan_ms),
                        ("sign", sign_ms),
                        ("http", http_ms),
                        ("wait", wait_ms),
                        ("persist", reserve_ms + elapsed_ms(t_settle)),
                    ]);
                    return Err(error);
                }
            }
            let persist_ms = reserve_ms + elapsed_ms(t_settle);
            self.report_segments(&[
                ("total", elapsed_ms(t_total)),
                ("plan", plan_ms),
                ("sign", sign_ms),
                ("http", http_ms),
                ("wait", wait_ms),
                ("persist", persist_ms),
            ]);
        }
        failures.extend(
            self.reconcile_pending(creds, gateway, &mut gate, taker_rate, maker_rate)
                .await?
                .into_iter()
                .map(|failure| format!("对账: {failure}")),
        );
        let t_protect = Instant::now();
        let protect_failures = self.ensure_protection(creds, gateway, dual_side).await?;
        self.report_segments(&[("protect", elapsed_ms(t_protect))]);
        failures.extend(protect_failures);
        self.save_ban_gate(&gate);
        Ok(failures)
    }

    async fn run_live(&self, plans: Vec<LivePlan>) -> Result<Vec<String>, String> {
        let creds = self.live_credentials()?;
        let gateway = crate::live::HttpGateway::new();
        self.run_live_with(&creds, &gateway, plans).await
    }

    /// 上一轮没拿到明确结论的订单：按 clientOrderId 查，确认后再结算。
    /// LIMIT 开仓单长时间未成交会一直占用标的，超过超时后主动撤单；
    /// 撤单失败/未知时不重发，保持 UNKNOWN 并告警。
    async fn reconcile_pending(
        &self,
        creds: &LiveCreds,
        gateway: &dyn crate::live::Gateway,
        gate: &mut crate::submit::BanGate,
        taker_rate: f64,
        maker_rate: f64,
    ) -> Result<Vec<String>, String> {
        let (pending, timeout_sec) = {
            let conn = self
                .db
                .conn
                .lock()
                .map_err(|_| "数据库锁不可用".to_string())?;
            let timeout_sec = limit_order_timeout_sec(&conn);
            let mut stmt = conn
                .prepare(
                    // 只查市价/限价单：条件单是保护腿，按 clientAlgoId 挂在 Algo 接口上，
                    // 用订单接口查只会拿到 -2013。
                    "SELECT client_order_id,symbol,side,purpose,order_type,ts FROM orders \
                     WHERE mode='live' AND status IN ('SUBMITTING','UNKNOWN','NEW','PARTIALLY_FILLED') \
                     AND order_type IN ('MARKET','LIMIT') ORDER BY id LIMIT 20",
                )
                .map_err(|error| error.to_string())?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, i64>(5)?,
                    ))
                })
                .map_err(|error| error.to_string())?;
            let rows = rows
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())?;
            (rows, timeout_sec)
        };
        let now = crate::now();
        let mut failures = Vec::new();
        for (client_order_id, symbol, side, purpose, order_type, ts) in pending {
            let reply = self
                .resolve_live(creds, gateway, &symbol, &client_order_id)
                .await;
            let limit_entry = order_type == "LIMIT" && purpose == "entry";
            let timed_out = limit_entry && now.saturating_sub(ts) >= timeout_sec;
            let plan = LivePlan {
                symbol: symbol.clone(),
                position_side: if (side == "BUY") == (purpose == "entry") {
                    "long"
                } else {
                    "short"
                }
                .to_string(),
                side,
                qty: 0.0,
                purpose,
                mark_price: 0.0,
                order_type: LiveOrderType::from_setting(Some(&order_type)),
                price: None,
                entry: None,
                features: None,
            };
            let fee_rate = Self::plan_fee_rate(&plan, taker_rate, maker_rate);
            match &reply {
                crate::live::Reply::Filled {
                    avg_price,
                    executed_qty,
                    ..
                } => {
                    if !(avg_price.is_finite()
                        && *avg_price > 0.0
                        && executed_qty.is_finite()
                        && *executed_qty > 0.0)
                    {
                        // 没有成交价就算不出盈亏，保持未知，等下一轮再查。
                        failures.push(format!("{symbol} 已成交但缺成交价，暂不结算"));
                        continue;
                    }
                    let filled = *executed_qty;
                    if let Some(failure) =
                        self.settle_live(&plan, &client_order_id, filled, fee_rate, &reply)?
                    {
                        failures.push(failure);
                    }
                }
                crate::live::Reply::Rejected { code, message } => {
                    self.update_order_status(&client_order_id, "REJECTED", "")?;
                    failures.push(format!("{symbol} 补查为拒单 {code} {message}"));
                }
                crate::live::Reply::NotFound | crate::live::Reply::Unknown => {
                    if timed_out {
                        if matches!(&reply, crate::live::Reply::NotFound) {
                            // 交易所查不到这笔挂单：没有可撤的单，直接标记取消，解除 inflight。
                            self.update_order_status(&client_order_id, "CANCELED", "")?;
                            self.events.record(
                                "warn",
                                "trade",
                                &format!("{symbol} 限价开仓超时且交易所无此单，已标记取消"),
                                json!({"symbol": symbol, "client_order_id": client_order_id}),
                            );
                        } else {
                            // 结果未知时不能撤单也不能重发：保持 UNKNOWN，人工确认前不放开。
                            self.update_order_status(&client_order_id, "UNKNOWN", "")?;
                            failures.push(format!("{symbol} 限价开仓超时但结果未知，未撤单未重发"));
                            self.events.record(
                                "warn",
                                "trade",
                                &format!("{symbol} 限价开仓超时但结果未知，保持 UNKNOWN 不重发"),
                                json!({"symbol": symbol, "client_order_id": client_order_id}),
                            );
                        }
                    }
                }
                crate::live::Reply::Accepted { status, .. } => {
                    let partial = status.eq_ignore_ascii_case("PARTIALLY_FILLED");
                    if timed_out && !partial {
                        match self
                            .cancel_limit_order(creds, gateway, gate, &symbol, &client_order_id)
                            .await
                        {
                            Ok(()) => {
                                self.update_order_status(&client_order_id, "CANCELED", "")?;
                                self.events.record(
                                    "warn",
                                    "trade",
                                    &format!("{symbol} 限价开仓超时，已撤单"),
                                    json!({"symbol": symbol, "client_order_id": client_order_id}),
                                );
                            }
                            Err(error) => {
                                self.update_order_status(&client_order_id, "UNKNOWN", "")?;
                                failures.push(format!("{symbol} 限价开仓撤单失败: {error}"));
                                self.events.record(
                                    "warn",
                                    "trade",
                                    &format!("{symbol} 限价开仓撤单失败，保持 UNKNOWN 不重发"),
                                    json!({
                                        "symbol": symbol,
                                        "client_order_id": client_order_id,
                                        "error": error,
                                    }),
                                );
                            }
                        }
                        continue;
                    }
                    self.update_order_status(
                        &client_order_id,
                        if status.is_empty() { "NEW" } else { status },
                        "",
                    )?;
                }
            }
        }
        Ok(failures)
    }

    /// 撤掉未成交的 LIMIT 开仓单。任何超时/网络不确定都返回 Err，调用方保持 UNKNOWN。
    async fn cancel_limit_order(
        &self,
        creds: &LiveCreds,
        gateway: &dyn crate::live::Gateway,
        gate: &mut crate::submit::BanGate,
        symbol: &str,
        client_order_id: &str,
    ) -> Result<(), String> {
        let params = vec![
            ("symbol".to_string(), symbol.to_string()),
            (
                "origClientOrderId".to_string(),
                client_order_id.to_string(),
            ),
            ("recvWindow".to_string(), creds.recv_window_ms.to_string()),
        ];
        let request = self
            .build_live(gate, creds, "DELETE", crate::live::ORDER_PATH, &params)
            .await?;
        let reply = gateway.send(&request).await;
        if reply.transport_failed {
            return Err("撤单结果未知: 传输失败".into());
        }
        if !(200..300).contains(&reply.status) {
            return Err(format!(
                "撤单失败 HTTP {}: {}",
                reply.status,
                truncate_body(&reply.body)
            ));
        }
        Ok(())
    }

    /// 需要补挂保护单的仓位。
    ///
    /// 默认只接管引擎自己开过的仓（本地有 `entry` 委托）；手上手动开的仓
    /// 要等 `risk.auto_protect_external` 打开，否则会替别人的仓位擅自挂单。
    fn protection_targets(&self) -> Result<Vec<(String, String, f64, f64)>, String> {
        let auto_external = self
            .db
            .setting("risk")
            .ok()
            .flatten()
            .map(|risk| risk["auto_protect_external"].as_bool().unwrap_or(false))
            .unwrap_or(false);
        let ownership = if auto_external {
            ""
        } else {
            "AND EXISTS(SELECT 1 FROM orders e WHERE e.mode=p.mode AND e.symbol=p.symbol AND e.purpose='entry')"
        };
        let sql = format!(
            "SELECT p.symbol,p.side,p.sl_price,p.tp_price FROM positions p \
             WHERE p.mode='live' AND p.status='open' AND p.qty > 0 \
             AND (p.sl_price > 0 OR p.tp_price > 0) {ownership} \
             AND NOT EXISTS(SELECT 1 FROM orders o WHERE o.mode=p.mode AND o.symbol=p.symbol \
                 AND o.side = CASE p.side WHEN 'long' THEN 'SELL' ELSE 'BUY' END \
                 AND o.purpose IN ('stop_loss','take_profit') \
                 AND o.status IN ('SUBMITTING','ACCEPTED','NEW','UNKNOWN') \
                 AND (o.status != 'ACCEPTED' OR o.exchange_order_id != ''))"
        );
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|_| "数据库锁不可用".to_string())?;
        let mut stmt = conn.prepare(&sql).map_err(|error| error.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<f64>>(2)?.unwrap_or(0.0),
                    row.get::<_, Option<f64>>(3)?.unwrap_or(0.0),
                ))
            })
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())
    }

    async fn ensure_protection(
        &self,
        creds: &LiveCreds,
        gateway: &dyn crate::live::Gateway,
        dual_side: bool,
    ) -> Result<Vec<String>, String> {
        let targets = self.protection_targets()?;
        if targets.is_empty() {
            return Ok(Vec::new());
        }
        let symbols: Vec<String> = targets.iter().map(|item| item.0.clone()).collect();
        let filters = {
            let conn = self
                .db
                .conn
                .lock()
                .map_err(|_| "数据库锁不可用".to_string())?;
            crate::live::load_filters(&conn, &symbols)
        };
        let mut gate = self.ban_gate();
        let mut failures = Vec::new();
        for (index, (symbol, side, sl, tp)) in targets.iter().enumerate() {
            let filter = filters.get(symbol).copied().unwrap_or_default();
            let legs = crate::live::protection_legs_mode(
                symbol,
                side,
                *sl,
                *tp,
                &filter,
                creds.recv_window_ms,
                index * 2,
                dual_side,
            );
            for leg in legs {
                let purpose = if leg.kind == "STOP_MARKET" {
                    "stop_loss"
                } else {
                    "take_profit"
                };
                let outcome = crate::live::place_protection_leg(
                    &mut gate,
                    gateway,
                    &creds.base,
                    &creds.api_key,
                    &creds.secret,
                    &leg,
                )
                .await;
                match outcome.failure {
                    None => self.record_protection_order(
                        symbol,
                        purpose,
                        &leg,
                        "ACCEPTED",
                        &outcome.exchange_order_id,
                    )?,
                    Some(failure) => {
                        // 明确拒绝也留痕，但不阻塞下一轮重试；结果未知的腿必须阻塞，避免重复挂单。
                        let status = if outcome.unknown { "UNKNOWN" } else { "REJECTED" };
                        self.record_protection_order(symbol, purpose, &leg, status, "")?;
                        failures.push(format!("{symbol} 保护单 {failure}"));
                    }
                }
            }
        }
        self.save_ban_gate(&gate);
        Ok(failures)
    }

    fn record_protection_order(
        &self,
        symbol: &str,
        purpose: &str,
        leg: &crate::live::ProtectionLeg,
        status: &str,
        exchange_order_id: &str,
    ) -> Result<(), String> {
        let param = |name: &str| {
            leg.algo_params
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
                .unwrap_or_default()
        };
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|_| "数据库锁不可用".to_string())?;
        conn.execute(
            "INSERT INTO orders(ts,symbol,side,order_type,qty,price,status,purpose,\
             client_order_id,exchange_order_id,reduce_only,mode,latency_ms) \
             VALUES(?1,?2,?3,?4,0,0,?5,?6,?7,?8,1,'live',0)",
            (
                crate::now(),
                symbol,
                param("side"),
                leg.kind.clone(),
                status,
                purpose,
                param("clientAlgoId"),
                exchange_order_id,
            ),
        )
        .map(|_| ())
        .map_err(|error| error.to_string())
    }

    async fn refresh_snapshot(
        &self,
        mode: &str,
        provider: &dyn crate::providers::SnapshotProvider,
    ) -> Result<Vec<LivePlan>, String> {
        use crate::providers::ProviderError;
        let snapshot = tokio::time::timeout(Duration::from_secs(30), provider.snapshot())
            .await
            .map_err(|_| "交易所快照超时".to_string())?
            .map_err(|error| match error {
                ProviderError::Timeout => "交易所快照超时".into(),
                ProviderError::Unavailable(message) | ProviderError::InvalidData(message) => {
                    message
                }
            })?;
        self.persist_snapshot(mode, &snapshot)
    }

    async fn sync_exchange(&self) -> Result<(String, Vec<LivePlan>), String> {
        let engine = self
            .db
            .setting("engine")
            .map_err(|error| error.to_string())?
            .unwrap_or(Value::Null);
        // 交易环境以 engine_state.mode 为准：切换开关会同时写状态和配置组，
        // 但只有状态是 `/engine/status` 与其它路由读取的那份。
        let state_mode: Option<String> = {
            let conn = self
                .db
                .conn
                .lock()
                .map_err(|_| "数据库锁不可用".to_string())?;
            conn.query_row(
                "SELECT value FROM engine_state WHERE key='mode'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())?
        };
        let mode_raw = state_mode
            .filter(|mode| !mode.is_empty())
            .or_else(|| engine["mode"].as_str().map(str::to_string))
            .unwrap_or_else(|| "demo".into());
        let (mode, base) = match mode_raw.as_str() {
            "demo" | "paper" => ("demo", "https://testnet.binancefuture.com"),
            "live" => ("live", "https://fapi.binance.com"),
            _ => return Err("交易环境无效".into()),
        };
        let credentials = match self.exchange_credentials(mode) {
            Ok(credentials) => Some(credentials),
            Err(error) if mode == "demo" => {
                tracing::info!(%error, "demo 未配置凭证，改用公开行情 + 本地账本");
                None
            }
            Err(error) => return Err(error),
        };
        let symbols = {
            let conn = self
                .db
                .conn
                .lock()
                .map_err(|_| "数据库锁不可用".to_string())?;
            let mut stmt = conn.prepare("SELECT symbol FROM trading_symbols WHERE enabled != 0 ORDER BY sort_order, symbol")
                .map_err(|error| error.to_string())?;
            let rows = stmt
                .query_map([], |row| row.get(0))
                .map_err(|error| error.to_string())?;
            rows.collect::<Result<Vec<String>, _>>()
                .map_err(|error| error.to_string())?
        };
        let strategy = self
            .db
            .setting("strategy")
            .map_err(|error| error.to_string())?
            .unwrap_or(Value::Null);
        let timeframe = strategy["timeframe"].as_str().unwrap_or("4h");
        let provider = match credentials {
            Some((api_key, secret)) => crate::providers::BinanceProvider::with_symbols(
                base,
                symbols.clone(),
                api_key,
                secret,
            ),
            // demo 没有凭证也必须是真实行情：公开接口 + 本地账本，禁止假行情。
            None => crate::providers::BinanceProvider::with_symbols_public(
                base,
                symbols.clone(),
                self.db.clone(),
            ),
        }
        .and_then(|provider| provider.with_timeframe(timeframe))
        .map_err(|error| format!("无法初始化交易所快照客户端: {error:?}"))?;
        let plans = self.refresh_snapshot(mode, &provider).await?;
        Ok((mode.to_string(), plans))
    }

    fn validate_snapshot(
        mode: &str,
        snapshot: &crate::providers::TradingSnapshot,
    ) -> Result<(), String> {
        use std::collections::BTreeSet;
        let enabled_symbols: BTreeSet<&str> = snapshot
            .markets
            .iter()
            .map(|market| market.symbol.as_str())
            .collect();
        if enabled_symbols.is_empty()
            || snapshot.markets.iter().any(|market| {
                market.mark_price <= 0.0
                    || !market.mark_price.is_finite()
                    || !market.quote_volume.is_finite()
                    || market.quote_volume < 0.0
                    || market.candles.len() < 20
                    || market
                        .candles
                        .windows(2)
                        .any(|pair| pair[0].close_time >= pair[1].close_time)
                    || market.candles.iter().any(|candle| {
                        !candle.open.is_finite()
                            || !candle.high.is_finite()
                            || !candle.low.is_finite()
                            || !candle.close.is_finite()
                            || !candle.volume.is_finite()
                            || candle.open <= 0.0
                            || candle.high < candle.low
                            || candle.low <= 0.0
                            || candle.close < candle.low
                            || candle.close > candle.high
                            || candle.volume < 0.0
                    })
            })
        {
            return Err("行情快照缺少有效 K 线或成交额".into());
        }
        if !snapshot.account.equity.is_finite() || !snapshot.account.available_balance.is_finite() {
            return Err("账户快照中的权益或可用余额无效".into());
        }
        if snapshot.positions.iter().any(|position| {
            !position.qty.is_finite()
                || position.qty <= 0.0
                || !position.entry_price.is_finite()
                || position.entry_price <= 0.0
                || !matches!(position.side.as_str(), "long" | "short")
        }) {
            return Err("持仓快照中的方向或价量无效".into());
        }
        if mode.is_empty() {
            return Err("交易环境无效".into());
        }
        Ok(())
    }

    fn record_position_differences(
        conn: &rusqlite::Connection,
        mode: &str,
        snapshot: &crate::providers::TradingSnapshot,
    ) -> Result<(), String> {
        use std::collections::BTreeMap;
        let mut stmt = conn
            .prepare("SELECT symbol, side, qty FROM positions WHERE mode = ?1 AND status = 'open'")
            .map_err(|error| error.to_string())?;
        let rows = stmt
            .query_map([mode], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, f64>(2)?,
                ))
            })
            .map_err(|error| error.to_string())?;
        let mut quantities = BTreeMap::<(String, String), (f64, f64)>::new();
        for row in rows {
            let (symbol, side, qty) = row.map_err(|error| error.to_string())?;
            if !qty.is_finite() || qty <= 0.0 || !matches!(side.as_str(), "long" | "short") {
                return Err("本地持仓数量或方向无效".into());
            }
            quantities.entry((symbol, side)).or_default().0 += qty;
        }
        for position in &snapshot.positions {
            quantities
                .entry((position.symbol.clone(), position.side.clone()))
                .or_default()
                .1 += position.qty;
        }
        let mut differences = Vec::new();
        for ((symbol, side), (local, venue)) in quantities {
            if !local.is_finite() || !venue.is_finite() {
                return Err("持仓合计数量无效".into());
            }
            let tolerance = f64::EPSILON * 8.0 * local.max(venue);
            if (local - venue).abs() > tolerance {
                differences.push(serde_json::json!({"symbol": symbol, "side": side, "local_qty": local, "venue_qty": venue}));
            }
        }
        upsert_state(
            &conn,
            &format!("position_differences.{mode}"),
            &serde_json::json!({"differences": differences}).to_string(),
        )
    }

    fn record_order_differences(
        conn: &rusqlite::Connection,
        mode: &str,
        snapshot: &crate::providers::TradingSnapshot,
    ) -> Result<Value, String> {
        type OrderIdentity = (String, String, String);
        let mut stmt = conn.prepare("SELECT symbol, client_order_id, exchange_order_id FROM orders WHERE mode=?1 AND lower(status) IN ('new','pending','partially_filled') ORDER BY id")
            .map_err(|error| error.to_string())?;
        let rows = stmt
            .query_map([mode], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .map_err(|error| error.to_string())?;
        let local = rows
            .collect::<Result<Vec<OrderIdentity>, _>>()
            .map_err(|error| error.to_string())?;
        let venue: Vec<OrderIdentity> = snapshot
            .open_orders
            .iter()
            .map(|order| {
                (
                    order.symbol.clone(),
                    order.client_order_id.clone(),
                    order.order_id.to_string(),
                )
            })
            .collect();
        let matches = |local: &OrderIdentity, venue: &OrderIdentity| {
            local.0 == venue.0
                && if local.2.is_empty() {
                    !local.1.is_empty() && local.1 == venue.1
                } else {
                    local.2 == venue.2 && (local.1.is_empty() || local.1 == venue.1)
                }
        };
        let as_json = |(symbol, client_order_id, exchange_order_id): &OrderIdentity| serde_json::json!({"symbol":symbol,"client_order_id":client_order_id,"exchange_order_id":exchange_order_id});
        let local_only: Vec<_> = local
            .iter()
            .filter(|row| !venue.iter().any(|other| matches(row, other)))
            .map(as_json)
            .collect();
        let venue_only: Vec<_> = venue
            .iter()
            .filter(|row| !local.iter().any(|other| matches(other, row)))
            .map(as_json)
            .collect();
        let report = serde_json::json!({"local_only":local_only,"venue_only":venue_only});
        upsert_state(
            conn,
            &format!("order_differences.{mode}"),
            &report.to_string(),
        )?;
        Ok(report)
    }

    async fn tick(&self, config: &RuntimeConfig) -> Result<(), String> {
        let started = Instant::now();
        let mut failures: Vec<String> = Vec::new();
        let result = if config.enabled {
            match self.sync_exchange().await {
                Ok((mode, plans)) => {
                    let mut outcome = if mode == "live" {
                        match self.run_live(plans).await {
                            Ok(items) => {
                                failures = items;
                                Ok(())
                            }
                            Err(error) => Err(error),
                        }
                    } else {
                        Ok(())
                    };
                    // close_on_halt 的实盘平仓在快照事务里只落标记：撤单和发单必须走网络，
                    // 提交后在这里执行一次；标记读取即删除，后续轮次不重复全平。
                    if outcome.is_ok() {
                        match self.take_halt_flatten_pending(&mode) {
                            Ok(true) => {
                                // flatten 属于 EngineControl：复用撤单 + 市价平仓 + 本地结算的现有路径。
                                let control = EngineControl::new(
                                    self.db.clone(),
                                    self.master_key.clone(),
                                );
                                match control.flatten(false).await {
                                    Ok(value) => {
                                        self.events.record(
                                            "warn",
                                            "risk",
                                            "风控熔断平仓完成",
                                            json!({"mode": mode, "result": value}),
                                        );
                                    }
                                    Err(error) => {
                                        failures.push(format!("风控熔断平仓失败: {error}"));
                                        self.events.record(
                                            "error",
                                            "risk",
                                            "风控熔断平仓失败",
                                            json!({"mode": mode, "error": error}),
                                        );
                                        outcome = Err(format!("风控熔断平仓失败: {error}"));
                                    }
                                }
                            }
                            Ok(false) => {}
                            Err(error) => {
                                failures.push(format!("读取熔断平仓标记失败: {error}"));
                            }
                        }
                    }
                    outcome
                }
                Err(error) => Err(error),
            }
        } else {
            Ok(())
        };
        for failure in &failures {
            tracing::warn!(%failure, "实盘执行未完成");
            self.events.record(
                "error",
                "trade",
                &format!("实盘执行未完成: {failure}"),
                serde_json::json!({"detail": failure}),
            );
        }
        if let Err(error) = &result {
            self.events.record(
                "error",
                "engine",
                &format!("引擎循环异常: {error}"),
                serde_json::json!({"detail": error}),
            );
        }
        let elapsed = started.elapsed().as_millis() as i64;
        let mut conn = self
            .db
            .conn
            .lock()
            .map_err(|_| "数据库锁不可用".to_string())?;
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        upsert_state(
            &tx,
            "last_error",
            result.as_ref().err().map(String::as_str).unwrap_or(""),
        )?;
        upsert_state(
            &tx,
            "execution.failures",
            &serde_json::to_string(&failures).unwrap_or_else(|_| "[]".to_string()),
        )?;
        if result.is_ok() {
            upsert_state(&tx, "last_cycle_ms", &elapsed.to_string())?;
            upsert_state(&tx, "last_cycle_ts", &crate::now().to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())?;
        Self::write_heartbeat(&self.db.path);
        result
    }

    /// 心跳文件：守护脚本靠它判断「进程还在、引擎却不动了」。
    /// 引擎停用时后台同样在跑，所以这个文件只表示后台任务活着。
    /// 读取并清掉 close_on_halt 的实盘平仓标记。读取即删除，
/// 即使平仓失败也不会在后续轮次重复全平（失败只告警，交给人工处理）。
    fn take_halt_flatten_pending(&self, mode: &str) -> Result<bool, String> {
        let key = format!("close_on_halt.pending.{mode}");
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|_| "数据库锁不可用".to_string())?;
        let present: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM engine_state WHERE key=?1)",
                [&key],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if present {
            conn.execute("DELETE FROM engine_state WHERE key=?1", [&key])
                .map_err(|error| error.to_string())?;
        }
        Ok(present)
    }

    fn write_heartbeat(db_path: &str) {
        let Some(parent) = std::path::Path::new(db_path).parent() else {
            return;
        };
        let _ = std::fs::write(parent.join("engine.heartbeat"), crate::now().to_string());
    }

    pub(crate) async fn run(self, mut stop: watch::Receiver<bool>) {
        self.runtime_active.store(true, Ordering::SeqCst);
        self.set_runtime_task("alive");
        let mut last_config = RuntimeConfig::from_db(&self.db);
        let mut due = Instant::now();
        let mut last_venue_sync = Instant::now();
        loop {
            if *stop.borrow() {
                break;
            }
            if Instant::now() >= due {
                let config = RuntimeConfig::from_db(&self.db);
                self.cycle_busy.store(true, Ordering::SeqCst);
                // 周期里 panic 不能把整个后台任务带走：崩了就记账并停下来。
                // 否则面板只会显示「引擎未运行」，没人知道后台任务已经死了。
                let outcome = std::panic::AssertUnwindSafe(self.tick(&config))
                    .catch_unwind()
                    .await;
                self.cycle_busy.store(false, Ordering::SeqCst);
                match outcome {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        tracing::error!(%error, "Rust 引擎周期未执行");
                    }
                    Err(payload) => {
                        let detail = panic_detail(payload.as_ref());
                        tracing::error!(%detail, "Rust 引擎周期 panic");
                        self.events.record(
                            "error",
                            "engine",
                            "引擎周期 panic，引擎循环已停止",
                            serde_json::json!({"detail": detail}),
                        );
                        self.set_runtime_task("panic");
                        self.runtime_active.store(false, Ordering::SeqCst);
                        return;
                    }
                }
                due = Instant::now() + Duration::from_secs(config.interval_sec);
                last_config = config;
                continue;
            }
            // 每 2 秒回看一次配置：点「启动引擎」不能等满一个周期才生效。
            tokio::select! {
                biased;
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() { break; }
                }
                _ = wait_cycle_signal(&self.cycle_signal) => {
                    due = Instant::now();
                }
                _ = sleep(Duration::from_secs(2)) => {
                    let latest = RuntimeConfig::from_db(&self.db);
                    if latest.enabled != last_config.enabled
                        || latest.interval_sec != last_config.interval_sec
                    {
                        due = Instant::now();
                        last_config = latest;
                    }
                    // 手动平仓不经过引擎，主周期最长 10 分钟；这里 15 秒对一次持仓。
                    if last_venue_sync.elapsed() >= Duration::from_secs(15) {
                        last_venue_sync = Instant::now();
                        if let Err(error) = self.sync_venue_positions().await {
                            tracing::warn!(%error, "持仓快速对账失败");
                        }
                    }
                }
            }
        }
        self.runtime_active.store(false, Ordering::SeqCst);
        self.set_runtime_task("stopped");
        tracing::info!("Rust 引擎后台任务已停止");
    }

    /// 后台任务状态写进 engine_state：面板据此区分「引擎停了」和「任务崩了」。
    fn set_runtime_task(&self, value: &str) {
        if let Ok(conn) = self.db.conn.lock() {
            let _ = upsert_state(&conn, "runtime_task", value);
        }
    }
}

async fn wait_cycle_signal(signal: &Option<Arc<Notify>>) {
    match signal {
        Some(notify) => notify.notified().await,
        None => std::future::pending::<()>().await,
    }
}

fn truncate_body(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.chars().count() > 200 {
        format!("{}...", trimmed.chars().take(200).collect::<String>())
    } else {
        trimmed.to_string()
    }
}

/// 只读记录每个标的与整体市场的 regime，键按交易环境隔离（`symbol_regime.{mode}` / `market_regime.{mode}`）。
fn record_regimes(
    conn: &rusqlite::Connection,
    mode: &str,
    markets: &[crate::providers::MarketSnapshot],
    metrics: &std::collections::BTreeMap<&str, crate::providers::CandleMetrics>,
) -> Result<(), String> {
    let mut regimes = Vec::new();
    let mut symbol_regimes = serde_json::Map::new();
    for market in markets {
        if let Some(metric) = metrics.get(market.symbol.as_str()) {
            if let Some(regime) = crate::trade_policy::regime_for(
                &market.candles,
                market.mark_price,
                metric.atr,
                crate::trade_policy::MarketFeatures::default(),
            ) {
                regimes.push(regime);
                symbol_regimes.insert(market.symbol.clone(), json!(regime.as_str()));
            }
        }
    }
    if !symbol_regimes.is_empty() {
        upsert_state(
            conn,
            &format!("symbol_regime.{mode}"),
            &Value::Object(symbol_regimes).to_string(),
        )?;
    }
    match crate::regime::market_regime(&regimes) {
        Some(regime) => upsert_state(conn, &format!("market_regime.{mode}"), regime.as_str())?,
        None => tracing::warn!(
            valid = regimes.len(),
            "market_regime 有效标的不足 quorum，保留上一值"
        ),
    }
    Ok(())
}

fn upsert_state(conn: &rusqlite::Connection, key: &str, value: &str) -> Result<(), String> {
    conn.execute(
        "INSERT INTO engine_state(key,value,updated_at) VALUES(?1,?2,?3) ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
        (key, value, crate::now()),
    ).map(|_| ()).map_err(|e| e.to_string())
}

/// 读取某 mode 的 regime 状态机并推进一轮；切换时按新 regime 激活AI 调参。
///
/// C 期只做 regime 切换触发激活，不实现 C3 的自动启用与无候选回滚。
fn apply_regime_transition(conn: &rusqlite::Connection, mode: &str) -> Result<(), String> {
    apply_regime_transition_at(conn, mode, crate::now())
}

const REGIME_ACTIVATION_FAILURE_LOG_SEC: i64 = 3_600;

fn apply_regime_transition_at(
    conn: &rusqlite::Connection,
    mode: &str,
    now: i64,
) -> Result<(), String> {
    let read_state = |key: String| -> Option<String> {
        conn.query_row(
            "SELECT value FROM engine_state WHERE key=?1",
            [key],
            |row| row.get::<_, String>(0),
        )
        .ok()
    };
    let Some(next) = read_state(format!("market_regime.{mode}"))
        .and_then(|raw| crate::regime::Regime::from_str(&raw))
    else {
        // 本轮有效标的不足 quorum：保持当前 regime，不重置 pending。
        return Ok(());
    };
    let config = crate::settings_api::group_value_public(conn, "ai_tuning");
    let confirm_sec = config
        .get("regime_confirm_sec")
        .and_then(Value::as_i64)
        .unwrap_or(1_800)
        .max(0);
    let cooldown_sec = config
        .get("regime_cooldown_sec")
        .and_then(Value::as_i64)
        .unwrap_or(1_800)
        .max(0);
    let current = read_state(format!("regime.current.{mode}"))
        .and_then(|raw| crate::regime::Regime::from_str(&raw));
    let pending = read_state(format!("regime.pending.{mode}"))
        .and_then(|raw| crate::regime::Regime::from_str(&raw));
    let pending_since =
        read_state(format!("regime.pending_since.{mode}")).and_then(|raw| raw.parse::<i64>().ok());
    let changed_at = read_state(format!("regime.changed_at.{mode}"))
        .and_then(|raw| raw.parse::<i64>().ok())
        .unwrap_or(0);
    let mut tracker = crate::regime::RegimeTracker::restore(
        current,
        pending,
        pending_since,
        changed_at,
        confirm_sec,
        cooldown_sec,
    );
    let switched = tracker.observe(next, now);
    upsert_state(
        conn,
        &format!("regime.current.{mode}"),
        tracker.current().map(|r| r.as_str()).unwrap_or_default(),
    )?;
    upsert_state(
        conn,
        &format!("regime.pending.{mode}"),
        tracker.pending().map(|r| r.as_str()).unwrap_or_default(),
    )?;
    upsert_state(
        conn,
        &format!("regime.pending_since.{mode}"),
        &tracker
            .pending_since()
            .map(|value| value.to_string())
            .unwrap_or_default(),
    )?;
    upsert_state(
        conn,
        &format!("regime.changed_at.{mode}"),
        &tracker.changed_at().to_string(),
    )?;
    if let Some(regime) = switched {
        if let Err(error) =
            crate::ai_tuning::activate_for_regime(conn, regime.as_str(), "regime", mode)
        {
            // 激活失败不能把 regime 留在“已切换”状态：否则下轮不再触发激活，
            // 策略与 regime 永久错位。恢复旧 current/changed_at，并清空 pending，
            // 让下一轮按 market_regime 重新累计确认后重试。
            let previous = current.map(|r| r.as_str()).unwrap_or_default();
            let _ = upsert_state(conn, &format!("regime.current.{mode}"), previous);
            let _ = upsert_state(conn, &format!("regime.pending.{mode}"), "");
            let _ = upsert_state(conn, &format!("regime.pending_since.{mode}"), "");
            let _ = upsert_state(
                conn,
                &format!("regime.changed_at.{mode}"),
                &changed_at.to_string(),
            );
            // 首次失败后 current 为空，下轮会立即重试；事件与日志按小时限流，
            // 避免每 15 秒刷一条。
            let failed_key = format!("ai_tuning.regime_activation_failed_ts.{mode}");
            let last_failed =
                read_state(failed_key.clone()).and_then(|raw| raw.parse::<i64>().ok());
            let should_log =
                !last_failed.is_some_and(|last| now - last < REGIME_ACTIVATION_FAILURE_LOG_SEC);
            if should_log {
                tracing::warn!(%error, regime = regime.as_str(), %mode, "regime 切换激活AI 调参失败");
                let _ = upsert_state(conn, &failed_key, &now.to_string());
                let _ = conn.execute(
                    "INSERT INTO engine_events(ts,level,category,message,payload) \
                     VALUES(?1,'warn','ai_tuning',?2,?3)",
                    (
                        now,
                        format!("ai_tuning.regime_activation_failed: {error}"),
                        json!({
                            "mode": mode,
                            "regime": regime.as_str(),
                            "previous": previous,
                            "error": error,
                        })
                        .to_string(),
                    ),
                );
            }
        } else {
            // 激活成功即初始化守卫，计时从切换时刻开始。
            match crate::ai_tuning::active_set_for_regime(conn, regime.as_str()) {
                Ok(Some(set)) => {
                    if let Err(error) = crate::ai_tuning::set_guard(
                        conn,
                        regime.as_str(),
                        set.id,
                        now,
                    ) {
                        tracing::warn!(%error, regime = regime.as_str(), %mode, "AI 调参守卫初始化失败");
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(%error, regime = regime.as_str(), %mode, "读取激活AI 调参失败");
                }
            }
        }
    }
    Ok(())
}

/// 周期尾AI 调参治理：自动启用 approved 候选，再推进无候选守卫。
///
/// 调用方在 regime 切换处理后、同一事务内调用；错误由调用方告警，不阻断周期。
fn apply_ai_tuning_governance(
    conn: &rusqlite::Connection,
    mode: &str,
    live_timeframe: &str,
    has_candidate: bool,
    skipped: bool,
    now: i64,
) -> Result<(), String> {
    let current_regime = conn
        .query_row(
            "SELECT value FROM engine_state WHERE key=?1",
            [format!("regime.current.{mode}")],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|error| error.to_string())?
        .unwrap_or_default();
    if current_regime.is_empty() {
        return Ok(());
    }
    let auto_apply_error = match crate::ai_tuning::auto_apply_approved(
        conn,
        &current_regime,
        mode,
        live_timeframe,
        now,
    ) {
        Ok(Some(applied)) => {
            let applied_set = crate::ai_tuning::load_set(conn, applied)?;
            crate::ai_tuning::set_guard(conn, &applied_set.regime, applied, now)?;
            None
        }
        Ok(None) => None,
        Err(error) => {
            // 自动启用失败要留事件，不能只靠日志；guard 仍继续推进。
            if let Err(record_error) = conn.execute(
                "INSERT INTO engine_events(ts,level,category,message,payload) \
                 VALUES(?1,'warn','ai_tuning',?2,?3)",
                (
                    now,
                    format!("ai_tuning.auto_apply_failed: {error}"),
                    json!({
                        "mode": mode,
                        "regime": current_regime,
                        "error": &error,
                    })
                    .to_string(),
                ),
            ) {
                tracing::warn!(%record_error, "写入AI 调参自动启用失败事件失败");
            }
            Some(error)
        }
    };
    let Some(active) = crate::ai_tuning::active_set_for_regime(conn, &current_regime)? else {
        return auto_apply_error.map_or(Ok(()), Err);
    };
    // 守卫已把 active 集回滚标记写进自己的状态，这里不能再 set_guard 覆盖。
    crate::ai_tuning::update_guard(
        conn,
        &current_regime,
        active.id,
        has_candidate,
        skipped,
        now,
    )?;
    auto_apply_error.map_or(Ok(()), Err)
}

pub(crate) fn acquire_lock(data_dir: &Path) -> io::Result<File> {
    std::fs::create_dir_all(data_dir)?;
    // 与 legacy 内核（已退役） 共用锁文件。不能截断或删除，否则其他进程可能锁住不同的文件。
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(data_dir.join("engine.lock"))?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            LockFileEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
        };
        let mut overlapped: windows_sys::Win32::System::IO::OVERLAPPED =
            unsafe { std::mem::zeroed() };
        let result = unsafe {
            LockFileEx(
                file.as_raw_handle() as _,
                LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                0,
                1,
                0,
                &mut overlapped,
            )
        };
        if result == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(file)
}

pub(crate) fn stop_channel() -> (watch::Sender<bool>, watch::Receiver<bool>) {
    watch::channel(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn roi_min_r_reads_strategy_and_matches_backtest_config() {
        let strategy = json!({"roi_min_r": 2.5});
        let risk = json!({});
        let live = EngineRuntime::manage_config(&strategy, &risk);
        assert_eq!(live.roi_min_r, 2.5);
        assert_eq!(
            EngineRuntime::manage_config(&json!({}), &json!({})).roi_min_r,
            1.0,
            "strategy.roi_min_r 缺省 1.0"
        );
        let backtest = EngineRuntime::manage_config(&strategy, &risk);
        assert_eq!(backtest, live, "回测与实盘共用同一 manage_config");
    }

    #[test]
    fn scan_concurrency_falls_back_to_one_for_zero_or_invalid_values() {
        assert_eq!(
            EngineRuntime::scan_concurrency_of(&json!({"scan_concurrency": 0})),
            1,
            "0 不能当并发度，回退串行"
        );
        assert_eq!(
            EngineRuntime::scan_concurrency_of(&json!({"scan_concurrency": -3})),
            1
        );
        assert_eq!(
            EngineRuntime::scan_concurrency_of(&json!({"scan_concurrency": "fast"})),
            1
        );
        assert_eq!(EngineRuntime::scan_concurrency_of(&json!({})), 1);
        assert_eq!(
            EngineRuntime::scan_concurrency_of(&json!({"scan_concurrency": 3})),
            3
        );
        assert_eq!(
            EngineRuntime::scan_concurrency_of(&json!({"scan_concurrency": 99})),
            20,
            "并发度上限 20"
        );
    }

    #[test]
    fn scan_concurrency_matches_serial_candidates_without_duplicates() {
        let candidates: Vec<crate::scan::Candidate> = (0..40)
            .map(|index| crate::scan::Candidate {
                symbol: format!("SYM{index:03}USDT"),
                data_ok: true,
                atr_pct: 1.0 + index as f64 * 0.01,
                vol_ratio: 1.0,
                quote_volume: 1e8,
                spread_bps: 2.0,
                strength: index as f64 / 40.0,
                preview_side: None,
            })
            .collect();
        let serial = EngineRuntime::map_bounded(&candidates, 1, |candidate| candidate.symbol.clone());
        let parallel = EngineRuntime::map_bounded(&candidates, 4, |candidate| candidate.symbol.clone());
        assert_eq!(parallel, serial, "并发结果必须与串行完全一致且顺序不变");
        assert_eq!(
            serial,
            candidates
                .iter()
                .map(|candidate| candidate.symbol.clone())
                .collect::<Vec<_>>()
        );
        let unique: std::collections::BTreeSet<&String> = parallel.iter().collect();
        assert_eq!(
            unique.len(),
            parallel.len(),
            "并发扫描不能产生重复标的"
        );
    }

    #[test]
    fn the_liveness_threshold_follows_the_scan_interval() {
        // 扫描周期 10 分钟时，两轮之间的间歇期长达近 10 分钟：
        // 阈值必须跟着周期走，否则面板每 10 分钟有 7 分钟误报「引擎未运行」。
        assert_eq!(liveness_threshold_sec(600), 1260);
        // 短周期仍保底 180 秒
        assert_eq!(liveness_threshold_sec(15), 180);
        assert_eq!(liveness_threshold_sec(60), 180);
    }

    #[test]
    fn order_type_limit_price_uses_top_of_book_and_falls_back_to_mark() {
        assert_eq!(
            EngineRuntime::limit_entry_price("BUY", 99.5, 100.5, 100.0),
            99.5,
            "买单挂买一"
        );
        assert_eq!(
            EngineRuntime::limit_entry_price("SELL", 99.5, 100.5, 100.0),
            100.5,
            "卖单挂卖一"
        );
        assert_eq!(
            EngineRuntime::limit_entry_price("BUY", 0.0, 0.0, 100.0),
            100.0,
            "没有盘口时退回标记价"
        );
        assert_eq!(
            EngineRuntime::limit_entry_price("SELL", f64::NAN, 0.0, 101.0),
            101.0,
            "盘口非有限值同样退回标记价"
        );

        assert_eq!(LiveOrderType::from_setting(None), LiveOrderType::Market);
        assert_eq!(
            LiveOrderType::from_setting(Some("market")),
            LiveOrderType::Market
        );
        assert_eq!(
            LiveOrderType::from_setting(Some("limit_post_only")),
            LiveOrderType::LimitPostOnly
        );
        assert_eq!(
            LiveOrderType::from_setting(Some("unknown")),
            LiveOrderType::Market,
            "未知配置按市价处理，不能发出不合法的订单类型"
        );
        assert_eq!(LiveOrderType::Market.as_str(), "MARKET");
        assert_eq!(LiveOrderType::LimitPostOnly.as_str(), "LIMIT");
    }

    #[test]
    fn max_spread_bps_setting_rejects_a_wide_book() {
        let gates = EngineRuntime::scan_gates_of(&serde_json::json!({"max_spread_bps": 2.0}));
        assert_eq!(gates.spread_bps_max, 2.0, "界面字段 max_spread_bps 必须生效");
        let candidate = crate::scan::Candidate {
            symbol: "BTCUSDT".into(),
            data_ok: true,
            atr_pct: gates.atr_pct_min + 0.1,
            vol_ratio: gates.volume_ratio_min + 0.1,
            quote_volume: gates.min_24h_quote_volume + 1.0,
            spread_bps: 3.0,
            strength: 0.5,
            preview_side: None,
        };
        assert!(
            crate::scan::risk_gate(&candidate, &gates).contains("盘口点差过大"),
            "点差 3bp 超过上限 2bp 必须被闸门拒绝"
        );

        let legacy = EngineRuntime::scan_gates_of(&serde_json::json!({"spread_bps_max": 4.0}));
        assert_eq!(legacy.spread_bps_max, 4.0, "旧键仍要兼容");
        let fallback = EngineRuntime::scan_gates_of(&serde_json::json!({}));
        assert_eq!(fallback.spread_bps_max, 8.0, "两者都缺时缺省 8bp");
        let both = EngineRuntime::scan_gates_of(
            &serde_json::json!({"max_spread_bps": 2.0, "spread_bps_max": 9.0}),
        );
        assert_eq!(both.spread_bps_max, 2.0, "新键优先于旧键");
    }

    fn test_runtime(db: &Db) -> EngineRuntime {
        let lock = File::open(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap();
        EngineRuntime::new(
            db.clone(),
            lock,
            b"MDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDA=".to_vec(),
            crate::events::EventBus::new(None),
        )
        .with_reviewer(Arc::new(TestApproveReviewer))
    }

    struct TestApproveReviewer;

    impl crate::review::AiReviewer for TestApproveReviewer {
        fn review(
            &self,
            proposal: &crate::review::EntryProposal,
        ) -> Result<crate::review::ReviewDecision, String> {
            let Some(side) = proposal.quant_side.clone() else {
                return Ok(crate::review::ReviewDecision {
                    action: crate::review::ReviewAction::Reject,
                    side: None,
                    leverage: None,
                    stop_price: None,
                    take_profit: None,
                    reason: "flat 候选不自动开仓".to_string(),
                    confidence: 0.0,
                });
            };
            Ok(crate::review::ReviewDecision {
                action: crate::review::ReviewAction::Agree,
                side: Some(side),
                leverage: None,
                stop_price: None,
                take_profit: Some(proposal.take_profit),
                reason: "test approve".to_string(),
                confidence: 0.0,
            })
        }
    }

    #[test]
    fn runtime_wires_production_ai_reviewer() {
        let db = test_db();
        let runtime = test_runtime(&db);
        assert!(
            runtime.reviewer.is_some(),
            "生产 runtime 必须注入 AiReviewer，不能保持 None"
        );
    }

    #[test]
    fn engine_lock_rejects_second_owner_and_can_be_reacquired() {
        let dir = std::env::temp_dir().join(format!(
            "nanoquant-lock-{}-{}",
            std::process::id(),
            crate::now_millis()
        ));
        let first = acquire_lock(&dir).unwrap();
        assert!(acquire_lock(&dir).is_err());
        drop(first);
        drop(acquire_lock(&dir).unwrap());
        std::fs::remove_file(dir.join("engine.lock")).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    #[tokio::test]
    async fn snapshot_records_scan_and_honors_halt_and_stale_candles() {
        use crate::providers::{DemoProvider, SnapshotProvider};
        let db = test_db();
        let runtime = test_runtime(&db);
        let mut snapshot = DemoProvider::with_mark_price(100.0)
            .snapshot()
            .await
            .unwrap();
        for candle in &mut snapshot.markets[0].candles {
            candle.high = 101.0;
            candle.low = 99.0;
        }
        let read_scan = || {
            let conn = db.conn.lock().unwrap();
            let raw: String = conn
                .query_row(
                    "SELECT value FROM engine_state WHERE key = 'cycle_preview.demo'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            serde_json::from_str::<Value>(&raw).unwrap()
        };
        runtime.persist_snapshot("demo", &snapshot).unwrap();
        assert_eq!(read_scan()["skipped"], "引擎未启用");
        assert_eq!(read_scan()["scan_candidates"], serde_json::json!([]));
        db.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE app_settings SET value=?1 WHERE key='engine'",
                [r#"{"enabled":true,"mode":"demo"}"#],
            )
            .unwrap();
        runtime.persist_snapshot("demo", &snapshot).unwrap();
        assert_eq!(
            read_scan()["scan_candidates"],
            serde_json::json!(["BTCUSDT"])
        );
        upsert_state(&db.conn.lock().unwrap(), "halted", "手动暂停").unwrap();
        runtime.persist_snapshot("demo", &snapshot).unwrap();
        let preview = read_scan();
        assert_eq!(preview["scan_candidates"], serde_json::json!([]));
        assert!(preview["skipped"].as_str().unwrap().contains("熔断"));
        {
            let conn = db.conn.lock().unwrap();
            upsert_state(&conn, "halted", "").unwrap();
            upsert_state(
                &conn,
                "equity_peak",
                &(snapshot.account.equity * 3.0).to_string(),
            )
            .unwrap();
        }
        runtime.persist_snapshot("demo", &snapshot).unwrap();
        let preview = read_scan();
        assert_eq!(preview["scan_candidates"], serde_json::json!([]));
        assert!(preview["skipped"].as_str().unwrap().contains("最大回撤"));
        assert_eq!(preview["rejected"], serde_json::json!({}));
        {
            let conn = db.conn.lock().unwrap();
            upsert_state(&conn, "equity_peak", "0").unwrap();
        }
        for candle in &mut snapshot.markets[0].candles {
            candle.close_time -= 48 * 60 * 60 * 1_000;
        }
        runtime.persist_snapshot("demo", &snapshot).unwrap();
        assert_eq!(read_scan()["scan_candidates"], serde_json::json!([]));
        assert!(read_scan()["rejected"]["BTCUSDT"]
            .as_str()
            .unwrap()
            .contains("行情"));
        let audited: i64 = db
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM signals WHERE mode='demo' AND decision='rejected'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(audited > 0, "行情闸门挡下的标的也要进信号审计");
    }

    #[tokio::test]
    async fn snapshot_records_symbol_regime_without_market_regime_below_quorum() {
        use crate::providers::{DemoProvider, SnapshotProvider};
        let db = test_db();
        let runtime = test_runtime(&db);
        let mut snapshot = DemoProvider::with_mark_price(100.0)
            .snapshot()
            .await
            .unwrap();
        for candle in &mut snapshot.markets[0].candles {
            candle.high = 100.5;
            candle.low = 99.5;
        }
        runtime.persist_snapshot("demo", &snapshot).unwrap();
        let conn = db.conn.lock().unwrap();
        let raw: String = conn
            .query_row(
                "SELECT value FROM engine_state WHERE key = 'symbol_regime.demo'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let symbol_regimes: Value = serde_json::from_str(&raw).unwrap();
        let symbol_regime = symbol_regimes
            .get("BTCUSDT")
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(
            matches!(symbol_regime, "trend" | "range" | "chaos"),
            "单标的 regime 必须是合法枚举值: {raw}"
        );
        let market_regime: Option<String> = conn
            .query_row(
                "SELECT value FROM engine_state WHERE key = 'market_regime.demo'",
                [],
                |row| row.get(0),
            )
            .optional()
            .unwrap();
        assert!(
            market_regime.is_none(),
            "有效标的不足 6 个时不应写出 market_regime"
        );
    }

    #[tokio::test]
    async fn record_regimes_writes_market_regime_with_full_quorum() {
        use crate::providers::{DemoProvider, SnapshotProvider};
        let db = test_db();
        let snapshot = DemoProvider::with_mark_price(100.0)
            .snapshot()
            .await
            .unwrap();
        let template = snapshot.markets[0].clone();
        let symbols = [
            "AAAUSDT", "BBBUSDT", "CCCUSDT", "DDDUSDT", "EEEUSDT", "FFFUSDT",
        ];
        let markets: Vec<_> = symbols
            .iter()
            .map(|symbol| {
                let mut market = template.clone();
                market.symbol = (*symbol).into();
                for candle in &mut market.candles {
                    candle.high = 100.5;
                    candle.low = 99.5;
                }
                market
            })
            .collect();
        let metrics: std::collections::BTreeMap<_, _> = markets
            .iter()
            .map(|market| {
                (
                    market.symbol.as_str(),
                    crate::providers::candle_metrics(&market.candles, 14).unwrap(),
                )
            })
            .collect();
        let mut conn = db.conn.lock().unwrap();
        let tx = conn.transaction().unwrap();
        record_regimes(&tx, "demo", &markets, &metrics).unwrap();
        let market_regime: String = tx
            .query_row(
                "SELECT value FROM engine_state WHERE key = 'market_regime.demo'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            matches!(market_regime.as_str(), "trend" | "range" | "chaos"),
            "6 个有效标的应写出合法 market_regime: {market_regime}"
        );
        let raw: String = tx
            .query_row(
                "SELECT value FROM engine_state WHERE key = 'symbol_regime.demo'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let symbol_regimes: Value = serde_json::from_str(&raw).unwrap();
        for symbol in symbols {
            let value = symbol_regimes
                .get(symbol)
                .and_then(Value::as_str)
                .unwrap_or_default();
            assert!(
                matches!(value, "trend" | "range" | "chaos"),
                "{symbol} 的 regime 必须是合法枚举值: {raw}"
            );
        }
        tx.commit().unwrap();
    }

    #[tokio::test]
    async fn single_symbol_persist_keeps_existing_market_regime() {
        use crate::providers::{DemoProvider, SnapshotProvider};
        let db = test_db();
        let runtime = test_runtime(&db);
        let mut snapshot = DemoProvider::with_mark_price(100.0)
            .snapshot()
            .await
            .unwrap();
        for candle in &mut snapshot.markets[0].candles {
            candle.high = 100.5;
            candle.low = 99.5;
        }
        upsert_state(&db.conn.lock().unwrap(), "market_regime.demo", "chaos").unwrap();
        runtime.persist_snapshot("demo", &snapshot).unwrap();
        let value: String = db
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT value FROM engine_state WHERE key = 'market_regime.demo'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(value, "chaos", "不足 quorum 不能覆盖上一轮 market_regime");
    }

    #[tokio::test]
    async fn empty_symbol_regimes_keep_previous_values() {
        use crate::providers::{DemoProvider, SnapshotProvider};
        let db = test_db();
        let runtime = test_runtime(&db);
        let snapshot = DemoProvider::with_mark_price(100.0)
            .snapshot()
            .await
            .unwrap();
        upsert_state(
            &db.conn.lock().unwrap(),
            "symbol_regime.demo",
            r#"{"BTCUSDT":"range"}"#,
        )
        .unwrap();
        runtime.persist_snapshot("demo", &snapshot).unwrap();
        let value: String = db
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT value FROM engine_state WHERE key = 'symbol_regime.demo'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            value, r#"{"BTCUSDT":"range"}"#,
            "空 regime 映射不能清空历史记录"
        );
    }

    #[tokio::test]
    async fn every_cycle_leaves_a_signal_audit_row() {
        let (db, snapshot) = entry_fixture().await;
        let runtime = test_runtime(&db);
        runtime.persist_snapshot("demo", &snapshot).unwrap();
        let conn = db.conn.lock().unwrap();
        let rows: Vec<(String, String, String, i64)> = conn
            .prepare(
                "SELECT symbol, decision, reject_reason, repeats FROM signals WHERE mode='demo'",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .flatten()
            .collect();
        assert!(!rows.is_empty(), "每轮扫描都必须留下信号审计行");
        assert!(
            rows.iter().any(|(_, decision, _, _)| decision == "opened"),
            "开仓那一刀要记成已开仓: {rows:?}"
        );
        assert!(
            rows.iter().all(|(_, _, reason, _)| !reason.is_empty()
                || rows.iter().any(|(_, decision, _, _)| decision == "opened")),
            "被拒的行必须写清楚原因: {rows:?}"
        );
    }

    #[tokio::test]
    async fn gate_rejections_keep_preview_direction_score_and_stage() {
        let (db, mut snapshot) = entry_fixture().await;
        let market = &mut snapshot.markets[0];
        let len = market.candles.len();
        for candle in market.candles.iter_mut().take(len - 1).skip(len - 20) {
            candle.volume = 100.0;
        }
        market.candles.last_mut().unwrap().volume = 0.01;
        let runtime = test_runtime(&db);
        runtime.persist_snapshot("demo", &snapshot).unwrap();
        let conn = db.conn.lock().unwrap();
        let (direction, score, features): (String, f64, String) = conn
            .query_row(
                "SELECT direction, score, features FROM signals WHERE mode='demo' AND decision='rejected' \
                 AND reject_reason LIKE '%量能%' ORDER BY id DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert!(
            direction == "long" || direction == "short",
            "初筛拒绝行应保留预览方向: {direction}"
        );
        assert!(score.abs() > 0.0, "初筛拒绝行应保留预览评分: {score}");
        let value: serde_json::Value = serde_json::from_str(&features).unwrap();
        assert_eq!(value["stage"], "gate");
    }

    struct HoldReviewer;

    impl crate::review::AiReviewer for HoldReviewer {
        fn review(
            &self,
            _proposal: &crate::review::EntryProposal,
        ) -> Result<crate::review::ReviewDecision, String> {
            Ok(crate::review::ReviewDecision {
                action: crate::review::ReviewAction::Hold,
                side: None,
                leverage: None,
                stop_price: None,
                take_profit: None,
                reason: "测试观望".to_string(),
                confidence: 0.62,
            })
        }
    }

    #[tokio::test]
    async fn ai_hold_keeps_preview_direction_score_and_stage() {
        let (db, mut snapshot) = entry_fixture().await;
        let market = &mut snapshot.markets[0];
        market.funding_rate = 0.0;
        market.open_interest_change_pct = 0.0;
        let runtime = test_runtime(&db).with_reviewer(Arc::new(HoldReviewer));
        runtime.persist_snapshot("demo", &snapshot).unwrap();
        let conn = db.conn.lock().unwrap();
        let (direction, score, features, ai_action, ai_confidence, decision): (
            String,
            f64,
            String,
            String,
            f64,
            String,
        ) = conn
            .query_row(
                "SELECT direction, score, features, ai_action, ai_confidence, decision \
                 FROM signals WHERE mode='demo' AND ai_action='hold' ORDER BY id DESC LIMIT 1",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(direction, "long", "AI 观望行要保留量化预评分方向");
        assert!(score > 0.0, "AI 观望行要保留量化预评分: {score}");
        let value: serde_json::Value = serde_json::from_str(&features).unwrap();
        assert_eq!(value["stage"], "evaluate");
        assert_eq!(value["preview_side"], "long");
        assert!(value["quant_side"].is_null());
        assert_eq!(value["quant_passed"], false);
        assert!(value["atr_pct"].as_f64().is_some_and(|v| v > 0.0), "AI 复核行要带顶层 ATR");
        assert!(value["vol_ratio"].as_f64().is_some(), "AI 复核行要带顶层量比");
        assert!(value["quote_volume"].as_f64().is_some(), "AI 复核行要带顶层 24h 成交额");
        assert!(value["spread_bps"].as_f64().is_some(), "AI 复核行要带顶层点差");
        let preview_score = value["preview_score"].as_f64().unwrap();
        assert!((preview_score - score).abs() < 1e-9);
        assert_eq!(ai_action, "hold");
        assert!((ai_confidence - 0.62).abs() < 1e-9, "AI 置信度不能被清零");
        assert_eq!(decision, "rejected");
    }

    #[tokio::test]
    async fn demo_signal_opens_once_with_sized_quantity_and_protection() {
        let (db, snapshot) = entry_fixture().await;
        let runtime = test_runtime(&db);
        runtime.persist_snapshot("demo", &snapshot).unwrap();
        runtime.persist_snapshot("demo", &snapshot).unwrap();
        let conn = db.conn.lock().unwrap();
        let (count, qty, leverage, sl, tp): (i64, f64, f64, f64, f64) = conn
            .query_row("SELECT COUNT(*),COALESCE(MAX(qty),0),COALESCE(MAX(leverage),0),COALESCE(MAX(sl_price),0),COALESCE(MAX(tp_price),0) FROM positions WHERE mode='demo' AND status='open' AND side='long'", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))
            .unwrap();
        assert_eq!(count, 1);
        assert!((qty - 50.0).abs() < 1e-9);
        assert_eq!(leverage, 5.0);
        assert!(sl > 0.0 && sl < 100.0 && tp > 100.0);
        let orders: i64 = conn.query_row("SELECT COUNT(*) FROM orders WHERE purpose='entry' AND status='FILLED' AND reduce_only=0", [], |r| r.get(0)).unwrap();
        assert_eq!(orders, 1);
        let features: String = conn
            .query_row(
                "SELECT features FROM positions WHERE mode='demo' AND status='open'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let features: Value = serde_json::from_str(&features).unwrap();
        assert!(
            !features["injected_mantra_ids"]
                .as_array()
                .unwrap()
                .is_empty(),
            "开仓前要记录注入了哪些心法: {features}"
        );
        assert!(
            features["injected_experience_ids"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!(1)),
            "同币种同向的赢家经验要注入: {features}"
        );
        let (journal_status, journal_mode, journal_direction, journal_entry): (String, String, String, f64) = conn
            .query_row(
                "SELECT status, mode, direction, entry_price FROM decision_journal ORDER BY id DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(journal_status, "pending", "开仓要留下待回访的决策快照");
        assert_eq!(journal_mode, "demo", "快照必须绑定开仓环境");
        assert_eq!(journal_direction, "long");
        assert_eq!(journal_entry, 100.0);
    }

    #[tokio::test]
    async fn live_entry_reconciles_after_restart_at_actual_fill_price() {
        let (db, snapshot) = entry_fixture().await;
        let runtime = test_runtime(&db);
        let plans = runtime.persist_snapshot("live", &snapshot).unwrap();
        assert_eq!(plans.len(), 1);
        let plan = &plans[0];
        assert_eq!(plan.purpose, "entry");
        runtime
            .reserve_order(plan, "nq-entry-test", plan.qty)
            .unwrap();
        assert!(runtime
            .persist_snapshot("live", &snapshot)
            .unwrap()
            .is_empty());
        let gateway = FakeLiveGateway::new(vec![
            (200, r#"{"dualSidePosition":false}"#),
            (
                200,
                r#"{"orderId":42,"status":"FILLED","executedQty":"2","avgPrice":"100.1"}"#,
            ),
            (200, r#"{"algoId":12}"#),
            (200, r#"{"algoId":13}"#),
        ]);
        let restarted = test_runtime(&db);
        let failures = restarted
            .run_live_with(&live_creds(), &gateway, vec![])
            .await
            .unwrap();
        assert!(failures.is_empty(), "{failures:?}");
        let urls = gateway.sent_urls();
        assert_eq!(urls.len(), 4);
        assert!(urls[0].contains("/fapi/v1/positionSide/dual"));
        assert!(urls[1].contains("origClientOrderId=nq-entry-test"));
        let conn = db.conn.lock().unwrap();
        let position: (String, f64, f64, f64) = conn
            .query_row(
                "SELECT side,qty,entry_price,leverage FROM positions WHERE mode='live'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(position, ("long".into(), 2.0, 100.1, 5.0));
    }

    #[test]
    fn live_order_type_fee_rate_uses_maker_only_for_post_only_entries() {
        let plan = |order_type: LiveOrderType, purpose: &str| LivePlan {
            symbol: "BTCUSDT".into(),
            position_side: "long".into(),
            side: "BUY".into(),
            qty: 1.0,
            purpose: purpose.into(),
            mark_price: 100.0,
            order_type,
            price: None,
            entry: None,
            features: None,
        };
        assert_eq!(
            EngineRuntime::plan_fee_rate(&plan(LiveOrderType::LimitPostOnly, "entry"), 0.0005, 0.0002),
            0.0002,
            "maker 开仓成交用 maker 费率"
        );
        assert_eq!(
            EngineRuntime::plan_fee_rate(&plan(LiveOrderType::Market, "entry"), 0.0005, 0.0002),
            0.0005,
            "市价开仓仍用 taker 费率"
        );
        assert_eq!(
            EngineRuntime::plan_fee_rate(&plan(LiveOrderType::LimitPostOnly, "stop_loss"), 0.0005, 0.0002),
            0.0005,
            "平仓始终是 taker，即使订单类型写着 LIMIT"
        );
    }

    #[tokio::test]
    async fn live_order_type_limit_post_only_entry_waits_without_position_or_replan() {
        let (db, snapshot) = entry_fixture().await;
        set_engine_settings(
            &db,
            r#"{"enabled":true,"mode":"live","order_type":"limit_post_only"}"#,
        );
        let runtime = test_runtime(&db);
        let plans = runtime.persist_snapshot("live", &snapshot).unwrap();
        assert_eq!(plans.len(), 1);
        let plan = plans[0].clone();
        assert_eq!(plan.order_type, LiveOrderType::LimitPostOnly);
        assert_eq!(plan.side, "BUY");
        let bid = snapshot.markets[0].bid;
        assert!(
            (plan.price.unwrap() - bid).abs() < 1e-9,
            "买单要挂在买一，拿到 {:?} 而非 {bid}",
            plan.price
        );

        let leverage = plan.entry.as_ref().unwrap().leverage.round() as i64;
        let leverage_body = Box::leak(
            format!(r#"{{"symbol":"BTCUSDT","leverage":{leverage}}}"#).into_boxed_str(),
        );
        let gateway = FakeLiveGateway::new(vec![
            (200, r#"{"dualSidePosition":false}"#),
            (200, leverage_body),
            (
                200,
                r#"{"orderId":55,"status":"NEW","executedQty":"0","avgPrice":"0"}"#,
            ),
        ]);
        let failures = runtime
            .run_live_with(&live_creds(), &gateway, vec![plan])
            .await
            .unwrap();
        assert!(failures.is_empty(), "挂单不是失败：{failures:?}");
        let urls = gateway.sent_urls();
        assert_eq!(urls.len(), 4, "挂单后同轮还会对账查询一次");
        assert_eq!(
            urls.iter().filter(|url| url.contains("type=LIMIT")).count(),
            1,
            "限价开仓只发一次，后续只是查询"
        );
        assert!(urls[2].contains("type=LIMIT"), "{}", urls[2]);
        assert!(urls[2].contains("timeInForce=GTX"), "{}", urls[2]);
        assert!(urls[2].contains("price=99.99"), "{}", urls[2]);
        assert!(urls[3].contains("origClientOrderId="), "{}", urls[3]);
        {
            let conn = db.conn.lock().unwrap();
            let (order_type, status): (String, String) = conn
                .query_row(
                    "SELECT order_type,status FROM orders WHERE mode='live'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(order_type, "LIMIT", "reserve_order 不能继续写 MARKET");
            assert_eq!(status, "NEW");
            let positions: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM positions WHERE mode='live'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(positions, 0, "未成交的限价单不能当成已开仓");
        }
        let replanned = runtime.persist_snapshot("live", &snapshot).unwrap();
        assert!(replanned.is_empty(), "挂单未完成时不能重复发单");
    }

    #[tokio::test]
    async fn live_order_type_limit_entry_rejection_is_not_resent() {
        let (db, snapshot) = entry_fixture().await;
        set_engine_settings(
            &db,
            r#"{"enabled":true,"mode":"live","order_type":"limit_post_only"}"#,
        );
        let runtime = test_runtime(&db);
        let plans = runtime.persist_snapshot("live", &snapshot).unwrap();
        let leverage = plans[0].entry.as_ref().unwrap().leverage.round() as i64;
        let leverage_body = Box::leak(
            format!(r#"{{"symbol":"BTCUSDT","leverage":{leverage}}}"#).into_boxed_str(),
        );
        let gateway = FakeLiveGateway::new(vec![
            (200, r#"{"dualSidePosition":false}"#),
            (200, leverage_body),
            (
                400,
                r#"{"code":-2021,"msg":"Order would immediately match and take."}"#,
            ),
        ]);
        let failures = runtime
            .run_live_with(&live_creds(), &gateway, plans)
            .await
            .unwrap();
        assert!(
            failures.iter().any(|failure| failure.contains("-2021")),
            "拒单原因必须上报：{failures:?}"
        );
        assert_eq!(gateway.sent_count(), 3, "挂单被拒必须放弃，不能追价改市价");
        let conn = db.conn.lock().unwrap();
        let (order_type, status): (String, String) = conn
            .query_row(
                "SELECT order_type,status FROM orders WHERE mode='live'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(order_type, "LIMIT");
        assert_eq!(status, "REJECTED");
        let positions: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM positions WHERE mode='live'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(positions, 0);
    }

    #[tokio::test]
    async fn live_order_type_unknown_entry_is_not_resent() {
        let (db, snapshot) = entry_fixture().await;
        set_engine_settings(
            &db,
            r#"{"enabled":true,"mode":"live","order_type":"limit_post_only"}"#,
        );
        let runtime = test_runtime(&db);
        let plans = runtime.persist_snapshot("live", &snapshot).unwrap();
        let leverage = plans[0].entry.as_ref().unwrap().leverage.round() as i64;
        let leverage_body = Box::leak(
            format!(r#"{{"symbol":"BTCUSDT","leverage":{leverage}}}"#).into_boxed_str(),
        );
        let gateway = FakeLiveGateway::new(vec![
            (200, r#"{"dualSidePosition":false}"#),
            (200, leverage_body),
            (0, ""),
            (0, ""),
        ]);
        runtime
            .run_live_with(&live_creds(), &gateway, plans)
            .await
            .unwrap();
        assert_eq!(
            gateway.sent_count(),
            5,
            "未知结果只查询，绝不重发第二笔挂单"
        );
        let urls = gateway.sent_urls();
        assert_eq!(
            urls.iter().filter(|url| url.contains("type=LIMIT")).count(),
            1,
            "只有一笔真实的限价发单"
        );
        {
            let conn = db.conn.lock().unwrap();
            let status: String = conn
                .query_row(
                    "SELECT status FROM orders WHERE mode='live'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(status, "UNKNOWN");
            let positions: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM positions WHERE mode='live'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(positions, 0);
        }
        let replanned = runtime.persist_snapshot("live", &snapshot).unwrap();
        assert!(replanned.is_empty(), "结果未知的挂单不能重复发单");
    }

    #[tokio::test]
    async fn live_order_type_limit_fill_charges_the_maker_fee() {
        let (db, snapshot) = entry_fixture().await;
        set_engine_settings(
            &db,
            r#"{"enabled":true,"mode":"live","order_type":"limit_post_only"}"#,
        );
        let runtime = test_runtime(&db);
        let plans = runtime.persist_snapshot("live", &snapshot).unwrap();
        let leverage = plans[0].entry.as_ref().unwrap().leverage.round() as i64;
        let leverage_body = Box::leak(
            format!(r#"{{"symbol":"BTCUSDT","leverage":{leverage}}}"#).into_boxed_str(),
        );
        let gateway = FakeLiveGateway::new(vec![
            (200, r#"{"dualSidePosition":false}"#),
            (200, leverage_body),
            (
                200,
                r#"{"orderId":66,"status":"FILLED","executedQty":"2","avgPrice":"99.5"}"#,
            ),
            (200, r#"{"algoId":101}"#),
            (200, r#"{"algoId":102}"#),
        ]);
        let failures = runtime
            .run_live_with(&live_creds(), &gateway, plans)
            .await
            .unwrap();
        assert!(failures.is_empty(), "{failures:?}");
        let conn = db.conn.lock().unwrap();
        let (qty, entry_price, fees): (f64, f64, f64) = conn
            .query_row(
                "SELECT qty,entry_price,fees FROM positions WHERE mode='live' AND status='open'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!((qty, entry_price), (2.0, 99.5));
        let expected = 2.0 * 99.5 * 0.0002;
        assert!(
            (fees - expected).abs() < 1e-12,
            "maker 成交手续费应为 {expected}，实际 {fees}"
        );
    }

    #[tokio::test]
    async fn live_entry_requires_confirmed_leverage_before_sending_order() {
        let (db, snapshot) = entry_fixture().await;
        let runtime = test_runtime(&db);
        let plans = runtime.persist_snapshot("live", &snapshot).unwrap();
        assert_eq!(plans.len(), 1);
        let gateway = FakeLiveGateway::new(vec![
            (200, r#"{"dualSidePosition":false}"#),
            (200, r#"{"symbol":"BTCUSDT","leverage":4}"#),
        ]);
        let failures = runtime
            .run_live_with(&live_creds(), &gateway, plans)
            .await
            .unwrap();
        assert_eq!(failures.len(), 1);
        assert_eq!(gateway.sent_count(), 2);
        assert!(gateway.sent_urls()[0].contains("/fapi/v1/positionSide/dual"));
        assert!(gateway.sent_urls()[1].contains("/fapi/v1/leverage"));
        let orders: i64 = db
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM orders", [], |r| r.get(0))
            .unwrap();
        assert_eq!(orders, 0);
    }

    fn seed_limit_entry_order(db: &Db, ts: i64, status: &str) {
        db.conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO orders(ts,symbol,side,order_type,qty,price,status,purpose,\
                 client_order_id,exchange_order_id,reduce_only,mode,latency_ms) \
                 VALUES(?1,'BTCUSDT','BUY','LIMIT',1,99,?2,'entry','nq-entry-old','',0,'live',0)",
                (ts, status),
            )
            .unwrap();
    }

    #[tokio::test]
    async fn limit_order_entry_is_canceled_after_timeout() {
        let db = test_db();
        set_engine_settings(
            &db,
            r#"{"enabled":true,"mode":"live","order_type":"limit_post_only"}"#,
        );
        seed_limit_entry_order(&db, crate::now() - 300, "NEW");
        let runtime = test_runtime_with_events(&db);
        let gateway = FakeLiveGateway::new(vec![
            (200, r#"{"dualSidePosition":false}"#),
            (
                200,
                r#"{"orderId":9,"status":"NEW","executedQty":"0","avgPrice":"0"}"#,
            ),
            (
                200,
                r#"{"orderId":9,"status":"CANCELED","executedQty":"0","avgPrice":"0"}"#,
            ),
        ]);
        let failures = runtime
            .run_live_with(&live_creds(), &gateway, vec![])
            .await
            .unwrap();
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(
            gateway.sent_count(),
            3,
            "超时后只查一次、撤一次，不能重发新单"
        );
        {
            let conn = db.conn.lock().unwrap();
            let status: String = conn
                .query_row(
                    "SELECT status FROM orders WHERE client_order_id='nq-entry-old'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(status, "CANCELED", "超时未成交的 LIMIT 开仓必须标记撤销");
            let events: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM engine_events WHERE message LIKE '%已撤单%'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(events > 0, "撤单必须写事件");
        }
        let inflight = {
            let mut conn = db.conn.lock().unwrap();
            let tx = conn.transaction().unwrap();
            EngineRuntime::live_entry_inflight(&tx, "BTCUSDT").unwrap()
        };
        assert!(!inflight, "撤单成功后同标的 inflight 必须解除");
    }

    #[tokio::test]
    async fn limit_order_entry_is_not_canceled_before_timeout() {
        let db = test_db();
        set_engine_settings(
            &db,
            r#"{"enabled":true,"mode":"live","order_type":"limit_post_only"}"#,
        );
        seed_limit_entry_order(&db, crate::now(), "NEW");
        let runtime = test_runtime(&db);
        let gateway = FakeLiveGateway::new(vec![
            (200, r#"{"dualSidePosition":false}"#),
            (
                200,
                r#"{"orderId":9,"status":"NEW","executedQty":"0","avgPrice":"0"}"#,
            ),
        ]);
        let failures = runtime
            .run_live_with(&live_creds(), &gateway, vec![])
            .await
            .unwrap();
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(gateway.sent_count(), 2, "未超时不能发撤单请求");
        let status: String = db
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT status FROM orders WHERE client_order_id='nq-entry-old'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "NEW");
    }

    #[tokio::test]
    async fn limit_order_cancel_unknown_keeps_unknown_and_is_not_resent() {
        let db = test_db();
        set_engine_settings(
            &db,
            r#"{"enabled":true,"mode":"live","order_type":"limit_post_only"}"#,
        );
        seed_limit_entry_order(&db, crate::now() - 300, "NEW");
        let runtime = test_runtime_with_events(&db);
        let gateway = FakeLiveGateway::new(vec![
            (200, r#"{"dualSidePosition":false}"#),
            (
                200,
                r#"{"orderId":9,"status":"NEW","executedQty":"0","avgPrice":"0"}"#,
            ),
            (0, ""),
        ]);
        let failures = runtime
            .run_live_with(&live_creds(), &gateway, vec![])
            .await
            .unwrap();
        assert!(
            failures.iter().any(|failure| failure.contains("撤单")),
            "撤单未知必须告警：{failures:?}"
        );
        assert_eq!(gateway.sent_count(), 3);
        assert!(
            gateway
                .sent_urls()
                .iter()
                .all(|url| !url.contains("type=LIMIT")),
            "撤单未知时绝不能重发开仓单"
        );
        let conn = db.conn.lock().unwrap();
        let status: String = conn
            .query_row(
                "SELECT status FROM orders WHERE client_order_id='nq-entry-old'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "UNKNOWN", "撤单未知时保持 UNKNOWN 等人工确认");
        let events: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM engine_events WHERE message LIKE '%保持 UNKNOWN%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(events > 0, "撤单未知必须写告警事件");
    }

    #[tokio::test]
    async fn demo_entry_rejects_active_cooldown_and_projected_category_limit() {
        let (db, snapshot) = entry_fixture().await;
        let runtime = test_runtime(&db);
        let now = crate::now();
        db.conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO cooldowns VALUES('BTCUSDT',?1,'loss','symbol')",
                [now + 60],
            )
            .unwrap();
        runtime.persist_snapshot("demo", &snapshot).unwrap();
        let entries = || {
            db.conn
                .lock()
                .unwrap()
                .query_row(
                    "SELECT COUNT(*) FROM orders WHERE purpose='entry'",
                    [],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap()
        };
        assert_eq!(entries(), 0);
        db.conn.lock().unwrap().execute_batch("UPDATE cooldowns SET until_ts=0; UPDATE app_settings SET value=json_set(value,'$.category_exposure_limit_pct',60) WHERE key='risk'; INSERT INTO positions(symbol,side,qty,entry_price,mark_price,mode,status) VALUES('BTCDOMUSDT','short',20,100,100,'demo','open');").unwrap();
        runtime.persist_snapshot("demo", &snapshot).unwrap();
        assert_eq!(entries(), 0);
        db.conn
            .lock()
            .unwrap()
            .execute("UPDATE positions SET symbol='ETHUSDT'", [])
            .unwrap();
        runtime.persist_snapshot("demo", &snapshot).unwrap();
        assert_eq!(entries(), 1);
    }

    struct RejectReviewer;

    impl crate::review::AiReviewer for RejectReviewer {
        fn review(
            &self,
            _proposal: &crate::review::EntryProposal,
        ) -> Result<crate::review::ReviewDecision, String> {
            Ok(crate::review::ReviewDecision {
                action: crate::review::ReviewAction::Reject,
                side: None,
                leverage: None,
                stop_price: None,
                take_profit: None,
                reason: "test reject".to_string(),
                confidence: 0.0,
            })
        }
    }

    #[tokio::test]
    async fn ai_reject_prevents_entry_and_leaves_no_order() {
        let (db, snapshot) = entry_fixture().await;
        db.conn
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS ai_providers(id INTEGER PRIMARY KEY, enabled INTEGER); \
                 INSERT OR REPLACE INTO ai_providers(id,enabled) VALUES(1,1); \
                 INSERT INTO app_settings(key,value,updated_at) VALUES('ai','{\"enabled\":true}',0) \
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at;",
            )
            .unwrap();
        let runtime = test_runtime(&db).with_reviewer(Arc::new(RejectReviewer));
        runtime.persist_snapshot("demo", &snapshot).unwrap();
        let orders: i64 = db
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM orders WHERE purpose='entry'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(orders, 0, "AI 拒绝后不能产生订单");
        let (score, features): (f64, String) = db
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT score, features FROM signals WHERE mode='demo' AND decision='rejected' \
                 ORDER BY id DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(score > 0.0, "量化已过门槛时仍要保留真实评分: {score}");
        let value: serde_json::Value = serde_json::from_str(&features).unwrap();
        assert!(
            value["stage"].is_null(),
            "量化已过门槛的行不能标成预评分: {value}"
        );
    }

    #[tokio::test]
    async fn demo_losing_close_starts_cooldown() {
        let (db, mut snapshot) = entry_fixture().await;
        let runtime = test_runtime(&db);
        runtime.persist_snapshot("demo", &snapshot).unwrap();
        snapshot.markets[0].mark_price = 95.0;
        runtime.persist_snapshot("demo", &snapshot).unwrap();
        let expiry: i64 = db
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT until_ts FROM cooldowns WHERE symbol='BTCUSDT'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(expiry > crate::now());
    }

    fn test_runtime_with_events(db: &Db) -> EngineRuntime {
        let lock = File::open(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap();
        EngineRuntime::new(
            db.clone(),
            lock,
            b"MDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDA=".to_vec(),
            crate::events::EventBus::new(Some(db.clone())),
        )
        .with_reviewer(Arc::new(TestApproveReviewer))
    }

    /// 一笔已开仓的 demo 夹具：由标记价决定触发止损（亏损）还是止盈（盈利）。
    /// 策略关掉部分止盈并把 ROI 门槛抬高，确保一轮里是一次整仓平仓。
    async fn cooldown_position_fixture(mark: f64) -> (Db, crate::providers::TradingSnapshot) {
        use crate::providers::{DemoProvider, SnapshotProvider};
        let db = test_db();
        db.conn
            .lock()
            .unwrap()
            .execute_batch(
r#"ALTER TABLE positions ADD COLUMN ai_confidence REAL NOT NULL DEFAULT 0;
                ALTER TABLE positions ADD COLUMN ai_reason TEXT NOT NULL DEFAULT '';
                UPDATE app_settings SET value='{"enabled":true,"mode":"demo"}' WHERE key='engine';
                INSERT INTO app_settings VALUES('risk','{"cooldown_minutes":7,"symbol_cooldown_minutes":5}',0);
                INSERT INTO app_settings VALUES('strategy','{"partial_tp_at_r":0,"entry_threshold":2.0,"roi_table":[[0,1000]]}',0);
                INSERT INTO positions(symbol,side,qty,entry_price,mark_price,sl_price,tp_price,peak_price,initial_sl_price,signal_score,opened_at,status,mode)
                VALUES('BTCUSDT','long',0.5,100,100,95,105,100,95,0,1,'open','demo');"#,
            )
            .unwrap();
        let snapshot = DemoProvider::with_mark_price(mark).snapshot().await.unwrap();
        (db, snapshot)
    }

    #[tokio::test]
    async fn losing_close_writes_global_cooldown_minutes() {
        let (db, snapshot) = cooldown_position_fixture(95.0).await;
        let runtime = test_runtime(&db);
        runtime.persist_snapshot("demo", &snapshot).unwrap();
        let now = crate::now();
        let conn = db.conn.lock().unwrap();
        let (until, kind): (i64, String) = conn
            .query_row(
                "SELECT until_ts, kind FROM cooldowns WHERE symbol='__global__'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(kind, "global");
        let remain = until - now;
        assert!(
            (7 * 60 - 10..=7 * 60).contains(&remain),
            "全局冷却应等于 risk.cooldown_minutes=7，实际剩余 {remain}s"
        );
        let (symbol_until, symbol_kind): (i64, String) = conn
            .query_row(
                "SELECT until_ts, kind FROM cooldowns WHERE symbol='BTCUSDT'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(symbol_kind, "symbol");
        assert!(symbol_until > now, "symbol 冷却不能被全局冷却取代");
    }

    #[tokio::test]
    async fn profitable_close_does_not_write_global_cooldown_minutes() {
        let (db, snapshot) = cooldown_position_fixture(110.0).await;
        let runtime = test_runtime(&db);
        runtime.persist_snapshot("demo", &snapshot).unwrap();
        let conn = db.conn.lock().unwrap();
        let (status, reason): (String, String) = conn
            .query_row(
                "SELECT status, exit_reason FROM positions WHERE symbol='BTCUSDT'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "closed");
        assert_eq!(reason, "take_profit", "本用例必须是一笔盈利的整仓平仓");
        let global: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM cooldowns WHERE kind='global'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(global, 0, "盈利平仓不能触发全局冷却");
    }

    #[tokio::test]
    async fn global_cooldown_minutes_blocks_entries_until_it_expires() {
        let (db, snapshot) = entry_fixture().await;
        let runtime = test_runtime(&db);
        db.conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO cooldowns VALUES('__global__',?1,'test','global')",
                [crate::now() + 600],
            )
            .unwrap();
        runtime.persist_snapshot("demo", &snapshot).unwrap();
        assert_eq!(entry_orders(&db), 0, "全局冷却内不能新开仓");
        db.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE cooldowns SET until_ts=?1 WHERE kind='global'",
                [crate::now() - 1],
            )
            .unwrap();
        runtime.persist_snapshot("demo", &snapshot).unwrap();
        assert_eq!(entry_orders(&db), 1, "全局冷却到期后必须恢复开仓");
    }

    #[tokio::test]
    async fn global_cooldown_minutes_keeps_symbol_cooldown_independent() {
        let (db, snapshot) = cooldown_position_fixture(95.0).await;
        let runtime = test_runtime(&db);
        runtime.persist_snapshot("demo", &snapshot).unwrap();
        {
            let conn = db.conn.lock().unwrap();
            conn.execute(
                "UPDATE cooldowns SET until_ts=0 WHERE kind='global'",
                [],
            )
            .unwrap();
        }
        // 下一轮会清掉过期的全局冷却；symbol 冷却仍在有效期内，不能被一起删掉。
        runtime.persist_snapshot("demo", &snapshot).unwrap();
        let conn = db.conn.lock().unwrap();
        let global: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM cooldowns WHERE kind='global'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(global, 0, "过期的全局冷却应被清理");
        let symbol: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM cooldowns WHERE symbol='BTCUSDT' AND kind='symbol' \
                 AND until_ts>?1",
                [crate::now()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(symbol, 1, "symbol 冷却必须独立于全局冷却");
    }

    async fn halt_flatten_fixture(close_on_halt: bool) -> (Db, crate::providers::TradingSnapshot) {
        let (db, snapshot) = cooldown_position_fixture(100.0).await;
        {
            let conn = db.conn.lock().unwrap();
            conn.execute(
                "UPDATE app_settings SET value=?1 WHERE key='engine'",
                [format!(
                    r#"{{"enabled":true,"mode":"demo","close_on_halt":{close_on_halt}}}"#
                )],
            )
            .unwrap();
            // 回撤 66% 超过 50% 上限，稳定触发风控熔断。
            upsert_state(
                &conn,
                "equity_peak",
                &(snapshot.account.equity * 3.0).to_string(),
            )
            .unwrap();
        }
        (db, snapshot)
    }

    #[tokio::test]
    async fn close_on_halt_flattens_positions_on_a_risk_halt() {
        let (db, snapshot) = halt_flatten_fixture(true).await;
        let runtime = test_runtime_with_events(&db);
        runtime.persist_snapshot("demo", &snapshot).unwrap();
        let conn = db.conn.lock().unwrap();
        let (status, reason): (String, String) = conn
            .query_row(
                "SELECT status, exit_reason FROM positions WHERE symbol='BTCUSDT'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "closed", "close_on_halt=true 必须平掉仓位");
        assert_eq!(reason, "halt_flatten");
        let purpose: String = conn
            .query_row(
                "SELECT purpose FROM orders WHERE mode='demo' AND reduce_only=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(purpose, "halt_flatten");
        let events: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM engine_events WHERE message LIKE '%close_on_halt%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(events > 0, "熔断平仓必须写事件");
    }

    #[tokio::test]
    async fn close_on_halt_false_leaves_positions_alone() {
        let (db, snapshot) = halt_flatten_fixture(false).await;
        let runtime = test_runtime(&db);
        runtime.persist_snapshot("demo", &snapshot).unwrap();
        let conn = db.conn.lock().unwrap();
        let status: String = conn
            .query_row(
                "SELECT status FROM positions WHERE symbol='BTCUSDT'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "open", "close_on_halt 默认 false 时不能动仓位");
        let closes: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM orders WHERE purpose='halt_flatten'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(closes, 0);
    }

    #[tokio::test]
    async fn close_on_halt_ignores_a_manual_halt() {
        let (db, snapshot) = halt_flatten_fixture(true).await;
        {
            let conn = db.conn.lock().unwrap();
            upsert_state(&conn, "equity_peak", "0").unwrap();
            upsert_state(&conn, "halted", "手动暂停").unwrap();
        }
        let runtime = test_runtime(&db);
        runtime.persist_snapshot("demo", &snapshot).unwrap();
        let conn = db.conn.lock().unwrap();
        let status: String = conn
            .query_row(
                "SELECT status FROM positions WHERE symbol='BTCUSDT'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "open", "手动熔断不能触发 close_on_halt");
        let raw: String = conn
            .query_row(
                "SELECT value FROM engine_state WHERE key='cycle_preview.demo'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let preview: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(preview["halt_flatten"], false);
        assert!(
            preview["skipped"].as_str().unwrap().contains("熔断"),
            "手动熔断仍要拦扫描，只是不平仓"
        );
    }

    #[tokio::test]
    async fn close_on_halt_dedupes_repeated_halts() {
        let (db, snapshot) = halt_flatten_fixture(true).await;
        let runtime = test_runtime(&db);
        runtime.persist_snapshot("demo", &snapshot).unwrap();
        db.conn
            .lock()
            .unwrap()
            .execute_batch(
                "INSERT INTO positions(symbol,side,qty,entry_price,mark_price,sl_price,tp_price,\
                 peak_price,initial_sl_price,signal_score,opened_at,status,mode) \
                 VALUES('BTCUSDT','long',0.5,100,100,95,105,100,95,0,1,'open','demo');",
            )
            .unwrap();
        runtime.persist_snapshot("demo", &snapshot).unwrap();
        let conn = db.conn.lock().unwrap();
        let closes: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM orders WHERE purpose='halt_flatten'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(closes, 1, "同一段熔断期间只允许全平一次");
        let open: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM positions WHERE mode='demo' AND status='open'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(open, 1, "去重后新仓不应被再次全平");
    }

    fn entry_orders(db: &Db) -> i64 {
        db.conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM orders WHERE mode='demo' AND purpose='entry'",
                [],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn set_engine_settings(db: &Db, value: &str) {
        db.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE app_settings SET value=?1 WHERE key='engine'",
                [value],
            )
            .unwrap();
    }

    async fn entry_fixture() -> (Db, crate::providers::TradingSnapshot) {
        use crate::providers::{DemoProvider, SnapshotProvider};
        let db = test_db();
        db.conn.lock().unwrap().execute_batch(
            r#"ALTER TABLE positions ADD COLUMN ai_confidence REAL NOT NULL DEFAULT 0;
            ALTER TABLE positions ADD COLUMN ai_reason TEXT NOT NULL DEFAULT '';
            ALTER TABLE positions ADD COLUMN entry_slippage_bps REAL NOT NULL DEFAULT 0;
            UPDATE app_settings SET value='{"enabled":true,"mode":"demo"}' WHERE key='engine';
            UPDATE engine_state SET value='true' WHERE key='enabled';
            INSERT INTO app_settings VALUES('risk','{"min_leverage":10,"max_leverage":20,"margin_pct":10,"rsi_long_max":100.1}',0);
            CREATE TABLE trading_symbols(symbol TEXT PRIMARY KEY,enabled INTEGER,max_leverage INTEGER,tick_sz TEXT,min_sz TEXT,min_notional REAL);
            INSERT INTO trading_symbols VALUES('BTCUSDT',1,5,'0.01','0.001',5);
            CREATE TABLE IF NOT EXISTS cooldowns(symbol TEXT PRIMARY KEY,until_ts INTEGER,reason TEXT,kind TEXT);
            INSERT INTO mantras VALUES(1,'回踩入场','当价格回踩 EMA21 且量比大于 0.8 时开多','[]','entry','manual',0.8,0.9,0,0,0,1,1,1,0,'{}',100,100,0);
            INSERT INTO experiences VALUES(1,'BTCUSDT','["sym:BTCUSDT","dir:long"]','{}','突破后回踩再进','win',0,'journal',1,1,0,1,100,100);"#,
        ).unwrap();
        let mut snapshot = DemoProvider::with_mark_price(100.0)
            .snapshot()
            .await
            .unwrap();
        let market = &mut snapshot.markets[0];
        market.funding_rate = -0.001;
        market.open_interest_change_pct = 5.0;
        let len = market.candles.len();
        for (i, candle) in market.candles.iter_mut().enumerate() {
            let price = 100.0 - (len - i - 1) as f64 * 0.15;
            candle.open = price;
            candle.close = price;
            candle.high = price + 0.05;
            candle.low = price - 0.05;
        }
        (db, snapshot)
    }

    #[tokio::test]
    async fn demo_cycle_closes_stop_loss_once_and_reports_the_fill() {
        use crate::providers::{DemoProvider, SnapshotProvider};
        let db = test_db();
        {
            let conn = db.conn.lock().unwrap();
            conn.execute_batch(
                "ALTER TABLE positions ADD COLUMN ai_confidence REAL NOT NULL DEFAULT 0; \
                 ALTER TABLE positions ADD COLUMN ai_reason TEXT NOT NULL DEFAULT '';",
            )
            .unwrap();
            conn.execute(
                "UPDATE app_settings SET value=?1 WHERE key='engine'",
                [r#"{"enabled":true,"mode":"demo"}"#],
            )
            .unwrap();
            conn.execute_batch(
                "INSERT INTO positions(symbol,side,qty,entry_price,mark_price,sl_price,tp_price,\
                 peak_price,initial_sl_price,signal_score,opened_at,status,mode) \
                 VALUES('BTCUSDT','long',0.5,51000,51000,50000,60000,51000,50000,0.0,1,'open','demo');",
            )
            .unwrap();
        }
        let runtime = test_runtime(&db);
        // 标记价跌破硬止损，管仓必须当轮平掉，不能等下一轮。
        let snapshot = DemoProvider::with_mark_price(50_000.0)
            .snapshot()
            .await
            .unwrap();
        runtime.persist_snapshot("demo", &snapshot).unwrap();
        {
            let conn = db.conn.lock().unwrap();
            let (status, exit_reason, qty): (String, String, f64) = conn
                .query_row(
                    "SELECT status, exit_reason, qty FROM positions WHERE symbol='BTCUSDT'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap();
            assert_eq!(status, "closed");
            assert_eq!(exit_reason, "stop_loss");
            assert_eq!(qty, 0.0);
            let (purpose, reduce_only, price): (String, i64, f64) = conn
                .query_row(
                    "SELECT purpose, reduce_only, price FROM orders WHERE mode='demo'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap();
            assert_eq!(purpose, "stop_loss");
            assert_eq!(reduce_only, 1);
            assert_eq!(price, 50_000.0);
        }
        let raw: String = db
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT value FROM engine_state WHERE key='cycle_preview.demo'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let preview: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            preview["executed"][0]["reason"], "stop_loss",
            "预览里必须能看出当轮真的成交了"
        );
        // 同一份快照再跑一轮：仓位已平，不能重复下单。
        runtime.persist_snapshot("demo", &snapshot).unwrap();
        let orders: i64 = db
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM orders WHERE mode='demo'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(orders, 1, "已平的仓不能再产生第二笔平仓单");
    }

    #[tokio::test]
    async fn demo_cycle_skips_stale_quotes_and_symbols_with_unmatched_orders() {
        use crate::providers::{DemoProvider, SnapshotProvider};
        let db = test_db();
        {
            let conn = db.conn.lock().unwrap();
            conn.execute(
                "UPDATE app_settings SET value=?1 WHERE key='engine'",
                [r#"{"enabled":true,"mode":"demo"}"#],
            )
            .unwrap();
            conn.execute_batch(
                "INSERT INTO positions(symbol,side,qty,entry_price,mark_price,sl_price,tp_price,\
                 peak_price,initial_sl_price,signal_score,opened_at,status,mode) \
                 VALUES('BTCUSDT','long',0.5,51000,51000,50000,60000,51000,50000,0.0,1,'open','demo');",
            )
            .unwrap();
        }
        let runtime = test_runtime(&db);
        let mut snapshot = DemoProvider::with_mark_price(50_000.0)
            .snapshot()
            .await
            .unwrap();
        // 停更行情上的标记价不是当前价，用它触发止损等于赌运气。
        for candle in &mut snapshot.markets[0].candles {
            candle.close_time -= 48 * 60 * 60 * 1_000;
        }
        runtime.persist_snapshot("demo", &snapshot).unwrap();
        let status: String = db
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT status FROM positions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(status, "open", "停更行情上不能撮合");

        let mut fresh = DemoProvider::with_mark_price(50_000.0)
            .snapshot()
            .await
            .unwrap();
        fresh.markets[0].candles = snapshot.markets[0]
            .candles
            .iter()
            .cloned()
            .map(|mut candle| {
                candle.close_time += 48 * 60 * 60 * 1_000;
                candle
            })
            .collect();
        db.conn
            .lock()
            .unwrap()
            .execute_batch(
                "INSERT INTO orders(symbol,client_order_id,exchange_order_id,mode,status) \
                 VALUES('BTCUSDT','entry-1','','demo','pending');",
            )
            .unwrap();
        runtime.persist_snapshot("demo", &fresh).unwrap();
        let conn = db.conn.lock().unwrap();
        let status: String = conn
            .query_row("SELECT status FROM positions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(status, "open", "订单状态对不上时不能继续对该标的发单");
        let closings: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM orders WHERE purpose='stop_loss'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(closings, 0);
    }

    struct FakeLiveGateway {
        replies: std::sync::Mutex<Vec<crate::live::GatewayReply>>,
        sent: std::sync::Mutex<Vec<String>>,
    }

    impl FakeLiveGateway {
        /// `(0, "")` 表示传输失败，其余按状态码+正文回包。
        fn new(replies: Vec<(u16, &str)>) -> Self {
            Self {
                replies: std::sync::Mutex::new(
                    replies
                        .into_iter()
                        .map(|(status, body)| crate::live::GatewayReply {
                            status,
                            body: body.to_string(),
                            transport_failed: status == 0,
                        })
                        .collect(),
                ),
                sent: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn sent_count(&self) -> usize {
            self.sent.lock().unwrap().len()
        }

        fn sent_urls(&self) -> Vec<String> {
            self.sent.lock().unwrap().clone()
        }
    }

    impl crate::live::Gateway for FakeLiveGateway {
        fn send<'a>(
            &'a self,
            request: &'a crate::submit::PreparedRequest,
        ) -> crate::live::GatewayFuture<'a> {
            self.sent.lock().unwrap().push(request.url.clone());
            let mut replies = self.replies.lock().unwrap();
            let reply = if replies.is_empty() {
                crate::live::GatewayReply {
                    status: 0,
                    body: String::new(),
                    transport_failed: true,
                }
            } else {
                replies.remove(0)
            };
            Box::pin(async move { reply })
        }
    }

    fn live_creds() -> LiveCreds {
        LiveCreds {
            base: "https://example.invalid".into(),
            api_key: "key".into(),
            secret: "secret".into(),
            recv_window_ms: 5_000,
        }
    }

    fn seed_live_position(db: &Db) {
        let conn = db.conn.lock().unwrap();
        // test_db 的持仓夹具早于 trades 生产 schema 字段，补齐 record_trade 读取的列。
        conn.execute_batch(
            "ALTER TABLE positions ADD COLUMN ai_confidence REAL NOT NULL DEFAULT 0; \
             ALTER TABLE positions ADD COLUMN ai_reason TEXT NOT NULL DEFAULT '';",
        )
        .unwrap();
        conn.execute(
            "UPDATE app_settings SET value=?1 WHERE key='engine'",
            [r#"{"enabled":true,"mode":"live"}"#],
        )
        .unwrap();
        conn.execute("UPDATE engine_state SET value='live' WHERE key='mode'", [])
            .unwrap();
        conn.execute_batch(
            "INSERT INTO positions(symbol,side,qty,entry_price,mark_price,sl_price,tp_price,\
             peak_price,initial_sl_price,signal_score,opened_at,status,mode) \
             VALUES('BTCUSDT','long',0.5,51000,49000,49000,60000,51000,49000,0.0,1,'open','live');",
        )
        .unwrap();
    }

    async fn live_snapshot() -> crate::providers::TradingSnapshot {
        use crate::providers::{DemoProvider, PositionSnapshot, SnapshotProvider};
        let mut snapshot = DemoProvider::with_mark_price(49_000.0)
            .snapshot()
            .await
            .unwrap();
        // 交易所侧确实有这个仓：否则会被判成 venue_sync 直接关掉。
        snapshot.positions.push(PositionSnapshot {
            symbol: "BTCUSDT".into(),
            side: "long".into(),
            qty: 0.5,
            entry_price: 51_000.0,
        });
        snapshot
    }

    #[tokio::test]
    async fn hedge_opposite_side_closes_the_missing_local_side() {
        use crate::providers::{DemoProvider, PositionSnapshot, SnapshotProvider};
        let db = test_db();
        seed_live_position(&db);
        let mut snapshot = DemoProvider::with_mark_price(49_000.0)
            .snapshot()
            .await
            .unwrap();
        snapshot.positions.clear();
        snapshot.positions.push(PositionSnapshot {
            symbol: "BTCUSDT".into(),
            side: "short".into(),
            qty: 0.5,
            entry_price: 49_000.0,
        });
        let conn = db.conn.lock().unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        let closed = EngineRuntime::close_missing_positions(&tx, "live", &snapshot).unwrap();
        assert_eq!(closed.len(), 1, "交易所有空头不代表本地的多头还在");
        assert_eq!(closed[0]["side"], "long");
        let status: String = tx
            .query_row(
                "SELECT status FROM positions WHERE mode='live'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "closed");
        tx.rollback().unwrap();
    }

    #[tokio::test]
    async fn a_position_missing_at_the_venue_is_closed_locally_with_a_trade() {
        use crate::providers::{DemoProvider, SnapshotProvider};
        let db = test_db();
        seed_live_position(&db);
        let runtime = test_runtime(&db);
        // 保护单在交易所被触发：快照里已经没有这个仓位。
        let snapshot = DemoProvider::with_mark_price(49_000.0)
            .snapshot()
            .await
            .unwrap();
        let plans = runtime.persist_snapshot("live", &snapshot).unwrap();
        assert!(plans.is_empty(), "交易所已无此仓，不能再发平仓单");
        let conn = db.conn.lock().unwrap();
        let (status, exit_reason, exit_price): (String, String, f64) = conn
            .query_row(
                "SELECT status, exit_reason, exit_price FROM positions WHERE mode='live'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(status, "closed");
        assert_eq!(exit_reason, "venue_sync");
        assert_eq!(exit_price, 49_000.0, "拿不到成交价时按标记价结算");
        let order_rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM orders", [], |row| row.get(0))
            .unwrap();
        assert_eq!(order_rows, 0, "交易所触发的平仓不该伪造委托");
        drop(conn);
        let raw: String = db
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT value FROM engine_state WHERE key='cycle_preview.live'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let preview: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(preview["venue_closed"][0]["reason"], "venue_sync");
    }

    #[tokio::test]
    async fn a_live_stop_is_submitted_settled_and_not_replanned() {
        let db = test_db();
        seed_live_position(&db);
        let runtime = test_runtime(&db);
        let snapshot = live_snapshot().await;
        let plans = runtime.persist_snapshot("live", &snapshot).unwrap();
        assert_eq!(plans.len(), 1, "跌破硬止损必须产生平仓计划");
        assert_eq!(plans[0].side, "SELL");
        assert_eq!(plans[0].purpose, "stop_loss");

        let gateway = FakeLiveGateway::new(vec![
            (200, r#"{"dualSidePosition":false}"#),
            (
                200,
                r#"{"orderId":77,"status":"FILLED","executedQty":"0.5","avgPrice":"49000"}"#,
            ),
        ]);
        let failures = runtime
            .run_live_with(&live_creds(), &gateway, plans)
            .await
            .unwrap();
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(gateway.sent_count(), 2);
        assert!(gateway.sent_urls()[1].contains("reduceOnly=true"));
        assert!(!gateway.sent_urls()[1].contains("positionSide="));
        {
            let conn = db.conn.lock().unwrap();
            let (status, qty): (String, f64) = conn
                .query_row(
                    "SELECT status, qty FROM positions WHERE mode='live'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(status, "closed");
            assert_eq!(qty, 0.0);
            let (orders, order_status, exchange_id): (i64, String, String) = conn
                .query_row(
                    "SELECT COUNT(*), MAX(status), MAX(exchange_order_id) FROM orders WHERE mode='live'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap();
            assert_eq!(orders, 1, "预留行要被推进，不能插出第二条");
            assert_eq!(order_status, "FILLED");
            assert_eq!(exchange_id, "77");
        }
        // 仓位已平：同一份快照不会再生成计划，重复周期不会重复下单。
        let replanned = runtime.persist_snapshot("live", &snapshot).unwrap();
        assert!(replanned.is_empty(), "已平的仓不能再出平仓计划");
    }

    #[tokio::test]
    async fn an_unknown_live_fill_is_confirmed_by_query_instead_of_resent() {
        let db = test_db();
        seed_live_position(&db);
        let runtime = test_runtime(&db);
        let plans = runtime
            .persist_snapshot("live", &live_snapshot().await)
            .unwrap();
        let gateway = FakeLiveGateway::new(vec![
            (200, r#"{"dualSidePosition":false}"#),
            (0, ""),
            (
                200,
                r#"{"orderId":88,"status":"FILLED","executedQty":"0.5","avgPrice":"48990"}"#,
            ),
        ]);
        let failures = runtime
            .run_live_with(&live_creds(), &gateway, plans)
            .await
            .unwrap();
        assert!(failures.is_empty(), "{failures:?}");
        let urls = gateway.sent_urls();
        assert_eq!(urls.len(), 3, "写请求只发一次，之后是查询");
        assert!(urls[0].contains("/fapi/v1/positionSide/dual"));
        assert!(urls[1].contains("/fapi/v1/order?"));
        assert!(urls[2].contains("origClientOrderId="));
        let (status, exchange_id): (String, String) = db
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT status, exchange_order_id FROM orders WHERE mode='live'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "FILLED");
        assert_eq!(exchange_id, "88");
    }

    #[tokio::test]
    async fn a_rejected_live_order_keeps_the_position_and_reports_the_code() {
        let db = test_db();
        seed_live_position(&db);
        let runtime = test_runtime(&db);
        let plans = runtime
            .persist_snapshot("live", &live_snapshot().await)
            .unwrap();
        let gateway = FakeLiveGateway::new(vec![
            (200, r#"{"dualSidePosition":false}"#),
            (400, r#"{"code":-2019,"msg":"Margin is insufficient."}"#),
        ]);
        let failures = runtime
            .run_live_with(&live_creds(), &gateway, plans)
            .await
            .unwrap();
        assert!(
            failures.iter().any(|failure| failure.contains("-2019")),
            "拒单原因必须上报：{failures:?}"
        );
        let conn = db.conn.lock().unwrap();
        let status: String = conn
            .query_row(
                "SELECT status FROM positions WHERE mode='live'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "open", "被拒的平仓单不能改本地仓位");
        let order_status: String = conn
            .query_row("SELECT status FROM orders WHERE mode='live'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(order_status, "REJECTED");
    }

    #[tokio::test]
    async fn protection_failure_is_reported_but_the_filled_leg_is_recorded() {
        let db = test_db();
        {
            let conn = db.conn.lock().unwrap();
            conn.execute(
                "UPDATE app_settings SET value=?1 WHERE key='engine'",
                [r#"{"enabled":true,"mode":"live"}"#],
            )
            .unwrap();
            conn.execute_batch(
                "INSERT INTO positions(symbol,side,qty,entry_price,mark_price,sl_price,tp_price,\
                 peak_price,initial_sl_price,signal_score,opened_at,status,mode) \
                 VALUES('BTCUSDT','long',0.5,51000,50500,49000,60000,51000,49000,0.0,1,'open','live'); \
                 INSERT INTO orders(ts,symbol,side,order_type,qty,price,status,purpose,client_order_id,exchange_order_id,reduce_only,mode) \
                 VALUES(1,'BTCUSDT','BUY','MARKET',0.5,51000,'FILLED','entry','nq-entry','',0,'live');",
            )
            .unwrap();
        }
        let runtime = test_runtime(&db);
        let gateway = FakeLiveGateway::new(vec![
            (200, r#"{"dualSidePosition":false}"#),
            (200, r#"{"orderId":101,"status":"NEW"}"#),
            (400, r#"{"code":-2019,"msg":"Margin is insufficient."}"#),
        ]);
        let failures = runtime
            .run_live_with(&live_creds(), &gateway, Vec::new())
            .await
            .unwrap();
        assert!(
            failures.iter().any(|failure| failure.contains("保护单")),
            "止盈挂不上必须让调用方看到：{failures:?}"
        );
        let conn = db.conn.lock().unwrap();
        let (stop, take): (i64, i64) = conn
            .query_row(
                "SELECT SUM(purpose='stop_loss'), SUM(purpose='take_profit') FROM orders WHERE mode='live'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(stop, 1, "挂上的那条腿要留痕");
        assert_eq!(take, 1, "失败的腿要留痕");
        let take_status: String = conn
            .query_row(
                "SELECT status FROM orders WHERE purpose='take_profit'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(take_status, "REJECTED", "失败的腿不能记成已挂");
    }

    #[tokio::test]
    async fn an_unknown_close_is_reconciled_by_the_next_cycle() {
        let db = test_db();
        seed_live_position(&db);
        db.conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO orders(ts,symbol,side,order_type,qty,price,status,purpose,\
                 client_order_id,exchange_order_id,reduce_only,mode) \
                 VALUES(1,'BTCUSDT','SELL','MARKET',0.5,0,'UNKNOWN','stop_loss','nq-prev','',1,'live')",
                [],
            )
            .unwrap();
        let runtime = test_runtime(&db);
        let gateway = FakeLiveGateway::new(vec![
            (200, r#"{"dualSidePosition":false}"#),
            (
                200,
                r#"{"orderId":55,"status":"FILLED","executedQty":"0.5","avgPrice":"48800"}"#,
            ),
        ]);
        let failures = runtime
            .run_live_with(&live_creds(), &gateway, Vec::new())
            .await
            .unwrap();
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(gateway.sent_count(), 2, "先查模式，再查未知单，不重发");
        let conn = db.conn.lock().unwrap();
        let (status, exit_price): (String, f64) = conn
            .query_row(
                "SELECT status, exit_price FROM positions WHERE mode='live'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "closed");
        assert_eq!(exit_price, 48_800.0, "按交易所成交价结算");
        let (orders, order_status): (i64, String) = conn
            .query_row(
                "SELECT COUNT(*), MAX(status) FROM orders WHERE mode='live'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(orders, 1, "对账只推进原订单");
        assert_eq!(order_status, "FILLED");
    }

    #[tokio::test]
    async fn reconciling_a_close_for_an_already_closed_position_only_updates_the_order() {
        let db = test_db();
        {
            let conn = db.conn.lock().unwrap();
            conn.execute_batch(
                "INSERT INTO positions(symbol,side,qty,entry_price,mark_price,sl_price,tp_price,\
                 peak_price,initial_sl_price,signal_score,opened_at,closed_at,exit_price,exit_reason,\
                 realized_pnl,fees,status,mode) \
                 VALUES('BTCUSDT','long',0,51000,49000,49000,60000,51000,49000,0.0,1,2,49000,'venue_sync',\
                 -100.0,0.5,'closed','live'); \
                 INSERT INTO orders(ts,symbol,side,order_type,qty,price,status,purpose,client_order_id,\
                 exchange_order_id,reduce_only,mode) \
                 VALUES(1,'BTCUSDT','SELL','MARKET',0.5,0,'UNKNOWN','stop_loss','nq-prev','',1,'live');",
            )
            .unwrap();
        }
        let runtime = test_runtime(&db);
        let gateway = FakeLiveGateway::new(vec![
            (200, r#"{"dualSidePosition":false}"#),
            (
                200,
                r#"{"orderId":66,"status":"FILLED","executedQty":"0.5","avgPrice":"49000"}"#,
            ),
        ]);
        let failures = runtime
            .run_live_with(&live_creds(), &gateway, Vec::new())
            .await
            .unwrap();
        assert!(failures.is_empty(), "{failures:?}");
        let conn = db.conn.lock().unwrap();
        let (status, realized, fees): (String, f64, f64) = conn
            .query_row(
                "SELECT status, realized_pnl, fees FROM positions WHERE mode='live'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(status, "closed");
        assert_eq!(realized, -100.0, "已结算过的仓位不能再记一次盈亏");
        assert_eq!(fees, 0.5);
        let order_status: String = conn
            .query_row("SELECT status FROM orders WHERE mode='live'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(order_status, "FILLED");
    }

    #[tokio::test]
    async fn an_inflight_close_blocks_a_second_plan_for_the_same_symbol() {
        let db = test_db();
        seed_live_position(&db);
        db.conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO orders(ts,symbol,side,order_type,qty,price,status,purpose,\
                 client_order_id,exchange_order_id,reduce_only,mode) \
                 VALUES(1,'BTCUSDT','SELL','MARKET',0.5,0,'UNKNOWN','stop_loss','nq-old','',1,'live')",
                [],
            )
            .unwrap();
        let runtime = test_runtime(&db);
        let plans = runtime
            .persist_snapshot("live", &live_snapshot().await)
            .unwrap();
        assert!(
            plans.is_empty(),
            "上一轮的平仓单还没结论，这轮再发就是重复平仓"
        );
    }

    #[tokio::test]
    async fn an_unknown_protection_leg_is_recorded_and_never_rearmed() {
        let db = test_db();
        {
            let conn = db.conn.lock().unwrap();
            conn.execute_batch(
                "INSERT INTO positions(symbol,side,qty,entry_price,mark_price,sl_price,tp_price,\
                 peak_price,initial_sl_price,signal_score,opened_at,status,mode) \
                 VALUES('BTCUSDT','long',0.5,51000,50500,49000,60000,51000,49000,0.0,1,'open','live'); \
                 INSERT INTO orders(ts,symbol,side,order_type,qty,price,status,purpose,client_order_id,exchange_order_id,reduce_only,mode) \
                 VALUES(1,'BTCUSDT','BUY','MARKET',0.5,51000,'FILLED','entry','nq-entry','',0,'live');",
            )
            .unwrap();
        }
        let runtime = test_runtime(&db);
        let gateway = FakeLiveGateway::new(vec![
            (200, r#"{"dualSidePosition":false}"#),
            (0, ""),
            (0, ""),
        ]);
        let failures = runtime
            .run_live_with(&live_creds(), &gateway, Vec::new())
            .await
            .unwrap();
        assert!(
            failures.iter().any(|failure| failure.contains("未知")),
            "{failures:?}"
        );
        let unknown: i64 = db
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM orders WHERE mode='live' AND status='UNKNOWN' \
                 AND purpose IN ('stop_loss','take_profit')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(unknown, 2, "两条腿都要留下未知痕迹");

        let retry = FakeLiveGateway::new(vec![(200, r#"{"dualSidePosition":false}"#)]);
        let failures = runtime
            .run_live_with(&live_creds(), &retry, Vec::new())
            .await
            .unwrap();
        assert_eq!(retry.sent_count(), 1, "只查模式，未知保护腿不能重挂");
        assert!(retry.sent_urls()[0].contains("/fapi/v1/positionSide/dual"));
        assert!(failures.is_empty(), "{failures:?}");
    }

    #[tokio::test]
    async fn an_unreadable_position_mode_sends_no_orders() {
        let db = test_db();
        seed_live_position(&db);
        let runtime = test_runtime(&db);
        let plans = runtime
            .persist_snapshot("live", &live_snapshot().await)
            .unwrap();
        assert!(!plans.is_empty(), "没有计划就测不到停单");
        let gateway = FakeLiveGateway::new(vec![(500, r#"{"code":-1000}"#)]);
        let error = runtime
            .run_live_with(&live_creds(), &gateway, plans)
            .await
            .unwrap_err();
        assert!(error.contains("持仓模式"), "{error}");
        assert_eq!(gateway.sent_count(), 1);
        let orders: i64 = db
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM orders", [], |row| row.get(0))
            .unwrap();
        assert_eq!(orders, 0, "模式不明不能发单");
    }

    #[tokio::test]
    async fn hedge_mode_protects_each_side_of_the_same_symbol() {
        let db = test_db();
        db.conn
            .lock()
            .unwrap()
            .execute_batch(
                "INSERT INTO positions(symbol,side,qty,entry_price,mark_price,sl_price,tp_price,\
                 peak_price,initial_sl_price,signal_score,opened_at,status,mode) VALUES \
                 ('BTCUSDT','long',0.5,51000,50500,49000,60000,51000,49000,0.0,1,'open','live'), \
                 ('BTCUSDT','short',0.4,50000,50500,52000,47000,50000,52000,0.0,1,'open','live'); \
                 INSERT INTO orders(ts,symbol,side,order_type,qty,price,status,purpose,client_order_id,exchange_order_id,reduce_only,mode) VALUES \
                 (1,'BTCUSDT','BUY','MARKET',0.5,51000,'FILLED','entry','nq-long','',0,'live'), \
                 (1,'BTCUSDT','SELL','MARKET',0.4,50000,'FILLED','entry','nq-short','',0,'live');",
            )
            .unwrap();
        let runtime = test_runtime(&db);
        let gateway = FakeLiveGateway::new(vec![
            (200, r#"{"dualSidePosition":true}"#),
            (200, r#"{"algoId":1}"#),
            (200, r#"{"algoId":2}"#),
            (200, r#"{"algoId":3}"#),
            (200, r#"{"algoId":4}"#),
        ]);
        let failures = runtime
            .run_live_with(&live_creds(), &gateway, Vec::new())
            .await
            .unwrap();
        assert!(failures.is_empty(), "{failures:?}");
        let urls = gateway.sent_urls();
        assert_eq!(urls.len(), 5, "一次模式查询加四条保护腿");
        let long_legs = urls
            .iter()
            .filter(|url| url.contains("positionSide=LONG"))
            .count();
        let short_legs = urls
            .iter()
            .filter(|url| url.contains("positionSide=SHORT"))
            .count();
        assert_eq!((long_legs, short_legs), (2, 2));
        assert!(urls.iter().all(|url| !url.contains("reduceOnly=")));
        let recorded: i64 = db
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM orders WHERE purpose IN ('stop_loss','take_profit')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(recorded, 4);

        let again = FakeLiveGateway::new(vec![(200, r#"{"dualSidePosition":true}"#)]);
        let failures = runtime
            .run_live_with(&live_creds(), &again, Vec::new())
            .await
            .unwrap();
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(again.sent_count(), 1, "两侧都已挂上，不能再补");
    }

    #[test]
    fn reads_engine_interval_with_minimum_floor() {
        let db = test_db();
        let config = RuntimeConfig::from_db(&db);
        assert_eq!(config.interval_sec, MIN_INTERVAL_SEC as u64);
        assert!(!config.enabled);
    }

    #[tokio::test]
    async fn disabled_tick_updates_heartbeat_and_clears_error() {
        let db = test_db();
        let runtime = test_runtime(&db);
        assert!(runtime.tick(&RuntimeConfig::from_db(&db)).await.is_ok());
        let conn = db.conn.lock().unwrap();
        let ts: i64 = conn
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM engine_state WHERE key='last_cycle_ts'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(ts > 0);
        let error: String = conn
            .query_row(
                "SELECT value FROM engine_state WHERE key='last_error'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(error.is_empty());
    }

    #[tokio::test]
    async fn enabled_tick_refuses_to_fake_cycle_and_records_error() {
        let db = test_db();
        {
            let conn = db.conn.lock().unwrap();
            conn.execute(
                "UPDATE app_settings SET value=?1 WHERE key='engine'",
                [r#"{"enabled":true,"mode":"live","cycle_interval_sec":15}"#],
            )
            .unwrap();
            conn.execute_batch("INSERT OR REPLACE INTO engine_state VALUES ('mode','live',0); INSERT INTO engine_state VALUES ('last_cycle_ts','123',0), ('last_cycle_ms','7',0);").unwrap();
        }
        let runtime = test_runtime(&db);
        assert!(runtime.tick(&RuntimeConfig::from_db(&db)).await.is_err());
        let conn = db.conn.lock().unwrap();
        let error: String = conn
            .query_row(
                "SELECT value FROM engine_state WHERE key='last_error'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(error.contains("未配置实盘"), "实际错误: {error}");
        let successful_ts: String = conn
            .query_row(
                "SELECT value FROM engine_state WHERE key='last_cycle_ts'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(successful_ts, "123");
        let previous_ms: String = conn
            .query_row(
                "SELECT value FROM engine_state WHERE key='last_cycle_ms'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(previous_ms, "7", "失败的一轮不能刷新成功周期耗时");
    }

    #[tokio::test]
    async fn snapshot_is_persisted_without_marking_execution_successful() {
        let db = test_db();
        let runtime = test_runtime(&db);
        runtime
            .refresh_snapshot("demo", &crate::providers::DemoProvider::new())
            .await
            .unwrap();
        let conn = db.conn.lock().unwrap();
        let raw: String = conn
            .query_row(
                "SELECT value FROM engine_state WHERE key='snapshot.demo'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let snapshot: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(snapshot["markets"][0]["symbol"], "BTCUSDT");
        assert_eq!(snapshot["account"]["equity"], 10_000.0);
        assert_eq!(snapshot["positions"], serde_json::json!([]));
        let raw_metrics: String = conn
            .query_row(
                "SELECT value FROM engine_state WHERE key='candle_metrics.demo'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let metrics: Value = serde_json::from_str(&raw_metrics).unwrap();
        assert_eq!(metrics["BTCUSDT"]["atr_pct"], 0.0);
        assert_eq!(metrics["BTCUSDT"]["vol_ratio"], 1.0);
        assert!(conn
            .query_row(
                "SELECT value FROM engine_state WHERE key='last_cycle_ts'",
                [],
                |r| r.get::<_, String>(0)
            )
            .is_err());
    }

    #[tokio::test]
    async fn position_differences_are_scoped_to_mode_symbol_and_side() {
        let db = test_db();
        db.conn
            .lock()
            .unwrap()
            .execute_batch(
                "INSERT INTO positions(symbol, side, qty, mode, status) VALUES ('BTCUSDT','long',2,'demo','open'),
                 ('BTCUSDT','short',1,'demo','open'), ('ETHUSDT','long',4,'live','open'),
                 ('SOLUSDT','short',1,'demo','closed');",
            )
            .unwrap();
        let runtime = test_runtime(&db);
        use crate::providers::{DemoProvider, PositionSnapshot, SnapshotProvider};
        let mut snapshot = DemoProvider::new().snapshot().await.unwrap();
        snapshot.positions.push(PositionSnapshot {
            symbol: "BTCUSDT".into(),
            side: "short".into(),
            qty: 1.0,
            entry_price: 100.0,
        });
        runtime.persist_snapshot("demo", &snapshot).unwrap();
        let conn = db.conn.lock().unwrap();
        let raw: String = conn
            .query_row(
                "SELECT value FROM engine_state WHERE key='position_differences.demo'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&raw).unwrap()["differences"],
            serde_json::json!([
                {"symbol":"BTCUSDT","side":"long","local_qty":2.0,"venue_qty":0.0}
            ])
        );
        let rows: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM positions WHERE status='open'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 3);
    }

    #[tokio::test]
    async fn missing_open_order_remains_pending_and_is_reported() {
        use crate::providers::{DemoProvider, SnapshotProvider};
        let db = test_db();
        db.conn.lock().unwrap().execute_batch(
            "INSERT INTO orders(symbol, client_order_id, exchange_order_id, mode, status) VALUES
             ('BTCUSDT','entry-1','','demo','pending'),
             ('BTCUSDT','live-1','','live','NEW'),
             ('BTCUSDT','done-1','15','demo','FILLED');",
        ).unwrap();
        let snapshot = DemoProvider::new().snapshot().await.unwrap();
        test_runtime(&db)
            .persist_snapshot("demo", &snapshot)
            .unwrap();
        let conn = db.conn.lock().unwrap();
        let report: String = conn
            .query_row(
                "SELECT value FROM engine_state WHERE key='order_differences.demo'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&report).unwrap(),
            serde_json::json!({
                "local_only": [{"symbol":"BTCUSDT","client_order_id":"entry-1","exchange_order_id":""}],
                "venue_only": []
            })
        );
        let status: String = conn
            .query_row(
                "SELECT status FROM orders WHERE client_order_id='entry-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "pending");
    }

    #[tokio::test]
    async fn rejected_snapshot_timestamp_rolls_back_snapshot() {
        let db = test_db();
        db.conn
            .lock()
            .unwrap()
            .execute_batch(
                "INSERT INTO positions(symbol, side, qty, mode, status) VALUES ('BTCUSDT','long',-1,'demo','open');
             CREATE TRIGGER reject_timestamp BEFORE INSERT ON engine_state
             WHEN NEW.key = 'snapshot_ts.demo' BEGIN SELECT RAISE(ABORT, 'rejected'); END;",
            )
            .unwrap();
        let runtime = test_runtime(&db);
        assert!(runtime
            .refresh_snapshot("demo", &crate::providers::DemoProvider::new())
            .await
            .is_err());
        let exists: bool = db
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM engine_state WHERE key='snapshot.demo')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!exists);
    }

    #[tokio::test]
    async fn stop_signal_interrupts_the_interval_wait() {
        let db = test_db();
        let (tx, rx) = stop_channel();
        let task = tokio::spawn(test_runtime(&db).run(rx));
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let has_tick = db
                    .conn
                    .lock()
                    .unwrap()
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM engine_state WHERE key='last_cycle_ts')",
                        [],
                        |row| row.get::<_, bool>(0),
                    )
                    .unwrap();
                if has_tick {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("首次停用 tick 应及时写入心跳");
        tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("停止信号应打断周期等待")
            .unwrap();
    }

    #[tokio::test]
    async fn already_stopped_worker_does_not_write_a_tick() {
        let db = test_db();
        let (tx, rx) = stop_channel();
        tx.send(true).unwrap();
        test_runtime(&db).run(rx).await;
        let writes: i64 = db.conn.lock().unwrap().query_row("SELECT count(*) FROM engine_state WHERE key IN ('last_cycle_ts', 'last_cycle_ms', 'last_error')", [], |r| r.get(0)).unwrap();
        assert_eq!(writes, 0, "收到停止信号后不能执行新的周期");
    }

    #[tokio::test]
    async fn closing_a_demo_trade_triggers_review_and_preserves_injected_ids() {
        use crate::providers::{DemoProvider, SnapshotProvider};
        let db = test_db();
        {
            let conn = db.conn.lock().unwrap();
            conn.execute_batch(
                "ALTER TABLE positions ADD COLUMN ai_confidence REAL NOT NULL DEFAULT 0; \
                 ALTER TABLE positions ADD COLUMN ai_reason TEXT NOT NULL DEFAULT '';",
            )
            .unwrap();
            conn.execute(
                "UPDATE app_settings SET value=?1 WHERE key='engine'",
                [r#"{"enabled":true,"mode":"demo"}"#],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO app_settings VALUES('evolution',\
                 '{\"enabled\":true,\"review_on_close\":true,\"batch_review_size\":5}',0)",
                [],
            )
            .unwrap();
            conn.execute_batch(
                "INSERT INTO positions(symbol,side,qty,entry_price,mark_price,sl_price,tp_price,\
                 peak_price,initial_sl_price,signal_score,opened_at,status,mode,features) \
                 VALUES('BTCUSDT','long',0.5,51000,51000,50000,60000,51000,50000,0.0,1,'open','demo',\
                 '{\"injected_mantra_ids\":[1],\"injected_experience_ids\":[2]}');",
            )
            .unwrap();
        }
        let runtime = test_runtime(&db);
        let snapshot = DemoProvider::with_mark_price(50_000.0)
            .snapshot()
            .await
            .unwrap();
        runtime.persist_snapshot("demo", &snapshot).unwrap();
        {
            let conn = db.conn.lock().unwrap();
            let (features, trade_id): (String, i64) = conn
                .query_row(
                    "SELECT features, trade_id FROM positions WHERE symbol='BTCUSDT'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert!(trade_id > 0, "平仓成交必须回填到仓位");
            let trade_features: String = conn
                .query_row(
                    "SELECT features FROM trades WHERE id=?1",
                    [trade_id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(features, trade_features, "成交记录要携带决策时的注入 ID");
            assert!(trade_features.contains("injected_experience_ids"));
        }
        let mut seen = false;
        for _ in 0..100 {
            {
                let conn = db.conn.lock().unwrap();
                let count: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM engine_events WHERE category='evolution'",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap();
                if count > 0 {
                    seen = true;
                }
            }
            if seen {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            seen,
            "平仓提交后必须触发复盘并留下事件（样本不足时记录跳过原因）"
        );
    }

    #[test]
    fn apply_regime_transition_restores_previous_regime_when_activation_fails() {
        let conn = regime_transition_fixture(Some("range"), 100);

        apply_regime_transition_at(&conn, "demo", 10_000).expect("激活失败不能阻断周期");

        assert_eq!(
            regime_state(&conn, "regime.current.demo"),
            "range",
            "激活失败必须恢复旧 regime，下轮才能重新触发"
        );
        assert_eq!(regime_state(&conn, "regime.pending.demo"), "");
        assert_eq!(regime_state(&conn, "regime.pending_since.demo"), "");
        assert_eq!(
            regime_state(&conn, "regime.changed_at.demo"),
            "100",
            "changed_at 必须恢复激活前的值"
        );
        let failures: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM engine_events \
                 WHERE level='warn' AND category='ai_tuning' \
                 AND message LIKE '%regime_activation_failed%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(failures, 1);
    }

    #[test]
    fn apply_regime_transition_throttles_activation_failure_events() {
        let conn = regime_transition_fixture(Some("range"), 100);
        let base = 10_000;
        let failure_count = |conn: &rusqlite::Connection| -> i64 {
            conn.query_row(
                "SELECT COUNT(*) FROM engine_events \
                 WHERE category='ai_tuning' AND message LIKE '%regime_activation_failed%'",
                [],
                |row| row.get(0),
            )
            .unwrap()
        };

        apply_regime_transition_at(&conn, "demo", base).unwrap();
        apply_regime_transition_at(&conn, "demo", base + 3_599).unwrap();
        assert_eq!(failure_count(&conn), 1, "3600 秒内失败事件必须限流");
        assert_eq!(
            regime_state(&conn, "ai_tuning.regime_activation_failed_ts.demo"),
            base.to_string()
        );
        assert_eq!(
            regime_state(&conn, "regime.current.demo"),
            "range",
            "限流不影响失败回退"
        );

        apply_regime_transition_at(&conn, "demo", base + 3_600).unwrap();
        assert_eq!(failure_count(&conn), 2, "满 3600 秒后可再写一条");
        assert_eq!(
            regime_state(&conn, "ai_tuning.regime_activation_failed_ts.demo"),
            (base + 3_600).to_string()
        );
    }

    #[test]
    fn apply_regime_transition_clears_current_when_first_activation_fails() {
        let conn = regime_transition_fixture(None, 0);

        apply_regime_transition_at(&conn, "demo", 10_000).expect("首次激活失败不能阻断周期");

        assert_eq!(
            regime_state(&conn, "regime.current.demo"),
            "",
            "首次激活失败必须清空 current，下轮按首次初始化重试"
        );
        assert_eq!(regime_state(&conn, "regime.pending.demo"), "");
        assert_eq!(regime_state(&conn, "regime.pending_since.demo"), "");
        let failures: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM engine_events \
                 WHERE category='ai_tuning' AND message LIKE '%regime_activation_failed%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(failures, 1);
    }

    fn regime_transition_fixture(current: Option<&str>, changed_at: i64) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE app_settings(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER); \
             CREATE TABLE engine_state(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER); \
             CREATE TABLE engine_events(id INTEGER PRIMARY KEY, ts INTEGER, level TEXT, category TEXT, message TEXT, payload TEXT); \
             CREATE TABLE trades(id INTEGER PRIMARY KEY, ai_tuning_set_id INTEGER); \
             INSERT INTO app_settings VALUES('strategy','{\"timeframe\":\"1h\"}',0); \
             INSERT INTO app_settings VALUES('ai_tuning','{\"regime_confirm_sec\":0,\"regime_cooldown_sec\":0}',0); \
             INSERT INTO engine_state VALUES('market_regime.demo','chaos',0);",
        )
        .unwrap();
        if let Some(current) = current {
            conn.execute(
                "INSERT INTO engine_state VALUES('regime.current.demo',?1,0)",
                [current],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO engine_state VALUES('regime.changed_at.demo',?1,0)",
                [changed_at.to_string()],
            )
            .unwrap();
        }
        crate::schema::ensure_parameter_changes(&conn).unwrap();
        crate::schema::ensure_ai_tuning(&conn).unwrap();
        conn
    }

    fn regime_state(conn: &rusqlite::Connection, key: &str) -> String {
        conn.query_row(
            "SELECT value FROM engine_state WHERE key=?1",
            [key],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn ai_tuning_governance_fixture() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE app_settings(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO app_settings VALUES('strategy',?1,0)",
            [serde_json::json!({
                "timeframe": "1h",
                "atr_pct_min": 1.5,
                "atr_pct_max": 8,
                "volume_ratio_min": 1.1,
                "entry_threshold": 0.55
            })
            .to_string()],
        )
        .unwrap();
        crate::schema::migrate(&conn).unwrap();
        crate::ai_tuning::seed_bootstrap(&conn).unwrap();
        conn.execute(
            "INSERT INTO engine_state(key,value,updated_at) VALUES('regime.current.demo','range',0) \
             ON CONFLICT(key) DO UPDATE SET value='range'",
            [],
        )
        .unwrap();
        conn
    }

    fn governance_approved_candidate(conn: &rusqlite::Connection) -> i64 {
        let parent = crate::ai_tuning::active_for_regime(conn, "range")
            .unwrap()
            .unwrap();
        let snapshot = crate::ai_tuning::validate_candidate(
            &parent.params,
            &[crate::ai_client::ParamUpdate {
                group: "strategy".to_string(),
                key: "entry_threshold".to_string(),
                value: serde_json::json!(0.40),
                reason: String::new(),
            }],
        )
        .unwrap();
        let id = crate::ai_tuning::insert_candidate(
            conn,
            "range-ai-governance",
            "range",
            "1h",
            &snapshot,
            Some(parent.id),
        )
        .unwrap();
        crate::ai_tuning::mark_evidence(conn, id, true, &serde_json::json!({"gate":{"pass":true}}))
            .unwrap();
        id
    }

    fn enable_auto_apply(conn: &rusqlite::Connection, since: &str) {
        conn.execute(
            "INSERT INTO app_settings(key,value,updated_at) VALUES('ai_tuning',?1,0) \
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [serde_json::json!({
                "enabled": true,
                "auto_apply": true,
                "auto_apply_timeframes": "1h",
            })
            .to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO engine_state(key,value,updated_at) \
             VALUES('ai_tuning.auto_apply_since',?1,0) \
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [since],
        )
        .unwrap();
    }

    #[test]
    fn apply_ai_tuning_governance_materializes_approved_candidate() {
        let conn = ai_tuning_governance_fixture();
        let candidate_id = governance_approved_candidate(&conn);
        enable_auto_apply(&conn, "0");
        let now = crate::now();

        apply_ai_tuning_governance(&conn, "demo", "1h", true, false, now)
            .expect("自动启用不能阻断周期");

        let active = crate::ai_tuning::active_for_regime(&conn, "range")
            .unwrap()
            .unwrap();
        assert_eq!(active.id, candidate_id, "当前 regime 必须直接启用候选");
        let strategy_raw: String = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key='strategy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let strategy: serde_json::Value = serde_json::from_str(&strategy_raw).unwrap();
        assert_eq!(
            strategy["entry_threshold"],
            serde_json::json!(0.40),
            "当前 regime 必须物化 strategy"
        );
        let trigger: String = conn
            .query_row(
                "SELECT trigger FROM ai_tuning_activations ORDER BY id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(trigger, "auto_apply");
        let guard_raw: String = conn
            .query_row(
                "SELECT value FROM engine_state WHERE key='ai_tuning.guard.range'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let guard: serde_json::Value = serde_json::from_str(&guard_raw).unwrap();
        assert_eq!(guard["set_id"], serde_json::json!(candidate_id));
        assert_eq!(guard["rollback_done"], serde_json::json!(false));
    }

    #[test]
    fn apply_ai_tuning_governance_rolls_back_after_no_candidate_window() {
        let conn = ai_tuning_governance_fixture();
        let bootstrap_id: i64 = conn
            .query_row(
                "SELECT id FROM ai_tuning_sets WHERE name='range-bootstrap'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let candidate_id = governance_approved_candidate(&conn);
        crate::ai_tuning::activate(&conn, candidate_id, "regime", "demo").unwrap();
        let now = crate::now();
        crate::ai_tuning::set_guard(&conn, "range", candidate_id, now).unwrap();

        apply_ai_tuning_governance(&conn, "demo", "1h", false, false, now + 1_799)
            .expect("守卫推进不能阻断周期");
        assert_eq!(
            crate::ai_tuning::active_for_regime(&conn, "range")
                .unwrap()
                .unwrap()
                .id,
            candidate_id,
            "未满窗口不得回滚"
        );

        apply_ai_tuning_governance(&conn, "demo", "1h", false, false, now + 1_800)
            .expect("守卫回滚不能阻断周期");
        assert_eq!(
            crate::ai_tuning::active_for_regime(&conn, "range")
                .unwrap()
                .unwrap()
                .id,
            bootstrap_id,
            "无候选满窗口必须回滚到上一套"
        );
        let trigger: String = conn
            .query_row(
                "SELECT trigger FROM ai_tuning_activations WHERE set_id=?1 \
                 ORDER BY id DESC LIMIT 1",
                [bootstrap_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(trigger, "no_candidate");
        let warnings: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM engine_events WHERE level='warn' \
                 AND message LIKE 'ai_tuning.no_candidate_rollback%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(warnings, 1);
    }

    #[test]
    fn apply_ai_tuning_governance_pauses_on_skipped_cycle() {
        let conn = ai_tuning_governance_fixture();
        let candidate_id = governance_approved_candidate(&conn);
        crate::ai_tuning::activate(&conn, candidate_id, "regime", "demo").unwrap();
        let now = crate::now();
        crate::ai_tuning::set_guard(&conn, "range", candidate_id, now).unwrap();

        apply_ai_tuning_governance(&conn, "demo", "1h", false, true, now + 10_000)
            .expect("熔断轮不得报错");
        assert_eq!(
            crate::ai_tuning::active_for_regime(&conn, "range")
                .unwrap()
                .unwrap()
                .id,
            candidate_id,
            "skipped 轮次必须暂停计时，不得累计零候选"
        );
        let paused: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM engine_events WHERE message LIKE 'ai_tuning.guard_paused%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(paused, 1);
    }

    fn test_db() -> Db {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE app_settings(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER); CREATE TABLE cooldowns(symbol TEXT PRIMARY KEY, until_ts INTEGER, reason TEXT, kind TEXT); CREATE TABLE engine_state(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER); CREATE TABLE positions(id INTEGER PRIMARY KEY, symbol TEXT, side TEXT, qty REAL, entry_price REAL, mark_price REAL, leverage REAL, margin REAL, sl_price REAL, tp_price REAL, peak_price REAL, initial_sl_price REAL, signal_score REAL, opened_at INTEGER, closed_at INTEGER, exit_price REAL, exit_reason TEXT, realized_pnl REAL, fees REAL, status TEXT, mode TEXT, venue_unrealized REAL, venue_ts INTEGER, partial_taken_at INTEGER, features TEXT, trade_id INTEGER DEFAULT 0); CREATE TABLE orders(id INTEGER PRIMARY KEY, ts INTEGER, symbol TEXT, side TEXT, order_type TEXT, qty REAL, price REAL, status TEXT, purpose TEXT, client_order_id TEXT, exchange_order_id TEXT, reduce_only INTEGER, mode TEXT, latency_ms INTEGER NOT NULL DEFAULT 0); CREATE TABLE engine_events(id INTEGER PRIMARY KEY, ts INTEGER, level TEXT, category TEXT, message TEXT, payload TEXT); CREATE TABLE trades(id INTEGER PRIMARY KEY, symbol TEXT, side TEXT, qty REAL, entry_price REAL, exit_price REAL, leverage REAL, notional REAL, pnl REAL, pnl_pct REAL, roi_pct REAL, fees REAL, opened_at INTEGER, closed_at INTEGER, hold_seconds INTEGER, exit_reason TEXT, ai_confidence REAL, ai_reason TEXT, signal_score REAL, slippage_bps REAL, mode TEXT, reviewed INTEGER, review_note TEXT, features TEXT, mantra_ids TEXT); CREATE TABLE mantras(id INTEGER PRIMARY KEY, title TEXT, rule TEXT, tags TEXT, category TEXT, source TEXT, confidence REAL, health REAL, hits INTEGER, wins INTEGER, losses INTEGER, enabled INTEGER, visible INTEGER, version INTEGER, superseded_by INTEGER, evidence TEXT, created_at INTEGER, updated_at INTEGER, expires_at INTEGER); CREATE TABLE experiences(id INTEGER PRIMARY KEY, symbol TEXT, tags TEXT, scenario TEXT, lesson TEXT, outcome TEXT, source_decision_id INTEGER, source TEXT, hits INTEGER, wins INTEGER, losses INTEGER, enabled INTEGER, created_at INTEGER, updated_at INTEGER); CREATE TABLE decision_journal(id INTEGER PRIMARY KEY, ts INTEGER, symbol TEXT, direction TEXT, confidence REAL, entry_price REAL, target_pct REAL, reason TEXT, snapshot TEXT, decision TEXT, signal_id INTEGER, injected_experience_ids TEXT, status TEXT, outcome TEXT, actual_price REAL, actual_pct REAL, labeled_at INTEGER, archived INTEGER, mode TEXT); CREATE TABLE signals(id INTEGER PRIMARY KEY, ts INTEGER, symbol TEXT, direction TEXT, score REAL, features TEXT, ai_action TEXT, ai_confidence REAL, ai_reason TEXT, decision TEXT, reject_reason TEXT, mode TEXT, repeats INTEGER); CREATE TABLE evolution_runs(id INTEGER PRIMARY KEY, ts INTEGER, kind TEXT, input_count INTEGER, produced INTEGER, replaced INTEGER, ok INTEGER, summary TEXT, raw TEXT); INSERT INTO app_settings VALUES('engine','{\"enabled\":false,\"cycle_interval_sec\":0,\"mode\":\"demo\"}',0); INSERT INTO engine_state VALUES('enabled','false',0); INSERT INTO engine_state VALUES('mode','demo',0);").unwrap();
        Db {
            path: String::new(),
            conn: Arc::new(std::sync::Mutex::new(conn)),
        }
    }
}
