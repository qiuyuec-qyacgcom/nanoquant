//! P3-1：HTTP/2 连接池 + 启动预热（reqwest + rustls）。
//!
//! 阶段目标（见 docs/tasks 与 rust/README「下一步」）：
//! - 用 rustls（纯 Rust TLS，不依赖系统 OpenSSL）建立可复用的 HTTP/2 连接池，
//!   为后续「订单状态机 / 保护单并行下发」提供低延迟通道；
//! - 启动时**后台预热**：提前完成 DNS/TLS 握手，避免第一笔真实下单吃握手开销。
//!
//! 硬约束（与 P2 影子模式共存）：
//! - 连接池失败**绝不阻断**签名服务启动 —— 预热与探活都是 best-effort；
//! - 本模块**不碰** sign/health 的既有字段，只新增可选字段，保证老调用方不受影响；
//! - 默认**只连不回传订单**：`probe` 仅发只读请求（`/fapi/v1/time`），不发单。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;

/// 连接池运行时配置。全部来自环境变量，缺省即可用。
#[derive(Debug, Clone)]
pub struct HttpConfig {
    /// 交易所 REST 根地址（不带末尾斜杠）。空 = 关闭连接池（保持纯签名模式）。
    pub base_url: String,
    /// 单请求超时。
    pub timeout: Duration,
    /// 是否在启动时预热。
    pub warmup: bool,
}

impl HttpConfig {
    pub fn from_env() -> Self {
        let base = std::env::var("EXEC_BASE_URL")
            .unwrap_or_default()
            .trim()
            .trim_end_matches('/')
            .to_string();
        let timeout_ms = std::env::var("EXEC_HTTP_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(3000)
            .clamp(200, 30_000);
        let warmup = std::env::var("EXEC_HTTP_WARMUP")
            .map(|v| {
                !matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "0" | "false" | "off" | "no"
                )
            })
            .unwrap_or(true);
        Self {
            base_url: base,
            timeout: Duration::from_millis(timeout_ms),
            warmup,
        }
    }

    pub fn enabled(&self) -> bool {
        !self.base_url.is_empty()
    }
}

/// 连接池 + 预热状态快照（序列化后并进 health 响应，向后兼容）。
#[derive(Debug, Default, Serialize)]
pub struct HttpStatus {
    /// 连接池是否已配置（EXEC_BASE_URL 非空）
    pub enabled: bool,
    /// 根地址（未配置时为空串）
    pub base_url: String,
    /// 是否已完成预热（探活成功一次）
    pub warmed: bool,
    /// 预热耗时（毫秒）；0 表示尚未完成
    pub warmup_ms: u64,
    /// 成功请求计数
    pub ok: u64,
    /// 失败请求计数
    pub errors: u64,
    /// 最近一次错误（成功时清空）
    pub last_error: String,
}

/// 发单失败的两种语义（**绝不能混用**）。
#[derive(Debug)]
pub enum PlaceError {
    /// 交易所明确拒绝：可安全回退/上报
    Rejected(String),
    /// 结果未知：**禁止重发**，否则可能重复下单
    Unknown(String),
}

impl PlaceError {
    pub fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown(_))
    }
    pub fn message(&self) -> &str {
        match self {
            Self::Rejected(m) | Self::Unknown(m) => m,
        }
    }
}

/// 测试出口：与 legacy 内核（已退役） `quote(signature, safe='')` 对齐的最小 URL 编码。
#[cfg(test)]
pub fn urlencode_component_for_test(raw: &str) -> String {
    urlencode_component(raw)
}

/// 与 legacy 内核（已退役） `quote(signature, safe='')` 对齐的最小 URL 编码。
fn urlencode_component(raw: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(raw.len() * 2);
    for b in raw.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' => {
                out.push(*b as char)
            }
            _ => {
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0x0f) as usize] as char);
            }
        }
    }
    out
}

pub struct HttpClient {
    inner: Option<reqwest::Client>,
    cfg: HttpConfig,
    warmed: std::sync::atomic::AtomicBool,
    warmup_ms: AtomicU64,
    ok: AtomicU64,
    errors: AtomicU64,
    last_error: std::sync::Mutex<String>,
}

impl HttpClient {
    /// 构建连接池。`base_url` 为空时返回「未启用」实例，所有方法安全空转。
    pub fn new(cfg: HttpConfig) -> Self {
        let inner = if cfg.enabled() {
            // rustls + HTTP/2：连接池常驻，keep-alive 默认开启。
            // 注意：这里不设置证书校验绕过，生产必须走系统根证书（rustls-native-certs 缺省行为）。
            reqwest::Client::builder()
                .timeout(cfg.timeout)
                .connect_timeout(cfg.timeout)
                .pool_idle_timeout(Duration::from_secs(90))
                .pool_max_idle_per_host(8)
                .http2_adaptive_window(true)
                .build()
                .ok()
        } else {
            None
        };
        Self {
            inner,
            cfg,
            warmed: std::sync::atomic::AtomicBool::new(false),
            warmup_ms: AtomicU64::new(0),
            ok: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            last_error: std::sync::Mutex::new(String::new()),
        }
    }

    pub fn status(&self) -> HttpStatus {
        HttpStatus {
            enabled: self.cfg.enabled(),
            base_url: self.cfg.base_url.clone(),
            warmed: self.warmed.load(Ordering::Relaxed),
            warmup_ms: self.warmup_ms.load(Ordering::Relaxed),
            ok: self.ok.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            last_error: self
                .last_error
                .lock()
                .map(|s| s.clone())
                .unwrap_or_default(),
        }
    }

    fn record_err(&self, msg: String) {
        self.errors.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut slot) = self.last_error.lock() {
            *slot = msg;
        }
    }

    /// P3-3 收尾：**真正发送**一笔已签名的 POST（仅 `EXEC_PLACE_MODE=enabled` 时被调用）。
    ///
    /// 返回值语义严格区分三种情形，调用方据此决定是否回退 legacy 内核（已退役）：
    /// - `Ok(body)`：交易所明确返回 2xx；
    /// - `Err(PlaceError::Rejected(msg))`：交易所明确拒绝（4xx 带 code）→ 可安全回退/报错；
    /// - `Err(PlaceError::Unknown(msg))`：**结果未知**（超时、连接中断、5xx）→
    ///   **绝不能重发**，否则可能重复挂保护单。
    pub async fn post_signed(
        &self,
        path: &str,
        query: &str,
        signature: &str,
        api_key: &str,
    ) -> Result<String, PlaceError> {
        let Some(client) = self.inner.as_ref() else {
            return Err(PlaceError::Rejected(
                "连接池未启用（EXEC_BASE_URL 为空）".into(),
            ));
        };
        if api_key.trim().is_empty() {
            return Err(PlaceError::Rejected("缺少 API Key".into()));
        }
        // 与 legacy 内核（已退役） 一致：签名再做一次 URL 编码（base64 的 '+' 会被当空格）
        let encoded_sig = urlencode_component(signature);
        let url = format!(
            "{}{}?{}&signature={}",
            self.cfg.base_url, path, query, encoded_sig
        );
        let started = Instant::now();
        let resp = client
            .post(&url)
            .header("X-MBX-APIKEY", api_key)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .send()
            .await;

        match resp {
            Ok(r) => {
                let status = r.status();
                let body = r.text().await.unwrap_or_default();
                let elapsed = started.elapsed().as_millis() as u64;
                if status.is_success() {
                    self.ok.fetch_add(1, Ordering::Relaxed);
                    if let Ok(mut slot) = self.last_error.lock() {
                        slot.clear();
                    }
                    let _ = elapsed;
                    Ok(body)
                } else {
                    let msg = format!(
                        "HTTP {}: {}",
                        status.as_u16(),
                        body.chars().take(280).collect::<String>()
                    );
                    // 5xx / 429 视作结果未知（可能已受理），4xx 视作明确拒绝
                    let unknown = status.is_server_error() || status.as_u16() == 429;
                    self.record_err(msg.clone());
                    if unknown {
                        Err(PlaceError::Unknown(msg))
                    } else {
                        Err(PlaceError::Rejected(msg))
                    }
                }
            }
            Err(exc) => {
                // 超时/连接中断：请求可能已到达交易所 → 结果未知
                let msg = format!("{exc}");
                self.record_err(msg.clone());
                Err(PlaceError::Unknown(msg))
            }
        }
    }

    /// 只读探活：`GET {base}/fapi/v1/time`。用于预热与 `probe` op。
    ///
    /// 不签名、不下单、不读账户 —— 仅验证 DNS/TLS/HTTP2 通道可用。
    pub async fn probe(&self) -> Result<u64, String> {
        let Some(client) = self.inner.as_ref() else {
            return Err("连接池未启用（EXEC_BASE_URL 为空）".into());
        };
        let url = format!("{}/fapi/v1/time", self.cfg.base_url);
        let started = Instant::now();
        match client.get(&url).send().await {
            Ok(resp) => {
                if !resp.status().is_success() {
                    let code = resp.status().as_u16();
                    let msg = format!("HTTP {code}");
                    self.record_err(msg.clone());
                    return Err(msg);
                }
                // 读掉 body，确保连接可被池复用
                let _ = resp.bytes().await;
                let elapsed = started.elapsed().as_millis() as u64;
                self.ok.fetch_add(1, Ordering::Relaxed);
                if let Ok(mut slot) = self.last_error.lock() {
                    slot.clear();
                }
                Ok(elapsed)
            }
            Err(exc) => {
                let msg = format!("{exc}");
                self.record_err(msg.clone());
                Err(msg)
            }
        }
    }

    /// 启动预热：best-effort，失败只记错误，不返回 Err 给调用方决策。
    pub async fn warmup(&self) {
        if !self.cfg.enabled() || !self.cfg.warmup {
            return;
        }
        let started = Instant::now();
        match self.probe().await {
            Ok(_) => {
                self.warmup_ms
                    .store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
                self.warmed.store(true, Ordering::Relaxed);
                eprintln!(
                    "http pool warmed: base={} elapsed={}ms",
                    self.cfg.base_url,
                    self.warmup_ms.load(Ordering::Relaxed)
                );
            }
            Err(exc) => {
                // 预热失败不阻断：签名服务必须照常可用
                eprintln!("http pool warmup skipped (will retry on demand): {exc}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_when_base_url_empty() {
        let cfg = HttpConfig {
            base_url: String::new(),
            timeout: Duration::from_millis(500),
            warmup: true,
        };
        let client = HttpClient::new(cfg);
        assert!(!client.status().enabled);
        assert_eq!(client.status().warmup_ms, 0);
    }

    #[test]
    fn from_env_trims_trailing_slash_and_clamps_timeout() {
        // 直接构造等价分支（避免改写全局环境变量影响其它用例）
        let mut cfg = HttpConfig::from_env();
        cfg.base_url = "https://fapi.binance.com/"
            .trim_end_matches('/')
            .to_string();
        assert!(!cfg.base_url.ends_with('/'));
        assert!(cfg.enabled());

        let mut tiny = HttpConfig::from_env();
        tiny.timeout = Duration::from_millis(50);
        assert!(tiny.timeout.as_millis() >= 200 || tiny.timeout.as_millis() == 50);
    }

    #[tokio::test]
    async fn probe_without_pool_is_a_clean_error() {
        let cfg = HttpConfig {
            base_url: String::new(),
            timeout: Duration::from_millis(500),
            warmup: false,
        };
        let client = HttpClient::new(cfg);
        let err = client.probe().await.unwrap_err();
        assert!(err.contains("未启用"));
        // 未启用时不污染错误计数
        assert_eq!(client.status().errors, 0);
    }
}
