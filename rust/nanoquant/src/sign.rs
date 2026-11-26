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
    /// 待签名的 query：只有测试在断言，签名方正式路径只取 `wire`。
    #[cfg(test)]
    pub query: String,
    /// 签名本身：同上，正式路径只把它拼进 `wire`。
    #[cfg(test)]
    pub signature: String,
    /// 可以直接拼到 URL 上的最终 query。
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
        #[cfg(test)]
        query,
        #[cfg(test)]
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
#[cfg(test)]
pub fn order_params(
    symbol: &str,
    order_side: &str,
    position_side: &str,
    qty: &str,
    client_id: &str,
    dual: bool,
    closing: bool,
) -> Vec<(String, String)> {
    order_params_typed(
        symbol,
        order_side,
        position_side,
        qty,
        client_id,
        dual,
        closing,
        "MARKET",
        None,
        false,
    )
}

/// 同 `order_params`，但支持只做 Maker 的限价单。
///
/// `price` 只在 `LIMIT` 时使用；`post_only` 为真时追加 `timeInForce=GTX`，
/// 交易所会把无法立即成交的挂单拒掉而不是转为 taker。
#[allow(clippy::too_many_arguments)]
pub fn order_params_typed(
    symbol: &str,
    order_side: &str,
    position_side: &str,
    qty: &str,
    client_id: &str,
    dual: bool,
    closing: bool,
    order_type: &str,
    price: Option<&str>,
    post_only: bool,
) -> Vec<(String, String)> {
    let kind = if order_type.trim().eq_ignore_ascii_case("LIMIT") {
        "LIMIT"
    } else {
        "MARKET"
    };
    let mut params = vec![
        ("symbol".to_string(), symbol.to_string()),
        ("side".to_string(), order_side.to_string()),
        ("type".to_string(), kind.to_string()),
        ("quantity".to_string(), qty.to_string()),
    ];
    if kind == "LIMIT" {
        if let Some(price) = price.filter(|value| !value.is_empty()) {
            params.push(("price".to_string(), price.to_string()));
        }
        if post_only {
            params.push(("timeInForce".to_string(), "GTX".to_string()));
        }
    }
    params.push(("newClientOrderId".to_string(), client_id.to_string()));
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
    fn order_type_market_keeps_market_and_limit_post_only_sends_gtx_price() {
        let market = order_params("BTCUSDT", "BUY", "long", "0.25", "nq-mkt", false, false);
        assert_eq!(
            market
                .iter()
                .find(|(k, _)| k == "type")
                .map(|(_, v)| v.as_str()),
            Some("MARKET"),
            "market 模式必须继续用市价单"
        );
        assert!(!market
            .iter()
            .any(|(k, _)| k == "timeInForce" || k == "price"));

        let limit = order_params_typed(
            "BTCUSDT",
            "BUY",
            "long",
            "0.25",
            "nq-lmt",
            false,
            false,
            "LIMIT",
            Some("99999.9"),
            true,
        );
        let value = |key: &str| {
            limit
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(value("type"), Some("LIMIT"));
        assert_eq!(value("timeInForce"), Some("GTX"), "只做 Maker 必须用 GTX");
        assert_eq!(value("price"), Some("99999.9"));
        assert!(!limit.iter().any(|(k, _)| k == "reduceOnly"));
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
