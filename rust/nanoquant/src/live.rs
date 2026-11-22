//! 实盘下单：参数组装、回执判定、未知结果查询与保护单。
//!
//! 三条硬规矩，和 `submit.rs` 的错误分类保持一致：
//! 1. 写请求超时/连接中断/5xx 一律是**结果未知**，必须查询确认，绝不重发；
//! 2. 只有明确拒单（带 code 的 4xx）才算失败，可以继续下一轮；
//! 3. 保护单结果未知时也绝不回退重发，否则会挂出两套条件单。

#[cfg(test)]
use crate::submit::ReplyClass;
use crate::submit::{BanGate, PreparedRequest};
use serde_json::Value;
use std::collections::BTreeMap;

pub(crate) const ORDER_PATH: &str = "/fapi/v1/order";
pub(crate) const ALGO_PATH: &str = "/fapi/v1/algoOrder";

/// 交易对的步进与下限。取值来自 `trading_symbols`，缺失时回落到保守默认。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct SymbolFilter {
    pub qty_step: f64,
    pub min_qty: f64,
    pub price_tick: f64,
    pub min_notional: f64,
}

impl Default for SymbolFilter {
    fn default() -> Self {
        Self {
            qty_step: 0.001,
            min_qty: 0.001,
            price_tick: 0.01,
            min_notional: 5.0,
        }
    }
}

impl SymbolFilter {
    pub(crate) fn floor_qty(&self, qty: f64) -> f64 {
        floor_to_step(qty, self.qty_step)
    }

    pub(crate) fn ceil_qty(&self, qty: f64) -> f64 {
        ceil_to_step(qty, self.qty_step)
    }

    /// 按步进上取整后的真实最小可下单量。
    ///
    /// 交易所只保证「数量 ≥ min_qty」和「名义额 ≥ min_notional」，
    /// 真实门槛是两者里更严的那个，卡在 min_qty 上常常仍被 -1013 拒。
    pub(crate) fn effective_min_qty(&self, price: f64) -> f64 {
        let min_by_notional = if price.is_finite() && price > 0.0 {
            ceil_to_step(self.min_notional / price, self.qty_step)
        } else {
            0.0
        };
        self.min_qty
            .max(min_by_notional)
            .max(self.qty_step)
            .max(0.0)
    }

    pub(crate) fn effective_min_notional(&self, price: f64) -> f64 {
        round_n(self.effective_min_qty(price) * price, 8)
    }

    pub(crate) fn validate(&self, qty: f64, price: f64) -> Result<(), String> {
        if !qty.is_finite() || qty <= 0.0 {
            return Err("数量为 0".into());
        }
        if qty + 1e-12 < self.min_qty {
            return Err(format!("低于最小数量 {}", self.min_qty));
        }
        let notional = qty * price;
        if notional + 1e-9 < self.min_notional {
            return Err(format!(
                "名义额 {notional:.4} USDT 低于交易所下限 {} USDT",
                self.min_notional
            ));
        }
        Ok(())
    }

    pub(crate) fn format_qty(&self, qty: f64) -> String {
        format_number(qty, decimals_of(self.qty_step))
    }

    pub(crate) fn format_price(&self, price: f64) -> String {
        format_number(price, decimals_of(self.price_tick))
    }
}

fn round_n(value: f64, digits: i32) -> f64 {
    let factor = 10f64.powi(digits);
    (value * factor).round() / factor
}

/// 交易所拒绝的精度问题几乎都来自浮点尾巴：0.1*3 是 0.30000000000000004。
pub(crate) fn floor_to_step(value: f64, step: f64) -> f64 {
    if !value.is_finite() || !step.is_finite() || step <= 0.0 {
        return value;
    }
    let steps = (value / step + 1e-9).floor();
    format_number(steps * step, decimals_of(step))
        .parse()
        .unwrap_or(0.0)
}

pub(crate) fn ceil_to_step(value: f64, step: f64) -> f64 {
    if !value.is_finite() || !step.is_finite() || step <= 0.0 {
        return value;
    }
    let steps = (value / step - 1e-9).ceil();
    format_number(steps * step, decimals_of(step))
        .parse()
        .unwrap_or(0.0)
}

pub(crate) fn decimals_of(step: f64) -> usize {
    if !step.is_finite() || step <= 0.0 {
        return 8;
    }
    let mut scaled = step;
    for digits in 0..=8 {
        if (scaled - scaled.round()).abs() < 1e-9 {
            return digits;
        }
        scaled *= 10.0;
    }
    8
}

fn format_number(value: f64, decimals: usize) -> String {
    if !value.is_finite() {
        return "0".into();
    }
    let text = format!("{value:.decimals$}");
    let trimmed = text.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() || trimmed == "-" {
        "0".into()
    } else {
        trimmed.to_string()
    }
}

/// 一笔准备发出的订单。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OrderSpec {
    pub symbol: String,
    /// 交易所方向：平多 SELL，平空 BUY。
    pub side: String,
    pub qty: f64,
    pub reduce_only: bool,
    pub position_side: String,
    pub dual_side: bool,
    pub purpose: String,
    pub client_order_id: String,
}

impl OrderSpec {
    pub(crate) fn params(
        &self,
        filter: &SymbolFilter,
        recv_window_ms: i64,
    ) -> Vec<(String, String)> {
        let mut params = crate::sign::order_params(
            &self.symbol,
            &self.side,
            &self.position_side,
            &filter.format_qty(self.qty),
            &self.client_order_id,
            self.dual_side,
            self.reduce_only,
        );
        params.push(("recvWindow".into(), recv_window_ms.to_string()));
        params
    }
}

/// 开仓前的准入。只有通过这里的数量才允许真正发到交易所。
pub(crate) fn admit_qty(
    qty: f64,
    price: f64,
    filter: &SymbolFilter,
    notional_buffer: f64,
) -> Result<f64, String> {
    if !price.is_finite() || price <= 0.0 {
        return Err("标记价无效，拒绝下单".into());
    }
    if !qty.is_finite() || qty <= 0.0 {
        return Err("下单数量无效".into());
    }
    let qty = filter.floor_qty(qty);
    let min_qty = filter.effective_min_qty(price);
    if qty <= 0.0 || qty + 1e-12 < min_qty {
        return Err(format!("数量 {qty} 低于交易所最小可下单量 {min_qty}"));
    }
    let buffer = notional_buffer.max(1.0);
    let need = filter.effective_min_notional(price) * buffer;
    let notional = qty * price;
    if need > 0.0 && notional + 1e-9 < need {
        return Err(format!("名义价值 {notional:.4} 低于交易所下限 {need:.4}"));
    }
    filter.validate(qty, price)?;
    Ok(qty)
}

/// 交易所对一次写请求的结论。

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Reply {
    Filled {
        order_id: String,
        executed_qty: f64,
        avg_price: f64,
    },
    /// 已受理但还没成交（限价单）或成交量为 0。
    Accepted {
        order_id: String,
        status: String,
    },
    Rejected {
        code: i64,
        message: String,
    },
    /// -2013：按 clientOrderId 查不到。它**不代表**订单没提交。
    NotFound,
    Unknown,
}

impl Reply {
    #[cfg(test)]
    pub(crate) fn class(&self) -> ReplyClass {
        match self {
            Reply::Filled { .. } => ReplyClass::Filled,
            Reply::Accepted { status, .. } if status.eq_ignore_ascii_case("PARTIALLY_FILLED") => {
                ReplyClass::PartiallyFilled
            }
            Reply::Accepted { .. } => ReplyClass::Accepted,
            Reply::Rejected { .. } => ReplyClass::Rejected,
            Reply::NotFound => ReplyClass::NotFound,
            Reply::Unknown => ReplyClass::Unknown,
        }
    }

    pub(crate) fn status_text(&self) -> String {
        match self {
            Reply::Filled { .. } => "FILLED".into(),
            Reply::Accepted { status, .. } => {
                if status.is_empty() {
                    "NEW".into()
                } else {
                    status.clone()
                }
            }
            Reply::Rejected { .. } => "REJECTED".into(),
            Reply::NotFound => "NOT_FOUND".into(),
            Reply::Unknown => "UNKNOWN".into(),
        }
    }
}

fn number(value: &Value) -> f64 {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|text| text.parse::<f64>().ok()))
        .filter(|parsed| parsed.is_finite())
        .unwrap_or(0.0)
}

/// 判定一次回执。`method` 只用于区分读写，写请求才把不可读正文当未知。
pub(crate) fn classify_reply(method: &str, status: u16, body: &str) -> Reply {
    // 5xx/429/3xx 与「2xx 但正文不可读」都由 `interpret` 判成结果未知，
    // 这里复用同一套分类，避免两处规则漂移。
    if crate::submit::interpret(method, status, body).uncertain {
        return Reply::Unknown;
    }
    let Some(parsed) = serde_json::from_str::<Value>(body).ok() else {
        return Reply::Unknown;
    };
    if (200..300).contains(&status) {
        let order_id = parsed["orderId"]
            .as_i64()
            .map(|id| id.to_string())
            .or_else(|| parsed["orderId"].as_str().map(str::to_string))
            .unwrap_or_default();
        let status_text = parsed["status"]
            .as_str()
            .unwrap_or("")
            .trim()
            .to_ascii_uppercase();
        let executed_qty = number(&parsed["executedQty"]);
        let avg_price = number(&parsed["avgPrice"]);
        if status_text == "FILLED"
            || (matches!(
                status_text.as_str(),
                "CANCELED" | "EXPIRED" | "EXPIRED_IN_MATCH"
            ) && executed_qty > 0.0)
        {
            return Reply::Filled {
                order_id,
                executed_qty,
                avg_price,
            };
        }
        return Reply::Accepted {
            order_id,
            status: status_text,
        };
    }
    let code = parsed["code"].as_i64().unwrap_or(-1);
    if code == -2013 {
        return Reply::NotFound;
    }
    Reply::Rejected {
        code,
        message: parsed["msg"].as_str().unwrap_or("").to_string(),
    }
}

pub(crate) fn client_order_id(prefix: &str, tag: &str, index: usize) -> String {
    let tag: String = tag
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(8)
        .collect();
    format!("{prefix}{}{:02}{tag}", crate::now_millis(), index % 100)
}

#[derive(Debug, Clone)]
pub(crate) struct GatewayReply {
    pub status: u16,
    pub body: String,
    pub transport_failed: bool,
}

pub(crate) type GatewayFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = GatewayReply> + Send + 'a>>;

/// 交易所出口。测试用假实现替换，业务代码只依赖这个接口。
pub(crate) trait Gateway: Send + Sync {
    fn send<'a>(&'a self, request: &'a PreparedRequest) -> GatewayFuture<'a>;
}

pub(crate) struct HttpGateway {
    transport: crate::submit::Transport,
}

impl HttpGateway {
    pub(crate) fn new() -> Self {
        Self {
            transport: crate::submit::Transport::new(),
        }
    }
}

impl Default for HttpGateway {
    fn default() -> Self {
        Self::new()
    }
}

impl Gateway for HttpGateway {
    fn send<'a>(&'a self, request: &'a PreparedRequest) -> GatewayFuture<'a> {
        Box::pin(async move {
            match self.transport.send_raw(request).await {
                Ok((status, body)) => GatewayReply {
                    status,
                    body,
                    transport_failed: false,
                },
                Err(()) => GatewayReply {
                    status: 0,
                    body: String::new(),
                    transport_failed: true,
                },
            }
        })
    }
}

pub(crate) async fn submit(gateway: &dyn Gateway, request: &PreparedRequest) -> Reply {
    let reply = gateway.send(request).await;
    if reply.transport_failed {
        return Reply::Unknown;
    }
    classify_reply(&request.method, reply.status, &reply.body)
}

pub(crate) fn query_params(
    symbol: &str,
    client_order_id: &str,
    recv_window_ms: i64,
) -> Vec<(String, String)> {
    vec![
        ("symbol".to_string(), symbol.to_string()),
        ("origClientOrderId".to_string(), client_order_id.to_string()),
        ("recvWindow".to_string(), recv_window_ms.to_string()),
    ]
}

/// 一道保护腿：优先 Algo 接口，只有明确拒绝才回落到老接口。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProtectionLeg {
    pub kind: String,
    pub algo_params: Vec<(String, String)>,
    pub legacy_params: Vec<(String, String)>,
}

#[cfg(test)]
pub(crate) fn protection_legs(
    symbol: &str,
    position_side: &str,
    sl_price: f64,
    tp_price: f64,
    filter: &SymbolFilter,
    recv_window_ms: i64,
    index: usize,
) -> Vec<ProtectionLeg> {
    protection_legs_mode(
        symbol,
        position_side,
        sl_price,
        tp_price,
        filter,
        recv_window_ms,
        index,
        false,
    )
}

pub(crate) fn protection_legs_mode(
    symbol: &str,
    position_side: &str,
    sl_price: f64,
    tp_price: f64,
    filter: &SymbolFilter,
    recv_window_ms: i64,
    index: usize,
    dual_side: bool,
) -> Vec<ProtectionLeg> {
    let close_side = if position_side == "long" {
        "SELL"
    } else {
        "BUY"
    };
    let mut legs = Vec::new();
    let mut push = |kind: &str, price: f64, tag: &str| {
        if !price.is_finite() || price <= 0.0 {
            return;
        }
        let price = filter.format_price(price);
        let algo_id = client_order_id("np", tag, index);
        let legacy_id = client_order_id("np", tag, index + 1);
        // 顺序与 legacy 内核（已退役） `_place_protection` 一致：单向不带 positionSide，
        // 双向才追加在签名字段末尾（recvWindow 由签名器另加）。
        let mut algo_params = vec![
            ("algoType".into(), "CONDITIONAL".into()),
            ("symbol".into(), symbol.into()),
            ("side".into(), close_side.into()),
            ("type".into(), kind.into()),
            ("triggerPrice".into(), price.clone()),
            ("closePosition".into(), "true".into()),
            ("workingType".into(), "MARK_PRICE".into()),
            ("priceProtect".into(), "TRUE".into()),
            ("clientAlgoId".into(), algo_id),
        ];
        let mut legacy_params = vec![
            ("symbol".into(), symbol.into()),
            ("side".into(), close_side.into()),
            ("type".into(), kind.into()),
            ("stopPrice".into(), price),
            ("closePosition".into(), "true".into()),
            ("workingType".into(), "MARK_PRICE".into()),
            ("priceProtect".into(), "TRUE".into()),
            ("newClientOrderId".into(), legacy_id),
        ];
        if dual_side {
            let venue_side = if position_side == "long" {
                "LONG"
            } else {
                "SHORT"
            };
            algo_params.push(("positionSide".into(), venue_side.into()));
            legacy_params.push(("positionSide".into(), venue_side.into()));
        }
        algo_params.push(("recvWindow".into(), recv_window_ms.to_string()));
        legacy_params.push(("recvWindow".into(), recv_window_ms.to_string()));
        legs.push(ProtectionLeg {
            kind: kind.to_string(),
            algo_params,
            legacy_params,
        });
    };
    push("STOP_MARKET", sl_price, "sl");
    push("TAKE_PROFIT_MARKET", tp_price, "tp");
    legs
}

/// 一条保护腿的结果。`unknown` 为真时必须记下来，下一轮不能再挂一遍。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LegOutcome {
    pub failure: Option<String>,
    pub unknown: bool,
}

impl LegOutcome {
    fn ok() -> Self {
        Self {
            failure: None,
            unknown: false,
        }
    }

    fn unknown(reason: String) -> Self {
        Self {
            failure: Some(reason),
            unknown: true,
        }
    }

    fn failed(reason: String) -> Self {
        Self {
            failure: Some(reason),
            unknown: false,
        }
    }
}

/// 挂一条保护腿。`failure` 为空表示挂上了。
pub(crate) async fn place_protection_leg(
    gate: &mut BanGate,
    gateway: &dyn Gateway,
    base: &str,
    api_key: &str,
    secret: &str,
    leg: &ProtectionLeg,
) -> LegOutcome {
    let request = match prepare(
        gate,
        base,
        "POST",
        ALGO_PATH,
        &leg.algo_params,
        api_key,
        secret,
    )
    .await
    {
        Ok(request) => request,
        Err(error) => return LegOutcome::failed(format!("{}: {error}", leg.kind)),
    };
    let raw = gateway.send(&request).await;
    if raw.transport_failed {
        return LegOutcome::unknown(format!("{}: 结果未知，已阻止重发", leg.kind));
    }
    crate::exchange_time::note_response(base, raw.status, &raw.body).await;
    let reply = classify_reply("POST", raw.status, &raw.body);
    if let Reply::Rejected {
        code: -1003,
        message,
    } = &reply
    {
        gate.observe(message, crate::now_millis());
    }
    match reply {
        Reply::Filled { .. } | Reply::Accepted { .. } => LegOutcome::ok(),
        Reply::Rejected { code, .. } => {
            // 只有明确拒绝才回落；未知结果回落等于挂第二套保护单。
            if !crate::submit::protection_plan("POST", raw.status, &raw.body).fallback {
                return LegOutcome::failed(format!("{}: 交易所拒绝 {code}", leg.kind));
            }
            let legacy = match prepare(
                gate,
                base,
                "POST",
                ORDER_PATH,
                &leg.legacy_params,
                api_key,
                secret,
            )
            .await
            {
                Ok(request) => request,
                Err(error) => return LegOutcome::failed(format!("{}: {error}", leg.kind)),
            };
            match submit(gateway, &legacy).await {
                Reply::Filled { .. } | Reply::Accepted { .. } => LegOutcome::ok(),
                Reply::Rejected { code, message } => {
                    LegOutcome::failed(format!("{}: 回落失败 {code} {message}", leg.kind))
                }
                _ => LegOutcome::unknown(format!("{}: 回落结果未知，已阻止重发", leg.kind)),
            }
        }
        Reply::Unknown => LegOutcome::unknown(format!("{}: 结果未知，已阻止重发", leg.kind)),
        Reply::NotFound => LegOutcome::unknown(format!("{}: 交易所查不到该保护单", leg.kind)),
    }
}

async fn prepare(
    gate: &mut BanGate,
    base: &str,
    method: &str,
    path: &str,
    params: &[(String, String)],
    api_key: &str,
    secret: &str,
) -> Result<PreparedRequest, String> {
    let now = crate::now_millis();
    let timestamp = crate::exchange_time::signed_timestamp(base).await;
    crate::submit::prepare(
        gate, now, method, base, path, params, api_key, secret, timestamp, 5_000,
    )
}

/// 从 `trading_symbols` 读取步进与下限。缺字段时用保守默认值。
pub(crate) fn load_filters(
    conn: &rusqlite::Connection,
    symbols: &[String],
) -> BTreeMap<String, SymbolFilter> {
    let mut filters = BTreeMap::new();
    if symbols.is_empty() {
        return filters;
    }
    let Ok(mut stmt) = conn.prepare(
        "SELECT symbol, tick_sz, min_sz, min_notional FROM trading_symbols WHERE enabled != 0",
    ) else {
        return filters;
    };
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<f64>>(3)?,
        ))
    });
    let Ok(rows) = rows else {
        return filters;
    };
    for row in rows.flatten() {
        let (symbol, tick, min_sz, min_notional) = row;
        if !symbols.iter().any(|item| item == &symbol) {
            continue;
        }
        let default = SymbolFilter::default();
        let filter = SymbolFilter {
            qty_step: min_sz
                .as_deref()
                .and_then(|text| text.parse::<f64>().ok())
                .filter(|value| *value > 0.0)
                .unwrap_or(default.qty_step),
            min_qty: min_sz
                .as_deref()
                .and_then(|text| text.parse::<f64>().ok())
                .filter(|value| *value > 0.0)
                .unwrap_or(default.min_qty),
            price_tick: tick
                .as_deref()
                .and_then(|text| text.parse::<f64>().ok())
                .filter(|value| *value > 0.0)
                .unwrap_or(default.price_tick),
            min_notional: min_notional
                .filter(|value| *value > 0.0)
                .unwrap_or(default.min_notional),
        };
        filters.insert(symbol, filter);
    }
    filters
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::submit::build_request;
    use std::sync::Mutex;

    #[test]
    fn hedge_protection_params_keep_the_position_direction() {
        let filter = SymbolFilter {
            qty_step: 0.001,
            min_qty: 0.001,
            price_tick: 0.01,
            min_notional: 5.0,
        };
        for (position, expected, close_side) in
            [("long", "LONG", "SELL"), ("short", "SHORT", "BUY")]
        {
            let legs =
                protection_legs_mode("BTCUSDT", position, 95.0, 110.0, &filter, 5000, 0, true);
            assert_eq!(legs.len(), 2);
            for leg in legs {
                for params in [&leg.algo_params, &leg.legacy_params] {
                    assert!(params
                        .iter()
                        .any(|(key, value)| key == "positionSide" && value == expected));
                    assert!(params
                        .iter()
                        .any(|(key, value)| key == "side" && value == close_side));
                    assert!(params
                        .iter()
                        .any(|(key, value)| key == "closePosition" && value == "true"));
                }
            }
        }
    }

    #[test]
    fn single_side_order_parameters_distinguish_entry_and_close() {
        let entry =
            crate::sign::order_params("BTCUSDT", "BUY", "long", "0.25", "nq-entry", false, false);
        assert!(!entry.iter().any(|(key, _)| key == "positionSide"));
        assert!(!entry.iter().any(|(key, _)| key == "reduceOnly"));
        let close =
            crate::sign::order_params("BTCUSDT", "SELL", "long", "0.25", "nq-close", false, true);
        assert!(!close.iter().any(|(key, _)| key == "positionSide"));
        assert!(close
            .iter()
            .any(|(key, value)| key == "reduceOnly" && value == "true"));
    }

    #[test]
    fn hedge_order_parameters_use_position_side_and_never_reduce_only() {
        for (side, position_side, closing) in [
            ("BUY", "long", false),
            ("SELL", "long", true),
            ("SELL", "short", false),
            ("BUY", "short", true),
        ] {
            let params = crate::sign::order_params(
                "BTCUSDT",
                side,
                position_side,
                "0.25",
                "nq-hedge",
                true,
                closing,
            );
            let expected = if position_side == "long" {
                "LONG"
            } else {
                "SHORT"
            };
            assert!(params
                .iter()
                .any(|(key, value)| key == "positionSide" && value == expected));
            assert!(!params.iter().any(|(key, _)| key == "reduceOnly"));
        }
    }

    /// 按脚本回包的假出口，同时记录发出去的请求。
    struct FakeGateway {
        replies: Mutex<Vec<GatewayReply>>,
        sent: Mutex<Vec<String>>,
    }

    impl FakeGateway {
        fn new(replies: Vec<(u16, &str)>) -> Self {
            Self {
                replies: Mutex::new(
                    replies
                        .into_iter()
                        .map(|(status, body)| GatewayReply {
                            status,
                            body: body.to_string(),
                            transport_failed: false,
                        })
                        .collect(),
                ),
                sent: Mutex::new(Vec::new()),
            }
        }

        fn timeout() -> Self {
            Self {
                replies: Mutex::new(Vec::new()),
                sent: Mutex::new(Vec::new()),
            }
        }

        fn sent_count(&self) -> usize {
            self.sent.lock().unwrap().len()
        }
    }

    impl Gateway for FakeGateway {
        fn send<'a>(&'a self, request: &'a PreparedRequest) -> GatewayFuture<'a> {
            self.sent.lock().unwrap().push(request.url.clone());
            let mut replies = self.replies.lock().unwrap();
            let reply = if replies.is_empty() {
                GatewayReply {
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

    fn spec() -> OrderSpec {
        OrderSpec {
            symbol: "BTCUSDT".into(),
            side: "SELL".into(),
            qty: 0.25,
            reduce_only: true,
            position_side: "long".into(),
            dual_side: false,
            purpose: "stop_loss".into(),
            client_order_id: "nq42".into(),
        }
    }

    fn request() -> PreparedRequest {
        build_request(
            "POST",
            "https://a.example",
            ORDER_PATH,
            &spec().params(&SymbolFilter::default(), 5_000),
            "key",
            "secret",
            1_700_000_000_000,
            5_000,
        )
    }

    #[test]
    fn quantities_are_floored_to_the_exchange_step() {
        let filter = SymbolFilter {
            qty_step: 0.001,
            min_qty: 0.001,
            price_tick: 0.1,
            min_notional: 5.0,
        };
        assert_eq!(filter.floor_qty(0.30000000000000004), 0.3);
        assert_eq!(filter.format_qty(0.3), "0.3");
        assert_eq!(filter.floor_qty(0.0009), 0.0);
        assert_eq!(filter.format_price(50_000.04), "50000");
        assert_eq!(filter.format_price(50_000.16), "50000.2");
    }

    #[test]
    fn admission_rejects_qty_below_the_exchange_floor_and_min_notional() {
        let filter = SymbolFilter {
            qty_step: 0.001,
            min_qty: 0.01,
            price_tick: 0.1,
            min_notional: 5.0,
        };
        assert!(
            admit_qty(0.005, 50_000.0, &filter, 1.05).is_err(),
            "低于最小量"
        );
        assert!(
            admit_qty(0.01, 100.0, &filter, 1.05).is_err(),
            "名义价值不足"
        );
        assert_eq!(admit_qty(0.02, 50_000.0, &filter, 1.05).unwrap(), 0.02);
    }

    #[test]
    fn a_zero_executed_order_is_accepted_not_filled() {
        let reply = classify_reply(
            "POST",
            200,
            r#"{"orderId":7,"status":"NEW","executedQty":"0","avgPrice":"0"}"#,
        );
        assert_eq!(
            reply,
            Reply::Accepted {
                order_id: "7".into(),
                status: "NEW".into()
            },
            "挂了未成不能写成本地已成交的仓位"
        );
    }

    #[test]
    fn a_filled_order_carries_quantity_and_average_price() {
        let reply = classify_reply(
            "POST",
            200,
            r#"{"orderId":9,"status":"FILLED","executedQty":"0.25","avgPrice":"49850.5"}"#,
        );
        assert_eq!(
            reply,
            Reply::Filled {
                order_id: "9".into(),
                executed_qty: 0.25,
                avg_price: 49_850.5
            }
        );
    }

    #[test]
    fn an_explicit_rejection_is_not_unknown() {
        let reply = classify_reply(
            "POST",
            400,
            r#"{"code":-2019,"msg":"Margin is insufficient."}"#,
        );
        assert_eq!(
            reply,
            Reply::Rejected {
                code: -2019,
                message: "Margin is insufficient.".into()
            }
        );
    }

    #[test]
    fn unreadable_bodies_and_5xx_are_unknown() {
        assert_eq!(
            classify_reply("POST", 200, "gateway timeout"),
            Reply::Unknown
        );
        assert_eq!(classify_reply("POST", 502, ""), Reply::Unknown);
        assert_eq!(classify_reply("POST", 429, ""), Reply::Unknown);
        assert_eq!(classify_reply("POST", 302, ""), Reply::Unknown);
    }

    #[test]
    fn a_missing_order_is_reported_as_not_found() {
        let reply = classify_reply(
            "GET",
            400,
            r#"{"code":-2013,"msg":"Order does not exist."}"#,
        );
        assert_eq!(reply, Reply::NotFound, "-2013 只说明查不到，不代表没提交");
    }

    #[test]
    fn reply_class_covers_all_business_outcomes() {
        assert_eq!(
            classify_reply(
                "POST",
                200,
                r#"{"orderId":1,"status":"FILLED","executedQty":"1","avgPrice":"100"}"#,
            )
            .class(),
            ReplyClass::Filled
        );
        assert_eq!(
            classify_reply(
                "POST",
                200,
                r#"{"orderId":2,"status":"PARTIALLY_FILLED","executedQty":"0.5","avgPrice":"100"}"#,
            )
            .class(),
            ReplyClass::PartiallyFilled
        );
        assert_eq!(
            classify_reply("POST", 200, r#"{"orderId":3,"status":"NEW"}"#).class(),
            ReplyClass::Accepted
        );
        assert_eq!(
            classify_reply("POST", 400, r#"{"code":-2019,"msg":"margin"}"#).class(),
            ReplyClass::Rejected
        );
        assert_eq!(
            classify_reply("GET", 400, r#"{"code":-2013}"#).class(),
            ReplyClass::NotFound
        );
        assert_eq!(classify_reply("POST", 500, "").class(), ReplyClass::Unknown);
    }

    #[tokio::test]
    async fn a_transport_timeout_is_unknown_and_never_retried_here() {
        let gateway = FakeGateway::timeout();
        let reply = submit(&gateway, &request()).await;
        assert_eq!(reply, Reply::Unknown);
        assert_eq!(
            gateway.sent_count(),
            1,
            "只发一次，重发交给调用方的查询流程"
        );
    }

    #[tokio::test]
    async fn a_rejected_write_is_reported_with_its_code() {
        let gateway = FakeGateway::new(vec![(
            400,
            r#"{"code":-1111,"msg":"Precision is over the maximum."}"#,
        )]);
        let reply = submit(&gateway, &request()).await;
        assert_eq!(
            reply,
            Reply::Rejected {
                code: -1111,
                message: "Precision is over the maximum.".into()
            }
        );
    }

    #[test]
    fn protection_legs_quote_the_close_side_and_skip_empty_prices() {
        let filter = SymbolFilter::default();
        let legs = protection_legs("BTCUSDT", "long", 49_000.0, 52_000.0, &filter, 5_000, 1);
        assert_eq!(legs.len(), 2);
        assert_eq!(legs[0].kind, "STOP_MARKET");
        // 参数顺序就是签名顺序：错一位签名就对不上，条件单会被拒，仓位等于裸奔。
        let keys: Vec<&str> = legs[0]
            .algo_params
            .iter()
            .map(|(k, _)| k.as_str())
            .collect();
        assert_eq!(
            keys,
            [
                "algoType",
                "symbol",
                "side",
                "type",
                "triggerPrice",
                "closePosition",
                "workingType",
                "priceProtect",
                "clientAlgoId",
                "recvWindow"
            ],
            "保护单参数顺序必须与 legacy 内核一致"
        );
        let side = legs[0]
            .algo_params
            .iter()
            .find(|(key, _)| key == "side")
            .map(|(_, value)| value.as_str());
        assert_eq!(side, Some("SELL"), "平多必须用 SELL");
        assert!(legs[0]
            .algo_params
            .iter()
            .any(|(key, value)| key == "closePosition" && value == "true"));
        let short = protection_legs("BTCUSDT", "short", 1.0, 1.0, &filter, 5_000, 1);
        let side = short[0]
            .algo_params
            .iter()
            .find(|(key, _)| key == "side")
            .map(|(_, value)| value.as_str());
        assert_eq!(side, Some("BUY"), "平空必须用 BUY");

        let none = protection_legs("BTCUSDT", "long", 0.0, 0.0, &filter, 5_000, 1);
        assert!(none.is_empty(), "没有保护价就不该挂条件单");
    }

    #[tokio::test]
    async fn a_rejected_algo_leg_falls_back_to_the_legacy_order_endpoint() {
        let gateway = FakeGateway::new(vec![
            (400, r#"{"code":-4120,"msg":"use algo"}"#),
            (200, r#"{"orderId":11,"status":"NEW"}"#),
        ]);
        let mut gate = BanGate::new();
        let legs = protection_legs(
            "BTCUSDT",
            "long",
            49_000.0,
            52_000.0,
            &SymbolFilter::default(),
            5_000,
            1,
        );
        let failure = place_protection_leg(
            &mut gate,
            &gateway,
            "https://a.example",
            "key",
            "secret",
            &legs[0],
        )
        .await;
        assert!(
            failure.failure.is_none(),
            "明确拒绝可以回落，回落成功就算挂上"
        );
        let sent = gateway.sent.lock().unwrap();
        assert_eq!(sent.len(), 2);
        assert!(sent[0].contains(ALGO_PATH));
        assert!(sent[1].contains(ORDER_PATH));
    }

    #[tokio::test]
    async fn an_unknown_protection_result_never_falls_back() {
        let gateway = FakeGateway::timeout();
        let mut gate = BanGate::new();
        let legs = protection_legs(
            "BTCUSDT",
            "long",
            49_000.0,
            52_000.0,
            &SymbolFilter::default(),
            5_000,
            1,
        );
        let failure = place_protection_leg(
            &mut gate,
            &gateway,
            "https://a.example",
            "key",
            "secret",
            &legs[0],
        )
        .await;
        assert!(failure.unknown, "未知结果必须标记，下一轮不能再挂");
        assert!(failure.failure.unwrap().contains("未知"));
        assert_eq!(gateway.sent_count(), 1, "未知结果回落等于挂第二套保护单");
    }

    #[tokio::test]
    async fn a_rejected_protection_leg_is_reported_as_failure() {
        let gateway = FakeGateway::new(vec![(
            400,
            r#"{"code":-2019,"msg":"Margin is insufficient."}"#,
        )]);
        let mut gate = BanGate::new();
        let legs = protection_legs(
            "BTCUSDT",
            "long",
            49_000.0,
            52_000.0,
            &SymbolFilter::default(),
            5_000,
            1,
        );
        let failure = place_protection_leg(
            &mut gate,
            &gateway,
            "https://a.example",
            "key",
            "secret",
            &legs[0],
        )
        .await;
        assert!(
            failure.failure.unwrap().contains("-2019"),
            "拒单必须上报给调用方"
        );
        assert!(!failure.unknown, "明确拒单可以安全重试，不算未知");
    }
}
