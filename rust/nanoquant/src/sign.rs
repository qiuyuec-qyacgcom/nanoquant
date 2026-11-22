//! 币安签名。与 legacy 内核（已退役） `exchange/client.py::sign_query` 逐字节一致。
//!
//! 币安有两类 API Key：HMAC 的 Secret 是普通字符串，签名是小写 hex；
//! Ed25519 的 Secret 是 base64(PKCS#8 DER) 私钥，签名是 base64。
//! 用「能否解析出 Ed25519 私钥」来判定走哪条，解析不出就退回 HMAC，
//! 不能靠字符串长相猜测，否则普通密钥会被误签成一堆必然被拒的请求。

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use ed25519_dalek::pkcs8::DecodePrivateKey;
use ed25519_dalek::{Signer, SigningKey};
use hmac::{Hmac, Mac};
use sha2::Sha256;

const HEX: &[u8; 16] = b"0123456789abcdef";

fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// 按 `-----` 标记删除 PEM 头尾，而不是按行过滤。
/// 单行 PEM（`-----BEGIN ...----- <body> -----END ...-----`）按行过滤会被清空，
/// 随后静默退回 HMAC，签名全错。
pub fn normalize_pem(value: &str) -> String {
    let mut text = value.trim().to_string();
    if !text.contains("-----") {
        return text.split_whitespace().collect();
    }
    while let Some(start) = text.find("-----") {
        let Some(offset) = text[start + 5..].find("-----") else {
            break;
        };
        let end = start + 5 + offset + 5;
        text.replace_range(start..end, "");
    }
    text.split_whitespace().collect()
}

/// 与 legacy 内核（已退役） `urllib.parse.quote(value, safe='')` 对齐：除字母数字和 `_ . - ~` 外全部百分号编码。
/// base64 签名里的 `+ / =` 必须编码，否则币安会按另一个字符串验签。
pub fn urlencode_component(raw: &str) -> String {
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

/// 一笔已签名的请求：待签名的 query、签名本身、以及可以直接拼到 URL 上的最终 query。
pub struct SignedQuery {
    // 调试用：请求方目前只取 wire，query/signature 尚未被读取，接线后取消豁免。
    #[allow(dead_code)]
    pub query: String,
    #[allow(dead_code)]
    pub signature: String,
    pub wire: String,
}

/// 把参数拼成币安认的签名请求。
///
/// 顺序即签名顺序：`urlencode` 按插入顺序编码，调换两个键会得到另一个签名。
/// `timestamp` 与 `recvWindow` 追加在业务参数之后，签名再百分号编码接到末尾。
pub fn signed_query(
    params: &[(&str, &str)],
    secret: &str,
    timestamp_ms: i64,
    recv_window_ms: i64,
) -> SignedQuery {
    let mut parts: Vec<String> = params
        .iter()
        .map(|(k, v)| format!("{k}={}", urlencode_component(v)))
        .collect();
    parts.push(format!("timestamp={timestamp_ms}"));
    parts.push(format!("recvWindow={recv_window_ms}"));
    let query = parts.join("&");
    let signature = sign_query(secret, &query);
    let wire = format!("{query}&signature={}", urlencode_component(&signature));
    SignedQuery {
        query,
        signature,
        wire,
    }
}

/// 开仓/平仓的下单参数。
///
/// `order_side` 是订单方向（BUY/SELL），`position_side` 是被操作的仓位方向（long/short）。
/// 平多仓时两者相反：卖出（SELL）去减多头仓（LONG）。
/// 双向持仓用 `positionSide` 标明仓位，且不能再带 `reduceOnly`（币安会拒单）；
/// 单向持仓只在平仓时带 `reduceOnly=true`。
pub fn order_params(
    symbol: &str,
    order_side: &str,
    position_side: &str,
    qty: &str,
    client_id: &str,
    dual: bool,
    closing: bool,
) -> Vec<(String, String)> {
    let mut params = vec![
        ("symbol".to_string(), symbol.to_string()),
        ("side".to_string(), order_side.to_string()),
        ("type".to_string(), "MARKET".to_string()),
        ("quantity".to_string(), qty.to_string()),
        ("newClientOrderId".to_string(), client_id.to_string()),
    ];
    if dual {
        params.push((
            "positionSide".to_string(),
            if position_side == "long" {
                "LONG"
            } else {
                "SHORT"
            }
            .to_string(),
        ));
    } else if closing {
        params.push(("reduceOnly".to_string(), "true".to_string()));
    }
    params
}

/// 平仓单：多头仓卖出、空头仓买入，数量不超过持仓。
#[cfg(test)]
pub fn closing_order(
    symbol: &str,
    position_side: &str,
    qty: f64,
    held: f64,
    client_id: &str,
    dual: bool,
) -> Vec<(String, String)> {
    let order_side = if position_side == "long" {
        "SELL"
    } else {
        "BUY"
    };
    let close_qty = qty.min(held);
    order_params(
        symbol,
        order_side,
        position_side,
        &trim_qty(close_qty),
        client_id,
        dual,
        true,
    )
}

#[cfg(test)]
fn trim_qty(qty: f64) -> String {
    let text = format!("{qty:.8}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// 步进的小数位数。`0.001` → 3，整数步进 → 0。
#[cfg(test)]
fn step_decimals(step: f64) -> usize {
    if step <= 0.0 {
        return 8;
    }
    let text = format!("{step:.12}");
    let trimmed = text.trim_end_matches('0');
    trimmed
        .split_once('.')
        .map(|(_, frac)| frac.len())
        .unwrap_or(0)
}

/// 按步进向下取整，并格式化成严格等于步进小数位的字符串。
///
/// 直接用 `:.12f` 会把 `12417.2` 打成 `12417.200000000001`，币安以 -1111 拒单。
#[cfg(test)]
pub fn format_qty(qty: f64, step: f64) -> String {
    if step <= 0.0 {
        return trim_qty(qty);
    }
    let steps = (qty / step + 1e-9).floor();
    let decimals = step_decimals(step);
    format!("{:.*}", decimals, steps * step)
}

/// 保护单（止损或止盈）的参数，顺序必须与 legacy 内核（已退役） `_place_protection` 一致：
/// algoType, symbol, side, type, triggerPrice, closePosition, workingType,
/// priceProtect, clientAlgoId，双向持仓再追加 positionSide。
/// 顺序错一位，签名就对不上，条件单会被拒，仓位等于裸奔。
#[cfg(test)]
pub fn protection_params(
    symbol: &str,
    position_side: &str,
    kind: &str,
    trigger_price: &str,
    client_algo_id: &str,
    dual: bool,
) -> Vec<(String, String)> {
    let close_side = if position_side == "long" {
        "SELL"
    } else {
        "BUY"
    };
    let mut params = vec![
        ("algoType".to_string(), "CONDITIONAL".to_string()),
        ("symbol".to_string(), symbol.to_string()),
        ("side".to_string(), close_side.to_string()),
        ("type".to_string(), kind.to_string()),
        ("triggerPrice".to_string(), trigger_price.to_string()),
        ("closePosition".to_string(), "true".to_string()),
        ("workingType".to_string(), "MARK_PRICE".to_string()),
        ("priceProtect".to_string(), "TRUE".to_string()),
        ("clientAlgoId".to_string(), client_algo_id.to_string()),
    ];
    if dual {
        params.push((
            "positionSide".to_string(),
            if position_side == "long" {
                "LONG"
            } else {
                "SHORT"
            }
            .to_string(),
        ));
    }
    params
}

/// 发单结果。`Unknown` 表示请求可能已经到达交易所（超时、连接中断、5xx、429），
/// 调用方绝不能重发，否则会挂出重复的订单或保护单。
#[derive(Debug, PartialEq, Eq)]
#[cfg(test)]
pub enum SubmitOutcome {
    Accepted,
    Rejected(i64),
    Unknown(i64),
}

/// 写请求的结果分类。只读请求的失败可以换线路重试，写请求不行。
#[cfg(test)]
pub fn classify_submit(status: u16, code: i64) -> SubmitOutcome {
    if (200..300).contains(&status) {
        return SubmitOutcome::Accepted;
    }
    // 5xx / 429：交易所可能已受理；3xx：重定向，请求去向不明
    if status >= 500 || status == 429 || (300..400).contains(&status) {
        return SubmitOutcome::Unknown(code);
    }
    // 超时、内部错误、服务不可用、时钟偏差：响应里明确说结果不确定
    if matches!(code, -1000 | -1006 | -1007 | -1021) {
        return SubmitOutcome::Unknown(code);
    }
    SubmitOutcome::Rejected(code)
}

/// 保护单走 Algo 接口被拒后，是否可以回落到老的 `/fapi/v1/order` 接口。
///
/// 只有 -4120（必须走 Algo）、-1121（交易对无效）、-1102（缺参数）这三种明确拒绝才回落。
/// 结果未知一律不回落：请求可能已经挂上，再走老接口就会挂出第二套保护单。
#[cfg(test)]
pub fn protection_should_fallback(outcome: &SubmitOutcome) -> bool {
    matches!(outcome, SubmitOutcome::Rejected(code) if matches!(code, -4120 | -1121 | -1102))
}

/// 当前环境的下单凭证。任一为空即拒绝下单，避免用空密钥签出一堆必然失败的请求。
#[derive(Debug)]
pub struct Credentials {
    pub api_key: String,
    pub secret: String,
}

pub fn require_credentials(mode: &str, api_key: &str, secret: &str) -> Result<Credentials, String> {
    if api_key.trim().is_empty() || secret.trim().is_empty() {
        let label = if mode == "live" {
            "实盘"
        } else {
            "模拟盘"
        };
        return Err(format!("未配置{label}环境的币安 API Key/Secret"));
    }
    Ok(Credentials {
        api_key: api_key.trim().to_string(),
        secret: secret.trim().to_string(),
    })
}

/// 按密钥类型签名：能解析出 Ed25519 私钥就出 base64，否则 HMAC-SHA256 出小写 hex。
pub fn sign_query(secret: &str, query: &str) -> String {
    let body = normalize_pem(secret);
    if let Ok(bytes) = STANDARD.decode(&body) {
        if let Ok(key) = SigningKey::from_pkcs8_der(&bytes) {
            return STANDARD.encode(key.sign(query.as_bytes()).to_bytes());
        }
    }
    sign_hmac(secret, query)
}

pub fn sign_hmac(secret: &str, query: &str) -> String {
    let key = normalize_pem(secret);
    let mut mac = Hmac::<Sha256>::new_from_slice(key.as_bytes()).expect("hmac accepts any key");
    mac.update(query.as_bytes());
    to_hex(&mac.finalize().into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::pkcs8::EncodePrivateKey;

    const QUERY: &str = "symbol=BTCUSDT&side=BUY&type=MARKET&timestamp=1700000000000";
    const HMAC_HEX: &str = "c5bee7ef23d0f08ff08fe04022f745e9ff389ec6ea2ad8203e0a096b238dcf36";

    #[test]
    fn hmac_matches_legacy() {
        assert_eq!(sign_hmac("secret", QUERY), HMAC_HEX);
    }

    #[test]
    fn ed25519_signature_verifies_and_plain_secrets_stay_hmac() {
        use ed25519_dalek::Verifier;

        // RFC 8032 第 7.1 节测试向量 1 的种子
        let seed = hex_bytes("9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60");
        let key = SigningKey::from_bytes(&seed);
        let der = key.to_pkcs8_der().unwrap();
        let secret = STANDARD.encode(der.as_bytes());

        let signature = sign_query(&secret, QUERY);
        let raw = STANDARD.decode(&signature).unwrap();
        key.verifying_key()
            .verify(
                QUERY.as_bytes(),
                &ed25519_dalek::Signature::from_slice(&raw).unwrap(),
            )
            .expect("Ed25519 签名必须能用对应公钥验证");

        // 普通字符串解不出私钥，必须退回 HMAC，不能误签
        assert_eq!(sign_query("secret", QUERY), HMAC_HEX);

        // 用户常把 PEM 头尾和正文粘成一行，按行过滤会被清空后静默退回 HMAC
        let one_line = format!("-----BEGIN PRIVATE KEY----- {secret} -----END PRIVATE KEY-----");
        assert_eq!(
            sign_query(&one_line, QUERY),
            signature,
            "单行 PEM 必须与裸 base64 签出同一结果"
        );

        let signed = signed_query(&[("symbol", "BTCUSDT")], &secret, 1_700_000_000_000, 5000);
        let encoded = urlencode_component(&signed.signature);
        assert!(
            !encoded.contains('+') && !encoded.contains('/') && !encoded.contains('='),
            "base64 签名的 + / = 必须百分号编码"
        );
        assert!(signed.wire.contains(&encoded));
    }

    fn hex_bytes(text: &str) -> [u8; 32] {
        let mut out = [0u8; 32];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).unwrap();
        }
        out
    }

    #[test]
    fn single_line_pem_is_stripped_before_signing() {
        let wrapped = "-----BEGIN PRIVATE KEY----- secret -----END PRIVATE KEY-----";
        assert_eq!(normalize_pem(wrapped), "secret");
        assert_eq!(
            sign_hmac(wrapped, QUERY),
            HMAC_HEX,
            "包了 PEM 头的密钥应与裸密钥签出同一结果"
        );
    }

    #[test]
    fn signed_query_appends_timestamp_and_changes_when_order_changes() {
        let a = signed_query(
            &[("symbol", "BTCUSDT"), ("side", "BUY")],
            "secret",
            1_700_000_000_000,
            5000,
        );
        assert!(a
            .query
            .ends_with("&timestamp=1700000000000&recvWindow=5000"));
        assert!(a.wire.ends_with(&format!("&signature={}", a.signature)));

        let reversed = signed_query(
            &[("side", "BUY"), ("symbol", "BTCUSDT")],
            "secret",
            1_700_000_000_000,
            5000,
        );
        assert_ne!(
            a.signature, reversed.signature,
            "参数顺序不同必须得到不同签名"
        );
    }

    #[test]
    fn hedge_mode_uses_position_side_and_never_reduce_only() {
        let open = order_params("BTCUSDT", "BUY", "long", "0.01", "nq1", true, false);
        let keys: Vec<&str> = open.iter().map(|(k, _)| k.as_str()).collect();
        assert!(keys.contains(&"positionSide"));
        assert!(
            !keys.contains(&"reduceOnly"),
            "双向持仓带 reduceOnly 会被币安拒单"
        );
        assert_eq!(
            open.iter().find(|(k, _)| k == "positionSide").unwrap().1,
            "LONG"
        );

        let one_way_close = order_params("BTCUSDT", "BUY", "short", "0.01", "nq2", false, true);
        assert_eq!(
            one_way_close
                .iter()
                .find(|(k, _)| k == "reduceOnly")
                .map(|(_, v)| v.as_str()),
            Some("true")
        );
    }

    #[test]
    fn closing_a_long_sells_and_never_exceeds_the_position() {
        let close = closing_order("BTCUSDT", "long", 5.0, 1.5, "nx1", true);
        assert_eq!(
            close.iter().find(|(k, _)| k == "side").unwrap().1,
            "SELL",
            "平多仓必须卖出"
        );
        assert_eq!(
            close.iter().find(|(k, _)| k == "quantity").unwrap().1,
            "1.5",
            "平仓数量不能超过持仓"
        );
        assert_eq!(
            close.iter().find(|(k, _)| k == "positionSide").unwrap().1,
            "LONG"
        );
    }

    #[test]
    fn quantity_is_floored_to_step_without_float_tail() {
        assert_eq!(format_qty(1.2399, 0.01), "1.23");
        assert_eq!(format_qty(12417.2, 0.1), "12417.2", "不能带出浮点尾巴");
        assert_eq!(format_qty(0.00019, 0.0001), "0.0001");
    }

    #[test]
    fn protection_order_keeps_param_order_and_closes_the_position() {
        let stop = protection_params("BTCUSDT", "long", "STOP_MARKET", "90000", "np1", true);
        let keys: Vec<&str> = stop.iter().map(|(k, _)| k.as_str()).collect();
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
                "positionSide"
            ],
            "参数顺序就是签名顺序"
        );
        assert_eq!(
            stop.iter().find(|(k, _)| k == "side").unwrap().1,
            "SELL",
            "多头的保护单必须卖出平仓"
        );
        assert_eq!(
            stop.iter().find(|(k, _)| k == "closePosition").unwrap().1,
            "true"
        );

        let one_way = protection_params(
            "BTCUSDT",
            "short",
            "TAKE_PROFIT_MARKET",
            "80000",
            "np2",
            false,
        );
        assert!(
            !one_way.iter().any(|(k, _)| k == "positionSide"),
            "单向持仓不带 positionSide"
        );
        assert_eq!(one_way.iter().find(|(k, _)| k == "side").unwrap().1, "BUY");
    }

    #[test]
    fn uncertain_submission_must_not_be_retried() {
        assert_eq!(classify_submit(200, 0), SubmitOutcome::Accepted);
        assert_eq!(
            classify_submit(400, -4120),
            SubmitOutcome::Rejected(-4120),
            "-4120 是明确拒绝，可以回落"
        );
        assert_eq!(classify_submit(400, -1111), SubmitOutcome::Rejected(-1111));
        assert_eq!(
            classify_submit(503, -1000),
            SubmitOutcome::Unknown(-1000),
            "5xx 结果未知，重发会重复下单"
        );
        assert_eq!(classify_submit(429, -1003), SubmitOutcome::Unknown(-1003));
        assert_eq!(
            classify_submit(400, -1021),
            SubmitOutcome::Unknown(-1021),
            "时钟偏差时请求可能已被受理"
        );
    }

    #[test]
    fn protection_falls_back_only_on_explicit_rejection() {
        assert!(protection_should_fallback(&SubmitOutcome::Rejected(-4120)));
        assert!(protection_should_fallback(&SubmitOutcome::Rejected(-1121)));
        assert!(
            !protection_should_fallback(&SubmitOutcome::Rejected(-2019)),
            "保证金不足不是接口问题，回落也一样失败"
        );
        assert!(
            !protection_should_fallback(&SubmitOutcome::Unknown(-1000)),
            "结果未知回落会挂出第二套保护单"
        );
        assert!(!protection_should_fallback(&SubmitOutcome::Accepted));
    }

    #[test]
    fn missing_credentials_refuse_to_trade_and_name_the_environment() {
        assert!(require_credentials("live", "", "secret").is_err());
        let err = require_credentials("live", "key", "  ").unwrap_err();
        assert!(err.contains("实盘"), "报错要指明是哪个环境没配");
        assert!(require_credentials("demo", "key", "secret").is_ok());
    }

    #[test]
    fn base64_signature_is_fully_url_encoded() {
        let signature = "ab+c/d=";
        assert_eq!(urlencode_component(signature), "ab%2Bc%2Fd%3D");
    }
}
