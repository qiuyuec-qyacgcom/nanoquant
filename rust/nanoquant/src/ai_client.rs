//! AI 调用层：协议适配、思考强度、预算、截断归因与调用审计。
//!
//! 对齐 legacy 内核（已退役） `ai/client.py`、`ai/decision.py`、`ai/compact.py` 的契约：
//! 请求体按 `openai_chat` / `openai_responses` / `anthropic_messages` 三种协议构造；
//! 网关不认识 `response_format` 或思考强度参数时，第一次 400/422 后去掉该参数重试一次，
//! 不让整次调用失败。决策协议默认 v2（英文短键与枚举码），解析时自动兼容 v1，
//! 中文 reason / risk_flags / invalidation 由本地码表生成后落库。

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::db::Db;
use crate::reasoning;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, Clone)]
pub struct HttpReply {
    pub status: u16,
    pub body: String,
}

/// HTTP 传输的唯一注入点。测试用确定性实现替换，禁止真实网络。
pub trait AiTransport: Send + Sync {
    fn post_json<'a>(
        &'a self,
        url: &'a str,
        headers: &'a [(String, String)],
        body: &'a Value,
        timeout_secs: u64,
    ) -> BoxFuture<'a, Result<HttpReply, String>>;

    fn get_json<'a>(
        &'a self,
        url: &'a str,
        headers: &'a [(String, String)],
        timeout_secs: u64,
    ) -> BoxFuture<'a, Result<HttpReply, String>>;
}

pub struct HttpTransport {
    client: reqwest::Client,
}

impl HttpTransport {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(12))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }
}

impl Default for HttpTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl AiTransport for HttpTransport {
    fn post_json<'a>(
        &'a self,
        url: &'a str,
        headers: &'a [(String, String)],
        body: &'a Value,
        timeout_secs: u64,
    ) -> BoxFuture<'a, Result<HttpReply, String>> {
        Box::pin(async move {
            let mut request = self
                .client
                .post(url)
                .timeout(Duration::from_secs(timeout_secs.max(1)))
                .json(body);
            for (name, value) in headers {
                request = request.header(name.as_str(), value.as_str());
            }
            let response = request
                .send()
                .await
                .map_err(|error| format!("网络错误: {error}"))?;
            let status = response.status().as_u16();
            let body = response
                .text()
                .await
                .map_err(|error| format!("读取响应失败: {error}"))?;
            Ok(HttpReply { status, body })
        })
    }

    fn get_json<'a>(
        &'a self,
        url: &'a str,
        headers: &'a [(String, String)],
        timeout_secs: u64,
    ) -> BoxFuture<'a, Result<HttpReply, String>> {
        Box::pin(async move {
            let mut request = self
                .client
                .get(url)
                .timeout(Duration::from_secs(timeout_secs.max(1)));
            for (name, value) in headers {
                request = request.header(name.as_str(), value.as_str());
            }
            let response = request
                .send()
                .await
                .map_err(|error| format!("网络错误: {error}"))?;
            let status = response.status().as_u16();
            let body = response
                .text()
                .await
                .map_err(|error| format!("读取响应失败: {error}"))?;
            Ok(HttpReply { status, body })
        })
    }
}

#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub id: i64,
    pub name: String,
    pub protocol: String,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub temperature: f64,
    pub max_tokens: i64,
    pub reasoning_effort: String,
    pub timeout_sec: i64,
    pub json_mode: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

impl ChatMessage {
    pub fn new(role: &str, content: &str) -> Self {
        Self {
            role: role.to_string(),
            content: content.to_string(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ChatOutcome {
    pub ok: bool,
    pub text: String,
    pub provider: String,
    pub model: String,
    pub latency_ms: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cost_usd: f64,
    pub error: String,
    pub raw_text: String,
    pub finish_reason: String,
    pub attempts: Vec<Value>,
    pub log_id: i64,
}

#[derive(Clone, Copy)]
pub struct ChatCall<'a> {
    pub messages: &'a [ChatMessage],
    pub purpose: &'a str,
    pub json_mode: Option<bool>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<i64>,
    pub reasoning_effort: Option<&'a str>,
    pub retries: usize,
}

impl<'a> ChatCall<'a> {
    pub fn new(messages: &'a [ChatMessage], purpose: &'a str) -> Self {
        Self {
            messages,
            purpose,
            json_mode: None,
            temperature: None,
            max_tokens: None,
            reasoning_effort: None,
            retries: 2,
        }
    }
}

/// 拼接调用地址。Anthropic 固定 `/v1/messages`；OpenAI 兼容按是否已带版本段补 `/v1`。
pub fn join_url(base: &str, protocol: &str) -> Result<String, String> {
    let base = base.trim().trim_end_matches('/');
    if base.is_empty() {
        return Err("base_url 为空".to_string());
    }
    if protocol == "anthropic_messages" {
        return Ok(if base.ends_with("/v1/messages") {
            base.to_string()
        } else {
            format!("{base}/v1/messages")
        });
    }
    if base.ends_with("/chat/completions") {
        return Ok(base.to_string());
    }
    if protocol == "openai_responses" {
        return Ok(if base.ends_with("/responses") {
            base.to_string()
        } else {
            format!("{base}/responses")
        });
    }
    let tail = base.rsplit('/').next().unwrap_or("");
    if let Some(version) = tail.strip_prefix('v') {
        if !version.is_empty() && version.chars().all(|c| c.is_ascii_digit()) {
            return Ok(format!("{base}/chat/completions"));
        }
    }
    if base.matches('/').count() > 2 {
        return Ok(format!("{base}/chat/completions"));
    }
    Ok(format!("{base}/v1/chat/completions"))
}

fn build_headers(provider: &ProviderConfig) -> Vec<(String, String)> {
    let mut headers = vec![("Content-Type".to_string(), "application/json".to_string())];
    if provider.protocol == "anthropic_messages" {
        headers.push(("x-api-key".to_string(), provider.api_key.clone()));
        headers.push(("anthropic-version".to_string(), "2023-06-01".to_string()));
    } else {
        headers.push((
            "Authorization".to_string(),
            format!("Bearer {}", provider.api_key),
        ));
    }
    headers
}

fn build_payload(
    provider: &ProviderConfig,
    messages: &[ChatMessage],
    json_mode: bool,
    temperature: f64,
    max_tokens: i64,
) -> Value {
    if provider.protocol == "anthropic_messages" {
        let system = messages
            .iter()
            .filter(|m| m.role == "system")
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        let chat: Vec<Value> = messages
            .iter()
            .filter(|m| m.role != "system")
            .map(|m| json!({"role": m.role, "content": m.content}))
            .collect();
        return json!({
            "model": provider.model,
            "max_tokens": max_tokens,
            "temperature": temperature,
            "system": system,
            "messages": chat,
        });
    }
    if provider.protocol == "openai_responses" {
        let input = messages
            .iter()
            .map(|m| format!("[{}] {}", m.role, m.content))
            .collect::<Vec<_>>()
            .join("\n\n");
        return json!({
            "model": provider.model,
            "input": input,
            "temperature": temperature,
        });
    }
    let mut payload = json!({
        "model": provider.model,
        "messages": messages
            .iter()
            .map(|m| json!({"role": m.role, "content": m.content}))
            .collect::<Vec<_>>(),
        "temperature": temperature,
        "max_tokens": max_tokens,
        "stream": false,
    });
    if json_mode {
        payload["response_format"] = json!({"type": "json_object"});
    }
    payload
}

fn extract_text(data: &Value, protocol: &str) -> (String, i64, i64) {
    if protocol == "anthropic_messages" {
        let text = data
            .get("content")
            .and_then(Value::as_array)
            .map(|blocks| {
                blocks
                    .iter()
                    .filter_map(|block| block.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default();
        let usage = data.get("usage").unwrap_or(&Value::Null);
        return (
            text,
            usage
                .get("input_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(0),
            usage
                .get("output_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(0),
        );
    }
    if protocol == "openai_responses" {
        let mut text = data
            .get("output_text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if text.is_empty() {
            if let Some(items) = data.get("output").and_then(Value::as_array) {
                for item in items {
                    if let Some(parts) = item.get("content").and_then(Value::as_array) {
                        for part in parts {
                            if let Some(chunk) = part.get("text").and_then(Value::as_str) {
                                text.push_str(chunk);
                            }
                        }
                    }
                }
            }
        }
        let usage = data.get("usage").unwrap_or(&Value::Null);
        return (
            text,
            usage
                .get("input_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(0),
            usage
                .get("output_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(0),
        );
    }
    let first = data
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first());
    let text = first
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let usage = data.get("usage").unwrap_or(&Value::Null);
    (
        text,
        usage
            .get("prompt_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(0),
        usage
            .get("completion_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(0),
    )
}

fn finish_reason(data: &Value) -> String {
    data.get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("finish_reason"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// 把「JSON 解析不出来」的锅提前归因到截断或只有思考过程，禁止笼统报解析失败。
pub fn output_problem(text: &str, finish: &str, max_tokens: i64) -> String {
    if finish == "length" {
        if max_tokens < 512 {
            return format!(
                "输出被 max_tokens={max_tokens} 截断 (finish_reason=length)；该探针预算偏小，推理模型光思考就会耗尽——请升级到最新版（诊断探针已放宽到 4096），或改用非推理模型/关闭思考"
            );
        }
        return format!(
            "输出被 max_tokens={max_tokens} 截断 (finish_reason=length)；请把供应商 max_tokens 提到 3000+，或改用非推理模型/关闭思考"
        );
    }
    if text.trim().is_empty() {
        return "模型只返回 thinking/reasoning，未给出决策正文；请关闭思考或换非推理模型"
            .to_string();
    }
    String::new()
}

pub fn estimate_tokens(text: &str) -> i64 {
    if text.is_empty() {
        return 0;
    }
    (text.chars().count() as f64 / 2.6).max(1.0) as i64
}

const PRICE_TABLE: [(&str, f64, f64); 13] = [
    ("gpt-4o", 0.0025, 0.01),
    ("gpt-4o-mini", 0.00015, 0.0006),
    ("gpt-4.1", 0.002, 0.008),
    ("gpt-4.1-mini", 0.0004, 0.0016),
    ("o3-mini", 0.0011, 0.0044),
    ("deepseek-chat", 0.00027, 0.0011),
    ("deepseek-reasoner", 0.00055, 0.0022),
    ("qwen-plus", 0.0004, 0.0012),
    ("qwen-max", 0.0016, 0.0064),
    ("glm-4-flash", 0.0, 0.0),
    ("glm-4-plus", 0.0007, 0.0007),
    ("moonshot-v1-8k", 0.0017, 0.0017),
    ("claude-3-5-sonnet", 0.003, 0.015),
];

pub fn estimate_cost(model: &str, prompt_tokens: i64, completion_tokens: i64) -> f64 {
    let lowered = model.to_lowercase();
    let Some((_, input, output)) = PRICE_TABLE
        .iter()
        .find(|(key, _, _)| lowered.starts_with(key))
    else {
        return 0.0;
    };
    let cost = prompt_tokens as f64 / 1000.0 * input + completion_tokens as f64 / 1000.0 * output;
    (cost * 100_000_000.0).round() / 100_000_000.0
}

fn log_call(
    db: &Db,
    provider: &ProviderConfig,
    purpose: &str,
    ok: bool,
    latency_ms: i64,
    prompt_tokens: i64,
    completion_tokens: i64,
    cost_usd: f64,
    error: &str,
    raw_text: &str,
) -> i64 {
    let Ok(conn) = db.conn.lock() else {
        return 0;
    };
    if conn
        .execute(
            "INSERT INTO ai_call_logs(ts, provider, model, purpose, ok, latency_ms, prompt_tokens, \
         completion_tokens, cost_usd, error, raw_text) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            (
                crate::now(),
                &provider.name,
                &provider.model,
                purpose,
                i64::from(ok),
                latency_ms,
                prompt_tokens,
                completion_tokens,
                cost_usd,
                truncate(error, 480),
                truncate(raw_text, 2000),
            ),
        )
        .is_err()
    {
        return 0;
    }
    conn.last_insert_rowid()
}

fn truncate(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

/// 请求预算按 provider 上限取大：显式预算只作为下限，避免推理模型被小预算截断。
fn effective_max_tokens(call_max: Option<i64>, provider_max: i64) -> i64 {
    call_max.map_or(provider_max, |v| v.max(provider_max)).max(1)
}

/// 一次真实调用。成功与失败都写 `ai_call_logs`；失败不伪造空成功。
pub async fn chat(
    transport: &dyn AiTransport,
    db: &Db,
    provider: &ProviderConfig,
    call: ChatCall<'_>,
) -> ChatOutcome {
    let mut outcome = ChatOutcome {
        provider: provider.name.clone(),
        model: provider.model.clone(),
        ..Default::default()
    };
    let url = match join_url(&provider.base_url, &provider.protocol) {
        Ok(url) => url,
        Err(error) => {
            outcome.error = error;
            return outcome;
        }
    };
    let json_mode = call.json_mode.unwrap_or(provider.json_mode);
    let temperature = call.temperature.unwrap_or(provider.temperature);
    let max_tokens = effective_max_tokens(call.max_tokens, provider.max_tokens);
    let mut payload = build_payload(provider, call.messages, json_mode, temperature, max_tokens);
    let effort = call
        .reasoning_effort
        .unwrap_or(&provider.reasoning_effort)
        .to_string();
    let mut applied = reasoning::apply_effort(
        &mut payload,
        &provider.protocol,
        &provider.model,
        &effort,
        max_tokens,
    );
    let headers = build_headers(provider);
    let timeout = u64::try_from(provider.timeout_sec.max(1)).unwrap_or(180);
    let mut attempts: Vec<Value> = Vec::new();
    let mut last_error = String::new();

    for attempt in 0..=call.retries {
        let started = Instant::now();
        let reply = transport.post_json(&url, &headers, &payload, timeout).await;
        let latency = started.elapsed().as_millis() as i64;
        let reply = match reply {
            Ok(reply) => reply,
            Err(error) => {
                last_error = error.clone();
                attempts.push(
                    json!({"status": -1, "note": truncate(&error, 200), "latency_ms": latency}),
                );
                if attempt < call.retries {
                    sleep_backoff(attempt).await;
                }
                continue;
            }
        };

        if reply.status >= 400 {
            let body = truncate(&reply.body, 300);
            let mut dropped: Vec<String> = Vec::new();
            if matches!(reply.status, 400 | 422) {
                if payload.get("response_format").is_some() {
                    remove_key(&mut payload, "response_format");
                    dropped.push("response_format(json_mode)".to_string());
                }
                if !applied.is_empty() {
                    for key in applied.clone() {
                        remove_key(&mut payload, &key);
                    }
                    if applied.iter().any(|key| key == "max_tokens") {
                        payload["max_tokens"] = json!(max_tokens);
                    }
                    dropped.push(format!("reasoning_effort({})", applied.join(",")));
                    applied.clear();
                }
            }
            if !dropped.is_empty() && attempt == 0 {
                attempts.push(json!({
                    "status": reply.status,
                    "note": format!("服务端不接受 {}，已回退默认", dropped.join(" / ")),
                    "latency_ms": latency,
                }));
                continue;
            }
            attempts.push(json!({"status": reply.status, "note": body, "latency_ms": latency}));
            last_error = format!("HTTP {}: {body}", reply.status);
            if matches!(reply.status, 401 | 403) {
                break;
            }
            if attempt < call.retries {
                sleep_backoff(attempt).await;
            }
            continue;
        }

        let data: Value = match serde_json::from_str(&reply.body) {
            Ok(data) => data,
            Err(error) => {
                last_error = format!("返回不是 JSON: {error}");
                attempts.push(json!({"status": reply.status, "note": truncate(&last_error, 200), "latency_ms": latency}));
                if attempt < call.retries {
                    sleep_backoff(attempt).await;
                }
                continue;
            }
        };
        let (text, prompt_tokens, completion_tokens) = extract_text(&data, &provider.protocol);
        let prompt_tokens = if prompt_tokens > 0 {
            prompt_tokens
        } else {
            estimate_tokens(
                &call
                    .messages
                    .iter()
                    .map(|m| m.content.clone())
                    .collect::<Vec<_>>()
                    .join(" "),
            )
        };
        let completion_tokens = if completion_tokens > 0 {
            completion_tokens
        } else {
            estimate_tokens(&text)
        };
        let cost = estimate_cost(&provider.model, prompt_tokens, completion_tokens);
        bump_calls(db, crate::now());
        let finish = finish_reason(&data);
        let problem = output_problem(&text, &finish, max_tokens);
        let raw_text = truncate(&text, 2000);
        if problem.is_empty() {
            outcome.log_id = log_call(
                db,
                provider,
                call.purpose,
                true,
                latency,
                prompt_tokens,
                completion_tokens,
                cost,
                "",
                &raw_text,
            );
            attempts.push(json!({"status": reply.status, "latency_ms": latency}));
            outcome.ok = true;
            outcome.text = text;
            outcome.latency_ms = latency;
            outcome.prompt_tokens = prompt_tokens;
            outcome.completion_tokens = completion_tokens;
            outcome.cost_usd = cost;
            outcome.raw_text = raw_text;
            outcome.finish_reason = finish;
            outcome.attempts = attempts;
            return outcome;
        }
        outcome.log_id = log_call(
            db,
            provider,
            call.purpose,
            false,
            latency,
            prompt_tokens,
            completion_tokens,
            cost,
            &problem,
            &raw_text,
        );
        attempts.push(json!({"status": reply.status, "latency_ms": latency, "note": problem}));
        outcome.error = problem;
        outcome.latency_ms = latency;
        outcome.prompt_tokens = prompt_tokens;
        outcome.completion_tokens = completion_tokens;
        outcome.cost_usd = cost;
        outcome.raw_text = raw_text;
        outcome.finish_reason = finish;
        outcome.attempts = attempts;
        return outcome;
    }

    outcome.log_id = log_call(
        db,
        provider,
        call.purpose,
        false,
        attempts
            .last()
            .and_then(|item| item.get("latency_ms"))
            .and_then(Value::as_i64)
            .unwrap_or(0),
        0,
        0,
        0.0,
        &last_error,
        "",
    );
    outcome.error = truncate(&last_error, 400);
    outcome.attempts = attempts;
    outcome
}

/// 预算内按优先级链调用；任一供应商成功即返回，全部失败时把最后一次错误带出去。
pub async fn chat_with_fallback(
    transport: &dyn AiTransport,
    db: &Db,
    chain: &[ProviderConfig],
    call: ChatCall<'_>,
    cfg: &AiConfig,
) -> ChatOutcome {
    if let Ok(conn) = db.conn.lock() {
        if let Some(reason) = budget_blocked(&conn, cfg, crate::now()) {
            return ChatOutcome {
                error: reason,
                ..Default::default()
            };
        }
    }
    if chain.is_empty() {
        return ChatOutcome {
            error: "未配置可用的 AI 模型, 请先在「AI 模型」中接入并启用".to_string(),
            ..Default::default()
        };
    }
    let mut merged: Vec<Value> = Vec::new();
    let mut last = ChatOutcome::default();
    for provider in chain {
        let mut outcome = chat(transport, db, provider, call).await;
        merged.append(&mut outcome.attempts);
        outcome.attempts = merged.clone();
        if outcome.ok {
            return outcome;
        }
        last = outcome;
    }
    last.attempts = merged;
    last
}

fn remove_key(payload: &mut Value, key: &str) {
    if let Some(map) = payload.as_object_mut() {
        map.remove(key);
    }
}

async fn sleep_backoff(attempt: usize) {
    let millis = (800.0 * 2_f64.powi(attempt as i32)).min(4000.0) as u64;
    tokio::time::sleep(Duration::from_millis(millis)).await;
}

#[derive(Debug, Clone, PartialEq)]
pub struct AiPersona {
    pub name: String,
    pub weight: f64,
    pub focus: String,
    pub enabled: bool,
}

#[derive(Debug, Clone)]
pub struct AiConfig {
    pub enabled: bool,
    pub mode: String,
    pub provider_id: i64,
    pub fallback_provider_ids: Vec<i64>,
    pub min_confidence: f64,
    pub max_calls_per_hour: i64,
    pub monthly_budget_usd: f64,
    pub timeout_sec: i64,
    pub temperature: f64,
    pub decision_schema_version: i64,
    pub inject_news: bool,
    pub inject_mantras: bool,
    pub require_agreement: bool,
    pub allow_ai_override: bool,
    pub personas: Vec<AiPersona>,
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: "single".to_string(),
            provider_id: 0,
            fallback_provider_ids: Vec::new(),
            min_confidence: 0.45,
            max_calls_per_hour: 30,
            monthly_budget_usd: 3.0,
            timeout_sec: 45,
            temperature: 0.2,
            decision_schema_version: 2,
            inject_news: true,
            inject_mantras: true,
            require_agreement: true,
            allow_ai_override: false,
            personas: Vec::new(),
        }
    }
}

#[cfg(test)]
pub fn read_ai_config(db: &Db) -> AiConfig {
    match db.conn.lock() {
        Ok(conn) => read_ai_config_conn(&conn),
        Err(_) => AiConfig::default(),
    }
}

/// 已持有连接锁时使用，避免同线程重入 `Mutex` 死锁。
pub fn read_ai_config_conn(conn: &rusqlite::Connection) -> AiConfig {
    let value = conn
        .query_row(
            "SELECT value FROM app_settings WHERE key = 'ai'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .unwrap_or(Value::Null);
    let mut cfg = AiConfig::default();
    if let Some(item) = value.get("enabled").and_then(Value::as_bool) {
        cfg.enabled = item;
    }
    if let Some(item) = value.get("mode").and_then(Value::as_str) {
        cfg.mode = item.to_string();
    }
    if let Some(item) = value.get("provider_id").and_then(Value::as_i64) {
        cfg.provider_id = item;
    }
    if let Some(items) = value.get("fallback_provider_ids").and_then(Value::as_array) {
        cfg.fallback_provider_ids = items.iter().filter_map(Value::as_i64).collect();
    }
    if let Some(item) = value.get("min_confidence").and_then(Value::as_f64) {
        cfg.min_confidence = item;
    }
    if let Some(item) = value.get("max_calls_per_hour").and_then(Value::as_i64) {
        cfg.max_calls_per_hour = item;
    }
    if let Some(item) = value.get("monthly_budget_usd").and_then(Value::as_f64) {
        cfg.monthly_budget_usd = item;
    }
    if let Some(item) = value.get("timeout_sec").and_then(Value::as_i64) {
        cfg.timeout_sec = item;
    }
    if let Some(item) = value.get("temperature").and_then(Value::as_f64) {
        cfg.temperature = item;
    }
    if let Some(item) = value.get("decision_schema_version").and_then(Value::as_i64) {
        cfg.decision_schema_version = item;
    }
    if let Some(item) = value.get("inject_news").and_then(Value::as_bool) {
        cfg.inject_news = item;
    }
    if let Some(item) = value.get("inject_mantras").and_then(Value::as_bool) {
        cfg.inject_mantras = item;
    }
    if let Some(item) = value.get("require_agreement").and_then(Value::as_bool) {
        cfg.require_agreement = item;
    }
    if let Some(item) = value.get("allow_ai_override").and_then(Value::as_bool) {
        cfg.allow_ai_override = item;
    }
    if let Some(items) = value.get("personas").and_then(Value::as_array) {
        cfg.personas = items
            .iter()
            .filter_map(|item| {
                let name = item.get("name").and_then(Value::as_str)?;
                Some(AiPersona {
                    name: name.to_string(),
                    weight: item.get("weight").and_then(Value::as_f64).unwrap_or(0.0),
                    focus: item
                        .get("focus")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    enabled: item
                        .get("enabled")
                        .and_then(Value::as_bool)
                        .unwrap_or(true),
                })
            })
            .collect();
    }
    cfg
}

fn state_i64(conn: &rusqlite::Connection, key: &str) -> i64 {
    conn.query_row(
        "SELECT value FROM engine_state WHERE key = ?1",
        [key],
        |row| row.get::<_, String>(0),
    )
    .ok()
    .and_then(|raw| raw.parse::<i64>().ok())
    .unwrap_or(0)
}

fn set_state_i64(conn: &rusqlite::Connection, key: &str, value: i64) {
    conn.execute(
        "INSERT INTO engine_state(key, value, updated_at) VALUES(?1, ?2, ?3) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        (key, value.to_string(), crate::now()),
    )
    .ok();
}

/// 返回阻塞原因；`None` 表示允许调用。每小时计数按整点桶，月度支出按滚动 30 天。
pub fn budget_blocked(conn: &rusqlite::Connection, cfg: &AiConfig, now: i64) -> Option<String> {
    let hour = now / 3600;
    let bucket = state_i64(conn, "ai_calls_hour_bucket");
    let count = if bucket == hour {
        state_i64(conn, "ai_calls_hour_count")
    } else {
        0
    };
    if cfg.max_calls_per_hour > 0 && count >= cfg.max_calls_per_hour {
        return Some(format!("本小时 AI 调用已达上限 {}", cfg.max_calls_per_hour));
    }
    if cfg.monthly_budget_usd > 0.0 {
        let spent: f64 = conn
            .query_row(
                "SELECT COALESCE(SUM(cost_usd), 0) FROM ai_call_logs WHERE ts >= ?1",
                [now - 30 * 86400],
                |row| row.get(0),
            )
            .unwrap_or(0.0);
        if spent >= cfg.monthly_budget_usd {
            return Some(format!(
                "本月 AI 预算已用尽 ({spent:.4} / {:.4} USD)",
                cfg.monthly_budget_usd
            ));
        }
    }
    None
}

pub fn bump_calls(db: &Db, now: i64) {
    let Ok(conn) = db.conn.lock() else {
        return;
    };
    let hour = now / 3600;
    if state_i64(&conn, "ai_calls_hour_bucket") != hour {
        set_state_i64(&conn, "ai_calls_hour_bucket", hour);
        set_state_i64(&conn, "ai_calls_hour_count", 1);
    } else {
        let count = state_i64(&conn, "ai_calls_hour_count");
        set_state_i64(&conn, "ai_calls_hour_count", count + 1);
    }
}

const CODE_TEXT: [(i64, &str); 24] = [
    (1, "趋势结构与信号方向一致，EMA 多头排列"),
    (2, "短周期动量延续，回撤风险可控"),
    (3, "量能放大配合方向，突破有效性较高"),
    (4, "资金费率与持仓量支持该方向"),
    (5, "持仓量变化确认资金流入"),
    (6, "均值回归条件成立，反向博弈"),
    (7, "关键价位突破，跟随动能"),
    (8, "趋势末端迹象，动能衰减"),
    (9, "预期波动不足以覆盖双边手续费"),
    (10, "成交额偏低，流动性不足"),
    (11, "盘口点差过宽，滑点吃掉优势"),
    (12, "RSI 超买，追高回撤风险"),
    (13, "RSI 超卖，追空反弹风险"),
    (14, "多头拥挤，资金费率偏正"),
    (15, "空头拥挤，资金费率偏负"),
    (16, "新闻面存在反向风险事件"),
    (17, "量化信号与结构/资讯互相矛盾"),
    (18, "关键数据缺失，无法验证"),
    (19, "ATR 波动率处于可交易区间"),
    (20, "波动率过高，止损易被扫"),
    (21, "强平价距止损过近，杠杆偏高"),
    (22, "价格临近前高/前低阻力"),
    (23, "量比不足，缺乏承接"),
    (24, "参考心法提示的历史相似情形"),
];

const INVALIDATION_CODES: [(i64, &str); 8] = [
    (1, "RSI 回落至 70 下方且 15m 重新转涨，则超买判断失效"),
    (2, "价格站回 EMA21 之上并放量，则空头结构失效"),
    (3, "RSI 回升至 30 上方且 15m 转跌，则超卖判断失效"),
    (4, "资金费率转正且持仓量回升，则拥挤判断失效"),
    (5, "ATR 回落至策略下限以内，则波动不足判断失效"),
    (6, "出现反向重大新闻或事件，则本判断失效"),
    (7, "点差收窄至阈值内且量能恢复，则流动性顾虑解除"),
    (8, "补全缺失行情数据后需重新评估"),
];

const NUMBER_LABELS: [(&str, &str); 17] = [
    ("rsi", "RSI"),
    ("atr", "ATR%"),
    ("atr_pct", "ATR%"),
    ("chg", "24h%"),
    ("change_24h", "24h%"),
    ("r5", "5根%"),
    ("r15", "15根%"),
    ("v", "量比"),
    ("vol_ratio", "量比"),
    ("eg", "EMA差%"),
    ("ema_gap_pct", "EMA差%"),
    ("fund", "资金费率%"),
    ("funding", "资金费率%"),
    ("oi", "持仓量"),
    ("spread", "点差bps"),
    ("price", "价格"),
    ("score", "评分"),
];

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParamUpdate {
    pub group: String,
    pub key: String,
    pub value: Value,
    pub reason: String,
}

// v2 自由文本与扩展数值字段的解析边界，normalized/localize/parse_decision 共用。
const TEXT_BUDGET: usize = 2000;
const WHY_MAX: usize = 800;
const INV_MAX: usize = 500;
const RISK_MAX: usize = 500;
const F_MAX: usize = 200;
const METRIC_PREFIX: &str = "｜指标：";
const FREE_PREFIX: &str = "｜补充：";
const IP_TOLERANCE: f64 = 0.10;
const EP_TOLERANCE: f64 = 0.005;
const VB_MIN: i64 = 1;
const VB_MAX: i64 = 24;
const DQ_MAX: f64 = 1.0;
const RR_MAX: f64 = 10.0;

/// 首尾空白（含全角空格 U+3000）裁掉。
fn trim_display(text: &str) -> &str {
    text.trim_matches(|ch: char| ch.is_whitespace() || ch == '\u{3000}')
}

/// 自由文本清洗：换行转空格避免粘连，其余控制字符删除，首尾空白裁掉。
fn normalize_free_text(text: &str) -> String {
    let replaced: String = text
        .chars()
        .map(|ch| if ch == '\n' || ch == '\r' { ' ' } else { ch })
        .filter(|ch| !ch.is_control())
        .collect();
    trim_display(&replaced).to_string()
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Decision {
    pub action: String,
    pub confidence: f64,
    pub leverage: f64,
    pub margin_pct: Option<f64>,
    pub sl_pct: f64,
    pub tp_pct: f64,
    pub reason: String,
    pub risk_flags: Vec<String>,
    pub invalidation: String,
    pub params: Vec<ParamUpdate>,
    /// 失效价（>0，相对 mark 偏离 ≤10% 才保留）
    pub invalidation_price: Option<f64>,
    /// 信号有效期（K 线根数，1–24）
    pub valid_bars: Option<i64>,
    /// 数据质量分（0–1）
    pub data_quality: Option<f64>,
    /// 预期盈亏比（0–10）
    pub rr_target: Option<f64>,
    /// 建议入场价（>0，相对 mark 偏离 ≤0.5% 才保留）
    pub entry_price: Option<f64>,
}

impl Decision {
    fn normalized(mut self) -> Self {
        let action = self.action.trim().to_lowercase();
        let action = match action.as_str() {
            "long" | "buy" | "openlong" => "open_long",
            "short" | "sell" | "openshort" => "open_short",
            "no_trade" | "none" | "wait" => "hold",
            _ => action.as_str(),
        };
        self.action = match action {
            "open_long" | "open_short" | "hold" | "close" => action.to_string(),
            _ => "hold".to_string(),
        };
        self.confidence = self.confidence.clamp(0.0, 1.0);
        self.leverage = self.leverage.clamp(0.0, 125.0);
        self.margin_pct = self
            .margin_pct
            .filter(|value| value.is_finite() && *value > 0.0)
            .map(|value| value.clamp(0.0, 100.0));
        self.sl_pct = self.sl_pct.clamp(0.0, 50.0);
        self.tp_pct = self.tp_pct.clamp(0.0, 100.0);
        self.reason = self.reason.chars().take(TEXT_BUDGET).collect();
        self.risk_flags.truncate(6);
        self.invalidation = self.invalidation.chars().take(INV_MAX).collect();
        self
    }
}

fn parse_param_updates(data: &Map<String, Value>) -> Vec<ParamUpdate> {
    let items = data
        .get("params")
        .or_else(|| data.get("p"))
        .and_then(Value::as_array);
    let mut updates = Vec::new();
    for item in items.into_iter().flatten() {
        let Some(object) = item.as_object() else {
            continue;
        };
        let group = object
            .get("group")
            .or_else(|| object.get("g"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let key = object
            .get("key")
            .or_else(|| object.get("k"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let Some(value) = object.get("value").or_else(|| object.get("v")).cloned() else {
            continue;
        };
        if group.is_empty() || key.is_empty() {
            continue;
        }
        let reason = object
            .get("reason")
            .or_else(|| object.get("w"))
            .and_then(Value::as_str)
            .unwrap_or("");
        updates.push(ParamUpdate {
            group: group.to_string(),
            key: key.to_string(),
            value,
            reason: reason.chars().take(200).collect(),
        });
    }
    updates
}

/// v1（中文详细 JSON）与 v2（英文短键）自动探测，统一产出中文字段。
pub fn parse_decision(text: &str, ctx: Option<&Value>) -> Result<Decision, String> {
    let data = extract_json(text)?;
    let params = parse_param_updates(&data);
    let localized = if is_compact_payload(&data) {
        localize(&data, ctx)
    } else {
        data
    };
    let float = |key: &str| {
        localized
            .get(key)
            .and_then(|value| {
                value
                    .as_f64()
                    .or_else(|| value.as_str().and_then(|text| text.parse::<f64>().ok()))
            })
            .unwrap_or(0.0)
    };
    let text_of = |key: &str| {
        localized
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let flags = localized
        .get("risk_flags")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str())
                .map(|item| item.chars().take(RISK_MAX).collect::<String>())
                .take(6)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let margin_pct = localized
        .get("margin_pct")
        .and_then(|value| {
            value
                .as_f64()
                .or_else(|| value.as_str().and_then(|text| text.parse::<f64>().ok()))
        })
        .filter(|value| value.is_finite() && *value > 0.0);
    let mark = ctx
        .and_then(|c| c.get("price"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let within = |v: f64, pct: f64| mark > 0.0 && v > 0.0 && (v - mark).abs() / mark <= pct;
    let opt_float = |key: &str| {
        localized
            .get(key)
            .and_then(|value| {
                value
                    .as_f64()
                    .or_else(|| value.as_str().and_then(|text| text.parse::<f64>().ok()))
            })
            .filter(|v| v.is_finite())
    };
    Ok(Decision {
        action: text_of("action"),
        confidence: float("confidence"),
        leverage: float("leverage"),
        margin_pct,
        sl_pct: float("sl_pct"),
        tp_pct: float("tp_pct"),
        reason: text_of("reason"),
        risk_flags: flags,
        invalidation: text_of("invalidation"),
        params,
        invalidation_price: opt_float("invalidation_price").filter(|v| within(*v, IP_TOLERANCE)),
        valid_bars: opt_float("valid_bars")
            .filter(|v| *v > 0.0)
            .map(|v| v.round() as i64)
            .map(|v| v.clamp(VB_MIN, VB_MAX)),
        data_quality: opt_float("data_quality").map(|v| v.clamp(0.0, DQ_MAX)),
        rr_target: opt_float("rr_target").map(|v| v.clamp(0.0, RR_MAX)),
        entry_price: opt_float("entry_price").filter(|v| within(*v, EP_TOLERANCE)),
    }
    .normalized())
}

pub fn is_compact_payload(data: &Map<String, Value>) -> bool {
    data.contains_key("a") && !data.contains_key("action")
}

pub fn localize(data: &Map<String, Value>, ctx: Option<&Value>) -> Map<String, Value> {
    let action = match data
        .get("a")
        .and_then(Value::as_str)
        .unwrap_or("H")
        .trim()
        .to_uppercase()
        .as_str()
    {
        "L" => "open_long",
        "S" => "open_short",
        "C" => "close",
        _ => "hold",
    };
    let numbers = data
        .get("n")
        .and_then(Value::as_object)
        .cloned()
        .or_else(|| {
            ctx.and_then(|ctx| ctx.get("features"))
                .and_then(Value::as_object)
                .cloned()
        })
        .unwrap_or_default();

    let mut reasons = codes_text(data.get("r"), &CODE_TEXT, 3);
    if reasons.is_empty() {
        reasons.push(if action == "hold" {
            "模型未给出理由码，按缺省 hold 处理".to_string()
        } else {
            "模型未给出理由码".to_string()
        });
    }

    // 四个自由文本字段 + 指标块 + 补充块共用总预算，
    // 按 why → inv → risk → numeric → f 顺序占用。
    fn take_budget(text: &str, max: usize, left: &mut usize) -> String {
        let out: String = text.chars().take(max.min(*left)).collect();
        *left -= out.chars().count();
        out
    }
    let mut left = TEXT_BUDGET;
    let why = take_budget(
        trim_display(data.get("why").and_then(Value::as_str).unwrap_or("")),
        WHY_MAX,
        &mut left,
    );
    let inv_text = take_budget(
        trim_display(data.get("inv").and_then(Value::as_str).unwrap_or("")),
        INV_MAX,
        &mut left,
    );
    let risk_text = take_budget(
        trim_display(data.get("risk").and_then(Value::as_str).unwrap_or("")),
        RISK_MAX,
        &mut left,
    );

    let numeric = numbers_text(&numbers);
    let numeric_block = if numeric.is_empty() || left <= METRIC_PREFIX.chars().count() {
        String::new()
    } else {
        take_budget(&format!("{METRIC_PREFIX}{numeric}"), usize::MAX, &mut left)
    };
    let free_source = normalize_free_text(data.get("f").and_then(Value::as_str).unwrap_or(""));
    let free_budget = F_MAX.min(left.saturating_sub(FREE_PREFIX.chars().count()));
    let free = take_budget(&free_source, free_budget, &mut left);
    let free_block = if free.is_empty() {
        String::new()
    } else {
        format!("{FREE_PREFIX}{free}")
    };

    let mut reason = if why.is_empty() {
        reasons.join("；")
    } else {
        why
    };
    reason.push_str(&numeric_block);
    reason.push_str(&free_block);

    let risks = if risk_text.is_empty() {
        codes_text(data.get("k"), &CODE_TEXT, 3)
    } else {
        vec![risk_text]
    };
    let x_code = data.get("x").and_then(Value::as_i64).unwrap_or(0);
    let invalidation = if inv_text.is_empty() {
        INVALIDATION_CODES
            .iter()
            .find(|(code, _)| *code == x_code)
            .map(|(_, text)| text.to_string())
            .unwrap_or_else(|| "数据或行情结构发生变化时本判断失效".to_string())
    } else {
        inv_text
    };

    let number = |key: &str| {
        data.get(key)
            .and_then(|value| {
                value
                    .as_f64()
                    .or_else(|| value.as_str().and_then(|text| text.parse::<f64>().ok()))
            })
            .unwrap_or(0.0)
    };

    let mut out = Map::new();
    out.insert("action".to_string(), json!(action));
    out.insert("confidence".to_string(), json!(number("c")));
    out.insert("leverage".to_string(), json!(number("l")));
    out.insert("margin_pct".to_string(), json!(number("m")));
    out.insert("sl_pct".to_string(), json!(number("s")));
    out.insert("tp_pct".to_string(), json!(number("t")));
    out.insert(
        "reason".to_string(),
        json!(reason.chars().take(TEXT_BUDGET).collect::<String>()),
    );
    out.insert("risk_flags".to_string(), json!(risks));
    out.insert("invalidation".to_string(), json!(invalidation));
    let opt = |key: &str| {
        data.get(key)
            .and_then(|value| {
                value
                    .as_f64()
                    .or_else(|| value.as_str().and_then(|text| text.parse::<f64>().ok()))
            })
            .filter(|v| v.is_finite())
    };
    out.insert("invalidation_price".to_string(), json!(opt("ip")));
    out.insert("valid_bars".to_string(), json!(opt("vb")));
    out.insert("data_quality".to_string(), json!(opt("dq")));
    out.insert("rr_target".to_string(), json!(opt("rr")));
    out.insert("entry_price".to_string(), json!(opt("ep")));
    out
}

fn codes_text(values: Option<&Value>, table: &[(i64, &str)], limit: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(items) = values.and_then(Value::as_array) {
        for item in items {
            let code = item
                .as_i64()
                .or_else(|| item.as_str().and_then(|text| text.parse::<i64>().ok()));
            let Some(code) = code else {
                continue;
            };
            if let Some((_, text)) = table.iter().find(|(key, _)| *key == code) {
                if !out.iter().any(|existing| existing == text) {
                    out.push((*text).to_string());
                }
            }
            if out.len() >= limit {
                break;
            }
        }
    }
    out
}

fn numbers_text(numbers: &Map<String, Value>) -> String {
    let mut parts: Vec<String> = Vec::new();
    for (key, value) in numbers {
        let Some(number) = value.as_f64() else {
            continue;
        };
        let label = NUMBER_LABELS
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, label)| *label)
            .unwrap_or(key.as_str());
        let trimmed = (number * 10000.0).round() / 10000.0;
        parts.push(format!("{label} {trimmed}"));
        if parts.len() >= 6 {
            break;
        }
    }
    parts.join("、")
}

/// 从模型输出提取 JSON 对象，允许代码围栏、前后散文与尾逗号。
pub fn extract_json(text: &str) -> Result<Map<String, Value>, String> {
    if text.trim().is_empty() {
        return Err("模型返回为空".to_string());
    }
    let mut raw = text.trim().to_string();
    if let Some(start) = raw.find("```") {
        let rest = &raw[start + 3..];
        let rest = rest.strip_prefix("json").unwrap_or(rest);
        if let Some(end) = rest.find("```") {
            raw = rest[..end].trim().to_string();
        }
    }
    if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&raw) {
        return Ok(map);
    }
    if let (Some(start), Some(end)) = (raw.find('{'), raw.rfind('}')) {
        if end > start {
            let candidate = &raw[start..=end];
            if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(candidate) {
                return Ok(map);
            }
            if let Ok(Value::Object(map)) =
                serde_json::from_str::<Value>(&strip_trailing_commas(candidate))
            {
                return Ok(map);
            }
        }
    }
    Err("无法从模型输出中解析出 JSON 对象".to_string())
}

fn strip_trailing_commas(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b',' {
            let mut lookahead = index + 1;
            while lookahead < bytes.len() && bytes[lookahead].is_ascii_whitespace() {
                lookahead += 1;
            }
            if lookahead < bytes.len() && (bytes[lookahead] == b'}' || bytes[lookahead] == b']') {
                index += 1;
                continue;
            }
        }
        let ch = text[index..].chars().next().unwrap_or(' ');
        out.push(ch);
        index += ch.len_utf8();
    }
    out
}

fn models_endpoints(base: &str, protocol: &str) -> Vec<String> {
    let base = base.trim().trim_end_matches('/');
    if base.is_empty() {
        return Vec::new();
    }
    let mut candidates: Vec<String> = Vec::new();
    if base.ends_with("/models") {
        candidates.push(base.to_string());
    } else {
        candidates.push(format!("{base}/models"));
        if let Some(stripped) = base.strip_suffix("/v1") {
            candidates.push(format!("{stripped}/models"));
        } else {
            let tail = base.rsplit('/').next().unwrap_or("");
            let has_version = tail
                .strip_prefix('v')
                .map(|digits| !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()))
                .unwrap_or(false);
            if !has_version {
                candidates.push(format!("{base}/v1/models"));
            }
        }
    }
    let mut unique: Vec<String> = Vec::new();
    for candidate in candidates {
        if !unique.contains(&candidate) {
            unique.push(candidate);
        }
    }
    let _ = protocol;
    unique
}

/// 探测 `/models`，按候选地址依次尝试；返回逐次尝试记录与去重后的模型列表。
pub async fn fetch_remote_models(
    transport: &dyn AiTransport,
    base_url: &str,
    api_key: &str,
    protocol: &str,
    timeout_secs: u64,
) -> Value {
    let base = base_url.trim().trim_end_matches('/');
    if !(base.starts_with("http://") || base.starts_with("https://")) {
        return json!({
            "ok": false,
            "models": [],
            "endpoint": "",
            "total": 0,
            "error": "Base URL 必须以 http:// 或 https:// 开头",
            "tried": [],
        });
    }
    let is_anthropic = base.contains("anthropic") || protocol.starts_with("anthropic");
    let mut headers = vec![("User-Agent".to_string(), "NanoQuant/1.0".to_string())];
    if is_anthropic {
        headers.push(("anthropic-version".to_string(), "2023-06-01".to_string()));
        if !api_key.is_empty() {
            headers.push(("x-api-key".to_string(), api_key.to_string()));
        }
    } else if !api_key.is_empty() {
        headers.push(("Authorization".to_string(), format!("Bearer {api_key}")));
    }

    let mut tried: Vec<Value> = Vec::new();
    for url in models_endpoints(base, protocol) {
        let reply = transport.get_json(&url, &headers, timeout_secs).await;
        let reply = match reply {
            Ok(reply) => reply,
            Err(error) => {
                tried.push(json!({"endpoint": url, "error": truncate(&error, 160)}));
                continue;
            }
        };
        if reply.status != 200 {
            tried.push(json!({
                "endpoint": url,
                "status": reply.status,
                "error": truncate(&reply.body, 160),
            }));
            continue;
        }
        let Ok(data) = serde_json::from_str::<Value>(&reply.body) else {
            tried.push(json!({"endpoint": url, "status": reply.status, "error": "返回不是 JSON"}));
            continue;
        };
        let models = parse_model_list(&data);
        if !models.is_empty() {
            tried.push(json!({"endpoint": url, "status": 200}));
            return json!({
                "ok": true,
                "models": models,
                "endpoint": url,
                "total": models.len(),
                "error": "",
                "tried": tried,
            });
        }
        tried.push(json!({"endpoint": url, "status": reply.status, "error": "返回里没有模型列表"}));
    }

    let last = tried.last().cloned().unwrap_or_else(|| json!({}));
    json!({
        "ok": false,
        "models": [],
        "endpoint": last.get("endpoint").cloned().unwrap_or_else(|| json!("")),
        "total": 0,
        "error": last
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("未取到模型列表"),
        "tried": tried,
    })
}

fn parse_model_list(data: &Value) -> Vec<Value> {
    let raw = match data {
        Value::Array(items) => items.clone(),
        Value::Object(map) => map
            .get("data")
            .or_else(|| map.get("models"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    let mut models: Vec<(String, Value)> = Vec::new();
    for item in raw {
        let (id, value) = if let Some(text) = item.as_str() {
            (
                text.to_string(),
                json!({"id": text, "name": text, "context": Value::Null}),
            )
        } else if let Value::Object(map) = &item {
            let id = map
                .get("id")
                .or_else(|| map.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            if id.is_empty() {
                continue;
            }
            let name = map
                .get("display_name")
                .or_else(|| map.get("name"))
                .and_then(Value::as_str)
                .unwrap_or(&id)
                .to_string();
            let context = map
                .get("context_length")
                .or_else(|| map.get("max_tokens"))
                .cloned()
                .unwrap_or(Value::Null);
            (
                id.clone(),
                json!({"id": id, "name": name, "context": context}),
            )
        } else {
            continue;
        };
        if models.iter().any(|(existing, _)| *existing == id) {
            continue;
        }
        models.push((id, value));
    }
    models.sort_by(|a, b| a.0.cmp(&b.0));
    models.into_iter().map(|(_, value)| value).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct FakeTransport {
        posts: Mutex<Vec<HttpReply>>,
        gets: Mutex<Vec<HttpReply>>,
        seen_posts: Mutex<Vec<Value>>,
        seen_urls: Mutex<Vec<String>>,
    }

    impl FakeTransport {
        fn new(posts: Vec<HttpReply>, gets: Vec<HttpReply>) -> Self {
            Self {
                posts: Mutex::new(posts),
                gets: Mutex::new(gets),
                seen_posts: Mutex::new(Vec::new()),
                seen_urls: Mutex::new(Vec::new()),
            }
        }
    }

    impl AiTransport for FakeTransport {
        fn post_json<'a>(
            &'a self,
            _url: &'a str,
            _headers: &'a [(String, String)],
            body: &'a Value,
            _timeout_secs: u64,
        ) -> BoxFuture<'a, Result<HttpReply, String>> {
            self.seen_posts.lock().unwrap().push(body.clone());
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
            url: &'a str,
            _headers: &'a [(String, String)],
            _timeout_secs: u64,
        ) -> BoxFuture<'a, Result<HttpReply, String>> {
            self.seen_urls.lock().unwrap().push(url.to_string());
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

    fn fixture_db(path: &std::path::Path) -> Db {
        let _ = std::fs::remove_file(path);
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE app_settings(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER); \
             CREATE TABLE engine_state(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER); \
             CREATE TABLE ai_call_logs(id INTEGER PRIMARY KEY, ts INTEGER, provider TEXT, model TEXT, purpose TEXT, \
             ok INTEGER, latency_ms INTEGER, prompt_tokens INTEGER, completion_tokens INTEGER, cost_usd REAL, error TEXT, raw_text TEXT); \
             INSERT INTO app_settings VALUES('ai','{\"max_calls_per_hour\":2,\"monthly_budget_usd\":5,\"timeout_sec\":5}',0);",
        )
        .unwrap();
        drop(conn);
        Db::open(path.to_str().unwrap()).unwrap()
    }

    fn provider() -> ProviderConfig {
        ProviderConfig {
            id: 1,
            name: "alpha".to_string(),
            protocol: "openai_chat".to_string(),
            base_url: "https://api.example.com/v1".to_string(),
            api_key: "sk-test".to_string(),
            model: "deepseek-v4".to_string(),
            temperature: 0.2,
            max_tokens: 1024,
            reasoning_effort: "high".to_string(),
            timeout_sec: 5,
            json_mode: true,
        }
    }

    #[test]
    fn join_url_matches_legacy_candidates() {
        assert_eq!(
            join_url("https://api.deepseek.com/v1", "openai_chat").unwrap(),
            "https://api.deepseek.com/v1/chat/completions"
        );
        assert_eq!(
            join_url("https://gw.example.com/openai", "openai_chat").unwrap(),
            "https://gw.example.com/openai/chat/completions"
        );
        assert_eq!(
            join_url("https://api.anthropic.com", "anthropic_messages").unwrap(),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            join_url("https://api.openai.com/v1", "openai_responses").unwrap(),
            "https://api.openai.com/v1/responses"
        );
        assert!(join_url("", "openai_chat").is_err());
    }

    #[test]
    fn output_problem_attributes_truncation_and_thinking_only_output() {
        assert!(output_problem("正文", "stop", 1000).is_empty());
        assert!(output_problem("", "length", 128).contains("截断"));
        assert!(output_problem("", "length", 4096).contains("3000+"));
        assert!(output_problem("", "stop", 4096).contains("thinking"));
    }

    #[test]
    fn parses_v1_and_v2_decisions_into_chinese_fields() {
        let v1 = r#"{"action":"LONG","confidence":1.4,"leverage":-3,"sl_pct":2,"tp_pct":5,"reason":"突破","risk_flags":["a"]}"#;
        let decision = parse_decision(v1, None).unwrap();
        assert_eq!(decision.action, "open_long");
        assert_eq!(decision.confidence, 1.0);
        assert_eq!(decision.leverage, 0.0);

        let v2 = r#"{"a":"S","c":0.72,"l":12,"s":1.5,"t":3,"r":[1,9],"k":[16],"x":2,"n":{"rsi":71.25},"f":"fade rally"}"#;
        let decision = parse_decision(v2, None).unwrap();
        assert_eq!(decision.action, "open_short");
        assert_eq!(decision.confidence, 0.72);
        assert_eq!(decision.leverage, 12.0);
        assert!(decision.reason.contains("趋势结构与信号方向一致"));
        assert!(decision.reason.contains("预期波动不足以覆盖双边手续费"));
        assert!(decision.reason.contains("RSI 71.25"));
        assert!(decision.reason.contains("fade rally"));
        assert_eq!(decision.risk_flags.len(), 1);
        assert!(decision.invalidation.contains("EMA21"));
    }

    #[test]
    fn sl_pct_is_clamped_to_the_50_margin_ceiling() {
        let text = r#"{"a":"S","c":0.7,"l":10,"s":150,"t":50,"r":[1],"k":[1],"x":1,"f":"x"}"#;
        let decision = parse_decision(text, None).unwrap();
        assert_eq!(decision.sl_pct, 50.0);
    }

    #[test]
    fn decision_parses_v2_param_updates() {
        let text = r#"{"a":"L","c":0.7,"l":3,"s":1,"t":2,"r":[1],"k":[1],"x":1,"p":[{"g":"strategy","k":"entry_threshold","v":0.35,"w":"放量"}]}"#;
        let decision = parse_decision(text, None).unwrap();
        assert_eq!(decision.params.len(), 1);
        assert_eq!(decision.params[0].group, "strategy");
        assert_eq!(decision.params[0].key, "entry_threshold");
        assert_eq!(decision.params[0].value, serde_json::json!(0.35));
        assert_eq!(decision.params[0].reason, "放量");
    }

    #[test]
    fn decision_drops_params_without_group_or_key() {
        let text = r#"{"a":"H","c":0.5,"p":[{"k":"entry_threshold","v":0.35},{"g":"risk","v":1}]}"#;
        assert!(parse_decision(text, None).unwrap().params.is_empty());
    }

    #[test]
    fn decision_parses_v1_param_updates() {
        let text = r#"{"action":"hold","confidence":0.5,"params":[{"group":"risk","key":"margin_pct","value":5,"reason":"降风险"}]}"#;
        let decision = parse_decision(text, None).unwrap();
        assert_eq!(decision.params.len(), 1);
        assert_eq!(decision.params[0].group, "risk");
        assert_eq!(decision.params[0].value, serde_json::json!(5));
    }

    #[test]
    fn extract_json_handles_fences_prose_and_trailing_commas() {
        let fenced = "说明\n```json\n{\"a\":\"L\",\"c\":0.5,}\n```";
        let data = extract_json(fenced).unwrap();
        assert_eq!(data.get("a").unwrap(), "L");
        let prose = "answer: {\"action\":\"hold\"} tail";
        assert_eq!(extract_json(prose).unwrap().get("action").unwrap(), "hold");
        assert!(extract_json("   ").is_err());
    }

    #[test]
    fn budget_blocks_hourly_rate_and_monthly_spend() {
        let path = std::env::temp_dir().join("nq-ai-budget.db");
        let db = fixture_db(&path);
        let cfg = read_ai_config(&db);
        let conn = db.conn.lock().unwrap();
        assert_eq!(cfg.max_calls_per_hour, 2);
        assert!(budget_blocked(&conn, &cfg, 10_000).is_none());
        set_state_i64(&conn, "ai_calls_hour_bucket", 10_000 / 3600);
        set_state_i64(&conn, "ai_calls_hour_count", 2);
        assert!(budget_blocked(&conn, &cfg, 10_000)
            .unwrap()
            .contains("本小时"));
        set_state_i64(&conn, "ai_calls_hour_count", 0);
        conn.execute(
            "INSERT INTO ai_call_logs(ts, provider, model, purpose, ok, latency_ms, prompt_tokens, completion_tokens, cost_usd, error, raw_text) \
             VALUES(?1, 'a', 'm', 'decision', 1, 1, 1, 1, 9.0, '', '')",
            [10_000],
        )
        .unwrap();
        assert!(budget_blocked(&conn, &cfg, 10_000)
            .unwrap()
            .contains("预算"));
        drop(conn);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn chat_drops_reasoning_effort_after_400_and_retries_once() {
        let path = std::env::temp_dir().join("nq-ai-retry.db");
        let db = fixture_db(&path);
        let transport = FakeTransport::new(
            vec![
                HttpReply {
                    status: 400,
                    body: "{\"error\":\"unsupported reasoning_effort\"}".to_string(),
                },
                HttpReply {
                    status: 200,
                    body: "{\"choices\":[{\"message\":{\"content\":\"{\\\"a\\\":\\\"H\\\"}\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}".to_string(),
                },
            ],
            Vec::new(),
        );
        let messages = vec![ChatMessage::new("user", "decide")];
        let mut call = ChatCall::new(&messages, "decision");
        call.retries = 1;
        let outcome = chat(&transport, &db, &provider(), call).await;
        assert!(outcome.ok, "第二次请求应成功: {}", outcome.error);
        assert_eq!(outcome.text, "{\"a\":\"H\"}");
        let seen = transport.seen_posts.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen[0].get("reasoning_effort").is_some());
        assert!(seen[1].get("reasoning_effort").is_none());
        assert!(seen[1].get("response_format").is_none());
        let conn = db.conn.lock().unwrap();
        let logs: i64 = conn
            .query_row("SELECT COUNT(*) FROM ai_call_logs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(logs, 1, "成功调用必须留痕");
        drop(conn);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn chat_outcome_carries_its_own_log_id() {
        let path = std::env::temp_dir().join("nq-ai-log-id.db");
        let db = fixture_db(&path);
        let body = "{\"choices\":[{\"message\":{\"content\":\"{\\\"a\\\":\\\"H\\\"}\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}";
        let transport = FakeTransport::new(
            vec![HttpReply {
                status: 200,
                body: body.to_string(),
            }],
            Vec::new(),
        );
        let messages = vec![ChatMessage::new("user", "decide")];
        let outcome = chat(
            &transport,
            &db,
            &provider(),
            ChatCall::new(&messages, "decision"),
        )
        .await;
        assert!(outcome.ok, "{}", outcome.error);
        assert!(outcome.log_id > 0);
        let conn = db.conn.lock().unwrap();
        let found: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM ai_call_logs WHERE id=?1",
                [outcome.log_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(found, 1);
        drop(conn);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn chat_marks_truncated_output_as_failure() {
        let path = std::env::temp_dir().join("nq-ai-truncate.db");
        let db = fixture_db(&path);
        let transport = FakeTransport::new(
            vec![HttpReply {
                status: 200,
                body: "{\"choices\":[{\"message\":{\"content\":\"\"},\"finish_reason\":\"length\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":1}}".to_string(),
            }],
            Vec::new(),
        );
        let messages = vec![ChatMessage::new("user", "decide")];
        let mut call = ChatCall::new(&messages, "decision");
        call.retries = 0;
        let outcome = chat(&transport, &db, &provider(), call).await;
        assert!(!outcome.ok);
        assert!(outcome.error.contains("截断"), "{}", outcome.error);
        assert_eq!(outcome.finish_reason, "length");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn fetch_remote_models_falls_back_to_v1_when_first_candidate_fails() {
        let transport = FakeTransport::new(
            Vec::new(),
            vec![
                HttpReply {
                    status: 404,
                    body: "not found".to_string(),
                },
                HttpReply {
                    status: 200,
                    body: "{\"data\":[{\"id\":\"m2\"},{\"id\":\"m1\"},{\"id\":\"m1\"}]}"
                        .to_string(),
                },
            ],
        );
        let result = fetch_remote_models(
            &transport,
            "https://api.example.com/v1",
            "k",
            "openai_chat",
            5,
        )
        .await;
        assert_eq!(result["ok"], true);
        assert_eq!(result["total"], 2);
        assert_eq!(result["models"][0]["id"], "m1", "结果按 id 排序并去重");
        let urls = transport.seen_urls.lock().unwrap();
        assert_eq!(urls[0], "https://api.example.com/v1/models");
        assert_eq!(urls[1], "https://api.example.com/models");
    }

    #[test]
    fn v2_extended_fields_are_parsed() {
        let ctx = json!({"price": 100.0});
        let text = r#"{"a":"L","c":0.7,"l":5,"m":20,"s":30,"t":60,
        "why":"趋势向上，量能放大","risk":"上方套牢盘密集","inv":"跌破 EMA21",
        "ip":95.0,"vb":6,"dq":0.8,"rr":1.5,"ep":100.2}"#;
        let d = parse_decision(text, Some(&ctx)).unwrap();
        assert_eq!(d.reason, "趋势向上，量能放大");
        assert_eq!(d.invalidation, "跌破 EMA21");
        assert_eq!(d.invalidation_price, Some(95.0));
        assert_eq!(d.valid_bars, Some(6));
        assert_eq!(d.data_quality, Some(0.8));
        assert_eq!(d.rr_target, Some(1.5));
        assert_eq!(d.entry_price, Some(100.2));
    }

    #[test]
    fn free_text_fields_keep_chinese_and_respect_total_budget() {
        let ctx = json!({"price": 100.0});
        let long = "很".repeat(3000);
        let text = format!(
            r#"{{"a":"H","c":0.5,"why":"{}","risk":"{}","inv":"{}","f":"中文补充"}}"#,
            long, long, long
        );
        let d = parse_decision(&text, Some(&ctx)).unwrap();
        assert!(d.reason.contains('很'));
        assert!(d.reason.chars().count() <= 2000);
        assert!(d.invalidation.chars().count() <= 500);
        assert!(
            d.risk_flags
                .iter()
                .map(|s| s.chars().count())
                .sum::<usize>()
                <= 500
        );
    }

    #[test]
    fn invalidation_and_entry_price_outside_range_become_none() {
        let ctx = json!({"price": 100.0});
        let text = r#"{"a":"L","c":0.7,"ip":50.0,"ep":101.0}"#;
        let d = parse_decision(text, Some(&ctx)).unwrap();
        assert_eq!(d.invalidation_price, None);
        assert_eq!(d.entry_price, None);
    }

    #[test]
    fn v1_payload_still_parses() {
        let ctx = json!({"price": 100.0});
        let text = r#"{"action":"hold","confidence":0.4,"reason":"观望"}"#;
        let d = parse_decision(text, Some(&ctx)).unwrap();
        assert_eq!(d.action, "hold");
        assert_eq!(d.data_quality, None);
    }

    #[test]
    fn free_text_fields_fall_back_to_code_tables() {
        let ctx = json!({"price": 100.0});
        let text = r#"{"a":"L","c":0.7,"r":[1],"k":[2],"x":3}"#;
        let d = parse_decision(text, Some(&ctx)).unwrap();
        assert!(d.reason.contains("EMA 多头排列"), "{}", d.reason);
        assert!(d.risk_flags.iter().any(|s| s.contains("短周期动量延续")));
        assert!(d.invalidation.contains("RSI 回升至 30"));
    }

    #[test]
    fn extended_numbers_are_clamped() {
        let ctx = json!({"price": 100.0});
        let low =
            parse_decision(r#"{"a":"L","c":0.7,"dq":-1,"rr":-1,"vb":0.4}"#, Some(&ctx)).unwrap();
        assert_eq!(low.data_quality, Some(0.0));
        assert_eq!(low.rr_target, Some(0.0));
        assert_eq!(low.valid_bars, Some(VB_MIN));

        let high =
            parse_decision(r#"{"a":"L","c":0.7,"dq":2,"rr":11,"vb":25}"#, Some(&ctx)).unwrap();
        assert_eq!(high.data_quality, Some(DQ_MAX));
        assert_eq!(high.rr_target, Some(RR_MAX));
        assert_eq!(high.valid_bars, Some(VB_MAX));
    }

    #[test]
    fn invalidation_and_entry_price_need_a_positive_mark() {
        let text = r#"{"a":"L","c":0.7,"ip":95.0,"ep":100.2}"#;
        let no_ctx = parse_decision(text, None).unwrap();
        assert_eq!(no_ctx.invalidation_price, None);
        assert_eq!(no_ctx.entry_price, None);

        let zero = json!({"price": 0.0});
        let zero_ctx = parse_decision(text, Some(&zero)).unwrap();
        assert_eq!(zero_ctx.invalidation_price, None);
        assert_eq!(zero_ctx.entry_price, None);
    }

    #[test]
    fn extended_numbers_accept_string_digits() {
        let ctx = json!({"price": 100.0});
        let d = parse_decision(
            r#"{"a":"L","c":0.7,"ip":"95.0","dq":"0.8","rr":"1.5","ep":"100.2"}"#,
            Some(&ctx),
        )
        .unwrap();
        assert_eq!(d.invalidation_price, Some(95.0));
        assert_eq!(d.data_quality, Some(0.8));
        assert_eq!(d.rr_target, Some(1.5));
        assert_eq!(d.entry_price, Some(100.2));
    }

    #[test]
    fn valid_bars_accepts_float_integral_values() {
        let ctx = json!({"price": 100.0});
        let d = parse_decision(r#"{"a":"L","c":0.7,"vb":6.0}"#, Some(&ctx)).unwrap();
        assert_eq!(d.valid_bars, Some(6));
    }

    #[test]
    fn vb_zero_means_not_given() {
        let ctx = json!({"price": 100.0});
        let d = parse_decision(r#"{"a":"L","c":0.7,"vb":0}"#, Some(&ctx)).unwrap();
        assert_eq!(d.valid_bars, None);
    }

    #[test]
    fn effective_max_tokens_respects_provider_ceiling() {
        assert_eq!(effective_max_tokens(Some(16000), 32000), 32000);
        assert_eq!(effective_max_tokens(Some(16000), 3000), 16000);
        assert_eq!(effective_max_tokens(None, 3000), 3000);
    }

    #[test]
    fn blank_free_text_is_not_appended() {
        let ctx = json!({"price": 100.0});
        let ascii =
            parse_decision(r#"{"a":"H","c":0.5,"why":"理由","f":"   "}"#, Some(&ctx)).unwrap();
        assert!(!ascii.reason.contains(FREE_PREFIX));

        let cjk = parse_decision(
            "{\"a\":\"H\",\"c\":0.5,\"why\":\"理由\",\"f\":\"\\u3000\\u3000\"}",
            Some(&ctx),
        )
        .unwrap();
        assert!(!cjk.reason.contains(FREE_PREFIX));

        let newline =
            parse_decision(r#"{"a":"H","c":0.5,"why":"理由","f":"a\nb"}"#, Some(&ctx)).unwrap();
        assert!(newline.reason.ends_with("a b"), "{}", newline.reason);
    }

    #[test]
    fn reason_budget_includes_numeric_and_free_blocks() {
        let ctx = json!({"price": 100.0, "features": {"rsi": 70.0, "v": 2.0}});
        let long = "很".repeat(3000);
        let text = format!(
            r#"{{"a":"L","c":0.7,"why":"{}","inv":"{}","risk":"{}","f":"{}"}}"#,
            long, long, long, long
        );
        let d = parse_decision(&text, Some(&ctx)).unwrap();
        assert!(
            d.reason.chars().count() <= TEXT_BUDGET,
            "length {}",
            d.reason.chars().count()
        );
        assert!(d.reason.starts_with(&"很".repeat(WHY_MAX)));
        assert!(d.reason.contains(METRIC_PREFIX));
        assert!(d.reason.contains(FREE_PREFIX));
    }
}
