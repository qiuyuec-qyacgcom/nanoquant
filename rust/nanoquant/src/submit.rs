//! 发单结果判定。与 legacy 内核（已退役） `exchange/client.py::_request` 的错误分类一致。
//!
//! 写请求（下单、撤单）遇到超时、连接中断或 -1000/-1006/-1007/-1021 时，
//! 交易所可能已经接单。这种结果未知必须停下来查询确认，直接重发就是重复下单。
//! 只读请求没有副作用，同样的异常可以换线路重试。

/// 交易所的一次回应：要么传输层失败，要么返回了带错误码的 JSON。
pub enum ExchangeReply {
    TransportError,
    /// 3xx：不是币安 JSON API 的正常响应。
    Redirect,
    ApiError {
        code: i64,
    },
}

/// 上层该怎么处理这次回应。
pub struct Outcome {
    /// 结果未知：订单可能已经生效，必须查询确认，禁止重发。
    pub uncertain: bool,
    /// 可以换线路重试。只对没有副作用的只读请求为真。
    pub retry: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReplyClass {
    Filled,
    Accepted,
    PartiallyFilled,
    Rejected,
    NotFound,
    Unknown,
}

const UNCERTAIN_CODES: [i64; 4] = [-1000, -1006, -1007, -1021];

/// 交易所线路。只读请求失败时切到下一条出口；写请求失败保持不动，
/// 因为结果未知时换线路重发就是重复下单。只有一条线路时无处可换。
pub struct EndpointLine {
    bases: Vec<String>,
    pos: usize,
}

impl EndpointLine {
    pub fn new(bases: &[&str]) -> Self {
        let bases: Vec<String> = bases
            .iter()
            .map(|b| b.trim().trim_end_matches('/').to_string())
            .filter(|b| !b.is_empty())
            .collect();
        Self { bases, pos: 0 }
    }

    pub fn current(&self) -> &str {
        self.bases.get(self.pos).map(String::as_str).unwrap_or("")
    }

    pub fn rotate_after(&mut self, method: &str) -> bool {
        if method.eq_ignore_ascii_case("GET") && self.bases.len() > 1 {
            self.pos = (self.pos + 1) % self.bases.len();
            return true;
        }
        false
    }
}

/// 封禁熔断。命中 -1003 后解析 "banned until <毫秒>"。
///
/// 交易所声明的封禁可能长达数小时，照单全收会让面板假死；但固定间隔去试探，
/// 每次试探本身又是一次超限请求，封禁计时会被续上。所以本地熔断取
/// 「声明的解封时间」和「指数退避」里较早的那个：60 秒起、每次翻倍、封顶 1800 秒。
/// 任何一次请求成功都把退避计数归零。
pub struct BanGate {
    until_ms: i64,
    streak: u32,
}

const BACKOFF_BASE_MS: i64 = 60_000;
const BACKOFF_MAX_MS: i64 = 1_800_000;
const BACKOFF_MAX_STREAK: u32 = 6;

impl BanGate {
    pub fn new() -> Self {
        Self {
            until_ms: 0,
            streak: 0,
        }
    }

    /// 从持久化的到期时间恢复：封禁跨进程重启仍然有效。
    pub fn with_until(until_ms: i64) -> Self {
        Self {
            until_ms: until_ms.max(0),
            streak: 0,
        }
    }

    pub fn observe(&mut self, message: &str, now_ms: i64) {
        if let Some(rest) = message.split("banned until ").nth(1) {
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(declared) = digits.parse::<i64>() {
                self.streak = (self.streak + 1).min(BACKOFF_MAX_STREAK);
                let backoff = (BACKOFF_BASE_MS << (self.streak - 1)).min(BACKOFF_MAX_MS);
                self.until_ms = self.until_ms.max(declared.min(now_ms + backoff));
            }
        }
    }

    /// 一次请求成功：退避计数归零，下一次封禁重新从 60 秒起算。
    pub fn observe_success(&mut self) {
        self.streak = 0;
    }

    pub fn allow(&self, now_ms: i64) -> bool {
        now_ms >= self.until_ms
    }

    pub fn until_ms(&self) -> i64 {
        self.until_ms
    }
}

/// 一次准备好、尚未发出的请求。签名放在 URL 里，API Key 放在请求头里，
/// 币安不认请求体里的签名。
#[derive(Debug)]
pub struct PreparedRequest {
    pub method: String,
    pub url: String,
    pub headers: std::collections::BTreeMap<String, String>,
}

/// 组装一笔签名请求。
///
/// 空参数丢掉：留下一个 `skip=` 会改变签名串，币安按另一个字符串验签。
/// 根地址的末尾斜杠去掉再拼接，避免拼出 `//fapi`。
pub fn build_request(
    method: &str,
    base: &str,
    path: &str,
    params: &[(String, String)],
    api_key: &str,
    secret: &str,
    timestamp_ms: i64,
    recv_window_ms: i64,
) -> PreparedRequest {
    let kept: Vec<(&str, &str)> = params
        .iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let signed = crate::sign::signed_query(&kept, secret, timestamp_ms, recv_window_ms);
    let mut headers = std::collections::BTreeMap::new();
    headers.insert("X-MBX-APIKEY".to_string(), api_key.to_string());
    PreparedRequest {
        method: method.to_string(),
        url: format!("{}{path}?{}", base.trim_end_matches('/'), signed.wire),
        headers,
    }
}

/// 发单前的总入口：封禁期内直接拒绝，不组装请求。
pub fn prepare(
    gate: &BanGate,
    now_ms: i64,
    method: &str,
    base: &str,
    path: &str,
    params: &[(String, String)],
    api_key: &str,
    secret: &str,
    timestamp_ms: i64,
    recv_window_ms: i64,
) -> Result<PreparedRequest, String> {
    if !gate.allow(now_ms) {
        return Err(format!(
            "币安限频封禁中，{now_ms} 尚未到 {}",
            gate.until_ms()
        ));
    }
    Ok(build_request(
        method,
        base,
        path,
        params,
        api_key,
        secret,
        timestamp_ms,
        recv_window_ms,
    ))
}

/// 解释一次真实的 HTTP 回应。
///
/// 2xx 但正文不是 JSON（网关超时页、空正文）对写请求按结果未知处理：
/// 订单可能已经生效，当成失败重发就是重复下单。
pub fn interpret(method: &str, status: u16, body: &str) -> Outcome {
    if (200..300).contains(&status) {
        let readable = serde_json::from_str::<serde_json::Value>(body).is_ok();
        let write = !method.eq_ignore_ascii_case("GET");
        return Outcome {
            uncertain: write && !readable,
            retry: false,
        };
    }
    if (300..400).contains(&status) {
        return classify(method, ExchangeReply::Redirect);
    }
    // 5xx / 429：交易所可能已受理，正文不可信
    if status >= 500 || status == 429 {
        return classify(method, ExchangeReply::TransportError);
    }
    let code = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("code").and_then(|c| c.as_i64()))
        .unwrap_or(-1);
    classify(method, ExchangeReply::ApiError { code })
}

/// 保护单的处置：只有 -4120/-1121/-1102 三种明确拒绝才回落老接口。
pub struct ProtectionPlan {
    pub fallback: bool,
}

pub fn protection_plan(method: &str, status: u16, body: &str) -> ProtectionPlan {
    let outcome = interpret(method, status, body);
    let code = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("code").and_then(|c| c.as_i64()))
        .unwrap_or(0);
    let fallback = !outcome.uncertain && matches!(code, -4120 | -1121 | -1102);
    ProtectionPlan { fallback }
}

/// 真正把请求发到交易所。传输失败（超时、连接中断）对写请求是结果未知。
pub struct Transport {
    client: reqwest::Client,
}

impl Transport {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { client }
    }

    /// 原始回应：状态码加正文。
    ///
    /// 下单要按回包里的 `executedQty`/`avgPrice` 落库，只拿到 `Outcome`
    /// 无法区分「已成交」和「挂着未成」。读请求同样要正文来认 `code`。
    pub async fn send_raw(&self, req: &PreparedRequest) -> Result<(u16, String), ()> {
        let method =
            reqwest::Method::from_bytes(req.method.as_bytes()).unwrap_or(reqwest::Method::POST);
        let mut builder = self.client.request(method, &req.url);
        for (k, v) in &req.headers {
            builder = builder.header(k, v);
        }
        match builder.send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                Ok((status, resp.text().await.unwrap_or_default()))
            }
            Err(_) => Err(()),
        }
    }

    pub async fn send(&self, req: &PreparedRequest) -> Outcome {
        match self.send_raw(req).await {
            Ok((status, body)) => interpret(&req.method, status, &body),
            Err(()) => classify(&req.method, ExchangeReply::TransportError),
        }
    }
}

pub fn classify(method: &str, reply: ExchangeReply) -> Outcome {
    let write = !method.eq_ignore_ascii_case("GET");
    match reply {
        ExchangeReply::TransportError | ExchangeReply::Redirect => Outcome {
            uncertain: write,
            retry: !write,
        },
        ExchangeReply::ApiError { code } if write && UNCERTAIN_CODES.contains(&code) => Outcome {
            uncertain: true,
            retry: false,
        },
        ExchangeReply::ApiError { .. } => Outcome {
            uncertain: false,
            retry: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_timeout_is_uncertain_and_must_not_be_retried() {
        let outcome = classify("POST", ExchangeReply::TransportError);
        assert!(outcome.uncertain, "写请求超时结果未知，重发会重复下单");
        assert!(!outcome.retry, "结果未知禁止重试");

        let rejected = classify("POST", ExchangeReply::ApiError { code: -2019 });
        assert!(
            !rejected.uncertain,
            "明确的拒单（保证金不足）可以安全地不再发送"
        );
    }

    #[test]
    fn read_timeout_may_be_retried_on_another_endpoint() {
        let outcome = classify("GET", ExchangeReply::TransportError);
        assert!(
            !outcome.uncertain && outcome.retry,
            "只读请求没有副作用，可以换线路重试"
        );

        let status = classify("DELETE", ExchangeReply::ApiError { code: -1007 });
        assert!(status.uncertain, "写请求收到 -1007 时订单可能已生效");
    }

    #[test]
    fn redirect_on_a_write_is_uncertain() {
        let outcome = classify("POST", ExchangeReply::Redirect);
        assert!(
            outcome.uncertain && !outcome.retry,
            "3xx 不是币安的正常响应，写请求禁止重发"
        );

        let read = classify("GET", ExchangeReply::Redirect);
        assert!(read.retry, "只读请求遇到重定向可以换线路重试");
    }

    #[test]
    fn only_a_failed_read_rotates_the_endpoint() {
        let bases = &["https://a.example", "https://b.example"];
        let mut line = EndpointLine::new(bases);

        assert_eq!(line.current(), "https://a.example");
        assert!(
            !line.rotate_after("POST"),
            "写请求失败不能换线路，换了重发就是重复下单"
        );
        assert_eq!(line.current(), "https://a.example");

        assert!(line.rotate_after("GET"), "只读请求失败可以换到下一条出口");
        assert_eq!(line.current(), "https://b.example");

        let mut single = EndpointLine::new(&["https://only.example"]);
        assert!(!single.rotate_after("GET"), "只有一条线路时无处可换");
    }

    #[test]
    fn ban_blocks_every_request_until_it_expires() {
        let mut gate = BanGate::new();
        assert!(gate.allow(1_000), "未封禁时放行");

        gate.observe("-1003 banned until 4000", 1_000);
        assert!(
            !gate.allow(3_999),
            "封禁期内每个请求都是一次违规，必须直接拒发"
        );
        assert!(gate.allow(4_000), "封禁到期后恢复放行");
    }

    #[test]
    fn long_ban_is_capped_by_backoff_and_a_success_resets_it() {
        let mut gate = BanGate::new();
        // 交易所声明封禁 3 小时，本地第一次只退避 60 秒，不能照单全收把面板停掉一下午
        gate.observe("banned until 10800000", 0);
        assert!(gate.allow(60_000), "第一次封禁只退避 60 秒");

        gate.observe("banned until 10800000", 60_000);
        assert!(!gate.allow(120_000), "连续第二次翻倍到 120 秒");
        assert!(gate.allow(180_000));

        gate.observe_success();
        gate.observe("banned until 10800000", 180_000);
        assert!(
            gate.allow(240_000),
            "一次成功后退避计数归零，重新从 60 秒起算"
        );
    }

    #[test]
    fn repeated_bans_stop_doubling_at_thirty_minutes() {
        let mut gate = BanGate::new();
        let mut now = 0_i64;
        for _ in 0..6 {
            gate.observe("banned until 99999999999", now);
            now = gate.until_ms();
        }
        let capped = gate.until_ms();
        gate.observe("banned until 99999999999", now);
        assert_eq!(
            gate.until_ms() - now,
            1_800_000,
            "第七次仍是 1800 秒，退避不再加长"
        );
        assert_eq!(capped - (now - 1_800_000), 1_800_000, "第六次已经到顶");
    }

    #[test]
    fn signed_request_puts_the_signature_in_the_url_and_the_key_in_the_header() {
        let params = vec![
            ("symbol".to_string(), "BTCUSDT".to_string()),
            ("skip".to_string(), String::new()),
        ];
        let req = build_request(
            "POST",
            "https://a.example/",
            "/fapi/v1/order",
            &params,
            "my-api-key",
            "secret",
            1_700_000_000_000,
            5000,
        );
        assert_eq!(req.method, "POST");
        assert!(
            req.url.starts_with("https://a.example/fapi/v1/order?"),
            "末尾斜杠不能拼出双斜杠"
        );
        assert!(req.url.contains("symbol=BTCUSDT"), "业务参数进 query");
        assert!(!req.url.contains("skip="), "空参数必须丢掉，否则签名对不上");
        assert!(req.url.contains("&signature="), "签名接在 query 末尾");
        assert_eq!(
            req.headers.get("X-MBX-APIKEY"),
            Some(&"my-api-key".to_string())
        );
    }

    #[test]
    fn a_banned_gate_refuses_before_any_request_is_built() {
        let mut gate = BanGate::new();
        gate.observe("banned until 9000", 1_000);
        let params = vec![("symbol".to_string(), "BTCUSDT".to_string())];
        let err = prepare(
            &gate,
            5_000,
            "POST",
            "https://a.example",
            "/fapi/v1/order",
            &params,
            "k",
            "secret",
            5_000,
            5000,
        )
        .expect_err("封禁期内签出请求再拒发，等于已经准备好一次违规");
        assert!(err.contains("封禁"), "拒绝原因要说明是封禁，而不是参数错误");

        let ok = prepare(
            &gate,
            9_000,
            "POST",
            "https://a.example",
            "/fapi/v1/order",
            &params,
            "k",
            "secret",
            9_000,
            5000,
        );
        assert!(ok.is_ok(), "封禁到期后恢复组装");
    }

    #[test]
    fn a_write_with_an_unreadable_body_is_uncertain() {
        let outcome = interpret("POST", 200, "upstream gateway timeout");
        assert!(
            outcome.uncertain,
            "2xx 但不是 JSON：不能当成成交，重发会重复下单"
        );

        let rejected = interpret(
            "POST",
            400,
            r#"{"code":-2019,"msg":"Margin is insufficient."}"#,
        );
        assert!(!rejected.uncertain && !rejected.retry, "明确拒单照常处理");

        let accepted = interpret("POST", 200, r#"{"orderId":1,"status":"FILLED"}"#);
        assert!(
            !accepted.uncertain && !accepted.retry,
            "合法的成交回包不是未知"
        );
    }

    #[test]
    fn protection_falls_back_only_on_three_explicit_rejections() {
        let plan = protection_plan("POST", 400, r#"{"code":-4120,"msg":"use algo"}"#);
        assert!(plan.fallback, "-4120 是明确拒绝，可以走老接口");

        let unknown = protection_plan("POST", 500, "");
        assert!(!unknown.fallback, "5xx 结果未知，回落会挂出第二套保护单");

        let other = protection_plan("POST", 400, r#"{"code":-2019,"msg":"margin"}"#);
        assert!(!other.fallback, "保证金不足不是回落条件");
    }

    #[tokio::test]
    async fn a_dropped_connection_on_a_write_is_uncertain() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await;
            drop(socket);
        });

        let transport = Transport::new();
        let req = PreparedRequest {
            method: "POST".to_string(),
            url: format!("http://{addr}/fapi/v1/order"),
            headers: std::collections::BTreeMap::new(),
        };
        let outcome = transport.send(&req).await;
        assert!(
            outcome.uncertain && !outcome.retry,
            "连接被掐断时订单可能已受理，禁止重发"
        );
    }

    #[tokio::test]
    async fn a_real_rejection_comes_back_as_rejected() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 2048];
            let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await;
            let body = r#"{"code":-2019,"msg":"Margin is insufficient."}"#;
            let resp = format!(
                "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, resp.as_bytes()).await;
        });

        let transport = Transport::new();
        let req = PreparedRequest {
            method: "POST".to_string(),
            url: format!("http://{addr}/fapi/v1/order"),
            headers: std::collections::BTreeMap::new(),
        };
        let outcome = transport.send(&req).await;
        assert!(
            !outcome.uncertain && !outcome.retry,
            "400 带 -2019 是明确拒单，不是结果未知"
        );
    }
}
