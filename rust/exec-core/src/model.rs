//! exec-core 协议 v1.0（JSON Lines，一行一请求/一响应，UTF-8，`\n` 结尾）。
//! op 集：hello | health | sign | metrics_ingest | metrics_query | metrics_reset；
//! order_open / order_close / protection_place 为二期预留，一期返回 reserved for phase 2。

use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct Request {
    /// 请求 id，调用方生成，原样回传
    pub id: String,
    pub op: String,
    /// hello：调用方版本号
    #[serde(default)]
    pub version: String,
    /// sign：待签名的 query（不含 signature）
    #[serde(default)]
    pub query: String,
    /// metrics_ingest：批量样本
    #[serde(default)]
    pub samples: Vec<SampleIn>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SampleIn {
    /// total | plan | sign | http | wait | protect | persist
    pub segment: String,
    pub ms: f64,
    #[serde(default)]
    pub ts: i64,
}

#[derive(Debug, Serialize)]
pub struct Response {
    pub id: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub algo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kernel_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics_capacity: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uptime_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sessions: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accepted: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dropped: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub segments: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    pub fn ok(id: &str) -> Self {
        Self {
            id: id.to_string(),
            ok: true,
            algo: None,
            kernel_version: None,
            capabilities: None,
            metrics_capacity: None,
            uptime_ms: None,
            sessions: None,
            signature: None,
            query: None,
            accepted: None,
            dropped: None,
            segments: None,
            error: None,
        }
    }

    pub fn err(id: &str, msg: impl Into<String>) -> Self {
        Self {
            id: id.to_string(),
            ok: false,
            algo: None,
            kernel_version: None,
            capabilities: None,
            metrics_capacity: None,
            uptime_ms: None,
            sessions: None,
            signature: None,
            query: None,
            accepted: None,
            dropped: None,
            segments: None,
            error: Some(msg.into()),
        }
    }
}