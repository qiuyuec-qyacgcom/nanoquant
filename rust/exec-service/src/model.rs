//! UDS 协议（JSON Lines，一行一请求/一响应）

use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct Request {
    /// 请求 id，由调用方生成，原样回传
    pub id: String,
    /// sign | plan | health | probe | order_register | order_transition | order_get | order_snapshot
    pub op: String,
    /// 待签名的 query string（不含 signature 本身）
    #[serde(default)]
    pub query: String,
    /// P3-2：订单幂等键（币安 newClientOrderId）
    #[serde(default)]
    pub client_order_id: String,
    /// P3-2：标的
    #[serde(default)]
    pub symbol: String,
    /// P3-2：目标状态（迁移用），如 SENT / ACKED / FILLED
    #[serde(default)]
    pub state: String,
    /// P3-2：累计成交量（可选）
    #[serde(default)]
    pub filled_qty: Option<f64>,
    /// P3-2：成交均价（可选）
    #[serde(default)]
    pub avg_price: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct Response {
    pub id: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub algo: Option<String>,
    /// 回显签名用的 query，便于调用方逐字节比对（影子模式）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// P3-1：连接池状态（仅 health/probe 响应带，老调用方忽略即可）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http: Option<crate::http::HttpStatus>,
    /// probe 成功时的往返耗时（毫秒）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    /// P3-2：订单台账（register/transition/get/snapshot 回传）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<serde_json::Value>,
    /// P3-2：register 是否命中去重（幂等）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duplicate: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    pub fn ok(id: &str) -> Self {
        Self {
            id: id.to_string(),
            ok: true,
            signature: None,
            algo: None,
            query: None,
            http: None,
            latency_ms: None,
            order: None,
            duplicate: None,
            error: None,
        }
    }

    pub fn err(id: &str, msg: impl Into<String>) -> Self {
        Self {
            id: id.to_string(),
            ok: false,
            signature: None,
            algo: None,
            query: None,
            http: None,
            latency_ms: None,
            order: None,
            duplicate: None,
            error: Some(msg.into()),
        }
    }
}
