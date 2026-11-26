//! AI 供应商配置：增删改查、模型探测、连通性测试与诊断。
//!
//! 对齐 legacy 内核（已退役） `api/routes/ai.py` 与 `ai/diagnostics.py`：
//! API Key 以主密钥加密落库，查询只返回脱敏值与 `has_key`；
//! 探测端点先支持"未保存的地址 + 密钥"，也能用 `provider_id` 复用已存配置。

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::ai_client::{self, AiTransport, ChatCall, ChatMessage, HttpTransport, ProviderConfig};
use crate::ai_market::AiMarketSource;
use crate::db::Db;
use crate::reasoning;
use crate::secret;

#[derive(Clone)]
pub struct AiState {
    pub db: Db,
    pub master_key: Vec<u8>,
    pub transport: Arc<dyn AiTransport>,
    pub market: Arc<dyn AiMarketSource>,
}

impl AiState {
    pub fn new(db: Db, master_key: Vec<u8>) -> Self {
        Self {
            db,
            master_key,
            transport: Arc::new(HttpTransport::new()),
            market: Arc::new(crate::ai_market::UnavailableMarket),
        }
    }

    #[cfg(test)]
    pub fn with_transport(db: Db, master_key: Vec<u8>, transport: Arc<dyn AiTransport>) -> Self {
        Self {
            db,
            master_key,
            transport,
            market: Arc::new(crate::ai_market::UnavailableMarket),
        }
    }

    pub fn with_market(mut self, market: Arc<dyn AiMarketSource>) -> Self {
        self.market = market;
        self
    }

    fn key_text(&self) -> Result<String, String> {
        std::str::from_utf8(&self.master_key)
            .map(str::to_string)
            .map_err(|_| "主密钥格式无效".to_string())
    }
}

pub fn routes() -> Router<AiState> {
    Router::new()
        .route("/ai/providers", get(list_providers).post(create_provider))
        .route("/ai/providers/fetch-models", post(fetch_models))
        .route(
            "/ai/providers/{id}",
            axum::routing::patch(patch_provider).delete(delete_provider),
        )
        .route("/ai/providers/{id}/test", post(test_provider))
        .route("/ai/providers/{id}/diagnose", get(diagnose_provider))
        .route("/ai/providers/{id}/models", get(provider_models))
}

#[derive(Debug, Clone)]
struct ProviderRow {
    id: i64,
    name: String,
    protocol: String,
    base_url: String,
    api_key_enc: String,
    model: String,
    temperature: f64,
    max_tokens: i64,
    reasoning_effort: String,
    timeout_sec: i64,
    json_mode: bool,
    enabled: bool,
    priority: i64,
    role: String,
    extra: Value,
    created_at: i64,
    updated_at: i64,
}

const PROVIDER_COLUMNS: &str = "id, name, protocol, base_url, api_key_enc, model, temperature, \
     max_tokens, reasoning_effort, timeout_sec, json_mode, enabled, priority, role, extra, \
     created_at, updated_at";

fn load_providers(conn: &rusqlite::Connection) -> Vec<ProviderRow> {
    let sql = format!("SELECT {PROVIDER_COLUMNS} FROM ai_providers ORDER BY priority ASC, id ASC");
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return Vec::new();
    };
    let rows = stmt.query_map([], |row| {
        Ok(ProviderRow {
            id: row.get(0)?,
            name: row.get(1)?,
            protocol: row.get(2)?,
            base_url: row.get(3)?,
            api_key_enc: row.get(4)?,
            model: row.get(5)?,
            temperature: row.get(6)?,
            max_tokens: row.get(7)?,
            reasoning_effort: row.get(8)?,
            timeout_sec: row.get(9)?,
            json_mode: row.get::<_, i64>(10)? != 0,
            enabled: row.get::<_, i64>(11)? != 0,
            priority: row.get(12)?,
            role: row.get(13)?,
            extra: row
                .get::<_, Option<String>>(14)?
                .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                .unwrap_or_else(|| json!({})),
            created_at: row.get(15)?,
            updated_at: row.get(16)?,
        })
    });
    match rows {
        Ok(rows) => rows.filter_map(Result::ok).collect(),
        Err(_) => Vec::new(),
    }
}

fn load_provider(conn: &rusqlite::Connection, id: i64) -> Option<ProviderRow> {
    let sql = format!("SELECT {PROVIDER_COLUMNS} FROM ai_providers WHERE id = ?1");
    conn.query_row(&sql, [id], |row| {
        Ok(ProviderRow {
            id: row.get(0)?,
            name: row.get(1)?,
            protocol: row.get(2)?,
            base_url: row.get(3)?,
            api_key_enc: row.get(4)?,
            model: row.get(5)?,
            temperature: row.get(6)?,
            max_tokens: row.get(7)?,
            reasoning_effort: row.get(8)?,
            timeout_sec: row.get(9)?,
            json_mode: row.get::<_, i64>(10)? != 0,
            enabled: row.get::<_, i64>(11)? != 0,
            priority: row.get(12)?,
            role: row.get(13)?,
            extra: row
                .get::<_, Option<String>>(14)?
                .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                .unwrap_or_else(|| json!({})),
            created_at: row.get(15)?,
            updated_at: row.get(16)?,
        })
    })
    .ok()
}

/// 最近一段时间内按供应商聚合的 P95 延迟；样本不足的供应商不进入结果。
pub fn provider_p95_latency(
    conn: &rusqlite::Connection,
    since: i64,
    min_samples: usize,
) -> HashMap<String, f64> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT provider, latency_ms FROM ai_call_logs \
         WHERE ts >= ?1 AND ok = 1 AND latency_ms > 0",
    ) else {
        return HashMap::new();
    };
    let Ok(rows) = stmt.query_map([since], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    }) else {
        return HashMap::new();
    };
    let mut grouped: HashMap<String, Vec<i64>> = HashMap::new();
    for (provider, latency) in rows.flatten() {
        grouped.entry(provider).or_default().push(latency);
    }
    let mut out = HashMap::new();
    for (provider, mut values) in grouped {
        if values.len() < min_samples.max(1) {
            continue;
        }
        values.sort_unstable();
        let rank = ((values.len() as f64 * 0.95).ceil() as usize).clamp(1, values.len()) - 1;
        out.insert(provider, values[rank] as f64);
    }
    out
}

/// 稳定排序：有数据的供应商按 P95 升序在前，样本不足/无数据者保持原顺序在后。
pub fn order_chain_by_latency(
    chain: &[ProviderConfig],
    p95: &HashMap<String, f64>,
    min_samples: usize,
) -> Vec<ProviderConfig> {
    // p95 已由调用方按 min_samples 过滤；保留参数以便函数自证样本门槛语义。
    let _ = min_samples;
    let (mut ranked, rest): (Vec<ProviderConfig>, Vec<ProviderConfig>) = chain
        .iter()
        .cloned()
        .partition(|provider| p95.contains_key(&provider.name));
    ranked.sort_by(|left, right| {
        p95.get(&left.name)
            .unwrap_or(&f64::MAX)
            .partial_cmp(p95.get(&right.name).unwrap_or(&f64::MAX))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    ranked.extend(rest);
    ranked
}

/// 按 ai 配置生成调用链：主供应商 → 回退 → 其余启用项，最多 5 个。
pub fn provider_chain(state: &AiState) -> (ai_client::AiConfig, Vec<ProviderConfig>) {
    let conn = state.db.lock();
    let cfg = ai_client::read_ai_config_conn(&conn);
    let rows = load_providers(&conn);
    let p95 = if cfg.provider_order_mode == "fastest" {
        provider_p95_latency(&conn, crate::now() - 86400, 5)
    } else {
        HashMap::new()
    };
    drop(conn);
    let enabled: Vec<ProviderConfig> = rows
        .iter()
        .filter(|row| row.enabled)
        .map(|row| provider_config(row, &state.master_key))
        .filter(|provider| !provider.api_key.is_empty())
        .collect();
    let find = |id: i64| enabled.iter().find(|provider| provider.id == id).cloned();
    let mut chain: Vec<ProviderConfig> = Vec::new();
    if cfg.provider_id > 0 {
        if let Some(provider) = find(cfg.provider_id) {
            chain.push(provider);
        }
    }
    for id in &cfg.fallback_provider_ids {
        if let Some(provider) = find(*id) {
            if !chain.iter().any(|item| item.id == provider.id) {
                chain.push(provider);
            }
        }
    }
    for provider in enabled {
        if !chain.iter().any(|item| item.id == provider.id) {
            chain.push(provider);
        }
    }
    chain.truncate(5);
    if cfg.provider_order_mode == "fastest" {
        chain = order_chain_by_latency(&chain, &p95, 5);
    }
    (cfg, chain)
}

fn mask_secret(plain: &str) -> String {
    if plain.is_empty() {
        return String::new();
    }
    let chars: Vec<char> = plain.chars().collect();
    if chars.len() <= 8 {
        return "*".repeat(chars.len());
    }
    let head: String = chars.iter().take(4).collect();
    let tail: String = chars.iter().skip(chars.len() - 4).collect();
    format!("{head}******{tail}")
}

fn decrypt_key(master_key: &[u8], ciphertext: &str) -> String {
    let Ok(key) = std::str::from_utf8(master_key) else {
        return String::new();
    };
    secret::decrypt_secret(key, ciphertext).unwrap_or_default()
}

fn serialize(row: &ProviderRow, master_key: &[u8]) -> Value {
    let plain = decrypt_key(master_key, &row.api_key_enc);
    json!({
        "id": row.id,
        "name": row.name,
        "protocol": row.protocol,
        "base_url": row.base_url,
        "model": row.model,
        "temperature": row.temperature,
        "max_tokens": row.max_tokens,
        "reasoning_effort": if row.reasoning_effort.is_empty() { "auto".to_string() } else { row.reasoning_effort.clone() },
        "timeout_sec": row.timeout_sec,
        "json_mode": row.json_mode,
        "enabled": row.enabled,
        "priority": row.priority,
        "role": row.role,
        "api_key_masked": mask_secret(&plain),
        "has_key": !plain.is_empty(),
        "extra": row.extra,
        "created_at": row.created_at,
        "updated_at": row.updated_at,
    })
}

fn provider_config(row: &ProviderRow, master_key: &[u8]) -> ProviderConfig {
    ProviderConfig {
        id: row.id,
        name: row.name.clone(),
        protocol: row.protocol.clone(),
        base_url: row.base_url.clone(),
        api_key: decrypt_key(master_key, &row.api_key_enc),
        model: row.model.clone(),
        temperature: row.temperature,
        max_tokens: row.max_tokens,
        reasoning_effort: if row.reasoning_effort.is_empty() {
            "auto".to_string()
        } else {
            row.reasoning_effort.clone()
        },
        timeout_sec: row.timeout_sec,
        json_mode: row.json_mode,
    }
}

fn bad_request(detail: impl Into<String>) -> (StatusCode, Json<Value>) {
    (
        StatusCode::BAD_REQUEST,
        json!({ "detail": detail.into() }).into(),
    )
}

fn not_found() -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_FOUND,
        json!({ "detail": "模型配置不存在" }).into(),
    )
}

async fn list_providers(State(state): State<AiState>) -> Json<Value> {
    let conn = state.db.lock();
    let items: Vec<Value> = load_providers(&conn)
        .iter()
        .map(|row| serialize(row, &state.master_key))
        .collect();
    let cfg = ai_client::read_ai_config_conn(&conn);
    json!({
        "items": items,
        "active": {
            "mode": cfg.mode,
            "provider_id": cfg.provider_id,
            "fallbacks": cfg.fallback_provider_ids,
            "enabled": cfg.enabled,
        }
    })
    .into()
}

#[derive(Deserialize)]
struct ProviderIn {
    #[serde(default)]
    name: String,
    #[serde(default = "default_protocol")]
    protocol: String,
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    api_key: String,
    #[serde(default = "default_model")]
    model: String,
    #[serde(default = "default_temperature")]
    temperature: f64,
    #[serde(default = "default_max_tokens")]
    max_tokens: i64,
    #[serde(default = "default_effort")]
    reasoning_effort: String,
    #[serde(default = "default_timeout")]
    timeout_sec: i64,
    #[serde(default = "default_true")]
    json_mode: bool,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default = "default_priority")]
    priority: i64,
    #[serde(default = "default_role")]
    role: String,
    #[serde(default)]
    extra: Option<Value>,
}

fn default_protocol() -> String {
    "openai_chat".to_string()
}

fn default_model() -> String {
    "gpt-4o-mini".to_string()
}

fn default_temperature() -> f64 {
    0.2
}

fn default_max_tokens() -> i64 {
    3000
}

fn default_effort() -> String {
    "auto".to_string()
}

fn default_timeout() -> i64 {
    45
}

fn default_true() -> bool {
    true
}

fn default_priority() -> i64 {
    100
}

fn default_role() -> String {
    "primary".to_string()
}

async fn create_provider(
    State(state): State<AiState>,
    Json(body): Json<ProviderIn>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return Err(bad_request("name 不能为空"));
    }
    let base_url = body.base_url.trim().trim_end_matches('/').to_string();
    if base_url.is_empty() {
        return Err(bad_request("base_url 不能为空"));
    }
    let effort = reasoning::normalize_effort(&body.reasoning_effort).to_string();
    let key = state.key_text().map_err(bad_request)?;
    let encrypted = secret::encrypt_secret(&key, &body.api_key).map_err(bad_request)?;
    let extra = body.extra.unwrap_or_else(|| json!({}));
    let now = crate::now();
    let conn = state.db.lock();
    let exists: bool = conn
        .query_row(
            "SELECT 1 FROM ai_providers WHERE name = ?1 LIMIT 1",
            [&name],
            |_| Ok(()),
        )
        .is_ok();
    if exists {
        return Err(bad_request("同名模型配置已存在"));
    }
    conn.execute(
        "INSERT INTO ai_providers(name, protocol, base_url, api_key_enc, model, temperature, \
         max_tokens, reasoning_effort, timeout_sec, json_mode, enabled, priority, role, extra, \
         created_at, updated_at) \
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
        (
            &name,
            &body.protocol,
            &base_url,
            &encrypted,
            &body.model,
            body.temperature,
            body.max_tokens,
            &effort,
            body.timeout_sec,
            i64::from(body.json_mode),
            i64::from(body.enabled),
            body.priority,
            &body.role,
            serde_json::to_string(&extra).unwrap_or_else(|_| "{}".to_string()),
            now,
            now,
        ),
    )
    .map_err(|error| bad_request(format!("写入模型配置失败: {error}")))?;
    let provider_id = conn.last_insert_rowid();
    let cfg = ai_client::read_ai_config_conn(&conn);
    if cfg.provider_id == 0 {
        update_ai_group(&conn, json!({"provider_id": provider_id}));
    }
    tracing::info!(provider_id, %name, "新增 AI 模型配置");
    Ok(json!({ "ok": true, "id": provider_id }).into())
}

#[derive(Deserialize)]
struct ProviderPatch {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    protocol: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    temperature: Option<f64>,
    #[serde(default)]
    max_tokens: Option<i64>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default)]
    timeout_sec: Option<i64>,
    #[serde(default)]
    json_mode: Option<bool>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    priority: Option<i64>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    extra: Option<Value>,
}

async fn patch_provider(
    State(state): State<AiState>,
    Path(id): Path<i64>,
    Json(body): Json<ProviderPatch>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.db.lock();
    let Some(mut row) = load_provider(&conn, id) else {
        return Err(not_found());
    };
    if let Some(name) = body.name {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err(bad_request("name 不能为空"));
        }
        let duplicate: bool = conn
            .query_row(
                "SELECT 1 FROM ai_providers WHERE name = ?1 AND id <> ?2 LIMIT 1",
                (trimmed, id),
                |_| Ok(()),
            )
            .is_ok();
        if duplicate {
            return Err(bad_request("同名模型配置已存在"));
        }
        row.name = trimmed.to_string();
    }
    if let Some(protocol) = body.protocol {
        row.protocol = protocol;
    }
    if let Some(base_url) = body.base_url {
        row.base_url = base_url.trim().trim_end_matches('/').to_string();
    }
    if let Some(model) = body.model {
        row.model = model;
    }
    if let Some(temperature) = body.temperature {
        row.temperature = temperature;
    }
    if let Some(max_tokens) = body.max_tokens {
        row.max_tokens = max_tokens;
    }
    if let Some(effort) = body.reasoning_effort {
        row.reasoning_effort = reasoning::normalize_effort(&effort).to_string();
    }
    if let Some(timeout_sec) = body.timeout_sec {
        row.timeout_sec = timeout_sec;
    }
    if let Some(json_mode) = body.json_mode {
        row.json_mode = json_mode;
    }
    if let Some(enabled) = body.enabled {
        row.enabled = enabled;
    }
    if let Some(priority) = body.priority {
        row.priority = priority;
    }
    if let Some(role) = body.role {
        row.role = role;
    }
    if let Some(extra) = body.extra {
        row.extra = extra;
    }
    if let Some(api_key) = body.api_key.filter(|key| !key.is_empty()) {
        let key = state.key_text().map_err(bad_request)?;
        row.api_key_enc = secret::encrypt_secret(&key, &api_key).map_err(bad_request)?;
    }
    let now = crate::now();
    conn.execute(
        "UPDATE ai_providers SET name=?1, protocol=?2, base_url=?3, api_key_enc=?4, model=?5, \
         temperature=?6, max_tokens=?7, reasoning_effort=?8, timeout_sec=?9, json_mode=?10, \
         enabled=?11, priority=?12, role=?13, extra=?14, updated_at=?15 WHERE id=?16",
        (
            &row.name,
            &row.protocol,
            &row.base_url,
            &row.api_key_enc,
            &row.model,
            row.temperature,
            row.max_tokens,
            &row.reasoning_effort,
            row.timeout_sec,
            i64::from(row.json_mode),
            i64::from(row.enabled),
            row.priority,
            &row.role,
            serde_json::to_string(&row.extra).unwrap_or_else(|_| "{}".to_string()),
            now,
            id,
        ),
    )
    .map_err(|error| bad_request(format!("更新模型配置失败: {error}")))?;
    tracing::info!(provider_id = id, "更新 AI 模型配置");
    Ok(json!({ "ok": true }).into())
}

async fn delete_provider(
    State(state): State<AiState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.db.lock();
    if load_provider(&conn, id).is_none() {
        return Err(not_found());
    }
    conn.execute("DELETE FROM ai_providers WHERE id = ?1", [id])
        .map_err(|error| bad_request(format!("删除模型配置失败: {error}")))?;
    let cfg = ai_client::read_ai_config_conn(&conn);
    if cfg.provider_id == id {
        let fallbacks: Vec<i64> = cfg
            .fallback_provider_ids
            .iter()
            .copied()
            .filter(|item| *item != id)
            .collect();
        update_ai_group(
            &conn,
            json!({"provider_id": 0, "fallback_provider_ids": fallbacks}),
        );
    }
    tracing::warn!(provider_id = id, "删除 AI 模型配置");
    Ok(json!({ "ok": true }).into())
}

#[derive(Deserialize)]
struct FetchModelsIn {
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    api_key: String,
    #[serde(default = "default_protocol")]
    protocol: String,
    #[serde(default)]
    provider_id: i64,
}

async fn fetch_models(
    State(state): State<AiState>,
    Json(body): Json<FetchModelsIn>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let mut base = body.base_url.trim().to_string();
    let mut key = body.api_key.trim().to_string();
    let mut protocol = body.protocol;
    if body.provider_id > 0 {
        let conn = state.db.lock();
        let Some(row) = load_provider(&conn, body.provider_id) else {
            return Err(not_found());
        };
        if base.is_empty() {
            base = row.base_url.clone();
        }
        protocol = row.protocol.clone();
        if key.is_empty() {
            key = decrypt_key(&state.master_key, &row.api_key_enc);
        }
    }
    if base.is_empty() {
        return Err(bad_request("需要填写 Base URL"));
    }
    let result =
        ai_client::fetch_remote_models(state.transport.as_ref(), &base, &key, &protocol, 12).await;
    Ok(result.into())
}

#[derive(Deserialize)]
struct TestProviderIn {
    #[serde(default = "default_test_prompt")]
    prompt: String,
}

fn default_test_prompt() -> String {
    "只回答两个字: 正常".to_string()
}

async fn test_provider(
    State(state): State<AiState>,
    Path(id): Path<i64>,
    Json(body): Json<TestProviderIn>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let provider = {
        let conn = state.db.lock();
        let Some(row) = load_provider(&conn, id) else {
            return Err(not_found());
        };
        provider_config(&row, &state.master_key)
    };
    let messages = vec![ChatMessage::new("user", &body.prompt)];
    let mut call = ChatCall::new(&messages, "test");
    call.json_mode = Some(false);
    call.retries = 0;
    let outcome = ai_client::chat(state.transport.as_ref(), &state.db, &provider, call).await;
    let mut payload = json!({
        "ok": outcome.ok,
        "text": outcome.text.chars().take(2000).collect::<String>(),
        "error": outcome.error,
        "latency_ms": outcome.latency_ms,
        "model": outcome.model,
        "prompt_tokens": outcome.prompt_tokens,
        "completion_tokens": outcome.completion_tokens,
        "cost_usd": outcome.cost_usd,
        "attempts": outcome.attempts,
    });
    payload["attempts"] = json!(payload["attempts"]);
    Ok(payload.into())
}

#[derive(Deserialize)]
struct DiagnoseQuery {
    #[serde(default = "default_true")]
    deep: bool,
}

async fn diagnose_provider(
    State(state): State<AiState>,
    Path(id): Path<i64>,
    Query(query): Query<DiagnoseQuery>,
) -> Json<Value> {
    let row = {
        let conn = state.db.lock();
        load_provider(&conn, id)
    };
    let Some(row) = row else {
        return json!({ "ok": false, "error": "模型配置不存在" }).into();
    };
    let provider = provider_config(&row, &state.master_key);
    let mut checks: Vec<Value> = Vec::new();
    checks.push(json!({
        "name": "凭据",
        "ok": !provider.api_key.is_empty(),
        "detail": if provider.api_key.is_empty() { "缺少 API Key" } else { "已配置 API Key" },
        "latency_ms": 0,
        "hint": "在「AI 模型」页填写 key",
    }));
    match ai_client::join_url(&provider.base_url, &provider.protocol) {
        Ok(url) => checks.push(
            json!({"name": "接口地址", "ok": true, "detail": url, "latency_ms": 0, "hint": ""}),
        ),
        Err(error) => {
            checks.push(json!({"name": "接口地址", "ok": false, "detail": error, "latency_ms": 0, "hint": "base_url 需要形如 https://api.deepseek.com/v1"}));
            return json!({"ok": false, "provider": provider.name, "checks": checks}).into();
        }
    }
    checks.push(json!({
        "name": "模型",
        "ok": !provider.model.is_empty(),
        "detail": if provider.model.is_empty() { "未填写模型名" } else { provider.model.as_str() },
        "latency_ms": 0,
        "hint": "",
    }));

    if query.deep {
        let messages = vec![ChatMessage::new(
            "user",
            "请直接回答两个字：正常。不要展开分析。",
        )];
        let mut call = ChatCall::new(&messages, "diagnose");
        call.max_tokens = Some(4096);
        call.temperature = Some(0.0);
        call.json_mode = Some(false);
        call.retries = 0;
        let started = std::time::Instant::now();
        let outcome = ai_client::chat(state.transport.as_ref(), &state.db, &provider, call).await;
        let latency = started.elapsed().as_millis() as i64;
        checks.push(json!({
            "name": "基础对话",
            "ok": outcome.ok,
            "detail": format!("响应 {latency}ms, 返回: {}", truncate_text(if outcome.ok { &outcome.text } else { &outcome.error }, 80)),
            "latency_ms": latency,
            "hint": if outcome.ok { "" } else { "检查网络/代理、模型名与余额" },
        }));
        if outcome.ok {
            let json_messages = vec![ChatMessage::new(
                "user",
                "只输出这个 JSON，不要任何其他文字：{\"ok\":true}",
            )];
            let mut json_call = ChatCall::new(&json_messages, "diagnose");
            json_call.max_tokens = Some(4096);
            json_call.temperature = Some(0.0);
            json_call.json_mode = Some(true);
            json_call.retries = 0;
            let json_started = std::time::Instant::now();
            let json_outcome =
                ai_client::chat(state.transport.as_ref(), &state.db, &provider, json_call).await;
            let json_latency = json_started.elapsed().as_millis() as i64;
            let structured_ok =
                json_outcome.ok && ai_client::extract_json(&json_outcome.text).is_ok();
            checks.push(json!({
                "name": "JSON 结构化输出",
                "ok": structured_ok,
                "detail": if structured_ok { "支持 response_format=json_object".to_string() } else { truncate_text(if json_outcome.error.is_empty() { "返回内容不是合法 JSON" } else { &json_outcome.error }, 160) },
                "latency_ms": json_latency,
                "hint": if structured_ok { "" } else { "系统会自动降级为纯文本 + 容错解析" },
            }));
        }
        let models = ai_client::fetch_remote_models(
            state.transport.as_ref(),
            &provider.base_url,
            &provider.api_key,
            &provider.protocol,
            12,
        )
        .await;
        let models_ok = models["ok"].as_bool().unwrap_or(false);
        let total = models["total"].as_i64().unwrap_or(0);
        checks.push(json!({
            "name": "/models 列表",
            "ok": models_ok,
            "detail": if models_ok { format!("发现 {total} 个模型") } else { truncate_text(models["error"].as_str().unwrap_or("未取到模型列表"), 160) },
            "latency_ms": 0,
            "hint": if models_ok { "" } else { "部分服务商不提供该接口, 不影响调用" },
        }));
        checks.push(json!({
            "name": "用量估算",
            "ok": true,
            "detail": format!("单次决策约 {} prompt tokens (按 4.2K 字符上下文估算)", ai_client::estimate_tokens(&"x".repeat(4200))),
            "latency_ms": 0,
            "hint": "",
        }));
    }

    let required = ["凭据", "接口地址", "模型", "基础对话"];
    let overall = checks.iter().all(|check| {
        let name = check["name"].as_str().unwrap_or("");
        !required.contains(&name) || check["ok"].as_bool().unwrap_or(false)
    });
    json!({
        "ok": overall,
        "provider": provider.name,
        "model": provider.model,
        "protocol": provider.protocol,
        "checks": checks,
        "checked_at": crate::now(),
    })
    .into()
}

async fn provider_models(
    State(state): State<AiState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let provider = {
        let conn = state.db.lock();
        let Some(row) = load_provider(&conn, id) else {
            return Err(not_found());
        };
        provider_config(&row, &state.master_key)
    };
    let result = ai_client::fetch_remote_models(
        state.transport.as_ref(),
        &provider.base_url,
        &provider.api_key,
        if provider.protocol.is_empty() {
            "openai_chat"
        } else {
            &provider.protocol
        },
        12,
    )
    .await;
    Ok(json!({
        "ok": result["ok"],
        "models": result["models"],
        "endpoint": result["endpoint"],
        "tried": result["tried"],
        "error": result["error"],
    })
    .into())
}

fn truncate_text(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

fn update_ai_group(conn: &rusqlite::Connection, patch: Value) {
    let mut current = conn
        .query_row(
            "SELECT value FROM app_settings WHERE key = 'ai'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    if let Some(items) = patch.as_object() {
        for (key, value) in items {
            current.insert(key.clone(), value.clone());
        }
    }
    let payload =
        serde_json::to_string(&Value::Object(current)).unwrap_or_else(|_| "{}".to_string());
    conn.execute(
        "INSERT INTO app_settings(key, value, updated_at) VALUES('ai', ?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        (&payload, crate::now()),
    )
    .ok();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai_client::HttpReply;
    use axum::body::Body;
    use axum::http::Request;
    use std::sync::Mutex;
    use tower::ServiceExt;

    struct FakeTransport {
        posts: Mutex<Vec<HttpReply>>,
        gets: Mutex<Vec<HttpReply>>,
    }

    impl FakeTransport {
        fn http(posts: Vec<HttpReply>, gets: Vec<HttpReply>) -> Self {
            Self {
                posts: Mutex::new(posts),
                gets: Mutex::new(gets),
            }
        }
    }

    impl AiTransport for FakeTransport {
        fn post_json<'a>(
            &'a self,
            _url: &'a str,
            _headers: &'a [(String, String)],
            _body: &'a Value,
            _timeout_secs: u64,
        ) -> ai_client::BoxFuture<'a, Result<HttpReply, String>> {
            let reply = self
                .posts
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
        ) -> ai_client::BoxFuture<'a, Result<HttpReply, String>> {
            let reply = self
                .gets
                .lock()
                .unwrap()
                .drain(..1)
                .next()
                .unwrap_or(HttpReply {
                    status: 404,
                    body: "not found".to_string(),
                });
            Box::pin(async move { Ok(reply) })
        }
    }

    const MASTER_KEY: &str = "MDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDA=";

    fn fixture(path: &std::path::Path) -> Db {
        let _ = std::fs::remove_file(path);
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE app_settings(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER); \
             CREATE TABLE engine_state(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER); \
             CREATE TABLE ai_call_logs(id INTEGER PRIMARY KEY, ts INTEGER, provider TEXT, model TEXT, purpose TEXT, \
             ok INTEGER, latency_ms INTEGER, prompt_tokens INTEGER, completion_tokens INTEGER, cost_usd REAL, error TEXT, raw_text TEXT); \
             CREATE TABLE ai_providers(id INTEGER PRIMARY KEY, name TEXT, protocol TEXT, base_url TEXT, \
             api_key_enc TEXT, model TEXT, temperature REAL, max_tokens INTEGER, reasoning_effort TEXT, \
             timeout_sec INTEGER, json_mode INTEGER, enabled INTEGER, priority INTEGER, role TEXT, \
             extra TEXT, created_at INTEGER, updated_at INTEGER); \
             INSERT INTO app_settings VALUES('ai','{\"enabled\":true,\"mode\":\"single\",\"provider_id\":0,\"fallback_provider_ids\":[],\"max_calls_per_hour\":30,\"monthly_budget_usd\":3}',0);",
        )
        .unwrap();
        drop(conn);
        Db::open(path.to_str().unwrap()).unwrap()
    }

    fn app(db: Db, transport: Arc<dyn AiTransport>) -> Router {
        Router::new()
            .nest("/api/v1", routes())
            .with_state(AiState::with_transport(
                db,
                MASTER_KEY.as_bytes().to_vec(),
                transport,
            ))
    }

    async fn body(response: axum::response::Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn create_list_and_update_never_return_plaintext_key() {
        let path = std::env::temp_dir().join("nq-ai-providers-crud.db");
        let db = fixture(&path);
        let transport: Arc<dyn AiTransport> = Arc::new(FakeTransport::http(vec![], vec![]));
        let router = app(db.clone(), transport);
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/ai/providers")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"name":"alpha","base_url":"https://api.example.com/v1/","api_key":"sk-1234567890abcdef","model":"deepseek-v4","reasoning_effort":"HIGH"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let created = body(response).await;
        let id = created["id"].as_i64().unwrap();

        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/ai/providers")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let listed = body(response).await;
        let item = &listed["items"][0];
        assert_eq!(item["id"], id);
        assert_eq!(
            item["base_url"], "https://api.example.com/v1",
            "尾部斜杠应去掉"
        );
        assert_eq!(item["reasoning_effort"], "high", "思考强度应归一化");
        assert_eq!(item["has_key"], true);
        assert_eq!(item["api_key_masked"], "sk-1******cdef");
        assert!(
            !listed.to_string().contains("sk-1234567890abcdef"),
            "列表不得出现明文 Key"
        );
        assert_eq!(
            listed["active"]["provider_id"], id,
            "首个模型自动成为当前供应商"
        );

        let conn = db.lock();
        let cipher: String = conn
            .query_row(
                "SELECT api_key_enc FROM ai_providers WHERE id = ?1",
                [id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            cipher.starts_with("gAAAA"),
            "落库必须是 Fernet 密文: {cipher}"
        );
        drop(conn);

        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri(format!("/api/v1/ai/providers/{id}"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"enabled":false,"model":"deepseek-r1"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let listed = body(
            router
                .oneshot(
                    Request::builder()
                        .uri("/api/v1/ai/providers")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(listed["items"][0]["enabled"], false);
        assert_eq!(listed["items"][0]["model"], "deepseek-r1");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn duplicate_name_and_unknown_provider_are_rejected() {
        let path = std::env::temp_dir().join("nq-ai-providers-dup.db");
        let db = fixture(&path);
        let transport: Arc<dyn AiTransport> = Arc::new(FakeTransport::http(vec![], vec![]));
        let router = app(db.clone(), transport);
        let create = r#"{"name":"alpha","base_url":"https://api.example.com/v1"}"#;
        let first = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/ai/providers")
                    .header("content-type", "application/json")
                    .body(Body::from(create))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let second = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/ai/providers")
                    .header("content-type", "application/json")
                    .body(Body::from(create))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::BAD_REQUEST);
        let missing = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/v1/ai/providers/999")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        let diagnose = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/ai/providers/999/diagnose")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            diagnose.status(),
            StatusCode::OK,
            "诊断对不存在配置返回明确结论"
        );
        assert_eq!(body(diagnose).await["ok"], false);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn test_and_models_call_the_selected_provider() {
        let path = std::env::temp_dir().join("nq-ai-providers-test.db");
        let db = fixture(&path);
        let transport: Arc<dyn AiTransport> = Arc::new(FakeTransport::http(
            vec![HttpReply {
                status: 200,
                body: "{\"choices\":[{\"message\":{\"content\":\"正常\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":2}}".to_string(),
            }],
            vec![HttpReply {
                status: 200,
                body: "{\"data\":[{\"id\":\"m1\"}]}".to_string(),
            }],
        ));
        let router = app(db.clone(), transport);
        let created = body(
            router
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/v1/ai/providers")
                        .header("content-type", "application/json")
                        .body(Body::from(
                            r#"{"name":"alpha","base_url":"https://api.example.com/v1","api_key":"k","model":"m1","json_mode":false}"#,
                        ))
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await;
        let id = created["id"].as_i64().unwrap();

        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/v1/ai/providers/{id}/test"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"prompt":"只回答两个字: 正常"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let tested = body(response).await;
        assert_eq!(tested["ok"], true, "{}", tested);
        assert_eq!(tested["text"], "正常");
        assert!(tested["latency_ms"].as_i64().unwrap() >= 0);

        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/ai/providers/{id}/models"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let models = body(response).await;
        assert_eq!(models["ok"], true);
        assert_eq!(models["models"][0]["id"], "m1");
        assert_eq!(models["endpoint"], "https://api.example.com/v1/models");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn fetch_models_accepts_unsaved_endpoint_and_rejects_missing_base_url() {
        let path = std::env::temp_dir().join("nq-ai-providers-fetch.db");
        let db = fixture(&path);
        let transport: Arc<dyn AiTransport> = Arc::new(FakeTransport::http(
            vec![],
            vec![HttpReply {
                status: 200,
                body: "{\"data\":[\"a\",\"b\"]}".to_string(),
            }],
        ));
        let router = app(db.clone(), transport);
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/ai/providers/fetch-models")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"base_url":"https://api.example.com/v1","api_key":"k"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let result = body(response).await;
        assert_eq!(result["ok"], true);
        assert_eq!(result["total"], 2);

        let missing = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/ai/providers/fetch-models")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::BAD_REQUEST);
        let _ = std::fs::remove_file(&path);
    }

    fn chain_provider(id: i64, name: &str) -> ProviderConfig {
        ProviderConfig {
            id,
            name: name.to_string(),
            protocol: "openai_chat".to_string(),
            base_url: "https://api.example.com/v1".to_string(),
            api_key: "k".to_string(),
            model: "m".to_string(),
            temperature: 0.2,
            max_tokens: 1024,
            reasoning_effort: "auto".to_string(),
            timeout_sec: 5,
            json_mode: false,
        }
    }

    #[test]
    fn order_chain_by_latency_puts_fastest_first() {
        let chain = vec![
            chain_provider(1, "slow"),
            chain_provider(2, "fast"),
            chain_provider(3, "unknown"),
            chain_provider(4, "tie"),
        ];
        let p95 = HashMap::from([
            ("slow".to_string(), 900.0),
            ("fast".to_string(), 200.0),
            ("tie".to_string(), 200.0),
        ]);
        let ordered = order_chain_by_latency(&chain, &p95, 5);
        let names: Vec<&str> = ordered.iter().map(|item| item.name.as_str()).collect();
        assert_eq!(names, vec!["fast", "tie", "slow", "unknown"]);
        assert_eq!(ordered[3].id, 3, "无数据供应商应保持原顺序排在最后");
    }

    #[test]
    fn order_chain_keeps_config_order_when_samples_insufficient() {
        let chain = vec![
            chain_provider(1, "alpha"),
            chain_provider(2, "beta"),
            chain_provider(3, "gamma"),
        ];
        let ordered = order_chain_by_latency(&chain, &HashMap::new(), 5);
        let ids: Vec<i64> = ordered.iter().map(|item| item.id).collect();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    #[test]
    fn provider_p95_latency_skips_thin_and_failed_samples() {
        let path = std::env::temp_dir().join("nq-ai-p95.db");
        let db = fixture(&path);
        let now = crate::now();
        let conn = db.lock();
        for latency in 100..108 {
            conn.execute(
                "INSERT INTO ai_call_logs(ts, provider, model, purpose, ok, latency_ms) \
                 VALUES(?1, 'fast', 'm', 'decision', 1, ?2)",
                (now, latency),
            )
            .unwrap();
        }
        for latency in [200, 210, 220] {
            conn.execute(
                "INSERT INTO ai_call_logs(ts, provider, model, purpose, ok, latency_ms) \
                 VALUES(?1, 'thin', 'm', 'decision', 1, ?2)",
                (now, latency),
            )
            .unwrap();
        }
        for _ in 0..8 {
            conn.execute(
                "INSERT INTO ai_call_logs(ts, provider, model, purpose, ok, latency_ms) \
                 VALUES(?1, 'failed', 'm', 'decision', 0, 500)",
                [now],
            )
            .unwrap();
        }
        let p95 = provider_p95_latency(&conn, now - 86400, 5);
        assert!(p95.contains_key("fast"));
        assert!(!p95.contains_key("thin"), "样本不足不应参与排序");
        assert!(!p95.contains_key("failed"), "失败调用不计入延迟");
        drop(conn);
        let _ = std::fs::remove_file(&path);
    }
}
