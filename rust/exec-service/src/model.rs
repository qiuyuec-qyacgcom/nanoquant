//! UDS 协议（JSON Lines，一行一请求/一响应）

use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct Request {
    /// 请求 id，由调用方生成，原样回传
    pub id: String,
    /// sign | plan | health
    pub op: String,
    /// 待签名的 query string（不含 signature 本身）
    #[serde(default)]
    pub query: String,
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
            error: Some(msg.into()),
        }
    }
}
