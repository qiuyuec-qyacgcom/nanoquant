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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    pub fn ok(id: &str) -> Self {
        Self { id: id.to_string(), ok: true, signature: None, algo: None, query: None, error: None }
    }

    pub fn err(id: &str, msg: impl Into<String>) -> Self {
        Self { id: id.to_string(), ok: false, signature: None, algo: None, query: None, error: Some(msg.into()) }
    }
}
